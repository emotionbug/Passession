use crate::providers::jsonl::{LineError, LineReader};
use anyhow::{Result, anyhow, bail};
use paseo_client::process::{CommandSpec, ManagedProcess};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct RpcError {
    pub code: Value,
    pub message: String,
    pub data: Value,
}
impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RpcError {}

impl RpcError {
    pub fn definitive_steer_rejection(&self) -> bool {
        if self.code == -32601 {
            return true;
        }
        if self.code != -32600 {
            return false;
        }
        if self.data["codexErrorInfo"]["activeTurnNotSteerable"].is_object() {
            return true;
        }
        if matches!(
            self.message.as_str(),
            "no active turn to steer" | "active turn uses a different output schema"
        ) {
            return true;
        }
        let Some(rest) = self.message.strip_prefix("expected active turn id `") else {
            return false;
        };
        let Some((expected, actual)) = rest.split_once("` but found `") else {
            return false;
        };
        !expected.is_empty()
            && !expected.contains('`')
            && actual
                .strip_suffix('`')
                .is_some_and(|s| !s.is_empty() && !s.contains('`'))
    }
    pub fn already_idle_interrupt(&self) -> bool {
        self.code == -32600 && self.message == "no active turn to interrupt"
    }
}

#[derive(Default)]
struct Control {
    pending: HashMap<u64, oneshot::Sender<Result<Value>>>,
    failure: Option<CloseFailure>,
}

struct CloseFailure {
    message: String,
    framing: bool,
    reported: bool,
}

pub struct Transport {
    pub process: Arc<ManagedProcess>,
    control: Mutex<Control>,
    next: AtomicU64,
    pub cancel: CancellationToken,
}
impl Transport {
    pub async fn spawn(
        spec: CommandSpec,
        cancel: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Value>)> {
        let (process, stdout) = ManagedProcess::spawn(spec).await?;
        let this = Arc::new(Self {
            process,
            control: Mutex::new(Control::default()),
            next: AtomicU64::new(1),
            cancel: cancel.child_token(),
        });
        let (sender, receiver) = mpsc::channel(4096);
        let reader = this.clone();
        tokio::spawn(async move {
            let mut input = LineReader::new(stdout);
            let (failure, framing_failure) = loop {
                let read = tokio::select! {
                    _ = reader.cancel.cancelled() => break ("Codex app-server canceled".to_owned(), false),
                    result = input.next_line() => result,
                };
                match read {
                    Ok(false) => {
                        let status = tokio::select! {
                            _ = reader.cancel.cancelled() => break ("Codex app-server canceled".to_owned(), false),
                            result=reader.process.wait()=>result,
                        };
                        let failure = match status {
                            Ok(status) => format!(
                                "Codex app-server exited with status {status}\n{}",
                                reader.process.stderr_tail()
                            )
                            .trim()
                            .to_owned(),
                            Err(error) => format!(
                                "Codex app-server exit failed: {error}\n{}",
                                reader.process.stderr_tail()
                            )
                            .trim()
                            .to_owned(),
                        };
                        break (failure, false);
                    }
                    Err(LineError::Io(error)) => {
                        break (format!("Codex app-server stdout: {error}"), false);
                    }
                    Err(error) => {
                        break (
                            format!("Invalid Codex app-server stdout frame: {error}"),
                            true,
                        );
                    }
                    Ok(true) => {}
                }
                if input.line().iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let message: Value = match serde_json::from_slice(input.line()) {
                    Ok(v) => v,
                    Err(e) => break (format!("Invalid Codex app-server JSON: {e}"), false),
                };
                input.clear_line();
                if message.get("method").is_none() {
                    if let Some(id) = message["id"].as_u64() {
                        let pending = reader.control.lock().unwrap().pending.remove(&id);
                        if let Some(pending) = pending {
                            let result = if let Some(error) = message.get("error") {
                                Err(RpcError {
                                    code: error["code"].clone(),
                                    message: error["message"]
                                        .as_str()
                                        .unwrap_or("Codex RPC error")
                                        .into(),
                                    data: error["data"].clone(),
                                }
                                .into())
                            } else {
                                Ok(message["result"].clone())
                            };
                            let _ = pending.send(result);
                        }
                    }
                } else if sender.try_send(message).is_err() {
                    break (
                        "Codex event queue exceeded 4096 pending frames or its consumer closed"
                            .to_owned(),
                        false,
                    );
                }
            };
            drop(input);
            reader.fail(failure, framing_failure);
            drop(sender);
            let _ = reader.process.terminate(Duration::from_secs(2)).await;
        });
        Ok((this, receiver))
    }

    fn fail(&self, message: String, framing: bool) {
        let mut control = self.control.lock().unwrap();
        if control.failure.is_none() {
            for (_, sender) in control.pending.drain() {
                let _ = sender.send(Err(anyhow!("{message}")));
            }
            control.failure = Some(CloseFailure {
                message,
                framing,
                reported: false,
            });
        }
    }

