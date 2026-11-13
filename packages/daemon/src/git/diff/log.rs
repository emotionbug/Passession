//! Repository-wide history for the Git Log dashboard: paged commit graph rows,
//! the ref inventory and single-commit details. Read-only and cancellable.
use super::history::parse_files;
use super::*;

const DEFAULT_LIMIT: u64 = 300;
const MAX_LIMIT: u64 = 1000;
const TAG_LIMIT: usize = 1000;
const LOG_FORMAT: &str = "--format=%x1e%H%x00%h%x00%P%x00%an%x00%ae%x00%aI%x00%cI%x00%D%x00%s";
const COMMIT_FORMAT: &str =
    "--format=%x1e%H%x00%h%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%D%x00%s%x00%b%x00";
const REF_FORMAT: &str = "--format=%(refname)%00%(objectname)%00%(*objectname)%00%(upstream:short)%00%(upstream:track,nobracket)%00%(HEAD)%00%(committerdate:unix)%00%(*committerdate:unix)%00%(symref)";

/// A `checkout.log.request` revision that is malformed or does not name a commit.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct InvalidRevision(pub String);

/// `checkout.log.request` filters. Every present filter must match.
pub struct LogQuery<'a> {
    pub revision: Option<&'a str>,
    pub skip: Option<u64>,
    pub limit: Option<u64>,
    pub text: Option<&'a str>,
    pub author: Option<&'a str>,
    pub path: Option<&'a str>,
}

/// One page of `--date-order` history over all branches, remotes, tags and
/// HEAD, or over the commits reachable from one revision.
pub async fn log(
    cwd: &Path,
    query: &LogQuery<'_>,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        bail!("Invalid limit: {limit}");
    }
    if let Some(revision) = query.revision {
        command::request_ref(revision, "revision")
            .map_err(|error| InvalidRevision(error.to_string()))?;
    }
    if let Some(path) = query.path {
        command::safe_path(path)?;
    }
    let env = readonly(env);
    let cwd = root(cwd, &env, &cancel).await?;
    let head = resolve(&cwd, "HEAD", &env, &cancel).await?;
    let mut command = args(&["log", "--date-order", "--decorate=full", "--no-color", LOG_FORMAT]);
    command.push(format!("--skip={}", query.skip.unwrap_or(0)));
    command.push(format!("--max-count={}", limit + 1));
    if query.text.is_some() || query.author.is_some() {
        command.extend(["-i".to_owned(), "-F".to_owned()]);
    }
    if let Some(text) = query.text {
        command.push(format!("--grep={text}"));
    }
    if let Some(author) = query.author {
        command.push(format!("--author={author}"));
    }
    match query.revision {
        Some(revision) => {
            let sha = resolve(&cwd, &format!("{revision}^{{commit}}"), &env, &cancel)
                .await?
                .ok_or_else(|| InvalidRevision(format!("Unknown revision: {revision}")))?;
            command.push(sha);
        }
        None => {
            command.extend(["--branches", "--remotes", "--tags"].map(str::to_owned));
            // An unborn HEAD names no commit; the remaining refs may still exist.
            if head.is_some() {
                command.push("HEAD".to_owned());
            }
        }
    }
    if let Some(path) = query.path {
        command.push("--".to_owned());
        command.push(format!(":(literal){path}"));
    }
    let mut commits = parse_log(&git(&cwd, &command, &env, &cancel, INVENTORY_LIMIT).await?)?;
    let has_more = commits.len() as u64 > limit;
    commits.truncate(limit as usize);
    Ok(json!({"head":head,"commits":commits,"hasMore":has_more}))
}

