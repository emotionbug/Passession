//! Results and unsolicited platform events. Everything reaches the GUI thread
//! through `Cx::post_action`, so feature widgets read them from `Event::Actions`.

use makepad_widgets::Cx;
use serde_json::{Map, Value};
use std::{path::PathBuf, sync::Arc};

pub type PlatformRequestId = u64;

#[derive(Debug, Clone)]
pub enum PlatformAction {
    Completed {
        request: PlatformRequestId,
        result: Result<PlatformValue, PlatformError>,
    },
    Event(PlatformEvent),
}

impl PlatformAction {
    /// The result for `request`, if this action completes it.
    pub fn completed(&self, request: PlatformRequestId) -> Option<&Result<PlatformValue, PlatformError>> {
        match self {
            Self::Completed { request: id, result } if *id == request => Some(result),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformError {
    pub code: String,
    pub message: String,
}

impl PlatformError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::new("unsupported", message)
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid", message)
    }
    pub fn native(message: impl Into<String>) -> Self {
        Self::new("native", message)
    }
    pub fn io(error: &std::io::Error) -> Self {
        Self::new("io", error.to_string())
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlatformError {}

impl From<anyhow::Error> for PlatformError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast::<PlatformError>() {
            Ok(error) => error,
            Err(error) => Self::native(format!("{error:#}")),
        }
    }
}

impl From<std::io::Error> for PlatformError {
    fn from(error: std::io::Error) -> Self {
        Self::io(&error)
    }
}

#[derive(Debug, Clone)]
pub enum PlatformValue {
    None,
    Bool(bool),
    Text(Option<String>),
    Json(Value),
    /// An empty list means the user cancelled.
    Files(Vec<PickedFile>),
    Directory(Option<PathBuf>),
    SaveLocation(Option<PathBuf>),
    Clipboard(ClipboardContent),
    EditorTargets(Vec<EditorTarget>),
    Permission(PermissionState),
    CloseDecision(CloseDecision),
    IdleMilliseconds(u64),
}

#[derive(Debug, Clone)]
pub struct PickedFile {
    pub path: PathBuf,
    pub name: String,
    pub mime_type: Option<String>,
    pub size: u64,
    /// The file is a private copy (mobile document providers); delete it after use.
    pub temporary: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ClipboardContent {
    pub images: Vec<ClipboardImage>,
    pub text: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub bytes: Arc<Vec<u8>>,
    pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTarget {
    pub id: String,
    pub label: String,
    /// `"editor"` or `"file-manager"`.
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    Provisional,
    NotDetermined,
}

impl PermissionState {
    pub fn allowed(self) -> bool {
        matches!(self, Self::Granted | Self::Provisional)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// The last open window hides; the tray keeps the app (and its daemon) alive.
    HideToTray,
    /// Other windows remain: run this window's close guard, then close it.
    CloseWindow,
    /// The last window without a usable tray: the guarded quit flow has started
    /// (a `QuitRequested` event follows).
    QuitApplication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    Quit,
    Shutdown,
    AppUpdate,
}

impl QuitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Shutdown => "shutdown",
            Self::AppUpdate => "app_update",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SafeInsets {
    pub top: f64,
    pub bottom: f64,
    pub left: f64,
    pub right: f64,
    pub keyboard: f64,
    pub keyboard_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformRoute {
    Agent { server_id: String, agent_id: String },
    ExecutionLog { server_id: String, workspace_id: String, terminal_id: String },
    Project { path: String, server_id: Option<String> },
    HostNewTab { server_id: String },
}

impl PlatformRoute {
    /// Notification payload routing shared by desktop clicks and mobile taps:
    /// workspace + terminal opens the log, an agent opens the session, else the host.
    pub fn from_notification_data(data: &Map<String, Value>) -> Option<Self> {
        let text = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let server_id = text("serverId")?;
        if let (Some(workspace_id), Some(terminal_id)) = (text("workspaceId"), text("terminalId")) {
            return Some(Self::ExecutionLog { server_id, workspace_id, terminal_id });
        }
        if let Some(agent_id) = text("agentId") {
            return Some(Self::Agent { server_id, agent_id });
        }
        Some(Self::HostNewTab { server_id })
    }
}

#[derive(Debug, Clone)]
pub enum PlatformEvent {
    Route(PlatformRoute),
    ConnectionLink { url: String },
    ShowWindow,
    /// The guarded quit flow: run every window's close guard, then call
    /// `PlatformHandle::respond_quit(token, allowed)`. Forced quits (OS shutdown,
    /// update installation) skip this and go straight to `QuitReady`.
    QuitRequested { token: u64, reason: QuitReason },
    /// The desktop-host helper is closed; call `cx.quit()`.
    QuitReady,
    QuitCancelled,
    /// A later launch asked for a new window, optionally opening a route in it.
    OpenWindow { route: Option<PlatformRoute> },
    LocalDaemonStatus { status: Value },
    HostError { message: String },
    Insets(SafeInsets),
    /// System theme or locale changed; refresh the app's resolved appearance.
    AppearanceChanged,
    AudioInterrupted,
    CameraFrame,
    QrScanned { link: String },
}

pub(crate) fn post_event(event: PlatformEvent) {
    Cx::post_action(PlatformAction::Event(event));
}

pub(crate) fn post_result(request: PlatformRequestId, result: Result<PlatformValue, PlatformError>) {
    let waiter = WAITERS.lock().unwrap().as_mut().and_then(|waiters| waiters.remove(&request));
    if let Some(waiter) = waiter {
        let _ = waiter.send(result);
        return;
    }
    Cx::post_action(PlatformAction::Completed { request, result });
}

type Waiter = std::sync::mpsc::Sender<Result<PlatformValue, PlatformError>>;
static WAITERS: std::sync::Mutex<Option<std::collections::HashMap<PlatformRequestId, Waiter>>> = std::sync::Mutex::new(None);

/// Routes the result of `request` to the returned receiver instead of the UI.
/// Register before the request can complete.
pub(crate) fn wait_for(request: PlatformRequestId) -> std::sync::mpsc::Receiver<Result<PlatformValue, PlatformError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    WAITERS.lock().unwrap().get_or_insert_with(Default::default).insert(request, tx);
    rx
}
