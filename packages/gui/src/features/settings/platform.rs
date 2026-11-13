//! The only place settings code touches `paseo-gui-platform` (platform-api.md).
//! Calls are non-blocking `PlatformHandle` requests; their results arrive as
//! `PlatformAction::Completed` in `Event::Actions`, which the panel routes back
//! to the waiting continuation by request id.

use makepad_widgets::*;
use paseo_gui_platform::{
    NotificationRequest, PermissionState, PlatformAction, PlatformError, PlatformRequestId, PlatformValue, platform,
};
use serde_json::{Map, Value};

pub const DESKTOP: bool = cfg!(any(target_os = "linux", target_os = "macos", target_os = "windows"));
pub const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));
pub const APPLE: bool = cfg!(any(target_os = "macos", target_os = "ios"));

/// Build-time client version (Flutter `--dart-define=PASEO_VERSION`), falling
/// back to the GUI crate version like the shell's `PlatformConfig.app_version`.
pub fn app_version() -> Option<String> {
    let raw = option_env!("PASEO_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")).trim();
    if raw.is_empty() { None } else { Some(raw.trim_start_matches(['v', 'V']).to_string()) }
}

pub fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// Whether the native desktop host service is running (Flutter `DesktopHost.instance != null`).
pub fn desktop_available() -> bool {
    DESKTOP && platform().map(|p| p.capabilities().desktop_host).unwrap_or(false)
}

pub type Reply = (PlatformRequestId, Result<Value, String>);

fn unavailable() -> String {
    super::i18n::t("settingsNative.desktopUnavailable")
}

fn to_value(value: &PlatformValue) -> Value {
    match value {
        PlatformValue::None => Value::Null,
        PlatformValue::Bool(flag) => Value::Bool(*flag),
        PlatformValue::Text(text) => text.clone().map(Value::String).unwrap_or(Value::Null),
        PlatformValue::Json(json) => json.clone(),
        PlatformValue::Permission(state) => Value::String(
            match state {
                PermissionState::Granted | PermissionState::Provisional => "granted",
                PermissionState::Denied => "denied",
                PermissionState::NotDetermined => "notDetermined",
            }
            .into(),
        ),
        _ => Value::Null,
    }
}

fn error_text(error: &PlatformError) -> String {
    if error.message.is_empty() { error.code.clone() } else { error.message.clone() }
}

/// Issues platform requests and converts their completions. Requests that
/// cannot start (no platform handle) complete on the next `collect`.
#[derive(Default)]
pub struct Bridge {
    failed: Vec<Reply>,
    next_local: u64,
}

impl Bridge {
    fn local_failure(&mut self, message: String) -> PlatformRequestId {
        // Local ids live in the top half so they never collide with platform ids.
        self.next_local += 1;
        let id = u64::MAX - self.next_local;
        self.failed.push((id, Err(message)));
        id
    }

    /// Desktop host IPC command (`desktop_settings_get`, `start_desktop_daemon`, …).
    pub fn desktop(&mut self, command: &str, args: Value) -> PlatformRequestId {
        match platform() {
            Some(handle) if DESKTOP && handle.capabilities().desktop_host => handle.desktop_invoke(command, args),
            _ => self.local_failure(unavailable()),
        }
    }

    pub fn request_notification_permission(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.request_notification_permission(),
            None => self.local_failure(unavailable()),
        }
    }

    /// Current notification permission without prompting (`granted` /
    /// `denied` / `notDetermined`).
    pub fn notification_permission_status(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.notification_permission_status(),
            None => self.local_failure(unavailable()),
        }
    }

    pub fn notify(&mut self, id: i32, title: String, body: String) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.notify(NotificationRequest { id, title, body: Some(body), data: Map::new(), event_key: None, sound: None }),
            None => self.local_failure(unavailable()),
        }
    }

    /// Original diagnostics `handlePlaybackTest`: stop current output, then
    /// play the thinking tone; completes when playback finishes.
    pub fn play_test_tone(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => {
                handle.cancel_audio();
                handle.play_audio(paseo_gui_platform::audio::THINKING_TONE_PCM16_16K.to_vec(), paseo_gui_platform::audio::THINKING_TONE_MIME)
            }
            None => self.local_failure(unavailable()),
        }
    }

    /// Completions in this event (platform actions plus local failures).
    pub fn collect(&mut self, event: &Event) -> Vec<Reply> {
        let mut out: Vec<Reply> = std::mem::take(&mut self.failed);
        if let Event::Actions(actions) = event {
            for action in actions.iter() {
                if let Some(PlatformAction::Completed { request, result }) = action.downcast_ref::<PlatformAction>() {
                    out.push((*request, result.as_ref().map(to_value).map_err(error_text)));
                }
            }
        }
        out
    }
}
