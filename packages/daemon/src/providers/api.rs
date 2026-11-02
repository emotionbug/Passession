use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Resolved executable prefix. Adapters append only their native protocol flags.
/// Secrets intentionally do not implement Debug or Serialize.
#[derive(Clone)]
pub struct ProviderSpec {
    pub id: String,
    pub family: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub options: Value,
    pub defaults: Value,
    pub runtime_settings: Value,
    pub configured_models: Vec<Value>,
}

/// Per-agent authority and configuration, never persisted as provider options.
#[derive(Clone)]
pub struct LaunchContext {
    pub agent_id: String,
    pub home: PathBuf,
    pub executable: PathBuf,
    pub env: BTreeMap<String, String>,
    pub mcp_servers: Value,
    pub tool_catalog: Value,
    pub tool_manifest: Value,
    pub tool_executor: Option<Arc<dyn NativeToolExecutor>>,
    pub cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct NativeToolInputError {
    pub status: u16,
    pub issues: Option<Value>,
    pub message: String,
}

/// Bound to one agent's authority; callers cannot supply a replacement identity.
#[async_trait]
pub trait NativeToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, input: Value, cancel: CancellationToken) -> Result<Value> {
        self.execute_streaming(name, input, cancel, None).await
    }

    async fn execute_streaming(
        &self,
        name: &str,
        input: Value,
        cancel: CancellationToken,
        updates: Option<mpsc::Sender<Value>>,
    ) -> Result<Value>;
}

/// One bounded consumer owns event persistence before outward publication.
/// The manager drains this receiver before creating or resuming a provider.
#[derive(Clone)]
pub struct EventSink {
    sender: mpsc::Sender<Value>,
    cancel: CancellationToken,
}

impl EventSink {
    pub fn channel(capacity: usize, cancel: CancellationToken) -> (Self, mpsc::Receiver<Value>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender, cancel }, receiver)
    }

    pub async fn emit(&self, event: Value) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(anyhow!("provider event owner closed")),
            result = self.sender.send(event) => result.map_err(|_| anyhow!("provider event consumer closed")),
        }
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Definite prompt delivery evidence raised synchronously by a provider or the
/// manager. `rejected` means the provider explicitly refused the prompt; false
/// means it was proven never to reach the provider. Ambiguous failures must not
/// use this type.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PromptDeliveryError {
    pub rejected: bool,
    pub message: String,
}
impl PromptDeliveryError {
    pub fn not_delivered(message: impl Into<String>) -> anyhow::Error {
        Self { rejected: false, message: message.into() }.into()
    }
    pub fn rejected(message: impl Into<String>) -> anyhow::Error {
        Self { rejected: true, message: message.into() }.into()
    }
}

/// A native acknowledgement correlated to one provider prompt request.
#[derive(Clone, Debug)]
pub enum PromptAck {
    /// Native evidence, for example the provider request ID.
    Accepted(Value),
    Rejected(String),
    /// The request was proven never written to the provider.
    NotDelivered(String),
    /// The request may have reached the provider without an acknowledgement.
    Unknown(String),
}
pub type PromptAckFuture = futures_util::future::BoxFuture<'static, PromptAck>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RewindTarget {
    Conversation,
    Files,
    Both,
}

pub struct ImportedSession {
    pub session: Arc<dyn AgentSession>,
    pub config: Value,
    pub persistence: Value,
    pub timeline: Vec<Value>,
    pub provider_subagent_events: Vec<Value>,
}

/// A successful replay may still be a drained cache or bounded context window.
/// Only a complete source snapshot can authorize explicit history truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryCoverage {
    Complete,
    #[default]
    Unverified,
}

