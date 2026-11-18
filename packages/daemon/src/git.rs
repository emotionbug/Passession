pub mod command;
pub mod diff;
mod facts;
pub mod forge;
mod head;
mod metadata;
#[cfg(test)]
mod mutation_tests;
mod mutations;
mod observation;
mod scheduler;
mod watch_tree;

use crate::{state::AppState, transport::RequestContext};
use anyhow::{Result, anyhow, bail};
pub use command::resolve_diff_base;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

/// What changed since the last refresh, as classified by the watcher.
#[derive(Default)]
pub(crate) struct CheckoutChange {
    pub refs: bool,
    pub index: bool,
    pub config: bool,
    pub paths: ChangedPaths,
}

#[derive(Default)]
pub(crate) enum ChangedPaths {
    #[default]
    None,
    Paths(BTreeSet<PathBuf>),
    All,
}

impl CheckoutChange {
    fn attributes_changed(&self) -> bool {
        matches!(&self.paths, ChangedPaths::Paths(paths) if paths.iter().any(|path| {
            path.file_name().is_some_and(|name| name == ".gitattributes")
        }))
    }
}

struct CachedSnapshot {
    generation: AtomicU64,
    build: Mutex<Option<facts::ObservedRepo>>,
    value: Mutex<Option<(u64, Value)>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffMode {
    Base,
    Uncommitted,
    All,
}

impl DiffMode {
    fn from_compare(compare: &Value) -> Self {
        match compare["mode"].as_str() {
            Some("base") => Self::Base,
            Some("uncommitted") => Self::Uncommitted,
            _ => Self::All,
        }
    }

    fn affected(self, base: bool, head: bool, worktree: bool) -> bool {
        match self {
            Self::Base => base || head,
            Self::Uncommitted => head || worktree,
            Self::All => base || head || worktree,
        }
    }
}
struct CachedRead {
    mode: DiffMode,
    changes: watch::Sender<u64>,
    generation: AtomicU64,
    build: Mutex<()>,
    value: Mutex<Option<(u64, Value)>>,
    built_at: std::sync::Mutex<Option<std::time::Instant>>,
}
impl CachedRead {
    fn new(mode: DiffMode) -> Self {
        Self {
            mode,
            changes: watch::channel(0).0,
            generation: AtomicU64::new(0),
            build: Mutex::new(()),
            value: Mutex::new(None),
            built_at: std::sync::Mutex::new(None),
        }
    }
    fn invalidate(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
    fn fresh(&self) -> bool {
        self.built_at
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some_and(|time| time.elapsed() < std::time::Duration::from_secs(15))
    }
}
struct Checkout {
    snapshot: CachedSnapshot,
    diffs: Mutex<HashMap<String, Arc<CachedRead>>>,
    forge_generation: AtomicU64,
    legacy_shortstat: AtomicUsize,
    mutation: Mutex<()>,
}
impl Checkout {
    fn new() -> Self {
        Self {
            snapshot: CachedSnapshot {
                generation: AtomicU64::new(0),
                build: Mutex::new(None),
                value: Mutex::new(None),
            },
            diffs: Mutex::new(HashMap::new()),
            forge_generation: AtomicU64::new(0),
            legacy_shortstat: AtomicUsize::new(0),
            mutation: Mutex::new(()),
        }
    }

