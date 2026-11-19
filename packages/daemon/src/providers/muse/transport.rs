use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::PathBuf,
    process::ExitStatus,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use rand::RngCore;
use serde::{
    Deserialize, Deserializer,
    ser::{SerializeMap, Serializer},
};
use serde_json::{Value, json, value::RawValue};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::providers::{api::ProviderSpec, jsonl::LineReader};
use paseo_client::process::{CommandSpec, ManagedProcess};

const FINGERPRINT: &str = "sha256:e0e163db6ccf00dbe68402ce55d6319b3edc33c421f31e9583b587b2de8a118f";
const NOTIFICATION_COUNT: usize = 256;
const NOTIFICATION_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct MuseError {
    pub kind: String,
    pub message: String,
    pub diagnostic: Option<String>,
    pub reason: Option<String>,
}

impl MuseError {
    pub fn new(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            diagnostic: None,
            reason: None,
        }
    }

    pub fn provider_error(&self) -> Value {
        let mut error = json!({"code": self.kind, "message": self.message});
        if let Some(diagnostic) = &self.diagnostic {
            error["diagnostic"] = diagnostic.clone().into();
        }
        error
    }
}

pub fn actionable_error(error: &anyhow::Error, env: &BTreeMap<String, String>) -> Value {
    let Some(error) = error.downcast_ref::<MuseError>() else {
        return json!({"code": "muse", "message": error.to_string()});
    };
    if error.kind == "internal" && error.message.contains("automated reviewer is unavailable") {
        let config_home = env
            .get("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env.get("HOME").map(String::as_str).unwrap_or("~")).join(".config")
            });
        return json!({
            "code": "defaultProfileUnavailable",
            "message": format!("Change permissions.default_profile in {}: the automated reviewer is unavailable on this host.", config_home.join("muse/settings.json").display()),
        });
    }
    if error.reason.as_deref() == Some("compaction_unavailable") {
        return json!({
            "code": error.kind,
            "message": "Muse cannot compact this session with its current model configuration. Ensure Muse has a context limit for the selected model (context_compaction.provider_context_limit_tokens).",
            "diagnostic": error.diagnostic.as_deref().unwrap_or(&error.message),
        });
    }
    if error.kind == "sessionNotFound" {
        return json!({"code": error.kind, "message": "This Muse session no longer exists. Create a new agent."});
    }
    if error.kind == "authRequired" {
        return json!({"code": error.kind, "message": "Run `muse login` or set META_API_KEY."});
    }
    if error.kind == "internal" && error.message.contains("UnsafePath") {
        let data_home = env
            .get("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env.get("HOME").map(String::as_str).unwrap_or("~"))
                    .join(".local/share")
            });
        return json!({
            "code": "unsafePath",
            "message": format!("Muse data directory {} requires mode 0700. Set its permissions before retrying.", data_home.join("muse").display()),
        });
    }
    error.provider_error()
}

pub fn actionable(error: &anyhow::Error, env: &BTreeMap<String, String>) -> anyhow::Error {
    let presented = actionable_error(error, env);
    let mut mapped = error
        .downcast_ref::<MuseError>()
        .cloned()
        .unwrap_or_else(|| MuseError::new("muse", error.to_string()));
    mapped.kind = presented["code"].as_str().unwrap_or("muse").into();
    mapped.message = presented["message"].as_str().unwrap_or_default().into();
    if let Some(diagnostic) = presented.get("diagnostic").and_then(Value::as_str) {
        mapped.diagnostic = Some(diagnostic.into());
    }
    mapped.into()
}

