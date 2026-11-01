//! Native Agent Client Protocol adapter for standalone Paseo plugins.
//!
//! Register [`AcpProvider::command`] for an external ACP executable, or use
//! [`AcpProvider::connector`] for an owned asynchronous byte transport. Both use
//! the same bounded JSON-RPC implementation, session lifecycle and transformers.
//! No JavaScript runtime or dependency on the Paseo daemon is required.
mod mapping;
mod session;
mod transport;
use crate::{
    Context, Result, anyhow, bail, ensure,
    provider::{Connection, Events, Metadata, Provider},
};
use async_trait::async_trait;
use serde_json::{Value, json};
use session::Session;
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
pub use transport::AcpStream;

/// ACP v1 startup/discovery/readiness policy. Prompt duration is not limited.
#[derive(Clone, Debug)]
pub struct AcpOptions {
    pub startup_timeout: Duration,
    pub workspace_only_catalog: bool,
    pub wait_for_initial_commands: bool,
    pub initial_commands_timeout: Duration,
    /// Grace period for an interrupted turn before its transport is closed.
    pub cancellation_timeout: Duration,
}
impl Default for AcpOptions {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(10),
            workspace_only_catalog: false,
            wait_for_initial_commands: false,
            initial_commands_timeout: Duration::from_secs(1),
            cancellation_timeout: Duration::from_secs(2),
        }
    }
}

#[derive(Clone, Debug)]
pub enum ConfigChange {
    Model(Value),
    Mode(Value),
    Thinking(Value),
    Setting { id: String, value: Value },
}
#[derive(Clone, Debug)]
pub enum VendorUpdate {
    Commands(Value),
    Config(Value),
    Timeline(Value),
    Notice(Value),
}

/// Access to the live ACP config options, including transformer-owned options.
#[derive(Clone)]
pub struct ConfigAccess {
    session: Weak<Session>,
}
impl ConfigAccess {
    pub async fn read(&self) -> Result<Value> {
        let session = self
            .session
            .upgrade()
            .ok_or_else(|| anyhow!("ACP session closed"))?;
        let values = session.mapping.lock().unwrap().values();
        Ok(values)
    }
    pub async fn set(&self, id: &str, value: Value) -> Result<()> {
        self.session
            .upgrade()
            .ok_or_else(|| anyhow!("ACP session closed"))?
            .set_option(id, value)
            .await
    }
}
#[derive(Clone)]
pub struct AcpContext {
    pub session_id: String,
    pub config: ConfigAccess,
}

/// Ordered vendor extensions. Notification/tool transforms are synchronous so
/// wire updates are applied before the following JSON-RPC response is resolved.
/// Return `true` from `configure` only after actually applying the change.
#[async_trait]
pub trait AcpTransformer: Send + Sync {
    async fn discover(&self, catalog: Value, _context: AcpContext) -> Result<Value> {
        Ok(catalog)
    }
    async fn configure(&self, _change: &ConfigChange, _context: AcpContext) -> Result<bool> {
        Ok(false)
    }
    fn notification(
        &self,
        _method: &str,
        _params: &Value,
        _session_id: &str,
    ) -> Result<Vec<VendorUpdate>> {
        Ok(Vec::new())
    }
    /// Snapshot shape: id, name?, title, kind?, status, input, output, locations.
    fn tool_call(&self, snapshot: Value, _session_id: &str) -> Result<Value> {
        Ok(snapshot)
    }
}

