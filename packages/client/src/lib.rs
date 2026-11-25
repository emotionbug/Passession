//! Async Paseo SDK. Requests are never replayed after uncertain disconnects.
//! Operation methods accept wire-shaped JSON fields so optional provider/plugin extensions
//! remain lossless. Use owned subscriptions for streaming and explicit cursor pagination.
pub use paseo_protocol::crypto;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod desktop_app_bridge;
#[cfg(not(target_arch = "wasm32"))]
mod discovery;
mod files;
mod operations;
#[cfg(not(target_arch = "wasm32"))]
pub mod process;
mod subscriptions;
pub mod transport;
#[cfg(windows)]
pub mod windows_security;
#[cfg(not(target_arch = "wasm32"))]
use crate::transport::target::Endpoint;
use crate::{
    crypto::SecurityMode,
    transport::{
        HostFrame,
        connector::{Connection, Sender},
        target::Target,
    },
};
use anyhow::{Result, anyhow, bail, ensure};
pub use files::*;
pub use operations::*;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
pub use subscriptions::*;
use tokio::sync::{Semaphore, broadcast, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ClientError {
    pub code: String,
    pub message: String,
    pub details: Option<Value>,
}
impl ClientError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: None,
        }
    }
    pub fn details(mut self, details: impl Into<Value>) -> Self {
        self.details = Some(details.into());
        self
    }
}

