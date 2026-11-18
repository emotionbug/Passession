//! Desktop integration: independent daemon startup, quit, routes and notifications.
//! One process owns the app's logical Makepad windows.

pub(crate) mod app_update;
mod app_update_owner;
pub mod component_store;
pub(crate) mod daemon_service;
pub(crate) mod editors;
pub(crate) mod geometry;
pub(crate) mod native;
pub(crate) mod settings;
pub(crate) mod single_instance;
pub(crate) mod updater;

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

pub use crate::facade::desktop::{LocalDaemon, available, invoke, local_daemon};

use crate::{
    action::*,
    deep_link,
    platform::{Completion, PlatformConfig, Shared},
};
use paseo_gui_core::CoreEvent;
use serde_json::{Map, Value, json};
use settings::DesktopSettings;
use std::{
    collections::VecDeque,
    io::Write as _,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, OnceCell, oneshot};

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
    /// A cancellable end-of-session query (Linux session managers, Windows).
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    SessionEnding,
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    SessionEndingCancelled,
    /// Linux session managers and macOS power-off; Windows ends at `SessionEnding`.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    SessionEnded,
    /// Linux session-manager failures.
    #[cfg(target_os = "linux")]
    LifecycleError(String),
    /// The app became active (dock click, focus): show the preferred window.
    /// Linux has no such signal; its activations arrive as `Activation`.
    #[cfg(any(target_os = "macos", windows))]
    Activated,
    NotificationClick(Map<String, Value>),
    /// Linux SIGTERM and the macOS application quit event.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    QuitRequestedByOs,
    /// The OS lifecycle bridge finished starting (or failed).
    LifecycleStarted(Result<(), PlatformError>),
}

static SIGNALS: Mutex<Vec<NativeSignal>> = Mutex::new(Vec::new());

/// Thread-safe delivery from OS callbacks; drained on the GUI thread.
pub(crate) fn deliver(signal: NativeSignal) {
    SIGNALS.lock().unwrap().push(signal);
    makepad_widgets::SignalToUI::set_ui_signal();
}

#[derive(Default)]
struct QuitState {
    /// Quitting intent (OS session ending or accepted quit).
    quitting: bool,
    prepared: bool,
    remote_update_handoff: bool,
    next_token: u64,
    pending: Option<PendingQuit>,
}

impl QuitState {
    fn is_quitting(&self) -> bool {
        self.quitting || self.remote_update_handoff
    }

    fn reserve_remote_update(&mut self) -> bool {
        if self.is_quitting() || self.prepared || self.pending.is_some() {
            return false;
        }
        self.remote_update_handoff = true;
        true
    }
}

