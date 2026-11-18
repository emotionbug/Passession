use super::{ChangedPaths, CheckoutChange, GitService, command, watch_tree};
use crate::state::AppState;
use anyhow::{Result, anyhow, bail};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    runtime::Handle,
    sync::{Notify, Semaphore, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const DEBOUNCE: Duration = Duration::from_secs(1);
// A refresh is one path-scoped status, so sustained edits can surface every two seconds.
const MAX_WAIT: Duration = Duration::from_secs(2);
const MAX_PATHS: usize = 1_000;
const FETCH_INTERVAL: Duration = Duration::from_secs(180);
const POLL_BASE: Duration = Duration::from_secs(5);
const POLL_MAX: Duration = Duration::from_secs(60);
const RECOVERY_BASE: Duration = Duration::from_secs(30);
const RECOVERY_MAX: Duration = Duration::from_secs(300);
const RELEASE_GRACE: Duration = Duration::from_secs(30);

fn setup_limit() -> &'static Semaphore {
    static LIMIT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));
    &LIMIT
}
fn refresh_limit() -> &'static Semaphore {
    static LIMIT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(4));
    &LIMIT
}

#[derive(Clone)]
struct Demand {
    count: usize,
    root: PathBuf,
    git_dir: PathBuf,
    aliases: HashMap<PathBuf, usize>,
    is_git: bool,
    lifetime: CancellationToken,
}
type Demands = Arc<StdMutex<HashMap<PathBuf, Demand>>>;

/// One actor per common Git directory. Dropping its final repository ownership
/// cancels active work and closes every watcher, timer and background fetch.
pub(super) struct Observation {
    demands: Demands,
    demand_changed: Arc<Notify>,
    cancel: CancellationToken,
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Message {
    root: PathBuf,
    event: notify::Result<Event>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Worktree,
    Common,
    Private,
}
struct Watch {
    kind: Kind,
    native: Option<Registered>,
    failed: Arc<AtomicBool>,
    overflow: Arc<AtomicBool>,
    detached: BTreeSet<PathBuf>,
    retry_at: Instant,
    attempts: u32,
    established: Option<Instant>,
    poll_at: Instant,
    poll_interval: Duration,
    poll_stamp: Option<MetadataStamp>,
}

struct Registered {
    watcher: RecommendedWatcher,
    directories: BTreeSet<PathBuf>,
    metadata_dirs: BTreeSet<PathBuf>,
    visible: BTreeSet<PathBuf>,
    tracked_ignored: BTreeSet<PathBuf>,
    ignore_config: Vec<u8>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
enum PathKind {
    #[default]
    File,
    Unknown,
    Directory,
}

#[derive(Default)]
struct PathEvent {
    kind: PathKind,
    replace: bool,
}

#[derive(Default)]
struct Pending {
    paths: BTreeMap<PathBuf, PathEvent>,
    all: bool,
    refs: bool,
    index: bool,
    config: bool,
    tree_all: bool,
    check_config: bool,
    retarget: bool,
    metadata_dirs: bool,
    rescan: bool,
}

impl Pending {
    fn refresh_all(&mut self, metadata: bool) {
        self.all = true;
        self.refs |= metadata;
        self.index |= metadata;
        self.config |= metadata;
    }

    fn rescan(&mut self, is_git: bool) {
        self.rescan_worktree(is_git);
        self.refresh_all(is_git);
        self.retarget = is_git;
        self.check_config = is_git;
        self.metadata_dirs = is_git;
    }

    fn rescan_worktree(&mut self, is_git: bool) {
        self.refresh_all(false);
        self.paths.clear();
        self.rescan = is_git;
        self.tree_all = is_git;
    }

    fn apply_metadata_changes(&mut self, changes: [bool; Metadata::ALL.len()]) {
        for (class, changed) in Metadata::ALL.into_iter().zip(changes) {
            if changed {
                class.apply(self);
            }
        }
        self.retarget |= changes[Metadata::Refs as usize];
    }

    fn add_path(&mut self, path: PathBuf, kind: PathKind, replace: bool) {
        if self.rescan || (self.all && kind == PathKind::File && !is_ignore_file(&path)) {
            return;
        }
        let event = self.paths.entry(path).or_default();
        event.kind = event.kind.max(kind);
        event.replace |= replace;
        if self.paths.len() > MAX_PATHS {
            // Once status is known to require a full refresh, keep only
            // events that can change the set of watched directories.
            self.all = true;
            self.paths
                .retain(|path, event| event.kind != PathKind::File || is_ignore_file(path));
            if self.paths.len() > MAX_PATHS {
                self.rescan_worktree(true);
            }
        }
    }
}

#[derive(Default)]
struct CheckoutClock {
    due: Option<Instant>,
    first: Option<Instant>,
    pending: Pending,
}

impl CheckoutClock {
    fn debounce(&mut self, now: Instant) {
        let first = *self.first.get_or_insert(now);
        self.due = Some((now + DEBOUNCE).min(first + MAX_WAIT));
    }

    fn take_pending(&mut self) -> Pending {
        self.due = None;
        self.first = None;
        std::mem::take(&mut self.pending)
    }
}

#[derive(Default)]
struct RefTargets {
    common: BTreeSet<PathBuf>,
    private: BTreeSet<PathBuf>,
    unborn: Option<PathBuf>,
}

impl GitService {
    pub fn observe_legacy_client(&self, context: &crate::transport::RequestContext) {
        // COMPAT(checkoutObservation): added in v0.13.35, remove after 2027-05-17 once native clients advertise checkout_observation.
        if crate::transport::has_capability(context.capabilities(), "checkout_observation") {
            return;
        }
        let context = context.clone();
        let weak = self.app.clone();
        tokio::spawn(async move {
            let cancel = context.cancellation();
            let mut held = HashSet::new();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tick.tick() => {}
                }
                let Some(app) = weak.upgrade() else { break };
                let workspaces = if context.allows("workspace.read") {
                    match app.projects.list_workspaces().await {
                        Ok(workspaces) => workspaces,
                        Err(error) => {
                            tracing::debug!(%error, "Legacy checkout demand unavailable");
                            continue;
                        }
                    }
                } else {
                    Vec::new()
                };
                let desired: HashSet<PathBuf> = workspaces
                    .iter()
                    .filter(|workspace| workspace["archivedAt"].is_null())
                    .filter_map(|workspace| workspace["cwd"].as_str().map(PathBuf::from))
                    .collect();
                let added: Vec<_> = desired.difference(&held).cloned().collect();
                for cwd in added {
                    if cancel.is_cancelled() {
                        break;
                    }
                    if let Err(error) = app.git.retain_observation(&cwd).await {
                        tracing::debug!(%error, cwd=%cwd.display(), "Legacy checkout observation unavailable");
                    } else {
                        app.git.retain_legacy_shortstat(&cwd).await;
                        held.insert(cwd);
                    }
                }
                let removed: Vec<_> = held.difference(&desired).cloned().collect();
                for cwd in removed {
                    app.git.release_legacy_shortstat(&cwd).await;
                    let _ = app.git.release_observation(&cwd).await;
                    held.remove(&cwd);
                }
            }
            if let Some(app) = weak.upgrade() {
                for cwd in held {
                    app.git.release_legacy_shortstat(&cwd).await;
                    let _ = app.git.release_observation(&cwd).await;
                }
            }
        });
    }

    /// Retain once per presented checkout or diff subscription.
    pub async fn retain_observation(&self, cwd: &Path) -> Result<()> {
        let _permit = tokio::select! { _ = self.cancel.cancelled() => bail!("Git service stopped"), permit = setup_limit().acquire() => permit? };
        let key = self.key(cwd).await?;
        let env = self.environment()?;
        let (common, root, dir, is_git) =
            observation_roots(&key, &env, self.cancel.child_token()).await?;
        let mut observers = self.observers.lock().await;
        if self.cancel.is_cancelled() {
            bail!("Git service stopped");
        }
        self.install_observation(
            &mut observers,
            common,
            key.clone(),
            Demand {
                count: 1,
                root,
                git_dir: dir,
                aliases: HashMap::from([(cwd.to_owned(), 1)]),
                is_git,
                lifetime: self.cancel.child_token(),
            },
        )?;
        if is_git {
            self.retain_forge_observation(&key, 1).await;
        }
        drop(observers);
        Ok(())
    }

