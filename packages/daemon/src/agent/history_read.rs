//! Saved history is readable independently of execution readiness. A capable
//! client is answered from the agent's projection (the ready runtime, else its
//! SQLite checkpoint) while one owned background preparation per agent starts
//! the provider and replays its history. Opening a child reads only that
//! child, fetching its native body through the parent's provider on demand.
//! Mutations keep calling `ensure_loaded`; nothing here authorizes them.
use super::*;
use crate::providers::api::{
    HistoryCoverage, ReplayOutcome, SubagentHistory, SubagentHistoryRequest, SubagentReplay,
};
use std::sync::atomic::AtomicU64;
use tokio::sync::watch;

/// Final outcome of one background attempt; `None` while it runs.
pub(super) type Outcome = Option<Result<(), String>>;

enum Flight {
    Running {
        generation: u64,
        done: watch::Receiver<Outcome>,
        abort: Option<tokio::task::AbortHandle>,
    },
    /// The latest attempt failed; observations report it until a retry starts
    /// (or, for a parent, a live owner supersedes it).
    Failed(String),
    /// A child body was read in this daemon run. Live collection keeps it current.
    Done,
}

enum Begin {
    Joined(watch::Receiver<Outcome>),
    Done,
    Started {
        generation: u64,
        sender: watch::Sender<Outcome>,
        done: watch::Receiver<Outcome>,
    },
}

/// Single-flight registry of owned background work, keyed by agent or child.
#[derive(Default)]
pub(super) struct Flights {
    entries: StdMutex<HashMap<String, Flight>>,
    generation: AtomicU64,
}