pub fn command_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub fn serve_args(options: &Value) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let Some(object) = options.as_object() else {
        return Err(MuseError::new(
            "invalidProviderOptions",
            "Invalid Muse providerOptions: providerOptions: expected object",
        )
        .into());
    };
    for key in object.keys() {
        if !matches!(key.as_str(), "sandbox" | "trustWorkspace") {
            errors.push(format!("providerOptions.{key}: Unrecognized key"));
        }
    }
    let mut enabled = false;
    let mut network = "proxy-only";
    if let Some(sandbox) = object.get("sandbox") {
        if let Some(sandbox) = sandbox.as_object() {
            for key in sandbox.keys() {
                if !matches!(key.as_str(), "enabled" | "network") {
                    errors.push(format!("providerOptions.sandbox.{key}: Unrecognized key"));
                }
            }
            if let Some(value) = sandbox.get("enabled") {
                match value.as_bool() {
                    Some(value) => enabled = value,
                    None => errors.push("providerOptions.sandbox.enabled: expected boolean".into()),
                }
            }
            if let Some(value) = sandbox.get("network") {
                match value.as_str() {
                    Some(value @ ("proxy-only" | "restricted" | "enabled")) => network = value,
                    _ => errors.push("providerOptions.sandbox.network: expected proxy-only, restricted, or enabled".into()),
                }
            }
        } else {
            errors.push("providerOptions.sandbox: expected object".into());
        }
    }
    let trust_workspace = match object.get("trustWorkspace") {
        None => true,
        Some(value) => match value.as_bool() {
            Some(value) => value,
            None => {
                errors.push("providerOptions.trustWorkspace: expected boolean".into());
                true
            }
        },
    };
    if !errors.is_empty() {
        return Err(MuseError::new(
            "invalidProviderOptions",
            format!("Invalid Muse providerOptions: {}", errors.join("; ")),
        )
        .into());
    }
    let mut args = vec!["--sandbox-network".into(), network.into()];
    if !enabled {
        args.push("--disable-sandbox".into());
    }
    if trust_workspace {
        args.push("--trust-workspace".into());
    }
    Ok(args)
}

type Reply = std::result::Result<Value, MuseError>;
type CloseResult = std::result::Result<(), MuseError>;

struct Notification {
    method: String,
    params: Option<Box<RawValue>>,
}

impl Notification {
    fn bytes(&self) -> u32 {
        (self.method.capacity() + self.params.as_ref().map_or(0, |p| p.get().len())) as u32
    }
}

pub(super) struct QueuedNotification {
    notification: Notification,
    _budget: OwnedSemaphorePermit,
}

impl QueuedNotification {
    pub(super) fn into_value(self) -> Result<Value> {
        let Notification { method, params } = self.notification;
        let params = match params {
            Some(params) => serde_json::from_str(params.get()).map_err(invalid_frame)?,
            None => Value::Null,
        };
        Ok(Value::Object(
            [
                ("method".into(), Value::String(method)),
                ("params".into(), params),
            ]
            .into_iter()
            .collect(),
        ))
    }
}

struct Notifications {
    sender: mpsc::Sender<QueuedNotification>,
    budget: Arc<Semaphore>,
}

impl Notifications {
    fn try_send(&self, notification: Notification) -> Option<Notification> {
        let slot = match self.sender.try_reserve() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(())) => return Some(notification),
            Err(mpsc::error::TrySendError::Closed(())) => return None,
        };
        let Ok(budget) = self
            .budget
            .clone()
            .try_acquire_many_owned(notification.bytes())
        else {
            return Some(notification);
        };
        slot.send(QueuedNotification {
            notification,
            _budget: budget,
        });
        None
    }

    async fn send(&self, pending: &mut Option<Notification>) {
        let bytes = pending.as_ref().expect("notification to send").bytes();
        let Ok(slot) = self.sender.reserve().await else {
            pending.take();
            return;
        };
        let Ok(budget) = self.budget.clone().acquire_many_owned(bytes).await else {
            unreachable!("notification budget is never closed");
        };
        let notification = pending.take().expect("notification to send");
        slot.send(QueuedNotification {
            notification,
            _budget: budget,
        });
    }
}

#[derive(Default)]
struct State {
    sequence: u64,
    pending: BTreeMap<u64, oneshot::Sender<Reply>>,
    failure: Option<MuseError>,
    closing: bool,
}

struct Inner {
    process: Arc<ManagedProcess>,
    error_env: BTreeMap<String, String>,
    state: Arc<Mutex<State>>,
    request_started: Notify,
    request_timeout: Duration,
    cancel: CancellationToken,
    close_requested: CancellationToken,
    reader_stop: CancellationToken,
    reader_done: CancellationToken,
    exited: CancellationToken,
    close_result: watch::Sender<Option<CloseResult>>,
    runtime: tokio::runtime::Handle,
}

pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    /// Transport controls are separate from strict Muse providerOptions. Omit
    /// providerOptions for maintenance hosts; pass {} for default session flags.
    pub(super) async fn spawn(
        spec: &ProviderSpec,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
        options: &Value,
        cancel: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<QueuedNotification>)> {
        if cancel.is_cancelled() {
            return Err(MuseError::new("closed", "Muse host is closing").into());
        }
        let (program, prefix) = spec
            .command
            .split_first()
            .filter(|(program, _)| !program.is_empty())
            .ok_or_else(|| {
                MuseError::new(
                    "missingLaunch",
                    "Muse requires a daemon-resolved executable",
                )
            })?;
        let mut args = prefix.to_vec();
        args.push("serve".into());
        if let Some(provider_options) = options.get("providerOptions") {
            args.extend(serve_args(provider_options)?);
        }
        let mut launch_env = spec.env.clone();
        launch_env.extend(env);
        let error_env = ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]
            .into_iter()
            .filter_map(|key| {
                launch_env
                    .get(key)
                    .map(|value| (key.to_owned(), value.clone()))
            })
            .collect();
        let (process, stdout) = ManagedProcess::spawn(CommandSpec {
            program: program.clone(),
            args,
            cwd,
            env: launch_env,
            clear_env: true,
        })
        .await
        .map_err(|error| {
            let not_found = error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound);
            MuseError::new(
                "spawn",
                format!(
                    "spawn {program}{}: {error:#}",
                    if not_found { " ENOENT" } else { "" }
                ),
            )
        })?;
        let (sender, receiver) = mpsc::channel(NOTIFICATION_COUNT);
        let notifications = Notifications {
            sender,
            budget: Arc::new(Semaphore::new(NOTIFICATION_BYTES)),
        };
        let (close_result, _) = watch::channel(None);
        let inner = Arc::new(Inner {
            process,
            error_env,
            state: Arc::new(Mutex::new(State::default())),
            request_started: Notify::new(),
            request_timeout: Duration::from_millis(
                options
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000),
            ),
            cancel,
            close_requested: CancellationToken::new(),
            reader_stop: CancellationToken::new(),
            reader_done: CancellationToken::new(),
            exited: CancellationToken::new(),
            close_result,
            runtime: tokio::runtime::Handle::current(),
        });
        let reader = inner.clone();
        inner.runtime.spawn(async move {
            if let Err(error) = reader.read_stdout(stdout, &notifications).await {
                reader.fail(error);
                reader.begin_close();
            }
            reader.reader_done.cancel();
        });
        let watcher = inner.clone();
        inner.runtime.spawn(async move {
            let status = watcher.process.wait_status().await;
            // Child exit can race the last buffered response. Drain stdout first,
            // unless an explicit close owns teardown of inherited open pipes.
            tokio::select! {
                _ = watcher.reader_done.cancelled() => {},
                _ = watcher.close_requested.cancelled() => {},
            }
            let error = match status {
                Ok(status) => classify_exit(status, &watcher.process.stderr_tail()),
                Err(error) => MuseError::new("crash", format!("Muse host exit failed: {error:#}")),
            };
            watcher.fail(error);
            watcher.exited.cancel();
            watcher.reader_stop.cancel();
        });
        let cancellation = inner.clone();
        inner.runtime.spawn(async move {
            tokio::select! {
                _ = cancellation.cancel.cancelled() => {
                    cancellation.fail(MuseError::new("closed", "Muse host is closing"));
                    cancellation.begin_close();
                },
                _ = cancellation.close_requested.cancelled() => {},
                _ = cancellation.exited.cancelled() => {},
            }
        });
        let connection = Arc::new(Self { inner });
        let startup_timeout = Duration::from_millis(
            options
                .get("startupTimeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(30_000),
        );
        let initialized = connection.request_with_timeout("initialize", json!({
            "clientInfo": {"name": "paseo", "version": "1"},
            "capabilities": {"experimentalApi": true, "requestedCapabilities": ["sessionMcp", "sessionListStream"]},
        }), startup_timeout).await;
        let initialized = match initialized {
            Ok(value) => value,
            Err(error) => {
                let _ = connection.close().await;
                return Err(error);
            }
        };
        let Some(fingerprint) = initialized
            .get("schema")
            .and_then(|schema| schema.get("fingerprint"))
            .and_then(Value::as_str)
        else {
            let _ = connection.close().await;
            return Err(MuseError::new(
                "invalidFrame",
                "Invalid MSP initialize result: schema.fingerprint must be a string",
            )
            .into());
        };
        if fingerprint != FINGERPRINT {
            tracing::warn!(
                expected = FINGERPRINT,
                received = fingerprint,
                "Muse MSP fingerprint mismatch"
            );
        }
        if let Err(error) = connection
            .inner
            .process
            .write_json(&json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}))
            .await
        {
            let _ = connection.close().await;
            return Err(connection
                .inner
                .failure()
                .unwrap_or_else(|| MuseError::new("closed", error.to_string()))
                .into());
        }
        Ok((connection, receiver))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.inner.request_timeout)
            .await
    }

    pub async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.request_params(method, &params, timeout).await
    }

    async fn request_params(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value> {
        let result = self
            .request_raw(method, params, timeout)
            .await
            .map_err(|error| actionable(&error, &self.inner.error_env))?;
        if !result.is_object() {
            return Err(MuseError::new(
                "invalidFrame",
                format!("Invalid MSP {method} result: expected object"),
            )
            .into());
        }
        Ok(result)
    }

    async fn request_raw(&self, method: &str, params: &Value, timeout: Duration) -> Result<Value> {
        let (sender, receiver) = oneshot::channel();
        let id = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(error) = &state.failure {
                return Err(error.clone().into());
            }
            if state.closing || self.inner.cancel.is_cancelled() {
                return Err(MuseError::new("closed", "Muse host is closing").into());
            }
            state.sequence += 1;
            let id = state.sequence;
            state.pending.insert(id, sender);
            id
        };
        self.inner.request_started.notify_one();
        let _pending = PendingGuard {
            state: self.inner.state.clone(),
            id,
        };
        let mut bytes = Vec::new();
        let mut serializer = serde_json::Serializer::new(&mut bytes);
        let mut frame = serializer.serialize_map(Some(4))?;
        frame.serialize_entry("jsonrpc", "2.0")?;
        frame.serialize_entry("id", &id)?;
        frame.serialize_entry("method", method)?;
        frame.serialize_entry("params", params)?;
        frame.end()?;
        bytes.push(b'\n');
        let response = tokio::time::timeout(timeout, async {
            // As in the MSP client, process exit owns broken-pipe diagnostics.
            // A write failure is never authority to replay an admitted command.
            let _ = self.inner.process.write_bytes(&bytes).await;
            receiver.await.unwrap_or_else(|_| {
                Err(self
                    .inner
                    .failure()
                    .unwrap_or_else(|| MuseError::new("closed", "Muse host is closing")))
            })
        })
        .await;
        match response {
            Ok(response) => response.map_err(Into::into),
            Err(_) => Err(MuseError::new("timeout", format!("Muse {method} timed out")).into()),
        }
    }

    pub async fn command(&self, method: &str, mut params: Value) -> Result<Value> {
        let input = params.as_object_mut().ok_or_else(|| {
            MuseError::new("invalidParams", "Muse command params must be an object")
        })?;
        if !input.get("commandId").is_some_and(Value::is_string) {
            input.insert("commandId".into(), command_id().into());
        }
        for attempt in 0..3 {
            match self
                .request_params(method, &params, self.inner.request_timeout)
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) => {
                    let retryable = error.downcast_ref::<MuseError>().is_some_and(|error| {
                        matches!(error.kind.as_str(), "overloaded" | "backpressured")
                    });
                    if !retryable || attempt == 2 {
                        return Err(error);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))) => {},
                        _ = self.inner.close_requested.cancelled() => {
                            let error = self.inner.failure().unwrap_or_else(|| MuseError::new("closed", "Muse host is closing"));
                            return Err(actionable(&error.into(), &self.inner.error_env));
                        },
                        _ = self.inner.cancel.cancelled() => return Err(MuseError::new("closed", "Muse host is closing").into()),
                    }
                }
            }
        }
        unreachable!("the last attempt always returns")
    }

    pub async fn close(&self) -> Result<()> {
        let mut completion = self.inner.close_result.subscribe();
        self.inner.begin_close();
        loop {
            if let Some(result) = completion.borrow_and_update().clone() {
                return result.map_err(Into::into);
            }
            if completion.changed().await.is_err() {
                return Err(MuseError::new("closed", "Muse close supervisor stopped").into());
            }
        }
    }

    pub(super) async fn notification_failure(&self) -> MuseError {
        // The reader drops its sender before the exit watcher publishes the
        // process diagnostic. An explicit framing error is already recorded.
        match self.inner.failure() {
            Some(error) => error,
            None => {
                self.inner.exited.cancelled().await;
                self.inner
                    .failure()
                    .expect("exit watcher records failure before signaling exit")
            }
        }
    }
}

