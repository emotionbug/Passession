//! Client of `paseo desktop-host` (original daemon_service.dart). One helper per
//! app session holds daemon ownership in memory; ownership is never inferred
//! from the desktopManaged bit after a crash. A daemon this session did not
//! start is never stopped on quit.

use crate::action::PlatformError;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    sync::{oneshot, OnceCell},
};

/// Unpacked, platform-native release assets (never inside an archive).
#[derive(Debug, Clone)]
pub(crate) struct DaemonRuntime {
    pub executable: PathBuf,
    pub cli_shim_path: PathBuf,
    pub working_directory: PathBuf,
    pub environment: HashMap<String, String>,
}

impl DaemonRuntime {
    /// `PASEO_DESKTOP_RUNTIME`, else `<exe dir>/runtime/desktop-runtime.json`
    /// (`Contents/Resources/runtime/` inside a macOS bundle).
    pub fn locate() -> anyhow::Result<Self> {
        let manifest = match std::env::var_os("PASEO_DESKTOP_RUNTIME") {
            Some(path) => PathBuf::from(path),
            None => {
                let executable = std::env::current_exe()?;
                let directory = executable.parent().unwrap_or(Path::new("."));
                if cfg!(target_os = "macos") {
                    directory.join("..").join("Resources").join("runtime").join("desktop-runtime.json")
                } else {
                    directory.join("runtime").join("desktop-runtime.json")
                }
            }
        };
        let bytes = std::fs::read(&manifest).map_err(|error| {
            anyhow::anyhow!(
                "Bundled native runtime manifest is missing. Package the native Paseo runtime. ({}: {error})",
                manifest.display()
            )
        })?;
        let data: Value = serde_json::from_slice(&bytes)?;
        let base = std::path::absolute(manifest.parent().unwrap_or(Path::new(".")))?;
        let resolve = |key: &str| -> anyhow::Result<PathBuf> {
            let value = data
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow::anyhow!("Runtime manifest is missing {key}"))?;
            Ok(normalize(&base.join(value)))
        };
        let environment = data
            .get("environment")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            executable: resolve("executable")?,
            cli_shim_path: resolve("cliShimPath")?,
            working_directory: resolve("workingDirectory")?,
            environment,
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "executable": self.executable,
            "cliShimPath": self.cli_shim_path,
            "workingDirectory": self.working_directory,
        })
    }
}

/// Lexical normalization (path package `p.normalize`).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DaemonSettings {
    pub managed: bool,
    pub keep_running_after_quit: bool,
}

