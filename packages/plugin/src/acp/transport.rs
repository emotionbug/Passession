use crate::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    io,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    process::{ChildStdin, Command},
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const PENDING_LIMIT: usize = 256;
const EVENT_LIMIT: usize = 16;
const WRITE_LIMIT: usize = 8;
const JOIN_TIMEOUT: Duration = Duration::from_secs(7);
#[cfg(unix)]
const PROCESS_GRACE: Duration = Duration::from_secs(1);

pub type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// Caller-owned, newline-delimited JSON-RPC IO. The adapter owns both halves and
/// cancels and joins the optional connector task when the connection closes.
pub struct AcpStream {
    pub reader: Reader,
    pub writer: Writer,
    pub cancellation: CancellationToken,
    pub task: Option<JoinHandle<()>>,
}

impl AcpStream {
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            cancellation: CancellationToken::new(),
            task: None,
        }
    }
}

// Closing stdin also stops a child when a stream is dropped before Rpc::start.
struct ProcessInput {
    stdin: Option<ChildStdin>,
    cancellation: CancellationToken,
}

impl Drop for ProcessInput {
    fn drop(&mut self) {
        self.stdin.take();
        self.cancellation.cancel();
    }
}

impl AsyncWrite for ProcessInput {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_write(cx, bytes),
            None => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "ACP stdin closed",
            ))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.stdin.as_mut() {
            Some(stdin) => Pin::new(stdin).poll_flush(cx),
            None => Poll::Ready(Ok(())),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        self.stdin.take();
        self.cancellation.cancel();
        Poll::Ready(Ok(()))
    }
}

pub(super) async fn spawn(
    command: &str,
    args: &[String],
    env: Option<&BTreeMap<String, String>>,
    context: &crate::Context,
    cancellation: CancellationToken,
) -> Result<AcpStream> {
    ensure!(!cancellation.is_cancelled(), "ACP process launch canceled");
    let mut command = Command::new(command);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = env {
        command.env_clear().envs(env);
    }
    let mut child = command.spawn()?;
    // These handles are guaranteed by the piped configuration above.
    let stdin = child.stdin.take().expect("piped ACP stdin");
    let stdout = child.stdout.take().expect("piped ACP stdout");
    let mut stderr = child.stderr.take().expect("piped ACP stderr");
    let context = context.clone();
    let cleanup_context = context.clone();
    let stop = cancellation.clone();
    let task = tokio::spawn(async move {
        let stderr_stop = CancellationToken::new();
        let drain_stop = stderr_stop.clone();
        let mut drain = OwnedTask::new(tokio::spawn(async move {
            let mut buffer = [0; 8192];
            loop {
                let count = tokio::select! {
                    _ = drain_stop.cancelled() => break,
                    read = stderr.read(&mut buffer) => match read {
                        Ok(0) | Err(_) => break,
                        Ok(count) => count,
                    },
                };
                // Logging failure must not leave the child's stderr pipe full.
                tokio::select! {
                    _ = drain_stop.cancelled() => break,
                    _ = context.log(String::from_utf8_lossy(&buffer[..count])) => {},
                }
            }
        }));
        tokio::select! {
            _ = child.wait() => {},
            _ = stop.cancelled() => {
                if let Err(error) = terminate_process(&mut child).await {
                    let message = format!("ACP process cleanup failed: {error}");
                    // Even if the platform termination helper fails, retain
                    // ownership and reap the direct child before returning.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = cleanup_context.log(message).await;
                }
            },
        }
        stderr_stop.cancel();
        let _ = drain.join().await;
    });
    Ok(AcpStream {
        reader: Box::new(stdout),
        writer: Box::new(ProcessInput {
            stdin: Some(stdin),
            cancellation: cancellation.clone(),
        }),
        cancellation,
        task: Some(task),
    })
}

