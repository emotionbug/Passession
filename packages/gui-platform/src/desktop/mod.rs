//! Desktop host: daemon helper ownership, quit/close coordination, routes,
//! notifications, tray and native window lifetime (original desktop_host.dart).
//! One process owns everything; Makepad windows are logical windows of the shell.

pub(crate) mod daemon_service;
pub(crate) mod editors;
pub(crate) mod geometry;
pub(crate) mod native;
pub(crate) mod settings;
pub(crate) mod single_instance;
pub(crate) mod updater;
#[cfg(any(target_os = "macos", windows))]
pub(crate) mod native_tray;

#[cfg(target_os = "linux")]
pub(crate) mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux as os;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos as os;
#[cfg(windows)]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use self::windows as os;
#[cfg(windows)]
pub(crate) mod windows_util;

pub use crate::facade::desktop::{available, invoke};

use crate::{
    action::*,
    deep_link,
    platform::{Completion, PlatformConfig, PlatformState, Shared},
};
use daemon_service::{DaemonRuntime, DaemonService, DaemonSettings};
use makepad_widgets::{Cx, Event};
use paseo_gui_core::CoreEvent;
use serde_json::{json, Map, Value};
use settings::DesktopSettings;
use std::{
    collections::VecDeque,
    io::Write as _,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{oneshot, OnceCell};

/// A later launch's request, forwarded by the single-instance channel.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Activation {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub project_path: Option<String>,
    /// Part of the primary's own launch (route into the first window).
    #[serde(default)]
    pub initial: bool,
}

/// Events that native integrations deliver before or after the platform starts.
#[derive(Debug, Clone)]
pub(crate) enum NativeSignal {
    Activation(Activation),
    SessionEnding,
    SessionEndingCancelled,
    SessionEnded,
    LifecycleError(String),
    /// The app became active (dock click, focus): show the preferred window.
    /// Linux has no such signal; its activations arrive as `Activation`.
    #[cfg(any(target_os = "macos", windows))]
    Activated,
    NotificationClick(Map<String, Value>),
    TrayOpen,
    TrayQuit,
    QuitRequestedByOs,
    /// The OS lifecycle bridge finished starting (or failed).
    LifecycleStarted(Result<(), PlatformError>),
    TrayFailed(String),
}

static SIGNALS: Mutex<Vec<NativeSignal>> = Mutex::new(Vec::new());

/// Thread-safe delivery from OS callbacks; drained on the GUI thread.
pub(crate) fn deliver(signal: NativeSignal) {
    SIGNALS.lock().unwrap().push(signal);
    makepad_widgets::SignalToUI::set_ui_signal();
}

#[derive(Default)]
struct QuitState {
    /// Quitting intent (OS session ending, accepted quit): disables close-to-tray.
    quitting: bool,
    prepared: bool,
    next_token: u64,
    pending: Option<PendingQuit>,
}

struct PendingQuit {
    token: u64,
    reason: QuitReason,
    /// The update installer waits for the guard decision itself.
    waiter: Option<oneshot::Sender<bool>>,
}

#[derive(Default)]
struct Routes {
    ready: bool,
    pending: VecDeque<PlatformEvent>,
}

pub(crate) struct DesktopShared {
    pub settings: OnceCell<Arc<DesktopSettings>>,
    daemon: tokio::sync::Mutex<Option<Arc<DaemonService>>>,
    daemon_failure: Mutex<Option<String>>,
    updater: updater::Updater,
    pub geometry: geometry::GeometryStore,
    quit: Mutex<QuitState>,
    routes: Mutex<Routes>,
    notifications: tokio::sync::Mutex<VecDeque<String>>,
    pub lifecycle_ready: AtomicBool,
    pub tray_created: AtomicBool,
    local_daemon_listening: AtomicBool,
    log: Mutex<Option<std::fs::File>>,
    log_path: PathBuf,
    app_version: String,
}