    async fn invalidate_diffs(&self, base: bool, head: bool, worktree: bool) {
        for diff in self.diffs.lock().await.values() {
            if diff.mode.affected(base, head, worktree) {
                diff.invalidate();
            }
        }
    }
}
pub struct GitService {
    app: Weak<AppState>,
    checkouts: Mutex<HashMap<PathBuf, Arc<Checkout>>>,
    aliases: Mutex<HashMap<PathBuf, PathBuf>>,
    pub forge: forge::ForgeEngine,
    observers: Mutex<HashMap<PathBuf, observation::Observation>>,
    forge_observation: Mutex<Option<observation::ForgeObservation>>,
    head_cache: head::HeadCache,
    cancel: CancellationToken,
}
impl GitService {
    pub fn new(app: Weak<AppState>) -> Self {
        Self {
            app,
            checkouts: Mutex::new(HashMap::new()),
            aliases: Mutex::new(HashMap::new()),
            forge: forge::ForgeEngine::new(),
            observers: Mutex::new(HashMap::new()),
            forge_observation: Mutex::new(None),
            head_cache: Default::default(),
            cancel: CancellationToken::new(),
        }
    }
    pub async fn initialize(&self) -> Result<()> {
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon shutting down"))?;
        self.apply_config(&app.config.snapshot(), &Value::Null)
            .await
    }
    pub async fn apply_config(&self, next: &Value, _previous: &Value) -> Result<()> {
        let git = &next["git"];
        scheduler::configure(
            git["maxProcessesPerSecond"].as_u64().unwrap_or(64) as usize,
            git["maxProcessConcurrency"].as_u64().unwrap_or(8) as usize,
        )
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.cancel.cancel();
        self.forge_observation.lock().await.take();
        self.observers.lock().await.clear();
        self.checkouts.lock().await.clear();
        self.aliases.lock().await.clear();
        Ok(())
    }
    fn environment(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon shutting down"))?
            .config
            .environment())
    }
    async fn key(&self, cwd: &Path) -> Result<PathBuf> {
        let canonical = tokio::fs::canonicalize(cwd).await?;
        let known = { self.aliases.lock().await.get(&canonical).cloned() };
        if let Some(root) = known {
            self.aliases
                .lock()
                .await
                .insert(cwd.to_owned(), root.clone());
            return Ok(root);
        }
        if self.checkouts.lock().await.contains_key(&canonical) {
            self.aliases
                .lock()
                .await
                .insert(cwd.to_owned(), canonical.clone());
            return Ok(canonical);
        }
        let env = self.environment()?;
        if let Some(root) = command::optional(
            &canonical,
            &["rev-parse", "--show-toplevel"],
            &env,
            self.cancel.child_token(),
        )
        .await?
        {
            let root = tokio::fs::canonicalize(root).await?;
            let mut aliases = self.aliases.lock().await;
            if aliases.len() >= 50_000 {
                aliases.clear();
            }
            aliases.insert(canonical, root.clone());
            aliases.insert(cwd.to_owned(), root.clone());
            return Ok(root);
        }
        Ok(canonical)
    }
    async fn checkout_entry(&self, cwd: &Path) -> Result<(PathBuf, Arc<Checkout>)> {
        let key = self.key(cwd).await?;
        let mut entries = self.checkouts.lock().await;
        let entry = entries
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Checkout::new()))
            .clone();
        Ok((key, entry))
    }
    async fn cached_entry(&self, cwd: &Path) -> Option<Arc<Checkout>> {
        let key = self.aliases.lock().await.get(cwd).cloned();
        self.checkouts
            .lock()
            .await
            .get(key.as_deref().unwrap_or(cwd))
            .cloned()
    }
    pub async fn cached_snapshot(&self, cwd: &Path) -> Option<Value> {
        let entry = self.cached_entry(cwd).await?;
        let value = entry.snapshot.value.lock().await;
        value.as_ref().map(|(_, value)| value.clone())
    }

    async fn retain_legacy_shortstat(&self, cwd: &Path) {
        if let Ok((_, entry)) = self.checkout_entry(cwd).await {
            entry.legacy_shortstat.fetch_add(1, Ordering::AcqRel);
        }
    }

    async fn release_legacy_shortstat(&self, cwd: &Path) {
        if let Some(entry) = self.cached_entry(cwd).await {
            let _ = entry
                .legacy_shortstat
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        }
    }

    async fn live_shortstat(
        entry: &Checkout,
        repo: Option<&mut facts::ObservedRepo>,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Value {
        if entry.legacy_shortstat.load(Ordering::Acquire) > 0
            && let Some(repo) = repo
        {
            repo.diff_stat(env, cancel.clone()).await
        } else {
            Value::Null
        }
    }

    pub async fn invalidate(&self, cwd: &Path) -> Result<()> {
        self.invalidate_checkout(cwd, true).await
    }
    async fn invalidate_checkout(&self, cwd: &Path, refs: bool) -> Result<()> {
        let (key, entry) = self.checkout_entry(cwd).await?;
        entry.snapshot.generation.fetch_add(1, Ordering::AcqRel);
        entry.invalidate_diffs(refs, refs, true).await;
        if refs {
            self.invalidate_forge(&key, &entry).await;
        }
        Ok(())
    }

    async fn invalidate_forge(&self, key: &Path, entry: &Checkout) {
        entry.forge_generation.fetch_add(1, Ordering::AcqRel);
        self.forge.invalidate(key).await;
        self.request_forge_refresh(key).await;
    }

    async fn read_snapshot_facts(
        &self,
        key: &Path,
        env: &BTreeMap<String, String>,
        cancel: &CancellationToken,
    ) -> Result<Option<facts::ObservedRepo>> {
        let (home, config) = {
            let app = self
                .app
                .upgrade()
                .ok_or_else(|| anyhow!("Daemon shutting down"))?;
            (app.home.clone(), app.config.snapshot())
        };
        let repo = facts::ObservedRepo::read(key, &home, &config, env, cancel).await?;
        if repo.is_some() {
            self.ensure_observation(key, env, cancel.clone()).await?;
        }
        Ok(repo)
    }

    fn snapshot_value(
        cwd: &Path,
        repo: Option<&facts::ObservedRepo>,
        stat: Value,
        previous: Option<&(u64, Value)>,
    ) -> Value {
        let mut git = repo.map_or_else(|| facts::non_git(cwd), |repo| repo.value(cwd));
        git["diffStat"] = stat;
        let forge = previous.map_or_else(
            || json!({"featuresEnabled":false,"pullRequest":null,"authState":"no_remote","error":null}),
            |(_, previous)| previous["forge"].clone(),
        );
        json!({"cwd":cwd,"git":git,"forge":forge})
    }

    /// Applies a classified watcher change to the cached facts, without
    /// rebuilding unrelated comparisons or requesting forge checks on edits.
    pub(crate) async fn refresh_observed(
        &self,
        cwd: &Path,
        change: CheckoutChange,
        cancel: CancellationToken,
    ) -> Result<Option<Value>> {
        let (key, entry) = self.checkout_entry(cwd).await?;
        // Do not observe the temporary index/refs of a daemon mutation.
        let _mutation = tokio::select! {
            _ = cancel.cancelled() => bail!("Git snapshot cancelled"),
            guard = entry.mutation.lock() => guard,
        };
        let mut facts = tokio::select! {
            _ = cancel.cancelled() => bail!("Git snapshot cancelled"),
            guard = entry.snapshot.build.lock() => guard,
        };
        let generation = entry.snapshot.generation.load(Ordering::Acquire);
        let env = self.environment()?;
        let refs = change.refs || change.config;
        let base = refs || change.attributes_changed();
        let worktree = change.index || change.config || !matches!(change.paths, ChangedPaths::None);
        let fresh = entry
            .snapshot
            .value
            .lock()
            .await
            .as_ref()
            .is_some_and(|(g, _)| *g == generation);
        if refs {
            self.invalidate_forge(&key, &entry).await;
        }
        let refreshed = match async {
            if let Some(repo) = facts.as_mut().filter(|_| fresh) {
                repo.refresh(&change, &env, &cancel).await
            } else {
                *facts = self.read_snapshot_facts(&key, &env, &cancel).await?;
                Ok(facts::Refreshed {
                    head_changed: true,
                    worktree_changed: true,
                })
            }
        }
        .await
        {
            Ok(refreshed) => refreshed,
            Err(error) => {
                // Do not lose an invalidation when a scoped refresh fails.
                entry.snapshot.generation.fetch_add(1, Ordering::AcqRel);
                entry.invalidate_diffs(base, refs, worktree).await;
                return Err(error);
            }
        };
        let stat = Self::live_shortstat(&entry, facts.as_mut(), &env, &cancel).await;
        let next = {
            let mut cached = entry.snapshot.value.lock().await;
            if entry.snapshot.generation.load(Ordering::Acquire) != generation {
                return Ok(None);
            }
            let value = Self::snapshot_value(&key, facts.as_ref(), stat, cached.as_ref());
            if cached
                .as_ref()
                .is_some_and(|(_, previous)| *previous == value)
            {
                // A full rebuild can settle a previously invalidated generation
                // even if the wire projection did not change.
                if let Some((old, _)) = cached.as_mut() {
                    *old = generation;
                }
                None
            } else {
                *cached = Some((generation, value.clone()));
                Some(value)
            }
        };
        entry
            .invalidate_diffs(base, refreshed.head_changed, refreshed.worktree_changed)
            .await;
        Ok(next)
    }

    async fn cached_snapshot_with_stat(&self, cwd: &Path) -> Option<Value> {
        let entry = self.cached_entry(cwd).await?;
        let mut facts = entry.snapshot.build.lock().await;
        let (generation, mut value) = entry.snapshot.value.lock().await.as_ref()?.clone();
        if let Some(repo) = facts.as_mut()
            && generation == entry.snapshot.generation.load(Ordering::Acquire)
            && let Ok(env) = self.environment()
        {
            value["git"]["diffStat"] = repo.diff_stat(&env, self.cancel.child_token()).await;
        }
        Some(value)
    }
    pub fn snapshot<'a>(
        &'a self,
        cwd: &'a Path,
        force: bool,
        cancel: CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let (key, entry) = self.checkout_entry(cwd).await?;
            if force {
                entry.snapshot.generation.fetch_add(1, Ordering::AcqRel);
            }
            let mut facts = tokio::select! {_=cancel.cancelled()=>bail!("Git snapshot cancelled"),guard=entry.snapshot.build.lock()=>guard};
            let generation = entry.snapshot.generation.load(Ordering::Acquire);
            if let Some((g, value)) = entry.snapshot.value.lock().await.as_ref()
                && *g == generation
            {
                return Ok(value.clone());
            }
            let env = self.environment()?;
            let mut next = self.read_snapshot_facts(&key, &env, &cancel).await?;
            let stat = Self::live_shortstat(&entry, next.as_mut(), &env, &cancel).await;
            let mut cached = entry.snapshot.value.lock().await;
            let value = Self::snapshot_value(&key, next.as_ref(), stat, cached.as_ref());
            if entry.snapshot.generation.load(Ordering::Acquire) == generation {
                *facts = next;
                *cached = Some((generation, value.clone()));
            }
            Ok(value)
        })
    }
    pub async fn checkout(&self, cwd: &Path, cancel: CancellationToken) -> Result<Value> {
        let mut value = self.snapshot(cwd, false, cancel).await?["git"].clone();
        value["worktreeRoot"] = value["repoRoot"].clone();
        Ok(value)
    }
    pub async fn read_diff(
        &self,
        cwd: &Path,
        compare: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let (key, checkout) = self.checkout_entry(cwd).await?;
        let entry = Self::diff_entry(&checkout, compare).await?;
        self.read_cached_diff(&key, &entry, compare, cancel).await
    }

    async fn diff_entry(checkout: &Checkout, compare: &Value) -> Result<Arc<CachedRead>> {
        let cache_key = serde_json::to_string(
            &json!({"mode":compare["mode"],"baseRef":compare["baseRef"],"ignoreWhitespace":compare["ignoreWhitespace"].as_bool().unwrap_or(false),"detail":compare["detail"].as_str().unwrap_or("full")}),
        )?;
        Ok({
            let mut diffs = checkout.diffs.lock().await;
            if diffs.len() >= 64 && !diffs.contains_key(&cache_key) {
                let expired = diffs
                    .iter()
                    .find(|(_, entry)| Arc::strong_count(entry) == 1 && !entry.fresh())
                    .or_else(|| {
                        diffs
                            .iter()
                            .find(|(_, entry)| Arc::strong_count(entry) == 1)
                    })
                    .map(|(key, _)| key.clone());
                if let Some(expired) = expired {
                    diffs.remove(&expired);
                }
            }
            diffs
                .entry(cache_key)
                .or_insert_with(|| Arc::new(CachedRead::new(DiffMode::from_compare(compare))))
                .clone()
        })
    }

    async fn read_cached_diff(
        &self,
        key: &Path,
        entry: &CachedRead,
        compare: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let generation = entry.generation.load(Ordering::Acquire);
        if entry.fresh()
            && let Some((g, value)) = entry.value.lock().await.as_ref()
            && *g == generation
        {
            return Ok(value.clone());
        }
        let _flight = tokio::select! {_=cancel.cancelled()=>bail!("Git diff cancelled"),guard=entry.build.lock()=>guard};
        let generation = entry.generation.load(Ordering::Acquire);
        if entry.fresh()
            && let Some((g, value)) = entry.value.lock().await.as_ref()
            && *g == generation
        {
            return Ok(value.clone());
        }
        let env = self.environment()?;
        let value = diff::read(key, compare, &env, cancel.clone()).await?;
        self.ensure_observation(key, &env, cancel).await?;
        if entry.generation.load(Ordering::Acquire) == generation {
            *entry.value.lock().await = Some((generation, value.clone()));
            *entry.built_at.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(std::time::Instant::now());
        }
        Ok(value)
    }
    async fn notify_mutation(&self, cwd: &Path, refs: bool) -> Result<()> {
        if let Err(error) = self.invalidate_checkout(cwd, refs).await {
            tracing::warn!(%error,cwd=%cwd.display(),"Git mutation succeeded but invalidation failed");
            return Ok(());
        }
        match self.snapshot(cwd, false, self.cancel.child_token()).await {
            Ok(snapshot) => {
                if let Err(error) = self.publish_checkout_snapshot(cwd, &snapshot).await {
                    tracing::warn!(%error,cwd=%cwd.display(),"Git mutation succeeded but publication failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error,cwd=%cwd.display(),"Git mutation succeeded but refresh failed")
            }
        }
        Ok(())
    }
    pub async fn stage(
        &self,
        cwd: &Path,
        paths: &[String],
        cancel: CancellationToken,
    ) -> Result<()> {
        let (_, entry) = self.checkout_entry(cwd).await?;
        let _lock = entry.mutation.lock().await;
        mutations::stage(cwd, paths, &self.environment()?, cancel).await?;
        self.notify_mutation(cwd, false).await
    }
    pub async fn unstage(
        &self,
        cwd: &Path,
        paths: &[String],
        cancel: CancellationToken,
    ) -> Result<()> {
        let (_, entry) = self.checkout_entry(cwd).await?;
        let _lock = entry.mutation.lock().await;
        mutations::unstage(cwd, paths, &self.environment()?, cancel).await?;
        self.notify_mutation(cwd, false).await
    }
    pub async fn revert(&self, cwd: &Path, sha: &str, cancel: CancellationToken) -> Result<()> {
        let (_, entry) = self.checkout_entry(cwd).await?;
        let _lock = entry.mutation.lock().await;
        mutations::revert(cwd, sha, &self.environment()?, cancel).await?;
        self.notify_mutation(cwd, true).await
    }
    pub async fn rebase(&self, cwd: &Path, base: &str, cancel: CancellationToken) -> Result<()> {
        let (_, entry) = self.checkout_entry(cwd).await?;
        let _lock = entry.mutation.lock().await;
        mutations::rebase(cwd, base, &self.environment()?, cancel).await?;
        self.notify_mutation(cwd, true).await
    }
    pub async fn handle(&self, context: &RequestContext, message: &Value) -> Result<bool> {
        self.handle_message(context, message).await
    }
}
pub(super) fn status_projection(snapshot: &Value) -> Value {
    let mut status = snapshot["git"].clone();
    if let Some(fields) = status.as_object_mut() {
        fields.remove("diffStat");
        fields.remove("worktreeRoot");
    }
    if status["isGit"] != true
        && let Some(fields) = status.as_object_mut()
    {
        fields.remove("mainRepoRoot");
    }
    status
}

fn checkout_status_update(cwd: &Path, snapshot: &Value) -> Value {
    let request_id = format!("subscription:{}", cwd.display());
    let mut payload = status_projection(snapshot);
    payload["cwd"] = json!(cwd);
    payload["requestId"] = json!(request_id);
    let forge = &snapshot["forge"];
    let mut pr = json!({
        "cwd":cwd, "requestId":request_id, "status":forge["pullRequest"],
        "githubFeaturesEnabled":forge["featuresEnabled"], "authState":forge["authState"],
        "error":null
    });
    if let Some(brand) = forge.get("forge") {
        pr["forge"] = brand.clone();
    }
    if !forge["error"].is_null() {
        pr["error"] = json!({"code":"UNKNOWN","message":forge["error"]["message"]});
    }
    payload["prStatus"] = pr;
    json!({"type":"checkout_status_update","payload":payload})
}

/// The source's `toCheckoutError`: only typed not-a-repository and merge-conflict
/// failures carry a specific code; every other failure, including raw git output
/// that mentions a missing repository, is `UNKNOWN`.
fn checkout_error(error: &anyhow::Error) -> Value {
    let code = if error.downcast_ref::<mutations::MergeConflict>().is_some() {
        "MERGE_CONFLICT"
    } else if error.downcast_ref::<command::NotGitRepo>().is_some() {
        "NOT_GIT_REPO"
    } else if error.downcast_ref::<diff::InvalidRevision>().is_some() {
        "invalid_revision"
    } else {
        "UNKNOWN"
    };
    json!({"code":code,"message":error.to_string()})
}
mod session;