impl DaemonSettings {
    fn to_json(self) -> Value {
        json!({"managed": self.managed, "keepRunningAfterQuit": self.keep_running_after_quit})
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, PlatformError>>>>>;

struct Helper {
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    pending: Pending,
    exited: Arc<tokio::sync::Notify>,
    alive: Arc<AtomicBool>,
}

pub(crate) struct DaemonService {
    runtime: DaemonRuntime,
    app_version: Option<String>,
    settings: Arc<dyn Fn() -> DaemonSettings + Send + Sync>,
    helper: OnceCell<Arc<Helper>>,
    next_id: AtomicU64,
    reject_new: AtomicBool,
    closed: AtomicBool,
    closing: tokio::sync::Mutex<()>,
}

fn helper_error(code: &str, message: impl Into<String>) -> PlatformError {
    PlatformError::new(code, message)
}

fn helper_command(runtime: &DaemonRuntime) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&runtime.executable);
    command
        .current_dir(&runtime.working_directory)
        .envs(&runtime.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    // A GUI parent does not stop Windows opening a console window for its CLI child.
    // Keep the JSONL pipes; only suppress the console window.
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    command
}

impl DaemonService {
    pub fn new(
        runtime: DaemonRuntime,
        app_version: Option<String>,
        settings: Arc<dyn Fn() -> DaemonSettings + Send + Sync>,
    ) -> Self {
        Self {
            runtime,
            app_version,
            settings,
            helper: OnceCell::new(),
            next_id: AtomicU64::new(0),
            reject_new: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closing: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn initialize(&self) -> Result<(), PlatformError> {
        self.ensure_open().await?;
        if (self.settings)().managed {
            self.start().await?;
        }
        Ok(())
    }

    async fn ensure_open(&self) -> Result<Arc<Helper>, PlatformError> {
        if self.closed.load(Ordering::SeqCst) || self.reject_new.load(Ordering::SeqCst) {
            return Err(helper_error("DAEMON_SERVICE_CLOSED", "Desktop daemon service is closed."));
        }
        self.helper.get_or_try_init(|| async {
            if self.closed.load(Ordering::SeqCst) || self.reject_new.load(Ordering::SeqCst) {
                return Err(helper_error("DAEMON_SERVICE_CLOSED", "Desktop daemon service is closed."));
            }
            self.open().await
        }).await.cloned()
    }

    async fn open(&self) -> Result<Arc<Helper>, PlatformError> {
        if !self.runtime.executable.is_file() {
            return Err(helper_error(
                "RUNTIME_MISSING",
                format!("Bundled native Paseo executable is missing. ({})", self.runtime.executable.display()),
            ));
        }
        let mut child = helper_command(&self.runtime)
            .arg("desktop-host")
            .spawn()
            .map_err(|error| helper_error("HELPER_START_FAILED", format!("Could not start the desktop daemon helper: {error}")))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let diagnostics = Arc::new(Mutex::new(String::new()));
        let exited = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::new(AtomicBool::new(true));

        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => receive(&reader_pending, &line),
                    Ok(None) => break,
                    Err(error) => {
                        fail_pending(&reader_pending, helper_error("HELPER_IO", error.to_string()));
                        break;
                    }
                }
            }
        });
        let stderr_diagnostics = diagnostics.clone();
        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            while let Ok(read) = stderr.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let mut text = stderr_diagnostics.lock().unwrap();
                text.push_str(&String::from_utf8_lossy(&buffer[..read]));
                if text.len() > 8192 {
                    let mut cut = text.len() - 8192;
                    while !text.is_char_boundary(cut) {
                        cut += 1;
                    }
                    text.drain(..cut);
                }
            }
        });
        let wait_pending = pending.clone();
        let wait_exited = exited.clone();
        let wait_alive = alive.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            wait_alive.store(false, Ordering::SeqCst);
            let code = status.ok().and_then(|s| s.code()).map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
            let text = diagnostics.lock().unwrap().clone();
            fail_pending(
                &wait_pending,
                helper_error(
                    "HELPER_EXITED",
                    format!("Desktop daemon helper exited ({code}).{}", if text.is_empty() { String::new() } else { format!("\n{text}") }),
                ),
            );
            wait_exited.notify_waiters();
            wait_exited.notify_one();
        });
        let helper = Arc::new(Helper {
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            exited,
            alive,
        });
        let mut args = json!({
            "runtime": self.runtime.to_json(),
            "settings": (self.settings)().to_json(),
        });
        if let Some(version) = self.app_version.as_deref().filter(|v| !v.is_empty()) {
            args["appVersion"] = version.into();
        }
        if let Err(error) = self.send(&helper, "initialize", args).await {
            // No start request was sent, so initialization failure owns no daemon.
            helper.stdin.lock().await.take();
            if helper.alive.load(Ordering::SeqCst) {
                helper.exited.notified().await;
            }
            return Err(error);
        }
        Ok(helper)
    }

    async fn send(&self, helper: &Helper, command: &str, args: Value) -> Result<Value, PlatformError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        // Register the response before writing so even immediate replies are safe.
        helper.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&json!({"id": id, "command": command, "args": args})).unwrap();
        line.push(b'\n');
        let written = {
            let mut stdin = helper.stdin.lock().await;
            match stdin.as_mut() {
                Some(stdin) => match stdin.write_all(&line).await {
                    Ok(()) => stdin.flush().await,
                    Err(error) => Err(error),
                },
                None => Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "Desktop daemon helper input is closed.")),
            }
        };
        if let Err(error) = written {
            helper.pending.lock().unwrap().remove(&id);
            return Err(helper_error("HELPER_IO", format!("Desktop daemon helper is not running: {error}")));
        }
        rx.await
            .unwrap_or_else(|_| Err(helper_error("HELPER_EXITED", "Desktop daemon helper exited.")))
    }

    async fn request(&self, command: &str, args: Map<String, Value>) -> Result<Value, PlatformError> {
        let helper = self.ensure_open().await?;
        if self.reject_new.load(Ordering::SeqCst) {
            return Err(helper_error("DAEMON_SERVICE_CLOSING", "Desktop daemon service is closing."));
        }
        let mut args = args;
        args.insert("settings".into(), (self.settings)().to_json());
        self.send(&helper, command, Value::Object(args)).await
    }

    pub async fn status(&self) -> Result<Value, PlatformError> {
        self.request("status", Map::new()).await
    }
    pub async fn start(&self) -> Result<Value, PlatformError> {
        self.request("start", Map::new()).await
    }
    pub async fn stop(&self, reason: &str, pid: Option<i64>, started_at: Option<String>) -> Result<Value, PlatformError> {
        let mut args = Map::new();
        args.insert("reason".into(), reason.into());
        if let Some(pid) = pid {
            args.insert("pid".into(), pid.into());
        }
        if let Some(started_at) = started_at {
            args.insert("startedAt".into(), started_at.into());
        }
        self.request("stop", args).await
    }
    pub async fn restart(&self, pid: Option<i64>, started_at: Option<String>) -> Result<Value, PlatformError> {
        let mut args = Map::new();
        if let Some(pid) = pid {
            args.insert("pid".into(), pid.into());
        }
        if let Some(started_at) = started_at {
            args.insert("startedAt".into(), started_at.into());
        }
        self.request("restart", args).await
    }
    pub async fn logs(&self) -> Result<Value, PlatformError> {
        self.request("logs", Map::new()).await
    }

    /// Retains the desktop command names used by settings/host workflows.
    pub async fn invoke(&self, command: &str, args: &Value) -> Result<Value, PlatformError> {
        let pid = args.get("pid").and_then(Value::as_i64);
        let started_at = args.get("startedAt").and_then(Value::as_str).map(str::to_owned);
        match command {
            "desktop_daemon_status" => self.status().await,
            "start_desktop_daemon" => self.start().await,
            "stop_desktop_daemon" => {
                let reason = args.get("reason").and_then(Value::as_str).unwrap_or("manual_ipc");
                self.stop(reason, pid, started_at).await
            }
            "restart_desktop_daemon" => self.restart(pid, started_at).await,
            "desktop_daemon_logs" => self.logs().await,
            _ => Err(PlatformError::invalid(format!("Unknown desktop daemon command: {command}"))),
        }
    }

    /// Cancels pending startup before stopping only the exact launch this session
    /// acquired. A failed shutdown is surfaced and the service stays usable.
    pub async fn close(&self, keep_running: Option<bool>) -> Result<(), PlatformError> {
        let _closing = self.closing.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.reject_new.store(true, Ordering::SeqCst);
        let result = async {
            // Wait for an in-flight open, but never start a helper during shutdown.
            // Failed initialization owns no daemon and must remain retryable.
            let helper = self
                .helper
                .get_or_try_init(|| async { Err(helper_error("DAEMON_SERVICE_CLOSED", "Desktop daemon service is closed.")) })
                .await
                .ok()
                .cloned();
            if let Some(helper) = helper.filter(|h| h.alive.load(Ordering::SeqCst)) {
                let keep = keep_running.unwrap_or((self.settings)().keep_running_after_quit);
                self.send(&helper, "close", json!({"keepRunning": keep})).await?;
                helper.stdin.lock().await.take();
                if helper.alive.load(Ordering::SeqCst) {
                    helper.exited.notified().await;
                }
            }
            Ok::<_, PlatformError>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.closed.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => {
                self.reject_new.store(false, Ordering::SeqCst);
                Err(error)
            }
        }
    }
}