/// One child transcript the user explicitly opened. Parent history replay and live
/// events announce children with descriptor-only `provider_subagent` upserts; a
/// child's native body is read only through `AgentProvider::stream_subagent_history`.
pub struct SubagentHistoryRequest {
    /// The parent's stored persistence handle (`describe_persistence` shape).
    pub persistence: Value,
    /// The parent's stored config (cwd, provider options).
    pub config: Value,
    pub child_id: String,
    /// The provider-owned `source` locator from the child's latest upsert. Null for
    /// descriptors stored before providers announced one.
    pub source: Value,
    /// The child's last recorded descriptor status. A child not recorded as
    /// finished may still be growing while its body is read, so its replay is
    /// never authoritative.
    pub status: Value,
    /// The in-stream replay transaction for a parent that publishes live rows.
    pub replay: SubagentReplay,
}

/// What the daemon did with an in-stream child replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayOutcome {
    Applied,
    Retained,
}

/// Two-phase replay of an opened child whose provider publishes live rows for it.
/// The provider publishes `marker(...)` on the parent's live sink, in order with its live
/// rows, holds that child's further live rows, awaits `outcome()`, then continues:
/// `Applied` → rebase its live state to the snapshot; `Retained` (or `None`) → keep its
/// previous live state. Held rows are computed against that state and published after.
#[derive(Clone)]
pub struct SubagentReplay {
    key: String,
    outcome: tokio::sync::watch::Receiver<Option<ReplayOutcome>>,
}
impl SubagentReplay {
    pub fn new(key: String, outcome: tokio::sync::watch::Receiver<Option<ReplayOutcome>>) -> Self {
        Self { key, outcome }
    }
    /// `events`: the replay's `subagent_row` envelopes and grandchild upsert envelopes.
    pub fn marker(
        &self,
        provider: &str,
        child_id: &str,
        events: Vec<Value>,
        coverage: HistoryCoverage,
    ) -> Value {
        serde_json::json!({"type":"provider_subagent","provider":provider,"event":{"type":"replay","id":child_id,
            "key":self.key,"historyComplete":coverage == HistoryCoverage::Complete,"events":events}})
    }
    /// `None` when the parent's event owner closed before applying it.
    pub async fn outcome(&self) -> Option<ReplayOutcome> {
        let mut outcome = self.outcome.clone();
        outcome.wait_for(Option::is_some).await.ok().and_then(|value| *value)
    }
}

/// Holds an opened child's live rows while its in-stream replay is decided. A
/// session publishes every outgoing event through `emit`; `publish_replay` runs
/// the two-phase transaction on that session's own live sink. One lane orders
/// both, so no live row is ever half-published around the hold or the marker.
/// Only the opening task waits for the daemon; the session keeps streaming,
/// with that child's rows held.
#[derive(Default)]
pub struct ChildRowHold {
    held: std::sync::Mutex<std::collections::HashMap<String, Vec<Value>>>,
    lane: tokio::sync::Mutex<()>,
}
impl ChildRowHold {
    /// Publishes a live event, or holds it when it is a row of a child whose
    /// replay is being decided.
    pub async fn emit(&self, sink: &EventSink, event: Value) -> Result<()> {
        let _lane = self.lane.lock().await;
        let id = (event["type"] == "provider_subagent")
            .then(|| event["event"]["id"].as_str().map(str::to_owned))
            .flatten();
        if let Some(id) = id {
            let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(rows) = held.get_mut(&id) {
                rows.push(event);
                return Ok(());
            }
        }
        sink.emit(event).await
    }
    /// Holds the child's live rows before `read` takes its snapshot, publishes
    /// the snapshot as `request.replay.marker` behind every row already
    /// published, waits for the daemon's decision, then publishes the held rows
    /// in order ahead of any later one. When the snapshot was applied, the live
    /// state is rebased onto it: a held row that `covered` says the snapshot
    /// already contains (by the provider's native record identity) is dropped.
    /// Otherwise every held row continues the state the daemon kept.
    pub async fn publish_replay(
        &self,
        request: &SubagentHistoryRequest,
        provider: &str,
        sink: &EventSink,
        read: impl Future<Output = Result<(Vec<Value>, HistoryCoverage)>>,
        covered: impl Fn(&Value, &[Value]) -> bool,
    ) -> Result<SubagentHistory> {
        let child = request.child_id.as_str();
        {
            let _lane = self.lane.lock().await;
            let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
            if held.contains_key(child) {
                return Err(anyhow!("Subagent '{child}' is already being replayed"));
            }
            held.insert(child.to_owned(), Vec::new());
        }
        let decided = async {
            let (mut events, read) = read.await?;
            let coverage = request.settle(&mut events, read);
            let snapshot: Vec<Value> = events
                .iter()
                .filter(|e| e["event"]["type"] == "timeline" && e["event"]["id"] == child)
                .map(|e| e["event"]["item"].clone())
                .collect();
            {
                let _lane = self.lane.lock().await;
                sink.emit(request.replay.marker(provider, child, events, coverage)).await?;
            }
            Ok::<_, anyhow::Error>((request.replay.outcome().await, snapshot))
        }
        .await;
        let _lane = self.lane.lock().await;
        let mut rows = self
            .held
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(child)
            .unwrap_or_default();
        if let Ok((Some(ReplayOutcome::Applied), snapshot)) = &decided {
            rows.retain(|row| {
                row["event"]["type"] != "timeline" || !covered(&row["event"]["item"], snapshot)
            });
        }
        for row in rows {
            sink.emit(row).await?;
        }
        decided?;
        Ok(SubagentHistory::Published)
    }
}

