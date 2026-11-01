use paseo_client::process::{CommandSpec, ManagedProcess, WriteError};
use anyhow::{Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{mpsc, oneshot, watch},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const MAX_REASSEMBLED: usize = 64 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
struct Assembly {
    id: String,
    count: usize,
    length: usize,
    index: usize,
    bytes: Vec<u8>,
}
#[derive(Default)]
struct Decoder {
    assembly: Option<Assembly>,
}
impl Decoder {
    fn decode(&mut self, line: &[u8]) -> Result<Option<Value>> {
        let frame: Value = serde_json::from_slice(line)?;
        if !frame.is_object() {
            return Ok(None);
        }
        if frame["type"] != "rpc_chunk" {
            return Ok(Some(frame));
        }
        let result = self.chunk(&frame);
        if result.is_err() {
            self.assembly = None;
        }
        result
    }
    fn chunk(&mut self, frame: &Value) -> Result<Option<Value>> {
        let id = frame["chunkId"]
            .as_str()
            .filter(|s| !s.is_empty() && s.encode_utf16().count() <= 128)
            .ok_or_else(|| anyhow!("invalid-chunk"))?;
        let index = frame["index"]
            .as_u64()
            .ok_or_else(|| anyhow!("invalid-chunk"))? as usize;
        let count = frame["count"]
            .as_u64()
            .ok_or_else(|| anyhow!("invalid-chunk"))? as usize;
        let length = frame["byteLength"]
            .as_u64()
            .ok_or_else(|| anyhow!("invalid-chunk"))? as usize;
        let data = frame["data"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 349528)
            .ok_or_else(|| anyhow!("invalid-base64"))?;
        if !(2..=256).contains(&count)
            || index >= count
            || !(MAX_FRAME..=MAX_REASSEMBLED).contains(&length)
        {
            bail!("invalid-chunk");
        }
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| anyhow!("invalid-base64"))?;
        if bytes.len() > 256 * 1024 || STANDARD.encode(&bytes) != data {
            bail!("invalid-base64");
        }
        if self.assembly.is_none() {
            if index != 0 {
                bail!("out-of-order-chunk");
            }
            self.assembly = Some(Assembly {
                id: id.into(),
                count,
                length,
                index: 0,
                bytes: Vec::new(),
            });
        }
        let assembly = self.assembly.as_mut().unwrap();
        if assembly.id != id
            || assembly.count != count
            || assembly.length != length
            || assembly.index != index
        {
            bail!("out-of-order-chunk");
        }
        if assembly.bytes.len() + bytes.len() > length {
            bail!("chunk-length-mismatch");
        }
        assembly.bytes.extend(bytes);
        assembly.index += 1;
        if assembly.index != count {
            return Ok(None);
        }
        let assembly = self.assembly.take().unwrap();
        if assembly.bytes.len() != length {
            bail!("chunk-length-mismatch");
        }
        let frame: Value = serde_json::from_slice(&assembly.bytes)
            .map_err(|_| anyhow!("invalid-chunk-payload"))?;
        if !frame.is_object() {
            bail!("invalid-chunk-payload");
        }
        Ok(Some(frame))
    }
}

#[derive(Debug)]
pub struct CommandError(pub String);
impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for CommandError {}

/// Transport failure evidence, not a provider acceptance/rejection. Once stdin
/// I/O starts, retrying a mutation can execute it twice even if no ACK arrived.
#[derive(Debug, thiserror::Error)]
#[error("JSONL request {request_id} failed (may_have_been_sent={may_have_been_sent}): {source}")]
pub struct RequestError {
    pub request_id: String,
    pub may_have_been_sent: bool,
    #[source]
    pub source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

struct PendingRequest<'a> {
    pending: &'a Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    id: &'a str,
}
impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(self.id);
    }
}