#[derive(Clone)]
pub enum DaemonTarget {
    Instance { home: PathBuf },
    Endpoint { host: String },
}
impl DaemonTarget {
    pub fn select(home: Option<&str>, host: Option<&str>, local_only: bool) -> Result<Self> {
        #[cfg(target_arch = "wasm32")]
        return Self::select_with_env(home, host, local_only, None, None);
        #[cfg(not(target_arch = "wasm32"))]
        Self::select_with_env(
            home,
            host,
            local_only,
            std::env::var("PASEO_HOME").ok().as_deref(),
            std::env::var("PASEO_HOST").ok().as_deref(),
        )
    }
    pub fn select_with_env(
        home: Option<&str>,
        host: Option<&str>,
        local_only: bool,
        env_home: Option<&str>,
        env_host: Option<&str>,
    ) -> Result<Self> {
        for (name, value) in [("home", home), ("host", host)] {
            if value.is_some_and(|v| v.trim().is_empty()) {
                return Err(ClientError::new(
                    "TARGET_INVALID",
                    format!("--{name} requires a non-empty value."),
                )
                .into());
            }
        }
        if home.is_some() && host.is_some() {
            return Err(ClientError::new(
                "TARGET_AMBIGUOUS",
                "Choose either --home or --host, not both.",
            )
            .into());
        }
        if local_only && host.is_some() {
            return Err(ClientError::new(
                "LOCAL_OPERATION",
                "This is a local operation; use --home. --host is not supported.",
            )
            .into());
        }
        if let Some(home) = home {
            return Ok(Self::Instance {
                home: resolve_home(Some(home))?,
            });
        }
        if local_only {
            return Ok(Self::Instance {
                home: resolve_home(env_home)?,
            });
        }
        if let Some(host) = host {
            return Ok(Self::Endpoint { host: host.into() });
        }
        if env_home.is_some_and(|s| !s.is_empty()) && env_host.is_some_and(|s| !s.is_empty()) {
            return Err(ClientError::new(
                "TARGET_AMBIGUOUS",
                "PASEO_HOME and PASEO_HOST are both set. Choose --home or --host explicitly.",
            )
            .into());
        }
        if let Some(host) = env_host.filter(|s| !s.is_empty()) {
            return Ok(Self::Endpoint { host: host.into() });
        }
        Ok(Self::Instance {
            home: resolve_home(env_home)?,
        })
    }
    pub fn description(&self) -> String {
        match self {
            Self::Instance { home } => format!("home {}", home.display()),
            Self::Endpoint { host } => {
                Target::parse(host)
                    .map(|t| t.description)
                    .unwrap_or_else(|_| {
                        if is_registration(host) {
                            format!("registered host {host}")
                        } else {
                            "<invalid endpoint>".into()
                        }
                    })
            }
        }
    }
    pub fn home(&self) -> Result<PathBuf> {
        #[cfg(target_arch = "wasm32")]
        return resolve_home(None);
        #[cfg(not(target_arch = "wasm32"))]
        match self {
            Self::Instance { home } => Ok(home.clone()),
            _ => resolve_home(std::env::var("PASEO_HOME").ok().as_deref()),
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub fn resolve_home(value: Option<&str>) -> Result<PathBuf> {
    let value = value.unwrap_or("~/.paseo");
    let path = if value == "~" {
        dirs::home_dir().ok_or_else(|| anyhow!("User home unavailable"))?
    } else if let Some(rest) = value.strip_prefix("~/") {
        dirs::home_dir()
            .ok_or_else(|| anyhow!("User home unavailable"))?
            .join(rest)
    } else {
        PathBuf::from(value)
    };
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}
#[cfg(target_arch = "wasm32")]
pub fn resolve_home(_value: Option<&str>) -> Result<PathBuf> {
    Err(ClientError::new(
        "BROWSER_UNSUPPORTED",
        "Daemon home is not available in the browser",
    )
    .into())
}
fn is_registration(host: &str) -> bool {
    host.starts_with("hst_") || host.starts_with("srv_")
}

#[derive(Clone)]
pub struct ConnectOptions {
    pub target: DaemonTarget,
    /// Identity/discovery directory override for an explicit endpoint.
    pub home: Option<PathBuf>,
    pub timeout: Duration,
    pub password: Option<String>,
    /// Explicit credential for an authenticated in-process or local connection.
    pub local_credential: Option<String>,
    /// Pins an encrypted direct WebSocket endpoint (relay URLs already carry the pin).
    pub server_public_key: Option<String>,
    pub client_id: Option<String>,
    pub minimum_security: SecurityMode,
    pub client_type: String,
}
impl ConnectOptions {
    pub fn new(target: DaemonTarget) -> Self {
        Self {
            target,
            home: None,
            timeout: Duration::from_secs(15),
            password: None,
            local_credential: None,
            server_public_key: None,
            client_id: None,
            minimum_security: SecurityMode::Legacy,
            client_type: "cli".into(),
        }
    }
}
struct Pending {
    response: String,
    reply: oneshot::Sender<Result<Value>>,
}
struct Shared {
    pending: Mutex<HashMap<String, Pending>>,
    events: broadcast::Sender<Value>,
    binary: broadcast::Sender<bytes::Bytes>,
    /// File frames of downloads in progress, by request ID. They bypass `binary`, whose
    /// readers lose frames when they fall behind (a burst of buffered chunks).
    transfers: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<bytes::Bytes>>>,
    info: RwLock<Value>,
    cancel: CancellationToken,
}
#[derive(Clone)]
enum ClientSender {
    Direct(Sender),
    Routed(tokio::sync::mpsc::Sender<HostFrame>, CancellationToken),
}
impl ClientSender {
    async fn frame(&self, frame: HostFrame) -> Result<()> {
        match self {
            Self::Direct(sender) => sender.send(frame).await,
            Self::Routed(sender, cancel) => {
                tokio::select! {_=cancel.cancelled()=>bail!("Host route closed"),result=sender.send(frame)=>result.map_err(|_|anyhow!("Host route closed"))}
            }
        }
    }
    async fn json(&self, value: Value) -> Result<()> {
        self.frame(HostFrame::Text(serde_json::to_string(&value)?))
            .await
    }
    async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.frame(HostFrame::Flush(tx)).await?;
        tokio::time::timeout(Duration::from_secs(15), rx).await???;
        Ok(())
    }
}
pub struct DaemonClient {
    sender: ClientSender,
    shared: Arc<Shared>,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    transport: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
/// Input to the SDK reader. Pumps that inspect JSON envelopes can transfer the
/// parsed value instead of making the reader parse the same text again.
#[derive(Debug)]
pub enum IncomingFrame {
    Json(Value),
    Raw(HostFrame),
}

impl From<HostFrame> for IncomingFrame {
    fn from(frame: HostFrame) -> Self {
        Self::Raw(frame)
    }
}

pub struct ClientEvents<T: Clone> {
    receiver: broadcast::Receiver<T>,
    cancel: CancellationToken,
}
impl<T: Clone> ClientEvents<T> {
    pub async fn recv(&mut self) -> std::result::Result<T, broadcast::error::RecvError> {
        tokio::select! {biased;event=self.receiver.recv()=>event,_=self.cancel.cancelled()=>Err(broadcast::error::RecvError::Closed)}
    }
    pub fn try_recv(&mut self) -> std::result::Result<T, broadcast::error::TryRecvError> {
        match self.receiver.try_recv() {
            Err(broadcast::error::TryRecvError::Empty) if self.cancel.is_cancelled() => {
                Err(broadcast::error::TryRecvError::Closed)
            }
            result => result,
        }
    }
}
/// Every file-transfer frame (`0x10`..`0x12`) the daemon sends for one download request, in
/// order and without loss. Dropping it stops the routing.
pub struct FileTransferFrames {
    receiver: tokio::sync::mpsc::UnboundedReceiver<bytes::Bytes>,
    request_id: String,
    shared: Arc<Shared>,
}
impl FileTransferFrames {
    /// The next frame, or `None` once the connection closed and earlier frames were read.
    pub async fn recv(&mut self) -> Option<bytes::Bytes> {
        tokio::select! {biased;
            frame = self.receiver.recv() => frame,
            _ = self.shared.cancel.cancelled() => self.receiver.try_recv().ok(),
        }
    }
}
impl Drop for FileTransferFrames {
    fn drop(&mut self) {
        self.shared
            .transfers
            .lock()
            .unwrap()
            .remove(&self.request_id);
    }
}
/// The request ID of a file-transfer frame: `[opcode, id length, id…]`.
fn file_transfer_request_id(bytes: &[u8]) -> Option<&str> {
    if !matches!(bytes.first(), Some(0x10..=0x12)) {
        return None;
    }
    let length = *bytes.get(1)? as usize;
    std::str::from_utf8(bytes.get(2..2 + length)?).ok()
}
fn deliver_binary(state: &Shared, bytes: bytes::Bytes) {
    if let Some(id) = file_transfer_request_id(&bytes)
        && let Some(route) = state.transfers.lock().unwrap().get(id)
        && route.send(bytes.clone()).is_ok()
    {
        return;
    }
    let _ = state.binary.send(bytes);
}
impl DaemonClient {
    pub async fn connect(options: ConnectOptions, cancel: CancellationToken) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        if let DaemonTarget::Endpoint { host } = &options.target
            && is_registration(host)
        {
            return Self::connect_registration(options, cancel).await;
        }
        Self::connect_direct(options, cancel).await
    }
    async fn connect_direct(options: ConnectOptions, cancel: CancellationToken) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        let home = options.home.clone().unwrap_or(options.target.home()?);
        #[cfg(target_arch = "wasm32")]
        if options.home.is_some() || options.local_credential.is_some() {
            return Err(ClientError::new(
                "BROWSER_UNSUPPORTED",
                "Local daemon credentials and paths are not available in the browser",
            )
            .into());
        }
        let host = match &options.target {
            DaemonTarget::Endpoint { host } => host.clone(),
            #[cfg(not(target_arch = "wasm32"))]
            DaemonTarget::Instance { home } => {
                discovery::listen(home, options.timeout, &cancel).await?
            }
            #[cfg(target_arch = "wasm32")]
            DaemonTarget::Instance { .. } => {
                return Err(ClientError::new(
                    "BROWSER_UNSUPPORTED",
                    "Local daemon discovery is not available in the browser",
                )
                .into());
            }
        };
        let mut target = Target::parse(&host)?;
        if let Some(key) = options.server_public_key {
            target.security = Some((key, options.minimum_security));
        }
        if let Some(password) = options.password {
            target.password = Some(password);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if target.password.is_none() {
            target.password = std::env::var("PASEO_PASSWORD")
                .ok()
                .filter(|v| !v.is_empty());
        }
        #[cfg(not(target_arch = "wasm32"))]
        let auth = if let Some(token) = options.local_credential {
            Some(json!({"kind":"localCredential","token":token}))
        } else if let Some(password) = target.password.as_ref() {
            Some(json!({"kind":"password","password":password}))
        } else {
            read_local_credential_for_target(&home, &host)
                .await?
                .map(|token| json!({"kind":"localCredential","token":token}))
        };
        #[cfg(target_arch = "wasm32")]
        let auth = target
            .password
            .take()
            .map(|password| json!({"kind":"password","password":password}));
        let identity = match options.client_id {
            Some(id) => id,
            #[cfg(not(target_arch = "wasm32"))]
            None => client_id(&home).await?,
            #[cfg(target_arch = "wasm32")]
            None => {
                return Err(ClientError::new(
                    "CLIENT_ID_REQUIRED",
                    "A client ID is required in the browser",
                )
                .into());
            }
        };
        let connect = Connection::connect(
            &target,
            options.minimum_security,
            Arc::new(Semaphore::new(256 * 1024 * 1024)),
            cancel.clone(),
        );
        let connection = tokio::select! {_=cancel.cancelled()=>bail!("Daemon connection cancelled"),result=tokio::time::timeout(options.timeout,connect)=>result.map_err(|_|ClientError::new("DAEMON_UNREACHABLE","Daemon connection timed out"))??};
        Self::from_parts(
            ClientSender::Direct(connection.sender),
            connection.incoming,
            connection.cancel,
            connection.task,
            hello(&identity, &options.client_type, auth),
            options.timeout,
        )
        .await
    }
    /// Attach an owned transport, including a native plugin host IPC bridge.
    /// The pump must honor cancellation and acknowledge `HostFrame::Flush`.
    /// Incoming frames may be raw transport frames or already-parsed envelopes.
    pub async fn from_transport<F: Into<IncomingFrame> + Send + 'static>(
        outgoing: tokio::sync::mpsc::Sender<HostFrame>,
        incoming: tokio::sync::mpsc::Receiver<Result<F>>,
        cancel: CancellationToken,
        task: tokio::task::JoinHandle<()>,
        client_id: &str,
        client_type: &str,
        timeout: Duration,
    ) -> Result<Self> {
        Self::from_parts(
            ClientSender::Routed(outgoing, cancel.clone()),
            incoming,
            cancel,
            task,
            hello(client_id, client_type, None),
            timeout,
        )
        .await
    }
    async fn from_parts<F: Into<IncomingFrame> + Send + 'static>(
        sender: ClientSender,
        mut incoming: tokio::sync::mpsc::Receiver<Result<F>>,
        cancel: CancellationToken,
        transport: tokio::task::JoinHandle<()>,
        hello: Value,
        timeout: Duration,
    ) -> Result<Self> {
        let (events, _) = broadcast::channel(1024);
        let (binary, _) = broadcast::channel(64);
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            events,
            binary,
            transfers: Mutex::new(HashMap::new()),
            info: RwLock::new(Value::Null),
            cancel,
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let state = shared.clone();
        let reader = tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            let outcome:Result<()>=async {
                loop { let frame=tokio::select!{biased;frame=incoming.recv()=>frame.ok_or_else(||ClientError::new("DAEMON_CONNECTION_LOST","Daemon connection closed"))??,_=state.cancel.cancelled()=>bail!("Daemon connection closed")};
                    let mut wire = match frame.into() {
                        IncomingFrame::Json(wire) => wire,
                        IncomingFrame::Raw(HostFrame::Text(text)) => serde_json::from_str::<Value>(&text)?,
                        IncomingFrame::Raw(HostFrame::Binary(bytes)) => { deliver_binary(&state, bytes); continue; }
                        IncomingFrame::Raw(HostFrame::Close { reason, .. }) => bail!("Daemon connection closed: {reason}"),
                        IncomingFrame::Raw(HostFrame::Flush(reply)) => { let _ = reply.send(Err(anyhow!("Invalid inbound flush barrier"))); continue; }
                    };
                    if wire["type"]=="hello.rejected" {let (code,message)=match wire["reason"].as_str(){Some("password_required")=>("AUTH_REQUIRED","Password required"),Some("incorrect_password")=>("AUTH_FAILED","Incorrect password"),_=>("INCOMPATIBLE_PROTOCOL","Incompatible protocol version")};return Err(ClientError::new(code,message).into());}
                    if wire["type"]=="pong" {continue;}
                    ensure!(wire["type"]=="session","Invalid daemon message envelope");
                    let message=wire.get_mut("message").map(Value::take).ok_or_else(||anyhow!("Missing daemon session message"))?;
                    if message["type"]=="status" && message["payload"]["status"]=="server_info" {
                        ensure!(message["payload"]["protocolVersion"].as_u64().is_none_or(|v|v==1),"Incompatible protocol version");
                        *state.info.write().unwrap()=message["payload"].clone();if let Some(ready)=ready.take(){let _=ready.send(Ok(()));}
                    }
                    dispatch(&state,message);
                }
            }.await;
            let failure = outcome
                .err()
                .unwrap_or_else(|| anyhow!("Daemon connection closed"));
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(failure));
            }
            let pending = std::mem::take(&mut *state.pending.lock().unwrap());
            for (_, pending) in pending {
                let _=pending.reply.send(Err(ClientError::new("DAEMON_CONNECTION_LOST","Daemon connection closed before the response; the operation may already have completed").into()));
            }
            state.cancel.cancel();
            // Downloads read what already arrived, then end.
            state.transfers.lock().unwrap().clear();
        });
        let client = Self {
            sender,
            shared,
            reader: Mutex::new(Some(reader)),
            transport: Mutex::new(Some(transport)),
        };
        let handshake = async {
            client.sender.json(hello).await?;
            ready_rx.await?
        };
        let result = tokio::select! {
            biased;
            result = handshake => result,
            _ = client.shared.cancel.cancelled() => Err(ClientError::new("DAEMON_CONNECTION_LOST", "Connection closed during greeting").into()),
            _ = async { if timeout.is_zero() { std::future::pending::<()>().await } else { tokio::time::sleep(timeout).await } } =>
                Err(ClientError::new("DAEMON_UNREACHABLE", "Timed out waiting for daemon greeting").into()),
        };
        if let Err(error) = result {
            client.close().await;
            return Err(error);
        }
        Ok(client)
    }
    #[cfg(not(target_arch = "wasm32"))]
    async fn connect_registration(
        options: ConnectOptions,
        cancel: CancellationToken,
    ) -> Result<Self> {
        let DaemonTarget::Endpoint { host } = options.target else {
            unreachable!()
        };
        let local = Self::connect_direct(
            ConnectOptions {
                target: DaemonTarget::Instance {
                    home: options
                        .home
                        .clone()
                        .unwrap_or(resolve_home(std::env::var("PASEO_HOME").ok().as_deref())?),
                },
                home: options.home.clone(),
                timeout: options.timeout,
                password: None,
                local_credential: None,
                server_public_key: None,
                client_id: None,
                minimum_security: SecurityMode::Legacy,
                client_type: options.client_type.clone(),
            },
            cancel.clone(),
        )
        .await?;
        let result=async {
            local.require_feature("hostMultiplexing","Update the host to use connected hosts.")?;
            let hosts=local.request(json!({"type":"hosts.list.request"}),"hosts.list.response",options.timeout).await?;
            let row=hosts["hosts"].as_array().and_then(|rows|rows.iter().find(|r|r["registrationId"]==host || r["serverId"]==host)).ok_or_else(||ClientError::new("host_unknown",format!("Unknown registration {host}")))?;
            ensure!(row["enabled"]==true,"Host registration is disabled");
            let server_id=row["serverId"].as_str().ok_or_else(||anyhow!("Host has not established its identity"))?;
            let mut events=local.subscribe();
            let mut binary=local.subscribe_binary();
            let opened=local.request(json!({"type":"hosts.route.open.request","serverId":server_id,"scopeId":uuid::Uuid::new_v4().to_string()}),"hosts.route.open.response",options.timeout).await?;
            let route=opened["routeId"].as_str().ok_or_else(||anyhow!("Missing host route ID"))?.to_owned();
            let (incoming_tx,incoming)=tokio::sync::mpsc::channel(64);
            let (outgoing_tx,mut outgoing)=tokio::sync::mpsc::channel(64);
            let route_cancel=cancel.child_token();let task_cancel=route_cancel.clone();let sender=local.sender.clone();let routing_id=route.clone();
            let task=tokio::spawn(async move {
                let work=async{loop{tokio::select!{
                    _=task_cancel.cancelled()=>break,
                    frame=outgoing.recv()=>{let Some(frame)=frame else{break};match frame {HostFrame::Text(data)=>sender.json(json!({"type":"session","message":{"type":"hosts.route.frame.request","routeId":routing_id,"data":data,"binary":false}})).await?,HostFrame::Binary(bytes)=>sender.frame(HostFrame::Binary(crate::transport::encode_frame(&routing_id,&bytes)?)).await?,HostFrame::Flush(reply)=>{let result=sender.flush().await;let failed=result.is_err();let _=reply.send(result);if failed{bail!("Host output flush failed");}},HostFrame::Close{..}=>break}},
                    event = events.recv() => {
                        let event = event?;
                        if event["payload"]["subscriptionId"] != routing_id {
                            continue;
                        }
                        if event["type"] == "hosts.route.closed" {
                            bail!("{}", event["payload"]["message"].as_str().unwrap_or("Connected host closed"));
                        }
                        if event["type"] == "hosts.route.frame" {
                            let data = event["payload"]["data"].as_str()
                                .ok_or_else(|| anyhow!("Invalid routed host frame"))?;
                            tokio::select! {
                                _ = task_cancel.cancelled() => break,
                                result = incoming_tx.send(Ok(HostFrame::Text(data.into()))) => {
                                    result.map_err(|_| anyhow!("Routed host client closed"))?;
                                }
                            }
                        }
                    },
                    frame=binary.recv()=>{let frame=frame?;if frame.first()!=Some(&0x30){continue;}let(id,offset)=crate::transport::decode_frame(&frame)?;if id==routing_id{tokio::select!{_=task_cancel.cancelled()=>break,result=incoming_tx.send(Ok(HostFrame::Binary(frame.slice(offset..))))=>{result.map_err(|_|anyhow!("Routed host client closed"))?;}}}}
                }}Ok::<(),anyhow::Error>(())};
                let result=tokio::select!{biased;_=task_cancel.cancelled()=>Ok(()),result=work=>result};
                if let Err(error)=result{let _=incoming_tx.try_send(Err(error));}
                let _=tokio::time::timeout(Duration::from_secs(1),sender.json(json!({"type":"session","message":{"type":"hosts.route.close.request","routeId":routing_id}}))).await;
                task_cancel.cancel();
            });
            Ok((outgoing_tx,incoming,route_cancel,task))
        }.await;
        let (outgoing, incoming, route_cancel, task) = match result {
            Ok(v) => v,
            Err(error) => {
                local.close().await;
                return Err(error);
            }
        };
        let client = Self::from_parts(
            ClientSender::Routed(outgoing, route_cancel.clone()),
            incoming,
            route_cancel,
            task,
            hello(
                &format!("cid_{}", uuid::Uuid::new_v4().simple()),
                &options.client_type,
                None,
            ),
            options.timeout,
        )
        .await;
        match client {
            Ok(client) => {
                let stop = client.shared.cancel.clone();
                tokio::spawn(async move {
                    stop.cancelled().await;
                    local.close().await;
                });
                Ok(client)
            }
            Err(error) => {
                local.close().await;
                Err(error)
            }
        }
    }
    pub fn server_info(&self) -> Value {
        self.shared.info.read().unwrap().clone()
    }
    pub fn subscribe(&self) -> ClientEvents<Value> {
        ClientEvents {
            receiver: self.shared.events.subscribe(),
            cancel: self.shared.cancel.clone(),
        }
    }
    pub fn subscribe_binary(&self) -> ClientEvents<bytes::Bytes> {
        ClientEvents {
            receiver: self.shared.binary.subscribe(),
            cancel: self.shared.cancel.clone(),
        }
    }
    /// Routes the file frames of download `request_id` to the returned receiver. Register
    /// before sending the request.
    pub fn file_transfer_frames(&self, request_id: &str) -> FileTransferFrames {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        if !self.shared.cancel.is_cancelled() {
            self.shared
                .transfers
                .lock()
                .unwrap()
                .insert(request_id.to_owned(), sender);
        }
        FileTransferFrames {
            receiver,
            request_id: request_id.to_owned(),
            shared: self.shared.clone(),
        }
    }
    pub fn cancellation(&self) -> CancellationToken {
        self.shared.cancel.clone()
    }
    pub fn require_feature(&self, key: &str, message: &str) -> Result<()> {
        if self.shared.info.read().unwrap()["features"][key] != true {
            bail!("{message}");
        }
        Ok(())
    }
    pub async fn send(&self, message: Value) -> Result<()> {
        self.sender
            .json(json!({"type":"session","message":message}))
            .await
    }
    pub async fn send_binary(&self, bytes: bytes::Bytes) -> Result<()> {
        self.sender.frame(HostFrame::Binary(bytes)).await
    }
    pub async fn flush(&self) -> Result<()> {
        self.sender.flush().await
    }
    pub async fn request(
        &self,
        message: Value,
        response_type: &str,
        timeout: Duration,
    ) -> Result<Value> {
        self.request_cancellable(message, response_type, timeout, CancellationToken::new())
            .await
    }
    pub async fn request_cancellable(
        &self,
        mut message: Value,
        response_type: &str,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(message.is_object(), "Request must be a JSON object");
        let started = tokio::time::Instant::now();
        if cancel.is_cancelled() {
            return Err(ClientError::new(
                "DAEMON_REQUEST_CANCELLED",
                "Daemon request cancelled before submission",
            )
            .into());
        }
        if self.shared.cancel.is_cancelled() {
            return Err(ClientError::new(
                "DAEMON_CONNECTION_LOST",
                "Daemon connection closed before submission",
            )
            .into());
        }
        let id = message["requestId"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        message["requestId"] = Value::String(id.clone());
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().unwrap();
            ensure!(!pending.contains_key(&id), "Duplicate live request ID");
            pending.insert(
                id.clone(),
                Pending {
                    response: response_type.into(),
                    reply: tx,
                },
            );
        }
        let guard = PendingGuard {
            state: self.shared.clone(),
            id,
        };
        tokio::select! {biased;
            _=cancel.cancelled()=>return Err(ClientError::new("DAEMON_REQUEST_CANCELLED","Daemon request cancelled before submission").into()),
            _=self.shared.cancel.cancelled()=>return Err(ClientError::new("DAEMON_CONNECTION_LOST","Daemon connection closed before submission").into()),
            _=async { if timeout.is_zero() { std::future::pending::<()>().await } else { tokio::time::sleep_until(started + timeout).await } }=>return Err(ClientError::new("DAEMON_REQUEST_TIMEOUT","Daemon request timed out during submission; outcome is unknown").into()),
            result=self.send(message)=>result?,
        }
        let response = async {
            rx.await.map_err(|_| {
                ClientError::new("DAEMON_CONNECTION_LOST", "Daemon request consumer closed")
            })?
        };
        let deadline = async {
            if timeout.is_zero() {
                std::future::pending::<()>().await;
            } else {
                tokio::time::sleep_until(started + timeout).await;
            }
        };
        let result = tokio::select! {biased;
            result=response=>result,
            _=cancel.cancelled()=>Err(ClientError::new("DAEMON_REQUEST_CANCELLED","Daemon request cancelled; a submitted mutation may still complete").into()),
            _=self.shared.cancel.cancelled()=>Err(ClientError::new("DAEMON_CONNECTION_LOST","Daemon connection closed; a submitted mutation may still complete").into()),
            _=deadline=>Err(ClientError::new("DAEMON_REQUEST_TIMEOUT","Daemon request timed out; a submitted mutation may still complete").into()),
        };
        drop(guard);
        result
    }
    pub async fn release_subscription(&self, id: &str) -> Result<()> {
        self.request(
            json!({"type":"subscription.release.request","subscriptionId":id}),
            "subscription.release.response",
            Duration::from_secs(15),
        )
        .await?;
        Ok(())
    }
    pub async fn daemon_status(&self, timeout: Duration) -> Result<Value> {
        self.require_feature("daemonStatusRpc", "Update the host to read daemon status.")?;
        self.request(
            json!({"type":"daemon.get_status.request"}),
            "daemon.get_status.response",
            timeout,
        )
        .await
    }
    pub async fn restart(&self, reason: &str, timeout: Duration) -> Result<Value> {
        self.request(
            json!({"type":"restart_server_request","reason":reason}),
            "status",
            timeout,
        )
        .await
    }
    pub async fn shutdown(&self, timeout: Duration) -> Result<Value> {
        self.request(json!({"type":"shutdown_server_request"}), "status", timeout)
            .await
    }
    pub async fn close(&self) {
        self.shared.cancel.cancel();
        let reader = self.reader.lock().unwrap().take();
        let transport = self.transport.lock().unwrap().take();
        if let Some(task) = reader {
            let _ = task.await;
        }
        if let Some(task) = transport {
            let _ = task.await;
        }
    }
}
impl Drop for DaemonClient {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}
struct PendingGuard {
    state: Arc<Shared>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state.pending.lock().unwrap().remove(&self.id);
    }
}
fn dispatch(state: &Shared, message: Value) {
    let id = message["payload"]["requestId"]
        .as_str()
        .or_else(|| message["requestId"].as_str());
    if let Some(id) = id {
        let pending = {
            let mut pending = state.pending.lock().unwrap();
            let matches = pending
                .get(id)
                .is_some_and(|p| message["type"] == p.response || message["type"] == "rpc_error");
            if matches { pending.remove(id) } else { None }
        };
        if let Some(pending) = pending {
            let payload = message
                .get("payload")
                .cloned()
                .unwrap_or_else(|| message.clone());
            // Waiting reports the agent's outcome, not failure of the RPC itself.
            let outcome_response = message["type"] == "wait_for_finish_response";
            let result = if message["type"] == "rpc_error"
                || (!outcome_response && payload["error"].is_string())
                || payload["accepted"] == false
                || payload["success"] == false
            {
                Err(ClientError::new(
                    payload["code"]
                        .as_str()
                        .or_else(|| payload["errorCode"].as_str())
                        .unwrap_or("DAEMON_RPC_ERROR"),
                    payload["error"]
                        .as_str()
                        .or_else(|| payload["message"].as_str())
                        .unwrap_or("Daemon rejected the request"),
                )
                .details(payload.clone())
                .into())
            } else {
                Ok(payload)
            };
            let _ = pending.reply.send(result);
        }
    }
    let _ = state.events.send(message);
}
fn hello(identity: &str, client_type: &str, auth: Option<Value>) -> Value {
    let capabilities = [
        "hello_rejection",
        "owned_subscriptions",
        "explicit_event_subscriptions",
        "all_providers",
        "selective_agent_timeline",
        "reasoning_merge_enum",
        "custom_mode_icons",
        "terminal_reflowable_snapshot",
        "provider_subagents",
        "projected_subagent_timeline",
        "project_updates",
        "compact_provider_snapshots",
        "provider_snapshot_references",
        "timeline_replacement_invalidation",
        "timeline_notifications",
        "plugin_timeline_items",
        "workspace_setup_observation",
        "workspace_setup_blocked",
    ]
    .into_iter()
    .map(|key| (key.to_owned(), Value::Bool(true)))
    .collect::<serde_json::Map<_, _>>();
    let mut value = json!({"type":"hello","clientId":identity,"clientType":client_type,"protocolVersion":1,"capabilities":capabilities,"appVersion":env!("CARGO_PKG_VERSION")});
    if let Some(auth) = auth {
        value["auth"] = auth;
    }
    value
}
#[cfg(not(target_arch = "wasm32"))]
async fn client_id(home: &Path) -> Result<String> {
    let path = home.join("cli-client-id");
    match tokio::fs::read_to_string(&path).await {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().into()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    tokio::fs::create_dir_all(home).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tokio::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700)).await;
    }
    let id = format!("cid_{}", uuid::Uuid::new_v4().simple());
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&path).await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(id.as_bytes()).await?;
    file.sync_all().await?;
    Ok(id)
}
#[cfg(not(target_arch = "wasm32"))]
pub async fn read_local_credential_for_target(home: &Path, host: &str) -> Result<Option<String>> {
    let lock = match tokio::fs::read(home.join("paseo.pid")).await {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(lock) => lock,
            Err(_) => return Ok(None),
        },
        Err(_) => return Ok(None),
    };
    let Some(listen) = lock["listen"].as_str() else {
        return Ok(None);
    };
    let Some(selected) = normalized_local_target(host) else {
        return Ok(None);
    };
    if normalized_local_target(listen).as_deref() != Some(selected.as_str()) {
        return Ok(None);
    }
    let token = match tokio::fs::read_to_string(home.join("local-credential")).await {
        Ok(token) => token,
        Err(_) => return Ok(None),
    };
    let token = token.trim();
    Ok((token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
    .then(|| token.to_owned()))
}
#[cfg(target_arch = "wasm32")]
pub async fn read_local_credential_for_target(_home: &Path, _host: &str) -> Result<Option<String>> {
    Err(ClientError::new(
        "BROWSER_UNSUPPORTED",
        "Local daemon credentials are not available in the browser",
    )
    .into())
}

