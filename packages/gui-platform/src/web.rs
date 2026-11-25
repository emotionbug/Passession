//! Browser operations run on the page thread. File chooser callbacks only post their result.

use crate::{
    Capabilities, NotificationRequest, OpenEditorRequest, PermissionSettings, PickFilesOptions,
    PickedFileBytes, PlatformError, PlatformEvent, PlatformValue,
    action::post_event,
    platform::{Completion, Shared},
};
use serde_json::Value;
use std::{path::Path, sync::Arc};

pub(crate) const NOTIFICATIONS_UNAVAILABLE: &str =
    "Notifications are not available in the browser.";
pub(crate) const DESKTOP_UNAVAILABLE: &str = "The desktop host is not available in the browser.";

pub(crate) fn capabilities() -> Capabilities {
    Capabilities::default()
}

pub(crate) fn report_error(_shared: &Arc<Shared>, message: &str) {
    post_event(PlatformEvent::HostError {
        message: message.to_owned(),
    });
}

pub(crate) fn acquire_audio_session() -> anyhow::Result<()> {
    // Makepad's Web Audio backend owns its audio context.
    Ok(())
}

pub(crate) fn release_audio_session() {}

pub(crate) fn open_external(_shared: &Arc<Shared>, url: String, done: Completion) {
    paseo_browser::open_url(&url);
    done.ok(PlatformValue::Submitted);
}

pub(crate) fn copy_text(_shared: &Arc<Shared>, text: String, done: Completion) {
    paseo_browser::clipboard_write(&text);
    done.ok(PlatformValue::Submitted);
}

pub(crate) fn pick_files(_shared: &Arc<Shared>, options: PickFilesOptions, done: Completion) {
    let accept = if options.images_only { "image/*" } else { "" };
    paseo_browser::pick_files(accept, options.multiple, move |result| {
        let files = result.map(|files| {
            files
                .into_iter()
                .map(|file| PickedFileBytes {
                    name: file.name,
                    mime_type: (!file.mime.is_empty()).then_some(file.mime),
                    bytes: Arc::new(file.data),
                })
                .collect()
        });
        done.finish(
            files
                .map(PlatformValue::FileBytes)
                .map_err(PlatformError::native),
        );
    });
}

pub(crate) fn download_file(name: &str, mime_type: &str, bytes: &[u8], done: Completion) {
    paseo_browser::download(name, mime_type, bytes);
    done.ok(PlatformValue::Submitted);
}

macro_rules! unsupported_requests {
    ($($name:ident($($arg:ty),*) => $message:expr;)*) => {
        $(pub(crate) fn $name($(_: $arg,)* done: Completion) {
            done.fail(PlatformError::unsupported($message));
        })*
    };
}

unsupported_requests! {
    open_path(&Path) => "Opening local paths is not available in the browser.";
    reveal_path(&Path) => "Revealing local paths is not available in the browser.";
    pick_directory(&Arc<Shared>, Option<String>) => "Directory picking is not available in the browser.";
    save_location(&str) => "The browser chooses where to save downloads.";
    save_file(&Path, &str, &str) => "Download the file's bytes instead of saving a local path.";
    save_image(&Path, &str, &str) => "Saving to a photo library is not available in the browser.";
    share_file(&Path, &str, &str) => "Native file sharing is not available in the browser.";
    read_clipboard(&Arc<Shared>, bool) => "Reading the clipboard through the platform layer is not available in the browser.";
    copy_image(Vec<u8>, &str) => "Copying images to the clipboard is not available in the browser.";
    notify(&Arc<Shared>, NotificationRequest) => NOTIFICATIONS_UNAVAILABLE;
    request_notification_permission(&Arc<Shared>) => NOTIFICATIONS_UNAVAILABLE;
    notification_permission_status(&Arc<Shared>) => NOTIFICATIONS_UNAVAILABLE;
    open_permission_settings(&Arc<Shared>, PermissionSettings) => "Native permission settings are not available in the browser.";
    background_restrictions(&Arc<Shared>) => "Native background restrictions are not available in the browser.";
    set_badge_count(&Arc<Shared>, u32) => "App badging is not available in the browser.";
    idle_milliseconds(&Arc<Shared>) => "System idle time is not available in the browser.";
    camera_permission(&Arc<Shared>, bool) => "QR scanning is not available in the browser.";
    desktop_invoke(&str, Value) => DESKTOP_UNAVAILABLE;
    list_editor_targets() => "Local editors are not available in the browser.";
    open_editor(OpenEditorRequest) => "Local editors are not available in the browser.";
    revoke_push_host(&str) => NOTIFICATIONS_UNAVAILABLE;
}
