use crate::{
    providers::api::{AgentSession, EventSink, RewindTarget},
    state::AppState,
};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, OnceCell, RwLock, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

mod actions;
pub(crate) mod admission;
pub(crate) mod creation;
mod date_parse;
pub(crate) mod event_stage;
mod generation;
mod history_read;
mod listing;
pub mod notifications;
pub mod receipts;
mod runtime;
pub mod storage;
pub mod timeline;
mod wire;
pub use generation::{StructuredAgentFallbackError, StructuredAgentResponseError};
pub(crate) use generation::{compile_schema, retry_prompt, validate_response};
pub use listing::select_agent_list_items;
use event_stage::EventStage;
use notifications::{AgentMessageDelivery, FinishSubscriptions};
use receipts::{CreationReceipts, MessageReceipts};
use storage::AgentStorage;
use timeline::{StreamCoalescer, SubagentStore, Timeline};

pub fn snapshot_payload(snapshot: &Value) -> Value {
    wire::snapshot_payload(snapshot)
}
pub fn serialize_stream_event(event: &Value) -> Option<Value> {
    wire::serialize_stream_event(event)
}
pub fn project_legacy_event(
    context: &crate::transport::RequestContext,
    message: &Value,
) -> Option<Value> {
    wire::project_legacy_event(context, message)
}
pub async fn legacy_attention_eligible(
    manager: &AgentManager,
    context: &crate::transport::RequestContext,
    snapshot: &Value,
) -> Result<bool> {
    wire::legacy_attention_eligible(manager, context, snapshot).await
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
/// Every touch strictly advances `updatedAt`, even within one millisecond or after a
/// clock step backwards, so directory stamps and activity ordering never regress.
fn next_updated_at(previous: &Value) -> String {
    let now = chrono::Utc::now();
    let next = previous
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|previous| previous.with_timezone(&chrono::Utc))
        .filter(|previous| previous.timestamp_millis() >= now.timestamp_millis())
        .map_or(now, |previous| {
            chrono::DateTime::from_timestamp_millis(previous.timestamp_millis() + 1)
                .unwrap_or(now)
        });
    next.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
/// A stored record's change time is the later of `updatedAt` and `lastActivityAt`:
/// metadata writes on unloaded agents move one without the other.
fn stored_updated_at(record: &Value) -> Option<String> {
    // Reversed so `max_by_key` (last maximum wins) keeps `updatedAt` on equal times.
    ["lastActivityAt", "updatedAt"]
        .iter()
        .filter_map(|key| text(record, key).filter(|value| !value.is_empty()))
        .filter_map(|raw| {
            date_parse::parse_date(raw)
                .ok()
                .flatten()
                .map(|parsed| (parsed, raw))
        })
        .max_by_key(|(parsed, _)| *parsed)
        .map(|(_, raw)| raw.to_owned())
}
fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    text(value, key).ok_or_else(|| anyhow!("Missing {key}"))
}
fn object(value: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !value.is_object() {
        *value = json!({});
    }
    value.as_object_mut().unwrap()
}
/// runtime-mcp-config.ts stripInternalPaseoMcpServer: the daemon-owned server lives in
/// `config.mcpServers.paseo`; a map emptied by removing it is dropped.
fn strip_config_internal_servers(config: &mut Value) {
    let Some(servers) = config.get_mut("mcpServers").filter(|servers| servers.is_object()) else {
        return;
    };
    let before = servers.as_object().map_or(0, |servers| servers.len());
    *servers = crate::mcp::strip_internal_servers(servers.take());
    if servers
        .as_object()
        .is_some_and(|servers| servers.is_empty() && before > 0)
    {
        object(config).remove("mcpServers");
    }
}
fn merge(target: &mut Value, source: &Value) {
    if let Some(source) = source.as_object() {
        for (key, value) in source {
            object(target).insert(key.clone(), value.clone());
        }
    }
}
fn is_busy(snapshot: &Value) -> bool {
    matches!(
        text(snapshot, "lifecycle").or_else(|| text(snapshot, "status")),
        Some("running" | "initializing")
    )
}
fn with_cwd(mut handle: Value, cwd: &Value) -> Value {
    if handle.is_object() {
        if !handle["metadata"].is_object() {
            handle["metadata"] = json!({});
        }
        handle["metadata"]["cwd"] = cwd.clone();
    }
    handle
}