/// GUI-thread-only desktop state (tray objects, native window handles).
#[derive(Default)]
pub(crate) struct DesktopMain {
    tray: Option<os::Tray>,
    windows: os::NativeWindows,
}

impl DesktopMain {
    pub fn hide_windows(&mut self) {
        self.windows.hide_all();
    }
    pub fn show_windows(&mut self) {
        self.windows.show_all();
    }
}

impl DesktopShared {
    pub fn new(config: &PlatformConfig) -> anyhow::Result<Self> {
        let support = crate::paths::app_paths().support_dir.clone();
        // Startup-only reads of a few small files (window geometry); everything
        // else loads on workers.
        std::fs::create_dir_all(&support)?;
        let log_path = support.join("desktop-main.log");
        Ok(Self {
            settings: OnceCell::new(),
            daemon: tokio::sync::Mutex::new(None),
            daemon_failure: Mutex::new(None),
            updater: updater::Updater::new(config.app_version.clone()),
            geometry: geometry::GeometryStore::load(),
            quit: Mutex::new(QuitState::default()),
            routes: Mutex::new(Routes::default()),
            notifications: tokio::sync::Mutex::new(VecDeque::new()),
            lifecycle_ready: AtomicBool::new(false),
            tray_created: AtomicBool::new(false),
            local_daemon_listening: AtomicBool::new(false),
            log: Mutex::new(None),
            log_path,
            app_version: config.app_version.clone(),
        })
    }

    pub async fn settings(&self) -> Result<Arc<DesktopSettings>, PlatformError> {
        self.settings
            .get_or_try_init(|| async {
                tokio::task::spawn_blocking(DesktopSettings::load)
                    .await
                    .map_err(|e| PlatformError::native(e.to_string()))?
                    .map(Arc::new)
                    .map_err(PlatformError::from)
            })
            .await
            .cloned()
    }

    pub fn quitting(&self) -> bool {
        self.quit.lock().unwrap().quitting
    }

    /// The first window has been built and accepts routes.
    #[cfg(target_os = "macos")]
    pub fn routes_ready(&self) -> bool {
        self.routes.lock().unwrap().ready
    }

    pub fn window_ready(&self) {
        let pending: Vec<_> = {
            let mut routes = self.routes.lock().unwrap();
            routes.ready = true;
            routes.pending.drain(..).collect()
        };
        for event in pending {
            post_event(event);
        }
    }

    /// Routes wait for the first window; other events go out immediately.
    fn route(&self, event: PlatformEvent) {
        let mut routes = self.routes.lock().unwrap();
        if routes.ready {
            drop(routes);
            post_event(event);
        } else {
            routes.pending.push_back(event);
        }
    }