fn receive(pending: &Pending, line: &str) {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            fail_pending(pending, helper_error("INVALID_HELPER_RESPONSE", error.to_string()));
            return;
        }
    };
    let Some(id) = message.get("id").and_then(Value::as_u64) else { return };
    let Some(sender) = pending.lock().unwrap().remove(&id) else { return };
    let result = match message.get("error") {
        Some(Value::Object(error)) => Err(helper_error(
            error.get("code").and_then(Value::as_str).unwrap_or("DAEMON_HELPER_ERROR"),
            error.get("message").and_then(Value::as_str).unwrap_or("Desktop daemon operation failed."),
        )),
        _ => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    };
    let _ = sender.send(result);
}

fn fail_pending(pending: &Pending, error: PlatformError) {
    let senders: Vec<_> = pending.lock().unwrap().drain().map(|(_, sender)| sender).collect();
    for sender in senders {
        let _ = sender.send(Err(error.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_helper_initialization_remains_retryable_until_close() {
        let directory = std::env::temp_dir().join(format!("paseo-missing-runtime-{}", uuid::Uuid::new_v4()));
        let service = DaemonService::new(
            DaemonRuntime {
                executable: directory.join("paseo"),
                cli_shim_path: directory.join("paseo"),
                working_directory: directory,
                environment: HashMap::new(),
            },
            None,
            Arc::new(|| DaemonSettings { managed: true, keep_running_after_quit: false }),
        );
        for _ in 0..2 {
            let error = service.ensure_open().await.err().unwrap();
            assert_eq!(error.code, "RUNTIME_MISSING");
            assert!(service.helper.get().is_none());
        }
        service.close(None).await.unwrap();
        let error = service.ensure_open().await.err().unwrap();
        assert_eq!(error.code, "DAEMON_SERVICE_CLOSED");
    }

    #[cfg(windows)]
    #[test]
    fn helper_console_fixture() {
        if std::env::var_os("PASEO_HELPER_CONSOLE_PROBE").is_none() {
            return;
        }
        use windows::Win32::System::Console::GetConsoleWindow;
        unsafe {
            assert!(GetConsoleWindow().0.is_null(), "helper allocated a console window");
        }
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        assert_eq!(input, "initialize\n");
        println!("helper-ready");
        eprintln!("helper-diagnostic");
        std::process::exit(0);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn desktop_helper_starts_without_console_and_keeps_pipes() {
        let executable = std::env::current_exe().unwrap();
        let runtime = DaemonRuntime {
            working_directory: executable.parent().unwrap().to_owned(),
            cli_shim_path: executable.clone(),
            executable,
            environment: HashMap::new(),
        };
        let mut child = helper_command(&runtime)
            .args(["--exact", "desktop::daemon_service::tests::helper_console_fixture", "--nocapture"])
            .env("PASEO_HELPER_CONSOLE_PROBE", "1")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"initialize\n").await.unwrap();
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
            .await.unwrap().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("helper-ready"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("helper-diagnostic"));
    }
}
