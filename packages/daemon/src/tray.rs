//! One optional tray child per ready daemon worker. Its inherited pipes carry
//! lifetime and a confirmed stop command, not daemon credentials.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "macos", windows))]
mod native;

use crate::state::AppState;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::OsString,
    fs::{OpenOptions, TryLockError},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    task::JoinHandle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

const START: u8 = b'T';
const STOP: u8 = b'S';
const STOP_LABEL: &str = "Stop daemon…";
const CONFIRM_LABEL: &str = "Confirm stop (may interrupt running agents)";
const OPEN_LABEL: &str = "Open Passession";
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(15);

async fn session_allowed(blocked: bool, probe: impl AsyncFnOnce() -> bool) -> bool {
    !blocked && probe().await
}

fn blocked_session() -> bool {
    let ssh = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    if ssh {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        linux::is_container()
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        false
    }
}

async fn session_available(blocked: bool) -> bool {
    #[cfg(target_os = "linux")]
    let probe = linux::watcher_available;
    #[cfg(any(target_os = "macos", windows))]
    let probe = async || native::interactive_session();
    session_allowed(blocked, probe).await
}

pub(crate) fn start(app: Arc<AppState>, listen: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        let blocked = blocked_session();
        loop {
            let available = tokio::select! {
                _ = app.cancel.cancelled() => return,
                available = session_available(blocked) => available,
            };
            if available && let Err(error) = supervise_child(&app, &listen).await {
                tracing::warn!(%error, "Daemon tray unavailable");
            }
            // A Linux user service may outlive the graphical login or start
            // before the shell has registered its StatusNotifierWatcher.
            if blocked || !cfg!(target_os = "linux") {
                return;
            }
            tokio::select! {
                _ = app.cancel.cancelled() => return,
                _ = tokio::time::sleep(RETRY_INTERVAL) => {},
            }
        }
    })
}

async fn supervise_child(app: &AppState, listen: &str) -> Result<()> {
    let mut command = Command::new(&app.executable);
    command
        .arg("__tray")
        .arg("--home")
        .arg(&app.home)
        .arg("--listen")
        .arg(listen)
        .env_remove("PASEO_NATIVE_OPTIONS")
        .env_remove("PASEO_NATIVE_SUPERVISOR")
        .env_remove("PASEO_NATIVE_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    let mut child = command.spawn().context("Cannot launch the tray child")?;
    let mut parent = child.stdin.take().context("Missing tray lifetime pipe")?;
    let mut requests = child.stdout.take().context("Missing tray request pipe")?;
    parent.write_u8(START).await?;
    let request = tokio::select! {
        biased;
        _ = app.cancel.cancelled() => None,
        result = requests.read_u8() => result.ok(),
    };
    if request == Some(STOP) {
        app.request_shutdown(false);
    }
    drop(parent); // EOF requests normal icon removal even before the worker exits.
    match tokio::time::timeout(CHILD_EXIT_TIMEOUT, child.wait()).await {
        Ok(status) => {
            let status = status?;
            ensure!(status.success(), "Tray child exited with {status}");
        }
        Err(_) => child.kill().await?,
    }
    Ok(())
}

pub fn run_child(args: impl Iterator<Item = OsString>) -> Result<i32> {
    let [home_arg, home, listen_arg, listen]: [OsString; 4] = args
        .collect::<Vec<_>>()
        .try_into()
        .ok()
        .context("Internal tray entry requires --home PATH --listen ADDRESS")?;
    let home = PathBuf::from(home);
    ensure!(
        home_arg == "--home" && home.is_absolute() && listen_arg == "--listen",
        "Internal tray entry requires --home PATH --listen ADDRESS"
    );
    let listen = listen
        .into_string()
        .ok()
        .context("Invalid tray listen address")?;
    let stdin = io::stdin();
    ensure!(
        !stdin.is_terminal(),
        "Tray must be launched by a daemon worker"
    );
    let mut start = [0];
    stdin.lock().read_exact(&mut start)?;
    ensure!(start == [START], "Invalid daemon tray startup message");

    // A replacement worker cannot display a duplicate while the former child
    // is still removing its icon. Never unlink the lock while it is in use.
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join(".tray.lock"))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(0),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    let cancel = CancellationToken::new();
    let parent_lost = cancel.clone();
    // A dedicated thread keeps EOF detection independent of the toolkit's event
    // loop, including while it is blocked tracking an open native menu.
    std::thread::Builder::new()
        .name("paseo-tray-parent".into())
        .spawn(move || {
            watch_parent(stdin.lock(), &parent_lost);
            std::thread::sleep(CHILD_EXIT_TIMEOUT / 2);
            std::process::exit(0);
        })?;

    let title = format!("Passession daemon {} · {listen}", crate::VERSION);
    #[cfg(target_os = "linux")]
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(linux::run(&home, title, cancel))?;
    #[cfg(any(target_os = "macos", windows))]
    native::run(&home, &title, cancel)?;
    Ok(0)
}

