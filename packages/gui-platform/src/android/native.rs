//! `crate::native` for Android.

use super::{call, call_async, call_await, object};
use crate::{
    action::*,
    audio::engine::AudioSink,
    paths::AppPaths,
    platform::{Capabilities, Completion, PickFilesOptions, PlatformState, Shared},
    NotificationRequest,
};
use makepad_widgets::{Cx, Event};
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

pub(crate) fn prepare_process(_cx: &mut Cx) -> anyhow::Result<()> {
    Ok(())
}

pub(crate) fn os_app_paths() -> AppPaths {
    // getFilesDir() is path_provider's application support directory on Android.
    let paths = call("paths", json!({})).ok().map(object).unwrap_or_default();
    let text = |key: &str| paths.get(key).and_then(Value::as_str).map(PathBuf::from);
    let cache = text("cache").unwrap_or_else(std::env::temp_dir);
    AppPaths {
        support_dir: text("files").unwrap_or_else(|| PathBuf::from("/data/data/sh.paseo.sessions/files")),
        temp_dir: cache.clone(),
        cache_dir: cache,
    }
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        desktop_host: false,
        tray: false,
        editors: false,
        updater: false,
        directory_picker: false,
        save_dialog: false,
        push: true,
        qr_scan: true,
        share: true,
        idle: false,
    }
}

pub(crate) fn start_services(_cx: &mut Cx, _shared: &Arc<Shared>, state: &mut PlatformState) {
    // App audio uses AudioTrack on every supported Android version (API 24+),
    // with the original transient-exclusive audio focus.
    state.audio.set_sink(AudioSink {
        play: Arc::new(|request, audio| {
            let pcm: Vec<u8> = audio
                .samples
                .iter()
                .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
                .collect();
            if let Err(error) = super::audio_play(request, &pcm, audio.channels as i32, crate::audio::OUTPUT_RATE as i32) {
                post_result(request, Err(error));
            }
        }),
        cancel: Arc::new(super::audio_cancel),
    });
    // Links and taps that launched the activity are replayed now.
    let _ = call("startServices", json!({}));
}

pub(crate) fn handle_event(_cx: &mut Cx, _shared: &Arc<Shared>, _state: &mut PlatformState, event: &Event) {
    if let Event::Resume = event {
        crate::mobile::deliver(crate::mobile::MobileSignal::Resumed);
    }
}

pub(crate) fn report_error(_shared: &Arc<Shared>, message: &str) {
    post_event(PlatformEvent::HostError { message: message.to_owned() });
}

/// Back past the shell's own navigation: leave the app without finishing the
/// activity, so remote sessions are not stopped.
pub(crate) fn background_app(_cx: &mut Cx) {
    let _ = call("moveTaskToBack", json!({}));
}

/// The Java AudioTrack player owns audio focus.
pub(crate) fn acquire_audio_session() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn release_audio_session() {}

fn none(_: Value) -> Result<PlatformValue, PlatformError> {
    Ok(PlatformValue::None)
}

pub(crate) fn open_external(shared: &Arc<Shared>, url: String, done: Completion) {
    shared.complete_blocking(done, move || call("openUrl", json!({"url": url})).and_then(none));
}

pub(crate) fn open_path(shared: &Arc<Shared>, path: PathBuf, done: Completion) {
    shared.complete_blocking(done, move || {
        let mime = mime_for(&path);
        call("openPath", json!({"path": path, "mime": mime})).and_then(none)
    });
}

pub(crate) fn reveal_path(_shared: &Arc<Shared>, _path: PathBuf, done: Completion) {
    done.fail(PlatformError::unsupported("Android has no file manager reveal; use share or open."));
}

fn mime_for(path: &std::path::Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    crate::paths::mime_for_name(&name).unwrap_or_else(|| "application/octet-stream".into())
}

fn files(value: Value) -> Result<PlatformValue, PlatformError> {
    let items = value.as_array().cloned().unwrap_or_default();
    Ok(PlatformValue::Files(
        items
            .into_iter()
            .filter_map(|item| {
                Some(PickedFile {
                    path: PathBuf::from(item.get("path")?.as_str()?),
                    name: item.get("name").and_then(Value::as_str).unwrap_or("file").to_owned(),
                    mime_type: item.get("mimeType").and_then(Value::as_str).map(str::to_owned),
                    size: item.get("size").and_then(Value::as_u64).unwrap_or(0),
                    temporary: true,
                })
            })
            .collect(),
    ))
}

/// ACTION_OPEN_DOCUMENT; selections are copied into a private cache directory.
pub(crate) fn pick_files(_shared: &Arc<Shared>, options: PickFilesOptions, done: Completion) {
    call_async("pickFiles", json!({"multiple": options.multiple, "imagesOnly": options.images_only}), done, files);
}

pub(crate) fn pick_directory(_shared: &Arc<Shared>, _title: Option<String>, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients browse folders on the selected host."));
}

pub(crate) fn save_location(_shared: &Arc<Shared>, _suggested_name: String, done: Completion) {
    done.fail(PlatformError::unsupported("Mobile clients save through the share sheet."));
}

pub(crate) fn share_file(_shared: &Arc<Shared>, path: PathBuf, mime_type: String, name: String, done: Completion) {
    call_async("share", json!({"path": path, "mime": mime_type, "name": name}), done, none);
}

pub(crate) fn copy_text(shared: &Arc<Shared>, text: String, done: Completion) {
    shared.complete_blocking(done, move || call("copyText", json!({"text": text})).and_then(none));
}

