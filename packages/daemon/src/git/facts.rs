use super::{ChangedPaths, CheckoutChange, command::*};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

pub fn non_git(cwd: &Path) -> Value {
    json!({"cwd":cwd,"isGit":false,"repoRoot":null,"mainRepoRoot":null,"diffStat":null,"currentBranch":null,"isDirty":null,"baseRef":null,"aheadBehind":null,"upstreamRef":null,"aheadOfOrigin":null,"behindOfOrigin":null,"hasRemote":false,"remoteUrl":null,"isPaseoOwnedWorktree":false,"error":null})
}
pub async fn current_branch(
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Option<String>> {
    if let Some(branch) = optional(
        cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        env,
        cancel.clone(),
    )
    .await?
    {
        return Ok(Some(branch));
    }
    rebase_branch(&git_dir(cwd, env, cancel).await?).await
}

async fn rebase_branch(dir: &Path) -> Result<Option<String>> {
    for name in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        match tokio::fs::read_to_string(dir.join(name)).await {
            Ok(s) => return Ok(Some(s.trim().trim_start_matches("refs/heads/").into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}
pub async fn divergence(
    cwd: &Path,
    left: &str,
    right: &str,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Option<(u64, u64)>> {
    let Some(counts) = optional(
        cwd,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{left}...{right}"),
        ],
        env,
        cancel,
    )
    .await?
    else {
        return Ok(None);
    };
    let mut values = counts.split_whitespace();
    Ok(Some((
        values
            .next()
            .ok_or_else(|| anyhow!("Missing ahead count"))?
            .parse()?,
        values
            .next()
            .ok_or_else(|| anyhow!("Missing behind count"))?
            .parse()?,
    )))
}
/// The commit and branch facts from status's branch header, when available.
#[derive(Default)]
struct BranchHeader<'a> {
    oid: Option<&'a str>,
    branch: Option<&'a str>,
    upstream: bool,
    counts: Option<(u64, u64)>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct StatusEntry {
    tracked: Option<TrackedStatus>,
    untracked: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct TrackedStatus {
    old_path: Option<PathBuf>,
    fields: Box<[u8]>,
}

impl StatusEntry {
    fn worktree_rename_candidate(&self) -> bool {
        // A worktree rename (including intent-to-add) may pair two paths outside
        // any one pathspec. Staged rename pairing is stable until the index or
        // HEAD changes; expand both endpoints when we already know the pair.
        self.tracked.as_ref().is_some_and(|tracked| {
            tracked
                .fields
                .get(3)
                .is_some_and(|flag| matches!(flag, b'A' | b'D' | b'R' | b'C'))
        })
    }

    fn old_path(&self) -> Option<&Path> {
        self.tracked.as_ref()?.old_path.as_deref()
    }
}

type StatusEntries = BTreeMap<PathBuf, StatusEntry>;

fn git_path(bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(bytes)).to_owned())
    }
    #[cfg(not(unix))]
    {
        Ok(PathBuf::from(std::str::from_utf8(bytes)?))
    }
}

fn parse_status(bytes: &[u8]) -> Result<(BranchHeader<'_>, StatusEntries)> {
    let mut header = BranchHeader::default();
    let mut entries = StatusEntries::new();
    let mut records = bytes.split(|b| *b == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            break;
        }
        if record.starts_with(b"# ") {
            let text = std::str::from_utf8(record)?;
            if let Some(oid) = text.strip_prefix("# branch.oid ") {
                header.oid = (oid != "(initial)").then_some(oid);
            } else if let Some(branch) = text.strip_prefix("# branch.head ") {
                header.branch = (branch != "(detached)").then_some(branch);
            } else if text.starts_with("# branch.upstream ") {
                header.upstream = true;
            } else if let Some(ab) = text.strip_prefix("# branch.ab ")
                && let Some((ahead, behind)) = ab.split_once(" -")
                && let Some(ahead) = ahead.strip_prefix('+')
            {
                header.counts = Some((ahead.parse()?, behind.parse()?));
            }
            continue;
        }
        // Porcelain v2 is unquoted with -z. Field counts are fixed, so spaces,
        // newlines, and literal pathspec metacharacters in names are preserved.
        let fields = match record.first() {
            Some(b'1') => 8,
            Some(b'2') => 9,
            Some(b'u') => 10,
            Some(b'?' | b'!') => 1,
            _ => bail!("Unexpected Git status record"),
        };
        let path = record
            .splitn(fields + 1, |b| *b == b' ')
            .nth(fields)
            .filter(|path| !path.is_empty())
            .context("Git status record had no path")?;
        let entry = entries.entry(git_path(path)?).or_default();
        if record.starts_with(b"? ") || record.starts_with(b"! ") {
            entry.untracked = true;
            continue;
        }
        let old_path = if record.starts_with(b"2 ") {
            Some(git_path(
                records
                    .next()
                    .filter(|path| !path.is_empty())
                    .context("Git status rename had no source")?,
            )?)
        } else {
            None
        };
        let prefix = &record[..record.len() - path.len() - 1];
        entry.tracked = Some(TrackedStatus {
            old_path,
            fields: prefix.into(),
        });
    }
    Ok((header, entries))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Untracked {
    Hide,
    Files,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct StatusOptions {
    untracked: Untracked,
    ignore_case: bool,
}

async fn status(
    root: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
    options: StatusOptions,
    branch: bool,
    paths: &[&str],
) -> Result<Vec<u8>> {
    let mut args = Vec::with_capacity(7 + paths.len());
    if !paths.is_empty() && !options.ignore_case {
        args.push("--literal-pathspecs".into());
    }
    args.extend(["status".into(), "--porcelain=v2".into(), "-z".into()]);
    // Use the same mode for full and scoped status. Expanding ordinary
    // untracked directories avoids replacing a collapsed directory with only
    // part of its contents when status is limited to a single child.
    args.push(
        match options.untracked {
            Untracked::Hide => "--untracked-files=no",
            Untracked::Files => "--untracked-files=all",
        }
        .into(),
    );
    if branch {
        args.extend(["--branch".into(), "--ahead-behind".into()]);
    }
    if !paths.is_empty() {
        args.push("--".into());
        args.extend(paths.iter().map(|path| {
            if options.ignore_case {
                format!(":(literal,icase){path}")
            } else {
                (*path).to_owned()
            }
        }));
    }
    checked(root, &args, env, cancel.clone(), READ_LIMIT, READ_TIMEOUT).await
}

struct StatusScope {
    paths: BTreeSet<PathBuf>,
    ignore_case: bool,
}

fn starts_with_ignoring_case(path: &Path, prefix: &Path) -> bool {
    let mut parts = path.components();
    prefix.components().all(|component| {
        parts.next().is_some_and(|part| {
            part.as_os_str()
                .as_encoded_bytes()
                .eq_ignore_ascii_case(component.as_os_str().as_encoded_bytes())
        })
    })
}

impl StatusScope {
    fn contains(&self, path: &Path) -> bool {
        if self.ignore_case {
            self.paths
                .iter()
                .any(|prefix| starts_with_ignoring_case(path, prefix))
        } else {
            path.ancestors().any(|part| self.paths.contains(part))
        }
    }

    fn overlaps(&self, path: &Path, entry: &StatusEntry) -> bool {
        self.contains(path) || entry.old_path().is_some_and(|old| self.contains(old))
    }
}

async fn scoped_status(
    repo: &ObservedRepo,
    paths: &BTreeSet<PathBuf>,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Option<(StatusScope, StatusEntries)>> {
    const PATH_LIMIT: usize = 256;
    const ARG_BYTES: usize = 24 * 1024;

    if paths.len() > PATH_LIMIT {
        return Ok(None);
    }
    let mut scope = StatusScope {
        paths: BTreeSet::new(),
        ignore_case: repo.status_options.ignore_case,
    };
    for path in paths {
        if path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
            || path.file_name().is_some_and(|name| name == ".gitmodules")
        {
            return Ok(None);
        }
        let mut dir = path.parent();
        while let Some(ancestor) = dir.filter(|ancestor| !ancestor.as_os_str().is_empty()) {
            // A submodule or untracked embedded repository is one opaque
            // status entry. Asking Git for a child of it can return nothing.
            if tokio::fs::symlink_metadata(repo.root.join(ancestor).join(".git"))
                .await
                .is_ok()
            {
                scope.paths.insert(ancestor.to_owned());
                break;
            }
            dir = ancestor.parent();
        }
        let path = if path.file_name().is_some_and(|name| name == ".gitignore") {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        if path.as_os_str().is_empty() {
            return Ok(None);
        }
        scope.paths.insert(path.to_owned());
    }
    let mut has_rename = false;
    for (path, entry) in &repo.entries {
        if ((entry.untracked && path.as_os_str().as_encoded_bytes().ends_with(b"/"))
            || entry
                .tracked
                .as_ref()
                .is_some_and(|tracked| tracked.fields.get(5) == Some(&b'S')))
            && scope
                .paths
                .iter()
                .any(|candidate| candidate.starts_with(path))
            && !scope.contains(path)
        {
            return Ok(None);
        }
        if scope.overlaps(path, entry) {
            if entry.worktree_rename_candidate() {
                return Ok(None);
            }
            if let Some(old) = entry.old_path() {
                has_rename = true;
                scope.paths.insert(path.clone());
                scope.paths.insert(old.to_owned());
            }
        }
    }
    if scope.paths.len() > PATH_LIMIT {
        return Ok(None);
    }
    let spec = scope
        .paths
        .iter()
        .filter(|path| {
            !path
                .ancestors()
                .skip(1)
                .any(|part| scope.paths.contains(part))
        })
        .map(|path| path.to_str().filter(|s| s.len() < ARG_BYTES / 2))
        .collect::<Option<Vec<_>>>();
    let Some(spec) = spec else { return Ok(None) };
    let mut entries = BTreeMap::new();
    let mut rest = spec.as_slice();
    while !rest.is_empty() {
        let mut bytes = 0;
        let count = rest
            .iter()
            .take_while(|path| {
                bytes += path.len() + 17; // includes the longest pathspec prefix and NUL
                bytes <= ARG_BYTES
            })
            .count();
        // Keep both ends of every staged rename in the same Git invocation.
        if has_rename && count < rest.len() {
            return Ok(None);
        }
        let output = status(
            &repo.root,
            env,
            cancel,
            repo.status_options,
            false,
            &rest[..count],
        )
        .await?;
        let (_, found) = parse_status(&output)?;
        if found.values().any(StatusEntry::worktree_rename_candidate) {
            return Ok(None);
        }
        entries.extend(found);
        rest = &rest[count..];
    }
    Ok(Some((scope, entries)))
}

#[derive(PartialEq)]
struct Refs {
    head: Option<String>,
    branch: Option<String>,
    upstream: Option<String>,
    upstream_counts: Option<(u64, u64)>,
    base: Option<String>,
    ahead_behind: Value,
    remote: Option<String>,
}

async fn refs_config(
    root: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<(Option<String>, StatusOptions)> {
    let output = run(
        root,
        &[
            "config".into(),
            "-z".into(),
            "--get-regexp".into(),
            "^(remote\\.origin\\.url|status\\.showuntrackedfiles|core\\.ignorecase)$".into(),
        ],
        env,
        cancel.clone(),
        READ_LIMIT,
        READ_TIMEOUT,
    )
    .await?;
    if !matches!(output.status, 0 | 1) {
        bail!("{}", output.stderr.trim());
    }
    let mut remote = None;
    let mut options = StatusOptions {
        untracked: Untracked::Files,
        ignore_case: false,
    };
    for entry in std::str::from_utf8(&output.stdout)?.split_terminator('\0') {
        let (key, value) = entry
            .split_once('\n')
            .map_or((entry, None), |(key, value)| (key, Some(value)));
        {
            let val = value.unwrap_or_default();
            match key {
                "remote.origin.url" => {
                    remote = (!val.trim().is_empty()).then(|| val.trim().to_owned())
                }
                "status.showuntrackedfiles" if val.eq_ignore_ascii_case("no") => {
                    options.untracked = Untracked::Hide
                }
                "status.showuntrackedfiles"
                    if val.eq_ignore_ascii_case("normal") || val.eq_ignore_ascii_case("all") =>
                {
                    options.untracked = Untracked::Files
                }
                "status.showuntrackedfiles" => {
                    bail!("Invalid status.showUntrackedFiles value: {val}")
                }
                "core.ignorecase" => {
                    options.ignore_case = value.is_none()
                        || val != "0"
                            && !["false", "no", "off", ""]
                                .iter()
                                .any(|v| val.eq_ignore_ascii_case(v))
                }
                _ => {}
            }
        }
    }
    Ok((remote, options))
}

async fn read_refs(
    root: &Path,
    dir: &Path,
    header: Option<BranchHeader<'_>>,
    remote: Option<String>,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Refs> {
    let head = async {
        if let Some(header) = &header {
            let branch = match header.branch {
                Some(branch) => Some(branch.to_owned()),
                None => rebase_branch(dir).await?,
            };
            return Ok::<_, anyhow::Error>((header.oid.map(str::to_owned), branch));
        }
        let (head, branch) = tokio::try_join!(
            optional(
                root,
                &["rev-parse", "--verify", "HEAD"],
                env,
                cancel.clone()
            ),
            optional(
                root,
                &["symbolic-ref", "--quiet", "HEAD"],
                env,
                cancel.clone()
            )
        )?;
        let branch = match branch {
            Some(branch) => Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(&branch)
                    .to_owned(),
            ),
            None => rebase_branch(dir).await?,
        };
        Ok((head, branch))
    };
    let tracking = async {
        let upstream = if header.as_ref().is_none_or(|header| header.upstream) {
            // Porcelain shortens this ref. Preserve the protocol's full name
            // for differently named remotes, custom fetch refspecs and local
            // branches (and null for a missing configured upstream).
            optional(
                root,
                &["rev-parse", "--symbolic-full-name", "@{upstream}"],
                env,
                cancel.clone(),
            )
            .await?
        } else {
            None
        };
        let counts = match (header.as_ref(), upstream.as_deref()) {
            (Some(header), Some(_)) => header.counts,
            (None, Some(upstream)) => {
                divergence(root, "HEAD", upstream, env, cancel.clone()).await?
            }
            _ => None,
        };
        Ok::<_, anyhow::Error>((upstream, counts))
    };
    let ((head, branch), (upstream, upstream_counts), base) = tokio::try_join!(
        head,
        tracking,
        operation_base_in_git_dir(root, dir, None, env, cancel.clone())
    )?;
    let ahead_behind = base_divergence(
        root,
        base.as_deref(),
        branch.as_deref(),
        env,
        cancel.clone(),
    )
    .await?;
    Ok(Refs {
        head,
        branch,
        upstream,
        upstream_counts,
        base,
        ahead_behind,
        remote,
    })
}

pub(super) struct ObservedRepo {
    root: PathBuf,
    git_dir: PathBuf,
    main: PathBuf,
    placed: bool,
    refs: Refs,
    entries: StatusEntries,
    status_options: StatusOptions,
    shortstat_base: Option<Option<String>>,
    diff_stat: Option<Value>,
}

pub(super) struct Refreshed {
    pub head_changed: bool,
    pub worktree_changed: bool,
}

impl ObservedRepo {
    pub(super) async fn read(
        cwd: &Path,
        home: &Path,
        config: &Value,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Option<Self>> {
        let output = run(
            cwd,
            &["rev-parse".into(), "--show-toplevel".into()],
            env,
            cancel.clone(),
            READ_LIMIT,
            READ_TIMEOUT,
        )
        .await?;
        if output.status != 0 {
            if output
                .stderr
                .to_lowercase()
                .contains("not a git repository")
            {
                return Ok(None);
            }
            bail!("{}", output.stderr.trim());
        }
        let root = PathBuf::from(String::from_utf8(output.stdout)?.trim());
        let (git_dir, main, placed, (remote, status_options)) = tokio::try_join!(
            git_dir(&root, env, cancel.clone()),
            crate::projects::worktrees::repo_root(&root, cancel.clone()),
            crate::projects::worktrees::in_worktrees_root(home, config, &root),
            refs_config(&root, env, cancel),
        )?;
        let output = status(&root, env, cancel, status_options, true, &[]).await?;
        let (header, entries) = parse_status(&output)?;
        let refs = read_refs(&root, &git_dir, Some(header), remote, env, cancel).await?;
        Ok(Some(Self {
            root,
            git_dir,
            main,
            placed,
            refs,
            entries,
            status_options,
            shortstat_base: None,
            diff_stat: None,
        }))
    }

    /// An unchanged dirty path can still change a comparison's contents,
    /// whereas a scoped read with no old or new status entries need not wake
    /// working-tree diff subscribers.
    pub(super) async fn refresh(
        &mut self,
        change: &CheckoutChange,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Refreshed> {
        let mut full = change.index || change.config || matches!(change.paths, ChangedPaths::All);
        let (remote, status_options) = if change.refs || change.config {
            refs_config(&self.root, env, cancel).await?
        } else {
            (None, self.status_options)
        };
        // Conditional config includes can depend on the current branch.
        full |= status_options != self.status_options;
        let mut next_refs = None;
        let mut next_entries = None;
        let mut scoped = None;
        if change.refs || change.config {
            let output = if full {
                Some(status(&self.root, env, cancel, status_options, true, &[]).await?)
            } else {
                None
            };
            let header = if let Some(output) = &output {
                let (header, entries) = parse_status(output)?;
                next_entries = Some(entries);
                Some(header)
            } else {
                None
            };
            next_refs =
                Some(read_refs(&self.root, &self.git_dir, header, remote, env, cancel).await?);
        }
        let head_changed = next_refs
            .as_ref()
            .is_some_and(|refs| refs.head != self.refs.head);
        // A soft reset or update-ref can move HEAD without touching the index.
        // In that case the staged part of status must be compared to the new HEAD.
        full |= head_changed;
        if !full && let ChangedPaths::Paths(paths) = &change.paths {
            scoped = scoped_status(self, paths, env, cancel).await?;
            full = scoped.is_none();
        }
        if full && next_entries.is_none() {
            let output = status(&self.root, env, cancel, status_options, false, &[]).await?;
            next_entries = Some(parse_status(&output)?.1);
        }
        if let Some(refs) = next_refs {
            self.refs = refs;
        }
        let mut worktree_changed = full || change.attributes_changed();
        if let Some(entries) = next_entries {
            self.entries = entries;
        } else if let Some((scope, entries)) = scoped {
            self.entries.retain(|path, entry| {
                let untouched = !scope.overlaps(path, entry);
                worktree_changed |= !untouched;
                untouched
            });
            worktree_changed |=
                !entries.is_empty() || self.status_options.untracked == Untracked::Hide;
            self.entries.extend(entries);
        }
        self.status_options = status_options;
        if change.refs || change.config {
            self.shortstat_base = None;
        }
        if change.refs || change.config || worktree_changed {
            self.diff_stat = None;
        }
        Ok(Refreshed {
            head_changed,
            worktree_changed,
        })
    }

    pub(super) fn value(&self, cwd: &Path) -> Value {
        let refs = &self.refs;
        json!({"cwd":cwd,"isGit":true,"repoRoot":self.root,"mainRepoRoot":self.main,
            "currentBranch":refs.branch,"isDirty":!self.entries.is_empty(),
            "baseRef":refs.base.as_deref().map(branch_name),"aheadBehind":refs.ahead_behind,
            "upstreamRef":refs.upstream,"aheadOfOrigin":refs.upstream_counts.map(|c|c.0),
            "behindOfOrigin":refs.upstream_counts.map(|c|c.1),"hasRemote":refs.remote.is_some(),
            "remoteUrl":refs.remote,"isPaseoOwnedWorktree":self.placed && refs.base.is_some(),
            "error":null,"diffStat":null})
    }

    async fn read_diff_stat(
        &mut self,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Option<Value>> {
        if self.shortstat_base.is_none() {
            let Some(branch) = &self.refs.branch else {
                return Ok(None);
            };
            self.shortstat_base = Some(
                super::diff::shortstat_base(
                    &self.root,
                    branch,
                    self.refs.base.as_deref(),
                    env,
                    cancel,
                )
                .await?,
            );
        }
        match self
            .shortstat_base
            .as_ref()
            .and_then(|base| base.as_deref())
        {
            Some(base) => super::diff::shortstat(&self.root, base, env, cancel).await,
            None => Ok(None),
        }
    }

    pub(super) async fn diff_stat(
        &mut self,
        env: &BTreeMap<String, String>,
        cancel: CancellationToken,
    ) -> Value {
        if let Some(stat) = &self.diff_stat {
            return stat.clone();
        }
        let stat = self
            .read_diff_stat(env, &cancel)
            .await
            .unwrap_or_else(|error| {
                tracing::debug!(%error,"Checkout shortstat unavailable");
                None
            })
            .unwrap_or(Value::Null);
        if !cancel.is_cancelled() {
            self.diff_stat = Some(stat.clone());
        }
        stat
    }
}
/// Ahead/behind of HEAD against the operation base, preferring its origin
/// branch for short names; null when on the base itself or either is unknown.
async fn base_divergence(
    root: &Path,
    base: Option<&str>,
    current: Option<&str>,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    let (Some(base), Some(branch)) = (base, current) else {
        return Ok(Value::Null);
    };
    if branch_name(base) == branch {
        return Ok(Value::Null);
    }
    let comparison = if base.starts_with("refs/") {
        base.to_owned()
    } else {
        let remote = format!("refs/remotes/origin/{}", branch_name(base));
        if exists(root, &remote, env, cancel.clone()).await? {
            remote
        } else {
            base.to_owned()
        }
    };
    Ok(
        match divergence(root, "HEAD", &comparison, env, cancel).await? {
            Some((ahead, behind)) => json!({"ahead":ahead,"behind":behind}),
            None => Value::Null,
        },
    )
}
/// The source's `normalizeBranchSuggestionName`: a short branch name, or None for
/// blanks and the `HEAD`/`origin` pseudo names.
pub fn normalize_branch(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let name = trimmed
        .strip_prefix("refs/heads/")
        .or_else(|| trimmed.strip_prefix("refs/remotes/"))
        .unwrap_or(trimmed);
    let name = name.strip_prefix("origin/").unwrap_or(name);
    (!name.is_empty() && name != "HEAD" && name != "origin").then_some(name)
}
/// The source's `resolveBranchCheckout`.
pub async fn resolve_branch(
    cwd: &Path,
    name: &str,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    safe_ref(name)?;
    require_repo(cwd, env, cancel.clone()).await?;
    let Some(name) = normalize_branch(name) else {
        return Ok(json!({"kind":"not-found"}));
    };
    if exists(cwd, &format!("refs/heads/{name}"), env, cancel.clone()).await? {
        return Ok(json!({"kind":"local","name":name}));
    }
    if exists(cwd, &format!("refs/remotes/origin/{name}"), env, cancel).await? {
        return Ok(json!({"kind":"remote-only","name":name,"remoteRef":format!("origin/{name}")}));
    }
    Ok(json!({"kind":"not-found"}))
}
/// The source's `listBranchSuggestions`.
pub async fn branches(
    cwd: &Path,
    query: &str,
    limit: usize,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Vec<Value>> {
    require_repo(cwd, env, cancel.clone()).await?;
    let refs = text(
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%09%(committerdate:unix)%09%(objectname)",
            "refs/heads",
            "refs/remotes/origin",
        ],
        env,
        cancel.clone(),
    )
    .await?;
    let mut entries: BTreeMap<String, Value> = BTreeMap::new();
    for row in refs.lines() {
        let fields: Vec<_> = row.split('\t').collect();
        if fields.len() != 3 {
            continue;
        }
        let Some(name) = normalize_branch(fields[0]) else {
            continue;
        };
        let local = fields[0].starts_with("refs/heads/");
        let entry = entries.entry(name.into()).or_insert_with(
            || json!({"name":name,"committerDate":0,"hasLocal":false,"hasRemote":false}),
        );
        entry[if local { "hasLocal" } else { "hasRemote" }] = json!(true);
        entry["committerDate"] = json!(
            entry["committerDate"]
                .as_i64()
                .unwrap_or(0)
                .max(fields[1].parse().unwrap_or(0))
        );
    }
    let raw_query = query.trim().to_lowercase();
    let query = normalize_branch(&raw_query)
        .map(str::to_owned)
        .unwrap_or_else(|| raw_query.clone());
    let mut values: Vec<_> = entries
        .into_values()
        .filter(|v| {
            v["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase()
                .contains(&query)
        })
        .collect();
    values.sort_by(|a, b| {
        let an = a["name"].as_str().unwrap_or("");
        let bn = b["name"].as_str().unwrap_or("");
        bn.to_lowercase()
            .starts_with(&query)
            .cmp(&an.to_lowercase().starts_with(&query))
            .then_with(|| {
                b["committerDate"]
                    .as_i64()
                    .cmp(&a["committerDate"].as_i64())
            })
            .then_with(|| crate::json::locale_compare(an, bn))
    });
    values.truncate(limit.clamp(1, 200));
    for value in &mut values {
        if value["hasLocal"] == true && value["hasRemote"] == true {
            let name = value["name"].as_str().unwrap_or("");
            // A ref may vanish between listing and comparison; keep the branch
            // without divergence metadata, like the source.
            if let Some((a, b)) = divergence(
                cwd,
                &format!("refs/heads/{name}"),
                &format!("refs/remotes/origin/{name}"),
                env,
                cancel.clone(),
            )
            .await
            .ok()
            .flatten()
            {
                value["localAhead"] = json!(a);
                value["localBehind"] = json!(b);
            }
        }
    }
    Ok(values)
}
pub async fn stashes(
    cwd: &Path,
    paseo_only: bool,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Vec<Value>> {
    let out = text(cwd, &["stash", "list", "--format=%gd%x00%gs"], env, cancel).await?;
    Ok(out
        .lines()
        .filter_map(|line| {
            let (reference, subject) = line.split_once('\0')?;
            let branch = subject
                .split_once("paseo-auto-stash:")
                .map(|(_, s)| s.trim())
                .filter(|s| !s.is_empty());
            let is_paseo = subject.contains("paseo-auto-stash:");
            if paseo_only && !is_paseo {
                return None;
            }
            let index = reference
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse::<u64>()
                .ok()?;
            Some(json!({"index":index,"message":subject,"branch":branch,"isPaseo":is_paseo}))
        })
        .collect())
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use std::cell::RefCell;

    struct Repo {
        dir: tempfile::TempDir,
        env: BTreeMap<String, String>,
        cancel: CancellationToken,
    }

    impl Repo {
        async fn new() -> Result<Self> {
            let repo = Self {
                dir: tempfile::tempdir()?,
                env: BTreeMap::from([
                    ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                    (
                        "GIT_CONFIG_GLOBAL".into(),
                        if cfg!(windows) { "NUL" } else { "/dev/null" }.into(),
                    ),
                    ("GIT_AUTHOR_NAME".into(), "Observation test".into()),
                    ("GIT_AUTHOR_EMAIL".into(), "test@example.invalid".into()),
                    ("GIT_COMMITTER_NAME".into(), "Observation test".into()),
                    ("GIT_COMMITTER_EMAIL".into(), "test@example.invalid".into()),
                ]),
                cancel: CancellationToken::new(),
            };
            repo.git(&["init", "-b", "main"]).await?;
            Ok(repo)
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        async fn git(&self, args: &[&str]) -> Result<()> {
            text(self.root(), args, &self.env, self.cancel.clone()).await?;
            Ok(())
        }

        async fn write(&self, path: &str, content: &str) -> Result<()> {
            let path = self.root().join(path);
            tokio::fs::create_dir_all(path.parent().unwrap()).await?;
            tokio::fs::write(path, content).await?;
            Ok(())
        }

        async fn full(&self) -> Result<ObservedRepo> {
            ObservedRepo::read(
                self.root(),
                self.root(),
                &json!({}),
                &self.env,
                &self.cancel,
            )
            .await?
            .context("Test checkout disappeared")
        }

        async fn scoped(&self, observed: &mut ObservedRepo, paths: &[&str]) -> Result<()> {
            let change = CheckoutChange {
                paths: ChangedPaths::Paths(paths.iter().map(PathBuf::from).collect()),
                ..CheckoutChange::default()
            };
            observed.refresh(&change, &self.env, &self.cancel).await?;
            self.assert_full(observed).await
        }

        async fn assert_full(&self, observed: &ObservedRepo) -> Result<()> {
            let full = self.full().await?;
            assert_eq!(observed.entries, full.entries);
            assert_eq!(observed.value(self.root()), full.value(self.root()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn scoped_observation_matches_full_over_edit_sequence() -> Result<()> {
        let repo = Repo::new().await?;
        repo.write("tracked.txt", "before\n").await?;
        repo.write("other.txt", "before\n").await?;
        repo.git(&["add", "--all"]).await?;
        repo.git(&["commit", "-m", "base"]).await?;
        let mut observed = repo.full().await?;
        assert!(observed.entries.is_empty());

        // Keep a dirty entry outside each later scope to catch accidental
        // full replacement of the inventory by a scoped result.
        repo.write("other.txt", "outside scope\n").await?;
        repo.scoped(&mut observed, &["other.txt"]).await?;
        repo.write("tracked.txt", "modified\n").await?;
        repo.scoped(&mut observed, &["tracked.txt"]).await?;

        repo.write("untracked/one.txt", "new\n").await?;
        repo.write("untracked/two.txt", "new\n").await?;
        repo.scoped(&mut observed, &["untracked/one.txt", "untracked/two.txt"])
            .await?;
        tokio::fs::remove_file(repo.root().join("untracked/one.txt")).await?;
        repo.scoped(&mut observed, &["untracked/one.txt"]).await?;
        tokio::fs::remove_file(repo.root().join("tracked.txt")).await?;
        repo.scoped(&mut observed, &["tracked.txt"]).await?;

        repo.write("ignored/.gitignore", "*.log\n").await?;
        repo.scoped(&mut observed, &["ignored/.gitignore"]).await?;
        tokio::fs::rename(
            repo.root().join("untracked/two.txt"),
            repo.root().join("ignored/two.log"),
        )
        .await?;
        repo.scoped(&mut observed, &["untracked/two.txt", "ignored/two.log"])
            .await?;
        tokio::fs::rename(
            repo.root().join("ignored/two.log"),
            repo.root().join("untracked/renamed.log"),
        )
        .await?;
        repo.scoped(&mut observed, &["ignored/two.log", "untracked/renamed.log"])
            .await?;
        repo.write("untracked/.gitignore", "*.log\n").await?;
        repo.scoped(&mut observed, &["untracked/.gitignore"])
            .await?;
        assert!(
            !observed
                .entries
                .contains_key(Path::new("untracked/renamed.log"))
        );
        repo.write(".gitignore", "untracked/\nignored/\n").await?;
        repo.scoped(&mut observed, &[".gitignore"]).await?;
        Ok(())
    }

    #[tokio::test]
    async fn scoped_observation_keeps_both_rename_endpoints_and_literal_paths() -> Result<()> {
        let repo = Repo::new().await?;
        repo.write("from.txt", "original\n").await?;
        repo.write("[x].txt", "original\n").await?;
        repo.git(&["add", "--all"]).await?;
        repo.git(&["commit", "-m", "base"]).await?;
        repo.git(&["mv", "from.txt", "to.txt"]).await?;
        let mut observed = repo.full().await?;
        assert_eq!(
            observed.entries[Path::new("to.txt")].old_path(),
            Some(Path::new("from.txt"))
        );
        repo.write("to.txt", "modified\n").await?;
        repo.scoped(&mut observed, &["to.txt"]).await?;
        repo.write("from.txt", "untracked at rename source\n")
            .await?;
        repo.scoped(&mut observed, &["from.txt"]).await?;
        repo.write("[x].txt", "literal edit\n").await?;
        repo.scoped(&mut observed, &["[x].txt"]).await?;

        repo.git(&["reset", "--hard", "HEAD"]).await?;
        tokio::fs::rename(repo.root().join("from.txt"), repo.root().join("to.txt")).await?;
        repo.git(&["add", "--intent-to-add", "to.txt"]).await?;
        let mut observed = repo.full().await?;
        assert_eq!(
            observed.entries[Path::new("to.txt")].old_path(),
            Some(Path::new("from.txt"))
        );
        repo.write("to.txt", "break the worktree rename\n").await?;
        repo.scoped(&mut observed, &["to.txt"]).await?;
        assert!(observed.entries[Path::new("to.txt")].old_path().is_none());
        Ok(())
    }

    #[tokio::test]
    async fn refs_and_path_refresh_only_run_their_own_commands() -> Result<()> {
        let repo = Repo::new().await?;
        repo.write("tracked.txt", "first\n").await?;
        repo.git(&["add", "--all"]).await?;
        repo.git(&["commit", "-m", "base"]).await?;
        repo.git(&[
            "remote",
            "add",
            "origin",
            "https://example.invalid/checkout",
        ])
        .await?;
        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
            .await?;
        repo.git(&["branch", "--set-upstream-to=origin/main"])
            .await?;
        repo.write("tracked.txt", "second\n").await?;
        repo.git(&["commit", "-am", "next"]).await?;
        let mut observed = repo.full().await?;
        assert_eq!(
            observed.value(repo.root())["upstreamRef"],
            "refs/remotes/origin/main"
        );
        assert_eq!(observed.value(repo.root())["aheadOfOrigin"], 1);

        repo.git(&["update-ref", "refs/remotes/origin/main", "HEAD"])
            .await?;
        let change = CheckoutChange {
            refs: true,
            ..CheckoutChange::default()
        };
        let commands = COMMAND_LOG
            .scope(RefCell::new(Vec::new()), async {
                assert!(
                    !observed
                        .refresh(&change, &repo.env, &repo.cancel)
                        .await?
                        .head_changed
                );
                Ok::<_, anyhow::Error>(COMMAND_LOG.with(|log| log.take()))
            })
            .await?;
        assert!(
            commands.iter().flatten().all(|arg| arg != "status"),
            "{commands:?}"
        );
        assert_eq!(observed.value(repo.root())["aheadOfOrigin"], 0);
        repo.assert_full(&observed).await?;

        repo.write("tracked.txt", "changed\n").await?;
        let change = CheckoutChange {
            paths: ChangedPaths::Paths([PathBuf::from("tracked.txt")].into()),
            ..CheckoutChange::default()
        };
        let commands = COMMAND_LOG
            .scope(RefCell::new(Vec::new()), async {
                observed.refresh(&change, &repo.env, &repo.cancel).await?;
                Ok::<_, anyhow::Error>(COMMAND_LOG.with(|log| log.take()))
            })
            .await?;
        assert_eq!(
            commands,
            [vec![
                "--literal-pathspecs",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--",
                "tracked.txt"
            ]]
        );
        repo.assert_full(&observed).await?;

        repo.git(&["update-ref", "HEAD", "HEAD^"]).await?;
        assert!(
            observed
                .refresh(
                    &CheckoutChange {
                        refs: true,
                        ..CheckoutChange::default()
                    },
                    &repo.env,
                    &repo.cancel
                )
                .await?
                .head_changed
        );
        repo.assert_full(&observed).await?;
        Ok(())
    }
}
