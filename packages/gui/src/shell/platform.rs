//! The shell's adapter to `paseo-gui-platform`. Asynchronous results arrive as
//! `PlatformAction`s in `Event::Actions`.

use paseo_gui_platform::{
    PermissionSettings, PlatformAction, PlatformEvent, PlatformHandle, PlatformRequestId,
    QuitReason,
};

fn handle() -> Option<PlatformHandle> {
    paseo_gui_platform::platform()
}

/// OS dark appearance for theme `auto`.
pub fn system_prefers_dark() -> bool {
    paseo_gui_platform::appearance::system_prefers_dark()
}

pub fn open_external(url: &str) -> Option<PlatformRequestId> {
    handle().map(|h| h.open_external(url))
}

pub fn copy_text(text: String) -> Option<PlatformRequestId> {
    handle().map(|h| h.copy_text(text))
}

/// Opens the system settings screen where `target` is turned on (completes `Err` where the
/// OS has none).
pub fn open_permission_settings(target: PermissionSettings) -> Option<PlatformRequestId> {
    handle().map(|h| h.open_permission_settings(target))
}

/// The OS notification permission, without prompting.
pub fn notification_permission_status() -> Option<PlatformRequestId> {
    handle().map(|h| h.notification_permission_status())
}

/// Shows the OS notification prompt where one can still show; completes with the state.
pub fn request_notification_permission() -> Option<PlatformRequestId> {
    handle().map(|h| h.request_notification_permission())
}

pub fn desktop_host() -> bool {
    handle().is_some_and(|h| h.capabilities().desktop_host)
}

pub fn window_close_requested(open_windows: usize) -> bool {
    handle().is_some_and(|h| h.window_close_requested(open_windows))
}

/// Hides the dormant first window until a new-window request reuses it.
pub fn hide_window(cx: &mut makepad_widgets::Cx, window: makepad_widgets::WindowId) {
    if let Some(h) = handle() {
        h.hide_window(cx, window);
    }
}

/// Shows a window hidden by [`hide_window`] again (restoring it if minimized) and activates it.
pub fn show_window(cx: &mut makepad_widgets::Cx, window: makepad_widgets::WindowId) {
    if let Some(h) = handle() {
        h.show_window(cx, window);
    }
}

pub fn request_quit() {
    if let Some(h) = handle() {
        h.request_quit(QuitReason::Quit, false);
    }
}

pub fn respond_quit(token: u64, allowed: bool) {
    if let Some(h) = handle() {
        h.respond_quit(token, allowed);
    }
}

pub fn report_error(message: &str) {
    if let Some(h) = handle() {
        h.report_error(message);
    }
}

pub fn revoke_push_host(host_id: &str) -> Option<PlatformRequestId> {
    handle()
        .filter(|h| h.capabilities().push)
        .map(|h| h.revoke_push_host(host_id))
}

pub fn window_geometry(key: &str) -> Option<paseo_gui_platform::WindowGeometry> {
    handle().and_then(|h| h.window_geometry(key))
}

pub fn record_window_geometry(key: &str, geometry: paseo_gui_platform::WindowGeometry) {
    if let Some(h) = handle() {
        h.record_window_geometry(key, geometry);
    }
}

pub fn safe_insets() -> paseo_gui_platform::SafeInsets {
    handle()
        .map(|h| h.safe_insets())
        .unwrap_or(paseo_gui_platform::SafeInsets {
            top: 0.0,
            bottom: 0.0,
            left: 0.0,
            right: 0.0,
            keyboard: 0.0,
            keyboard_visible: false,
        })
}

pub fn pick_image_file() -> Option<PlatformRequestId> {
    handle().map(|h| {
        h.pick_files(paseo_gui_platform::PickFilesOptions {
            multiple: false,
            images_only: true,
            title: None,
        })
    })
}

/// Platform actions in an `Event::Actions` batch.
pub fn actions(actions: &makepad_widgets::Actions) -> Vec<PlatformAction> {
    actions
        .iter()
        .filter_map(|action| action.downcast_ref::<PlatformAction>().cloned())
        .collect()
}

pub type Event = PlatformEvent;
