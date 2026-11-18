//! Native implementation of the directory-suggestions discovery/retrieval policy.
//! The limit caps retained matches, not discovery: a later candidate can still
//! win on rank, while equal-ranked, locale-equivalent paths keep discovery order.
use crate::json::locale_compare;
use anyhow::{Result, bail};
use futures_util::{FutureExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    fs::Metadata,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(8);
const MAX_DEPTH: usize = 12;
const SCAN_LIMIT: usize = 20_000;
const NONE: usize = usize::MAX;
const HIDDEN: &[&str] = &[
    ".agents",
    ".claude",
    ".codex",
    ".cursor",
    ".github",
    ".opencode",
    ".paseo",
    ".vscode",
];
const IGNORED: &[&str] = &[
    "node_modules",
    "venv",
    "env",
    "virtualenv",
    "dist",
    "build",
    "target",
    "out",
    "coverage",
    "vendor",
    "__pycache__",
    ".git",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum Kind {
    Directory,
    File,
    Symlink,
}
impl Kind {
    fn wire(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::File => "file",
            Self::Symlink => unreachable!(),
        }
    }
    fn from_metadata(metadata: &Metadata) -> Option<Self> {
        if metadata.is_dir() {
            Some(Self::Directory)
        } else if metadata.is_file() {
            Some(Self::File)
        } else {
            None
        }
    }
}
#[derive(Clone)]
struct RawEntry {
    name: String,
    kind: Kind,
}
enum FsValue {
    Metadata(Metadata),
    Path(PathBuf),
    Entries(Arc<Vec<RawEntry>>),
}
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum Operation {
    Stat,
    Realpath,
    ReadDir,
}
type Pending = futures_util::future::Shared<BoxFuture<'static, Option<Arc<FsValue>>>>;
type FsKey = (Operation, PathBuf);
struct Listing {
    expires: Instant,
    stamp: Stamp,
    entries: Arc<Vec<RawEntry>>,
}
#[derive(Eq, PartialEq)]
struct Stamp {
    modified: Option<SystemTime>,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl Stamp {
    fn of(metadata: &Metadata) -> Self {
        Self {
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            changed: {
                use std::os::unix::fs::MetadataExt;
                (metadata.ctime(), metadata.ctime_nsec())
            },
        }
    }
}
struct BoundedCache<T> {
    values: HashMap<PathBuf, T>,
    order: VecDeque<PathBuf>,
}
impl<T> Default for BoundedCache<T> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}
impl<T> BoundedCache<T> {
    fn insert(&mut self, key: PathBuf, value: T, cap: usize) {
        if !self.values.contains_key(&key) {
            self.order.push_back(key.clone());
        }
        self.values.insert(key, value);
        while self.values.len() > cap {
            if let Some(key) = self.order.pop_front() {
                self.values.remove(&key);
            }
        }
    }
}
type GitPending = futures_util::future::Shared<BoxFuture<'static, Arc<HashSet<PathBuf>>>>;
struct GitListing {
    expires: Instant,
    paths: GitPending,
}
struct Shared {
    permits: Arc<Semaphore>,
    pending: Mutex<HashMap<FsKey, Pending>>,
    listings: Mutex<BoundedCache<Listing>>,
    ignored: Mutex<BoundedCache<GitListing>>,
}
static SHARED: LazyLock<Shared> = LazyLock::new(|| Shared {
    permits: Arc::new(Semaphore::new(2)),
    pending: Mutex::new(HashMap::new()),
    listings: Mutex::new(BoundedCache::default()),
    ignored: Mutex::new(BoundedCache::default()),
});
fn shared() -> &'static Shared {
    &SHARED
}

// The permit lives in the blocking closure, not the requesting future. A cancelled
// search cannot release a thread still stuck in a protected directory or mount.
async fn filesystem(
    operation: Operation,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Option<Arc<FsValue>>> {
    if cancel.is_cancelled() {
        bail!("Directory search cancelled")
    }
    let key = (operation, path.to_owned());
    let pending = {
        let mut calls = shared().pending.lock().unwrap();
        if let Some(pending) = calls.get(&key) {
            pending.clone()
        } else {
            let worker_key = key.clone();
            let cleanup_key = key.clone();
            let task = tokio::spawn(async move {
                let permit = shared().permits.clone().acquire_owned().await.ok()?;
                let value = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let path = &worker_key.1;
                    match operation {
                        Operation::Stat => std::fs::metadata(path).ok().map(FsValue::Metadata),
                        Operation::Realpath => std::fs::canonicalize(path).ok().map(FsValue::Path),
                        Operation::ReadDir => {
                            let mut entries = Vec::new();
                            if let Ok(dir) = std::fs::read_dir(path) {
                                for entry in dir.flatten() {
                                    let Ok(kind) = entry.file_type() else {
                                        continue;
                                    };
                                    let kind = if kind.is_dir() {
                                        Kind::Directory
                                    } else if kind.is_file() {
                                        Kind::File
                                    } else if kind.is_symlink() {
                                        Kind::Symlink
                                    } else {
                                        continue;
                                    };
                                    entries.push(RawEntry {
                                        name: entry.file_name().to_string_lossy().into_owned(),
                                        kind,
                                    });
                                }
                            }
                            entries.sort_by(|a, b| locale_compare(&a.name, &b.name));
                            Some(FsValue::Entries(Arc::new(entries)))
                        }
                    }
                    .map(Arc::new)
                })
                .await
                .ok()
                .flatten();
                shared().pending.lock().unwrap().remove(&cleanup_key);
                value
            });
            let pending = async move { task.await.ok().flatten() }.boxed().shared();
            calls.insert(key, pending.clone());
            pending
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("Directory search cancelled"),
        value = pending => Ok(value),
    }
}