impl SubagentHistoryRequest {
    /// The coverage a replay may claim: a source read completely, of a child
    /// recorded as finished. Rows are marked to agree.
    pub fn settle(&self, events: &mut [Value], read: HistoryCoverage) -> HistoryCoverage {
        let finished = matches!(self.status.as_str(), Some("completed" | "failed" | "canceled"));
        let coverage = if finished { read } else { HistoryCoverage::Unverified };
        for event in events.iter_mut() {
            if event["event"]["type"] == "timeline" && event["event"]["id"] == self.child_id.as_str() {
                event["event"]["historyComplete"] =
                    Value::Bool(coverage == HistoryCoverage::Complete);
            }
        }
        coverage
    }
}

pub enum SubagentHistory {
    /// The child's native transcript was replayed into the sink as `provider_subagent`
    /// timeline rows for `child_id`, each carrying `historyComplete`, plus
    /// descriptor-only upserts for the child's own children.
    Replayed(HistoryCoverage),
    /// No separately readable child body exists: rows collected from the parent's
    /// native stream and history are the authority.
    Collected,
    /// The replay was published in-stream with `request.replay.marker`; no sink rows.
    Published,
}

/// One replayed child row in the envelope `stream_subagent_history` promises.
pub fn subagent_row(
    provider: &str,
    child_id: &str,
    item: Value,
    timestamp: Option<&Value>,
    coverage: HistoryCoverage,
) -> Value {
    let mut event = serde_json::json!({"type":"timeline","id":child_id,"item":item,
        "historyComplete":coverage == HistoryCoverage::Complete});
    if let Some(timestamp) = timestamp.filter(|timestamp| !timestamp.is_null()) {
        event["timestamp"] = timestamp.clone();
    }
    serde_json::json!({"type":"provider_subagent","provider":provider,"event":event})
}