    pub async fn release_observation(&self, cwd: &Path) -> Result<()> {
        let app = self.app.clone();
        let cwd = cwd.to_owned();
        let cancel = self.cancel.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(RELEASE_GRACE) => {}
            }
            if let Some(app) = app.upgrade() {
                let _ = app.git.release_observation_now(&cwd).await;
            }
        });
        Ok(())
    }

    async fn release_observation_now(&self, cwd: &Path) -> Result<()> {
        // Remembered aliases allow release after a symlink or checkout is removed.
        let key = tokio::fs::canonicalize(cwd)
            .await
            .unwrap_or_else(|_| cwd.to_owned());
        let mut observers = self.observers.lock().await;
        let mut empty = Vec::new();
        let mut released = None;
        for (common, observer) in observers.iter() {
            let mut demands = observer
                .demands
                .lock()
                .map_err(|_| anyhow!("Git observation state poisoned"))?;
            let key = demands
                .iter()
                .find_map(|(stored, demand)| {
                    (stored == &key || demand.aliases.contains_key(cwd)).then(|| stored.clone())
                })
                .unwrap_or_else(|| key.clone());
            if let Some(demand) = demands.get_mut(&key) {
                demand.count -= 1;
                if let Some(count) = demand.aliases.get_mut(cwd) {
                    *count -= 1;
                    if *count == 0 {
                        demand.aliases.remove(cwd);
                    }
                }
                if demand.is_git {
                    released = Some(key.clone());
                }
                if demand.count == 0
                    && let Some(demand) = demands.remove(&key)
                {
                    demand.lifetime.cancel();
                }
                if demands.is_empty() {
                    empty.push(common.clone());
                } else {
                    observer.demand_changed.notify_one();
                }
                break;
            }
        }
        for common in empty {
            observers.remove(&common);
        }
        if let Some(key) = released {
            self.release_forge_observation(&key).await;
        }
        drop(observers);
        Ok(())
    }

    /// Snapshot refreshes may request watcher reconciliation, but never retain
    /// another reference. In particular refresh -> snapshot cannot leak demand.
    pub(super) async fn ensure_observation(
        &self,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        cancel: CancellationToken,
    ) -> Result<()> {
        let previous = {
            let observers = self.observers.lock().await;
            let mut previous = None;
            for (root, observer) in observers.iter() {
                let demands = observer
                    .demands
                    .lock()
                    .map_err(|_| anyhow!("Git observation state poisoned"))?;
                if let Some(demand) = demands.get(cwd) {
                    if !demand.is_git {
                        previous = Some(root.clone());
                    }
                    break;
                }
            }
            previous
        };
        // A directory demand survives `git init`: move the same references to
        // the newly discovered common-dir actor without adding new consumers.
        if let Some(previous) = previous {
            let (common, root, git_dir, is_git) = observation_roots(cwd, env, cancel).await?;
            if is_git {
                let mut observers = self.observers.lock().await;
                let (demand, empty) = if let Some(observer) = observers.get(&previous) {
                    let mut demands = observer
                        .demands
                        .lock()
                        .map_err(|_| anyhow!("Git observation state poisoned"))?;
                    (demands.remove(cwd), demands.is_empty())
                } else {
                    (None, false)
                };
                if empty {
                    observers.remove(&previous);
                }
                if let Some(mut demand) = demand {
                    let consumers = demand.count;
                    demand.root = root;
                    demand.git_dir = git_dir;
                    demand.is_git = true;
                    self.install_observation(&mut observers, common, cwd.to_owned(), demand)?;
                    self.retain_forge_observation(cwd, consumers).await;
                }
            }
        }
        Ok(())
    }

    fn install_observation(
        &self,
        observers: &mut HashMap<PathBuf, Observation>,
        common: PathBuf,
        key: PathBuf,
        incoming: Demand,
    ) -> Result<()> {
        if let Some(observer) = observers.get(&common) {
            let mut demands = observer
                .demands
                .lock()
                .map_err(|_| anyhow!("Git observation state poisoned"))?;
            if let Some(demand) = demands.get_mut(&key) {
                demand.count = demand
                    .count
                    .checked_add(incoming.count)
                    .ok_or_else(|| anyhow!("Too many Git observation consumers"))?;
                for (alias, count) in incoming.aliases {
                    *demand.aliases.entry(alias).or_default() += count;
                }
            } else {
                demands.insert(key, incoming);
            }
            observer.demand_changed.notify_one();
            return Ok(());
        }
        let demands = Arc::new(StdMutex::new(HashMap::from([(key, incoming)])));
        let (events, receiver) = mpsc::channel(1_024);
        let demand_changed = Arc::new(Notify::new());
        let cancel = self.cancel.child_token();
        observers.insert(
            common.clone(),
            Observation {
                demands: demands.clone(),
                demand_changed: demand_changed.clone(),
                cancel: cancel.clone(),
            },
        );
        let actor = Actor {
            app: self.app.clone(),
            common,
            demands,
            demand_changed,
            events,
            receiver,
            cancel,
            watches: BTreeMap::new(),
            clocks: HashMap::new(),
            ref_targets: HashMap::new(),
            next_fetch: Instant::now(),
            fetch_task: None,
        };
        tokio::spawn(actor.run());
        Ok(())
    }
}

async fn observation_roots(
    cwd: &Path,
    env: &BTreeMap<String, String>,
    cancel: CancellationToken,
) -> Result<(PathBuf, PathBuf, PathBuf, bool)> {
    let root = command::run(
        cwd,
        &["rev-parse".into(), "--show-toplevel".into()],
        env,
        cancel.clone(),
        command::READ_LIMIT,
        command::READ_TIMEOUT,
    )
    .await?;
    if root.status != 0 {
        if root.stderr.to_lowercase().contains("not a git repository") {
            let cwd = tokio::fs::canonicalize(cwd).await?;
            return Ok((cwd.clone(), cwd.clone(), cwd, false));
        }
        bail!("{}", root.stderr);
    }
    let root = PathBuf::from(String::from_utf8(root.stdout)?.trim_end_matches(['\r', '\n']));
    let common = PathBuf::from(
        command::text(
            cwd,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            env,
            cancel.clone(),
        )
        .await?,
    );
    let dir = command::git_dir(cwd, env, cancel).await?;
    Ok((
        tokio::fs::canonicalize(common).await?,
        tokio::fs::canonicalize(root).await?,
        tokio::fs::canonicalize(dir).await?,
        true,
    ))
}

struct Actor {
    app: Weak<AppState>,
    common: PathBuf,
    demands: Demands,
    demand_changed: Arc<Notify>,
    events: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    cancel: CancellationToken,
    watches: BTreeMap<PathBuf, Watch>,
    clocks: HashMap<PathBuf, CheckoutClock>,
    ref_targets: HashMap<PathBuf, RefTargets>,
    next_fetch: Instant,
    fetch_task: Option<tokio::task::JoinHandle<bool>>,
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(task) = self.fetch_task.take() {
            task.abort();
        }
    }
}
impl Actor {
    fn demands(&self) -> HashMap<PathBuf, Demand> {
        self.demands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    async fn run(mut self) {
        self.reconcile_demands();
        while !self.cancel.is_cancelled() && !self.clocks.is_empty() {
            self.reconcile_overflow();
            self.schedule_recovery().await;
            self.refresh_ready().await;
            if self.next_fetch <= Instant::now() && self.fetch_task.is_none() {
                self.start_fetch();
                self.next_fetch = Instant::now() + FETCH_INTERVAL;
            }
            let deadline = self
                .fetch_task
                .is_none()
                .then_some(self.next_fetch)
                .into_iter()
                .chain(self.clocks.values().filter_map(|clock| clock.due))
                .chain(
                    self.watches
                        .values()
                        .filter(|w| w.native.is_none())
                        .map(|w| w.retry_at.min(w.poll_at)),
                )
                .min();
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {},
                result = async { match self.fetch_task.as_mut() { Some(task)=>task.await, None=>std::future::pending().await } }, if self.fetch_task.is_some() => {
                    self.fetch_task = None;
                    if !matches!(result, Ok(false)) {
                        for demand in self.demands().values().filter(|d| d.is_git) {
                            self.queue(&demand.root, |pending| pending.refs = true);
                        }
                    }
                },
                _ = self.demand_changed.notified() => self.reconcile_demands(),
                message = self.receiver.recv() => match message {
                    Some(Message { root, event: Ok(event) }) => self.file_event(&root, event),
                    Some(Message { root, event: Err(error) }) => {
                        tracing::debug!(%error, path=%root.display(), "Git watcher degraded to polling");
                        // The callback sets failed even if the event channel is full.
                    }
                    None => break,
                },
            }
        }
    }

    fn reconcile_overflow(&mut self) {
        let mut lost = Vec::new();
        for (root, watch) in &mut self.watches {
            if !watch.overflow.swap(false, Ordering::AcqRel) {
                continue;
            }
            if watch.kind != Kind::Worktree
                && let Some(native) = &watch.native
            {
                watch
                    .detached
                    .extend(native.directories.iter().filter(|p| *p != root).cloned());
            }
            lost.push((root.clone(), watch.kind));
        }
        if lost.is_empty() {
            return;
        }
        for demand in self.demands().values() {
            for (root, kind) in &lost {
                match kind {
                    Kind::Worktree if root == &demand.root => {
                        self.queue(root, |pending| pending.rescan_worktree(demand.is_git));
                    }
                    Kind::Common if demand.is_git => {
                        self.queue(&demand.root, |pending| pending.rescan(true));
                    }
                    Kind::Private if root == &demand.git_dir => {
                        self.queue(&demand.root, |pending| pending.rescan(true));
                    }
                    _ => {}
                }
            }
        }
    }

    fn reconcile_demands(&mut self) {
        let demands = self.demands();
        let mut desired = BTreeMap::new();
        let mut roots = HashSet::new();
        let now = Instant::now();
        for demand in demands.values() {
            if roots.insert(demand.root.clone()) && !self.clocks.contains_key(&demand.root) {
                let mut clock = CheckoutClock {
                    due: Some(now),
                    ..Default::default()
                };
                clock.pending.rescan(demand.is_git);
                self.clocks.insert(demand.root.clone(), clock);
                self.next_fetch = now;
            }
            desired.insert(demand.root.clone(), Kind::Worktree);
            if demand.is_git {
                desired.insert(self.common.clone(), Kind::Common);
                if demand.git_dir != self.common {
                    desired.insert(demand.git_dir.clone(), Kind::Private);
                }
            }
        }
        self.clocks.retain(|root, _| roots.contains(root));
        let targets = self.ref_targets.len();
        self.ref_targets.retain(|root, _| roots.contains(root));
        self.watches.retain(|root, _| desired.contains_key(root));
        for (root, kind) in desired {
            self.watches
                .entry(root)
                .or_insert_with(|| Watch::new(kind, now));
        }
        if targets != self.ref_targets.len()
            && let Some(demand) = demands.values().find(|d| d.is_git)
        {
            self.queue(&demand.root, |pending| pending.metadata_dirs = true);
        }
    }