async fn canonical(path: &Path, cancel: &CancellationToken) -> Result<Option<PathBuf>> {
    Ok(filesystem(Operation::Realpath, path, cancel)
        .await?
        .and_then(|v| match v.as_ref() {
            FsValue::Path(path) => Some(path.clone()),
            _ => None,
        }))
}
async fn metadata(path: &Path, cancel: &CancellationToken) -> Result<Option<Arc<FsValue>>> {
    filesystem(Operation::Stat, path, cancel).await
}
async fn kind(path: &Path, cancel: &CancellationToken) -> Result<Option<Kind>> {
    Ok(metadata(path, cancel)
        .await?
        .and_then(|v| match v.as_ref() {
            FsValue::Metadata(m) => Kind::from_metadata(m),
            _ => None,
        }))
}
async fn children(path: &Path, cancel: &CancellationToken) -> Result<Arc<Vec<RawEntry>>> {
    let Some(info) = metadata(path, cancel).await? else {
        return Ok(Arc::new(Vec::new()));
    };
    let FsValue::Metadata(info) = info.as_ref() else {
        unreachable!()
    };
    if !info.is_dir() {
        return Ok(Arc::new(Vec::new()));
    }
    let stamp = Stamp::of(info);
    if !cfg!(windows) {
        let listings = shared().listings.lock().unwrap();
        if let Some(cached) = listings.values.get(path)
            && cached.expires > Instant::now()
            && cached.stamp == stamp
        {
            return Ok(cached.entries.clone());
        }
    }
    let entries = match filesystem(Operation::ReadDir, path, cancel).await? {
        Some(value) => match value.as_ref() {
            FsValue::Entries(entries) => entries.clone(),
            _ => unreachable!(),
        },
        None => Arc::new(Vec::new()),
    };
    if !cfg!(windows) {
        shared().listings.lock().unwrap().insert(
            path.to_owned(),
            Listing {
                expires: Instant::now() + TTL,
                stamp,
                entries: entries.clone(),
            },
            4_000,
        );
    }
    Ok(entries)
}
async fn git_ignored(root: &Path, cancel: &CancellationToken) -> Result<Arc<HashSet<PathBuf>>> {
    let paths = {
        let mut cache = shared().ignored.lock().unwrap();
        let now = Instant::now();
        if let Some(cached) = cache.values.get(root).filter(|v| v.expires > now) {
            cached.paths.clone()
        } else {
            let root = root.to_owned();
            let worker_root = root.clone();
            let task = tokio::spawn(async move {
                let mut command = tokio::process::Command::new("git");
                command
                    .args([
                        "ls-files",
                        "-o",
                        "-i",
                        "--directory",
                        "--exclude-standard",
                        "-z",
                    ])
                    .current_dir(&worker_root)
                    .env("GIT_OPTIONAL_LOCKS", "0")
                    .kill_on_drop(true);
                let mut paths = HashSet::new();
                if let Ok(Ok(output)) =
                    tokio::time::timeout(Duration::from_secs(10), command.output()).await
                    && output.status.success()
                {
                    for path in String::from_utf8_lossy(&output.stdout)
                        .split('\0')
                        .filter(|p| !p.is_empty())
                    {
                        paths.insert(normalize(&worker_root.join(path.trim_end_matches('/'))));
                    }
                }
                Arc::new(paths)
            });
            let paths = async move { task.await.unwrap_or_else(|_| Arc::new(HashSet::new())) }
                .boxed()
                .shared();
            if cache.values.len() >= 256 {
                cache.values.retain(|_, v| v.expires > now);
                let retained: HashSet<_> = cache.values.keys().cloned().collect();
                cache.order.retain(|key| retained.contains(key));
            }
            cache.insert(
                root,
                GitListing {
                    expires: now + TTL,
                    paths: paths.clone(),
                },
                256,
            );
            paths
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("Directory search cancelled"),
        paths = paths => Ok(paths),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => result.push(other.as_os_str()),
        }
    }
    result
}
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(normalize(&if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    }))
}
fn inside(root: &Path, candidate: &Path) -> bool {
    if cfg!(windows) {
        let root = root.to_string_lossy().replace('\\', "/").to_lowercase();
        let candidate = candidate
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase();
        candidate == root || candidate.starts_with(&(root.trim_end_matches('/').to_owned() + "/"))
    } else {
        candidate.starts_with(root)
    }
}
fn relative_path<'a>(root: &Path, path: &'a Path) -> Cow<'a, Path> {
    if cfg!(windows) && inside(root, path) {
        Cow::Owned(path.components().skip(root.components().count()).collect())
    } else {
        Cow::Borrowed(path.strip_prefix(root).unwrap_or(path))
    }
}
fn relative(root: &Path, path: &Path) -> String {
    let suffix = relative_path(root, path);
    if suffix.as_os_str().is_empty() {
        ".".into()
    } else if cfg!(windows) {
        suffix.to_string_lossy().replace('\\', "/")
    } else {
        suffix.to_string_lossy().into_owned()
    }
}
struct Plan {
    path_query: bool,
    parent: String,
    term: String,
    normalized: String,
    browse_exact: bool,
}
fn parse_query(query: &str, root: &Path, configured: &Path, workspace: bool) -> Option<Plan> {
    let typed = query.trim().replace('\\', "/");
    let mut normalized = typed.clone();
    let mut rooted = false;
    let mut is_absolute = false;
    if !workspace && (normalized == "~" || normalized.starts_with("~/")) {
        rooted = true;
        normalized = normalized[1..].trim_start_matches('/').to_owned();
    }
    if Path::new(&normalized).is_absolute() {
        is_absolute = true;
        let trailing = normalized.ends_with('/');
        let path = normalize(Path::new(&normalized));
        let query_root = if inside(root, &path) {
            root
        } else if inside(configured, &path) {
            configured
        } else {
            return None;
        };
        rooted = true;
        normalized = relative(query_root, &path);
        if trailing && normalized != "." {
            normalized.push('/');
        }
    }
    if normalized.starts_with("./") {
        rooted = true;
        normalized = normalized[2..].trim_start_matches('/').to_owned();
    }
    let mut previous_slash = false;
    normalized.retain(|c| {
        let keep = c != '/' || !previous_slash;
        previous_slash = c == '/';
        keep
    });
    if normalized == "." && (rooted || typed == ".") {
        normalized.clear();
    }
    if normalized.is_empty() {
        if !rooted && typed != "." && !workspace {
            return None;
        }
        return Some(Plan {
            path_query: true,
            parent: String::new(),
            term: String::new(),
            normalized,
            browse_exact: false,
        });
    }
    if is_absolute && root.parent().is_none() && !normalized.contains('/') {
        return Some(Plan {
            path_query: true,
            parent: normalized.clone(),
            term: String::new(),
            normalized,
            browse_exact: true,
        });
    }
    let path_query = rooted || (workspace && normalized.contains('/'));
    let (parent, term) = if path_query {
        normalized.rsplit_once('/').unwrap_or(("", &normalized))
    } else {
        ("", normalized.as_str())
    };
    Some(Plan {
        path_query,
        parent: parent.to_owned(),
        term: term.to_owned(),
        normalized,
        browse_exact: false,
    })
}
struct Input {
    root: PathBuf,
    workspace: bool,
    plan: Plan,
    query: Vec<u16>,
    include_files: bool,
    include_directories: bool,
    suffix: bool,
    limit: usize,
    ignored: Arc<HashSet<PathBuf>>,
    excluded: Option<PathBuf>,
    cancel: CancellationToken,
}
impl Input {
    fn git_ignored(&self, path: &Path) -> bool {
        path.ancestors()
            .take_while(|p| *p != self.root && inside(&self.root, p))
            .any(|p| self.ignored.contains(p))
    }
    fn excluded(&self, path: &Path) -> bool {
        self.excluded
            .as_ref()
            .is_some_and(|root| inside(root, path))
    }
    fn discover(&self, raw: &RawEntry, resolved: &Path, kind: Kind, allow_excluded: bool) -> bool {
        if raw.kind == Kind::Symlink {
            if !inside(&self.root, resolved) || self.git_ignored(resolved) {
                return false;
            }
            if !allow_excluded && self.excluded(resolved) {
                return false;
            }
        } else if self.ignored.contains(resolved)
            || (!allow_excluded && self.excluded.as_deref() == Some(resolved))
        {
            return false;
        }
        if kind == Kind::File {
            return self.include_files && !raw.name.starts_with('.');
        }
        !IGNORED.contains(&raw.name.as_str())
            && (!raw.name.starts_with('.')
                || (self.workspace && HIDDEN.contains(&raw.name.as_str())))
    }
    fn format(&self, visible: &Path, kind: Kind) -> Value {
        json!({"path": if self.workspace { relative(&self.root, visible) } else { visible.to_string_lossy().into_owned() }, "kind": kind.wire()})
    }
}
#[derive(Default)]
struct ChildPaths {
    visible: PathBuf,
    resolved: PathBuf,
    hidden: bool,
}
impl ChildPaths {
    async fn resolve(
        &mut self,
        input: &Input,
        parent: &Path,
        visible_parent: &Path,
        raw: &RawEntry,
        allow_excluded: bool,
    ) -> Result<Option<Kind>> {
        self.resolved.clear();
        self.resolved.push(parent);
        self.resolved.push(&raw.name);
        let kind = if raw.kind == Kind::Symlink {
            let Some(resolved) = canonical(&self.resolved, &input.cancel).await? else {
                return Ok(None);
            };
            let Some(kind) = kind(&resolved, &input.cancel).await? else {
                return Ok(None);
            };
            self.resolved.clear();
            self.resolved.push(resolved);
            kind
        } else {
            raw.kind
        };
        if !input.discover(raw, &self.resolved, kind, allow_excluded) {
            return Ok(None);
        }
        self.visible.clear();
        self.visible.push(visible_parent);
        self.visible.push(&raw.name);
        self.hidden = raw.name.starts_with('.');
        Ok(Some(kind))
    }
}