/// The MSP request surface projections read through, so the child-history
/// boundary can be exercised against a recorded host.
#[async_trait::async_trait]
pub trait Host: Send + Sync {
    async fn request(&self, method: &str, params: Value) -> Result<Value>;
}

#[async_trait::async_trait]
impl Host for Connection {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Connection::request(self, method, params).await
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.inner.begin_close();
    }
}

struct PendingGuard {
    state: Arc<Mutex<State>>,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .remove(&self.id);
    }
}

impl Inner {
    async fn read_stdout(
        &self,
        stdout: impl tokio::io::AsyncRead + Unpin,
        notifications: &Notifications,
    ) -> std::result::Result<(), MuseError> {
        let mut lines = LineReader::new(stdout);
        let mut pending = None;
        loop {
            let staged = pending.is_some();
            // Without outstanding RPCs, let the pipe backpressure the host.
            // The actor itself can await an RPC while handling an event, so
            // when a request is pending we must look past the staged event for
            // the reply. A second event that cannot fit is terminal: waiting
            // for capacity here could deadlock, and dropping it would lose
            // ordered session state.
            let read_control = staged
                && !self
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .pending
                    .is_empty();
            let read = tokio::select! {
                biased;
                _ = self.reader_stop.cancelled() => return Ok(()),
                _ = notifications.send(&mut pending), if staged => continue,
                _ = self.request_started.notified(), if staged => continue,
                read = lines.next_line(), if !staged || read_control => read,
            };
            if !read.map_err(invalid_frame)? {
                if staged {
                    return Err(invalid_frame(
                        "stdout closed with a staged notification and a pending control request",
                    ));
                }
                return Ok(());
            }
            if lines.line().iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
                return Err(invalid_frame("expected object"));
            }
            let frame = serde_json::from_slice(lines.line()).map_err(invalid_frame)?;
            if let Some((method, params)) = self.receive(frame).await? {
                if let Some(previous) = pending.take()
                    && notifications.try_send(previous).is_some()
                {
                    return Err(MuseError::new(
                        "notificationOverload",
                        "Muse sent notifications faster than Paseo could process them; the session was closed to avoid losing updates.",
                    ));
                }
                let notification = Notification {
                    method,
                    params: params.map(RawValue::to_owned),
                };
                pending = notifications.try_send(notification);
            }
            lines.clear_line();
        }
    }

    fn failure(&self) -> Option<MuseError> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failure
            .clone()
    }

    fn fail(&self, error: MuseError) {
        let pending = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.failure.is_some() {
                return;
            }
            state.failure = Some(error.clone());
            std::mem::take(&mut state.pending)
        };
        for sender in pending.into_values() {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn begin_close(self: &Arc<Self>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closing {
                return;
            }
            state.closing = true;
        }
        self.close_requested.cancel();
        let inner = self.clone();
        self.runtime.spawn(async move {
            let result = inner.finish_close().await;
            if let Err(error) = &result {
                inner.fail(error.clone());
            }
            inner.reader_stop.cancel();
            inner.close_result.send_replace(Some(result));
        });
    }

    async fn finish_close(&self) -> CloseResult {
        let drained = tokio::time::timeout(Duration::from_secs(1), async {
            let _ = self.process.close_stdin().await;
            self.exited.cancelled().await;
        })
        .await
        .is_ok();
        if drained {
            return Ok(());
        }
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.process.terminate(Duration::ZERO),
        )
        .await
        {
            Ok(Ok(())) => {
                self.exited.cancelled().await;
                Ok(())
            }
            Ok(Err(error)) => Err(MuseError::new(
                "timeout",
                format!("Muse host did not close after termination: {error:#}"),
            )),
            Err(_) => Err(MuseError::new(
                "timeout",
                "Muse host did not close after termination",
            )),
        }
    }

    async fn receive<'a>(
        &self,
        frame: Frame<'a>,
    ) -> std::result::Result<Option<(String, Option<&'a RawValue>)>, MuseError> {
        frame.validate()?;
        if let Some(Value::String(method)) = frame.method
            && !method.is_empty()
        {
            let Some(id) = frame.id else {
                return Ok(Some((method, frame.params)));
            };
            let response = if matches!(method.as_str(), "approval/request" | "userInput/request") {
                json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Unsupported server request"}})
            };
            let _ = self.process.write_json(&response).await;
            return Ok(None);
        }
        let Some(id) = frame.id else {
            return Err(invalid_frame("MSP response has no id"));
        };
        if id.is_null() {
            if let Some(error) = frame.error {
                return Err(rpc_error(&error, false));
            }
            return Err(invalid_frame("MSP result has a null id"));
        }
        let Some(id) = id.as_u64().or_else(|| {
            let number = id.as_f64()?;
            (number >= 0.0 && number.fract() == 0.0 && number <= u64::MAX as f64)
                .then_some(number as u64)
        }) else {
            return Ok(None);
        };
        let sender = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending
            .remove(&id);
        if let Some(sender) = sender {
            let result = if let Some(error) = frame.error {
                Err(rpc_error(&error, true))
            } else if let Some(result) = frame.result {
                match serde_json::from_str(result.get()) {
                    Ok(result) => Ok(result),
                    Err(error) => {
                        let error = invalid_frame(error);
                        let _ = sender.send(Err(error.clone()));
                        return Err(error);
                    }
                }
            } else {
                Ok(Value::Null)
            };
            let _ = sender.send(result);
        }
        Ok(None)
    }
}