    async fn schedule_recovery(&mut self) {
        let now = Instant::now();
        let mut due = Vec::new();
        for (root, watch) in &mut self.watches {
            let failed = watch.failed.swap(false, Ordering::AcqRel) && watch.native.is_some();
            if failed {
                watch.mark_failed(now);
            }
            if watch.native.is_none() {
                let retry = watch.retry_at <= now;
                if retry || failed || watch.poll_at <= now {
                    watch.poll_at = now + watch.poll_interval;
                    due.push((root.clone(), watch.kind, retry, watch.attempts > 0));
                }
            }
        }
        if due.is_empty() {
            return;
        }
        let demands = self.demands();
        for (root, kind, retry, degraded) in due {
            let changes = if kind != Kind::Worktree && degraded {
                let owner = demands
                    .values()
                    .find(|d| d.git_dir == root)
                    .map(|d| d.root.as_path());
                self.poll_metadata(&root, owner).await
            } else {
                [false; Metadata::ALL.len()]
            };
            for demand in demands.values() {
                if (root != demand.root || kind != Kind::Worktree)
                    && (root != demand.git_dir || kind != Kind::Private)
                    && !(demand.is_git && kind == Kind::Common)
                {
                    continue;
                }
                if let Some(clock) = self.clocks.get_mut(&demand.root) {
                    clock.pending.refresh_all(false);
                    clock.pending.apply_metadata_changes(changes);
                    if retry {
                        clock.pending.tree_all |= demand.is_git && kind == Kind::Worktree;
                        clock.pending.metadata_dirs |= kind != Kind::Worktree;
                    }
                    clock.pending.retarget |= retry && kind != Kind::Worktree;
                    clock.due = Some(now);
                }
            }
        }
    }

