use crate::providers::jsonl::{LineError, LineReader};
use anyhow::{Result, anyhow, bail};
use paseo_client::process::{CommandSpec, ManagedProcess};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Control {
    pending: HashMap<String, oneshot::Sender<Result<Value>>>,
    failure: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("Invalid Claude stream-json frame: {0}")]
pub(super) struct StdoutFrameError(pub(super) LineError);

/// Claude's control channel is independent of the event consumer. Replies never
/// wait on permission resolution or on a caller holding a session-state lock.
pub(super) struct Transport {
    pub process: Arc<ManagedProcess>,
    pub generation: String,
    control: Mutex<Control>,
    stop: CancellationToken,
    healthy: AtomicBool,
    incoming: AtomicUsize,
    controls: AtomicUsize,
}

impl Transport {
    pub async fn spawn(
        command: CommandSpec,
        owner: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Result<Value>>)> {
        let (process, stdout) = ManagedProcess::spawn(command).await?;
        let this = Arc::new(Self {
            process,
            generation: uuid::Uuid::new_v4().to_string(),
            control: Mutex::new(Control::default()),
            stop: owner.child_token(),
            healthy: AtomicBool::new(true),
            incoming: AtomicUsize::new(0),
            controls: AtomicUsize::new(0),
        });
        let cleanup_stop = this.stop.clone();
        let cleanup_process = this.process.clone();
        tokio::spawn(async move {
            cleanup_stop.cancelled().await;
            let _ = cleanup_process.terminate(Duration::from_secs(2)).await;
        });
        let (tx, rx) = mpsc::channel(256);
        let weak = Arc::downgrade(&this);
        let process = this.process.clone();
        let exited = weak.clone();
        tokio::spawn(async move {
            let _ = process.wait_status().await;
            if let Some(transport) = exited.upgrade() {
                transport.healthy.store(false, Ordering::Release);
            }
        });
        let stop = this.stop.clone();
        tokio::spawn(async move {
            let mut reader = LineReader::new(stdout);
            let failure = loop {
                let read = tokio::select! {
                    _ = stop.cancelled() => break anyhow!("Claude transport closed"),
                    read = reader.next_line() => read,
                };
                match read {
                    Ok(false) => break anyhow!("Claude process output closed"),
                    Err(LineError::Io(error)) => break anyhow!("Reading Claude output: {error}"),
                    Err(error) => break StdoutFrameError(error).into(),
                    Ok(true) => {}
                }
                if reader.line().iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                let message: Value = match serde_json::from_slice(reader.line()) {
                    Ok(value) => value,
                    Err(error) => break anyhow!("Invalid Claude stream-json frame: {error}"),
                };
                reader.clear_line();
                if message["type"].as_str().is_none() {
                    break anyhow!("Claude stream-json frame has no type");
                }
                let Some(transport) = weak.upgrade() else {
                    return;
                };
                if message["type"] == "control_response" {
                    let response = &message["response"];
                    if let Some(id) = response["request_id"].as_str() {
                        let sender = transport.control.lock().unwrap().pending.remove(id);
                        if let Some(sender) = sender {
                            let result = if response["subtype"] == "error" {
                                Err(anyhow!(
                                    "{}",
                                    response["error"]
                                        .as_str()
                                        .unwrap_or("Claude control request failed")
                                ))
                            } else {
                                Ok(response["response"].clone())
                            };
                            let _ = sender.send(result);
                        }
                    }
                    continue;
                }
                transport.incoming.fetch_add(1, Ordering::AcqRel);
                drop(transport);
                let sent = tokio::select! {
                    _ = stop.cancelled() => false,
                    result = tx.send(Ok(message)) => result.is_ok(),
                };
                if !sent {
                    break anyhow!("Claude event consumer closed");
                }
            };
            drop(reader);
            let process = weak.upgrade().map(|transport| {
                transport.fail(&failure.to_string());
                transport.process.clone()
            });
            tokio::join!(
                async {
                    tokio::select! { _ = stop.cancelled() => {}, _ = tx.send(Err(failure)) => {} }
                },
                async {
                    if let Some(process) = process.filter(|_| !stop.is_cancelled()) {
                        let _ = process.terminate(Duration::from_secs(2)).await;
                    }
                },
            );
        });
        Ok((this, rx))
    }

    fn fail(&self, failure: &str) {
        self.healthy.store(false, Ordering::Release);
        let mut control = self.control.lock().unwrap();
        if control.failure.is_none() {
            control.failure = Some(failure.to_owned());
            for (_, sender) in control.pending.drain() {
                let _ = sender.send(Err(anyhow!("{failure}")));
            }
        }
    }