// Nested round-robin, not BFS or DFS: every sibling subtree gets one result
// per round. Subtree listings are created only when their branch gets a turn.
struct RoundRobin {
    parent: PathBuf,
    visible: PathBuf,
    depth: usize,
    raw: Option<Arc<Vec<RawEntry>>>,
    index: usize,
    first_round: bool,
    active: VecDeque<Branch>,
}
struct Branch {
    walk: RoundRobin,
    started: bool,
}
impl RoundRobin {
    fn new(parent: PathBuf, visible: PathBuf, depth: usize) -> Self {
        Self {
            parent,
            visible,
            depth,
            raw: None,
            index: 0,
            first_round: true,
            active: VecDeque::new(),
        }
    }
    async fn next(
        &mut self,
        input: &Input,
        visited: &mut HashSet<PathBuf>,
        paths: &mut ChildPaths,
    ) -> Result<Option<Kind>> {
        if input.cancel.is_cancelled() {
            bail!("Directory search cancelled")
        }
        if self.first_round {
            if self.raw.is_none() {
                self.raw = Some(children(&self.parent, &input.cancel).await?);
            }
            let raw = self.raw.as_ref().unwrap();
            while self.index < raw.len() {
                let child = &raw[self.index];
                self.index += 1;
                let Some(kind) = paths
                    .resolve(input, &self.parent, &self.visible, child, false)
                    .await?
                else {
                    continue;
                };
                if kind == Kind::Directory && self.depth < MAX_DEPTH {
                    self.active.push_back(Branch {
                        walk: Self::new(
                            paths.resolved.clone(),
                            paths.visible.clone(),
                            self.depth + 1,
                        ),
                        started: false,
                    });
                }
                return Ok(Some(kind));
            }
            self.raw = None;
            self.first_round = false;
        }
        while let Some(mut branch) = self.active.pop_front() {
            if !branch.started {
                branch.started = true;
                if !visited.insert(branch.walk.parent.clone()) {
                    continue;
                }
            }
            if let Some(kind) = Box::pin(branch.walk.next(input, visited, paths)).await? {
                self.active.push_back(branch);
                return Ok(Some(kind));
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Rank {
    tier: usize,
    segment: usize,
    offset: usize,
    fuzzy: usize,
    depth: usize,
    kind: Kind,
}

struct RankedPath {
    rank: Rank,
    visible: Arc<Path>,
    order: usize,
}
impl RankedPath {
    fn compare(&self, rank: Rank, visible: &Path, order: usize) -> Ordering {
        self.rank
            .cmp(&rank)
            .then_with(|| {
                locale_compare(&self.visible.to_string_lossy(), &visible.to_string_lossy())
            })
            // The old stable sort kept discovery order for collation ties.
            .then_with(|| self.order.cmp(&order))
    }
}
impl Ord for RankedPath {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other.rank, &other.visible, other.order)
    }
}
impl PartialOrd for RankedPath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for RankedPath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for RankedPath {}

struct TopMatches {
    heap: BinaryHeap<RankedPath>,
    seen: HashSet<(Arc<Path>, Kind)>,
    limit: usize,
    next_order: usize,
}
impl TopMatches {
    fn new(limit: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(limit),
            seen: HashSet::with_capacity(limit),
            limit,
            next_order: 0,
        }
    }
    fn add(&mut self, rank: Rank, visible: &Path) {
        let order = self.next_order;
        self.next_order += 1;
        let full = self.heap.len() == self.limit;
        if full
            && !self
                .heap
                .peek()
                .unwrap()
                .compare(rank, visible, order)
                .is_gt()
        {
            return;
        }
        // Repeated visible paths of a given kind always score alike. Once
        // evicted, a later duplicate cannot outrank the new cutoff.
        let visible = Arc::<Path>::from(visible);
        if !self.seen.insert((visible.clone(), rank.kind)) {
            return;
        }
        let candidate = RankedPath {
            rank,
            visible,
            order,
        };
        if full {
            let mut worst = self.heap.peek_mut().unwrap();
            self.seen.remove(&(worst.visible.clone(), worst.rank.kind));
            *worst = candidate;
        } else {
            self.heap.push(candidate);
        }
    }
}

const SLASH: u16 = b'/' as u16;

struct Scorer {
    path: Vec<u16>,
    compact: CompactScore,
}
impl Scorer {
    fn new(query: &[u16]) -> Self {
        Self {
            path: Vec::new(),
            compact: CompactScore {
                query: query.iter().copied().filter(|&c| ascii_word(c)).collect(),
                path: Vec::new(),
                offsets: Vec::new(),
            },
        }
    }