    /// Appends to `<support>/desktop-main.log`.
    pub fn log(&self, message: &str) {
        let line = format!("{} {message}\n", chrono::Local::now().to_rfc3339());
        let mut log = self.log.lock().unwrap();
        if log.is_none() {
            *log = std::fs::OpenOptions::new().create(true).append(true).open(&self.log_path).ok();
        }
        if let Some(file) = log.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn report(&self, message: &str) {
        self.log(message);
        post_event(PlatformEvent::HostError { message: message.to_owned() });
    }

    async fn daemon(&self) -> Result<Arc<DaemonService>, PlatformError> {
        let mut daemon = self.daemon.lock().await;
        if let Some(existing) = daemon.as_ref() {
            return Ok(existing.clone());
        }
        let service = self.create_daemon().await?;
        *daemon = Some(service.clone());
        Ok(service)
    }

    async fn create_daemon(&self) -> Result<Arc<DaemonService>, PlatformError> {
        let runtime = tokio::task::spawn_blocking(DaemonRuntime::locate)
            .await
            .map_err(|e| PlatformError::native(e.to_string()))?
            .map_err(|e| PlatformError::new("RUNTIME_MISSING", e.to_string()))?;
        let settings = self.settings().await?;
        let read: Arc<dyn Fn() -> DaemonSettings + Send + Sync> = Arc::new(move || DaemonSettings {
            managed: settings.managed(),
            keep_running_after_quit: settings.keep_running(),
        });
        Ok(Arc::new(DaemonService::new(
            runtime,
            Some(self.app_version.clone()).filter(|v| !v.is_empty()),
            read,
        )))
    }

    /// Original `_registerLocalHost`: a starting daemon is announced as soon as it
    /// reports a listen address (waiting at most 60 s for that address).
    async fn register_local_host(&self, status: Value) {
        let listen = |status: &Value| status.get("listen").and_then(Value::as_str).is_some_and(|l| !l.is_empty());
        let mut current = status;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while current.get("status").and_then(Value::as_str) == Some("starting")
            && !listen(&current)
            && !self.quitting()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Ok(daemon) = self.daemon().await else { return };
            match daemon.status().await {
                Ok(status) => current = status,
                Err(error) => {
                    self.report(&error.message);
                    return;
                }
            }
        }
        let state = current.get("status").and_then(Value::as_str);
        if !matches!(state, Some("running" | "starting")) || !listen(&current) {
            return;
        }
        self.local_daemon_listening.store(true, Ordering::SeqCst);
        post_event(PlatformEvent::LocalDaemonStatus { status: current });
    }
}

pub(crate) fn start(cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState, config: &PlatformConfig) {
    let _ = cx;
    state.desktop.windows = os::NativeWindows::new();
    // Lifecycle bridge first: the tray (close-to-tray) is created only once it
    // is ready, so logout is never intercepted.
    os::start_lifecycle(shared);
    if let Err(error) = os::init_notifications(shared) {
        shared.desktop.report(&error.message);
    }

    // Launch routing: an agent deep link in argv wins (first valid link only);
    // otherwise one project path (positional absolute directory or --open-project).
    let args: Vec<String> = config.argv.iter().skip(1).cloned().collect();
    if let Some((server_id, agent_id)) = deep_link::agent_deep_link_from_argv(&args) {
        shared.desktop.route(PlatformEvent::Route(PlatformRoute::Agent { server_id, agent_id }));
    } else if let Some(path) = deep_link::open_project_path_from_argv(&args) {
        shared.desktop.route(PlatformEvent::Route(PlatformRoute::Project { path: path.display().to_string(), server_id: None }));
    }

    // The daemon helper: waiting for a still-starting daemon never holds the window.
    let task_shared = shared.clone();
    shared.spawn(async move {
        let desktop = &task_shared.desktop;
        let result = async {
            let settings = desktop.settings().await?;
            let _ = settings;
            let daemon = desktop.daemon().await?;
            daemon.initialize().await?;
            daemon.status().await
        }
        .await;
        match result {
            Ok(status) => desktop.register_local_host(status).await,
            Err(error) => {
                *desktop.daemon_failure.lock().unwrap() = Some(error.message.clone());
                desktop.report(&error.message);
            }
        }
    });
}

pub(crate) fn handle_event(cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState, event: &Event) {
    let signals = std::mem::take(&mut *SIGNALS.lock().unwrap());
    for signal in signals {
        handle_signal(cx, shared, state, signal);
    }
    if let Event::Signal = event
        && let Some(tray) = state.desktop.tray.as_mut() {
            tray.poll();
        }
}

