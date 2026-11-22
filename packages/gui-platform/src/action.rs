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
    pub fn completed(
        &self,
        request: PlatformRequestId,
    ) -> Option<&Result<PlatformValue, PlatformError>> {
        match self {
            Self::Completed {
                request: id,
                result,
            } if *id == request => Some(result),
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
        Self {
            code: code.into(),
            message: message.into(),
        }
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
    PhotoLibrarySaved,
    Clipboard(ClipboardContent),
    EditorTargets(Vec<EditorTarget>),
    Permission(PermissionState),
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

/// A private, process-local copy of an Android Sharesheet selection. Keep the
/// lease alive through the local import; dropping it removes the staging batch.
#[derive(Debug)]
pub struct SharedImages {
    pub(crate) directory: PathBuf,
    pub(crate) files: Vec<PickedFile>,
}

impl SharedImages {
    pub fn files(&self) -> &[PickedFile] {
        &self.files
    }
}

impl Drop for SharedImages {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn inbound_share_copies_exist_only_while_a_lease_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("paseo-inbound-share/batch-test");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("image-1");
    std::fs::write(&path, b"staged").unwrap();
    let chooser = Arc::new(SharedImages {
        directory: directory.clone(),
        files: Vec::new(),
    });
    let worker = chooser.clone();
    drop(chooser);
    assert!(path.exists());
    drop(worker);
    assert!(!directory.exists());
}

#[derive(Debug, Clone, Copy)]
pub enum ImageShareError {
    Invalid,
    Unsupported,
    Limits,
    Unreadable,
    Busy,
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
    /// Denied, but asking again may still show the OS prompt (Android while
    /// it would show a rationale).
    Denied,
    Provisional,
    /// Asking shows the OS prompt.
    NotDetermined,
    /// The OS will not prompt again; only system settings can change it.
    Blocked,
}

impl PermissionState {
    pub fn allowed(self) -> bool {
        matches!(self, Self::Granted | Self::Provisional)
    }
}

/// The system settings screen [`PlatformHandle::open_permission_settings`]
/// opens. A platform without the precise screen opens the closest one it has
/// and fails with `unsupported` when it has none.
///
/// [`PlatformHandle::open_permission_settings`]: crate::PlatformHandle::open_permission_settings
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionSettings {
    /// The app's notification settings.
    Notifications,
    /// One notification channel (Android, e.g. `"tailnet"`); elsewhere the
    /// app's notification settings.
    NotificationChannel(String),
    /// Where camera access is granted (mobile).
    Camera,
    /// The app's own settings screen (mobile).
    App,
    /// Battery optimization exemptions (Android).
    BatteryOptimization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    Quit,
    Shutdown,
    /// Component activation is refused when there are unsaved documents.
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

impl SafeInsets {
    /// Occlusion remaining inside the render surface. The IME and navigation
    /// bar cover the same bottom edge, so reserve their union, not their sum.
    pub fn bottom_occlusion(self) -> f64 {
        if self.keyboard_visible {
            self.bottom.max(self.keyboard)
        } else {
            self.bottom
        }
    }
}

#[cfg(test)]
mod inset_tests {
    use super::SafeInsets;

    #[test]
    fn keyboard_and_navigation_share_the_bottom_edge() {
        let mut insets = SafeInsets {
            bottom: 34.0,
            keyboard: 320.0,
            keyboard_visible: true,
            ..SafeInsets::default()
        };
        assert_eq!(insets.bottom_occlusion(), 320.0);
        insets.keyboard_visible = false;
        assert_eq!(insets.bottom_occlusion(), 34.0);
    }

    #[test]
    fn consumed_or_floating_keyboard_keeps_remaining_navigation_inset() {
        let insets = SafeInsets {
            bottom: 34.0,
            keyboard: 0.0,
            keyboard_visible: true,
            ..SafeInsets::default()
        };
        assert_eq!(insets.bottom_occlusion(), 34.0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformRoute {
    Agent {
        server_id: String,
        agent_id: String,
    },
    ExecutionLog {
        server_id: String,
        workspace_id: String,
        terminal_id: String,
    },
    Project {
        path: String,
        server_id: Option<String>,
    },
    HostNewTab {
        server_id: String,
    },
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
            return Some(Self::ExecutionLog {
                server_id,
                workspace_id,
                terminal_id,
            });
        }
        if let Some(agent_id) = text("agentId") {
            return Some(Self::Agent {
                server_id,
                agent_id,
            });
        }
        Some(Self::HostNewTab { server_id })
    }
}

/// What the embedded Tailscale connection is doing, for the OS background
/// presence (Android foreground-service notification).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailnetPresenceState {
    Connected,
    Connecting,
    Paused,
    /// Sign-in, approval or an error needs the app.
    NeedsAttention,
}

/// The background presence of an enabled embedded Tailscale connection. The
/// app localizes every label; platforms only lay them out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailnetPresence {
    pub state: TailnetPresenceState,
    /// "Embedded Tailscale"
    pub title: String,
    /// One-line status, e.g. "Connected" or "Paused".
    pub status: String,
    pub pause: String,
    pub resume: String,
    pub stop: String,
}

/// A background-presence control the user chose outside the app window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailnetControl {
    /// Disconnect until resumed; keeps Embedded Tailscale enabled.
    Pause,
    Resume,
    /// Turn Embedded Tailscale off (the Settings disconnect).
    Stop,
}

#[derive(Debug, Clone)]
pub enum PlatformEvent {
    Route(PlatformRoute),
    SharedImages(Result<Arc<SharedImages>, ImageShareError>),
    ConnectionLink {
        url: String,
    },
    /// The guarded quit flow: run every window's close guard, then call
    /// `PlatformHandle::respond_quit(token, allowed)`. Forced OS shutdown skips
    /// this and goes straight to `QuitReady`.
    QuitRequested {
        token: u64,
        reason: QuitReason,
    },
    /// App quit preparation is complete; call `cx.quit()`.
    QuitReady,
    QuitCancelled,
    /// A later launch asked for a new window, optionally opening a route in it.
    OpenWindow {
        route: Option<PlatformRoute>,
    },
    LocalDaemonStatus {
        status: Value,
    },
    HostError {
        message: String,
    },
    Insets(SafeInsets),
    /// System theme or locale changed; refresh the app's resolved appearance.
    AppearanceChanged,
    AudioInterrupted,
    CameraFrame,
    QrScanned {
        link: String,
    },
    /// The user chose a tailnet control in the OS background presence.
    TailnetControl(TailnetControl),
}

pub(crate) fn post_event(event: PlatformEvent) {
    Cx::post_action(PlatformAction::Event(event));
}

pub(crate) fn post_result(
    request: PlatformRequestId,
    result: Result<PlatformValue, PlatformError>,
) {
    let waiter = WAITERS
        .lock()
        .unwrap()
        .as_mut()
        .and_then(|waiters| waiters.remove(&request));
    if let Some(waiter) = waiter {
        let _ = waiter.send(result);
        return;
    }
    Cx::post_action(PlatformAction::Completed { request, result });
}

type Waiter = std::sync::mpsc::Sender<Result<PlatformValue, PlatformError>>;
static WAITERS: std::sync::Mutex<Option<std::collections::HashMap<PlatformRequestId, Waiter>>> =
    std::sync::Mutex::new(None);

/// Routes the result of `request` to the returned receiver instead of the UI.
/// Register before the request can complete.
pub(crate) fn wait_for(
    request: PlatformRequestId,
) -> std::sync::mpsc::Receiver<Result<PlatformValue, PlatformError>> {
    let (tx, rx) = std::sync::mpsc::channel();
    WAITERS
        .lock()
        .unwrap()
        .get_or_insert_with(Default::default)
        .insert(request, tx);
    rx
}