type ConnectorFuture = Pin<Box<dyn Future<Output = Result<AcpStream>> + Send>>;
type Connector = Arc<dyn Fn(CancellationToken) -> ConnectorFuture + Send + Sync>;
#[derive(Clone)]
enum Source {
    Command(Vec<String>),
    Connector(Connector),
}
#[derive(Clone)]
pub struct AcpProvider {
    metadata: Metadata,
    source: Source,
    options: AcpOptions,
    transformers: Vec<Arc<dyn AcpTransformer>>,
}
impl AcpProvider {
    pub fn command(
        id: impl Into<String>,
        label: impl Into<String>,
        command: Vec<String>,
    ) -> Result<Self> {
        ensure!(
            !command.is_empty() && !command[0].trim().is_empty(),
            "ACP command requires an executable"
        );
        let mut metadata = Metadata::new(id, label);
        metadata.command = Some(command.clone());
        metadata.validate()?;
        Ok(Self {
            metadata,
            source: Source::Command(command),
            options: AcpOptions::default(),
            transformers: Vec::new(),
        })
    }
    /// The connector is called for the capability probe, discovery and each
    /// session. It must return a fresh owned stream; cancellation closes it.
    pub fn connector<F, Fut>(
        id: impl Into<String>,
        label: impl Into<String>,
        connector: F,
    ) -> Result<Self>
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<AcpStream>> + Send + 'static,
    {
        let metadata = Metadata::new(id, label);
        metadata.validate()?;
        Ok(Self {
            metadata,
            source: Source::Connector(Arc::new(move |cancel| Box::pin(connector(cancel)))),
            options: AcpOptions::default(),
            transformers: Vec::new(),
        })
    }
    pub fn options(mut self, options: AcpOptions) -> Self {
        self.options = options;
        self
    }
    pub fn transformer(mut self, transformer: impl AcpTransformer + 'static) -> Self {
        self.transformers.push(Arc::new(transformer));
        self
    }
    pub fn description(mut self, value: impl Into<String>) -> Self {
        self.metadata.description = Some(value.into());
        self
    }
    pub fn icon_path(mut self, value: impl Into<String>) -> Self {
        self.metadata.icon_path = Some(value.into());
        self
    }
}