struct AgentData {
    snapshot: Value,
    session: Option<Arc<dyn AgentSession>>,
    ready: bool,
    history_primed: bool,
    finalized: VecDeque<String>,
    resolutions: HashMap<String, Value>,
    event_error: Option<String>,
    owner_ended: bool,
    /// agent-manager.ts pendingReplacement: a replacing prompt holds the agent busy
    /// while the interrupted turn settles.
    pending_replacement: bool,
}
impl AgentData {
    /// A live, history-synchronized owner that may serve reads and mutations.
    fn serves_runtime(&self) -> bool {
        self.ready
            && self.history_primed
            && self.session.is_some()
            && !self.owner_ended
            && self.event_error.is_none()
    }
}
struct AgentCell {
    data: Mutex<AgentData>,
    mutation: Mutex<()>,
    commit: Mutex<()>,
    timeline: Arc<StdMutex<Timeline>>,
    cancel: CancellationToken,
    owner_ended: CancellationToken,
    sink: EventSink,
    stage: Mutex<Option<EventStage>>,
    barriers: Mutex<HashMap<String, oneshot::Sender<()>>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

pub struct AgentManager {
    app: Weak<AppState>,
    pub(super) storage: OnceCell<AgentStorage>,
    pub(super) receipts: OnceCell<MessageReceipts>,
    pub(super) creations: OnceCell<CreationReceipts>,
    creation_state: creation::CreationState,
    pub directory_sync: crate::directory_sync::DirectorySyncService,
    pub notifications: FinishSubscriptions,
    pub message_delivery: AgentMessageDelivery,
    pub(crate) update_admission: Arc<admission::WorkAdmission>,
    agents: RwLock<HashMap<String, Arc<AgentCell>>>,
    lifecycle: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    subagent_store: Arc<StdMutex<SubagentStore>>,
    /// Owned background provider preparation, one per parent agent.
    preparations: history_read::Flights,
    /// Owned native child-body reads, one per parent/child pair.
    subagent_reads: history_read::Flights,
    /// Child-body readers waiting for their replay to pass a live parent's events.
    child_replays: history_read::ReplayWaiters,
    /// Per-parent gates: child reads shared, parent start/replay exclusive.
    child_read_gates: Mutex<HashMap<String, Weak<RwLock<()>>>>,
    events: broadcast::Sender<Value>,
    closing: AtomicBool,
}

impl AgentManager {
    pub fn new(app: Weak<AppState>) -> Self {
        let (events, _) = broadcast::channel(2048);
        Self {
            notifications: FinishSubscriptions::new(app.clone()),
            message_delivery: AgentMessageDelivery::new(app.clone()),
            update_admission: Arc::new(admission::WorkAdmission::default()),
            app,
            storage: OnceCell::new(),
            receipts: OnceCell::new(),
            creations: OnceCell::new(),
            creation_state: creation::CreationState::default(),
            directory_sync: crate::directory_sync::DirectorySyncService::new(),
            agents: RwLock::new(HashMap::new()),
            lifecycle: Mutex::new(HashMap::new()),
            subagent_store: Arc::new(StdMutex::new(SubagentStore::default())),
            preparations: history_read::Flights::default(),
            subagent_reads: history_read::Flights::default(),
            child_replays: history_read::ReplayWaiters::default(),
            child_read_gates: Mutex::new(HashMap::new()),
            events,
            closing: AtomicBool::new(false),
        }
    }
    pub(super) fn app(&self) -> Result<Arc<AppState>> {
        self.app
            .upgrade()
            .ok_or_else(|| anyhow!("Agent manager is shutting down"))
    }
    pub(super) fn storage(&self) -> Result<&AgentStorage> {
        self.storage
            .get()
            .ok_or_else(|| anyhow!("Agent storage is not initialized"))
    }
    pub(super) fn receipts(&self) -> Result<&MessageReceipts> {
        self.receipts
            .get()
            .ok_or_else(|| anyhow!("Agent receipts are not initialized"))
    }
    pub(super) fn creations(&self) -> Result<&CreationReceipts> {
        self.creations
            .get()
            .ok_or_else(|| anyhow!("Creation receipts are not initialized"))
    }
    pub async fn initialize(&self) -> Result<()> {
        let home = self.app()?.home.clone();
        self.storage
            .get_or_try_init(|| async {
                let store = AgentStorage::new(home.clone());
                store.initialize().await?;
                Ok::<_, anyhow::Error>(store)
            })
            .await?;
        let fresh = self.receipts.get().is_none();
        let receipts = self
            .receipts
            .get_or_init(|| async { MessageReceipts::new(home.clone()) })
            .await;
        if fresh {
            wire::forward_receipt_updates(self.app.clone(), receipts.subscribe_events());
        }
        self.creations
            .get_or_init(|| async { CreationReceipts::new(home.clone()) })
            .await;
        self.restore_persisted_state().await?;
        crate::daemon_admin::update_operation::initialize_admission(&self.app()?).await?;
        self.notifications.initialize().await
    }
    // Catalogs are read per parent on demand; unrelated corruption cannot abort startup.
    async fn restore_persisted_state(&self) -> Result<()> {
        for mut record in self.storage()?.list().await? {
            let interrupted = matches!(text(&record, "lastStatus"), Some("running" | "initializing"));
            let transient = object(&mut record).remove("snapshotAvailability").is_some();
            if interrupted {
                record["lastStatus"] = json!("closed");
                record["lastTurnOutcome"] = json!("turn_canceled");
            }
            if interrupted || transient {
                // Recovery is not new user activity. Keep timestamps, attention,
                // provider identity and history intact while discarding view-only state.
                self.storage()?.upsert(record.clone()).await?;
            }
        }
        Ok(())
    }
    pub async fn handle(
        &self,
        context: &crate::transport::RequestContext,
        message: &Value,
    ) -> Result<bool> {
        if self.notifications.handle(context, message).await? {
            return Ok(true);
        }
        wire::handle(self, context, message).await
    }
    pub fn subscribe_events(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }
    pub async fn register_finish_subscription(
        &self,
        input: Value,
        pending: bool,
    ) -> Result<notifications::FinishRegistration> {
        self.notifications.register(input, pending).await
    }
    pub async fn has_in_flight_run(&self, id: &str) -> Result<bool> {
        let cell = self.cell(id).await?;
        let has_session = {
            let data = cell.data.lock().await;
            if data.session.is_none() || data.owner_ended {
                return Ok(false);
            }
            if is_busy(&data.snapshot) || data.snapshot["activeForegroundTurnId"].is_string() {
                return Ok(true);
            }
            data.session.is_some()
        };
        Ok(has_session && cell.stage.lock().await.is_some())
    }
    async fn lane(&self, id: &str) -> Arc<Mutex<()>> {
        let mut lanes = self.lifecycle.lock().await;
        if let Some(lane) = lanes.get(id).and_then(Weak::upgrade) {
            return lane;
        }
        lanes.retain(|_, lane| lane.strong_count() > 0);
        let lane = Arc::new(Mutex::new(()));
        lanes.insert(id.to_owned(), Arc::downgrade(&lane));
        lane
    }
    async fn cell(&self, id: &str) -> Result<Arc<AgentCell>> {
        self.agents
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown agent '{id}'"))
    }
    pub async fn session(&self, id: &str) -> Result<Option<Arc<dyn AgentSession>>> {
        let cell = { self.agents.read().await.get(id).cloned() };
        Ok(match cell {
            Some(cell) => {
                let data = cell.data.lock().await;
                if data.owner_ended { None } else { data.session.clone() }
            }
            None => None,
        })
    }
    /// Authoritative state for ownership checks and mutations, including internal
    /// agents which have no persisted fallback. Never substitute a read-only view.
    pub async fn agent_snapshot(&self, id: &str) -> Result<Option<Value>> {
        let cell = { self.agents.read().await.get(id).cloned() };
        if let Some(cell) = cell {
            return Ok(Some(cell.data.lock().await.snapshot.clone()));
        }
        Ok(self.storage()?.get(id).await?.map(|record| self.stored_snapshot(record)))
    }
    /// Nonblocking observation only. Callers must not persist or authorize from
    /// a snapshot marked as last_persisted.
    pub async fn observe_agent_snapshot(&self, id: &str) -> Result<Option<Value>> {
        let cell = { self.agents.read().await.get(id).cloned() };
        let mut unavailable = false;
        if let Some(cell) = cell {
            match cell.data.try_lock() {
                Ok(data) if data.ready => {
                    let mut snapshot = data.snapshot.clone();
                    self.overlay_preparation(&mut snapshot, data.serves_runtime());
                    return Ok(Some(snapshot));
                }
                Ok(_) => unavailable = true,
                Err(_) => unavailable = true,
            }
        }
        let record = self.storage()?.get(id).await?;
        if unavailable && record.is_none() {
            bail!("Agent '{id}' snapshot is temporarily unavailable");
        }
        Ok(record.map(|record| {
            let mut snapshot = self.stored_snapshot(record);
            if unavailable {
                snapshot["snapshotAvailability"] = json!("last_persisted");
            }
            self.overlay_preparation(&mut snapshot, false);
            snapshot
        }))
    }
    fn stored_snapshot(&self, mut record: Value) -> Value {
        object(&mut record).remove("snapshotAvailability");
        let available = self.app.upgrade().is_some_and(|app| {
            text(&record, "provider").is_some_and(|id| app.providers.spec(id).is_some())
        });
        record["_live"] = json!(false);
        record["_providerAvailable"] = json!(available);
        record["status"] = record.get("lastStatus").cloned().unwrap_or(json!("idle"));
        record["lifecycle"] = record["status"].clone();
        if let Some(updated) = stored_updated_at(&record) {
            record["updatedAt"] = json!(updated);
        }
        record["currentModeId"] = record.get("lastModeId").cloned().unwrap_or(Value::Null);
        record["availableModes"] = json!([]);
        record["pendingPermissions"] = json!([]);
        record["activeTurn"] = Value::Null;
        record["activeTurnId"] = Value::Null;
        record["activeForegroundTurnId"] = Value::Null;
        record["attention"] = json!({"requiresAttention":record["requiresAttention"]==true,"attentionReason":record["attentionReason"],"attentionTimestamp":record["attentionTimestamp"]});
        if record.get("capabilities").is_none() {
            record["capabilities"] = json!({"supportsStreaming":false,"supportsSessionPersistence":true,"supportsDynamicModes":false,"supportsMcpServers":false,"supportsReasoningStream":false,"supportsToolInvocations":true,"supportsRewindConversation":false,"supportsRewindFiles":false,"supportsRewindBoth":false});
        }
        record
    }
    pub async fn list_snapshots(&self) -> Result<Vec<Value>> {
        let mut all: HashMap<String, Value> = self
            .storage()?
            .list()
            .await?
            .into_iter()
            .filter_map(|record| {
                text(&record, "id")
                    .map(str::to_owned)
                    .map(|id| (id, self.stored_snapshot(record)))
            })
            .collect();
        let cells = self
            .agents
            .read()
            .await
            .iter()
            .map(|(id, cell)| (id.clone(), cell.clone()))
            .collect::<Vec<_>>();
        for (id, cell) in cells {
            match cell.data.try_lock() {
                Ok(data) if data.ready => {
                    let mut snapshot = data.snapshot.clone();
                    self.overlay_preparation(&mut snapshot, data.serves_runtime());
                    all.insert(id, snapshot);
                }
                _ => {
                    if let Some(snapshot) = all.get_mut(&id) {
                        snapshot["snapshotAvailability"] = json!("last_persisted");
                    }
                }
            }
        }
        for snapshot in all.values_mut().filter(|snapshot| snapshot["_live"] == false) {
            self.overlay_preparation(snapshot, false);
        }
        Ok(all.into_values().collect())
    }
    /// Usage discovery is a read-only observation, never a waiter for provider
    /// mutation/commit locks. Preserve unavailable identities rather than
    /// interpreting them as absent sessions.
    pub async fn usage_session_snapshots(&self) -> Result<Vec<Value>> {
        let snapshots = self.list_snapshots().await?;
        let mut result = Vec::with_capacity(snapshots.len());
        for snapshot in snapshots {
            let id = required(&snapshot, "id")?;
            let cell = { self.agents.read().await.get(id).cloned() };
            let (session, unavailable) = match cell {
                Some(cell) => match cell.data.try_lock() {
                    Ok(data) => (if data.owner_ended { None } else { data.session.clone() }, !data.ready),
                    Err(_) => (None, true),
                },
                None => (None, false),
            };
            let mut usage = session.and_then(|session| session.usage_session())
                .unwrap_or_else(|| json!({}));
            usage["agentId"] = json!(id);
            if unavailable {
                usage["_runtimeUnavailable"] = json!(true);
            }
            result.push(usage);
        }
        Ok(result)
    }
    pub async fn cwd(&self, id: &str) -> Result<PathBuf> {
        let snapshot = self
            .agent_snapshot(id)
            .await?
            .ok_or_else(|| anyhow!("Unknown agent '{id}'"))?;
        Ok(PathBuf::from(required(&snapshot, "cwd")?))
    }
    pub(super) async fn authorize(&self, actor: &Value, id: &str) -> Result<()> {
        let snapshot = self
            .agent_snapshot(id)
            .await?
            .ok_or_else(|| anyhow!("Unknown agent '{id}'"))?;
        match text(actor, "kind") {
            Some("daemon") => {
                let owner = &snapshot["owner"];
                if owner["kind"] != "daemon"
                    || owner["daemonId"] != actor["daemonId"]
                    || owner["executionId"] != actor["executionId"]
                {
                    bail!("Agent is not owned by this daemon execution");
                }
            }
            Some("agent") => {
                let caller = required(actor, "agentId")?;
                if self.agent_snapshot(caller).await?.is_none() {
                    bail!("Caller agent is unavailable");
                }
            }
            Some("owner" | "system") => {}
            _ => {
                if actor.is_null() {
                    bail!("Missing agent operation authority");
                }
            }
        }
        Ok(())
    }
    pub(super) async fn normalize_config(
        &self,
        config: Value,
        resolve_default_model: bool,
    ) -> Result<Value> {
        self.normalize_config_for(config, resolve_default_model, false)
            .await
    }
    async fn normalize_config_for(
        &self,
        mut config: Value,
        resolve_default_model: bool,
        history: bool,
    ) -> Result<Value> {
        let app = self.app()?;
        let provider = required(&config, "provider")?.to_owned();
        app.providers.get(&provider)?;
        let options = config.get("providerOptions").cloned().unwrap_or(json!({}));
        config["providerOptions"] = app.providers.validate_options(&provider, &options)?;
        let cwd = required(&config, "cwd")?;
        let expanded = if cwd == "~" {
            app.user_home.clone()
        } else if let Some(rest) = cwd.strip_prefix("~/") {
            app.user_home.join(rest)
        } else {
            PathBuf::from(cwd)
        };
        let cwd = if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()?.join(expanded)
        };
        if !history
            && !tokio::fs::metadata(&cwd)
                .await
                .with_context(|| format!("Working directory does not exist: {}", cwd.display()))?
                .is_dir()
        {
            bail!("Working directory is not a directory: {}", cwd.display());
        }
        config["cwd"] = json!(cwd);
        object(&mut config).remove("daemonAppendSystemPrompt");
        strip_config_internal_servers(&mut config);
        if let Some(model) = text(&config, "model") {
            let model = model.trim().to_owned();
            if model.is_empty() || model == "default" {
                object(&mut config).remove("model");
            } else {
                config["model"] = json!(model);
            }
        }
        if resolve_default_model && config.get("model").is_none() {
            if let Ok(catalog) = app.providers.catalog(&provider, Some(&cwd), true).await {
                if let Some(models) = catalog["models"].as_array() {
                    if let Some(model) = models
                        .iter()
                        .find(|m| m["isDefault"] == true)
                        .or_else(|| models.first())
                        .and_then(|m| m.get("id"))
                    {
                        config["model"] = model.clone();
                    }
                }
            }
        }
        if let Some(grants) = config["toolPolicy"]["preapproved"].as_array() {
            for grant in grants {
                let server = required(grant, "server")?;
                if config["mcpServers"].get(server).is_none() {
                    bail!(
                        "toolPolicy preapproval '{}.{}' requires MCP server '{}' in the same agent request",
                        server,
                        text(grant, "tool").unwrap_or(""),
                        server
                    );
                }
            }
        }
        if let Some(policy) = config.get("toolPolicy").cloned() {
            config = app
                .providers
                .apply_tool_policy(&provider, config, &policy)?;
        }
        Ok(config)
    }
    async fn make_cell(
        &self,
        id: &str,
        mut snapshot: Value,
        history: bool,
    ) -> Result<(Arc<AgentCell>, mpsc::Receiver<Value>)> {
        // Drain final provider events after daemon cancellation, before committing closed state.
        let cancel = CancellationToken::new();
        let (sink, receiver) = EventSink::channel(256, cancel.clone());
        let path = self.timeline_path(id)?;
        let owned_id = id.to_owned();
        let timeline =
            tokio::task::spawn_blocking(move || Timeline::load(&path, &owned_id)).await??;
        object(&mut snapshot).remove("snapshotAvailability");
        snapshot["_live"] = json!(true);
        snapshot["_providerAvailable"] = json!(true);
        snapshot["lifecycle"] = json!("initializing");
        snapshot["status"] = json!("initializing");
        snapshot["pendingPermissions"] = json!([]);
        snapshot["availableModes"] = json!([]);
        snapshot["activeTurn"] = Value::Null;
        snapshot["activeTurnId"] = Value::Null;
        snapshot["activeForegroundTurnId"] = Value::Null;
        let cell = Arc::new(AgentCell {
            data: Mutex::new(AgentData {
                snapshot,
                session: None,
                ready: false,
                history_primed: history,
                finalized: VecDeque::new(),
                resolutions: HashMap::new(),
                event_error: None,
                owner_ended: false,
                pending_replacement: false,
            }),
            mutation: Mutex::new(()),
            commit: Mutex::new(()),
            timeline: Arc::new(StdMutex::new(timeline)),
            cancel,
            owner_ended: CancellationToken::new(),
            sink,
            // Replay history before live events, even when the saved checkpoint is partial.
            stage: Mutex::new((!history).then(EventStage::default)),
            barriers: Mutex::new(HashMap::new()),
            task: Mutex::new(None),
        });
        self.agents
            .write()
            .await
            .insert(id.to_owned(), cell.clone());
        Ok((cell, receiver))
    }
    fn start_events(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        mut receiver: mpsc::Receiver<Value>,
    ) -> tokio::task::JoinHandle<()> {
        let app = self.app.clone();
        let id = id.to_owned();
        let cell = Arc::downgrade(cell);
        tokio::spawn(async move {
            let mut coalescer = StreamCoalescer::new(60);
            loop {
                let Some(current) = cell.upgrade() else { break };
                let cancel = current.cancel.clone();
                drop(current);
                let deadline = coalescer.next_deadline().map(|deadline| {
                    Duration::from_millis(
                        deadline
                            .saturating_sub(chrono::Utc::now().timestamp_millis().max(0) as u64),
                    )
                });
                let event = tokio::select! {biased;event=receiver.recv()=>event,_=cancel.cancelled()=>None,_=async{match deadline{Some(delay)=>tokio::time::sleep(delay).await,None=>std::future::pending::<()>().await}}=>Some(json!({"type":"__flush"}))};
                let Some(state) = app.upgrade() else { break };
                let manager = state.agents.clone();
                drop(state);
                let Some(current) = cell.upgrade() else { break };
                let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
                let mut flushed = coalescer.flush_due(now_ms);
                let closing = event.is_none();
                let mut direct = None;
                if let Some(event) = event {
                    if event["type"] != "__flush" {
                        if matches!(text(&event, "type"), Some("__barrier" | "__release_stage")) {
                            flushed.extend(coalescer.flush_for(&id, now_ms));
                            direct = Some(event);
                        } else if let Some(output) = coalescer.handle(&id, &event, now_ms) {
                            flushed.extend(output);
                        } else {
                            flushed.extend(coalescer.flush_for(&id, now_ms));
                            direct = Some(event);
                        }
                    }
                } else {
                    flushed.extend(coalescer.flush_all(now_ms));
                }
                let result=async {for output in flushed{let mut event=json!({"type":"timeline","provider":output["provider"],"item":output["item"]});if let Some(turn)=output.get("turnId"){event["turnId"]=turn.clone();}manager.apply_event(&id,&current,event,false).await?;}
                    if let Some(event)=direct {
                        if matches!(text(&event,"type"),Some("__barrier"|"__release_stage")){
                            if event["type"]=="__release_stage" {
                                let staged = current.stage.lock().await.take();
                                if let Some(staged) = staged {
                                    let mut replay = staged.replay().await?;
                                    loop {
                                        let batch = replay.next_batch().await?;
                                        if batch.is_empty() { break; }
                                        for event in batch {
                                            manager.apply_event(&id, &current, event, false).await?;
                                        }
                                    }
                                }
                            }
                            if let Some(key)=text(&event,"key"){if let Some(sender)=current.barriers.lock().await.remove(key){let _=sender.send(());}}
                        }else{manager.apply_event(&id,&current,event,false).await?;}
                    }
                    Ok::<_,anyhow::Error>(())}.await;
                if let Err(error) = result {
                    manager.fail_event_owner(&id, &current, &error).await;
                    break;
                }
                if closing {
                    let unexpected = {
                        let data = current.data.lock().await;
                        data.ready && !data.owner_ended && data.snapshot["lifecycle"] != "closed"
                    };
                    if unexpected {
                        manager.fail_event_owner(
                            &id, &current, &anyhow!("Agent event consumer closed unexpectedly"),
                        ).await;
                    }
                    break;
                }
            }
        })
    }
    async fn drain_events(&self, cell: &Arc<AgentCell>) -> Result<()> {
        self.event_barrier(cell, false).await
    }
    async fn begin_event_stage(&self, cell: &Arc<AgentCell>) -> Result<()> {
        self.drain_events(cell).await?;
        let mut stage = cell.stage.lock().await;
        if stage.is_some() {
            bail!("Agent already has a pending event stage");
        }
        *stage = Some(EventStage::default());
        Ok(())
    }
    async fn release_event_stage(&self, cell: &Arc<AgentCell>) -> Result<()> {
        self.event_barrier(cell, true).await
    }
    async fn event_barrier(&self, cell: &Arc<AgentCell>, release: bool) -> Result<()> {
        if let Some(error) = cell.data.lock().await.event_error.clone() {
            bail!("Agent event persistence failed: {error}");
        }
        let key = uuid::Uuid::new_v4().to_string();
        let (sender, receiver) = oneshot::channel();
        cell.barriers.lock().await.insert(key.clone(), sender);
        let result = async {
            cell.sink.emit(json!({
                "type":if release { "__release_stage" } else { "__barrier" },
                "key":key
            })).await?;
            tokio::select! {
                result = receiver => result.map_err(|_| anyhow!("Agent event consumer closed")),
                _ = cell.cancel.cancelled() => Err(anyhow!("Agent event consumer closed")),
            }
        }.await;
        cell.barriers.lock().await.remove(&key);
        result
    }
    async fn fail_event_owner(&self, id: &str, cell: &Arc<AgentCell>, error: &anyhow::Error) {
        let message = error.to_string();
        tracing::error!(agent_id=%id,error=%message,"Failed to persist agent provider event");
        cell.stage.lock().await.take();
        let (provider, session) = {
            let _commit = cell.commit.lock().await;
            let mut data = cell.data.lock().await;
            data.event_error = Some(message.clone());
            data.owner_ended = true;
            data.snapshot["_live"] = json!(false);
            data.pending_replacement = false;
            if let Some(turn) = text(&data.snapshot, "activeTurnId").map(str::to_owned) {
                // A failed terminal commit may already have entered deduplication.
                data.finalized.retain(|finalized| finalized != &turn);
            }
            (data.snapshot["provider"].clone(), data.session.take())
        };
        cell.cancel.cancel();
        cell.owner_ended.cancel();
        cell.barriers.lock().await.clear();
        if let Err(report_error) = self.apply_event(
            id, cell, json!({"type":"turn_failed","provider":provider,"error":message}), false,
        ).await {
            // The original failure may itself be a failed disk commit. Keep the
            // in-memory state terminal even when no durable error row can be written.
            tracing::error!(agent_id=%id,error=%report_error,"Failed to persist event owner failure");
            let _commit = cell.commit.lock().await;
            let mut data = cell.data.lock().await;
            let snapshot = &mut data.snapshot;
            snapshot["lifecycle"] = json!("error");
            snapshot["status"] = json!("error");
            snapshot["lastTurnOutcome"] = json!("turn_failed");
            snapshot["lastError"] = json!(message);
            snapshot["updatedAt"] = json!(now());
            snapshot["activeTurnId"] = Value::Null;
            snapshot["activeForegroundTurnId"] = Value::Null;
            snapshot["activeTurn"] = Value::Null;
            snapshot["pendingPermissions"] = json!([]);
            runtime::transition_attention(None, snapshot);
            let _ = self.events.send(json!({"type":"agent_state","agent":snapshot}));
        }
        if let Some(session) = session {
            if let Err(close_error) = session.close().await {
                tracing::error!(agent_id=%id,error=%close_error,"Failed to close failed event owner session");
                // The event consumer has ended, but failed process cleanup still
                // belongs to this cell and must block replacement until retried.
                cell.data.lock().await.session = Some(session);
            }
        }
    }
    /// Non-activity writes preserve both durable activity stamps and the live
    /// snapshot, even when an older record uses different values for the two.
    async fn persist_snapshot(&self, snapshot: &mut Value, record_activity: bool) -> Result<()> {
        object(snapshot).remove("snapshotAvailability");
        if record_activity {
            snapshot["lastActivityAt"] = snapshot["updatedAt"].clone();
        } else if snapshot["internal"] != true {
            if let Some(previous) = self.storage()?.get(required(snapshot, "id")?).await? {
                for field in ["updatedAt", "lastActivityAt"] {
                    match previous.get(field) {
                        Some(value) => { snapshot[field] = value.clone(); }
                        None => { object(snapshot).remove(field); }
                    }
                }
            }
        }
        if snapshot["internal"] == true {
            return Ok(());
        }
        let mut record = snapshot.clone();
        let fields = record
            .as_object_mut()
            .ok_or_else(|| anyhow!("Invalid agent snapshot"))?;
        for field in [
            "_live",
            "_providerAvailable",
            "_historyOnly",
            "snapshotAvailability",
            "preparation",
            "status",
            "lifecycle",
            "session",
            "pendingPermissions",
            "availableModes",
            "capabilities",
            "activeTurn",
            "activeTurnId",
            "activeForegroundTurnId",
            "attention",
            "model",
            "thinkingOptionId",
            "effectiveThinkingOptionId",
        ] {
            fields.remove(field);
        }
        record["lastStatus"] = snapshot["lifecycle"].clone();
        record["lastModeId"] = snapshot
            .get("currentModeId")
            .filter(|mode| mode.is_string())
            .or_else(|| snapshot["config"].get("modeId").filter(|mode| mode.is_string()))
            .cloned()
            .unwrap_or(Value::Null);
        if snapshot["requiresAttention"] != true {
            record["attentionReason"] = Value::Null;
            record["attentionTimestamp"] = Value::Null;
        }
        if let Some(config) = record.get_mut("config") {
            object(config).remove("daemonAppendSystemPrompt");
            strip_config_internal_servers(config);
        }
        if let Some(metadata) = record
            .get_mut("persistence")
            .and_then(|handle| handle.get_mut("metadata"))
            .filter(|value| value.is_object())
        {
            object(metadata).remove("daemonAppendSystemPrompt");
            strip_config_internal_servers(metadata);
        }
        self.storage()?.upsert(record).await
    }
    async fn commit_snapshot(&self, cell: &Arc<AgentCell>, snapshot: Value, touch_activity: bool) -> Result<()> {
        let _commit = cell.commit.lock().await;
        self.commit_snapshot_locked(cell, snapshot, touch_activity).await
    }
    /// Caller owns cell.commit; never hold cell.data across persistence.
    async fn commit_snapshot_locked(&self, cell: &Arc<AgentCell>, mut snapshot: Value, touch_activity: bool) -> Result<()> {
        if snapshot["lifecycle"] != "closed" {
            let data = cell.data.lock().await;
            if let Some(error) = data.event_error.as_deref() {
                bail!("Agent event persistence failed: {error}");
            }
            if data.owner_ended && is_busy(&snapshot) {
                bail!("Agent provider session has ended");
            }
        }
        if touch_activity {
            snapshot["updatedAt"] = json!(next_updated_at(&snapshot["updatedAt"]));
        }
        snapshot["status"] = snapshot["lifecycle"].clone();
        let ready = cell.data.lock().await.ready;
        if ready {
            self.persist_snapshot(&mut snapshot, touch_activity).await?;
        }
        {
            let mut data = cell.data.lock().await;
            if snapshot["lifecycle"] == "closed" {
                data.owner_ended = true;
                data.pending_replacement = false;
                data.session = None;
                cell.owner_ended.cancel();
            }
            data.snapshot = snapshot.clone();
        }
        if ready {
            let _ = self
                .events
                .send(json!({"type":"agent_state","agent":snapshot}));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authoritative_reads_wait_for_live_state_not_persisted_observations() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home) }).await;
        let id = "00000000-0000-4000-8000-000000000041";
        let stored = json!({
            "id":id,"provider":"mock","cwd":root.path(),"config":{},"internal":false,
            "createdAt":"2026-01-01T00:00:00.000Z",
            "lastStatus":"idle","updatedAt":"2026-01-01T00:00:00.000Z",
            "owner":{"kind":"daemon","daemonId":"daemon","executionId":"old"}
        });
        manager.storage().unwrap().upsert(stored.clone()).await.unwrap();
        let (cell, _receiver) = manager.make_cell(id, stored, true).await.unwrap();
        let mut data = cell.data.lock().await;
        data.ready = true;
        data.snapshot["owner"]["executionId"] = json!("current");
        let read = manager.agent_snapshot(id);
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
        let observed = manager.observe_agent_snapshot(id).await.unwrap().unwrap();
        assert_eq!(observed["snapshotAvailability"], "last_persisted");
        assert_eq!(observed["owner"]["executionId"], "old");
        drop(data);
        let authoritative = read.await.unwrap().unwrap();
        assert_eq!(authoritative["owner"]["executionId"], "current");
        assert!(authoritative.get("snapshotAvailability").is_none());
        assert!(manager.authorize(&json!({"kind":"daemon","daemonId":"daemon","executionId":"old"}), id).await.is_err());
        manager.authorize(&json!({"kind":"daemon","daemonId":"daemon","executionId":"current"}), id).await.unwrap();
    }

    #[tokio::test]
    async fn contended_internal_agent_is_not_misreported_as_missing() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home) }).await;
        let id = "00000000-0000-4000-8000-000000000042";
        let snapshot = json!({"id":id,"provider":"mock","internal":true,"config":{},"cwd":root.path()});
        let (cell, _receiver) = manager.make_cell(id, snapshot, true).await.unwrap();
        let mut data = cell.data.lock().await;
        data.ready = true;
        data.snapshot["lifecycle"] = json!("running");
        let read = manager.agent_snapshot(id);
        tokio::pin!(read);
        assert!(futures_util::poll!(&mut read).is_pending());
        assert!(manager.observe_agent_snapshot(id).await.is_err());
        drop(data);
        let snapshot = read.await.unwrap().unwrap();
        assert_eq!(snapshot["id"], id);
        assert_eq!(snapshot["lifecycle"], "running");
        assert!(manager.storage().unwrap().get(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn transient_snapshot_markers_are_removed_without_touching_activity() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home.clone()) }).await;
        let id = "00000000-0000-4000-8000-000000000043";
        let original = json!({
            "id":id,"provider":"mock","cwd":root.path(),"config":{},"lastStatus":"idle",
            "createdAt":"2026-01-01T00:00:00.000Z",
            "updatedAt":"2026-01-02T00:00:00.000Z","lastActivityAt":"2026-01-01T00:00:00.000Z",
            "snapshotAvailability":"last_persisted"
        });
        manager.storage().unwrap().upsert(original.clone()).await.unwrap();
        manager.restore_persisted_state().await.unwrap();
        let record = manager.storage().unwrap().get(id).await.unwrap().unwrap();
        assert!(record.get("snapshotAvailability").is_none());
        assert_eq!(record["updatedAt"], original["updatedAt"]);
        assert_eq!(record["lastActivityAt"], original["lastActivityAt"]);
        let (cell, _receiver) = manager.make_cell(id, original.clone(), true).await.unwrap();
        assert!(cell.data.lock().await.snapshot.get("snapshotAvailability").is_none());
        let mut snapshot = cell.data.lock().await.snapshot.clone();
        snapshot["snapshotAvailability"] = json!("last_persisted");
        snapshot["historySync"] = json!({"status":"complete"});
        manager.persist_snapshot(&mut snapshot, false).await.unwrap();
        assert!(snapshot.get("snapshotAvailability").is_none());
        let reopened = AgentStorage::new(home).get(id).await.unwrap().unwrap();
        assert!(reopened.get("snapshotAvailability").is_none());
        assert_eq!(reopened["historySync"]["status"], "complete");
        assert_eq!(reopened["updatedAt"], original["updatedAt"]);
        assert_eq!(reopened["lastActivityAt"], original["lastActivityAt"]);
    }

    #[tokio::test]
    async fn restart_closes_interrupted_records_without_rewriting_user_data() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let storage = AgentStorage::new(home.clone());
        let mut originals = Vec::new();
        for status in ["running", "initializing", "idle", "error", "closed"] {
            let record = json!({
                "id":format!("restart-{status}"),"provider":"omp","cwd":root.path(),
                "config":{"model":"test/model"},"lastStatus":status,
                "createdAt":"2026-10-24T00:00:00.000Z","updatedAt":"2026-10-31T00:00:00.000Z",
                "lastActivityAt":"2026-10-31T00:00:01.000Z","lastTurnOutcome":"turn_completed",
                "title":"Keep this title","labels":{"custom":"value"},"workspaceId":"workspace",
                "requiresAttention":true,"attentionReason":"finished",
                "attentionTimestamp":"2026-10-31T00:00:01.000Z",
                "persistence":{"provider":"omp","sessionId":"native","nativeHandle":"private.jsonl"}
            });
            storage.upsert(record.clone()).await.unwrap();
            originals.push(record);
        }
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home.clone()) }).await;
        manager.restore_persisted_state().await.unwrap();
        let reopened = AgentStorage::new(home.clone());
        for mut expected in originals {
            let id = expected["id"].as_str().unwrap().to_owned();
            if matches!(expected["lastStatus"].as_str(), Some("running" | "initializing")) {
                expected["lastStatus"] = json!("closed");
                expected["lastTurnOutcome"] = json!("turn_canceled");
            }
            assert_eq!(reopened.get(&id).await.unwrap().unwrap(), expected);
            let snapshot = manager.agent_snapshot(&id).await.unwrap().unwrap();
            assert!(!is_busy(&snapshot));
            assert_eq!(snapshot["_live"], false);
            assert!(snapshot["activeTurn"].is_null());
            assert!(snapshot["activeTurnId"].is_null());
            assert!(snapshot["activeForegroundTurnId"].is_null());
        }
        assert!(manager.agents.read().await.is_empty());
        manager.restore_persisted_state().await.unwrap();
        assert_eq!(reopened.list().await.unwrap(), manager.storage().unwrap().list().await.unwrap());
    }

    #[tokio::test]
    async fn unavailable_subagent_catalog_is_reported_and_scoped_to_its_parent() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let storage = AgentStorage::new(home.clone());
        let subagents = home.join("agent-subagents");
        let mut store = SubagentStore::default();
        for id in ["healthy", "corrupt"] {
            storage.upsert(json!({
                "id":id,"provider":"omp","cwd":root.path(),"config":{"model":"test/model"},
                "lastStatus":"closed","createdAt":"2026-10-24T00:00:00.000Z","updatedAt":"2026-10-31T00:00:00.000Z",
                "archivedAt":"2026-10-31T00:00:00.000Z"
            })).await.unwrap();
            store.apply_persisted(&subagents, id, "omp",
                &json!({"type":"upsert","id":"child","timestamp":"2026-10-31T00:00:00.000Z"})).unwrap();
        }
        drop(store);
        let catalog = subagents.join("corrupt").join("index.sqlite3");
        std::fs::write(&catalog, b"not a sqlite database").unwrap();
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home.clone()) }).await;
        manager.restore_persisted_state().await.unwrap();
        let (activity, unavailable) = manager.list_provider_subagent_activity().await.unwrap();
        assert_eq!(activity.len(), 1);
        assert_eq!(activity[0]["parentAgentId"], "healthy");
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].parent, "corrupt");
        assert_eq!(unavailable[0].child, None);
        assert_eq!(std::fs::read(&catalog).unwrap(), b"not a sqlite database");
    }

    #[test]
    fn internal_runtime_server_is_stripped_from_config_servers() {
        let internal = json!({"type":"http","url":"http://127.0.0.1:1/mcp/agents?callerAgentId=a","headers":{"Authorization":"Bearer t"}});
        let mut only = json!({"provider":"codex","mcpServers":{"paseo":internal}});
        strip_config_internal_servers(&mut only);
        assert!(only.get("mcpServers").is_none());
        let mut mixed = json!({"mcpServers":{"paseo":internal,"other":{"type":"stdio","command":"x"}}});
        strip_config_internal_servers(&mut mixed);
        assert_eq!(mixed["mcpServers"], json!({"other":{"type":"stdio","command":"x"}}));
        let mut empty = json!({"mcpServers":{}});
        strip_config_internal_servers(&mut empty);
        assert_eq!(empty["mcpServers"], json!({}));
    }

    #[test]
    fn updated_at_strictly_advances_past_previous_stamp() {
        let future = "2999-01-01T00:00:00.000Z";
        assert_eq!(next_updated_at(&json!(future)), "2999-01-01T00:00:00.001Z");
        let before = chrono::Utc::now().timestamp_millis();
        let fresh = next_updated_at(&json!("2001-01-01T00:00:00.000Z"));
        let parsed = chrono::DateTime::parse_from_rfc3339(&fresh)
            .unwrap()
            .timestamp_millis();
        assert!(parsed >= before);
        assert!(chrono::DateTime::parse_from_rfc3339(&next_updated_at(&Value::Null)).is_ok());
    }

    #[test]
    fn stored_updated_at_uses_latest_of_updated_and_activity() {
        let record = json!({"updatedAt":"2026-01-01T00:00:00.000Z","lastActivityAt":"2026-02-01T00:00:00.000Z"});
        assert_eq!(
            stored_updated_at(&record).as_deref(),
            Some("2026-02-01T00:00:00.000Z")
        );
        let renamed = json!({"updatedAt":"2026-03-01T00:00:00.000Z","lastActivityAt":"2026-02-01T00:00:00.000Z"});
        assert_eq!(
            stored_updated_at(&renamed).as_deref(),
            Some("2026-03-01T00:00:00.000Z")
        );
        assert_eq!(stored_updated_at(&json!({"updatedAt":"bad"})), None);
    }
}
