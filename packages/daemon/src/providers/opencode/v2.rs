mod notices;
mod permissions;
mod session;
mod timeline;
#[cfg(test)]
mod tests;

use super::{
    catalog,
    family::LiveFamily,
    reader::NativeReader,
    runtime::{Runtime, RuntimeManager},
};
use crate::providers::api::{
    AgentSession, EventSink, HistoryCoverage, ProviderSpec, RewindTarget,
    SubagentHistoryRequest, subagent_row,
};
use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use session::Session;
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use timeline::{Timeline, timestamp};

pub(super) fn s<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
pub(super) fn array(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
pub(super) fn component(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}
pub(super) fn session_path(id: &str, suffix: &str) -> String {
    format!("/api/session/{}{suffix}", component(id))
}
pub(super) fn usage(info: &Value) -> Value {
    json!({"inputTokens":info["tokens"]["input"],"outputTokens":info["tokens"]["output"],"cachedInputTokens":info["tokens"]["cache"]["read"],"totalCostUsd":info["cost"]})
}
/// Every message of one session, oldest first, across all pages.
pub(super) async fn messages(reader: &dyn NativeReader, id: &str) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let mut query = vec![("limit".to_owned(), "100".to_owned())];
        if let Some(cursor) = &cursor {
            query.push(("cursor".to_owned(), cursor.clone()));
        } else {
            query.push(("order".to_owned(), "asc".to_owned()));
        }
        let page = reader.get(&session_path(id, "/message"), &query).await?;
        let data = page["data"]
            .as_array()
            .ok_or_else(|| anyhow!("OpenCode session {id} messages are malformed"))?;
        result.extend(data.iter().cloned());
        cursor = page["cursor"]["next"].as_str().map(str::to_owned);
        let Some(next) = &cursor else {
            break;
        };
        if !seen.insert(next.clone()) {
            bail!("OpenCode repeated a message pagination cursor");
        }
    }
    Ok(result)
}
/// Direct children of one session across all `parentID` pages.
async fn children(reader: &dyn NativeReader, parent: &str) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    loop {
        let mut query = vec![("limit".to_owned(), "100".to_owned())];
        if let Some(cursor) = &cursor {
            query.push(("cursor".to_owned(), cursor.clone()));
        } else {
            query.push(("parentID".to_owned(), parent.to_owned()));
        }
        let page = reader.get("/api/session", &query).await?;
        result.extend(array(&page["data"]).iter().cloned());
        cursor = page["cursor"]["next"].as_str().map(str::to_owned);
        let Some(next) = &cursor else {
            break;
        };
        if !seen.insert(next.clone()) {
            bail!("OpenCode repeated a child pagination cursor");
        }
    }
    Ok(result)
}
/// Every native descendant of `parent`, depth first. Session metadata only: a
/// child's messages are read live while it runs or when it is opened.
pub(super) async fn descendants(reader: &dyn NativeReader, parent: &str) -> Result<Vec<Value>> {
    let mut parents = vec![parent.to_owned()];
    let mut visited = HashSet::new();
    let mut found = Vec::new();
    while let Some(parent) = parents.pop() {
        if !visited.insert(parent.clone()) {
            continue;
        }
        for child in children(reader, &parent).await? {
            parents.push(s(&child, "id").to_owned());
            found.push(child);
        }
    }
    Ok(found)
}
/// The native runner map: a session is running while its entry is set.
pub(super) async fn active_sessions(reader: &dyn NativeReader) -> Result<Value> {
    Ok(reader.get("/api/session/active", &[]).await?["data"].take())
}
fn running(active: &Value, id: &str) -> bool {
    active.get(id).is_some_and(|v| !v.is_null() && v != false)
}
/// The descriptor-only upsert for one native descendant of `root`, carrying the
/// opaque `source` the opened-child replay reads.
pub(super) fn descriptor(info: &Value, root: &str, active: &Value) -> Value {
    let id = s(info, "id");
    let status = if running(active, id) {
        "running"
    } else {
        match s(info, "outcome") {
            "failed" => "failed",
            "interrupted" => "canceled",
            _ => "completed",
        }
    };
    let parent = if s(info, "parentID") == root {
        Value::Null
    } else {
        info["parentID"].clone()
    };
    json!({"type":"upsert","id":id,"parentSubagentId":parent,"title":info["title"],"status":status,
        "cwd":info["location"]["directory"],"timestamp":timestamp(&info["time"]["updated"]),
        "source":{"sessionId":id,"directory":info["location"]["directory"]}})
}
async fn session_info(reader: &dyn NativeReader, id: &str) -> Result<Value> {
    let info = reader.get(&session_path(id, ""), &[]).await?["data"].take();
    if s(&info, "id") != id {
        bail!("OpenCode session {id} was not found");
    }
    Ok(info)
}
/// Replays one opened child: descriptor-only upserts for the sessions it spawned,
/// then its own transcript as `provider_subagent` rows. Reads no sibling. A child
/// still running keeps collecting live, so its replay is `Unverified`. Every read
/// completes before any row exists.
pub(super) async fn replay_child(
    reader: &dyn NativeReader,
    provider: &str,
    parent: &str,
    child: &str,
) -> Result<(Vec<Value>, HistoryCoverage)> {
    let (events, coverage, _) = read_child(reader, provider, parent, child).await?;
    Ok((events, coverage))
}
/// `replay_child` plus the native messages the live timeline rebases onto.
async fn read_child(
    reader: &dyn NativeReader,
    provider: &str,
    parent: &str,
    child: &str,
) -> Result<(Vec<Value>, HistoryCoverage, Vec<Value>)> {
    let info = session_info(reader, child).await?;
    let mut ancestor = s(&info, "parentID").to_owned();
    let mut depth = 0;
    while ancestor != parent {
        depth += 1;
        if ancestor.is_empty() || depth > 100 {
            bail!("OpenCode session {child} is not a subagent of {parent}");
        }
        ancestor = s(&session_info(reader, &ancestor).await?, "parentID").to_owned();
    }
    let history = messages(reader, child).await?;
    let spawned = children(reader, child).await?;
    let active = active_sessions(reader).await?;
    let coverage = if running(&active, child) {
        HistoryCoverage::Unverified
    } else {
        HistoryCoverage::Complete
    };
    let mut events: Vec<Value> = spawned
        .iter()
        .map(|info| {
            json!({"type":"provider_subagent","provider":provider,"event":descriptor(info, parent, &active)})
        })
        .collect();
    for event in Timeline::default().messages(&history, false)? {
        events.push(subagent_row(
            provider,
            child,
            event["item"].clone(),
            event.get("timestamp"),
            coverage,
        ));
    }
    Ok((events, coverage, history))
}
#[derive(Clone)]
struct Snapshot {
    info: Value,
    config: Value,
    pending: Vec<Value>,
    modes: Vec<Value>,
    features: Arc<[Value]>,
}
struct Call {
    operation: Operation,
    reply: oneshot::Sender<Result<Value>>,
}
enum Operation {
    Start(Value, Value),
    Steer(Value, Value),
    History(EventSink),
    RuntimeInfo,
    Mode(String),
    Permission(String, Value),
    Interrupt,
    Commands,
    Model(Option<String>),
    Thinking(Option<String>),
    Feature(String, Value),
    Rewind(String),
    /// An opened child's in-stream replay: the actor publishes its marker, holds
    /// only that child's re-reads, and replies; a watcher posts the outcome back.
    ReplayChild(String, Box<SubagentHistoryRequest>),
}
struct V2Session {
    provider: String,
    id: String,
    capabilities: Value,
    initial: Vec<Value>,
    snapshot: Arc<Mutex<Snapshot>>,
    sender: mpsc::Sender<Call>,
    cancel: CancellationToken,
    closed: CancellationToken,
    close_error: Arc<Mutex<Option<String>>>,
}
impl V2Session {
    async fn call(&self, operation: Operation) -> Result<Value> {
        let (reply, receiver) = oneshot::channel();
        tokio::select! { _=self.cancel.cancelled()=>bail!("OpenCode session is closed"), result=self.sender.send(Call{operation,reply})=>result.map_err(|_|anyhow!("OpenCode session is closed"))? }
        receiver
            .await
            .map_err(|_| anyhow!("OpenCode session owner closed"))?
    }
}
impl Drop for V2Session {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
pub(super) async fn attach(
    runtime: Arc<Runtime>,
    manager: Arc<RuntimeManager>,
    spec: ProviderSpec,
    launch: super::SessionLaunch,
) -> Result<Arc<dyn AgentSession>> {
    let super::SessionLaunch { info, mut config, context, options, sink } = launch;
    let id = s(&info, "id").to_owned();
    if id.is_empty() {
        bail!("OpenCode session response omitted its id");
    }
    let initial = notices::initialize(&mut config)?;
    let snapshot = Arc::new(Mutex::new(Snapshot {
        info: info.clone(),
        config: config.clone(),
        pending: Vec::new(),
        modes: Vec::new(),
        features: catalog::features_v2(&config).into(),
    }));
    let (sender, receiver) = mpsc::channel(64);
    let cancel = context.cancel.child_token();
    let closed = CancellationToken::new();
    let close_error = Arc::new(Mutex::new(None));
    let facade = Arc::new(V2Session {
        provider: spec.id.clone(),
        id: id.clone(),
        capabilities: json!({"supportsStreaming":true,"supportsSessionPersistence":true,"supportsSessionListing":true,"supportsDynamicModes":true,"supportsMcpServers":true,"supportsReasoningStream":true,"supportsToolInvocations":true,"supportsRewindBoth":true,"supportsRewindConversation":false,"supportsRewindFiles":false}),
        initial,
        snapshot,
        sender,
        cancel: cancel.clone(),
        closed,
        close_error,
    });
    // Registered before the actor starts publishing: a child opened while the
    // session initializes queues its replay behind initialization, in-stream.
    let family: Arc<dyn LiveFamily> = facade.clone();
    manager
        .families
        .register(&facade.id, Arc::downgrade(&family));
    drop(family);
    let actor = Session::new(
        runtime,
        manager,
        id,
        super::SessionLaunch { info, config, context, options, sink },
        &facade,
    );
    let (ready, initialized) = oneshot::channel();
    tokio::spawn(actor.run(receiver, ready));
    match initialized.await {
        Ok(Ok(())) => Ok(facade),
        Ok(Err(error)) => {
            cancel.cancel();
            Err(error)
        }
        Err(_) => {
            cancel.cancel();
            Err(anyhow!("OpenCode session initialization failed"))
        }
    }
}
#[async_trait]
impl LiveFamily for V2Session {
    fn is_live(&self) -> bool {
        !self.cancel.is_cancelled()
    }
    async fn publish_replay(
        self: Arc<Self>,
        provider: &str,
        request: SubagentHistoryRequest,
        _directory: &str,
        cancel: CancellationToken,
    ) -> Result<()> {
        let operation = Operation::ReplayChild(provider.to_owned(), Box::new(request));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("OpenCode child history read canceled"),
            published = self.call(operation) => published?,
        };
        Ok(())
    }
}
#[async_trait]
impl AgentSession for V2Session {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn native_id(&self) -> Option<String> {
        Some(self.id.clone())
    }
    fn capabilities(&self) -> &Value {
        &self.capabilities
    }
    fn features(&self) -> Arc<[Value]> {
        self.snapshot.lock().unwrap().features.clone()
    }
    fn initial_timeline(&self) -> &[Value] {
        &self.initial
    }
    fn describe_persistence(&self) -> Option<Value> {
        Some(
            json!({"provider":self.provider,"sessionId":self.id,"nativeHandle":self.id,"metadata":self.snapshot.lock().unwrap().config}),
        )
    }
    fn pending_permissions(&self) -> Vec<Value> {
        self.snapshot.lock().unwrap().pending.clone()
    }
    fn usage_session(&self) -> Option<Value> {
        None
    }
    async fn start_turn(&self, prompt: Value, options: Value) -> Result<Value> {
        self.call(Operation::Start(prompt, options)).await
    }
    async fn steer_active_turn(&self, prompt: Value, options: Value) -> Result<Value> {
        self.call(Operation::Steer(prompt, options)).await
    }
    async fn stream_history(&self, sink: EventSink) -> Result<crate::providers::api::HistoryCoverage> {
        self.call(Operation::History(sink)).await?;
        Ok(crate::providers::api::HistoryCoverage::Complete)
    }
    async fn runtime_info(&self) -> Result<Value> {
        self.call(Operation::RuntimeInfo).await
    }
    async fn available_modes(&self) -> Result<Vec<Value>> {
        Ok(self.snapshot.lock().unwrap().modes.clone())
    }
    async fn current_mode(&self) -> Result<Option<String>> {
        Ok(self.snapshot.lock().unwrap().info["agent"]
            .as_str()
            .map(str::to_owned))
    }
    async fn set_mode(&self, mode: &str) -> Result<Option<Value>> {
        self.call(Operation::Mode(mode.to_owned())).await?;
        Ok(None)
    }
    async fn respond_to_permission(&self, id: &str, response: Value) -> Result<Option<Value>> {
        self.call(Operation::Permission(id.to_owned(), response))
            .await?;
        Ok(None)
    }
    async fn interrupt(&self) -> Result<()> {
        self.call(Operation::Interrupt).await?;
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.cancel.cancel();
        self.closed.cancelled().await;
        if let Some(error) = self.close_error.lock().unwrap().as_ref() {
            bail!("{error}");
        }
        Ok(())
    }
    async fn list_commands(&self) -> Result<Vec<Value>> {
        Ok(array(&self.call(Operation::Commands).await?).to_vec())
    }
    async fn set_model(&self, model: Option<&str>) -> Result<()> {
        self.call(Operation::Model(model.map(str::to_owned)))
            .await?;
        Ok(())
    }
    async fn set_thinking_option(&self, option: Option<&str>) -> Result<Option<Value>> {
        self.call(Operation::Thinking(option.map(str::to_owned)))
            .await?;
        Ok(None)
    }
    async fn set_feature(&self, feature: &str, value: Value) -> Result<()> {
        self.call(Operation::Feature(feature.to_owned(), value))
            .await?;
        Ok(())
    }
    async fn rewind(&self, target: RewindTarget, message_id: &str) -> Result<()> {
        if target != RewindTarget::Both {
            bail!("OpenCode only supports rewinding conversation and files together");
        }
        self.call(Operation::Rewind(message_id.to_owned())).await?;
        Ok(())
    }
}