fn handle_signal(_cx: &mut Cx, shared: &Arc<Shared>, state: &mut PlatformState, signal: NativeSignal) {
    let desktop = &shared.desktop;
    match signal {
        NativeSignal::Activation(activation) => activate(shared, state, activation),
        NativeSignal::SessionEnding => {
            // A cancellable OS query is intent, not consent to stop ongoing sessions.
            desktop.quit.lock().unwrap().quitting = true;
        }
        NativeSignal::SessionEndingCancelled => {
            let mut quit = desktop.quit.lock().unwrap();
            if !quit.prepared {
                quit.quitting = false;
            }
        }
        NativeSignal::SessionEnded => {
            desktop.quit.lock().unwrap().quitting = true;
            request_quit(shared, QuitReason::Shutdown, true);
        }
        NativeSignal::LifecycleStarted(result) => match result {
            Ok(()) => {
                desktop.lifecycle_ready.store(true, Ordering::SeqCst);
                os::create_tray(shared);
            }
            // A desktop without a session manager is an environment capability,
            // not a failure the user can act on: record it without a banner.
            Err(error) if error.code == "logout_unavailable" => desktop.log(&error.message),
            Err(error) => desktop.report(&error.message),
        },
        NativeSignal::TrayFailed(message) => desktop.report(&message),
        NativeSignal::LifecycleError(message) => {
            desktop.lifecycle_ready.store(false, Ordering::SeqCst);
            desktop.report(&message);
            show_preferred(state);
        }
        #[cfg(any(target_os = "macos", windows))]
        NativeSignal::Activated => {
            if !desktop.quitting() {
                show_preferred(state);
            }
        }
        NativeSignal::NotificationClick(data) => {
            show_preferred(state);
            if let Some(route) = PlatformRoute::from_notification_data(&data) {
                desktop.route(PlatformEvent::Route(route));
            }
        }
        NativeSignal::TrayOpen => {
            if !desktop.quitting() {
                show_preferred(state);
            }
        }
        NativeSignal::TrayQuit | NativeSignal::QuitRequestedByOs => request_quit(shared, QuitReason::Quit, false),
    }
}

fn show_preferred(state: &mut PlatformState) {
    state.desktop.windows.show_all();
    post_event(PlatformEvent::ShowWindow);
}

/// Original `_activate`.
fn activate(shared: &Arc<Shared>, state: &mut PlatformState, activation: Activation) {
    let desktop = &shared.desktop;
    if desktop.quitting() {
        return;
    }
    if let Some(url) = activation.url {
        if let Some((server_id, agent_id)) = deep_link::parse_agent_deep_link(&url) {
            show_preferred(state);
            desktop.route(PlatformEvent::Route(PlatformRoute::Agent { server_id, agent_id }));
        }
    } else if let Some(path) = activation.project_path {
        let route = PlatformRoute::Project { path, server_id: None };
        if activation.initial {
            desktop.route(PlatformEvent::Route(route));
        } else {
            state.desktop.windows.show_all();
            desktop.route(PlatformEvent::OpenWindow { route: Some(route) });
        }
    } else {
        show_preferred(state);
    }
}

pub(crate) fn observe_core_event(_shared: &Arc<Shared>, _event: &CoreEvent) {
    // Desktop attention notifications and badges are driven by the shell through
    // `notification_policy` + `notify`/`set_badge_count`; nothing to track here.
}

// ----- quit / close -----

pub(crate) fn request_quit(shared: &Arc<Shared>, reason: QuitReason, force: bool) {
    let desktop = &shared.desktop;
    let token = {
        let mut quit = desktop.quit.lock().unwrap();
        if quit.prepared {
            drop(quit);
            post_event(PlatformEvent::QuitReady);
            return;
        }
        if quit.pending.is_some() && !force {
            return;
        }
        quit.next_token += 1;
        let token = quit.next_token;
        if force {
            quit.quitting = true;
            if let Some(pending) = quit.pending.take()
                && let Some(waiter) = pending.waiter {
                    let _ = waiter.send(false);
                }
        } else {
            quit.pending = Some(PendingQuit { token, reason, waiter: None });
        }
        token
    };
    if force {
        let shared = shared.clone();
        shared.clone().spawn(async move { finish_quit(&shared, reason).await });
    } else {
        post_event(PlatformEvent::QuitRequested { token, reason });
    }
}