    async fn poll_metadata(
        &mut self,
        root: &Path,
        owner: Option<&Path>,
    ) -> [bool; Metadata::ALL.len()] {
        let directories = self.desired_metadata(root, owner);
        let watched = root.to_owned();
        let cancel = self.cancel.child_token();
        let permit = tokio::select! {
            _ = cancel.cancelled() => return [false; Metadata::ALL.len()],
            permit = setup_limit().acquire() => match permit {
                Ok(permit) => permit,
                Err(_) => return [false; Metadata::ALL.len()],
            },
        };
        let read = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            MetadataStamp::read(&watched, &directories, &cancel)
        });
        match read.await {
            Ok(Ok(stamp)) => {
                let old = self
                    .watches
                    .get_mut(root)
                    .unwrap()
                    .poll_stamp
                    .replace(stamp);
                std::array::from_fn(|i| old.is_none_or(|old| stamp.0[i] != old.0[i]))
            }
            error => {
                tracing::debug!(?error, path=%root.display(), "Cannot classify degraded Git metadata; refreshing conservatively");
                [true; Metadata::ALL.len()]
            }
        }
    }

    fn desired_metadata(&self, root: &Path, owner: Option<&Path>) -> BTreeSet<PathBuf> {
        let common = root == self.common;
        let mut desired = metadata_directories(root, common);
        if common {
            for targets in self.ref_targets.values() {
                desired.extend(targets.common.iter().cloned());
            }
        } else if let Some(targets) = owner.and_then(|owner| self.ref_targets.get(owner)) {
            desired.extend(targets.private.iter().cloned());
        }
        desired
    }

    fn queue(&mut self, root: &Path, update: impl FnOnce(&mut Pending)) {
        if let Some(clock) = self.clocks.get_mut(root) {
            update(&mut clock.pending);
            clock.debounce(Instant::now());
        }
    }

    fn file_event(&mut self, root: &Path, event: Event) {
        let Some(watch) = self.watches.get_mut(root) else {
            return;
        };
        if event.need_rescan() {
            watch.overflow.store(true, Ordering::Release);
            return;
        }
        let kind = watch.kind;
        if event
            .paths
            .iter()
            .enumerate()
            .any(|(i, path)| path == root && detached(event.kind, i))
        {
            watch.failed.store(true, Ordering::Release);
            return;
        }
        match kind {
            Kind::Worktree => {
                let is_git = self.watches.contains_key(&self.common)
                    && self.watches[&self.common].kind == Kind::Common;
                if event
                    .paths
                    .iter()
                    .any(|path| worktree_path(root, path, is_git))
                {
                    self.queue(root, |pending| {
                        if !is_git {
                            pending.refresh_all(false);
                            return;
                        }
                        let kind = path_kind(event.kind);
                        let replace = can_replace(event.kind);
                        for path in event.paths {
                            if worktree_path(root, &path, true) {
                                pending.add_path(path, kind, replace);
                            }
                        }
                    });
                }
            }
            Kind::Common | Kind::Private => {
                let demands = self.demands();
                for path in &event.paths {
                    let Ok(relative) = path.strip_prefix(root) else {
                        continue;
                    };
                    let Some((class, owner_only)) = metadata_effect(relative) else {
                        continue;
                    };
                    let topology = self
                        .watches
                        .get(root)
                        .and_then(|w| w.native.as_ref())
                        .is_some_and(|native| native.metadata_dirs.contains(path));
                    if topology
                        && can_replace(event.kind)
                        && let Some(watch) = self.watches.get_mut(root)
                    {
                        watch.detached.insert(path.clone());
                    }
                    for demand in demands.values().filter(|d| {
                        d.is_git && (d.git_dir == root || (kind == Kind::Common && !owner_only))
                    }) {
                        let unborn = self
                            .ref_targets
                            .get(&demand.root)
                            .and_then(|t| t.unborn.as_ref())
                            == Some(path);
                        let origin_head = relative == Path::new("refs/remotes/origin/HEAD");
                        self.queue(&demand.root, |pending| {
                            class.apply(pending);
                            pending.metadata_dirs |= topology;
                            pending.retarget |= origin_head || unborn;
                        });
                    }
                }
            }
        }
    }

    async fn prepare_metadata(
        &mut self,
        demand: &Demand,
        pending: &mut Pending,
        env: &BTreeMap<String, String>,
    ) -> Result<()> {
        if !demand.is_git || !(pending.retarget || pending.metadata_dirs) {
            return Ok(());
        }
        let common = self.common.clone();
        let git_dir = &demand.git_dir;
        let roots = || std::iter::once(&common).chain((git_dir != &common).then_some(git_dir));
        let mut error = None;
        // Watch the roots before discovering refs, so HEAD/config updates
        // during startup cannot leave the graph anchored to stale refs.
        for root in roots() {
            if self.watches[root].native.is_none() && self.watches[root].retry_at <= Instant::now()
            {
                let watched = root.clone();
                if let Err(e) = self
                    .update_watch(root, self.cancel.child_token(), move |native| {
                        if !native.register(&watched)? {
                            bail!("Git metadata watch root disappeared");
                        }
                        Ok(())
                    })
                    .await
                {
                    error = Some(e);
                }
            }
        }
        if pending.retarget {
            match ref_targets(&common, demand, env).await {
                Ok(targets) => {
                    self.ref_targets.insert(demand.root.clone(), targets);
                }
                Err(e) => error = Some(e),
            }
        }
        for root in roots() {
            if self.watches[root].native.is_none() {
                continue;
            }
            let desired = self.desired_metadata(root, Some(&demand.root));
            let removed = std::mem::take(&mut self.watches.get_mut(root).unwrap().detached);
            let watched = root.clone();
            if let Err(e) = self
                .update_watch(root, self.cancel.child_token(), move |native| {
                    native.sync_metadata(&watched, desired, removed)
                })
                .await
            {
                error = Some(e);
            } else if self.watches[root].poll_stamp.is_some() {
                // Cover the gap between the last fallback probe and native
                // watch registration, including shared metadata for siblings.
                let changes = self.poll_metadata(root, Some(&demand.root)).await;
                self.watches.get_mut(root).unwrap().poll_stamp = None;
                pending.apply_metadata_changes(changes);
                if root == &common && changes.into_iter().any(|changed| changed) {
                    for other in self
                        .demands()
                        .values()
                        .filter(|d| d.is_git && d.root != demand.root)
                    {
                        self.queue(&other.root, |pending| {
                            pending.apply_metadata_changes(changes)
                        });
                    }
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    async fn prepare_worktree(
        &mut self,
        demand: &Demand,
        pending: &mut Pending,
        env: &BTreeMap<String, String>,
    ) -> Result<ChangedPaths> {
        let root = &demand.root;
        let watch = &self.watches[root];
        if watch.native.is_none() && watch.retry_at > Instant::now() {
            return Ok(ChangedPaths::All);
        }
        let paths = std::mem::take(&mut pending.paths);
        let topology = watch.native.is_none()
            || pending.tree_all
            || pending.index
            || pending.check_config
            || paths
                .iter()
                .any(|(path, event)| event.kind != PathKind::File || is_ignore_file(path));
        let (mut files, mut scopes) = if topology {
            let worktree = demand.is_git;
            let rebuild = pending.tree_all || watch.native.is_none();
            let reset = pending.rescan;
            let index = pending.index;
            let check_config = pending.check_config || watch.native.is_none();
            let environment = env.clone();
            let cancel = demand.lifetime.child_token();
            let runtime = Handle::current();
            let watched = root.clone();
            self.update_watch(root, cancel.clone(), move |native| {
                if !native.register(&watched)? {
                    bail!("Checkout watch root disappeared");
                }
                if !worktree {
                    return Ok((BTreeSet::new(), BTreeSet::new()));
                }
                let plan = WorktreePlan {
                    paths,
                    rebuild,
                    reset,
                    index,
                    check_config,
                };
                native.apply_worktree(&watched, plan, &environment, &cancel, &runtime)
            })
            .await?
        } else {
            (paths.into_keys().collect(), BTreeSet::new())
        };
        if pending.all || scopes.contains(root) {
            return Ok(ChangedPaths::All);
        }
        if !(files.is_empty() || pending.refs || pending.index || pending.config) {
            match watch_tree::ignored_paths(
                root,
                files.iter().map(PathBuf::as_path),
                false,
                env,
                &demand.lifetime,
            )
            .await
            {
                Ok(ignored) => {
                    files.retain(|file| !ignored.contains(file.strip_prefix(root).unwrap_or(file)))
                }
                Err(error) => {
                    // A check-ignore error must not suppress a real file change.
                    tracing::debug!(%error, path=%root.display(), "Git ignore filtering failed");
                    return Ok(ChangedPaths::All);
                }
            }
        }
        for file in files {
            insert_scope(&mut scopes, file, root);
        }
        if scopes.is_empty() {
            Ok(ChangedPaths::None)
        } else {
            Ok(ChangedPaths::Paths(
                scopes
                    .into_iter()
                    .map(|path| path.strip_prefix(root).unwrap().to_owned())
                    .collect(),
            ))
        }
    }

    async fn update_watch<T: Send + 'static>(
        &mut self,
        root: &Path,
        cancel: CancellationToken,
        update: impl FnOnce(&mut Registered) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let permit = tokio::select! {
            _ = cancel.cancelled() => bail!("Git watch update cancelled"),
            permit = setup_limit().acquire() => permit?,
        };
        let watch = self.watches.get_mut(root).unwrap();
        let kind = watch.kind;
        let previous = watch.native.take();
        let reconnect = previous.is_none();
        if reconnect {
            watch.failed = Arc::new(AtomicBool::new(false));
        }
        let watched = root.to_owned();
        let failure = watch.failed.clone();
        let overflow = watch.overflow.clone();
        let events = self.events.clone();
        let task = tokio::task::spawn_blocking(move || -> Result<_> {
            let _permit = permit;
            let mut native = match previous {
                Some(native) => native,
                None => Registered::open(watched, kind, events, failure, overflow)?,
            };
            let result = update(&mut native)?;
            Ok((native, result))
        });
        let result = tokio::select! {
            _ = cancel.cancelled() => Err(anyhow!("Git watch update cancelled")),
            result = task => result.map_err(anyhow::Error::from).and_then(|r| r),
        };
        match result {
            Ok((native, output)) => {
                if reconnect {
                    watch.established = Some(Instant::now());
                }
                watch.native = Some(native);
                Ok(output)
            }
            Err(error) => {
                watch.mark_failed(Instant::now());
                Err(error)
            }
        }
    }

    async fn refresh_ready(&mut self) {
        let now = Instant::now();
        let ready: Vec<_> = self
            .clocks
            .iter()
            .filter_map(|(root, clock)| {
                clock
                    .due
                    .is_some_and(|due| due <= now)
                    .then_some(root.clone())
            })
            .collect();
        if ready.is_empty() {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            self.cancel.cancel();
            return;
        };
        let Ok(env) = app.git.environment() else {
            self.cancel.cancel();
            return;
        };
        let demands = self.demands();
        for root in ready {
            let Some((cwd, demand)) = demands.iter().find(|(_, d)| d.root == root) else {
                self.reconcile_demands();
                continue;
            };
            if self.cancel.is_cancelled() {
                return;
            }
            let Some(clock) = self.clocks.get_mut(&root) else {
                continue;
            };
            let mut pending = clock.take_pending();
            if let Err(error) = self.prepare_metadata(demand, &mut pending, &env).await {
                tracing::debug!(%error, path=%root.display(), "Git metadata watcher reconciliation failed");
                let clock = self.clocks.get_mut(&root).unwrap();
                clock.pending.retarget = true;
                clock.pending.metadata_dirs = true;
                clock.due = Some(Instant::now() + POLL_BASE);
            }
            let paths = match self.prepare_worktree(demand, &mut pending, &env).await {
                Ok(paths) => paths,
                Err(error) => {
                    tracing::debug!(%error, path=%root.display(), "Git worktree watcher reconciliation failed");
                    ChangedPaths::All
                }
            };
            if matches!(paths, ChangedPaths::None)
                && !(pending.refs || pending.index || pending.config)
            {
                continue;
            }
            let _permit = tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = demand.lifetime.cancelled() => continue,
                permit = refresh_limit().acquire() => match permit { Ok(permit) => permit, Err(_) => return },
            };
            let change = CheckoutChange {
                refs: pending.refs,
                index: pending.index,
                config: pending.config,
                paths,
            };
            match app
                .git
                .refresh_observed(cwd, change, demand.lifetime.child_token())
                .await
            {
                Ok(snapshot) if !demand.lifetime.is_cancelled() => {
                    let changed = snapshot.is_some();
                    if let Some(snapshot) = snapshot
                        && let Err(error) = app.git.publish_checkout_snapshot(cwd, &snapshot).await
                    {
                        tracing::debug!(%error, "Checkout update publication failed");
                    }
                    for target in [&root, &demand.git_dir, &self.common] {
                        if let Some(watch) =
                            self.watches.get_mut(target).filter(|w| w.native.is_none())
                        {
                            watch.poll_interval = if changed {
                                POLL_BASE
                            } else {
                                (watch.poll_interval * 2).min(POLL_MAX)
                            };
                            watch.poll_at = Instant::now() + watch.poll_interval;
                        }
                    }
                }
                Err(error) if !demand.lifetime.is_cancelled() => {
                    tracing::debug!(%error, "Checkout observation refresh failed; retrying");
                    let clock = self.clocks.get_mut(&root).unwrap();
                    clock.pending.refresh_all(false);
                    clock.pending.refs |= pending.refs;
                    clock.pending.index |= pending.index;
                    clock.pending.config |= pending.config;
                    clock.due = Some(Instant::now() + POLL_BASE);
                }
                _ => {}
            }
        }
    }

    fn start_fetch(&mut self) {
        let Some((cwd, _)) = self.demands().into_iter().find(|(_, demand)| demand.is_git) else {
            return;
        };
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Ok(env) = app.git.environment() else {
            return;
        };
        let cancel = self.cancel.child_token();
        self.fetch_task = Some(tokio::spawn(async move {
            let origin = command::optional(
                &cwd,
                &["config", "--get", "remote.origin.url"],
                &env,
                cancel.clone(),
            )
            .await;
            if !matches!(origin, Ok(Some(_))) {
                return false;
            }
            let refs = ["for-each-ref", "--format=%(refname)%00%(objectname)"];
            let before = command::text(&cwd, &refs, &env, cancel.clone()).await;
            let result = command::checked(
                &cwd,
                &["fetch".into(), "origin".into(), "--prune".into()],
                &env,
                cancel.clone(),
                command::READ_LIMIT,
                command::MUTATION_TIMEOUT,
            )
            .await;
            if let Err(error) = result {
                tracing::debug!(%error,"Background Git fetch failed");
            }
            let after = command::text(&cwd, &refs, &env, cancel).await;
            !matches!((&before,&after),(Ok(before),Ok(after)) if before==after)
        }));
    }
}

impl Watch {
    fn new(kind: Kind, now: Instant) -> Self {
        Self {
            kind,
            native: None,
            failed: Arc::new(AtomicBool::new(false)),
            overflow: Arc::new(AtomicBool::new(false)),
            detached: BTreeSet::new(),
            retry_at: now,
            attempts: 0,
            established: None,
            poll_at: now + POLL_BASE,
            poll_interval: POLL_BASE,
            poll_stamp: None,
        }
    }

    fn mark_failed(&mut self, now: Instant) {
        self.native = None;
        self.detached.clear();
        if self
            .established
            .take()
            .is_some_and(|at| now.duration_since(at) >= RECOVERY_MAX)
        {
            self.attempts = 0;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at = now
            + RECOVERY_BASE
                .saturating_mul(1u32 << self.attempts.saturating_sub(1).min(4))
                .min(RECOVERY_MAX);
        self.poll_interval = POLL_BASE;
        self.poll_at = now + POLL_BASE;
    }
}

#[derive(Default)]
struct WorktreePlan {
    paths: BTreeMap<PathBuf, PathEvent>,
    rebuild: bool,
    reset: bool,
    index: bool,
    check_config: bool,
}

impl Registered {
    fn open(
        root: PathBuf,
        kind: Kind,
        events: mpsc::Sender<Message>,
        failure: Arc<AtomicBool>,
        overflow: Arc<AtomicBool>,
    ) -> Result<Self> {
        let watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            match &event {
                Ok(event) if event.need_rescan() => {}
                Ok(event) if matches!(event.kind, EventKind::Access(_)) => return,
                Ok(event)
                    if kind != Kind::Worktree
                        && !event.paths.iter().any(|path| {
                            path.strip_prefix(&root).is_ok_and(|relative| {
                                if relative.as_os_str().is_empty() {
                                    event
                                        .paths
                                        .iter()
                                        .enumerate()
                                        .any(|(i, p)| p == &root && detached(event.kind, i))
                                } else {
                                    metadata_effect(relative).is_some()
                                }
                            })
                        }) =>
                {
                    return;
                }
                Ok(_) => {}
                Err(_) => failure.store(true, Ordering::Release),
            }
            if events
                .try_send(Message {
                    root: root.clone(),
                    event,
                })
                .is_err()
            {
                overflow.store(true, Ordering::Release);
            }
        })?;
        Ok(Self {
            watcher,
            directories: BTreeSet::new(),
            metadata_dirs: BTreeSet::new(),
            visible: BTreeSet::new(),
            tracked_ignored: BTreeSet::new(),
            ignore_config: Vec::new(),
        })
    }

    fn register(&mut self, path: &Path) -> Result<bool> {
        if self.directories.contains(path) {
            return Ok(true);
        }
        if !watch_tree::is_dir(path)? {
            return Ok(false);
        }
        if let Err(error) = self.watcher.watch(path, RecursiveMode::NonRecursive) {
            if watch_tree::is_dir(path)? {
                return Err(error.into());
            }
            return Ok(false);
        }
        self.directories.insert(path.to_owned());
        Ok(true)
    }

    fn unregister(&mut self, path: &Path) -> Result<()> {
        if self.directories.remove(path) {
            match self.watcher.unwatch(path) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind,
                        notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn forget_subtree(&mut self, base: &Path, retain_base: bool) -> Result<()> {
        let removed: Vec<_> = watch_tree::subtree(&self.directories, base)
            .filter(|path| !retain_base || *path != base)
            .cloned()
            .collect();
        for path in removed.iter().rev() {
            self.visible.remove(path);
            self.unregister(path)?;
        }
        Ok(())
    }

    fn sync_metadata(
        &mut self,
        root: &Path,
        desired: BTreeSet<PathBuf>,
        detached: BTreeSet<PathBuf>,
    ) -> Result<()> {
        for path in detached {
            self.forget_subtree(&path, false)?;
        }
        let stale: Vec<_> = self.directories.difference(&desired).cloned().collect();
        for path in stale.iter().rev() {
            self.unregister(path)?;
        }
        if !self.register(root)? {
            bail!("Git metadata watch root disappeared");
        }
        for path in &desired {
            if path != root && path.parent().is_some_and(|p| self.directories.contains(p)) {
                self.register(path)?;
            }
        }
        self.metadata_dirs = desired;
        Ok(())
    }

    fn apply_worktree(
        &mut self,
        root: &Path,
        plan: WorktreePlan,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
        runtime: &Handle,
    ) -> Result<(BTreeSet<PathBuf>, BTreeSet<PathBuf>)> {
        let mut rebuild = plan.rebuild;
        if plan.check_config {
            let config = runtime.block_on(ignore_config(root, env, cancel))?;
            rebuild |= config != self.ignore_config;
            self.ignore_config = config;
        }
        if plan.reset {
            self.forget_subtree(root, true)?;
        }
        let mut files = BTreeSet::new();
        let mut reports = BTreeSet::new();
        let mut scans = BTreeSet::new();
        let mut ignore_scopes = BTreeSet::new();
        let mut directories = BTreeSet::new();
        if rebuild {
            scans.insert(root.to_owned());
            reports.insert(root.to_owned());
        }
        for (path, event) in plan.paths {
            if is_ignore_file(&path) {
                let scope = path.parent().unwrap().to_owned();
                insert_scope(&mut ignore_scopes, scope.clone(), root);
                insert_scope(&mut scans, scope.clone(), root);
                insert_scope(&mut reports, scope, root);
                continue;
            }
            if event.kind == PathKind::File {
                files.insert(path);
                continue;
            }
            let previous = self.directories.contains(&path);
            let directory = watch_tree::is_dir(&path)?;
            if event.kind == PathKind::Directory || previous || directory {
                if previous && (event.replace || !directory) {
                    self.forget_subtree(&path, false)?;
                    insert_scope(&mut reports, path.clone(), root);
                }
                if directory && (!previous || event.replace) {
                    insert_scope(&mut scans, path.clone(), root);
                    directories.insert(path);
                }
            } else {
                files.insert(path);
            }
        }

        if rebuild || plan.index || ignore_scopes.contains(root) {
            self.update_tracked(
                root,
                runtime.block_on(watch_tree::tracked_ignored_dirs(root, root, env, cancel))?,
            )?;
        } else {
            for scope in ignore_scopes {
                let tracked = runtime
                    .block_on(watch_tree::tracked_ignored_dirs(root, &scope, env, cancel))?;
                self.update_tracked(&scope, tracked)?;
            }
        }
        for scope in scans {
            let visible =
                watch_tree::visible_directories(root, &scope, env, cancel, runtime, |p| {
                    self.register(p)
                })?;
            let stale: Vec<_> = watch_tree::subtree(&self.visible, &scope)
                .filter(|p| !visible.contains(*p))
                .cloned()
                .collect();
            for path in stale.iter().rev() {
                self.visible.remove(path);
                if !self.tracked_ignored.contains(path) {
                    self.unregister(path)?;
                }
            }
            self.visible.extend(visible);
            // Exceptions may have been absent (or detached) before this scan.
            let existing: Vec<_> = watch_tree::subtree(&self.tracked_ignored, &scope)
                .filter(|path| !self.directories.contains(*path))
                .cloned()
                .collect();
            for path in existing {
                self.register_if_parent_watched(&path)?;
            }
        }
        for path in directories {
            if self.directories.contains(&path) {
                insert_scope(&mut reports, path, root);
            }
        }
        Ok((files, reports))
    }

    fn update_tracked(&mut self, scope: &Path, tracked: BTreeSet<PathBuf>) -> Result<()> {
        let removed: Vec<_> = watch_tree::subtree(&self.tracked_ignored, scope)
            .filter(|path| !tracked.contains(*path))
            .cloned()
            .collect();
        for path in removed.iter().rev() {
            self.tracked_ignored.remove(path);
            if !self.visible.contains(path) {
                self.unregister(path)?;
            }
        }
        for path in tracked
            .difference(&self.tracked_ignored)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.register_if_parent_watched(&path)?;
        }
        self.tracked_ignored.extend(tracked);
        Ok(())
    }

    fn register_if_parent_watched(&mut self, path: &Path) -> Result<()> {
        if path.parent().is_some_and(|p| self.directories.contains(p)) {
            self.register(path)?;
        }
        Ok(())
    }
}

fn insert_scope(scopes: &mut BTreeSet<PathBuf>, path: PathBuf, root: &Path) {
    if path
        .ancestors()
        .take_while(|p| p.starts_with(root))
        .any(|p| scopes.contains(p))
    {
        return;
    }
    let narrower: Vec<_> = watch_tree::subtree(scopes, &path).cloned().collect();
    for scope in narrower {
        scopes.remove(&scope);
    }
    scopes.insert(path);
}

fn path_kind(event: EventKind) -> PathKind {
    match event {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => {
            PathKind::Directory
        }
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => PathKind::File,
        EventKind::Any
        | EventKind::Create(_)
        | EventKind::Remove(_)
        | EventKind::Modify(ModifyKind::Name(_) | ModifyKind::Any) => PathKind::Unknown,
        _ => PathKind::File,
    }
}

fn can_replace(event: EventKind) -> bool {
    matches!(
        event,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    )
}

fn detached(event: EventKind, index: usize) -> bool {
    matches!(
        event,
        EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::From | RenameMode::Any | RenameMode::Other
            ))
    ) || (index == 0 && matches!(event, EventKind::Modify(ModifyKind::Name(RenameMode::Both))))
}