impl Flights {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Flight>> {
        // Every critical section is a single map operation; a poisoned map is
        // still consistent.
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn begin(&self, key: &str) -> Begin {
        let mut entries = self.entries();
        match entries.get(key) {
            Some(Flight::Running { done, .. }) => return Begin::Joined(done.clone()),
            Some(Flight::Done) => return Begin::Done,
            Some(Flight::Failed(_)) | None => {}
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (sender, done) = watch::channel(None);
        entries.insert(
            key.to_owned(),
            Flight::Running {
                generation,
                done: done.clone(),
                abort: None,
            },
        );
        Begin::Started {
            generation,
            sender,
            done,
        }
    }
    fn attach(&self, key: &str, generation: u64, handle: tokio::task::AbortHandle) {
        if let Some(Flight::Running {
            generation: current,
            abort,
            ..
        }) = self.entries().get_mut(key)
            && *current == generation
        {
            *abort = Some(handle);
        }
    }
    /// Replaces this attempt's entry unless shutdown already discarded it.
    fn settle(&self, key: &str, generation: u64, next: Option<Flight>) {
        let mut entries = self.entries();
        if matches!(entries.get(key), Some(Flight::Running { generation: current, .. }) if *current == generation)
        {
            match next {
                Some(flight) => entries.insert(key.to_owned(), flight),
                None => entries.remove(key),
            };
        }
    }
    fn status(&self, key: &str) -> Option<Value> {
        match self.entries().get(key)? {
            Flight::Running { .. } => Some(json!({"status":"preparing"})),
            Flight::Failed(error) => Some(json!({"status":"failed","error":error})),
            Flight::Done => None,
        }
    }
    pub(super) fn clear_failure(&self, key: &str) -> bool {
        let mut entries = self.entries();
        let failed = matches!(entries.get(key), Some(Flight::Failed(_)));
        if failed {
            entries.remove(key);
        }
        failed
    }
    pub(super) fn abort_all(&self) {
        for (_, flight) in self.entries().drain() {
            if let Flight::Running {
                abort: Some(abort), ..
            } = flight
            {
                abort.abort();
            }
        }
    }
}

async fn finished(mut done: watch::Receiver<Outcome>) -> Result<()> {
    let outcome = done
        .wait_for(Option::is_some)
        .await
        .map_err(|_| anyhow!("Agent history preparation was cancelled"))?
        .clone();
    match outcome {
        Some(Err(error)) => Err(anyhow!(error)),
        _ => Ok(()),
    }
}

fn subagent_key(parent: &str, id: &str) -> String {
    format!("{parent}\0{id}")
}

/// The replay of one opened child: its own rows (completeness capped by the
/// provider's reported coverage) and descriptor upserts. Rows for any other
/// child are dropped, so opening one child never hydrates a sibling.
fn child_replay(events: Vec<Value>, id: &str, coverage: HistoryCoverage) -> Vec<Value> {
    events
        .into_iter()
        .filter_map(|mut envelope| {
            if envelope["type"] != "provider_subagent" {
                return None;
            }
            let event = &mut envelope["event"];
            match event["type"].as_str() {
                Some("timeline") if event["id"] == id => {
                    if coverage != HistoryCoverage::Complete {
                        event["historyComplete"] = json!(false);
                    }
                }
                Some("upsert") => {}
                _ => return None,
            }
            Some(envelope)
        })
        .collect()
}

impl AgentManager {
    /// Observation-only overlay. A failure recorded before a live owner took
    /// over is not reported once that owner serves the agent.
    pub(super) fn overlay_preparation(&self, snapshot: &mut Value, serving: bool) {
        let status = text(snapshot, "id").and_then(|id| self.preparations.status(id));
        if let Some(status) = status.filter(|status| !serving || status["status"] == "preparing") {
            snapshot["preparation"] = status;
        }
    }
    pub(super) async fn publish_preparation(&self, id: &str) {
        match self.observe_agent_snapshot(id).await {
            Ok(Some(agent)) if agent["internal"] != true => {
                let _ = self
                    .events
                    .send(json!({"type":"agent_preparation","agent":agent}));
            }
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(agent_id = id, %error, "Agent preparation state unavailable")
            }
        }
    }
    /// Starts, or joins, the owned background preparation of `id`'s provider
    /// and history. A failed attempt is recorded for observation and retried
    /// by the next saved read.
    fn prepare(&self, id: &str) -> Result<watch::Receiver<Outcome>> {
        if self.closing.load(Ordering::Acquire) {
            bail!("Agent manager is shutting down");
        }
        let (generation, sender, done) = match self.preparations.begin(id) {
            Begin::Joined(done) => return Ok(done),
            Begin::Done => return Ok(watch::channel(Some(Ok(()))).1),
            Begin::Started {
                generation,
                sender,
                done,
            } => (generation, sender, done),
        };
        let app = self.app.clone();
        let key = id.to_owned();
        let task = tokio::spawn(async move {
            let Some(manager) = app.upgrade().map(|app| app.agents.clone()) else {
                return;
            };
            manager.publish_preparation(&key).await;
            let result = manager
                .ensure_loaded(&key)
                .await
                .map(drop)
                .map_err(|error| error.to_string());
            if let Err(error) = &result {
                tracing::warn!(agent_id = %key, %error, "Agent preparation failed; saved history remains readable");
            }
            manager.preparations.settle(
                &key,
                generation,
                result.as_ref().err().cloned().map(Flight::Failed),
            );
            let _ = sender.send(Some(result));
            manager.publish_preparation(&key).await;
        });
        self.preparations
            .attach(id, generation, task.abort_handle());
        Ok(done)
    }
    /// A capable client's parent page. A ready runtime answers with its live
    /// projection; otherwise the saved projection answers immediately and
    /// preparation continues in the background. An agent with no checkpoint
    /// has nothing saved to show, so that read waits for preparation instead
    /// of presenting an empty conversation as its history.
    pub async fn saved_timeline(&self, id: &str, options: &Value) -> Result<Value> {
        if let Some(page) = self.projection_page(id, options).await? {
            if page["historySource"] == "saved" {
                self.prepare(id)?;
            }
            return Ok(page);
        }
        self.agent_snapshot(id)
            .await?
            .ok_or_else(|| anyhow!("Unknown agent '{id}'"))?;
        if let Some(mut page) = self.read_checkpoint(id, options.clone()).await? {
            self.prepare(id)?;
            page["historySource"] = json!("saved");
            return Ok(page);
        }
        finished(self.prepare(id)?).await?;
        self.projection_page(id, options)
            .await?
            .ok_or_else(|| anyhow!("Agent '{id}' closed before its history was read"))
    }
    /// The in-memory projection, labelled by whether a live owner serves it.
    async fn projection_page(&self, id: &str, options: &Value) -> Result<Option<Value>> {
        let Some(cell) = ({ self.agents.read().await.get(id).cloned() }) else {
            return Ok(None);
        };
        let live = cell.data.lock().await.serves_runtime();
        let mut page = cell
            .timeline
            .lock()
            .map_err(|_| anyhow!("Timeline lock poisoned"))?
            .fetch(options);
        page["historySource"] = json!(if live { "live" } else { "saved" });
        Ok(Some(page))
    }
    /// One page of the agent's checkpoint, or `None` when it has none. A
    /// present but unreadable checkpoint is an error, never an empty history.
    async fn read_checkpoint(&self, id: &str, options: Value) -> Result<Option<Value>> {
        let path = self.timeline_path(id)?;
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            if !path.try_exists()? {
                return Ok(None);
            }
            Timeline::fetch_saved(&path, &id, &options).map(Some)
        })
        .await?
    }
    /// The current projection epoch without starting a provider; `None` when
    /// the agent has neither a runtime nor a checkpoint.
    pub async fn timeline_epoch(&self, id: &str) -> Result<Option<String>> {
        let page = match self
            .projection_page(id, &json!({"direction":"tail","limit":1}))
            .await?
        {
            Some(page) => Some(page),
            None => {
                self.read_checkpoint(id, json!({"direction":"tail","limit":1}))
                    .await?
            }
        };
        Ok(page.and_then(|page| page["epoch"].as_str().map(str::to_owned)))
    }
    /// Runs `read` on the runtime projection, else on the saved checkpoint,
    /// without starting a provider. Only an agent with neither is loaded.
    pub(super) async fn read_projection<T: Send + 'static>(
        &self,
        id: &str,
        read: impl FnOnce(&Timeline) -> T + Send + 'static,
    ) -> Result<T> {
        let cell = { self.agents.read().await.get(id).cloned() };
        let cell = match cell {
            Some(cell) => cell,
            None => {
                self.agent_snapshot(id)
                    .await?
                    .ok_or_else(|| anyhow!("Unknown agent '{id}'"))?;
                let path = self.timeline_path(id)?;
                if tokio::fs::try_exists(&path).await? {
                    let id = id.to_owned();
                    return tokio::task::spawn_blocking(move || {
                        Timeline::load(&path, &id).map(|timeline| read(&timeline))
                    })
                    .await?;
                }
                self.ensure_loaded(id).await?
            }
        };
        let timeline = cell
            .timeline
            .lock()
            .map_err(|_| anyhow!("Timeline lock poisoned"))?;
        Ok(read(&timeline))
    }

    /// Reads `id`'s stored page under `parent`, fetching that child's native
    /// body first when nothing was stored for it. Stored rows answer at once;
    /// the native body then refreshes once per daemon run in the background.
    /// Siblings are never read or hydrated.
    pub(super) async fn read_subagent(
        &self,
        root: &Path,
        parent: &str,
        id: &str,
        options: &Value,
    ) -> Result<Value> {
        let mut page = self.stored_subagent_page(root, parent, id, options).await?;
        let Some(done) = self.fetch_subagent_body(root, parent, id)? else {
            return Ok(page);
        };
        if page["window"]["maxSeq"].as_i64().unwrap_or(0) > 0 {
            if let Some(refresh) = self.subagent_refresh(parent, id) {
                page["nativeRefresh"] = refresh;
            }
            return Ok(page);
        }
        finished(done).await?;
        self.stored_subagent_page(root, parent, id, options).await
    }
    /// `{status:"refreshing"}` while the child's native body read runs, or
    /// `{status:"failed",error}` after the latest one failed; absent otherwise.
    fn subagent_refresh(&self, parent: &str, id: &str) -> Option<Value> {
        let mut status = self.subagent_reads.status(&subagent_key(parent, id))?;
        if status["status"] == "preparing" {
            status["status"] = json!("refreshing");
        }
        Some(status)
    }
    /// Observation-only overlay on a client-facing child descriptor.
    pub(super) fn overlay_subagent_refresh(&self, descriptor: &mut Value) {
        let refresh = match (text(descriptor, "parentAgentId"), text(descriptor, "id")) {
            (Some(parent), Some(id)) => self.subagent_refresh(parent, id),
            _ => None,
        };
        if let Some(refresh) = refresh {
            descriptor["nativeRefresh"] = refresh;
        }
    }
    /// Announces a child's body-read state through the existing descriptor upsert.
    async fn publish_subagent_read(&self, parent: &str, id: &str) {
        let store = self.subagent_store.clone();
        let (owner, child) = (parent.to_owned(), id.to_owned());
        let descriptor = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .ok()
                .and_then(|store| store.get(&owner, &child).cloned())
        })
        .await;
        if let Ok(Some(descriptor)) = descriptor
            && let Err(error) = self
                .emit_subagent_output(
                    parent,
                    false,
                    json!({"type":"upsert","subagent":descriptor}),
                )
                .await
        {
            tracing::debug!(parent_id = parent, subagent_id = id, %error, "Subagent read state not published");
        }
    }
    async fn stored_subagent_page(
        &self,
        root: &Path,
        parent: &str,
        id: &str,
        options: &Value,
    ) -> Result<Value> {
        let store = self.subagent_store.clone();
        let root = root.to_owned();
        let (parent, id, options) = (parent.to_owned(), id.to_owned(), options.clone());
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| anyhow!("Subagent lock poisoned"))?;
            let mut page = store.fetch_persisted(&root, &parent, &id, &options)?;
            if let Some(descriptor) = store.get(&parent, &id) {
                page["provider"] = descriptor["provider"].clone();
            }
            Ok(page)
        })
        .await?
    }
    /// Single-flight native body read for one child; `None` once a finished
    /// child's read succeeded in this daemon run. A running child reads again
    /// on every open; a failure stays observable until the next open retries it.
    fn fetch_subagent_body(
        &self,
        root: &Path,
        parent: &str,
        id: &str,
    ) -> Result<Option<watch::Receiver<Outcome>>> {
        if self.closing.load(Ordering::Acquire) {
            bail!("Agent manager is shutting down");
        }
        let key = subagent_key(parent, id);
        let (generation, sender, done) = match self.subagent_reads.begin(&key) {
            Begin::Joined(done) => return Ok(Some(done)),
            Begin::Done => return Ok(None),
            Begin::Started {
                generation,
                sender,
                done,
            } => (generation, sender, done),
        };
        let app = self.app.clone();
        let (root, parent, id) = (root.to_owned(), parent.to_owned(), id.to_owned());
        let task_key = key.clone();
        let task = tokio::spawn(async move {
            let Some(manager) = app.upgrade().map(|app| app.agents.clone()) else {
                return;
            };
            manager.publish_subagent_read(&parent, &id).await;
            let result = manager
                .stream_subagent_body(root, &parent, &id)
                .await
                .map_err(|error| error.to_string());
            if let Err(error) = &result {
                tracing::warn!(parent_id = %parent, subagent_id = %id, %error, "Subagent history read failed; stored rows retained");
            }
            let running = matches!(result, Ok(BodyRead::Current))
                && manager.subagent_running(&parent, &id).await;
            manager
                .subagent_reads
                .settle(&task_key, generation, settled_read(&result, running));
            let _ = sender.send(Some(result.map(drop)));
            manager.publish_subagent_read(&parent, &id).await;
        });
        self.subagent_reads
            .attach(&key, generation, task.abort_handle());
        Ok(Some(done))
    }
    async fn subagent_running(&self, parent: &str, id: &str) -> bool {
        let store = self.subagent_store.clone();
        let (owner, child) = (parent.to_owned(), id.to_owned());
        tokio::task::spawn_blocking(move || {
            store.lock().ok().is_some_and(|store| {
                store
                    .get(&owner, &child)
                    .is_some_and(|descriptor| descriptor["status"] == "running")
            })
        })
        .await
        .unwrap_or(false)
    }
    /// Per-parent gate between child reads (shared) and starting or
    /// replaying the parent (exclusive, taken by `resume_unlocked`). A child
    /// read holds it from before the provider decides live vs. cold until its
    /// rows are applied, so a parent session cannot start in between and
    /// publish live rows the cold result would overwrite; and it waits for a
    /// parent already starting, whose history replay stage would otherwise
    /// hold the child's marker. Child reads never start the parent.
    pub(super) async fn child_read_gate(&self, parent: &str) -> Arc<RwLock<()>> {
        let mut gates = self.child_read_gates.lock().await;
        if let Some(gate) = gates.get(parent).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(RwLock::new(()));
        gates.insert(parent.to_owned(), Arc::downgrade(&gate));
        gate
    }
    /// Streams one child's native body through the parent's provider and
    /// replays it as a guarded replacement of that child alone.
    async fn stream_subagent_body(
        &self,
        root: PathBuf,
        parent: &str,
        id: &str,
    ) -> Result<BodyRead> {
        let _reading = self.child_read_gate(parent).await.read_owned().await;
        let record = self
            .agent_snapshot(parent)
            .await?
            .ok_or_else(|| anyhow!("Unknown parent agent"))?;
        let Some(handle) = record
            .get("persistence")
            .filter(|handle| handle.is_object())
            .cloned()
        else {
            // No native session exists; collected rows are all there is.
            return Ok(BodyRead::Current);
        };
        let store = self.subagent_store.clone();
        let (owner, child) = (parent.to_owned(), id.to_owned());
        let (source, status) = tokio::task::spawn_blocking(move || {
            store
                .lock()
                .map_err(|_| anyhow!("Subagent lock poisoned"))
                .map(|store| {
                    store.get(&owner, &child).map(|descriptor| {
                        (descriptor["source"].clone(), descriptor["status"].clone())
                    })
                })
        })
        .await??
        .ok_or_else(|| anyhow!("Unknown agent '{parent}\0{id}'"))?;
        let provider = text(&handle, "provider")
            .or_else(|| text(&record, "provider"))
            .ok_or_else(|| anyhow!("Agent has no provider"))?
            .to_owned();
        let factory = self.app()?.providers.get(&provider)?;
        let config = Self::stored_launch_config(&record, Some(&handle), &provider, &json!({}));
        let config = self.normalize_config_for(config, false, true).await?;
        let context = self
            .history_context(parent, &config, record.get("workspaceId"))
            .await?;
        let cancel = context.cancel.clone();
        let (sink, mut receiver) = EventSink::channel(256, cancel.clone());
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
            }
            events
        };
        let (key, replay, applied) = self.child_replays.begin();
        // However this read ends (error, cancellation, abort at shutdown), the
        // waiter goes and the provider's `outcome()` resolves.
        let _waiter = WaiterGuard {
            waiters: &self.child_replays,
            key: &key,
        };
        let request = SubagentHistoryRequest {
            persistence: handle,
            config,
            child_id: id.to_owned(),
            source,
            status,
            replay,
        };
        let (result, events) = tokio::join!(
            factory.stream_subagent_history(request, context, sink),
            collect
        );
        cancel.cancel();
        let history = result?;
        if matches!(history, SubagentHistory::Published) {
            return self.await_published_replay(parent, &key, applied).await;
        }
        // Nothing was published in-stream; release the provider's outcome.
        self.child_replays.take(&key);
        self.apply_subagent_body(root, parent, id, history, events)
            .await
    }
    /// Applies a child read returned through the hook's sink. A provider
    /// returns rows this way only when no live session of its own publishes
    /// rows for the child, so nothing can interleave with the replacement.
    /// Only a complete replay that replaced the stored rows (or a provider
    /// without a separate body) is current.
    async fn apply_subagent_body(
        &self,
        root: PathBuf,
        parent: &str,
        id: &str,
        history: SubagentHistory,
        events: Vec<Value>,
    ) -> Result<BodyRead> {
        let coverage = match history {
            SubagentHistory::Collected => return Ok(BodyRead::Current),
            SubagentHistory::Replayed(coverage) => coverage,
            SubagentHistory::Published => bail!("A published subagent replay has no hook rows"),
        };
        let replay = self
            .replay_child(root, parent, id, child_replay(events, id, coverage))
            .await?;
        Ok(body_read(
            replay.replaced,
            replay.finished && coverage == HistoryCoverage::Complete,
        ))
    }
    /// Waits for a replay the provider published on the parent's live sink,
    /// in order with its live rows, to pass the parent's event consumer.
    async fn await_published_replay(
        &self,
        parent: &str,
        key: &str,
        applied: tokio::sync::oneshot::Receiver<Result<(bool, bool), String>>,
    ) -> Result<BodyRead> {
        let Some(cell) = ({ self.agents.read().await.get(parent).cloned() }) else {
            self.child_replays.take(key);
            bail!("No live parent applies the published subagent replay");
        };
        tokio::select! {
            result = applied => {
                let (replaced, complete) = result
                    .map_err(|_| anyhow!("Parent event owner closed during the subagent replay"))?
                    .map_err(|error| anyhow!(error))?;
                Ok(body_read(replaced, complete))
            }
            _ = cell.cancel.cancelled() => {
                self.child_replays.take(key);
                bail!("Parent event owner closed during the subagent replay")
            }
        }
    }
    /// The parent's event consumer reached a provider-published child replay.
    /// It is applied exactly here in the parent's event order; the provider
    /// learns whether it was applied or retained before continuing that
    /// child's live rows, and the reader learns the same outcome.
    pub(super) async fn apply_published_replay(&self, parent: &str, envelope: &Value) {
        let event = &envelope["event"];
        let id = text(event, "id").unwrap_or_default().to_owned();
        // Taking the waiter decides the transaction. If its read already
        // ended, the provider has resolved `None` and kept its previous state,
        // so applying now would desynchronize the stored rows from it.
        let Some(waiter) = text(event, "key").and_then(|key| self.child_replays.take(key)) else {
            tracing::info!(parent_id = parent, subagent_id = %id, "Abandoned subagent replay not applied");
            return;
        };
        let coverage = if event["historyComplete"] == true {
            HistoryCoverage::Complete
        } else {
            HistoryCoverage::Unverified
        };
        let events = child_replay(
            event["events"].as_array().cloned().unwrap_or_default(),
            &id,
            coverage,
        );
        let result = match self.app() {
            Ok(app) => {
                self.replay_child(app.home.join("agent-subagents"), parent, &id, events)
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            tracing::warn!(parent_id = parent, subagent_id = %id, %error, "Published subagent replay not applied; stored rows retained");
        }
        let outcome = match &result {
            Ok(replay) if replay.replaced => ReplayOutcome::Applied,
            _ => ReplayOutcome::Retained,
        };
        let _ = waiter.provider.send(Some(outcome));
        let _ = waiter.reader.send(
            result
                .map(|replay| {
                    (
                        replay.replaced,
                        replay.finished && coverage == HistoryCoverage::Complete,
                    )
                })
                .map_err(|error| error.to_string()),
        );
    }
    /// Replaces one child's stored rows with an explicitly read native body.
    ///
    /// A complete native history is authoritative (may drop stored rows it
    /// lacks) only for a child already recorded as finished. A running child
    /// can have live rows the native history does not contain yet, however
    /// well-formed it is, so its replay stays a guarded replacement.
    async fn replay_child(
        &self,
        root: PathBuf,
        parent: &str,
        id: &str,
        events: Vec<Value>,
    ) -> Result<ChildReplay> {
        let store = self.subagent_store.clone();
        let (owner, child) = (parent.to_owned(), id.to_owned());
        let (outputs, finished) = tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| anyhow!("Subagent lock poisoned"))?;
            let (descriptors, _) = store.list_persisted(&root, &owner)?;
            let finished = descriptors
                .iter()
                .find(|descriptor| descriptor["id"] == child.as_str())
                .is_some_and(|descriptor| finished_status(&descriptor["status"]));
            let outputs = store.replay_history(Some(&root), &owner, events, finished)?;
            Ok::<_, anyhow::Error>((outputs, finished))
        })
        .await??;
        let replaced = outputs
            .iter()
            .any(|output| output["type"] == "timeline" && output["subagentId"] == id);
        if !replaced {
            tracing::info!(
                parent_id = parent,
                subagent_id = id,
                "Subagent replay did not replace stored rows; next open reads again"
            );
        }
        for output in outputs {
            self.emit_subagent_output(parent, false, output).await?;
        }
        Ok(ChildReplay { replaced, finished })
    }
}