fn watch_parent(mut input: impl Read, cancel: &CancellationToken) {
    // An unnamed pipe closes on graceful shutdown or parent death without
    // probing/reusing a PID. Interrupted reads are retried by io::copy.
    let _ = io::copy(&mut input, &mut io::sink());
    cancel.cancel();
}

fn icon() -> Result<(u32, u32, Vec<u8>)> {
    #[cfg(target_os = "macos")]
    let raw = include_bytes!("../assets/trayTemplate.png");
    #[cfg(not(target_os = "macos"))]
    let raw = include_bytes!("../assets/tray.png");
    let mut reader = png::Decoder::new(io::Cursor::new(raw)).read_info()?;
    let mut rgba = vec![
        0;
        reader
            .output_buffer_size()
            .context("Invalid tray icon size")?
    ];
    let info = reader.next_frame(&mut rgba)?;
    ensure!(
        info.color_type == png::ColorType::Rgba && info.bit_depth == png::BitDepth::Eight,
        "Tray icon must be RGBA8"
    );
    rgba.truncate(info.buffer_size());
    Ok((info.width, info.height, rgba))
}

#[derive(Default)]
struct Confirmation(Option<Instant>);

impl Confirmation {
    fn arm(&mut self) {
        self.0 = Some(Instant::now() + CONFIRM_TIMEOUT);
    }

    fn confirmed(&self) -> bool {
        self.0.is_some_and(|deadline| Instant::now() < deadline)
    }

    fn expire(&mut self) -> bool {
        if self.0.is_some_and(|deadline| Instant::now() >= deadline) {
            self.0 = None;
            return true;
        }
        false
    }
}

fn request_stop() -> Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(&[STOP])?;
    output.flush()?;
    Ok(())
}

async fn open_app(home: &Path) {
    if let Err(error) = crate::cli::open_daemon_desktop(home).await {
        eprintln!("Cannot open Passession from the daemon tray: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_detection_never_probes_blocked_sessions() {
        assert!(!session_allowed(true, async || panic!("blocked session was probed")).await);
        assert!(!session_allowed(false, async || false).await);
        assert!(session_allowed(false, async || true).await);
    }

    #[test]
    fn parent_exit_cancels_child_on_eof_or_broken_pipe() {
        let cancel = CancellationToken::new();
        watch_parent(io::Cursor::new([]), &cancel);
        assert!(cancel.is_cancelled());

        struct BrokenPipe;
        impl Read for BrokenPipe {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
        let cancel = CancellationToken::new();
        watch_parent(BrokenPipe, &cancel);
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_requires_a_second_click_before_expiry() {
        let mut confirmation = Confirmation::default();
        assert!(!confirmation.confirmed());
        confirmation.arm();
        assert!(confirmation.confirmed());
        tokio::time::advance(CONFIRM_TIMEOUT).await;
        assert!(!confirmation.confirmed());
        assert!(confirmation.expire());
        assert!(confirmation.0.is_none());
    }

    #[test]
    fn embedded_icon_is_valid_rgba() {
        let (width, height, rgba) = icon().unwrap();
        assert!(width > 0 && height > 0);
        assert_eq!(rgba.len(), (width * height * 4) as usize);
    }
}