struct PendingQuit {
    token: u64,
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
    /// Serializes automatic and explicit starts from this app.
    daemon_start: tokio::sync::Mutex<()>,
    daemon_runtime: OnceCell<daemon_service::DaemonRuntime>,
    daemon_status: Mutex<Value>,
    updater: updater::Updater,
    app_updates: app_update::Operations,
    app_update_bridge_stop: Notify,
    daemon_start_unblocked: AtomicBool,
    pub geometry: geometry::GeometryStore,
    quit: Mutex<QuitState>,
    routes: Mutex<Routes>,
    notifications: tokio::sync::Mutex<VecDeque<String>>,
    pub lifecycle_ready: AtomicBool,
    /// The shell hid its first window itself (dormant): it still owns routes.
    dormant: AtomicBool,
    log: Mutex<Option<std::fs::File>>,
    log_path: PathBuf,
    app_version: String,
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
            daemon_start: tokio::sync::Mutex::new(()),
            daemon_runtime: OnceCell::new(),
            daemon_status: Mutex::new(Value::Null),
            updater: updater::Updater::new(config.app_version.clone()),
            app_updates: Default::default(),
            app_update_bridge_stop: Notify::new(),
            daemon_start_unblocked: AtomicBool::new(
                std::env::var_os("PASEO_UPDATE_OPERATION_DIRECTORY").is_none(),
            ),
            geometry: geometry::GeometryStore::load(),
            quit: Mutex::new(QuitState::default()),
            routes: Mutex::new(Routes::default()),
            notifications: tokio::sync::Mutex::new(VecDeque::new()),
            lifecycle_ready: AtomicBool::new(false),
            dormant: AtomicBool::new(false),
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
        self.quit.lock().unwrap().is_quitting()
    }

    pub fn set_dormant(&self, dormant: bool) {
        self.dormant.store(dormant, Ordering::SeqCst);
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
            *log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log_path)
                .ok();
        }
        if let Some(file) = log.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    pub fn report(&self, message: &str) {
        self.log(message);
        post_event(PlatformEvent::HostError {
            message: message.to_owned(),
        });
    }

    /// The local daemon as onboarding shows it: (starting, last start failure, listening).
    pub fn local_daemon(&self) -> (bool, Option<String>, bool) {
        let status = self.daemon_status.lock().unwrap();
        (
            status["status"] == "starting",
            status["error"].as_str().map(str::to_owned),
            status["status"] == "running"
                && status["listen"].as_str().is_some_and(|s| !s.is_empty()),
        )
    }

    fn publish_daemon_status(&self, status: Value) {
        *self.daemon_status.lock().unwrap() = status.clone();
        post_event(PlatformEvent::LocalDaemonStatus { status });
    }

    async fn start_daemon(&self, explicit: bool) -> Result<Value, PlatformError> {
        if !self.daemon_start_unblocked.load(Ordering::SeqCst) {
            return Err(PlatformError::new(
                "APP_UPDATE_IN_PROGRESS",
                "Application update verification must finish before starting the local daemon.",
            ));
        }
        let _gate = self.daemon_start.lock().await;
        let result: Result<Value, PlatformError> = async {
            let enabled = explicit || self.settings().await?.start_daemon_if_not_running();
            let home = daemon_service::home()?;
            let mut observed = daemon_service::status(&self.daemon_runtime, &home).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            while observed["status"] == "starting" && !self.quitting() {
                if tokio::time::Instant::now() >= deadline {
                    return Err(PlatformError::new(
                        "DAEMON_TIMEOUT",
                        "The local daemon did not publish a listen address.",
                    ));
                }
                self.publish_daemon_status(observed);
                tokio::time::sleep(Duration::from_secs(1)).await;
                observed = daemon_service::status(&self.daemon_runtime, &home).await?;
            }
            daemon_service::ensure(
                &self.daemon_runtime,
                &home,
                enabled && !self.quitting(),
                observed,
                || self.publish_daemon_status(json!({"status":"starting","home":home})),
            )
            .await
        }
        .await;
        match &result {
            Ok(status) => self.publish_daemon_status(status.clone()),
            Err(error) => {
                self.log(&error.message);
                self.publish_daemon_status(json!({"status":"failed","error":error.message}));
            }
        }
        result
    }
}

pub(crate) fn start(shared: &Arc<Shared>, config: &PlatformConfig) {
    os::start_lifecycle(shared);
    if let Err(error) = os::init_notifications(shared) {
        shared.desktop.report(&error.message);
    }

    // Launch routing: an agent deep link in argv wins (first valid link only);
    // otherwise one project path (positional absolute directory or --open-project).
    let args: Vec<String> = config.argv.iter().skip(1).cloned().collect();
    if let Some((server_id, agent_id)) = deep_link::agent_deep_link_from_argv(&args) {
        shared
            .desktop
            .route(PlatformEvent::Route(PlatformRoute::Agent {
                server_id,
                agent_id,
            }));
    } else if let Some(path) = deep_link::open_project_path_from_argv(&args) {
        shared
            .desktop
            .route(PlatformEvent::Route(PlatformRoute::Project {
                path: path.display().to_string(),
                server_id: None,
            }));
    }

    let task_shared = shared.clone();
    shared.spawn(async move {
        match app_update::recover_health(&task_shared).await {
            Ok(true) => {
                task_shared
                    .desktop
                    .daemon_start_unblocked
                    .store(true, Ordering::SeqCst);
                app_update_owner::start(&task_shared);
                let _ = task_shared.desktop.start_daemon(false).await;
            }
            Ok(false) => {}
            Err(error) => task_shared.desktop.report(&error.message),
        }
    });
}

pub(crate) fn handle_event(shared: &Arc<Shared>) {
    let signals = std::mem::take(&mut *SIGNALS.lock().unwrap());
    for signal in signals {
        handle_signal(shared, signal);
    }
}