pub(crate) fn read_clipboard(shared: &Arc<Shared>, images_only: bool, done: Completion) {
    shared.complete_blocking(done, move || {
        let directory = crate::paths::transfer_staging_file("clipboard")?;
        let reply = object(call("readClipboard", json!({"imagesOnly": images_only, "target": directory}))?);
        if let Some(path) = reply.get("imagePath").and_then(Value::as_str) {
            let bytes = std::fs::read(path)?;
            let _ = crate::paths::remove_transfer_staging(std::path::Path::new(path));
            let mime = crate::image::sniff_image_mime(&bytes)
                .map(str::to_owned)
                .ok_or_else(|| PlatformError::invalid("The clipboard image is not PNG, JPEG, GIF or WebP."))?;
            return Ok(PlatformValue::Clipboard(ClipboardContent {
                images: vec![ClipboardImage { bytes: Arc::new(bytes), mime_type: mime }],
                text: None,
            }));
        }
        let _ = crate::paths::remove_transfer_staging(&directory);
        if images_only {
            return Err(PlatformError::new("no_image", "No image in the current clipboard."));
        }
        Ok(PlatformValue::Clipboard(ClipboardContent {
            images: Vec::new(),
            text: reply.get("text").and_then(Value::as_str).map(str::to_owned),
        }))
    });
}

pub(crate) fn copy_image(shared: &Arc<Shared>, bytes: Vec<u8>, mime_type: String, done: Completion) {
    shared.complete_blocking(done, move || {
        let extension = match crate::image::sniff_image_mime(&bytes) {
            Some("image/png") => "png",
            Some("image/jpeg") => "jpg",
            Some("image/gif") => "gif",
            Some("image/webp") => "webp",
            _ => return Err(PlatformError::invalid(format!("Unsupported clipboard image type {mime_type}."))),
        };
        let file = crate::paths::transfer_staging_file(&format!("clipboard.{extension}"))?;
        std::fs::write(&file, &bytes)?;
        call("copyImage", json!({"path": file, "mime": mime_type})).and_then(none)
    });
}

/// Locally presented notification on the "default" channel.
pub(crate) fn notify(shared: &Arc<Shared>, request: NotificationRequest, done: Completion) {
    let payload = json!({"id": request.event_key.clone().unwrap_or_else(|| request.id.to_string()), "data": request.data});
    shared.complete_blocking(done, move || {
        call(
            "notify",
            json!({
                "id": request.id,
                "title": request.title,
                "body": request.body,
                "payload": payload.to_string(),
                "sound": request.sound.unwrap_or(true),
            }),
        )
        .and_then(none)
    });
}

fn permission(value: Value) -> Result<PlatformValue, PlatformError> {
    Ok(PlatformValue::Permission(parse_permission(&value)))
}

fn parse_permission(value: &Value) -> PermissionState {
    match value.as_str() {
        Some("granted") => PermissionState::Granted,
        Some("denied") => PermissionState::Denied,
        _ => PermissionState::NotDetermined,
    }
}

pub(crate) fn request_notification_permission(_shared: &Arc<Shared>, done: Completion) {
    call_async("permission", json!({"permission": "notifications", "request": true}), done, permission);
}

pub(crate) fn camera_permission(_shared: &Arc<Shared>, request: bool, done: Completion) {
    call_async("permission", json!({"permission": "camera", "request": request}), done, permission);
}

/// Launcher badges (the vendor broadcast app_badge_plus used; launchers
/// without numeric badges ignore it).
pub(crate) fn set_badge_count(shared: &Arc<Shared>, count: u32, done: Completion) {
    shared.complete_blocking(done, move || call("setBadge", json!({"count": count})).and_then(none));
}

pub(crate) fn idle_milliseconds(_shared: &Arc<Shared>, done: Completion) {
    done.fail(PlatformError::new("idle_unsupported", "Mobile clients report app activity only."));
}

pub(crate) fn system_prefers_dark() -> bool {
    call("appearance", json!({})).ok().and_then(|v| v.get("dark").and_then(Value::as_bool)).unwrap_or(false)
}

pub(crate) fn set_system_bars(dark: bool, background: u32) {
    if let Err(error) = call("setSystemBars", json!({"dark": dark, "background": background})) {
        post_event(PlatformEvent::HostError { message: error.message });
    }
}

pub(crate) fn system_language() -> String {
    call("appearance", json!({}))
        .ok()
        .and_then(|v| v.get("language").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| "en".into())
}

pub(crate) fn insets_from_json(value: &Value) -> SafeInsets {
    let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0);
    SafeInsets {
        top: number("top"),
        bottom: number("bottom"),
        left: number("left"),
        right: number("right"),
        keyboard: number("keyboard"),
        keyboard_visible: value.get("keyboardVisible").and_then(Value::as_bool).unwrap_or(false),
    }
}

// ----- push -----

/// Firebase is optional: builds without `google-services.json` cannot issue tokens.
pub(crate) async fn push_provisioned() -> bool {
    matches!(call_await("pushProvisioned", json!({})).await, Ok(Value::Bool(true)))
}

pub(crate) async fn notification_permission(request: bool) -> Result<PermissionState, PlatformError> {
    let value = call_await("permission", json!({"permission": "notifications", "request": request})).await?;
    Ok(parse_permission(&value))
}

/// The FCM registration token (Firebase Messaging 24 `getToken`).
pub(crate) async fn device_push_token() -> Result<String, PlatformError> {
    Ok(call_await("pushToken", json!({})).await?.as_str().unwrap_or("").to_owned())
}

/// Android FCM tokens are always production.
pub(crate) fn push_environment() -> Result<String, PlatformError> {
    Ok("production".into())
}

/// Errors recorded by the background messaging service while no UI ran.
pub(crate) fn take_background_push_error() -> Option<String> {
    call("takeBackgroundError", json!({})).ok().and_then(|v| v.as_str().map(str::to_owned))
}