async fn terminate_process(child: &mut tokio::process::Child) -> Result<()> {
    let Some(pid) = child.id() else {
        return Ok(());
    };
    #[cfg(unix)]
    {
        // The owned Child remains unreaped, so its PID cannot be reused while
        // sending SIGTERM. Match the old ACP adapter's graceful shutdown.
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error.into());
            }
        }
        if let Ok(status) = tokio::time::timeout(PROCESS_GRACE, child.wait()).await {
            status?;
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        // Windows command launchers can own descendants. Preserve the former
        // taskkill /T contract instead of terminating only the launcher.
        let mut kill = Command::new("taskkill.exe");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut killer = kill.spawn()?;
        let status = match tokio::time::timeout(Duration::from_secs(5), killer.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = killer.start_kill();
                let _ = killer.wait().await;
                bail!("Windows ACP process-tree termination timed out");
            }
        };
        ensure!(
            status.success() || status.code() == Some(128),
            "Windows ACP process-tree termination failed: {status}"
        );
        child.wait().await?;
        return Ok(());
    }
    #[cfg(not(windows))]
    {
        child.start_kill()?;
        child.wait().await?;
        Ok(())
    }
}

// A canceled close future must not detach its supervisor or connector tasks.
struct OwnedTask<T> {
    handle: JoinHandle<T>,
}

impl<T> OwnedTask<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }

    async fn join(&mut self) -> Result<T> {
        match tokio::time::timeout(JOIN_TIMEOUT, &mut self.handle).await {
            Ok(result) => Ok(result?),
            Err(_) => {
                self.handle.abort();
                // Async task abortion is cooperative; do not wait forever for
                // a connector which blocks a runtime thread in its poll method.
                let _ = tokio::time::timeout(JOIN_TIMEOUT, &mut self.handle).await;
                bail!("ACP task did not stop within shutdown deadline")
            }
        }
    }
}

impl<T> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

type Pending = oneshot::Sender<Result<Value>>;
struct State {
    pending: HashMap<String, Pending>,
    failure: Option<String>,
    // Reserving one queue slot guarantees a terminal error never waits behind
    // a full event queue while the runtime is itself waiting for a write.
    terminal: Option<mpsc::OwnedPermit<Result<Value>>>,
}

struct Shared {
    state: Mutex<State>,
    cancellation: CancellationToken,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fail(&self, error: impl std::fmt::Display) {
        let mut state = self.state();
        if state.failure.is_none() {
            let message = error.to_string();
            for (_, sender) in state.pending.drain() {
                let _ = sender.send(Err(anyhow!("{message}")));
            }
            if let Some(terminal) = state.terminal.take() {
                terminal.send(Err(anyhow!("{message}")));
            }
            state.failure = Some(message);
        }
        self.cancellation.cancel();
    }
}

struct PendingGuard<'a> {
    shared: &'a Shared,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared.state().pending.remove(&self.id);
    }
}

struct Outgoing {
    bytes: Vec<u8>,
    completion: oneshot::Sender<Result<()>>,
}

pub(super) struct Rpc {
    shared: Arc<Shared>,
    outgoing: mpsc::Sender<Outgoing>,
    sequence: AtomicU64,
    supervisor: AsyncMutex<Option<JoinHandle<Result<()>>>>,
}

impl Drop for Rpc {
    fn drop(&mut self) {
        // The supervisor retains cleanup ownership after the last Rpc is
        // dropped; aborting it here would interrupt child kill-and-wait.
        self.shared.fail("ACP connection dropped");
    }
}