fn handle_signal(shared: &Arc<Shared>, signal: NativeSignal) {
    let desktop = &shared.desktop;
    match signal {
        NativeSignal::Activation(activation) => activate(shared, activation),
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        NativeSignal::SessionEnding => {
            // A cancellable OS query is intent, not consent to stop ongoing sessions.
            desktop.quit.lock().unwrap().quitting = true;
        }
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        NativeSignal::SessionEndingCancelled => {
            let mut quit = desktop.quit.lock().unwrap();
            if !quit.prepared {
                quit.quitting = false;
            }
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        NativeSignal::SessionEnded => {
            desktop.quit.lock().unwrap().quitting = true;
            request_quit(shared, QuitReason::Shutdown, true);
        }
        NativeSignal::LifecycleStarted(result) => match result {
            Ok(()) => desktop.lifecycle_ready.store(true, Ordering::SeqCst),
            // A desktop without a session manager is an environment capability,
            // not a failure the user can act on: record it without a banner.
            Err(error) if error.code == "logout_unavailable" => desktop.log(&error.message),
            Err(error) => desktop.report(&error.message),
        },
        #[cfg(target_os = "linux")]
        NativeSignal::LifecycleError(message) => {
            desktop.lifecycle_ready.store(false, Ordering::SeqCst);
            desktop.report(&message);
        }
        #[cfg(any(target_os = "macos", windows))]
        NativeSignal::Activated => {
            if !desktop.quitting() {
                os::show_windows();
            }
        }
        NativeSignal::NotificationClick(data) => match PlatformRoute::from_notification_data(&data)
        {
            Some(route) => show_route(shared, route),
            None => os::show_windows(),
        },
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        NativeSignal::QuitRequestedByOs => request_quit(shared, QuitReason::Quit, false),
    }
}

/// Only the first window processes routes. If it is dormant, reopen it
/// like New Window instead of routing while invisible.
fn show_route(shared: &Arc<Shared>, route: PlatformRoute) {
    os::show_windows();
    let event = if shared.desktop.dormant.swap(false, Ordering::SeqCst) {
        PlatformEvent::OpenWindow { route: Some(route) }
    } else {
        PlatformEvent::Route(route)
    };
    shared.desktop.route(event);
}

/// Original `_activate`.
fn activate(shared: &Arc<Shared>, activation: Activation) {
    let desktop = &shared.desktop;
    if desktop.quitting() {
        return;
    }
    if let Some(url) = activation.url {
        if let Some((server_id, agent_id)) = deep_link::parse_agent_deep_link(&url) {
            show_route(
                shared,
                PlatformRoute::Agent {
                    server_id,
                    agent_id,
                },
            );
        }
    } else if let Some(path) = activation.project_path {
        let route = PlatformRoute::Project {
            path,
            server_id: None,
        };
        if activation.initial {
            desktop.route(PlatformEvent::Route(route));
        } else {
            os::show_windows();
            desktop.route(PlatformEvent::OpenWindow { route: Some(route) });
        }
    } else {
        os::show_windows();
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
        if quit.prepared || (!force && (quit.remote_update_handoff || quit.pending.is_some())) {
            return;
        }
        quit.next_token += 1;
        let token = quit.next_token;
        if force {
            quit.quitting = true;
            if let Some(pending) = quit.pending.take()
                && let Some(waiter) = pending.waiter
            {
                let _ = waiter.send(false);
            }
        } else {
            quit.pending = Some(PendingQuit {
                token,
                waiter: None,
            });
        }
        token
    };
    if force {
        let shared = shared.clone();
        shared
            .clone()
            .spawn(async move { finish_quit(&shared).await });
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
            let task_shared = shared.clone();
            shared.spawn(async move { finish_quit(&task_shared).await });
        }
    }
}

/// Asks the shell for close-guard consent and waits (update installation).
async fn guarded_consent(shared: &Arc<Shared>, reason: QuitReason) -> bool {
    let (tx, rx) = oneshot::channel();
    let token = {
        let mut quit = shared.desktop.quit.lock().unwrap();
        if quit.pending.is_some() || quit.is_quitting() {
            return false;
        }
        quit.next_token += 1;
        let token = quit.next_token;
        quit.pending = Some(PendingQuit {
            token,
            waiter: Some(tx),
        });
        token
    };
    post_event(PlatformEvent::QuitRequested { token, reason });
    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(result) => result.unwrap_or(false),
        Err(_) => {
            let mut quit = shared.desktop.quit.lock().unwrap();
            if quit
                .pending
                .as_ref()
                .is_some_and(|pending| pending.token == token)
            {
                quit.pending = None;
            }
            false
        }
    }
}

async fn finish_quit(shared: &Arc<Shared>) {
    {
        let mut quit = shared.desktop.quit.lock().unwrap();
        if quit.prepared {
            return;
        }
        quit.quitting = true;
        quit.remote_update_handoff = false;
        quit.prepared = true;
    }
    shared.desktop.app_update_bridge_stop.notify_one();
    // Let in-flight notification deliveries settle before teardown.
    drop(shared.desktop.notifications.lock().await);
    let geometry_shared = shared.clone();
    let _ = tokio::task::spawn_blocking(move || geometry_shared.desktop.geometry.flush()).await;
    // macOS may be blocked inside a deferred `terminate:`; the reply must not
    // wait for the GUI event loop.
    os::termination_decided(true);
    post_event(PlatformEvent::QuitReady);
}