/// Values retain the existing agent-sdk-types.ts shapes, including opaque native
/// persistence handles and provider-specific metadata. Unsupported operations
/// must agree with the adapter's actual advertised capabilities.
#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &str;
    fn native_id(&self) -> Option<String>;
    fn capabilities(&self) -> &Value;
    fn features(&self) -> Arc<[Value]>;
    fn initial_timeline(&self) -> &[Value];
    fn describe_persistence(&self) -> Option<Value>;
    fn pending_permissions(&self) -> Vec<Value>;
    fn usage_session(&self) -> Option<Value>;

    async fn start_turn(&self, prompt: Value, options: Value) -> Result<Value>;
    async fn steer_active_turn(&self, prompt: Value, options: Value) -> Result<Value>;
    async fn stream_history(&self, sink: EventSink) -> Result<HistoryCoverage>;
    async fn runtime_info(&self) -> Result<Value>;
    async fn available_modes(&self) -> Result<Vec<Value>>;
    async fn current_mode(&self) -> Result<Option<String>>;
    async fn set_mode(&self, mode: &str) -> Result<Option<Value>>;
    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<Option<Value>>;
    async fn interrupt(&self) -> Result<()>;
    async fn close(&self) -> Result<()>;
    async fn list_commands(&self) -> Result<Vec<Value>>;
    async fn set_model(&self, model: Option<&str>) -> Result<()>;
    async fn set_thinking_option(&self, option: Option<&str>) -> Result<Option<Value>>;
    async fn set_feature(&self, feature: &str, value: Value) -> Result<()>;
    async fn rewind(&self, target: RewindTarget, message_id: &str) -> Result<()>;
    /// Recognize without requests or events. The manager publishes the accepted
    /// prompt before polling this owned operation outside its admission barrier.
    /// Execution failures become command-error stream events, not rejected sends.
    fn prepare_out_of_band(
        self: Arc<Self>,
        _prompt: &Value,
    ) -> Option<futures_util::future::BoxFuture<'static, Result<()>>> {
        None
    }
    /// Some providers return a turn ID before their native prompt request is
    /// acknowledged. They return that turn's correlated acknowledgement here,
    /// once; None means no separate native acknowledgement is observable.
    fn native_acknowledgement(&self, _turn_id: &str) -> Option<PromptAckFuture> {
        None
    }
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &Value;
    /// Whether the provider has paired native archive/restore operations.
    /// Paseo's own archive metadata remains supported for every provider.
    fn supports_native_archive(&self) -> bool { false }
    /// None delegates to the registry's generic creation policy, matching the
    /// original optional provider hook rather than overriding it with defaults.
    fn resolve_create_config(&self, _input: &Value) -> Option<Result<Value>> {
        None
    }
    fn is_create_config_unattended(&self, _input: &Value) -> Option<bool> {
        None
    }
    async fn create_session(
        &self,
        config: Value,
        context: LaunchContext,
        options: Value,
        sink: EventSink,
    ) -> Result<Arc<dyn AgentSession>>;
    async fn resume_session(
        &self,
        persistence: Value,
        config: Value,
        context: LaunchContext,
        options: Value,
        sink: EventSink,
    ) -> Result<Arc<dyn AgentSession>>;
    /// Reads the native body of one announced child, only when the user opens it.
    /// Works for a cold stored parent: no parent session is resumed or replayed.
    /// Errors are truthful: a missing or unreadable body is never an empty replay.
    async fn stream_subagent_history(
        &self,
        _request: SubagentHistoryRequest,
        _context: LaunchContext,
        _sink: EventSink,
    ) -> Result<SubagentHistory> {
        Ok(SubagentHistory::Collected)
    }
    async fn fetch_catalog(&self, options: Value, cancel: CancellationToken) -> Result<Value>;
    /// None preserves target-scoped discovery; Some shares only this provider's
    /// catalogs under its source-owned identity key.
    async fn catalog_cache_key(
        &self,
        _options: Value,
        _cancel: CancellationToken,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    fn resolve_configured_model(&self, model: Value) -> Result<Value> {
        Ok(model)
    }
    async fn resolve_default_mode(
        &self,
        _input: Value,
        _cancel: CancellationToken,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    async fn list_features(&self, config: &Value) -> Result<Vec<Value>>;
    async fn list_commands(&self, config: &Value) -> Result<Vec<Value>>;
    async fn list_importable_sessions(&self, options: Value) -> Result<Vec<Value>>;
    async fn import_session(
        &self,
        input: Value,
        config: Value,
        stored_config: Value,
        context: LaunchContext,
        sink: EventSink,
    ) -> Result<ImportedSession>;
    async fn is_available(&self, options: Value, cancel: CancellationToken) -> Result<bool>;
    async fn diagnostic(&self) -> Result<String>;
    async fn archive_native_session(&self, persistence: &Value) -> Result<()>;
    async fn unarchive_native_session(&self, persistence: &Value) -> Result<()>;
    async fn shutdown(&self) -> Result<()>;
}