pub(crate) fn respond_quit(shared: &Arc<Shared>, token: u64, allowed: bool) {
    let desktop = &shared.desktop;
    let pending = {
        let mut quit = desktop.quit.lock().unwrap();
        match quit.pending.as_ref() {
            Some(pending) if pending.token == token => {}
            _ => return,
        }
        let pending = quit.pending.take().unwrap();
        quit.quitting = allowed;
        pending
    };
    if !allowed {
        os::termination_decided(false);
        post_event(PlatformEvent::QuitCancelled);
        if let Some(waiter) = pending.waiter {
            let _ = waiter.send(false);
        }
        return;
    }
    match pending.waiter {
        Some(waiter) => {
            let _ = waiter.send(true);
        }
        None => {
            let reason = pending.reason;
            let task_shared = shared.clone();
            shared.spawn(async move { finish_quit(&task_shared, reason).await });
        }
    }
}

/// Asks the shell for close-guard consent and waits (update installation).
async fn guarded_consent(shared: &Arc<Shared>, reason: QuitReason) -> bool {
    let (tx, rx) = oneshot::channel();
    let token = {
        let mut quit = shared.desktop.quit.lock().unwrap();
        if quit.pending.is_some() {
            return false;
        }
        quit.next_token += 1;
        let token = quit.next_token;
        quit.pending = Some(PendingQuit { token, reason, waiter: Some(tx) });
        token
    };
    post_event(PlatformEvent::QuitRequested { token, reason });
    rx.await.unwrap_or(false)
}

/// Original `_prepareQuit` after consent: stop only the owned daemon (unless
/// keep-running; updates always stop the old-version daemon). A failed stop is
/// logged and the quit still completes.
async fn prepare_quit(shared: &Arc<Shared>, reason: QuitReason) {
    let desktop = &shared.desktop;
    if desktop.quit.lock().unwrap().prepared {
        return;
    }
    desktop.quit.lock().unwrap().quitting = true;
    let daemon = desktop.daemon.lock().await.clone();
    if let Some(daemon) = daemon {
        let keep = if reason == QuitReason::AppUpdate {
            false
        } else {
            match desktop.settings().await {
                Ok(settings) => settings.keep_running(),
                Err(_) => false,
            }
        };
        if let Err(error) = daemon.close(Some(keep)).await {
            desktop.log(&format!("Daemon stop failed during quit: {}", error.message));
        }
    }
    desktop.quit.lock().unwrap().prepared = true;
}

async fn finish_quit(shared: &Arc<Shared>, reason: QuitReason) {
    prepare_quit(shared, reason).await;
    // Let in-flight notification deliveries settle before teardown.
    drop(shared.desktop.notifications.lock().await);
    let geometry_shared = shared.clone();
    let _ = tokio::task::spawn_blocking(move || geometry_shared.desktop.geometry.flush()).await;
    // macOS may be blocked inside a deferred `terminate:`; the reply must not
    // wait for the GUI event loop.
    os::termination_decided(true);
    shared.on_main(|_, state| {
        state.desktop.tray = None;
        post_event(PlatformEvent::QuitReady);
    });
}

pub(crate) fn window_close_requested(shared: &Arc<Shared>, open_windows: usize, done: Completion) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        let desktop = &task_shared.desktop;
        if open_windows > 1 || desktop.quitting() {
            done.ok(PlatformValue::CloseDecision(CloseDecision::CloseWindow));
            return;
        }
        let tray = desktop.tray_created.load(Ordering::SeqCst)
            && desktop.lifecycle_ready.load(Ordering::SeqCst)
            && !desktop.quitting()
            && os::tray_host_available().await;
        if tray && !desktop.quitting() {
            // Hiding is not closing the document/session: no close guard here.
            task_shared.on_main(|_, state| state.desktop.hide_windows());
            done.ok(PlatformValue::CloseDecision(CloseDecision::HideToTray));
        } else {
            request_quit(&task_shared, QuitReason::Quit, false);
            done.ok(PlatformValue::CloseDecision(CloseDecision::QuitApplication));
        }
    });
}