#[async_trait]
impl Provider for AcpProvider {
    fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }
    async fn connect(
        &self,
        request: Value,
        events: Events,
        context: Context,
    ) -> Result<Arc<dyn Connection>> {
        ensure!(
            request["versions"]
                .as_array()
                .is_some_and(|a| a.contains(&json!(1))),
            "ACP adapter requires provider protocol 1"
        );
        ensure!(
            !self.options.startup_timeout.is_zero() && !self.options.cancellation_timeout.is_zero(),
            "ACP timeouts must be positive"
        );
        let cancel = events.cancellation().child_token();
        let launch = request.get("launch").cloned();
        let probe = Session::start(
            self.clone(),
            launch.as_ref(),
            "capability-probe",
            None,
            &json!({}),
            context.clone(),
            cancel.clone(),
        )
        .await?;
        let agent = probe.agent.lock().unwrap().clone();
        probe.close().await?;
        let mut supported = vec![
            "prompt.message",
            "prompt.command",
            "session.configure",
            "permission",
        ];
        if agent["promptCapabilities"]["image"] == true {
            supported.push("prompt.image");
        }
        if agent["sessionCapabilities"]
            .get("list")
            .is_some_and(|v| !v.is_null() && v != false)
        {
            supported.push("session.list");
        }
        if agent["loadSession"] == true {
            supported.push("session.persistence");
        }
        let capabilities = request["capabilities"]
            .as_array()
            .ok_or_else(|| anyhow!("Missing capabilities"))?
            .iter()
            .filter_map(Value::as_str)
            .filter(|v| supported.contains(v))
            .map(str::to_owned)
            .collect();
        let adapter = Arc::new(Adapter {
            provider: self.clone(),
            launch,
            events,
            context,
            cancel: cancel.clone(),
            capabilities,
            sessions: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Value>(32);
        let worker = adapter.clone();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            loop {
                let input=tokio::select!{biased;_=worker.cancel.cancelled()=>break,input=receiver.recv()=>match input{Some(input)=>input,None=>break}};
                tokio::select!{biased;_=worker.cancel.cancelled()=>break,_=worker.handle(input)=>{}}
            }
        });
        Ok(Arc::new(NativeConnection {
            adapter,
            sender,
            tasks: Mutex::new(tasks),
        }))
    }
}
struct Adapter {
    provider: AcpProvider,
    launch: Option<Value>,
    events: Events,
    context: Context,
    cancel: CancellationToken,
    capabilities: Vec<String>,
    sessions: Mutex<BTreeMap<String, Arc<Session>>>,
    closed: AtomicBool,
}
impl Adapter {
    fn require(&self, capability: &str) -> Result<()> {
        ensure!(
            self.capabilities.iter().any(|v| v == capability),
            "Unsupported provider capability: {capability}"
        );
        Ok(())
    }
    async fn session(&self, input: &Value) -> Result<Arc<Session>> {
        self.sessions
            .lock()
            .await
            .get(text(input, "sessionId")?)
            .cloned()
            .ok_or_else(|| anyhow!("Unknown ACP session"))
    }
    async fn start(&self, id: &str, env: &Value) -> Result<Arc<Session>> {
        Session::start(
            self.provider.clone(),
            self.launch.as_ref(),
            id,
            Some(self.events.clone()),
            env,
            self.context.clone(),
            self.cancel.clone(),
        )
        .await
    }
    async fn dispatch(&self, input: &Value) -> Result<()> {
        match text(input, "type")? {
            "catalog" => {
                let runtime = self.start("catalog", &json!({})).await?;
                let result = runtime
                    .discover(input.get("cwd").and_then(Value::as_str))
                    .await;
                let cleanup = runtime.close().await;
                let catalog = result?;
                cleanup?;
                self.events
                    .emit(
                        json!({"type":"catalog","requestId":input["requestId"],"catalog":catalog}),
                    )
                    .await
            }
            "sessions" => {
                self.require("session.list")?;
                let runtime = self.start("sessions", &json!({})).await?;
                let mut params = json!({});
                if let Some(cwd) = input.get("cwd") {
                    params["cwd"] = cwd.clone();
                }
                let result = runtime
                    .rpc
                    .request(
                        "session/list",
                        params,
                        Some(self.provider.options.startup_timeout),
                    )
                    .await;
                let cleanup = runtime.close().await;
                let response = result?;
                cleanup?;
                let rows = response["sessions"]
                    .as_array()
                    .ok_or_else(|| anyhow!("ACP session/list omitted sessions"))?;
                let limit = input["limit"].as_u64().unwrap_or(u64::MAX);
                let sessions=rows.iter().take(usize::try_from(limit).unwrap_or(usize::MAX)).map(|row|{
                    let mut value=json!({"persistence":{"version":1,"data":{"sessionId":row["sessionId"]}},"cwd":row["cwd"]});
                    for key in ["title","updatedAt"] {if let Some(v)=row.get(key).filter(|v|!v.is_null()){value[key]=v.clone();}}value
                }).collect::<Vec<_>>();
                self.events.emit(json!({"type":"sessions","requestId":input["requestId"],"sessions":sessions})).await
            }
            "session.open" => {
                if input.get("persistence").is_some_and(|v| !v.is_null()) {
                    self.require("session.persistence")?;
                }
                let id = text(input, "sessionId")?;
                ensure!(
                    !self.sessions.lock().await.contains_key(id),
                    "Session already exists: {id}"
                );
                let runtime = self.start(id, &input["config"]["env"]).await?;
                self.sessions
                    .lock()
                    .await
                    .insert(id.into(), runtime.clone());
                if let Err(error) = runtime.open(input, &self.capabilities).await {
                    self.sessions.lock().await.remove(id);
                    let _ = runtime.close().await;
                    return Err(error);
                }
                Ok(())
            }
            "session.prompt" => {
                let prompt = &input["prompt"];
                ensure!(
                    prompt["delivery"] != "steer",
                    "ACP adapter does not support steering"
                );
                ensure!(
                    prompt.get("outputSchema").is_none_or(Value::is_null),
                    "ACP adapter does not support structured output"
                );
                self.require(if prompt["input"]["type"] == "command" {
                    "prompt.command"
                } else {
                    "prompt.message"
                })?;
                if prompt["input"]["content"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v["type"] == "image"))
                {
                    self.require("prompt.image")?;
                }
                self.session(input).await?.prompt(prompt.clone()).await
            }
            "session.interrupt" => {
                self.session(input).await?.interrupt().await?;
                self.events
                    .emit(json!({"type":"request.completed","requestId":input["requestId"]}))
                    .await
            }
            "session.permission" => {
                self.require("permission")?;
                self.session(input)
                    .await?
                    .permission(text(input, "permissionId")?, &input["response"])
                    .await
            }
            "session.configure" => {
                self.require("session.configure")?;
                self.session(input)
                    .await?
                    .configure(&input["changes"])
                    .await?;
                self.events
                    .emit(json!({"type":"request.completed","requestId":input["requestId"]}))
                    .await
            }
            "session.close" => {
                let id = text(input, "sessionId")?;
                let runtime = self
                    .sessions
                    .lock()
                    .await
                    .remove(id)
                    .ok_or_else(|| anyhow!("Unknown ACP session"))?;
                runtime.close().await?;
                self.events
                    .emit(json!({"type":"session.closed","sessionId":id}))
                    .await?;
                self.events
                    .emit(json!({"type":"request.completed","requestId":input["requestId"]}))
                    .await
            }
            other => bail!("ACP adapter does not support {other}"),
        }
    }
}
struct NativeConnection {
    adapter: Arc<Adapter>,
    sender: tokio::sync::mpsc::Sender<Value>,
    tasks: Mutex<tokio::task::JoinSet<()>>,
}
impl Adapter {
    async fn handle(&self, input: Value) {
        if let Err(error) = self.dispatch(&input).await {
            let failure = json!({"message":error.to_string()});
            let event = if input["type"] == "session.prompt" {
                json!({"type":"session.prompt_result","sessionId":input["sessionId"],"clientMessageId":input["prompt"]["clientMessageId"],"result":{"type":"failed","error":failure}})
            } else if let Some(request) = input.get("requestId") {
                json!({"type":"request.failed","requestId":request,"error":failure})
            } else if let Some(session) = input.get("sessionId") {
                json!({"type":"session.runtime_failed","sessionId":session,"error":failure})
            } else {
                return;
            };
            let _ = self.events.emit(event).await;
        }
    }
}
#[async_trait]
impl Connection for NativeConnection {
    fn version(&self) -> u64 {
        1
    }
    fn capabilities(&self) -> Vec<String> {
        self.adapter.capabilities.clone()
    }
    async fn send(&self, input: Value) -> Result<()> {
        ensure!(
            !self.adapter.closed.load(Ordering::Acquire) && !self.adapter.cancel.is_cancelled(),
            "ACP connection closed"
        );
        let kind = text(&input, "type")?;
        ensure!(
            matches!(
                kind,
                "catalog"
                    | "sessions"
                    | "session.open"
                    | "session.prompt"
                    | "session.interrupt"
                    | "session.permission"
                    | "session.configure"
                    | "session.close"
            ),
            "Unsupported ACP provider input: {kind}"
        );
        if matches!(kind, "session.permission" | "session.interrupt") {
            let mut tasks = self.tasks.lock().await;
            while tasks.try_join_next().is_some() {}
            ensure!(tasks.len() < 257, "Too many pending ACP control requests");
            let adapter = self.adapter.clone();
            tasks.spawn(async move {
                tokio::select! {biased;_=adapter.cancel.cancelled()=>{},_=adapter.handle(input)=>{}}
            });
            Ok(())
        } else {
            self.sender
                .try_send(input)
                .map_err(|error| anyhow!("ACP request queue unavailable: {error}"))
        }
    }
    async fn close(&self) -> Result<()> {
        if self.adapter.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.adapter.cancel.cancel();
        let sessions = std::mem::take(&mut *self.adapter.sessions.lock().await);
        let mut failure = None;
        for runtime in sessions.into_values() {
            if let Err(error) = runtime.close().await {
                failure = Some(error);
            }
        }
        let mut tasks = self.tasks.lock().await;
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow!("ACP input requires {key}"))
}