struct ChildReplay {
    replaced: bool,
    /// The child was recorded as finished before this replay.
    finished: bool,
}

fn finished_status(status: &Value) -> bool {
    matches!(status.as_str(), Some("completed" | "failed" | "canceled"))
}

fn body_read(replaced: bool, complete: bool) -> BodyRead {
    if replaced && complete {
        BodyRead::Current
    } else {
        BodyRead::Incomplete
    }
}

type ReplayResult = Result<(bool, bool), String>;

struct ReplayWaiter {
    /// Tells the provider whether to rebase onto its snapshot or keep its state.
    provider: watch::Sender<Option<ReplayOutcome>>,
    /// Tells the reader `(replaced, complete)` or why the replay failed.
    reader: tokio::sync::oneshot::Sender<ReplayResult>,
}

/// Open two-phase child replays, by key. Each is bounded to one child read:
/// the provider holds only that child's live rows until its outcome.
#[derive(Default)]
pub(super) struct ReplayWaiters(StdMutex<HashMap<String, ReplayWaiter>>);

/// Removes a replay waiter when its read ends, however it ends, so the
/// provider's `outcome()` always resolves (to `None` when nothing applied it).
struct WaiterGuard<'a> {
    waiters: &'a ReplayWaiters,
    key: &'a str,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.waiters.take(self.key);
    }
}