// ----- invoke -----

pub(crate) fn invoke_command(shared: &Arc<Shared>, command: String, args: Value, done: Completion) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        let result = invoke_inner(&task_shared, &command, &args).await;
        done.finish(result.map(PlatformValue::Json));
    });
}

async fn invoke_inner(shared: &Arc<Shared>, command: &str, args: &Value) -> Result<Value, PlatformError> {
    let desktop = &shared.desktop;
    match command {
        "desktop_settings_get" => Ok(Value::Object(desktop.settings().await?.value())),
        "desktop_settings_update" => {
            let settings = desktop.settings().await?;
            let was_managed = settings.managed();
            let patch = args.get("patch").and_then(Value::as_object).cloned().unwrap_or_default();
            let patch_settings = settings.clone();
            let result = tokio::task::spawn_blocking(move || patch_settings.patch(&patch))
                .await
                .map_err(|e| PlatformError::native(e.to_string()))??;
            if was_managed && !settings.managed()
                && let Some(daemon) = desktop.daemon.lock().await.clone() {
                    daemon.stop("settings", None, None).await?;
                }
            if !was_managed && settings.managed() {
                let status = desktop.daemon().await?.start().await?;
                desktop.register_local_host(status).await;
            }
            Ok(Value::Object(result))
        }
        "desktop_get_runtime_info" => Ok(runtime_info(shared)),
        "desktop_app_logs" => {
            let path = desktop.log_path.clone();
            let contents = tokio::task::spawn_blocking(move || tail(&path, 128 * 1024))
                .await
                .map_err(|e| PlatformError::native(e.to_string()))??;
            Ok(json!({"logPath": desktop.log_path, "contents": contents}))
        }
        "desktop_update_diagnostics" => Ok(desktop.updater.diagnostics().await),
        "check_app_update" => {
            let channel = channel(shared, args).await;
            Ok(desktop.updater.check(&channel, args.get("intent").and_then(Value::as_str) == Some("manual")).await)
        }
        "install_app_update" => install_update(shared, args).await,
        _ => {
            let daemon = desktop.daemon().await?;
            let result = daemon.invoke(command, args).await?;
            if command == "start_desktop_daemon" || command == "restart_desktop_daemon" {
                desktop.register_local_host(result.clone()).await;
            }
            Ok(result)
        }
    }
}

async fn channel(shared: &Arc<Shared>, args: &Value) -> String {
    match args.get("releaseChannel").and_then(Value::as_str) {
        Some(channel) => channel.to_owned(),
        None => match shared.desktop.settings().await {
            Ok(settings) => settings.release_channel(),
            Err(_) => updater::Updater::default_channel().to_owned(),
        },
    }
}

async fn install_update(shared: &Arc<Shared>, args: &Value) -> Result<Value, PlatformError> {
    let desktop = &shared.desktop;
    let channel = channel(shared, args).await;
    let consent_shared = shared.clone();
    let result = desktop
        .updater
        .install(&channel, || async move {
            if !guarded_consent(&consent_shared, QuitReason::AppUpdate).await {
                return Err(PlatformError::new("cancelled", "Quit cancelled by an open document."));
            }
            prepare_quit(&consent_shared, QuitReason::AppUpdate).await;
            Ok(())
        })
        .await;
    let launched = result.get("installerLaunched") == Some(&Value::Bool(true)) || result.get("installed") == Some(&Value::Bool(true));
    let prepared = desktop.quit.lock().unwrap().prepared;
    if launched {
        let task_shared = shared.clone();
        shared.spawn(async move { finish_quit(&task_shared, QuitReason::AppUpdate).await });
    } else if prepared {
        recover_failed_update(shared).await;
    } else {
        // Consent refused or nothing installed: the app keeps running normally.
        desktop.quit.lock().unwrap().quitting = false;
    }
    Ok(result)
}