impl Rpc {
    pub(super) async fn start(
        stream: AcpStream,
        parent: CancellationToken,
    ) -> (Arc<Self>, mpsc::Receiver<Result<Value>>) {
        let AcpStream {
            reader,
            writer,
            cancellation: connector_stop,
            task,
        } = stream;
        let (events, receiver) = mpsc::channel(EVENT_LIMIT + 1);
        let terminal = events
            .clone()
            .reserve_owned()
            .await
            .expect("new ACP event queue");
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                pending: HashMap::new(),
                failure: None,
                terminal: Some(terminal),
            }),
            cancellation: CancellationToken::new(),
        });
        let (outgoing, incoming) = mpsc::channel(WRITE_LIMIT);
        let read_shared = shared.clone();
        let reader = OwnedTask::new(tokio::spawn(async move {
            let _stop = read_shared.cancellation.clone().drop_guard();
            read_loop(reader, events, &read_shared).await;
        }));
        let write_shared = shared.clone();
        let writer = OwnedTask::new(tokio::spawn(async move {
            let _stop = write_shared.cancellation.clone().drop_guard();
            write_loop(writer, incoming, &write_shared).await;
        }));
        let supervision = shared.clone();
        let supervisor = tokio::spawn(async move {
            let mut reader = reader;
            let mut writer = writer;
            let mut connector = task.map(OwnedTask::new);
            tokio::select! {
                _ = parent.cancelled() => supervision.fail("ACP parent canceled"),
                _ = connector_stop.cancelled() => supervision.fail("ACP connector canceled"),
                _ = supervision.cancellation.cancelled() => supervision.fail("ACP connection closed"),
            }
            connector_stop.cancel();
            // Join concurrently: stream halves must drop while the connector is
            // shutting down, not after waiting for its process to exit.
            let (read, write, connector) = tokio::join!(reader.join(), writer.join(), async {
                match connector.as_mut() {
                    Some(task) => task.join().await,
                    None => Ok(()),
                }
            });
            read?;
            write?;
            connector?;
            Ok(())
        });
        (
            Arc::new(Self {
                shared,
                outgoing,
                sequence: AtomicU64::new(1),
                supervisor: AsyncMutex::new(Some(supervisor)),
            }),
            receiver,
        )
    }

    pub(super) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let id = self
            .sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow!("ACP request ID space exhausted"))?
            .to_string();
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.shared.state();
            ensure!(
                state.failure.is_none() && !self.shared.cancellation.is_cancelled(),
                "ACP connection closed"
            );
            ensure!(
                state.pending.len() < PENDING_LIMIT,
                "ACP concurrent request limit exceeded"
            );
            state.pending.insert(id.clone(), sender);
        }
        let _pending = PendingGuard {
            shared: &self.shared,
            id,
        };
        let request = async {
            self.send(json!({"jsonrpc":"2.0", "id":_pending.id, "method":method, "params":params}))
                .await?;
            tokio::select! {
                biased;
                response = receiver => response.map_err(|_| anyhow!("ACP response channel closed"))?,
                _ = self.shared.cancellation.cancelled() => Err(anyhow!("ACP connection closed")),
            }
        };
        match timeout {
            Some(timeout) => tokio::time::timeout(timeout, request)
                .await
                .map_err(|_| anyhow!("ACP request {method} timed out"))?,
            None => request.await,
        }
    }

    pub(super) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(json!({"jsonrpc":"2.0", "method":method, "params":params}))
            .await
    }

    pub(super) async fn reply(&self, id: Value, result: Result<Value>) -> Result<()> {
        ensure!(valid_id(&id), "Invalid ACP JSON-RPC response ID");
        let message = match result {
            Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0", "id":id, "error":{"code":-32603, "message":error.to_string()}})
            }
        };
        self.send(message).await
    }

    /// Called by the runtime's sole event pump, preserving notification order.
    /// Late replies to timed-out or canceled requests are deliberately ignored.
    pub(super) fn resolve(&self, message: &Value) -> Result<()> {
        validate_frame(message)?;
        ensure!(
            message.get("method").is_none(),
            "ACP request is not a response"
        );
        let Some(id) = message.get("id").and_then(Value::as_str) else {
            return Ok(());
        };
        let sender = self.shared.state().pending.remove(id);
        if let Some(sender) = sender {
            let result = if let Some(error) = message.get("error") {
                Err(anyhow!(
                    "ACP JSON-RPC error {}: {}{}",
                    error["code"],
                    error["message"].as_str().unwrap_or("Unknown error"),
                    error
                        .get("data")
                        .map(|data| format!(" ({data})"))
                        .unwrap_or_default()
                ))
            } else {
                Ok(message["result"].clone())
            };
            let _ = sender.send(result);
        }
        Ok(())
    }

    async fn send(&self, message: Value) -> Result<()> {
        ensure!(
            !self.shared.cancellation.is_cancelled(),
            "ACP connection closed"
        );
        let bytes = encode_frame(&message)?;
        let (completion, written) = oneshot::channel();
        // The writer owns each admitted frame. Dropping this future cannot leave
        // a partial JSON record followed by another request on the same stream.
        tokio::select! {
            biased;
            _ = self.shared.cancellation.cancelled() => bail!("ACP connection closed"),
            sent = self.outgoing.send(Outgoing { bytes, completion }) => {
                sent.map_err(|_| anyhow!("ACP writer closed"))?;
            },
        }
        tokio::select! {
            biased;
            result = written => result.map_err(|_| anyhow!("ACP writer closed"))?,
            _ = self.shared.cancellation.cancelled() => Err(anyhow!("ACP connection closed")),
        }
    }

    pub(super) fn cancellation(&self) -> CancellationToken {
        self.shared.cancellation.clone()
    }

    pub(super) async fn close(&self) -> Result<()> {
        self.shared.fail("ACP connection closed");
        // Keep ownership in the mutex across the await: cancellation of close
        // leaves the supervisor available to a subsequent close call.
        let mut supervisor = self.supervisor.lock().await;
        if let Some(task) = supervisor.as_mut() {
            // The supervisor already bounds each concurrent child join. Do not
            // give it a shorter deadline than the cleanup it is supervising.
            let result = task.await;
            supervisor.take();
            result??;
        }
        Ok(())
    }
}