impl ReplayWaiters {
    fn waiters(&self) -> std::sync::MutexGuard<'_, HashMap<String, ReplayWaiter>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn begin(
        &self,
    ) -> (
        String,
        SubagentReplay,
        tokio::sync::oneshot::Receiver<ReplayResult>,
    ) {
        let key = uuid::Uuid::new_v4().to_string();
        let (provider, outcome) = watch::channel(None);
        let (reader, applied) = tokio::sync::oneshot::channel();
        self.waiters()
            .insert(key.clone(), ReplayWaiter { provider, reader });
        (key.clone(), SubagentReplay::new(key, outcome), applied)
    }
    /// Dropping the waiter resolves the provider's outcome as `None`.
    fn take(&self, key: &str) -> Option<ReplayWaiter> {
        self.waiters().remove(key)
    }
}

/// What a successful native child read established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyRead {
    /// Complete and applied, or the provider has no separate body.
    Current,
    /// Unverified, or retained because it could not account for stored rows.
    Incomplete,
}

/// The registry entry a settled child read leaves. Only a current read of a
/// finished child is reused for the daemon run; an incomplete read or a
/// running child reads again on the next open, and a failure stays visible
/// until then.
fn settled_read(result: &Result<BodyRead, String>, running: bool) -> Option<Flight> {
    match result {
        Ok(BodyRead::Current) if !running => Some(Flight::Done),
        Ok(_) => None,
        Err(error) => Some(Flight::Failed(error.clone())),
    }
}

#[cfg(test)]
#[path = "history_read_tests.rs"]
mod tests;