fn transient(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_encoded_bytes();
        name.starts_with(b".paseo-watcher-canary-")
            || name.starts_with(b".watchman-cookie-")
            || name.starts_with(b"watchman-cookie-")
    })
}

fn worktree_path(root: &Path, path: &Path, is_git: bool) -> bool {
    !transient(path)
        && path.strip_prefix(root).is_ok_and(|p| {
            !p.as_os_str().is_empty()
                && (!is_git || !p.components().any(|c| c.as_os_str() == ".git"))
        })
}

fn is_ignore_file(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == ".gitignore")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Metadata {
    Index,
    Refs,
    Head,
    Config,
    WorktreeConfig,
    Exclude,
}

impl Metadata {
    const ALL: [Self; 6] = [
        Self::Index,
        Self::Refs,
        Self::Head,
        Self::Config,
        Self::WorktreeConfig,
        Self::Exclude,
    ];

    fn apply(self, pending: &mut Pending) {
        match self {
            Self::Index => pending.index = true,
            Self::Refs => pending.refs = true,
            Self::Head => {
                pending.refs = true;
                pending.retarget = true;
                pending.check_config = true; // Includes may depend on the branch.
            }
            Self::Config => {
                pending.config = true;
                pending.retarget = true;
                pending.check_config = true;
            }
            Self::WorktreeConfig => {
                pending.config = true;
                pending.retarget = true;
            }
            Self::Exclude => {
                pending.config = true;
                pending.tree_all = true;
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MetadataStamp([u64; Metadata::ALL.len()]);

impl MetadataStamp {
    /// A degraded watcher has no path events, but can still distinguish a
    /// changed index from a changed ref/config by fingerprinting its bounded
    /// metadata directories. Do not walk objects or unrelated Git internals.
    fn read(
        root: &Path,
        directories: &BTreeSet<PathBuf>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        let mut stamp = [0; Metadata::ALL.len()];
        for dir in directories {
            if cancel.is_cancelled() {
                bail!("Git metadata polling cancelled");
            }
            if !watch_tree::is_dir(dir)? {
                continue;
            }
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let Some((class, _)) = metadata_effect(path.strip_prefix(root)?) else {
                    continue;
                };
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                let mut hash = DefaultHasher::new();
                path.hash(&mut hash);
                if !metadata.is_dir() {
                    metadata.len().hash(&mut hash);
                    metadata.modified().ok().hash(&mut hash);
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        metadata.ino().hash(&mut hash);
                        metadata.ctime().hash(&mut hash);
                        metadata.ctime_nsec().hash(&mut hash);
                    }
                }
                stamp[class as usize] ^= hash.finish();
            }
        }
        Ok(Self(stamp))
    }
}

/// Effect and whether it is local to the checkout that owns this Git dir.
fn metadata_effect(path: &Path) -> Option<(Metadata, bool)> {
    if transient(path)
        || path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().ends_with(b".lock"))
    {
        return None;
    }
    let class = match path.to_str() {
        Some("index") => (Metadata::Index, true),
        Some("HEAD") => (Metadata::Head, true),
        Some(
            "ORIG_HEAD" | "MERGE_HEAD" | "CHERRY_PICK_HEAD" | "REVERT_HEAD" | "REBASE_HEAD"
            | "AUTO_MERGE" | "BISECT_HEAD",
        ) => (Metadata::Refs, true),
        Some("packed-refs" | "FETCH_HEAD") => (Metadata::Refs, false),
        Some("config") => (Metadata::Config, false),
        Some("config.worktree") => (Metadata::Config, true),
        _ if path == Path::new("paseo") || path == Path::new("paseo/worktree.json") => {
            (Metadata::WorktreeConfig, true)
        }
        _ if path == Path::new("info") || path == Path::new("info/exclude") => {
            (Metadata::Exclude, false)
        }
        _ if path.starts_with("refs") || path.starts_with("reftable") => {
            (Metadata::Refs, private_ref(path))
        }
        _ => return None,
    };
    Some(class)
}

fn private_ref(path: &Path) -> bool {
    ["refs/worktree", "refs/bisect", "refs/rewritten"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn metadata_directories(root: &Path, common: bool) -> BTreeSet<PathBuf> {
    let shared = [
        "info",
        "refs/heads",
        "refs/tags",
        "refs/replace",
        "refs/remotes",
        "refs/remotes/origin",
    ];
    let private = ["refs/worktree", "refs/bisect", "refs/rewritten"];
    std::iter::once(root.to_owned())
        .chain(
            ["paseo", "reftable", "refs"]
                .iter()
                .chain(private.iter())
                .chain(shared.iter().filter(|_| common))
                .map(|path| root.join(path)),
        )
        .collect()
}

impl RefTargets {
    fn add(&mut self, reference: &str, common: &Path, git_dir: &Path) -> Option<PathBuf> {
        let path = Path::new(reference);
        if !path.starts_with("refs")
            || !path.components().all(|p| matches!(p, Component::Normal(_)))
        {
            return None;
        }
        let (dirs, root) = if private_ref(path) && git_dir != common {
            (&mut self.private, git_dir)
        } else {
            (&mut self.common, common)
        };
        for dir in path
            .parent()
            .into_iter()
            .flat_map(Path::ancestors)
            .filter(|p| !p.as_os_str().is_empty())
        {
            dirs.insert(root.join(dir));
        }
        Some(root.join(path))
    }

    fn add_base(&mut self, reference: &str, common: &Path, git_dir: &Path) {
        if reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/") {
            self.add(reference, common, git_dir);
        } else {
            let name = command::branch_name(reference);
            self.add(&format!("refs/heads/{name}"), common, git_dir);
            self.add(&format!("refs/remotes/origin/{name}"), common, git_dir);
        }
    }
}

async fn ref_targets(
    common: &Path,
    demand: &Demand,
    env: &BTreeMap<String, String>,
) -> Result<RefTargets> {
    let mut targets = RefTargets::default();
    let git_dir = &demand.git_dir;
    let head = symbolic_ref_file(&git_dir.join("HEAD")).await?;
    if let Some(head) = head
        && let Some(path) = targets.add(&head, common, git_dir)
    {
        // Unlike rev-parse @{upstream}, for-each-ref reports the configured
        // upstream even when that ref does not yet exist locally.
        let pair = command::text(
            &demand.root,
            &[
                "for-each-ref",
                "--count=1",
                "--format=%(refname)%09%(upstream)%09.",
                "--",
                &head,
            ],
            env,
            demand.lifetime.child_token(),
        )
        .await?;
        if let Some(upstream) = pair
            .strip_prefix(&head)
            .and_then(|pair| pair.strip_prefix('\t'))
            .and_then(|upstream| upstream.strip_suffix("\t."))
        {
            targets.add(upstream, common, git_dir);
        } else {
            targets.unborn = Some(path);
        }
    }
    if let Some(head) = symbolic_ref_file(&common.join("refs/remotes/origin/HEAD")).await? {
        targets.add(&head, common, git_dir);
        if let Some(branch) = head.strip_prefix("refs/remotes/origin/") {
            targets.add(&format!("refs/heads/{branch}"), common, git_dir);
        }
    }
    if let Some(bytes) = read_optional(&git_dir.join("paseo/worktree.json")).await?
        && let Ok(metadata) = serde_json::from_slice::<Value>(&bytes)
        && let Some(base) = metadata
            .get("baseRef")
            .or_else(|| metadata.get("baseRefName"))
            .and_then(Value::as_str)
    {
        targets.add_base(base, common, git_dir);
    }
    Ok(targets)
}

async fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn symbolic_ref_file(path: &Path) -> Result<Option<String>> {
    let Some(bytes) = read_optional(path).await? else {
        return Ok(None);
    };
    Ok(std::str::from_utf8(&bytes)?
        .trim_end()
        .strip_prefix("ref: ")
        .map(str::to_owned))
}

async fn ignore_config(
    root: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    let result = command::run(
        root,
        &[
            "config".into(),
            "--null".into(),
            "--includes".into(),
            "--get-regexp".into(),
            "^core\\.(excludesfile|ignorecase)$".into(),
        ],
        env,
        cancel.child_token(),
        command::READ_LIMIT,
        command::READ_TIMEOUT,
    )
    .await?;
    if result.status > 1 {
        bail!("{}", result.stderr);
    }
    Ok(result.stdout)
}

struct ForgeDemand {
    count: usize,
    next: Instant,
    failures: u32,
    status: Option<Value>,
}
pub(super) struct ForgeObservation {
    demands: Arc<StdMutex<HashMap<PathBuf, ForgeDemand>>>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
}
impl Drop for ForgeObservation {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl GitService {
    async fn retain_forge_observation(&self, cwd: &Path, consumers: usize) {
        let mut slot = self.forge_observation.lock().await;
        if slot.is_none() {
            let demands = Arc::new(StdMutex::new(HashMap::new()));
            let wake = Arc::new(Notify::new());
            let cancel = self.cancel.child_token();
            *slot = Some(ForgeObservation {
                demands: demands.clone(),
                wake: wake.clone(),
                cancel: cancel.clone(),
            });
            tokio::spawn(forge_loop(self.app.clone(), demands, wake, cancel));
        }
        let observation = slot.as_ref().unwrap();
        let mut demands = observation
            .demands
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let demand = demands.entry(cwd.to_owned()).or_insert(ForgeDemand {
            count: 0,
            next: Instant::now(),
            failures: 0,
            status: None,
        });
        demand.count += consumers;
        observation.wake.notify_one();
    }
    async fn release_forge_observation(&self, cwd: &Path) {
        let mut slot = self.forge_observation.lock().await;
        let empty = if let Some(observation) = slot.as_ref() {
            let mut demands = observation
                .demands
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(demand) = demands.get_mut(cwd) {
                demand.count -= 1;
                if demand.count == 0 {
                    demands.remove(cwd);
                }
            }
            observation.wake.notify_one();
            demands.is_empty()
        } else {
            false
        };
        if empty {
            slot.take();
        }
    }
    pub(super) async fn request_forge_refresh(&self, cwd: &Path) {
        if let Some(observation) = self.forge_observation.lock().await.as_ref() {
            if let Some(demand) = observation
                .demands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(cwd)
            {
                demand.next = Instant::now();
            }
            observation.wake.notify_one();
        }
    }
    pub(super) async fn apply_forge_payload(&self, cwd: &Path, payload: &Value) -> Result<()> {
        self.apply_forge_payload_at(cwd, payload, None).await
    }
    async fn apply_forge_payload_at(
        &self,
        cwd: &Path,
        payload: &Value,
        expected: Option<u64>,
    ) -> Result<()> {
        let (_, entry) = self.checkout_entry(cwd).await?;
        if entry.snapshot.value.lock().await.is_none() {
            self.snapshot(cwd, false, self.cancel.child_token()).await?;
        }
        let flight = entry.snapshot.build.lock().await;
        if expected
            .is_some_and(|expected| entry.forge_generation.load(Ordering::Acquire) != expected)
        {
            drop(flight);
            self.request_forge_refresh(cwd).await;
            return Ok(());
        }
        let mut forge = json!({"featuresEnabled":payload["githubFeaturesEnabled"],"pullRequest":payload["status"],"authState":payload["authState"],"error":payload["error"]});
        if let Some(brand) = payload.get("forge") {
            forge["forge"] = brand.clone();
        }
        let snapshot = {
            let mut cached = entry.snapshot.value.lock().await;
            let Some((_, snapshot)) = cached.as_mut() else {
                return Ok(());
            };
            if snapshot["forge"] == forge {
                return Ok(());
            }
            snapshot["forge"] = forge;
            snapshot.clone()
        };
        drop(flight);
        self.publish_checkout_snapshot(cwd, &snapshot).await
    }
    pub(super) async fn publish_checkout_snapshot(
        &self,
        cwd: &Path,
        snapshot: &Value,
    ) -> Result<()> {
        let root = snapshot["git"]["repoRoot"]
            .as_str()
            .map(Path::new)
            .unwrap_or(cwd);
        let aliases = {
            let observers = self.observers.lock().await;
            let mut aliases = HashSet::new();
            for observer in observers.values() {
                for (key, demand) in observer
                    .demands
                    .lock()
                    .map_err(|_| anyhow!("Git observation state poisoned"))?
                    .iter()
                {
                    if key == cwd || demand.root == root {
                        aliases.extend(demand.aliases.keys().cloned());
                    }
                }
            }
            if aliases.is_empty() {
                aliases.insert(cwd.to_owned());
            }
            aliases
        };
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon shutting down"))?;
        for alias in aliases {
            app.transport
                .publish_event(super::checkout_status_update(&alias, snapshot))
                .await?;
        }
        app.projects.reconcile_checkout(cwd).await?;
        Ok(())
    }
}

fn forge_interval(status: Option<&Value>, failures: u32) -> Duration {
    let pending = status.is_some_and(|status| {
        status["checksStatus"] == "pending"
            || status["checks"]
                .as_array()
                .is_some_and(|checks| checks.iter().any(|check| check["status"] == "pending"))
    });
    let base = Duration::from_secs(if pending { 20 } else { 120 });
    if failures <= 1 {
        base
    } else {
        base.saturating_mul(1u32 << failures.saturating_sub(1).min(4))
            .min(Duration::from_secs(300))
    }
}

async fn forge_loop(
    app: Weak<AppState>,
    demands: Arc<StdMutex<HashMap<PathBuf, ForgeDemand>>>,
    wake: Arc<Notify>,
    cancel: CancellationToken,
) {
    let epoch = Instant::now();
    loop {
        let next = demands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|d| d.next)
            .min();
        let Some(next) = next else {
            tokio::select! {_=cancel.cancelled()=>return,_=wake.notified()=>continue}
        };
        // All repositories use one five-second alignment and one engine batch.
        let ticks = next
            .saturating_duration_since(epoch)
            .as_millis()
            .div_ceil(5000) as u64;
        let deadline = epoch + Duration::from_secs(ticks.saturating_mul(5));
        tokio::select! {_=cancel.cancelled()=>return,_=wake.notified()=>continue,_=tokio::time::sleep_until(deadline)=>{}}
        let due: Vec<_> = demands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&(_cwd, d)| d.next <= Instant::now())
            .map(|(cwd, _d)| cwd.clone())
            .collect();
        let Some(app) = app.upgrade() else { return };
        let Ok(env) = app.git.environment() else {
            return;
        };
        let mut generations = HashMap::new();
        for cwd in &due {
            if let Ok((_, entry)) = app.git.checkout_entry(cwd).await {
                generations.insert(cwd.clone(), entry.forge_generation.load(Ordering::Acquire));
            }
        }
        let results = app
            .git
            .forge
            .poll_statuses(&due, &env, cancel.child_token())
            .await;
        if cancel.is_cancelled() {
            return;
        }
        for (cwd, payload) in results {
            let apply = {
                let mut demands = demands.lock().unwrap_or_else(|e| e.into_inner());
                let Some(demand) = demands.get_mut(&cwd) else {
                    continue;
                };
                let success = payload["error"].is_null();
                let apply = success || demand.status.is_none();
                if success {
                    demand.failures = 0;
                    demand.status = Some(payload["status"].clone());
                } else {
                    demand.failures = demand.failures.saturating_add(1);
                }
                demand.next =
                    Instant::now() + forge_interval(demand.status.as_ref(), demand.failures);
                apply
            };
            if apply
                && let Err(error) = app
                    .git
                    .apply_forge_payload_at(&cwd, &payload, generations.get(&cwd).copied())
                    .await
            {
                tracing::debug!(%error,"Forge observation publication failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_filters_ignore_object_noise_and_route_worktree_owners() {
        for noise in [
            "objects/ab/object",
            "refs/heads/main.lock",
            "worktrees/feature/HEAD",
            "watchman-cookie-a",
        ] {
            assert_eq!(metadata_effect(Path::new(noise)), None);
        }
        for (name, expected, owner_only, flags) in [
            ("index", Metadata::Index, true, (false, true, false)),
            ("HEAD", Metadata::Head, true, (true, false, false)),
            (
                "paseo/worktree.json",
                Metadata::WorktreeConfig,
                true,
                (false, false, true),
            ),
            (
                "info/exclude",
                Metadata::Exclude,
                false,
                (false, false, true),
            ),
            ("config", Metadata::Config, false, (false, false, true)),
            (
                "refs/remotes/origin/main",
                Metadata::Refs,
                false,
                (true, false, false),
            ),
            ("packed-refs", Metadata::Refs, false, (true, false, false)),
            ("FETCH_HEAD", Metadata::Refs, false, (true, false, false)),
        ] {
            let (class, owner) = metadata_effect(Path::new(name)).unwrap();
            assert_eq!((class, owner), (expected, owner_only));
            let mut pending = Pending::default();
            class.apply(&mut pending);
            assert_eq!((pending.refs, pending.index, pending.config), flags);
        }
    }

    #[test]
    fn debounce_trails_a_burst_and_does_not_starve_continuous_changes() {
        let start = Instant::now();
        let mut clock = CheckoutClock::default();
        clock.debounce(start);
        assert_eq!(clock.due, Some(start + DEBOUNCE));
        let later = start + Duration::from_millis(500);
        clock.debounce(later);
        assert_eq!(clock.due, Some(later + DEBOUNCE));
        for n in 2..=10 {
            clock.debounce(start + Duration::from_millis(n * 500));
        }
        assert_eq!(clock.due, Some(start + MAX_WAIT));
        clock.take_pending();
        clock.debounce(start + MAX_WAIT);
        assert_eq!(clock.due, Some(start + MAX_WAIT + DEBOUNCE));
    }

    #[test]
    fn path_overflow_promotes_to_all_and_bounds_memory() {
        let mut pending = Pending::default();
        for _ in 0..MAX_PATHS {
            pending.add_path("same/path".into(), PathKind::File, false);
        }
        assert!(!pending.all);
        for n in 0..MAX_PATHS {
            pending.add_path(format!("file-{n}").into(), PathKind::File, false);
        }
        assert!(pending.all && !pending.rescan);
        assert!(!(pending.refs || pending.index || pending.config));
        assert!(pending.paths.is_empty());
        for n in 0..=MAX_PATHS {
            pending.add_path(format!("dir-{n}").into(), PathKind::Directory, true);
        }
        assert!(pending.all && pending.tree_all && pending.rescan);
        assert!(pending.paths.is_empty());
    }
    #[test]
    fn forge_interval_tracks_pending_checks_and_caps_error_backoff() {
        let pending = json!({"checksStatus":"pending"});
        let individual = json!({"checksStatus":"success","checks":[{"status":"pending"}]});
        assert_eq!(forge_interval(Some(&pending), 0), Duration::from_secs(20));
        assert_eq!(
            forge_interval(Some(&individual), 1),
            Duration::from_secs(20)
        );
        assert_eq!(forge_interval(Some(&pending), 2), Duration::from_secs(40));
        assert_eq!(forge_interval(None, 0), Duration::from_secs(120));
        assert_eq!(forge_interval(None, 8), Duration::from_secs(300));
    }
    #[test]
    fn last_repository_owner_cancels_descendant_work() {
        let cancel = CancellationToken::new();
        let active = cancel.child_token();
        let demand_changed = Arc::new(Notify::new());
        let observer = Observation {
            demands: Arc::new(StdMutex::new(HashMap::new())),
            demand_changed,
            cancel,
        };
        drop(observer);
        assert!(active.is_cancelled());
    }
}

#[cfg(test)]
mod directory_tests {
    use super::*;

    struct TestRepo {
        actor: Actor,
        demand: Demand,
        env: BTreeMap<String, String>,
        _directory: tempfile::TempDir,
    }

    impl TestRepo {
        async fn new(dirs: &[&str], ignores: &str) -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let root = tokio::fs::canonicalize(directory.path()).await?;
            let env = git_env();
            let cancel = CancellationToken::new();
            command::text(&root, &["init", "-b", "main"], &env, cancel.child_token()).await?;
            for dir in dirs {
                tokio::fs::create_dir_all(root.join(dir)).await?;
            }
            tokio::fs::write(root.join(".gitignore"), ignores).await?;
            let demand = Demand {
                count: 1,
                root: root.clone(),
                git_dir: root.join(".git"),
                aliases: HashMap::new(),
                is_git: true,
                lifetime: cancel.child_token(),
            };
            let (events, receiver) = mpsc::channel(1_024);
            let mut actor = Actor {
                app: Weak::new(),
                common: demand.git_dir.clone(),
                demands: Arc::new(StdMutex::new(HashMap::from([(
                    root.clone(),
                    demand.clone(),
                )]))),
                demand_changed: Arc::new(Notify::new()),
                events,
                receiver,
                cancel,
                watches: BTreeMap::new(),
                clocks: HashMap::new(),
                ref_targets: HashMap::new(),
                next_fetch: Instant::now(),
                fetch_task: None,
            };
            actor.reconcile_demands();
            let mut pending = actor.clocks.get_mut(&root).unwrap().take_pending();
            actor.prepare_metadata(&demand, &mut pending, &env).await?;
            assert!(matches!(
                actor.prepare_worktree(&demand, &mut pending, &env).await?,
                ChangedPaths::All
            ));
            Ok(Self {
                actor,
                demand,
                env,
                _directory: directory,
            })
        }

        fn watches(&self, path: &str) -> bool {
            self.actor.watches[&self.demand.root]
                .native
                .as_ref()
                .unwrap()
                .directories
                .contains(&self.demand.root.join(path))
        }

        async fn paths_after(&mut self, event: Event) -> Result<ChangedPaths> {
            let root = &self.demand.root;
            self.actor.file_event(root, event);
            self.actor.reconcile_overflow();
            let mut pending = self.actor.clocks.get_mut(root).unwrap().take_pending();
            self.actor
                .prepare_worktree(&self.demand, &mut pending, &self.env)
                .await
        }

        async fn git(&self, args: &[&str]) -> Result<()> {
            command::text(
                &self.demand.root,
                args,
                &self.env,
                self.demand.lifetime.child_token(),
            )
            .await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn creates_and_moves_register_only_the_affected_subtree() -> Result<()> {
        let mut repo = TestRepo::new(&["existing/old"], "").await?;
        let new = repo.demand.root.join("new");
        tokio::fs::create_dir_all(new.join("child/grandchild")).await?;
        tokio::fs::create_dir_all(repo.demand.root.join("existing/unrelated")).await?;
        let paths = repo
            .paths_after(Event::new(EventKind::Create(CreateKind::Folder)).add_path(new.clone()))
            .await?;
        assert!(
            matches!(paths, ChangedPaths::Paths(paths) if paths == BTreeSet::from([PathBuf::from("new")]))
        );
        assert!(repo.watches("new/child/grandchild"));
        assert!(repo.watches("existing/old"));
        assert!(
            !repo.watches("existing/unrelated"),
            "sibling subtree must not have been read"
        );

        let moved = repo.demand.root.join("moved");
        tokio::fs::rename(&new, &moved).await?;
        let paths = repo
            .paths_after(
                Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(new)
                    .add_path(moved),
            )
            .await?;
        assert!(
            matches!(paths, ChangedPaths::Paths(paths) if paths == BTreeSet::from(["new".into(), "moved".into()]))
        );
        assert!(!repo.watches("new/child/grandchild"));
        assert!(repo.watches("moved/child/grandchild"));
        assert!(!repo.watches("existing/unrelated"));
        Ok(())
    }

    #[tokio::test]
    async fn ignored_directory_and_file_churn_does_not_refresh() -> Result<()> {
        let mut repo = TestRepo::new(&["visible"], "build*/\n*.tmp\n").await?;
        let ignored = repo.demand.root.join("build-new");
        tokio::fs::create_dir_all(ignored.join("nested")).await?;
        assert!(matches!(
            repo.paths_after(
                Event::new(EventKind::Create(CreateKind::Folder)).add_path(ignored.clone())
            )
            .await?,
            ChangedPaths::None,
        ));
        assert!(!repo.watches("build-new"));
        tokio::fs::remove_dir_all(&ignored).await?;
        assert!(matches!(
            repo.paths_after(Event::new(EventKind::Remove(RemoveKind::Folder)).add_path(ignored))
                .await?,
            ChangedPaths::None,
        ));
        let file = repo.demand.root.join("visible/build artifact.tmp");
        tokio::fs::write(&file, "ignored").await?;
        assert!(matches!(
            repo.paths_after(Event::new(EventKind::Create(CreateKind::File)).add_path(file))
                .await?,
            ChangedPaths::None,
        ));
        Ok(())
    }

    #[tokio::test]
    async fn scoped_gitignore_changes_only_reconcile_its_subtree() -> Result<()> {
        let mut repo = TestRepo::new(&["scope/child/deep", "sibling/old"], "").await?;
        tokio::fs::create_dir_all(repo.demand.root.join("sibling/unrelated")).await?;
        let ignore = repo.demand.root.join("scope/.gitignore");
        tokio::fs::write(&ignore, "child/\n").await?;
        let paths = repo
            .paths_after(Event::new(EventKind::Create(CreateKind::File)).add_path(ignore.clone()))
            .await?;
        assert!(
            matches!(paths, ChangedPaths::Paths(paths) if paths == BTreeSet::from([PathBuf::from("scope")]))
        );
        assert!(!repo.watches("scope/child/deep"));
        assert!(repo.watches("sibling/old"));
        assert!(!repo.watches("sibling/unrelated"));
        tokio::fs::remove_file(&ignore).await?;
        repo.paths_after(Event::new(EventKind::Remove(RemoveKind::File)).add_path(ignore))
            .await?;
        assert!(repo.watches("scope/child/deep"));
        assert!(!repo.watches("sibling/unrelated"));
        Ok(())
    }

    #[tokio::test]
    async fn backend_rescan_restores_missing_watches_and_refreshes_all() -> Result<()> {
        use notify::event::Flag;
        let mut repo = TestRepo::new(&[], "").await?;
        tokio::fs::create_dir_all(repo.demand.root.join("missed/nested")).await?;
        assert!(matches!(
            repo.paths_after(Event::new(EventKind::Other).set_flag(Flag::Rescan))
                .await?,
            ChangedPaths::All,
        ));
        assert!(repo.watches("missed/nested"));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_watches_retarget_nested_upstream_without_watching_objects() -> Result<()> {
        let mut repo = TestRepo::new(&[], "").await?;
        repo.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ])
        .await?;
        repo.git(&["config", "remote.team.url", "."]).await?;
        repo.git(&[
            "config",
            "remote.team.fetch",
            "+refs/heads/*:refs/remotes/team/*",
        ])
        .await?;
        repo.git(&["config", "branch.main.remote", "team"]).await?;
        repo.git(&["config", "branch.main.merge", "refs/heads/nested/base"])
            .await?;
        let git_dir = &repo.demand.git_dir;
        repo.git(&["update-ref", "refs/remotes/team/nested/base", "HEAD"])
            .await?;
        let mut pending = Pending {
            retarget: true,
            ..Default::default()
        };
        repo.actor
            .prepare_metadata(&repo.demand, &mut pending, &repo.env)
            .await?;
        let watched = &repo.actor.watches[git_dir]
            .native
            .as_ref()
            .unwrap()
            .directories;
        assert!(watched.contains(&git_dir.join("refs/remotes/team/nested")));
        assert!(
            !watched
                .iter()
                .any(|p| p.starts_with(git_dir.join("objects")))
        );

        repo.git(&["config", "branch.main.merge", "refs/heads/replacement/base"])
            .await?;
        repo.git(&["update-ref", "refs/remotes/team/replacement/base", "HEAD"])
            .await?;
        repo.actor
            .prepare_metadata(&repo.demand, &mut pending, &repo.env)
            .await?;
        let watched = &repo.actor.watches[git_dir]
            .native
            .as_ref()
            .unwrap()
            .directories;
        assert!(!watched.contains(&git_dir.join("refs/remotes/team/nested")));
        assert!(watched.contains(&git_dir.join("refs/remotes/team/replacement")));
        Ok(())
    }

    #[tokio::test]
    async fn degraded_metadata_poll_flags_only_changed_classes() -> Result<()> {
        let mut repo = TestRepo::new(&[], "").await?;
        let git_dir = &repo.demand.git_dir;
        repo.actor.poll_metadata(git_dir, None).await;
        tokio::fs::write(git_dir.join("index"), "new").await?;
        tokio::fs::write(git_dir.join("refs/heads/main.lock"), "transient").await?;
        let changes = repo.actor.poll_metadata(git_dir, None).await;
        let mut pending = Pending::default();
        pending.apply_metadata_changes(changes);
        assert!(pending.index && !pending.refs && !pending.config);
        assert_eq!(
            repo.actor.poll_metadata(git_dir, None).await,
            [false; Metadata::ALL.len()]
        );
        Ok(())
    }

    fn git_env() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            (
                "GIT_CONFIG_GLOBAL".into(),
                if cfg!(windows) {
                    "NUL".into()
                } else {
                    "/dev/null".into()
                },
            ),
        ])
    }

    #[tokio::test]
    async fn existing_directory_can_transition_to_git_without_workspace_recreation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let env = git_env();
        let cancel = CancellationToken::new();
        let before = observation_roots(directory.path(), &env, cancel.clone()).await?;
        assert!(!before.3);
        assert_eq!(before.0, before.1);
        command::checked(
            directory.path(),
            &["init".into(), "-b".into(), "main".into()],
            &env,
            cancel.clone(),
            command::READ_LIMIT,
            command::READ_TIMEOUT,
        )
        .await?;
        let after = observation_roots(directory.path(), &env, cancel).await?;
        assert!(after.3);
        assert_eq!(after.1, before.1);
        assert_eq!(after.0, after.2);
        assert_ne!(after.0, after.1);
        Ok(())
    }
}