async fn read_loop(reader: Reader, events: mpsc::Sender<Result<Value>>, shared: &Shared) {
    let mut reader = BufReader::new(reader);
    loop {
        let message = tokio::select! {
            biased;
            _ = shared.cancellation.cancelled() => return,
            message = read_frame(&mut reader) => message,
        };
        let message = match message {
            Ok(Some(message)) => message,
            Ok(None) => {
                shared.fail("ACP stream reached EOF");
                return;
            }
            Err(error) => {
                shared.fail(error);
                return;
            }
        };
        tokio::select! {
            biased;
            _ = shared.cancellation.cancelled() => return,
            sent = events.send(Ok(message)) => {
                if sent.is_err() { shared.fail("ACP event receiver closed"); return; }
            },
        }
    }
}

async fn write_loop(mut writer: Writer, mut incoming: mpsc::Receiver<Outgoing>, shared: &Shared) {
    loop {
        let message = tokio::select! {
            biased;
            _ = shared.cancellation.cancelled() => return,
            message = incoming.recv() => match message {
                Some(message) => message,
                None => { shared.fail("ACP writer queue closed"); return; },
            },
        };
        let result: Result<()> = tokio::select! {
            biased;
            _ = shared.cancellation.cancelled() => return,
            result = async {
                writer.write_all(&message.bytes).await?;
                writer.flush().await?;
                Ok(())
            } => result,
        };
        if let Err(error) = &result {
            shared.fail(format!("ACP write failed: {error}"));
        }
        let failed = result.is_err();
        let _ = message.completion.send(result);
        if failed {
            return;
        }
    }
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            ensure!(bytes.is_empty(), "Truncated ACP JSON-RPC frame");
            return Ok(None);
        }
        let length = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |index| index + 1);
        ensure!(
            length <= FRAME_LIMIT - bytes.len(),
            "ACP input exceeds 16 MiB frame limit"
        );
        let complete = chunk[length - 1] == b'\n';
        bytes.extend_from_slice(&chunk[..length]);
        reader.consume(length);
        if complete {
            let message = serde_json::from_slice(&bytes)?;
            validate_frame(&message)?;
            return Ok(Some(message));
        }
        ensure!(
            bytes.len() < FRAME_LIMIT,
            "ACP input exceeds 16 MiB frame limit"
        );
    }
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

fn validate_frame(message: &Value) -> Result<()> {
    ensure!(
        message.is_object() && message["jsonrpc"] == "2.0",
        "Invalid ACP JSON-RPC frame"
    );
    if let Some(id) = message.get("id") {
        ensure!(valid_id(id), "Invalid ACP JSON-RPC ID");
    }
    if let Some(method) = message.get("method") {
        ensure!(
            method.is_string() && message.get("result").is_none() && message.get("error").is_none(),
            "Invalid ACP JSON-RPC request"
        );
    } else {
        ensure!(
            message.get("id").is_some()
                && (message.get("result").is_some() != message.get("error").is_some()),
            "Invalid ACP JSON-RPC response"
        );
        if let Some(error) = message.get("error") {
            ensure!(
                error.is_object() && error["code"].is_i64() && error["message"].is_string(),
                "Invalid ACP JSON-RPC error"
            );
        }
    }
    Ok(())
}

