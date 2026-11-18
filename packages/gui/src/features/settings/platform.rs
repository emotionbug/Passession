//! The only place settings code touches `paseo-gui-platform` (platform-api.md).
//! Calls are non-blocking `PlatformHandle` requests; their results arrive as
//! `PlatformAction::Completed` in `Event::Actions` and are delivered only to
//! the settings instance with a pending continuation for that request id.

use makepad_widgets::*;
use paseo_gui_platform::{
    NotificationRequest, PermissionSettings, PermissionState, PlatformAction, PlatformError,
    PlatformRequestId, PlatformValue, platform,
};
use serde_json::{Map, Value};

use super::rpc::{Env, ReqError};
use super::state::Settings;

pub const DESKTOP: bool = cfg!(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
));
pub const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));
pub const ANDROID: bool = cfg!(target_os = "android");
pub const APPLE: bool = cfg!(any(target_os = "macos", target_os = "ios"));

/// Build-time client version (Flutter `--dart-define=PASEO_VERSION`), falling
/// back to the GUI crate version like the shell's `PlatformConfig.app_version`.
pub fn app_version() -> Option<String> {
    let raw = option_env!("PASEO_VERSION")
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.trim_start_matches(['v', 'V']).to_string())
    }
}

pub fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// Whether the native desktop host service is running (Flutter `DesktopHost.instance != null`).
pub fn desktop_available() -> bool {
    DESKTOP
        && platform()
            .map(|p| p.capabilities().desktop_host)
            .unwrap_or(false)
}

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
                PermissionState::Blocked => "blocked",
                PermissionState::NotDetermined => "notDetermined",
            }
            .into(),
        ),
        _ => Value::Null,
    }
}

fn error_text(error: &PlatformError) -> String {
    if error.message.is_empty() {
        error.code.clone()
    } else {
        error.message.clone()
    }
}

/// Issues platform requests and converts their completions. Requests that
/// cannot start (no platform handle) complete on the next event.
#[derive(Default)]
pub struct Bridge {
    failed: Vec<(PlatformRequestId, String)>,
    next_local: u64,
    #[cfg(test)]
    pub(super) desktop_requests: Option<Vec<(PlatformRequestId, String, Value)>>,
}

impl Bridge {
    fn local_failure(&mut self, message: String) -> PlatformRequestId {
        // Local ids live in the top half so they never collide with platform ids.
        self.next_local += 1;
        let id = u64::MAX - self.next_local;
        self.failed.push((id, message));
        id
    }

    #[cfg(test)]
    pub(super) fn record_desktop() -> Self {
        Self {
            desktop_requests: Some(Vec::new()),
            ..Self::default()
        }
    }

    /// Desktop host IPC command (`desktop_settings_get`, `start_desktop_daemon`, …).
    pub fn desktop(&mut self, command: &str, args: Value) -> PlatformRequestId {
        #[cfg(test)]
        if let Some(requests) = &mut self.desktop_requests {
            self.next_local += 1;
            let id = u64::MAX - self.next_local;
            requests.push((id, command.to_owned(), args));
            return id;
        }
        match platform() {
            Some(handle) if DESKTOP && handle.capabilities().desktop_host => {
                handle.desktop_invoke(command, args)
            }
            _ => self.local_failure(unavailable()),
        }
    }

    pub fn request_notification_permission(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.request_notification_permission(),
            None => self.local_failure(unavailable()),
        }
    }

    /// Current notification permission without prompting (`granted` / `denied` /
    /// `blocked` / `notDetermined`).
    pub fn notification_permission_status(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.notification_permission_status(),
            None => self.local_failure(unavailable()),
        }
    }

    /// The app's notification settings; `channel` opens one Android channel's settings.
    pub fn open_notification_settings(&mut self, channel: Option<&str>) -> PlatformRequestId {
        self.open_permission_settings(match channel {
            Some(channel) => PermissionSettings::NotificationChannel(channel.to_string()),
            None => PermissionSettings::Notifications,
        })
    }

    /// Android: the battery optimization list, or the app's details when the system
    /// restricts its background work (only that screen lifts it).
    pub fn open_background_settings(&mut self, restricted: bool) -> PlatformRequestId {
        self.open_permission_settings(if restricted {
            PermissionSettings::App
        } else {
            PermissionSettings::BatteryOptimization
        })
    }

    fn open_permission_settings(&mut self, target: PermissionSettings) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.open_permission_settings(target),
            None => self.local_failure(unavailable()),
        }
    }

    /// Android: `{batteryOptimized, backgroundRestricted}`, never prompting.
    pub fn background_restrictions(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.background_restrictions(),
            None => self.local_failure(unavailable()),
        }
    }

    pub fn notify(&mut self, id: i32, title: String, body: String) -> PlatformRequestId {
        match platform() {
            Some(handle) => handle.notify(NotificationRequest {
                id,
                title,
                body: Some(body),
                data: Map::new(),
                event_key: None,
                sound: None,
            }),
            None => self.local_failure(unavailable()),
        }
    }

    /// Original diagnostics `handlePlaybackTest`: stop current output, then
    /// play the thinking tone; completes when playback finishes.
    pub fn play_test_tone(&mut self) -> PlatformRequestId {
        match platform() {
            Some(handle) => {
                handle.cancel_audio();
                handle.play_audio(
                    paseo_gui_platform::audio::THINKING_TONE_PCM16_16K.to_vec(),
                    paseo_gui_platform::audio::THINKING_TONE_MIME,
                )
            }
            None => self.local_failure(unavailable()),
        }
    }
}

/// Runs this instance's continuations and reports whether any reply was applied.
pub(super) fn handle_event(s: &mut Settings, env: &mut Env, event: &Event) -> bool {
    let mut applied = false;
    // Detach this batch first: a continuation can queue another local failure.
    for (request, message) in std::mem::take(&mut env.bridge.failed) {
        if let Some(then) = s.take_bridge_job(request) {
            then(s, env, Err(ReqError::msg(message)));
            applied = true;
        }
    }
    if s.has_bridge_jobs()
        && let Event::Actions(actions) = event
    {
        for action in actions {
            if let Some(PlatformAction::Completed { request, result }) =
                action.downcast_ref::<PlatformAction>()
                && let Some(then) = s.take_bridge_job(*request)
            {
                let result = result
                    .as_ref()
                    .map(to_value)
                    .map_err(|error| ReqError::msg(error_text(error)));
                then(s, env, result);
                applied = true;
            }
        }
    }
    applied
}