/// Local branches with upstream tracking, every remote's branches and tags
/// (peeled to their commits), plus the HEAD state.
pub async fn refs(
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    let env = readonly(env);
    let cwd = root(cwd, &env, &cancel).await?;
    let sha = resolve(&cwd, "HEAD", &env, &cancel).await?;
    let branch = command::optional(
        &cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &env,
        cancel.clone(),
    )
    .await?;
    let output = git(
        &cwd,
        &args(&[
            "for-each-ref",
            REF_FORMAT,
            "refs/heads",
            "refs/remotes",
            "refs/tags",
        ]),
        &env,
        &cancel,
        INVENTORY_LIMIT,
    )
    .await?;
    let output = String::from_utf8_lossy(&output);
    let mut local = Vec::new();
    let mut remote = Vec::new();
    let mut tags = Vec::new();
    for line in output.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split('\0').collect();
        let [
            refname,
            object,
            peeled,
            upstream,
            track,
            current,
            date,
            peeled_date,
            symref,
        ] = fields[..]
        else {
            bail!("Unexpected for-each-ref record: {line}");
        };
        if !symref.is_empty() {
            continue;
        }
        if let Some(name) = refname.strip_prefix("refs/heads/") {
            let Some(date) = unix(date)? else {
                continue;
            };
            let (ahead, behind, gone) = tracking(upstream, track)?;
            local.push(json!({
                "name":name,
                "sha":object,
                "upstream":(!upstream.is_empty()).then_some(upstream),
                "ahead":ahead,
                "behind":behind,
                "gone":gone,
                "isCurrent":current == "*",
                "committerDate":date,
            }));
        } else if let Some(name) = refname.strip_prefix("refs/remotes/") {
            let (Some((remote_name, branch_name)), Some(date)) = (name.split_once('/'), unix(date)?)
            else {
                continue;
            };
            remote.push(json!({
                "name":name,
                "remote":remote_name,
                "branch":branch_name,
                "sha":object,
                "committerDate":date,
            }));
        } else if let Some(name) = refname.strip_prefix("refs/tags/") {
            let (sha, date) = if peeled.is_empty() {
                (object, date)
            } else {
                (peeled, peeled_date)
            };
            tags.push((unix(date)?, name.to_owned(), sha.to_owned()));
        }
    }
    local.sort_by_cached_key(|v| (lowercase(&v["name"]), v["name"].to_string()));
    remote.sort_by_cached_key(|v| {
        (
            lowercase(&v["remote"]),
            lowercase(&v["branch"]),
            v["name"].to_string(),
        )
    });
    tags.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    tags.truncate(TAG_LIMIT);
    let tags: Vec<Value> = tags
        .into_iter()
        .map(|(date, name, sha)| json!({"name":name,"sha":sha,"committerDate":date}))
        .collect();
    let detached = sha.is_some() && branch.is_none();
    Ok(json!({
        "head":{"sha":sha,"branch":branch,"detached":detached},
        "local":local,
        "remote":remote,
        "tags":tags,
    }))
}