    fn suggestible_rank(&mut self, input: &Input, paths: &ChildPaths, kind: Kind) -> Option<Rank> {
        if paths.hidden || (kind == Kind::Directory && !input.include_directories) {
            return None;
        }
        let relative = relative_path(&input.root, &paths.visible);
        let relative = relative.to_string_lossy();
        let relative = if relative.is_empty() { "." } else { &relative };
        self.path.clear();
        if relative.is_ascii() {
            self.path.extend(relative.bytes().map(|c| {
                if cfg!(windows) && c == b'\\' {
                    SLASH
                } else {
                    c.to_ascii_lowercase().into()
                }
            }));
        } else if cfg!(windows) {
            self.path
                .extend(relative.replace('\\', "/").to_lowercase().encode_utf16());
        } else {
            self.path.extend(relative.to_lowercase().encode_utf16());
        }
        let q = &input.query;
        if input.suffix && !input.plan.normalized.is_empty() {
            let mut path = self.path.rsplit(|&c| c == SLASH).filter(|p| !p.is_empty());
            if q.iter().all(|&c| c == SLASH)
                || !q
                    .rsplit(|&c| c == SLASH)
                    .filter(|p| !p.is_empty())
                    .all(|p| path.next() == Some(p))
            {
                return None;
            }
        }
        let rank = rank(&self.path, &mut self.compact, input, kind);
        (input.plan.normalized.is_empty() || input.suffix || rank.tier != 5).then_some(rank)
    }
}

struct CompactScore {
    query: Vec<u16>,
    path: Vec<u16>,
    offsets: Vec<usize>,
}
#[derive(Clone, Copy)]
struct Score {
    tier: usize,
    offset: usize,
    spread: usize,
}
fn ascii_word(c: u16) -> bool {
    c <= 127 && (c as u8).is_ascii_alphanumeric()
}
/// JavaScript `/\s/u`: Unicode White_Space plus U+FEFF, without U+0085 (NEL).
fn whitespace(c: u16) -> bool {
    char::from_u32(c as u32)
        .is_some_and(|c| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}
fn score_match(q: &[u16], text: &[u16]) -> Option<Score> {
    if q.is_empty() || q == text {
        return Some(Score {
            tier: 0,
            offset: 0,
            spread: NONE,
        });
    }
    let mut best: Option<Score> = None;
    for (offset, window) in text.windows(q.len()).enumerate() {
        if window != q {
            continue;
        }
        let starts = offset == 0 || !ascii_word(text[offset - 1]);
        let ends = offset + q.len() == text.len() || !ascii_word(text[offset + q.len()]);
        let tier = if starts && ends {
            1
        } else if offset == 0 {
            2
        } else if starts {
            3
        } else {
            4
        };
        if best.is_none_or(|b| (tier, offset) < (b.tier, b.offset)) {
            best = Some(Score {
                tier,
                offset,
                spread: NONE,
            });
        }
    }
    if best.is_some() {
        return best;
    }
    let mut qi = 0;
    let mut first = 0;
    for (i, &c) in text.iter().enumerate() {
        if whitespace(c) {
            qi = 0;
            continue;
        }
        if c != q[qi] {
            continue;
        }
        if qi == 0 {
            first = i;
        }
        qi += 1;
        if qi == q.len() {
            return Some(Score {
                tier: 5,
                offset: first,
                spread: i - first + 1,
            });
        }
    }
    None
}
impl CompactScore {
    fn score_path(&mut self, q: &[u16], text: &[u16]) -> Option<Score> {
        if let Some(score) = score_match(q, text) {
            return Some(score);
        }
        if self.query.is_empty() {
            return None;
        }
        self.path.clear();
        self.offsets.clear();
        for (i, &c) in text.iter().enumerate() {
            if ascii_word(c) {
                self.path.push(c);
                self.offsets.push(i);
            }
        }
        let mut score = score_match(&self.query, &self.path)?;
        score.offset = self.offsets[score.offset];
        Some(score)
    }
}
fn fuzzy_subsequence(q: &[u16], text: &[u16]) -> Option<usize> {
    if q.is_empty() {
        return None;
    }
    let mut qi = 0;
    for (i, c) in text.iter().enumerate() {
        if *c == q[qi] {
            qi += 1;
            if qi == q.len() {
                return Some(i + 1 - q.len());
            }
        }
    }
    None
}
fn rank(path: &[u16], compact: &mut CompactScore, input: &Input, kind: Kind) -> Rank {
    let segments = path.split(|&c| c == SLASH);
    let q = &input.query;
    let mut rank = Rank {
        tier: 5,
        segment: NONE,
        offset: NONE,
        fuzzy: NONE,
        depth: if path == [b'.' as u16] {
            0
        } else {
            segments.clone().count()
        },
        kind,
    };
    if input.plan.path_query {
        if !q.is_empty()
            && let Some(s) = compact.score_path(q, path)
        {
            rank.tier = s.tier.min(4);
            rank.offset = s.offset;
            rank.fuzzy = s.spread;
        }
        return rank;
    }
    if q.is_empty() {
        rank.tier = 3;
        return rank;
    }
    let offset = path.windows(q.len()).position(|w| w == q).unwrap_or(NONE);
    let fuzzy = fuzzy_subsequence(q, segments.clone().next_back().unwrap_or(&[])).unwrap_or(NONE);
    for tier in 0..3 {
        if let Some(index) = segments.clone().position(|s| match tier {
            0 => s == q,
            1 => s.starts_with(q),
            _ => s.windows(q.len()).any(|w| w == q),
        }) {
            rank.tier = tier;
            rank.segment = index;
            rank.offset = offset;
            rank.fuzzy = fuzzy;
            return rank;
        }
    }
    if if input.workspace {
        path.starts_with(q)
    } else {
        offset != NONE
    } {
        rank.tier = 3;
        rank.offset = offset;
        rank.fuzzy = fuzzy;
    } else if fuzzy != NONE {
        rank.tier = 4;
        rank.offset = offset;
        rank.fuzzy = fuzzy;
    } else if let Some(s) = compact.score_path(q, path) {
        rank.tier = s.tier.min(4);
        rank.offset = s.offset;
        rank.fuzzy = s.spread;
    }
    rank
}
async fn exact_entry(input: &Input) -> Result<Option<(PathBuf, Kind)>> {
    if input.plan.normalized.is_empty() {
        return Ok(None);
    }
    let visible = normalize(&input.root.join(&input.plan.normalized));
    let Some(resolved) = canonical(&visible, &input.cancel).await? else {
        return Ok(None);
    };
    if !inside(&input.root, &resolved) {
        return Ok(None);
    }
    let Some(kind) = kind(&resolved, &input.cancel).await? else {
        return Ok(None);
    };
    if (kind == Kind::Directory && !input.include_directories)
        || (kind == Kind::File && !input.include_files)
    {
        return Ok(None);
    }
    Ok(Some((visible, kind)))
}
fn wants_exact(input: &Input) -> bool {
    input.plan.browse_exact || (input.plan.path_query && (input.suffix || !input.workspace))
}

async fn scan_matches(input: &Input, mut add: impl FnMut(Rank, &Path)) -> Result<()> {
    let typed = !input.workspace && input.plan.path_query;
    let mut start = (input.root.clone(), input.root.clone());
    let mut browse =
        (input.plan.path_query && input.plan.normalized.is_empty()) || input.plan.browse_exact;
    if typed && !input.plan.parent.is_empty() && !browse {
        let visible = normalize(&input.root.join(&input.plan.parent));
        if let Some(resolved) = canonical(&visible, &input.cancel).await?
            && inside(&input.root, &resolved)
            && kind(&resolved, &input.cancel).await? == Some(Kind::Directory)
        {
            browse = input.excluded(&resolved);
            start = (resolved, visible);
        }
    }
    let mut scorer = Scorer::new(&input.query);
    let mut paths = ChildPaths::default();
    if browse {
        let visible = normalize(&input.root.join(&input.plan.parent));
        if let Some(parent) = canonical(&visible, &input.cancel).await?
            && inside(&input.root, &parent)
            && !input.git_ignored(&parent)
        {
            let allow_excluded = input.excluded(&parent);
            for raw in children(&parent, &input.cancel).await?.iter() {
                let Some(kind) = paths
                    .resolve(input, &parent, &visible, raw, allow_excluded)
                    .await?
                else {
                    continue;
                };
                if let Some(rank) = scorer.suggestible_rank(input, &paths, kind) {
                    add(rank, &paths.visible);
                }
            }
        }
    } else {
        let mut visited = HashSet::from([start.0.clone()]);
        let mut walk = RoundRobin::new(start.0, start.1, 1);
        let mut scanned = 0;
        let mut confident = false;
        let confident_query_length = if input.plan.normalized.is_empty() {
            input.plan.term.encode_utf16().count()
        } else {
            input.plan.normalized.encode_utf16().count()
        };
        while let Some(kind) = walk.next(input, &mut visited, &mut paths).await? {
            scanned += 1;
            if let Some(rank) = scorer.suggestible_rank(input, &paths, kind) {
                confident |=
                    rank.tier < 4 || (rank.tier == 4 && rank.fuzzy <= confident_query_length * 2);
                add(rank, &paths.visible);
            }
            if scanned >= SCAN_LIMIT || (!input.workspace && scanned >= 5_000 && confident) {
                break;
            }
        }
    }
    Ok(())
}

async fn search(input: &Input) -> Result<Vec<Value>> {
    let exact = if wants_exact(input) {
        exact_entry(input).await?
    } else {
        None
    };
    let mut entries = Vec::with_capacity(input.limit);
    if let Some((visible, kind)) = exact.as_ref() {
        entries.push(input.format(visible, *kind));
        if input.limit == 1 {
            return Ok(entries);
        }
    }
    let mut ranked = TopMatches::new(input.limit - entries.len());
    scan_matches(input, |rank, visible| {
        if !exact
            .as_ref()
            .is_some_and(|(path, kind)| path == visible && *kind == rank.kind)
        {
            ranked.add(rank, visible);
        }
    })
    .await?;
    entries.extend(
        ranked
            .heap
            .into_sorted_vec()
            .into_iter()
            .map(|entry| input.format(&entry.visible, entry.rank.kind)),
    );
    Ok(entries)
}

/// Returns the directory_suggestions_response payload. Transport owns the envelope
/// and converts propagated failures into its source-compatible error payload.
pub async fn suggest(
    request: &Value,
    user_home: &Path,
    cancel: CancellationToken,
) -> Result<Value> {
    let cwd = request
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let workspace = cwd.is_some();
    let configured = match cwd {
        Some("~") => user_home.to_owned(),
        Some(value) if value.starts_with("~/") => user_home.join(&value[2..]),
        Some(value) => PathBuf::from(value),
        None => user_home.to_owned(),
    };
    let configured = absolute(&configured)?;
    let mut entries = Vec::new();
    if let Some(root) = canonical(&configured, &cancel).await?
        && kind(&root, &cancel).await? == Some(Kind::Directory)
    {
        let ignored = if workspace {
            git_ignored(&root, &cancel).await?
        } else {
            Arc::new(HashSet::new())
        };
        let excluded = if !workspace && cfg!(target_os = "macos") {
            canonical(&user_home.join("Library"), &cancel).await?
        } else {
            None
        };
        let include_files = request
            .get("includeFiles")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let include_directories = request
            .get("includeDirectories")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if (include_files || include_directories)
            && let Some(plan) = parse_query(
                request.get("query").and_then(Value::as_str).unwrap_or(""),
                &root,
                &configured,
                workspace,
            )
        {
            let query = if plan.path_query {
                &plan.normalized
            } else {
                &plan.term
            }
            .to_lowercase()
            .encode_utf16()
            .collect();
            let limit = request
                .get("limit")
                .and_then(Value::as_f64)
                .filter(|n| n.is_finite())
                .unwrap_or(30.0)
                .trunc()
                .clamp(1.0, 100.0) as usize;
            entries = search(&Input {
                root,
                workspace,
                plan,
                query,
                include_files,
                include_directories,
                suffix: request.get("matchMode").and_then(Value::as_str) == Some("suffix"),
                limit,
                ignored,
                excluded,
                cancel,
            })
            .await?;
        }
    }
    let directories: Vec<_> = entries
        .iter()
        .filter(|v| v["kind"] == "directory")
        .map(|v| &v["path"])
        .collect();
    let mut response = json!({ "directories": directories, "entries": entries, "error": null });
    if let Some(id) = request.get("requestId") {
        response["requestId"] = id.clone();
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }
    fn directory(root: &Path, path: &str) {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    fn file(root: &Path, path: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    async fn query(root: &Path, value: Value) -> Value {
        suggest(&value, root, CancellationToken::new())
            .await
            .unwrap()
    }
    fn paths(response: &Value) -> Vec<&str> {
        response["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect()
    }
    fn input(root: &Path, query: &str) -> Input {
        search_input(root, query, false)
    }

    fn search_input(root: &Path, query: &str, workspace: bool) -> Input {
        let root = std::fs::canonicalize(root).unwrap();
        let plan = parse_query(query, &root, &root, workspace).unwrap();
        Input {
            query: if plan.path_query {
                &plan.normalized
            } else {
                &plan.term
            }
            .to_lowercase()
            .encode_utf16()
            .collect(),
            root,
            workspace,
            plan,
            include_files: false,
            include_directories: true,
            suffix: false,
            limit: 30,
            ignored: Arc::new(HashSet::new()),
            excluded: None,
            cancel: CancellationToken::new(),
        }
    }

    fn old_sort_matches(
        mut ranked: Vec<(Rank, PathBuf)>,
        exact: Option<(PathBuf, Kind)>,
    ) -> Vec<(PathBuf, Kind)> {
        ranked.sort_by(|(a, ap), (b, bp)| {
            a.cmp(b)
                .then_with(|| locale_compare(&ap.to_string_lossy(), &bp.to_string_lossy()))
        });
        let mut seen = HashSet::new();
        exact
            .into_iter()
            .chain(ranked.into_iter().map(|(rank, path)| (path, rank.kind)))
            .filter(|entry| seen.insert(entry.clone()))
            .collect()
    }

    #[tokio::test]
    async fn top_matches_equal_full_sort_across_fixture_queries_and_limits() {
        use rand::{Rng, SeedableRng, rngs::StdRng};

        let temp = fixture();
        let mut rng = StdRng::seed_from_u64(1);
        for index in 0..144 {
            let folder =
                ["src", "src/inner", "other/src", "notes", "misc", ""][rng.random_range(0..6)];
            let name = ["file", "profile", "f-ile", "notes", "file-notes"][rng.random_range(0..5)];
            let path = format!("{folder}/{name}-{index:03}.ts");
            file(temp.path(), path.trim_start_matches('/'));
        }
        for path in [
            "file",
            "src/file-notes.ts",
            "notes/src/file-notes.ts",
            "notes/file 5 notes.ts",
            "misc/ΟΣ.txt",
            "generated/file-notes.ts",
            ".file-notes.ts",
            "node_modules/file-notes.ts",
        ] {
            file(temp.path(), path);
        }
        directory(temp.path(), "notes/file");
        for (query, suffix, workspace) in [
            ("file", false, true),
            ("file-not", false, true),
            ("flnt", false, true),
            ("file 5 notes", false, true),
            ("οσ", false, true),
            ("ος", false, true),
            ("", false, true),
            ("not-found", false, true),
            ("src/file-notes.ts", true, true),
            ("~/src/file-not", false, false),
        ] {
            let mut input = search_input(temp.path(), query, workspace);
            input.include_files = true;
            input.suffix = suffix;
            input.ignored = Arc::new(HashSet::from([input.root.join("generated")]));
            let mut ranked = Vec::new();
            scan_matches(&input, |rank, path| ranked.push((rank, path.to_owned())))
                .await
                .unwrap();
            let exact = if wants_exact(&input) {
                exact_entry(&input).await.unwrap()
            } else {
                None
            };
            let expected = old_sort_matches(ranked, exact);
            for limit in [1, 2, 7, 30, 50, 100] {
                input.limit = limit;
                assert_eq!(
                    search(&input).await.unwrap(),
                    expected
                        .iter()
                        .take(limit)
                        .map(|(path, kind)| input.format(path, *kind))
                        .collect::<Vec<_>>(),
                    "query={query:?} suffix={suffix} workspace={workspace} limit={limit}",
                );
            }
        }
    }

    #[test]
    fn top_matches_keep_stable_locale_ties_and_deduplicate_by_path_and_kind() {
        // Canonically equivalent spellings collate equally but are distinct
        // paths (tested outside the filesystem to avoid host normalization).
        let composed = Path::new("file-é");
        let decomposed = Path::new("file-e\u{301}");
        assert_ne!(composed, decomposed);
        assert!(locale_compare(composed.to_str().unwrap(), decomposed.to_str().unwrap()).is_eq());
        let file_rank = Rank {
            tier: 1,
            segment: 0,
            offset: 0,
            fuzzy: 0,
            depth: 1,
            kind: Kind::File,
        };
        let dir_rank = Rank {
            kind: Kind::Directory,
            ..file_rank
        };
        let stream = [
            (file_rank, Path::new("zzz")),
            (file_rank, composed),
            (file_rank, decomposed),
            (file_rank, composed),
            (dir_rank, composed),
            (file_rank, Path::new("aaa")),
            (file_rank, Path::new("zzz")),
            (file_rank, Path::new("bbb")),
        ];
        let expected = old_sort_matches(
            stream
                .iter()
                .map(|(rank, path)| (*rank, path.to_path_buf()))
                .collect(),
            None,
        );
        for limit in 1..=stream.len() {
            let mut top = TopMatches::new(limit);
            for (rank, path) in stream {
                top.add(rank, path);
                assert!(top.heap.len() <= limit);
                assert_eq!(top.seen.len(), top.heap.len());
            }
            let sorted = top.heap.into_sorted_vec();
            assert!(
                sorted
                    .iter()
                    .map(|entry| (&*entry.visible, entry.rank.kind))
                    .eq(expected
                        .iter()
                        .take(limit)
                        .map(|(path, kind)| (path.as_path(), *kind))),
                "limit={limit}",
            );
        }
    }

    #[tokio::test]
    async fn name_search_stops_when_cancelled_after_a_match() {
        let temp = fixture();
        directory(temp.path(), "first-match");
        directory(temp.path(), "second-match");
        let input = input(temp.path(), "match");
        let mut matches = 0;
        let error = scan_matches(&input, |_, _| {
            matches += 1;
            input.cancel.cancel();
        })
        .await
        .unwrap_err();
        assert_eq!(matches, 1);
        assert_eq!(error.to_string(), "Directory search cancelled");
    }

    #[tokio::test]
    async fn distinguishes_home_discovery_from_workspace_browsing() {
        let temp = fixture();
        directory(temp.path(), "projects/paseo");
        file(temp.path(), "readme.txt");
        assert!(paths(&query(temp.path(), json!({"query": ""})).await).is_empty());
        let response = query(
            temp.path(),
            json!({"query": "", "cwd": temp.path(), "includeFiles": true, "requestId": "picker-1"}),
        )
        .await;
        assert_eq!(paths(&response), ["projects", "readme.txt"]);
        assert_eq!(response["directories"], json!(["projects"]));
        assert_eq!(response["error"], Value::Null);
        assert_eq!(response["requestId"], "picker-1");
        let response = query(temp.path(), json!({"query": "~"})).await;
        assert_eq!(
            response["directories"],
            json!([std::fs::canonicalize(temp.path()).unwrap().join("projects")])
        );
        let response = query(temp.path(), json!({"query": "", "cwd": "  ~  "})).await;
        assert_eq!(paths(&response), ["projects"]);
    }

    #[tokio::test]
    async fn ranks_basename_fuzzy_and_compacted_path_matches() {
        let temp = fixture();
        for name in [
            "msgrndr",
            "msgrndr-panel.tsx",
            "use-msgrndr.ts",
            "message-renderer.tsx",
        ] {
            file(temp.path(), &format!("src/components/{name}"));
        }
        directory(temp.path(), "blankpage/editor");
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": "msgrndr", "includeFiles": true, "includeDirectories": false})).await;
        assert_eq!(
            paths(&response),
            [
                "src/components/msgrndr",
                "src/components/msgrndr-panel.tsx",
                "src/components/use-msgrndr.ts",
                "src/components/message-renderer.tsx"
            ]
        );
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "blank page editor"}),
        )
        .await;
        assert_eq!(paths(&response), ["blankpage/editor"]);
    }

    #[test]
    fn subsequence_resets_only_on_javascript_whitespace() {
        let units = |s: &str| s.encode_utf16().collect::<Vec<_>>();
        // U+0085 is Unicode White_Space but not JavaScript `\s`.
        let score = score_match(&units("ab"), &units("a\u{85}b")).unwrap();
        assert_eq!((score.tier, score.offset, score.spread), (5, 0, 3));
        assert!(score_match(&units("ab"), &units("a\u{feff}b")).is_none());
        assert!(score_match(&units("ab"), &units("a b")).is_none());
    }

    #[tokio::test]
    async fn suffix_retrieves_hidden_exact_paths_without_discovering_them() {
        let temp = fixture();
        file(temp.path(), ".dev/cache/daemon.log");
        file(temp.path(), ".claude/settings.json");
        file(temp.path(), "src/file.ts");
        file(temp.path(), "packages/app/src/file.ts");
        file(temp.path(), "src/my-file.ts");
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": "src/file.ts", "includeFiles": true, "matchMode": "suffix"})).await;
        assert_eq!(
            paths(&response),
            ["src/file.ts", "packages/app/src/file.ts"]
        );
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": ".dev/cache/daemon.log", "includeFiles": true, "matchMode": "suffix", "limit": 1})).await;
        assert_eq!(paths(&response), [".dev/cache/daemon.log"]);
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "daemon", "includeFiles": true}),
        )
        .await;
        assert!(paths(&response).is_empty());
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "settings", "includeFiles": true}),
        )
        .await;
        assert_eq!(paths(&response), [".claude/settings.json"]);
    }

    #[tokio::test]
    async fn ignores_build_trees_and_obeys_depth_and_limit() {
        let temp = fixture();
        for name in IGNORED {
            directory(temp.path(), &format!("{name}/match"));
        }
        directory(temp.path(), "visible/match");
        let deep = format!("{}match", "deep/".repeat(MAX_DEPTH));
        directory(temp.path(), &deep);
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": "match"})).await;
        assert_eq!(paths(&response), ["visible/match"]);
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "", "limit": 0}),
        )
        .await;
        assert_eq!(paths(&response).len(), 1);
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "", "includeDirectories": false}),
        )
        .await;
        assert!(paths(&response).is_empty());
    }

    #[tokio::test]
    async fn nested_round_robin_reaches_each_sibling_subtree() {
        let temp = fixture();
        directory(temp.path(), "work/client/team/paseo-desktop");
        for index in 0..10 {
            directory(temp.path(), &format!("work/archive/noise-{index:02}"));
        }
        let input = input(temp.path(), "paseo-desktop");
        let mut walk = RoundRobin::new(input.root.clone(), input.root.clone(), 1);
        let mut visited = HashSet::from([input.root.clone()]);
        let mut scanned = Vec::new();
        let mut paths = ChildPaths::default();
        for _ in 0..8 {
            walk.next(&input, &mut visited, &mut paths)
                .await
                .unwrap()
                .unwrap();
            scanned.push(relative(&input.root, &paths.visible));
        }
        assert!(
            scanned
                .iter()
                .any(|p| p == "work/client/team/paseo-desktop")
        );
    }

    #[tokio::test]
    async fn typed_home_parent_anchors_but_workspace_fragments_do_not() {
        let temp = fixture();
        directory(temp.path(), "Developer/pasta");
        directory(temp.path(), "Archive/Developer/pasta");
        let response = query(temp.path(), json!({"query": "~/Developer/pas"})).await;
        assert_eq!(
            response["directories"],
            json!([std::fs::canonicalize(temp.path())
                .unwrap()
                .join("Developer/pasta")])
        );
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "Developer/pas"}),
        )
        .await;
        assert_eq!(
            paths(&response),
            ["Developer/pasta", "Archive/Developer/pasta"]
        );
        let response = query(temp.path(), json!({"query": "~/Devel/pas"})).await;
        assert_eq!(paths(&response).len(), 2);
    }

    #[tokio::test]
    async fn excluded_paths_allow_exact_retrieval_and_single_level_browsing() {
        let temp = fixture();
        directory(temp.path(), "Library/sub/match");
        let mut input = input(temp.path(), "match");
        input.excluded = Some(input.root.join("Library"));
        assert!(search(&input).await.unwrap().is_empty());
        input.plan = parse_query("~/Library/", &input.root, &input.root, false).unwrap();
        input.query = input
            .plan
            .normalized
            .to_lowercase()
            .encode_utf16()
            .collect();
        let entries = search(&input).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0]["path"],
            input.root.join("Library").to_string_lossy().as_ref()
        );
        assert_eq!(
            entries[1]["path"],
            input.root.join("Library/sub").to_string_lossy().as_ref()
        );
    }

    #[tokio::test]
    async fn git_ignore_filters_discovery_not_exact_retrieval() {
        let temp = fixture();
        let status = tokio::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(temp.path())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        std::fs::write(temp.path().join(".gitignore"), "generated/\n").unwrap();
        file(temp.path(), "generated/search-notes.md");
        file(temp.path(), "src/search-notes.md");
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "search-notes", "includeFiles": true}),
        )
        .await;
        assert_eq!(paths(&response), ["src/search-notes.md"]);
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": "generated/search-notes.md", "includeFiles": true, "matchMode": "suffix", "limit": 1})).await;
        assert_eq!(paths(&response), ["generated/search-notes.md"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_preserve_visible_names_and_cannot_escape_root() {
        use std::os::unix::fs::symlink;
        let temp = fixture();
        let outside = fixture();
        directory(temp.path(), ".linked/project");
        directory(outside.path(), "secret");
        symlink(temp.path().join(".linked"), temp.path().join("visible")).unwrap();
        symlink(temp.path(), temp.path().join("cycle")).unwrap();
        symlink(outside.path(), temp.path().join("escape")).unwrap();
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": "project"})).await;
        assert_eq!(paths(&response), ["visible/project"]);
        let response = query(
            temp.path(),
            json!({"cwd": temp.path(), "query": "escape/secret", "matchMode": "suffix"}),
        )
        .await;
        assert!(paths(&response).is_empty());
    }

    #[tokio::test]
    async fn cancellation_and_listing_invalidation() {
        let temp = fixture();
        directory(temp.path(), "first");
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": ""})).await;
        assert_eq!(paths(&response), ["first"]);
        directory(temp.path(), "second");
        let response = query(temp.path(), json!({"cwd": temp.path(), "query": ""})).await;
        assert_eq!(paths(&response), ["first", "second"]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            suggest(&json!({"query": "~"}), temp.path(), cancel)
                .await
                .is_err()
        );
    }
}