    fn control_error(&self, default: anyhow::Error) -> anyhow::Error {
        let control = self.control.lock().unwrap();
        match &control.failure {
            Some(failure) => anyhow!("{failure}"),
            None => default,
        }
    }
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire) && !self.stop.is_cancelled()
    }

    pub fn has_incoming_events(&self) -> bool {
        self.incoming.load(Ordering::Acquire) != 0
    }

    pub fn finish_event(&self) {
        self.incoming.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn has_pending_requests(&self) -> bool {
        self.controls.load(Ordering::Acquire) != 0
    }

    pub async fn request(&self, request: Value, timeout: Duration) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut control = self.control.lock().unwrap();
            if let Some(error) = &control.failure {
                bail!("{error}");
            }
            if self.stop.is_cancelled() {
                bail!("Claude transport is closed");
            }
            control.pending.insert(id.clone(), tx);
        }
        struct Pending<'a> {
            transport: &'a Transport,
            id: String,
            completed: bool,
        }
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                if !self.completed {
                    self.transport.healthy.store(false, Ordering::Release);
                }
                self.transport.controls.fetch_sub(1, Ordering::AcqRel);
                self.transport
                    .control
                    .lock()
                    .unwrap()
                    .pending
                    .remove(&self.id);
            }
        }
        self.controls.fetch_add(1, Ordering::AcqRel);
        let mut pending = Pending {
            transport: self,
            id: id.clone(),
            completed: false,
        };
        let operation = async {
            self.process
                .write_json(&json!({"type":"control_request","request_id":id,"request":request}))
                .await
                .map_err(|error| {
                    self.healthy.store(false, Ordering::Release);
                    self.control_error(error)
                })?;
            rx.await.map_err(|_| {
                self.healthy.store(false, Ordering::Release);
                anyhow!("Claude control reply channel closed")
            })?
        };
        let result = tokio::select! {
            _ = self.stop.cancelled() => {
                self.healthy.store(false, Ordering::Release);
                Err(self.control_error(anyhow!("Claude transport closed during control request")))
            },
            result = tokio::time::timeout(timeout, operation) => {
                result.map_err(|_| {
                    self.healthy.store(false, Ordering::Release);
                    anyhow!("Claude control request timed out; mutation outcome is unknown")
                })?
            }
        };
        pending.completed = true;
        result
    }

    pub async fn reply(&self, id: &str, response: Result<Value>) -> Result<()> {
        let response = match response {
            Ok(response) => json!({"subtype":"success","request_id":id,"response":response}),
            Err(error) => json!({"subtype":"error","request_id":id,"error":error.to_string()}),
        };
        self.process
            .write_json(&json!({"type":"control_response","response":response}))
            .await
    }

    pub async fn close(&self) -> Result<()> {
        self.fail("Claude session closed");
        self.stop.cancel();
        let _ = self.process.close_stdin().await;
        self.process.terminate(Duration::from_secs(2)).await
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::providers::jsonl::MAX_FRAME_BYTES;

    async fn assert_protocol_failure(script: &str, reason: &str) {
        let (transport, mut events) = Transport::spawn(
            CommandSpec {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    script.into(),
                    "claude-fixture".into(),
                    (MAX_FRAME_BYTES / (1024 * 1024)).to_string(),
                ],
                cwd: None,
                env: Default::default(),
                clear_env: false,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(15), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.is::<StdoutFrameError>());
        assert!(error.to_string().contains(reason), "{error}");
        assert!(!transport.is_healthy());
        assert_eq!(
            transport
                .request(json!({"subtype":"initialize"}), Duration::from_secs(1))
                .await
                .unwrap_err()
                .to_string(),
            error.to_string()
        );
        tokio::time::timeout(Duration::from_secs(5), transport.process.wait_status())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn oversized_stdout_ends_provider_even_with_pipe_still_open() {
        assert_protocol_failure(
            r#"dd if=/dev/zero bs=1048576 count="$1" 2>/dev/null; read -r hold"#,
            &format!("JSONL stdout frame exceeds {MAX_FRAME_BYTES} bytes"),
        )
        .await;
    }

    #[tokio::test]
    async fn unterminated_stdout_ends_provider_even_if_partial_json_is_valid() {
        assert_protocol_failure(
            r#"printf '%s' '{"type":"result"}'"#,
            "stdout ended with an unterminated frame",
        )
        .await;
    }
}