#[cfg(not(target_arch = "wasm32"))]
fn normalized_local_target(host: &str) -> Option<String> {
    let host = host.trim();
    if host.starts_with("unix://") || host.starts_with("pipe://") {
        return Some(host.into());
    }
    if host.starts_with('/') {
        return Some(format!("unix://{host}"));
    }
    if host.starts_with(r"\\.\pipe\") {
        return Some(format!("pipe://{host}"));
    }
    if host.contains("://") && !host.starts_with("tcp://") {
        return None;
    }
    let target = Target::parse(host).ok()?;
    if target.security.is_some() {
        return None;
    }
    match target.endpoint {
        Endpoint::Socket { url, ipc: None } => {
            let url = url::Url::parse(&url).ok()?;
            let host = url.host_str()?;
            let host = if matches!(
                host,
                "127.0.0.1" | "0.0.0.0" | "[::1]" | "::1" | "[::]" | "::" | "localhost"
            ) {
                "localhost"
            } else {
                host
            };
            Some(format!("{host}:{}", url.port_or_known_default()?))
        }
        _ => None,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    #[test]
    fn selectors_reject_ambiguous_authority() {
        assert!(
            DaemonTarget::select_with_env(None, None, false, Some("/a"), Some("host:6767"))
                .is_err()
        );
        assert!(
            DaemonTarget::select_with_env(Some("/a"), Some("host:6767"), false, None, None)
                .is_err()
        );
        assert!(
            matches!(DaemonTarget::select_with_env(Some("/a"),None,false,Some("/b"),Some("else:1")).unwrap(),DaemonTarget::Instance{home} if home==Path::new("/a"))
        );
    }
    #[test]
    fn credentials_are_never_selected_for_ssh_or_relay() {
        assert!(normalized_local_target("ssh://localhost?daemonPort=6767").is_none());
        assert_eq!(
            normalized_local_target("tcp://127.0.0.1:6767?password=private"),
            normalized_local_target("localhost:6767")
        );
    }

    #[test]
    fn websocket_password_is_not_part_of_the_wire_url() {
        let target = Target::parse("wss://host.example?password=s%40cret&mode=preview").unwrap();
        assert_eq!(target.password.as_deref(), Some("s@cret"));
        assert!(matches!(
            target.endpoint,
            Endpoint::Socket { url, ipc: None }
                if url == "wss://host.example/ws?mode=preview"
        ));
    }

    #[tokio::test]
    async fn request_correlation_ignores_other_response_type() {
        let (events, _) = broadcast::channel(4);
        let (binary, _) = broadcast::channel(1);
        let state = Shared {
            pending: Mutex::new(HashMap::new()),
            events,
            binary,
            transfers: Mutex::new(HashMap::new()),
            info: RwLock::new(Value::Null),
            cancel: CancellationToken::new(),
        };
        let (tx, mut rx) = oneshot::channel();
        state.pending.lock().unwrap().insert(
            "one".into(),
            Pending {
                response: "expected".into(),
                reply: tx,
            },
        );
        dispatch(
            &state,
            json!({"type":"wrong","payload":{"requestId":"one","result":false}}),
        );
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        dispatch(
            &state,
            json!({"type":"expected","payload":{"requestId":"one","result":true}}),
        );
        assert_eq!(rx.await.unwrap().unwrap()["result"], true);
    }
    #[tokio::test]
    async fn credentials_require_exact_published_authority() {
        let home = tempfile::tempdir().unwrap();
        let token = "a".repeat(43);
        tokio::fs::write(home.path().join("local-credential"), &token)
            .await
            .unwrap();
        tokio::fs::write(
            home.path().join("paseo.pid"),
            br#"{"listen":"0.0.0.0:6767"}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            read_local_credential_for_target(home.path(), "127.0.0.1:6767")
                .await
                .unwrap(),
            Some(token)
        );
        for target in [
            "localhost:6768",
            "remote.example:6767",
            "ws://localhost:6767/other",
            "ssh://localhost?daemonPort=6767",
        ] {
            assert_eq!(
                read_local_credential_for_target(home.path(), target)
                    .await
                    .unwrap(),
                None
            );
        }
        tokio::fs::write(home.path().join("paseo.pid"), b"{partial")
            .await
            .unwrap();
        assert_eq!(
            read_local_credential_for_target(home.path(), "localhost:6767")
                .await
                .unwrap(),
            None
        );
    }
    #[tokio::test]
    async fn empty_identity_is_replaced_and_reused() {
        let home = tempfile::tempdir().unwrap();
        tokio::fs::write(home.path().join("cli-client-id"), " \n")
            .await
            .unwrap();
        let first = client_id(home.path()).await.unwrap();
        assert!(first.starts_with("cid_"));
        assert_eq!(first.len(), 36);
        assert_eq!(client_id(home.path()).await.unwrap(), first);
    }
    #[tokio::test]
    async fn cancellation_prevents_submission_and_cleans_pending_ownership() {
        let (events, _) = broadcast::channel(4);
        let (binary, _) = broadcast::channel(1);
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared {
            pending: Mutex::new(HashMap::new()),
            events,
            binary,
            transfers: Mutex::new(HashMap::new()),
            info: RwLock::new(Value::Null),
            cancel: cancel.clone(),
        });
        let (tx, mut incoming) = tokio::sync::mpsc::channel(1);
        let client = DaemonClient {
            sender: ClientSender::Routed(tx, cancel),
            shared: shared.clone(),
            reader: Mutex::new(None),
            transport: Mutex::new(None),
        };
        let stop = CancellationToken::new();
        stop.cancel();
        assert!(
            client
                .request_cancellable(json!({"type":"mutation"}), "done", Duration::ZERO, stop)
                .await
                .is_err()
        );
        assert!(incoming.try_recv().is_err());
        assert!(shared.pending.lock().unwrap().is_empty());
        let stop = CancellationToken::new();
        let request = client.request_cancellable(
            json!({"type":"mutation","requestId":"owned"}),
            "done",
            Duration::ZERO,
            stop.clone(),
        );
        tokio::pin!(request);
        tokio::select! {result=&mut request=>panic!("Request finished before cancellation: {result:?}"),frame=incoming.recv()=>assert!(matches!(frame,Some(HostFrame::Text(_))))}
        assert!(shared.pending.lock().unwrap().contains_key("owned"));
        stop.cancel();
        assert!(request.await.is_err());
        assert!(shared.pending.lock().unwrap().is_empty());
    }
    fn incoming_json(text: &str, parsed: bool) -> IncomingFrame {
        if parsed {
            IncomingFrame::Json(serde_json::from_str(text).unwrap())
        } else {
            HostFrame::Text(text.into()).into()
        }
    }

    #[tokio::test]
    async fn raw_and_parsed_transports_keep_message_routing() {
        tokio::time::timeout(Duration::from_secs(5), async {
            for parsed in [false, true] {
                let cancel = CancellationToken::new();
                let stop = cancel.clone();
                let (outgoing, mut outbound) = tokio::sync::mpsc::channel(8);
                let (inbound, incoming) = tokio::sync::mpsc::channel(8);
                let task = tokio::spawn(async move {
                    assert!(matches!(outbound.recv().await, Some(HostFrame::Text(_))));
                    inbound.send(Ok(incoming_json(
                        r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","protocolVersion":1,"serverId":"srv"}}}"#,
                        parsed,
                    ))).await.unwrap();
                    assert!(matches!(outbound.recv().await, Some(HostFrame::Text(_))));
                    for text in [
                        r#"{"type":"pong"}"#,
                        r#"{"type":"session","message":{"type":"wrong","payload":{"requestId":"one","value":0}}}"#,
                        r#"{"type":"session","message":{"type":"notice","payload":{"text":"quoted \"text\" and 한글"}}}"#,
                        r#"{"type":"session","message":{"type":"expected","payload":{"requestId":"one","value":1}}}"#,
                    ] {
                        inbound.send(Ok(incoming_json(text, parsed))).await.unwrap();
                    }
                    inbound.send(Ok(HostFrame::Binary(bytes::Bytes::from_static(b"\xff\0")).into())).await.unwrap();
                    stop.cancelled().await;
                });
                let client = DaemonClient::from_transport(
                    outgoing, incoming, cancel, task, "test", "cli", Duration::from_secs(1),
                ).await.unwrap();
                assert_eq!(client.server_info()["serverId"], "srv");
                let mut events = client.subscribe();
                let mut binary = client.subscribe_binary();
                let response = client.request(
                    json!({"type":"request","requestId":"one"}), "expected", Duration::from_secs(1),
                ).await.unwrap();
                assert_eq!(response["value"], 1);
                for kind in ["wrong", "notice", "expected"] {
                    let event = events.recv().await.unwrap();
                    assert_eq!(event["type"], kind);
                    if kind == "notice" {
                        assert_eq!(event["payload"]["text"], "quoted \"text\" and 한글");
                    }
                }
                assert!(matches!(events.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
                assert_eq!(binary.recv().await.unwrap().as_ref(), b"\xff\0");
                client.close().await;
            }
        }).await.expect("transport routing stalled");
    }

    #[tokio::test]
    async fn raw_and_parsed_transports_keep_handshake_errors() {
        for parsed in [false, true] {
            for (text, expected) in [
                (
                    r#"{"type":"hello.rejected","reason":"password_required"}"#,
                    "Password required",
                ),
                (
                    r#"{"type":"hello.rejected","reason":"incorrect_password"}"#,
                    "Incorrect password",
                ),
                (
                    r#"{"type":"hello.rejected","reason":"incompatible_protocol"}"#,
                    "Incompatible protocol version",
                ),
                (r#"{"type":"invalid"}"#, "Invalid daemon message envelope"),
                (r#"{"type":"session"}"#, "Missing daemon session message"),
                (
                    r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","protocolVersion":2}}}"#,
                    "Incompatible protocol version",
                ),
            ] {
                let cancel = CancellationToken::new();
                let stop = cancel.clone();
                let (outgoing, mut outbound) = tokio::sync::mpsc::channel(1);
                let (inbound, incoming) = tokio::sync::mpsc::channel(1);
                let task = tokio::spawn(async move {
                    assert!(matches!(outbound.recv().await, Some(HostFrame::Text(_))));
                    inbound.send(Ok(incoming_json(text, parsed))).await.unwrap();
                    stop.cancelled().await;
                });
                let result = DaemonClient::from_transport(
                    outgoing,
                    incoming,
                    cancel,
                    task,
                    "test",
                    "cli",
                    Duration::from_secs(1),
                )
                .await;
                let error = match result {
                    Ok(_) => panic!("invalid handshake accepted"),
                    Err(error) => error,
                };
                assert_eq!(error.to_string(), expected);
            }
        }
    }

    #[tokio::test]
    async fn disconnection_drains_buffered_events_then_closes_subscription() {
        let (sender, receiver) = broadcast::channel(2);
        let cancel = CancellationToken::new();
        let mut events = ClientEvents {
            receiver,
            cancel: cancel.clone(),
        };
        sender.send(json!({"finished":true})).unwrap();
        cancel.cancel();
        assert_eq!(events.recv().await.unwrap()["finished"], true);
        assert!(matches!(
            events.recv().await,
            Err(broadcast::error::RecvError::Closed)
        ));
    }

    #[tokio::test]
    async fn a_fast_download_is_not_lost_behind_its_own_frames() {
        use paseo_protocol::binary_frames::{
            FileBeginMetadata, FileEncoding, FileTransferFrame, encode_file_transfer_frame,
        };
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let (outgoing, mut outbound) = tokio::sync::mpsc::channel(8);
        // The socket already holds the whole file when the consumer starts reading.
        let (inbound, incoming) = tokio::sync::mpsc::channel::<Result<IncomingFrame>>(1024);
        let chunks = 400;
        let task = tokio::spawn(async move {
            assert!(matches!(outbound.recv().await, Some(HostFrame::Text(_))));
            inbound.send(Ok(HostFrame::Text(r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","protocolVersion":1,"serverId":"srv"}}}"#.into()).into())).await.unwrap();
            let Some(HostFrame::Text(request)) = outbound.recv().await else {
                panic!()
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            let id = request["message"]["requestId"].as_str().unwrap().to_owned();
            let frame = |frame: FileTransferFrame<'_>| {
                Ok(HostFrame::Binary(encode_file_transfer_frame(&frame).unwrap().into()).into())
            };
            let metadata = FileBeginMetadata {
                mime: "application/octet-stream".into(),
                size: chunks * 1024,
                encoding: FileEncoding::Binary,
                modified_at: "2026-11-23T00:00:00.000Z".into(),
                revision: None,
                file_name: None,
            };
            inbound
                .send(frame(FileTransferFrame::Begin {
                    request_id: &id,
                    metadata,
                }))
                .await
                .unwrap();
            let payload = [7u8; 1024];
            for _ in 0..chunks {
                inbound
                    .send(frame(FileTransferFrame::Chunk {
                        request_id: &id,
                        payload: &payload,
                    }))
                    .await
                    .unwrap();
            }
            inbound
                .send(frame(FileTransferFrame::End { request_id: &id }))
                .await
                .unwrap();
            stop.cancelled().await;
        });
        let client = DaemonClient::from_transport(
            outgoing,
            incoming,
            cancel,
            task,
            "test",
            "cli",
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        let file = tokio::time::timeout(
            Duration::from_secs(5),
            client.read_file("/", "file", 1 << 20, RequestOptions::default()),
        )
        .await
        .expect("download stalled")
        .expect("download failed");
        assert_eq!(file.bytes.len() as u64, chunks * 1024);
        client.close().await;
    }
}