/// Returns true when this close belongs to the guarded application quit.
pub(crate) fn window_close_requested(shared: &Arc<Shared>, open_windows: usize) -> bool {
    request_last_window_close(open_windows, || {
        if !shared.desktop.quitting() {
            request_quit(shared, QuitReason::Quit, false);
        }
    })
}

fn request_last_window_close(open_windows: usize, quit: impl FnOnce()) -> bool {
    if open_windows > 1 {
        return false;
    }
    quit();
    true
}

// ----- invoke -----

pub(crate) fn invoke_command(shared: &Arc<Shared>, command: String, args: Value, done: Completion) {
    let task_shared = shared.clone();
    shared.spawn(async move {
        let result = invoke_inner(&task_shared, &command, &args).await;
        done.finish(result.map(PlatformValue::Json));
    });
}

async fn invoke_inner(
    shared: &Arc<Shared>,
    command: &str,
    args: &Value,
) -> Result<Value, PlatformError> {
    let desktop = &shared.desktop;
    match command {
        "desktop_settings_get" => Ok(Value::Object(desktop.settings().await?.value())),
        "desktop_settings_update" => {
            let settings = desktop.settings().await?;
            let patch = args
                .get("patch")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let patch_settings = settings.clone();
            let result = tokio::task::spawn_blocking(move || patch_settings.patch(&patch))
                .await
                .map_err(|e| PlatformError::native(e.to_string()))??;
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
        "check_app_update" => app_update::check_local(shared, args).await,
        "install_app_update" => {
            app_update::install_local(shared, args, app_update::Origin::Local).await
        }
        "app_update_status" => app_update::status_local(shared, args).await,
        "start_desktop_daemon" => desktop.start_daemon(true).await,
        "desktop_daemon_status" => {
            let current = desktop.daemon_status.lock().unwrap().clone();
            if current["status"] == "starting" {
                return Ok(current);
            }
            let mut status =
                daemon_service::status(&desktop.daemon_runtime, &daemon_service::home()?).await?;
            if status["status"] == "stopped" && current["status"] == "failed" {
                status["status"] = "failed".into();
                status["error"] = current["error"].clone();
            }
            Ok(status)
        }
        "desktop_daemon_logs" => {
            let home = daemon_service::home()?;
            let status = daemon_service::status(&desktop.daemon_runtime, &home).await?;
            let path = status["logPath"]
                .as_str()
                .map_or_else(|| home.join("daemon.log"), PathBuf::from);
            let read_path = path.clone();
            let contents = tokio::task::spawn_blocking(move || tail(&read_path, 128 * 1024))
                .await
                .map_err(|e| PlatformError::native(e.to_string()))??;
            Ok(json!({"logPath":path,"contents":contents}))
        }
        _ => Err(PlatformError::invalid(format!(
            "Unknown desktop command: {command}"
        ))),
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
        "daemonFailure": desktop.daemon_status.lock().unwrap()["error"],
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
            && keys.contains(key)
        {
            done.ok(PlatformValue::None);
            return;
        }
        if desktop.quit.lock().unwrap().prepared {
            done.fail(PlatformError::new(
                "shutting_down",
                "The application is shutting down.",
            ));
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
pub(crate) fn notification_clicked(
    payload: &str,
    notification_id: Option<i64>,
    action_id: Option<String>,
) {
    let mut data = match serde_json::from_str::<Value>(payload) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    };
    data.insert(
        "notificationId".into(),
        notification_id.map(Value::from).unwrap_or(Value::Null),
    );
    data.insert(
        "actionId".into(),
        action_id.map(Value::from).unwrap_or(Value::Null),
    );
    deliver(NativeSignal::NotificationClick(data));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_update_handoff_respects_existing_close_decisions() {
        let mut quit = QuitState::default();
        assert!(quit.reserve_remote_update());
        assert!(quit.is_quitting());
        assert!(!quit.reserve_remote_update());
        quit.remote_update_handoff = false;
        quit.pending = Some(PendingQuit {
            token: 1,
            waiter: None,
        });
        assert!(!quit.reserve_remote_update());
        quit.pending = None;
        quit.quitting = true;
        assert!(!quit.reserve_remote_update());
    }

    #[test]
    fn last_window_requests_guarded_quit_but_other_windows_can_close() {
        assert!(!request_last_window_close(2, || panic!(
            "not the last window"
        )));
        let mut quit = false;
        assert!(request_last_window_close(1, || quit = true));
        assert!(quit);
    }
}