/// Parse control data once while leaving large notification parameters and
/// response bodies borrowed from the input line until their consumer needs them.
#[derive(Deserialize)]
struct Frame<'a> {
    #[serde(borrow)]
    jsonrpc: Cow<'a, str>,
    #[serde(default, deserialize_with = "present")]
    id: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    method: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Value>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
}

// Missing and explicit null differ for JSON-RPC IDs and malformed fields.
fn present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn invalid_frame(error: impl std::fmt::Display) -> MuseError {
    MuseError::new("invalidFrame", format!("Invalid MSP frame: {error}"))
}

impl Frame<'_> {
    fn validate(&self) -> std::result::Result<(), MuseError> {
        if self.jsonrpc != "2.0" {
            return Err(invalid_frame("jsonrpc must be 2.0"));
        }
        if self
            .id
            .as_ref()
            .is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
        {
            return Err(invalid_frame("id must be a string, number, or null"));
        }
        if self
            .method
            .as_ref()
            .is_some_and(|method| !method.is_string())
        {
            return Err(invalid_frame("method must be a string"));
        }
        if let Some(error) = &self.error {
            let error = error
                .as_object()
                .ok_or_else(|| invalid_frame("error must be an object"))?;
            if !error.get("code").is_some_and(Value::is_number) {
                return Err(invalid_frame("error.code must be a number"));
            }
            if !error.get("message").is_some_and(Value::is_string) {
                return Err(invalid_frame("error.message must be a string"));
            }
            if let Some(data) = error.get("data") {
                let data = data
                    .as_object()
                    .ok_or_else(|| invalid_frame("error.data must be an object"))?;
                for key in ["kind", "reason"] {
                    if data.get(key).is_some_and(|value| !value.is_string()) {
                        return Err(invalid_frame(
                            "error.data.kind and error.data.reason must be strings when present",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn rpc_error(error: &Value, correlated: bool) -> MuseError {
    let mut result = MuseError::new(
        error
            .get("data")
            .and_then(|data| data.get("kind"))
            .and_then(Value::as_str)
            .unwrap_or("rpc"),
        error["message"].as_str().unwrap_or_default(),
    );
    if correlated {
        if error["code"].as_f64() == Some(-32020.0) {
            result.kind = "sessionNotFound".into();
        }
        result.reason = error
            .get("data")
            .and_then(|data| data.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    result
}

fn classify_exit(status: ExitStatus, stderr: &str) -> MuseError {
    let kind = match status.code() {
        Some(0) => "cleanExit",
        Some(2) => "usage",
        Some(3) => "credentials",
        Some(4) => "leaseHeld",
        Some(5) => "surfaceOff",
        _ => "crash",
    };
    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = status.signal() {
                    return nix::sys::signal::Signal::try_from(signal)
                        .map(|signal| signal.to_string())
                        .unwrap_or_else(|_| signal.to_string());
                }
            }
            status.to_string()
        });
    let mut units = 0;
    let mut start = stderr.len();
    for (index, character) in stderr.char_indices().rev() {
        units += character.len_utf16();
        if units > 8192 {
            break;
        }
        start = index;
    }
    MuseError::new(
        kind,
        format!("Muse host exited ({code}). {}", &stderr[start..]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::Poll;

    #[tokio::test]
    async fn notification_byte_budget_backpressures_even_with_free_slots() {
        let first = Notification {
            method: "test".into(),
            params: Some(RawValue::from_string(r#"{"seq":1}"#.into()).unwrap()),
        };
        let second = Notification {
            method: "test".into(),
            params: Some(RawValue::from_string(r#"{"seq":2}"#.into()).unwrap()),
        };
        let (sender, mut receiver) = mpsc::channel(2);
        let budget = first.bytes().max(second.bytes()) as usize;
        let notifications = Notifications {
            sender,
            budget: Arc::new(Semaphore::new(budget)),
        };
        assert!(notifications.try_send(first).is_none());
        let mut staged = notifications.try_send(second);
        assert!(staged.is_some());
        let mut send = std::pin::pin!(notifications.send(&mut staged));
        assert!(matches!(futures_util::poll!(&mut send), Poll::Pending));
        assert_eq!(
            receiver.recv().await.unwrap().into_value().unwrap()["params"]["seq"],
            1
        );
        send.await;
        assert_eq!(
            receiver.recv().await.unwrap().into_value().unwrap()["params"]["seq"],
            2
        );
    }

    #[cfg(unix)]
    async fn saturated_host(
        extra_notifications: usize,
    ) -> (Arc<Connection>, mpsc::Receiver<QueuedNotification>) {
        // No notification is consumed until after the second control request
        // returns. One extra frame exercises the stdout reader's staged slot.
        let script = r#"
            read -r initialize
            printf '{"jsonrpc":"2.0","id":1,"result":{"schema":{"fingerprint":"%s"}}}\n' "$2"
            read -r initialized
            i=0
            while [ "$i" -lt "$1" ]; do
                printf '{"jsonrpc":"2.0","method":"test","params":{"seq":%s}}\n' "$i"
                i=$((i+1))
            done
            read -r request
            printf '{"jsonrpc":"2.0","id":2,"result":{"ok":true}}\n'
            read -r close
        "#;
        let spec = ProviderSpec {
            id: "muse".into(),
            family: "muse".into(),
            command: vec![
                "/bin/sh".into(),
                "-c".into(),
                script.into(),
                "muse-fixture".into(),
                (NOTIFICATION_COUNT + extra_notifications).to_string(),
                FINGERPRINT.into(),
            ],
            env: BTreeMap::new(),
            options: Value::Null,
            defaults: json!({}),
            runtime_settings: json!({}),
            configured_models: vec![],
        };
        let (host, receiver) = Connection::spawn(
            &spec,
            BTreeMap::new(),
            None,
            &json!({"timeoutMs":5000, "startupTimeoutMs":5000}),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while receiver.len() < NOTIFICATION_COUNT {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        (host, receiver)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn full_notification_queue_delivers_control_reply_before_consumer_drains() {
        let (host, mut receiver) = saturated_host(1).await;
        let reply = host.request("test", json!({})).await.unwrap();
        assert_eq!(reply, json!({"ok":true}));
        assert_eq!(receiver.len(), NOTIFICATION_COUNT);
        for expected in 0..=NOTIFICATION_COUNT {
            let notification = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap()
                .into_value()
                .unwrap();
            assert_eq!(
                notification["params"]["seq"].as_u64(),
                Some(expected as u64)
            );
        }
        host.close().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notification_overload_fails_pending_control_and_reports_terminal_error() {
        let (host, mut receiver) = saturated_host(2).await;
        let error = host.request("test", json!({})).await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<MuseError>().unwrap().kind,
            "notificationOverload"
        );
        for _ in 0..NOTIFICATION_COUNT {
            receiver.recv().await.unwrap();
        }
        assert!(receiver.recv().await.is_none());
        assert_eq!(host.notification_failure().await.message, error.to_string());
        host.close().await.unwrap();
    }
}