/// Original `_recoverFailedUpdate`: cancel quit, start a new helper session and
/// re-announce the local daemon.
async fn recover_failed_update(shared: &Arc<Shared>) {
    let desktop = &shared.desktop;
    {
        let mut quit = desktop.quit.lock().unwrap();
        quit.quitting = false;
        quit.prepared = false;
    }
    post_event(PlatformEvent::QuitCancelled);
    *desktop.daemon.lock().await = None;
    let result = async {
        let daemon = desktop.daemon().await?;
        daemon.initialize().await?;
        daemon.status().await
    }
    .await;
    match result {
        Ok(status) => desktop.register_local_host(status).await,
        Err(error) => desktop.report(&error.message),
    }
}

fn runtime_info(shared: &Arc<Shared>) -> Value {
    let desktop = &shared.desktop;
    let (resident, peak) = os::memory_usage();
    json!({
        "platform": std::env::consts::OS,
        "operatingSystemVersion": os::os_version(),
        "executable": std::env::current_exe().ok(),
        "pid": std::process::id(),
        "processors": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        "locale": os::locale(),
        "residentMemoryBytes": resident,
        "peakResidentMemoryBytes": peak,
        "sessionType": std::env::var("XDG_SESSION_TYPE").ok(),
        "desktop": std::env::var("XDG_CURRENT_DESKTOP").ok(),
        "notificationInitializationStarted": true,
        "lifecycleReady": desktop.lifecycle_ready.load(Ordering::SeqCst),
        "appVersion": desktop.app_version,
        "windowId": "main",
        "primaryWindowId": "main",
        "daemonFailure": desktop.daemon_failure.lock().unwrap().clone(),
    })
}

fn tail(path: &std::path::Path, maximum: u64) -> std::io::Result<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    let length = file.metadata()?.len();
    if length > maximum {
        file.seek(SeekFrom::Start(length - maximum))?;
    }
    let mut bytes = Vec::new();
    file.take(maximum).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// ----- notifications -----

pub(crate) struct DesktopNotification {
    pub id: i32,
    pub title: String,
    pub body: Option<String>,
    pub sound: bool,
    /// JSON click payload.
    pub payload: String,
}

/// Serialized check + delivery so concurrent requests cannot both emit an
/// event; only successful deliveries are remembered (bounded to 512 keys).
pub(crate) fn notify(shared: &Arc<Shared>, request: crate::NotificationRequest, done: Completion) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        let desktop = &task_shared.desktop;
        let mut keys = desktop.notifications.lock().await;
        if let Some(key) = &request.event_key
            && keys.contains(key) {
                done.ok(PlatformValue::None);
                return;
            }
        if desktop.quit.lock().unwrap().prepared {
            done.fail(PlatformError::new("shutting_down", "The application is shutting down."));
            return;
        }
        let sound = match request.sound {
            Some(sound) => sound,
            None => match desktop.settings().await {
                Ok(settings) => settings.play_sound(),
                Err(_) => true,
            },
        };
        let mut data = request.data.clone();
        data.insert("windowId".into(), "main".into());
        let notification = DesktopNotification {
            id: request.id,
            title: request.title.clone(),
            body: request.body.clone(),
            sound,
            payload: Value::Object(data).to_string(),
        };
        match os::show_notification(&notification).await {
            Ok(()) => {
                if let Some(key) = request.event_key {
                    keys.push_back(key);
                    while keys.len() > 512 {
                        keys.pop_front();
                    }
                }
                done.ok(PlatformValue::None);
            }
            Err(error) => done.fail(error),
        }
    });
}

/// Click payload → route event (adds the notification/action ids like the original).
pub(crate) fn notification_clicked(payload: &str, notification_id: Option<i64>, action_id: Option<String>) {
    let mut data = match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    data.insert("notificationId".into(), notification_id.map(Value::from).unwrap_or(Value::Null));
    data.insert("actionId".into(), action_id.map(Value::from).unwrap_or(Value::Null));
    deliver(NativeSignal::NotificationClick(data));
}