    /// The draining reader and reconnect can both discover the disconnect.
    /// Preserve the original cause and publish the terminal event only once.
    pub(super) fn take_close_event(&self) -> Option<Value> {
        let mut control = self.control.lock().unwrap();
        let failure = control.failure.as_mut()?;
        if failure.reported {
            return None;
        }
        failure.reported = true;
        Some(json!({"method":"paseo/transportClosed","params":{
            "message":failure.message,"framingFailure":failure.framing,
        }}))
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_timeout(method, params, Duration::from_secs(14 * 24 * 60 * 60))
            .await
    }
    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut control = self.control.lock().unwrap();
            if let Some(failure) = &control.failure {
                bail!("{}", failure.message);
            }
            control.pending.insert(id, tx);
        }
        let result = async {
            if let Err(error) = self.process.write_json(&json!({"id":id,"method":method,"params":params})).await {
                let control = self.control.lock().unwrap();
                return Err(match &control.failure {
                    Some(failure) => anyhow!("{}", failure.message),
                    None => error,
                });
            }
            tokio::select! {
                _ = self.cancel.cancelled() => Err(anyhow!("Codex {method} canceled; request acceptance may be ambiguous")),
                r = tokio::time::timeout(timeout,rx) => match r { Ok(Ok(v))=>v, Ok(Err(_))=>Err(anyhow!("Codex {method} disconnected")), Err(_)=>Err(anyhow!("Codex {method} timed out; request acceptance may be ambiguous")) }
            }
        }.await;
        self.control.lock().unwrap().pending.remove(&id);
        result
    }
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.process
            .write_json(&json!({"method":method,"params":params}))
            .await
    }
    pub async fn reply(&self, id: Value, result: Value) -> Result<()> {
        self.process
            .write_json(&json!({"id":id,"result":result}))
            .await
    }
    pub async fn reject(&self, id: Value, message: &str) -> Result<()> {
        self.process
            .write_json(&json!({"id":id,"error":{"code":-32601,"message":message}}))
            .await
    }
    pub async fn initialize(&self) -> Result<Value> {
        let value = crate::providers::registry::catalog::refresh_activity("initialize",
            self.request_timeout("initialize",json!({"clientInfo":{"name":"codex_app_server_daemon","title":"Codex App Server Daemon","version":"0.0.0"},"capabilities":{"experimentalApi":true,"mcpServerOpenaiFormElicitation":true}}),Duration::from_secs(30))
        ).await?;
        self.notify("initialized", json!({})).await?;
        Ok(value)
    }
    pub async fn close(&self) -> Result<()> {
        self.fail("Codex app-server is closed".to_owned(), false);
        self.cancel.cancel();
        let shutdown = self.process.close_stdin().await;
        self.process.terminate(Duration::from_secs(2)).await?;
        shutdown
    }
}

/// A session may replace a dead connection, never replay a request that was
/// already written. Each operation pins exactly one transport generation.
pub struct SessionTransport {
    current: Mutex<Arc<Transport>>,
    pub cancel: CancellationToken,
}
impl SessionTransport {
    pub fn new(current: Arc<Transport>, cancel: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(current),
            cancel: cancel.child_token(),
        })
    }
    pub fn current(&self) -> Arc<Transport> {
        self.current.lock().unwrap().clone()
    }
    pub fn is_closed(&self) -> bool {
        self.current().control.lock().unwrap().failure.is_some()
    }
    pub fn is_current(&self, transport: &Arc<Transport>) -> bool {
        Arc::ptr_eq(&self.current(), transport)
    }
    pub fn replace(&self, transport: Arc<Transport>) {
        *self.current.lock().unwrap() = transport;
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.current().request(method, params).await
    }
    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.current()
            .request_timeout(method, params, timeout)
            .await
    }
    pub async fn reply(&self, id: Value, result: Value) -> Result<()> {
        self.current().reply(id, result).await
    }
    pub async fn reject(&self, id: Value, message: &str) -> Result<()> {
        self.current().reject(id, message).await
    }
    pub async fn close(&self) -> Result<()> {
        self.cancel.cancel();
        self.current().close().await
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn codex_preserves_framing_failure_for_eof_and_reconnect() {
        let (transport, mut receiver) = Transport::spawn(
            CommandSpec {
                program: "/bin/sh".into(),
                args: vec!["-c".into(), r#"printf '%s' '{"method":"test"}'"#.into()],
                cwd: None,
                env: Default::default(),
                clear_env: true,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
        let error = transport
            .request("initialize", json!({}))
            .await
            .unwrap_err();
        let event = transport.take_close_event().unwrap();
        assert_eq!(event["params"]["message"], error.to_string());
        assert_eq!(event["params"]["framingFailure"], true);
        assert!(transport.take_close_event().is_none());
        transport.close().await.unwrap();
    }
}