fn request_failure(id: &str, source: anyhow::Error, may_have_been_sent: bool) -> anyhow::Error {
    if source.is::<CommandError>() {
        // An explicit native response is different from a lost transport ACK.
        source
    } else {
        RequestError { request_id: id.to_owned(), may_have_been_sent, source: source.into() }.into()
    }
}
type Reply = Result<Value>;
pub struct Transport {
    pub process: Arc<ManagedProcess>,
    pending: Mutex<HashMap<String, oneshot::Sender<Reply>>>,
    sequence: AtomicU64,
    closed: AtomicBool,
    exited: AtomicBool,
    ready: watch::Sender<Option<Value>>,
    cancel: CancellationToken,
    timeout: Duration,
    host_tools: Mutex<Weak<super::host_tools::Router>>,
}
impl Transport {
    pub async fn spawn(
        spec: CommandSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<(Arc<Self>, mpsc::Receiver<Value>)> {
        if cancel.is_cancelled() {
            bail!("JSONL provider startup cancelled");
        }
        let (process, stdout) = ManagedProcess::spawn(spec).await?;
        let (ready, _) = watch::channel(None);
        let transport = Arc::new(Self {
            process,
            pending: Mutex::new(HashMap::new()),
            sequence: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            ready,
            cancel,
            timeout,
            host_tools: Mutex::new(Weak::new()),
        });
        let (events, receiver) = super::event_queue::channel(&transport, transport.cancel.clone());
        let reader = transport.clone();
        let exit_events = events.clone();
        tokio::spawn(async move {
            let mut input = BufReader::new(stdout);
            let mut line = Vec::new();
            let mut decoder = Decoder::default();
            loop {
                line.clear();
                // Legacy v1 frames have no wire size ceiling. Only the negotiated
                // v2 chunk envelope enforces the 64 MiB reassembled-frame maximum.
                let read = async {
                    let count = input.read_until(b'\n', &mut line).await?;
                    Ok::<_, std::io::Error>(count > 0 && line.last() == Some(&b'\n'))
                };
                let result = tokio::select! { _ = reader.cancel.cancelled() => break, result = read => result };
                match result {
                    Ok(false) => break,
                    Err(error) => {
                        reader.fail(&error.to_string());
                        let _ = reader.process.terminate(Duration::from_secs(2)).await;
                        break;
                    }
                    Ok(true) => {}
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match decoder.decode(&line) {
                    Ok(Some(frame)) => {
                        if frame["type"] == "ready" {
                            reader.ready.send_replace(Some(frame.clone()));
                        }
                        let pending = if frame["type"] == "response" {
                            frame["id"]
                                .as_str()
                                .and_then(|id| reader.pending.lock().unwrap().remove(id))
                        } else {
                            None
                        };
                        if let Some(reply) = pending {
                            let result = if frame["success"] == true {
                                Ok(frame.get("data").cloned().unwrap_or(Value::Null))
                            } else {
                                Err(CommandError(
                                    frame["error"]
                                        .as_str()
                                        .unwrap_or("JSONL request failed")
                                        .to_string(),
                                )
                                .into())
                            };
                            let _ = reply.send(result);
                        } else {
                            let router = reader.host_tools.lock().unwrap().upgrade();
                            if router.is_some_and(|router| router.handle(&frame)) {
                                continue;
                            }
                            tokio::select! { _ = reader.cancel.cancelled() => break, result = events.send(frame) => if result.is_err() { break; } }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "Ignoring invalid JSONL provider frame"),
                }
            }
        });
        let waiter = transport.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                result = waiter.process.wait() => result,
                _ = waiter.cancel.cancelled() => {
                    let _ = waiter.process.terminate(Duration::from_secs(2)).await;
                    waiter.process.wait().await
                }
            };
            waiter.exited.store(true, Ordering::Release);
            let message = format!(
                "JSONL process exited: {result:?}\n{}",
                waiter.process.stderr_tail()
            );
            waiter.fail(&message);
            let _ = exit_events
                .send(json!({"type":"process_exit","error":message}))
                .await;
        });
        Ok((transport, receiver))
    }
    pub(super) fn set_host_tools(&self, router: Weak<super::host_tools::Router>) {
        *self.host_tools.lock().unwrap() = router;
    }
    pub(super) fn event_queue_failed(&self, message: &str) {
        if self.closed.swap(true, Ordering::AcqRel) { return; }
        tracing::error!(%message, "Provider output spool failed");
        self.fail(message);
        let process = self.process.clone();
        tokio::spawn(async move { let _ = process.terminate(Duration::from_secs(2)).await; });
    }
    fn fail(&self, error: &str) {
        for (_, reply) in self.pending.lock().unwrap().drain() {
            let _ = reply.send(Err(anyhow!(error.to_owned())));
        }
    }
    pub async fn negotiate(&self, timeout: Duration) -> Result<()> {
        let mut ready = self.ready.subscribe();
        let frame = tokio::time::timeout(timeout, async {
            loop {
                if let Some(frame) = ready.borrow().clone() { return Ok::<_, anyhow::Error>(frame); }
                tokio::select! {
                    _ = self.cancel.cancelled() => bail!("OMP startup cancelled"),
                    _ = self.process.wait() => bail!("OMP exited before ready: {}", self.process.stderr_tail()),
                    result = ready.changed() => result.map_err(|_| anyhow!("OMP ready channel closed"))?,
                }
            }
        }).await.map_err(|_| anyhow!("Timed out waiting for OMP to become ready"))??;
        if frame["supportedProtocolVersions"]
            .as_array()
            .is_some_and(|v| v.contains(&json!(2)))
            && frame["maxFrameBytes"] == MAX_FRAME
            && frame["maxReassembledFrameBytes"] == MAX_REASSEMBLED
        {
            let reply = self
                .request(json!({"type":"negotiate_protocol","protocolVersion":2}))
                .await?;
            if reply["protocolVersion"] != 2 {
                bail!("OMP did not accept RPC protocol v2");
            }
        }
        Ok(())
    }
    pub async fn request(&self, command: Value) -> Result<Value> {
        self.request_with_timeout(command, Some(self.timeout))
            .await
            .map(|(_, value)| value)
    }
    pub async fn request_with_timeout(
        &self,
        mut command: Value,
        timeout: Option<Duration>,
    ) -> Result<(String, Value)> {
        let started = Instant::now();
        let id = format!("req_{}", self.sequence.fetch_add(1, Ordering::Relaxed));
        let deadline = timeout.map(|timeout| started.checked_add(timeout)
            .ok_or_else(|| request_failure(&id, anyhow!("JSONL request deadline exceeds clock range"), false)))
            .transpose()?;
        command["id"] = json!(id);
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().unwrap();
            if self.closed.load(Ordering::Acquire) || self.exited.load(Ordering::Acquire) {
                return Err(request_failure(&id, anyhow!("JSONL provider process is closed"), false));
            }
            // Exit drains this same map after setting exited; never install a
            // waiter behind that drain.
            pending.insert(id.clone(), sender);
        }
        let pending = PendingRequest { pending: &self.pending, id: &id };
        let result = async {
            self.send_with_deadline(&command, deadline).await.map_err(|error| {
                let attempted = error.downcast_ref::<WriteError>()
                    .is_none_or(|error| error.attempted);
                request_failure(&id, error, attempted)
            })?;
            let result = tokio::select! {
                biased;
                result = receiver => result.unwrap_or_else(|_| Err(anyhow!("JSONL request channel closed"))),
                _ = self.cancel.cancelled() => Err(anyhow!("JSONL request cancelled")),
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => Err(anyhow!("JSONL request timed out phase={}\n{}", command["type"], self.process.stderr_tail())),
            };
            result.map_err(|error| request_failure(&id, error, true))
        }.await;
        drop(pending);
        result.map(|value| (id, value))
    }
    pub async fn stop_work(&self, command: Value) -> Result<()> {
        if self.exited.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self.request(command).await;
        if self.exited.load(Ordering::Acquire) {
            Ok(())
        } else {
            result.map(|_| ())
        }
    }
    pub async fn send(&self, value: &Value) -> Result<()> {
        self.send_with_deadline(value, None).await
    }
    async fn send_with_deadline(&self, value: &Value, deadline: Option<Instant>) -> Result<()> {
        if self.closed.load(Ordering::Acquire) || self.exited.load(Ordering::Acquire) {
            return Err(WriteError {
                attempted: false, written: 0, total: 0,
                source: std::io::Error::new(std::io::ErrorKind::BrokenPipe, "JSONL process is closed"),
            }.into());
        }
        match self.process.write_json_with_deadline(value, deadline, &self.cancel).await {
            Ok(()) => Ok(()),
            Err(error) if error.downcast_ref::<WriteError>().is_some_and(|error| !error.attempted) => {
                // Waiting for another writer is not an attempted mutation and
                // cannot justify killing that writer's healthy process.
                Err(error)
            }
            Err(error) => {
                self.closed.store(true, Ordering::Release);
                self.fail(&error.to_string());
                self.cancel.cancel();
                // The partial-write guard and process supervisor own cleanup;
                // waiting here would extend the caller's absolute RPC deadline.
                Err(error)
            }
        }
    }
    pub async fn close(&self) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.fail("JSONL provider session is closed");
        self.cancel.cancel();
        self.process.close_stdin().await.ok();
        self.process.terminate(Duration::from_secs(2)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(value: &Value) -> Vec<Vec<u8>> {
        let bytes = serde_json::to_vec(value).unwrap();
        let count = bytes.len().div_ceil(256 * 1024);
        bytes.chunks(256*1024).enumerate().map(|(index,part)|serde_json::to_vec(&json!({"type":"rpc_chunk","chunkId":"fixture","index":index,"count":count,"byteLength":bytes.len(),"data":STANDARD.encode(part)})).unwrap()).collect()
    }

    #[test]
    fn reassembles_large_native_frames_across_utf8_boundaries() {
        let expected = json!({"type":"response","id":"req_41","success":true,"data":{"text":"a€".repeat(MAX_FRAME/3)}});
        let frames = chunks(&expected);
        let mut decoder = Decoder::default();
        for frame in &frames[..frames.len() - 1] {
            assert!(decoder.decode(frame).unwrap().is_none());
        }
        assert_eq!(
            decoder.decode(frames.last().unwrap()).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn nonchunk_frames_and_malformed_json_do_not_discard_pending_assembly() {
        let expected = json!({"type":"message_update","text":"x".repeat(MAX_FRAME)});
        let frames = chunks(&expected);
        let mut decoder = Decoder::default();
        assert!(decoder.decode(&frames[0]).unwrap().is_none());
        assert!(decoder.decode(b"{broken").is_err());
        assert_eq!(
            decoder.decode(b"{\"type\":\"ready\"}").unwrap(),
            Some(json!({"type":"ready"}))
        );
        for frame in &frames[1..frames.len() - 1] {
            assert!(decoder.decode(frame).unwrap().is_none());
        }
        assert_eq!(
            decoder.decode(frames.last().unwrap()).unwrap(),
            Some(expected)
        );
    }

    #[test]
    fn invalid_sequence_resets_assembly_without_poisoning_next_message() {
        let expected =
            json!({"type":"response","id":"req_2","success":true,"data":"x".repeat(MAX_FRAME)});
        let frames = chunks(&expected);
        let mut decoder = Decoder::default();
        assert!(decoder.decode(&frames[1]).is_err());
        assert!(decoder.decode(&frames[0]).unwrap().is_none());
        assert!(decoder.decode(&frames[2]).is_err());
        for frame in &frames[..frames.len() - 1] {
            assert!(decoder.decode(frame).unwrap().is_none());
        }
        assert_eq!(
            decoder.decode(frames.last().unwrap()).unwrap(),
            Some(expected)
        );
    }
}

#[cfg(all(test, unix))]
mod request_tests {
    use super::*;

    async fn fixture(script: &str) -> (Arc<Transport>, mpsc::Receiver<Value>) {
        Transport::spawn(CommandSpec {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            cwd: None, env: Default::default(), clear_env: false,
        }, Duration::from_secs(2), CancellationToken::new()).await.unwrap()
    }

    #[tokio::test]
    async fn request_deadline_includes_blocked_stdin_and_cleans_waiter() {
        let (transport, _events) = fixture("sleep 30").await;
        let command = json!({"type":"prompt", "text":"x".repeat(2 * 1024 * 1024)});
        let error = tokio::time::timeout(Duration::from_secs(3),
            transport.request_with_timeout(command, Some(Duration::from_millis(300))))
            .await.expect("the deadline must include a full stdin pipe").unwrap_err();
        let error = error.downcast_ref::<RequestError>().unwrap();
        assert!(error.may_have_been_sent);
        assert_eq!(error.request_id, "req_1");
        assert_eq!(error.source.downcast_ref::<WriteError>().unwrap().source.kind(),
            std::io::ErrorKind::TimedOut);
        assert!(transport.pending.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(3), transport.process.wait())
            .await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn dropping_request_removes_waiter_without_breaking_next_native_reply() {
        let (transport, mut events) = fixture(
            "IFS= read -r first; printf '{\"type\":\"input_seen\"}\\n'; \
             IFS= read -r second; printf '{\"type\":\"response\",\"id\":\"req_2\",\"success\":true,\"data\":{\"value\":42}}\\n'; \
             cat >/dev/null"
        ).await;
        let owner = transport.clone();
        let request = tokio::spawn(async move {
            owner.request_with_timeout(json!({"type":"prompt"}), None).await
        });
        assert_eq!(tokio::time::timeout(Duration::from_secs(3), events.recv())
            .await.unwrap().unwrap()["type"], "input_seen");
        assert_eq!(transport.pending.lock().unwrap().len(), 1);
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(transport.pending.lock().unwrap().is_empty());
        assert_eq!(transport.request(json!({"type":"get_state"})).await.unwrap(),
            json!({"value":42}));
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn native_rejection_is_distinct_from_transport_outcome_unknown() {
        let (transport, _events) = fixture(
            "IFS= read -r first; printf '{\"type\":\"response\",\"id\":\"req_1\",\"success\":false,\"error\":\"denied\"}\\n'; cat >/dev/null"
        ).await;
        let error = transport.request(json!({"type":"prompt"})).await.unwrap_err();
        assert_eq!(error.downcast_ref::<CommandError>().unwrap().0, "denied");
        assert!(error.downcast_ref::<RequestError>().is_none());
        transport.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_request_before_io_is_not_delivery_ambiguity() {
        let (transport, _events) = fixture("sleep 30").await;
        transport.cancel.cancel();
        let error = transport.request(json!({"type":"prompt"})).await.unwrap_err();
        assert!(!error.downcast_ref::<RequestError>().unwrap().may_have_been_sent);
        assert!(transport.pending.lock().unwrap().is_empty());
        tokio::time::timeout(Duration::from_secs(3), transport.process.wait())
            .await.unwrap().unwrap();
    }
}