// Enforce the limit while serializing, rather than first allocating an
// unbounded buffer for an oversized caller-supplied Value.
struct FrameBuffer(Vec<u8>);
impl io::Write for FrameBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() >= FRAME_LIMIT - self.0.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ACP output exceeds 16 MiB frame limit",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_frame(message: &Value) -> Result<Vec<u8>> {
    let mut frame = FrameBuffer(Vec::new());
    serde_json::to_writer(&mut frame, message)?;
    frame.0.push(b'\n');
    Ok(frame.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, split};

    #[cfg(unix)]
    #[test]
    #[ignore = "subprocess fixture invoked by termination tests"]
    fn termination_child_fixture() {
        let Ok(mode) = std::env::var("PASEO_ACP_TERMINATION_FIXTURE") else {
            return;
        };
        if mode == "ignore-term" {
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
        }
        println!("ACP_TERMINATION_READY");
        loop {
            std::thread::park();
        }
    }

    #[cfg(unix)]
    async fn termination_child(mode: &str) -> tokio::process::Child {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "acp::transport::tests::termination_child_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("PASEO_ACP_TERMINATION_FIXTURE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let line = output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("fixture exited before readiness");
                if line.contains("ACP_TERMINATION_READY") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        child
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_shutdown_allows_graceful_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = termination_child("default").await;
        terminate_process(&mut child).await.unwrap();
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGTERM));
        assert!(child.id().is_none(), "terminated child must be reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_shutdown_escalates_ignored_sigterm() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = termination_child("ignore-term").await;
        let started = tokio::time::Instant::now();
        terminate_process(&mut child).await.unwrap();
        assert!(started.elapsed() >= PROCESS_GRACE);
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGKILL));
        assert!(
            child.id().is_none(),
            "forcibly terminated child must be reaped"
        );
    }

    async fn next(events: &mut mpsc::Receiver<Result<Value>>) -> Value {
        tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn split_and_coalesced_frames_preserve_wire_order() {
        let (client, server) = duplex(1024);
        let (reader, writer) = split(client);
        let (rpc, mut events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        let (_server_reader, mut server_writer) = split(server);
        server_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"meth")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        server_writer
            .write_all(
                b"od\":\"first\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"second\"}\r\n",
            )
            .await
            .unwrap();
        assert_eq!(next(&mut events).await["method"], "first");
        assert_eq!(next(&mut events).await["method"], "second");
        rpc.close().await.unwrap();
    }

    #[tokio::test]
    async fn out_of_order_responses_require_the_event_pump() {
        let (client, server) = duplex(4096);
        let (reader, writer) = split(client);
        let (rpc, mut events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        let (server_reader, mut server_writer) = split(server);
        let mut server_reader = BufReader::new(server_reader);
        let first_rpc = rpc.clone();
        let first = tokio::spawn(async move {
            first_rpc
                .request("first", Value::Null, Some(Duration::from_secs(2)))
                .await
        });
        let first_message = read_frame(&mut server_reader).await.unwrap().unwrap();
        let second_rpc = rpc.clone();
        let second = tokio::spawn(async move {
            second_rpc
                .request("second", Value::Null, Some(Duration::from_secs(2)))
                .await
        });
        let second_message = read_frame(&mut server_reader).await.unwrap().unwrap();
        for (message, result) in [(&second_message, 2), (&first_message, 1)] {
            server_writer
                .write_all(
                    &encode_frame(&json!({"jsonrpc":"2.0", "id":message["id"], "result":result}))
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        let response = next(&mut events).await;
        assert_eq!(response["id"], second_message["id"]);
        assert_eq!(rpc.shared.state().pending.len(), 2);
        rpc.resolve(&response).unwrap();
        assert_eq!(second.await.unwrap().unwrap(), 2);
        assert!(!first.is_finished());
        rpc.resolve(&next(&mut events).await).unwrap();
        assert_eq!(first.await.unwrap().unwrap(), 1);
        assert!(rpc.shared.state().pending.is_empty());
        rpc.close().await.unwrap();
    }

    #[tokio::test]
    async fn eof_rejects_and_removes_pending_requests() {
        let (client, server) = duplex(1024);
        let (reader, writer) = split(client);
        let (rpc, mut events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        let request_rpc = rpc.clone();
        let request =
            tokio::spawn(async move { request_rpc.request("pending", Value::Null, None).await });
        let mut server = BufReader::new(server);
        read_frame(&mut server).await.unwrap().unwrap();
        drop(server);
        let error = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("EOF"));
        assert!(
            tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(rpc.shared.state().pending.is_empty());
        rpc.close().await.unwrap();
    }

    #[tokio::test]
    async fn canceled_request_future_removes_pending_entry() {
        let (client, server) = duplex(1024);
        let (reader, writer) = split(client);
        let (rpc, _events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        let request_rpc = rpc.clone();
        let request =
            tokio::spawn(async move { request_rpc.request("pending", Value::Null, None).await });
        let mut server = BufReader::new(server);
        read_frame(&mut server).await.unwrap().unwrap();
        assert_eq!(rpc.shared.state().pending.len(), 1);
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert!(rpc.shared.state().pending.is_empty());
        rpc.close().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_truncated_and_oversized_input_fail() {
        for bytes in [b"not json\n".as_slice(), b"{\"jsonrpc\":\"2.0\"}", b"[]\n"] {
            assert!(read_frame(&mut BufReader::new(bytes)).await.is_err());
        }
        let oversized = vec![b' '; FRAME_LIMIT];
        let error = read_frame(&mut BufReader::new(oversized.as_slice()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("16 MiB"));
        let oversized = json!({"text":"x".repeat(FRAME_LIMIT)});
        assert!(
            encode_frame(&oversized)
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
    }

    #[tokio::test]
    async fn failure_has_a_reserved_slot_in_a_full_event_queue() {
        let (client, mut server) = duplex(8192);
        let (reader, writer) = split(client);
        let (rpc, mut events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        for _ in 0..EVENT_LIMIT {
            server
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notice\"}\n")
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while events.len() != EVENT_LIMIT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        server.write_all(b"invalid\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), rpc.cancellation().cancelled())
            .await
            .unwrap();
        for _ in 0..EVENT_LIMIT {
            assert!(events.recv().await.unwrap().is_ok());
        }
        assert!(events.recv().await.unwrap().is_err());
        rpc.close().await.unwrap();
    }

    struct BrokenWriter;
    impl AsyncWrite for BrokenWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            _bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test write failure",
            )))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn writer_failure_cannot_deadlock_on_event_backpressure() {
        let (reader, mut server) = duplex(8192);
        let (rpc, mut events) = Rpc::start(
            AcpStream::new(reader, BrokenWriter),
            CancellationToken::new(),
        )
        .await;
        for _ in 0..EVENT_LIMIT {
            server
                .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notice\"}\n")
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while events.len() != EVENT_LIMIT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            rpc.request("fail", Value::Null, None),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(rpc.shared.state().pending.is_empty());
        for _ in 0..EVENT_LIMIT {
            assert!(events.recv().await.unwrap().is_ok());
        }
        assert!(
            events
                .recv()
                .await
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("write failed")
        );
        rpc.close().await.unwrap();
    }

    #[tokio::test]
    async fn pending_requests_are_bounded_and_timeout_releases_capacity() {
        let (client, server) = duplex(1024);
        let (reader, writer) = split(client);
        let (rpc, _events) =
            Rpc::start(AcpStream::new(reader, writer), CancellationToken::new()).await;
        let mut server = BufReader::new(server);
        let mut requests = Vec::new();
        for _ in 0..PENDING_LIMIT {
            let rpc = rpc.clone();
            requests.push(tokio::spawn(async move {
                rpc.request("pending", Value::Null, None).await
            }));
            read_frame(&mut server).await.unwrap().unwrap();
        }
        assert!(
            rpc.request("overflow", Value::Null, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        for request in requests {
            request.abort();
            let _ = request.await;
        }
        assert!(rpc.shared.state().pending.is_empty());
        assert!(
            rpc.request("timeout", Value::Null, Some(Duration::from_millis(1)))
                .await
                .unwrap_err()
                .to_string()
                .contains("timed out")
        );
        assert!(rpc.shared.state().pending.is_empty());
        rpc.close().await.unwrap();
    }
}
