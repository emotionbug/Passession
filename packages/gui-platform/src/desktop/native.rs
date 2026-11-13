//! `crate::native` for Linux, macOS and Windows.

use super::os;
use crate::{
    action::*,
    image::{decode_png_rgba, encode_rgba_png, sniff_image_mime},
    platform::{Capabilities, Completion, PickFilesOptions, Shared},
    NotificationRequest,
};
use makepad_widgets::Cx;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub(crate) fn prepare_process(_cx: &mut Cx) -> anyhow::Result<()> {
    os::prepare_process()
}

pub(crate) fn capabilities() -> Capabilities {
    Capabilities {
        desktop_host: true,
        tray: true,
        editors: true,
        updater: true,
        directory_picker: true,
        save_dialog: true,
        push: false,
        qr_scan: false,
        share: true,
        idle: os::idle_supported(),
    }
}

pub(crate) fn report_error(shared: &Arc<Shared>, message: &str) {
    shared.desktop.report(message);
}


pub(crate) fn system_prefers_dark() -> bool {
    os::system_prefers_dark()
}

pub(crate) fn system_language() -> String {
    os::locale()
}
/// Desktop windows have no back stack; Back never reaches here.
pub(crate) fn background_app(_cx: &mut Cx) {}

/// Desktop audio needs no session/focus negotiation.
pub(crate) fn acquire_audio_session() -> anyhow::Result<()> {
    Ok(())
}
pub(crate) fn release_audio_session() {}

pub(crate) fn open_external(shared: &Arc<Shared>, url: String, done: Completion) {
    shared.complete_blocking(done, move || os::open_uri(&url).map(|()| PlatformValue::None));
}

pub(crate) fn open_path(shared: &Arc<Shared>, path: PathBuf, done: Completion) {
    shared.complete_blocking(done, move || {
        if !path.exists() {
            return Err(PlatformError::new("io", format!("Path does not exist: {}", path.display())));
        }
        os::open_path(&path).map(|()| PlatformValue::None)
    });
}

pub(crate) fn reveal_path(shared: &Arc<Shared>, path: PathBuf, done: Completion) {
    shared.complete_blocking(done, move || {
        let (workspace, file) = if path.is_dir() {
            (path.clone(), None)
        } else {
            (path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone()), Some(path.clone()))
        };
        reveal_blocking(&workspace, file.as_deref()).map(|()| PlatformValue::None)
    });
}

/// Reveal in Finder/Explorer/FileManager1, else open the folder.
pub(crate) fn reveal_blocking(workspace: &Path, file: Option<&Path>) -> Result<(), PlatformError> {
    for path in std::iter::once(workspace).chain(file) {
        if !path.is_absolute() || !path.exists() {
            return Err(PlatformError::invalid(format!("Not an existing absolute local path: {}", path.display())));
        }
    }
    os::reveal(workspace, file)
}

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

fn picked(path: PathBuf) -> PickedFile {
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mime_type = crate::paths::mime_for_name(&name);
    PickedFile { path, name, mime_type, size, temporary: false }
}

pub(crate) fn pick_files(shared: &Arc<Shared>, options: PickFilesOptions, done: Completion) {
    shared.spawn(async move {
        let mut dialog = rfd::AsyncFileDialog::new();
        if let Some(title) = &options.title {
            dialog = dialog.set_title(title);
        }
        if options.images_only {
            dialog = dialog.add_filter("Images", IMAGE_EXTENSIONS);
        }
        let files: Vec<PathBuf> = if options.multiple {
            dialog.pick_files().await.unwrap_or_default().into_iter().map(|f| f.path().to_path_buf()).collect()
        } else {
            dialog.pick_file().await.into_iter().map(|f| f.path().to_path_buf()).collect()
        };
        done.ok(PlatformValue::Files(files.into_iter().map(picked).collect()));
    });
}

pub(crate) fn pick_directory(shared: &Arc<Shared>, title: Option<String>, done: Completion) {
    shared.spawn(async move {
        let mut dialog = rfd::AsyncFileDialog::new();
        if let Some(title) = &title {
            dialog = dialog.set_title(title);
        }
        let folder = dialog.pick_folder().await.map(|f| f.path().to_path_buf());
        done.ok(PlatformValue::Directory(folder));
    });
}