/// Full metadata and first-parent changed files for one commit; `commit` is
/// null when the SHA does not resolve to a commit.
pub async fn commit(
    cwd: &Path,
    sha: &str,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<Value> {
    if !(4..=64).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Invalid commit SHA: {sha}");
    }
    let env = readonly(env);
    let cwd = root(cwd, &env, &cancel).await?;
    let Some(full) = resolve(&cwd, &format!("{sha}^{{commit}}"), &env, &cancel).await? else {
        return Ok(json!({"commit":null}));
    };
    let bytes = git(
        &cwd,
        &args(&[
            "show",
            &full,
            "--diff-merges=first-parent",
            "--decorate=full",
            "--no-color",
            COMMIT_FORMAT,
            "--raw",
            "--numstat",
            "-z",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
        ]),
        &env,
        &cancel,
        INVENTORY_LIMIT,
    )
    .await?;
    let record = bytes
        .strip_prefix(b"\x1e")
        .context("Missing git commit record")?;
    let mut fields = record.split(|b| *b == 0);
    let mut header = Vec::with_capacity(12);
    for _ in 0..12 {
        header.push(String::from_utf8_lossy(
            fields.next().context("Incomplete git commit header")?,
        ));
    }
    let files = parse_files(fields)?
        .into_iter()
        .map(|file| {
            let status = file
                .status
                .with_context(|| format!("Missing change status for {}", file.path))?;
            Ok(json!({
                "path":file.path,
                "oldPath":file.old_path,
                "additions":file.additions,
                "deletions":file.deletions,
                "status":status,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"commit":{
        "sha":header[0],
        "shortSha":header[1],
        "parents":header[2].split_whitespace().collect::<Vec<_>>(),
        "authorName":header[3],
        "authorEmail":header[4],
        "authorDate":header[5],
        "committerName":header[6],
        "committerEmail":header[7],
        "committerDate":header[8],
        "refs":decorations(&header[9]),
        "subject":header[10],
        "body":header[11].trim(),
        "files":files,
    }}))
}

/// `git rev-parse --verify --quiet <revision>`: the full SHA, or None.
async fn resolve(
    cwd: &Path,
    revision: &str,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    command::optional(
        cwd,
        &["rev-parse", "--verify", "--quiet", revision],
        env,
        cancel.clone(),
    )
    .await
}

fn parse_log(bytes: &[u8]) -> Result<Vec<Value>> {
    bytes
        .split(|b| *b == 0x1e)
        .filter(|record| !record.iter().all(u8::is_ascii_whitespace))
        .map(|record| {
            let record = String::from_utf8_lossy(record);
            let fields: Vec<&str> = record.splitn(9, '\0').collect();
            let [sha, short, parents, name, email, authored, committed, refs, subject] =
                fields[..]
            else {
                bail!("Incomplete git log record");
            };
            Ok(json!({
                "sha":sha,
                "shortSha":short,
                "parents":parents.split_whitespace().collect::<Vec<_>>(),
                "authorName":name,
                "authorEmail":email,
                "authorDate":authored,
                "committerDate":committed,
                "subject":subject.trim_end_matches('\n'),
                "refs":decorations(refs),
            }))
        })
        .collect()
}

/// `%D` with `--decorate=full` as ref labels: detached HEAD first, then local
/// branches (the checked-out one first), remote branches and tags, each kind
/// by case-insensitive name. Remote `HEAD` symbolic refs and other namespaces
/// are dropped.
fn decorations(text: &str) -> Vec<Value> {
    let mut labels = Vec::new();
    for item in text.split(", ").filter(|item| !item.is_empty()) {
        let (current, reference) = match item.split_once(" -> ") {
            Some(("HEAD", target)) => (true, target),
            Some((source, _)) => (false, source),
            None => (item == "HEAD", item),
        };
        let reference = reference.strip_prefix("tag: ").unwrap_or(reference);
        let (rank, kind, name) = if reference == "HEAD" {
            (0, "head", reference)
        } else if let Some(name) = reference.strip_prefix("refs/heads/") {
            (1, "local", name)
        } else if let Some(name) = reference.strip_prefix("refs/remotes/") {
            if name.ends_with("/HEAD") {
                continue;
            }
            (2, "remote", name)
        } else if let Some(name) = reference.strip_prefix("refs/tags/") {
            (3, "tag", name)
        } else {
            continue;
        };
        labels.push((
            rank,
            !current,
            name.to_lowercase(),
            json!({"name":name,"kind":kind,"isHead":current}),
        ));
    }
    labels.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    labels.into_iter().map(|(_, _, _, label)| label).collect()
}

/// `%(upstream:track,nobracket)`: ahead/behind counts, or `gone` when the
/// configured upstream ref no longer exists. No upstream yields nulls.
fn tracking(upstream: &str, track: &str) -> Result<(Option<u64>, Option<u64>, bool)> {
    if upstream.is_empty() {
        return Ok((None, None, false));
    }
    if track == "gone" {
        return Ok((None, None, true));
    }
    let (mut ahead, mut behind) = (0, 0);
    for part in track.split(", ").filter(|part| !part.is_empty()) {
        match part.split_once(' ') {
            Some(("ahead", count)) => ahead = count.parse()?,
            Some(("behind", count)) => behind = count.parse()?,
            _ => bail!("Unexpected upstream tracking: {track}"),
        }
    }
    Ok((Some(ahead), Some(behind), false))
}

fn unix(text: &str) -> Result<Option<i64>> {
    Ok(if text.is_empty() {
        None
    } else {
        Some(text.parse()?)
    })
}

fn lowercase(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_lowercase()
}

#[cfg(test)]
mod tests;