pub(crate) fn save_location(shared: &Arc<Shared>, suggested_name: String, done: Completion) {
    shared.spawn(async move {
        let file = rfd::AsyncFileDialog::new().set_file_name(&suggested_name).save_file().await;
        done.ok(PlatformValue::SaveLocation(file.map(|f| f.path().to_path_buf())));
    });
}

pub(crate) fn share_file(shared: &Arc<Shared>, path: PathBuf, mime_type: String, name: String, done: Completion) {
    os::share_file(shared, path, mime_type, name, done);
}

// ----- clipboard -----

/// arboard keeps X11 selection ownership alive only while its instance lives,
/// so one instance is kept for the process.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn with_clipboard<R>(f: impl FnOnce(&mut arboard::Clipboard) -> Result<R, arboard::Error>) -> Result<R, PlatformError> {
    let mut guard = CLIPBOARD.lock().unwrap();
    if guard.is_none() {
        *guard = Some(arboard::Clipboard::new().map_err(|e| PlatformError::native(format!("Clipboard unavailable: {e}")))?);
    }
    f(guard.as_mut().unwrap()).map_err(|e| PlatformError::native(e.to_string()))
}

pub(crate) fn copy_text(shared: &Arc<Shared>, text: String, done: Completion) {
    shared.complete_blocking(done, move || with_clipboard(|c| c.set_text(text)).map(|()| PlatformValue::None));
}

pub(crate) fn read_clipboard(shared: &Arc<Shared>, images_only: bool, done: Completion) {
    shared.complete_blocking(done, move || {
        let image = with_clipboard(|c| match c.get_image() {
            Ok(image) => Ok(Some(image)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error),
        })?;
        if let Some(image) = image {
            // Platform clipboards expose decoded pixels: encode real PNG bytes
            // instead of mislabeling the data.
            let bytes = encode_rgba_png(image.width as u32, image.height as u32, &image.bytes)?;
            let mime = sniff_image_mime(&bytes).unwrap_or("image/png").to_owned();
            return Ok(PlatformValue::Clipboard(ClipboardContent {
                images: vec![ClipboardImage { bytes: Arc::new(bytes), mime_type: mime }],
                text: None,
            }));
        }
        if images_only {
            return Err(PlatformError::new("no_image", "No image in the current clipboard."));
        }
        let text = with_clipboard(|c| match c.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error),
        })?;
        Ok(PlatformValue::Clipboard(ClipboardContent { images: Vec::new(), text }))
    });
}

pub(crate) fn copy_image(shared: &Arc<Shared>, bytes: Vec<u8>, mime_type: String, done: Completion) {
    shared.complete_blocking(done, move || {
        if sniff_image_mime(&bytes) != Some("image/png") {
            return Err(PlatformError::invalid(format!("Only PNG images can be copied (got {mime_type}).")));
        }
        let (width, height, rgba) = decode_png_rgba(&bytes)?;
        with_clipboard(|c| {
            c.set_image(arboard::ImageData { width: width as usize, height: height as usize, bytes: rgba.into() })
        })
        .map(|()| PlatformValue::None)
    });
}

// ----- notifications, badge, idle -----

pub(crate) fn notify(shared: &Arc<Shared>, request: NotificationRequest, done: Completion) {
    super::notify(shared, request, done);
}

pub(crate) fn request_notification_permission(shared: &Arc<Shared>, done: Completion) {
    os::request_notification_permission(shared, done);
}

pub(crate) fn notification_permission_status(shared: &Arc<Shared>, done: Completion) {
    os::notification_permission_status(shared, done);
}

pub(crate) fn set_badge_count(shared: &Arc<Shared>, count: u32, done: Completion) {
    os::set_badge_count(shared, count, done);
}

pub(crate) fn idle_milliseconds(shared: &Arc<Shared>, done: Completion) {
    shared.complete_blocking(done, || os::idle_milliseconds().map(PlatformValue::IdleMilliseconds));
}

pub(crate) fn camera_permission(_shared: &Arc<Shared>, _request: bool, done: Completion) {
    done.fail(PlatformError::unsupported("Pairing QR scanning is a mobile feature; desktop pairs by link."));
}

