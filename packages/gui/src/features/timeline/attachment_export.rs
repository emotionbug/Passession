//! Exports take original bytes, never a rendered or thumbnail texture.
//! The browser downloads those bytes directly. On mobile, the native request
//! owns a staged path until the platform acknowledges the handoff.

use super::composer_bridge;
use super::i18n::{t, ta};
use super::images::{ImageSource, resolve_image_source};
use crate::common::UiContext;
use crate::common::host_http::IMAGE_MAX_BYTES;
use crate::features::composer::attachments::ImageBytes;
use crate::features::files::path::safe_local_name;
#[cfg(not(target_arch = "wasm32"))]
use crate::features::files::transfer::remove_staging_later;
use base64::Engine;
use makepad_widgets::{Event, SignalToUI};
use paseo_gui_core::{CoreHandle, RequestId};
use paseo_gui_platform::{PlatformAction, PlatformError, PlatformRequestId, PlatformValue};
use serde_json::Value;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{LazyLock, Mutex, mpsc::SyncSender};
use std::sync::mpsc::{self, Receiver, TryRecvError};
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, SystemTime};

#[cfg(not(target_arch = "wasm32"))]
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
#[cfg(not(target_arch = "wasm32"))]
const MAX_STAGED_FILES: usize = 64;
#[cfg(not(target_arch = "wasm32"))]
const MAX_STAGED_BYTES: u64 = 256 << 20;
#[cfg(not(target_arch = "wasm32"))]
static ACTIVE_FILES: LazyLock<Mutex<HashSet<uuid::Uuid>>> = LazyLock::new(Mutex::default);

pub(super) enum Original<'a> {
    Blob {
        id: &'a str,
        name: Option<&'a str>,
        mime: Option<&'a str>,
    },
    Inline {
        data: &'a str,
        name: Option<&'a str>,
        mime: Option<&'a str>,
    },
    File {
        cwd: String,
        path: String,
    },
}

impl<'a> Original<'a> {
    pub(super) fn user(image: &'a Value) -> Option<Self> {
        let name = image
            .get("fileName")
            .and_then(Value::as_str)
            .or_else(|| image.get("name").and_then(Value::as_str));
        let mime = image.get("mimeType").and_then(Value::as_str);
        match composer_bridge::OriginalContent::from_value(image)? {
            composer_bridge::OriginalContent::Blob(id) => Some(Self::Blob { id, name, mime }),
            composer_bridge::OriginalContent::Inline(data) => Some(Self::Inline { data, name, mime }),
        }
    }

    pub(super) fn assistant(uri: &'a str, cwd: Option<&str>) -> Option<Self> {
        let uri = uri.trim();
        if is_data_uri(uri) {
            return Some(Self::Inline {
                data: uri,
                name: None,
                mime: None,
            });
        }
        if let Some(path) = uri.strip_prefix("file://") {
            // The display resolver also accepts file URIs with an authority.
            // Export only the session host's files, not an ambiguous host alias.
            let authority = path.split('/').next().unwrap_or("");
            if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
                return None;
            }
        }
        // An arbitrary network image may itself be a thumbnail. There is no
        // original identity to export for an HTTP or browser-local preview.
        match resolve_image_source(uri, cwd)? {
            ImageSource::File { cwd, path } => Some(Self::File { cwd, path }),
            ImageSource::Direct(_) => None,
        }
    }
}

fn is_data_uri(data: &str) -> bool {
    data.get(..5)
        .is_some_and(|head| head.eq_ignore_ascii_case("data:"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct HostRoute {
    server: String,
    generation: u64,
}

impl HostRoute {
    pub(super) fn capture(ui: &UiContext, host: &str) -> Option<Self> {
        let record = ui.host(host)?;
        Self::connected(record)
    }

    fn connected(record: &Value) -> Option<Self> {
        if record["status"] != "connected" {
            return None;
        }
        Some(Self {
            server: record["serverId"]
                .as_str()
                .filter(|id| !id.is_empty())?
                .to_owned(),
            generation: record["connectionGeneration"].as_u64()?,
        })
    }

    pub(super) fn still_current(&self, ui: &UiContext, host: &str) -> bool {
        ui.host(host).is_some_and(|record| self.matches(record))
    }

    fn matches(&self, record: &Value) -> bool {
        record["serverId"].as_str() == Some(self.server.as_str())
            && record["connectionGeneration"].as_u64() == Some(self.generation)
            && record["status"] == "connected"
    }

    pub(super) fn server(&self) -> &str {
        &self.server
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Save,
    #[cfg(not(target_arch = "wasm32"))]
    Share,
}

#[derive(Debug, PartialEq, Eq)]
enum Notice {
    ReadFailed,
    StageFailed,
    #[cfg(not(target_arch = "wasm32"))]
    SavedToPhotos,
    #[cfg(not(target_arch = "wasm32"))]
    SavedFile,
    NativeFailed(Mode, String),
}

impl Notice {
    fn text(&self) -> String {
        match self {
            Self::ReadFailed => t("message.attachments.lightbox.readFailed"),
            Self::StageFailed => t("message.attachments.lightbox.stageFailed"),
            #[cfg(not(target_arch = "wasm32"))]
            Self::SavedToPhotos => t("message.attachments.lightbox.savedToPhotos"),
            #[cfg(not(target_arch = "wasm32"))]
            Self::SavedFile => t("message.attachments.lightbox.savedFile"),
            Self::NativeFailed(mode, error) => ta(
                match mode {
                    Mode::Save => "message.attachments.lightbox.saveFailed",
                    #[cfg(not(target_arch = "wasm32"))]
                    Mode::Share => "message.attachments.lightbox.shareFailed",
                },
                &[("error", error)],
            ),
        }
    }

    fn is_error(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        return !matches!(self, Self::SavedToPhotos | Self::SavedFile);
        #[cfg(target_arch = "wasm32")]
        true
    }
}

struct HostRead {
    core: CoreHandle,
    id: RequestId,
}

impl Drop for HostRead {
    fn drop(&mut self) {
        self.core.cancel_request(&self.id);
    }
}

type Prepared = Result<Staged, Notice>;
type NativeResult = Cell<Option<Option<Notice>>>;

enum Pending {
    Preparing {
        mode: Mode,
        reply: Receiver<Prepared>,
        read: Option<HostRead>,
    },
    Presenting(Mode, Rc<NativeResult>),
}

#[derive(Default)]
pub(super) struct Export {
    pending: Option<Pending>,
    notice: Option<Notice>,
}

impl Export {
    pub(super) fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn status(&self) -> Option<(String, bool)> {
        if let Some(pending) = &self.pending {
            let mode = match pending {
                Pending::Preparing { mode, .. } | Pending::Presenting(mode, _) => mode,
            };
            return Some((
                t(match mode {
                    Mode::Save => "message.attachments.lightbox.saving",
                    #[cfg(not(target_arch = "wasm32"))]
                    Mode::Share => "message.attachments.lightbox.preparingShare",
                }),
                false,
            ));
        }
        self.notice
            .as_ref()
            .map(|notice| (notice.text(), notice.is_error()))
    }

    pub(super) fn start(
        &mut self,
        ui: &UiContext,
        route: Option<&HostRoute>,
        mode: Mode,
        original: Original<'_>,
    ) {
        if self.busy() {
            return;
        }
        self.notice = None;
        let (sender, reply) = mpsc::channel();
        let read = match original {
            Original::Blob { id, name, mime } => {
                let id = id.to_owned();
                let name = name.map(str::to_owned);
                let mime = mime.map(str::to_owned);
                if !queue(move || {
                    let prepared = composer_bridge::read_original_blob(&id)
                        .map_err(|_| Notice::ReadFailed)
                        .and_then(|bytes| prepare(bytes, name.as_deref(), mime.as_deref()));
                    publish(sender, prepared);
                }) {
                    self.notice = Some(Notice::StageFailed);
                    return;
                }
                None
            }
            Original::Inline { data, name, mime } => {
                if data.len() > IMAGE_MAX_BYTES.div_ceil(3) * 4 {
                    self.notice = Some(Notice::ReadFailed);
                    return;
                }
                let data = data.to_owned();
                let name = name.map(str::to_owned);
                let mime = mime.map(str::to_owned);
                if !queue(move || {
                    let prepared = decode_inline(&data).and_then(|(bytes, hint)| {
                        prepare(bytes, name.as_deref(), mime.as_deref().or(hint))
                    });
                    publish(sender, prepared);
                }) {
                    self.notice = Some(Notice::StageFailed);
                    return;
                }
                None
            }
            Original::File { cwd, path } => {
                let Some(route) = route else {
                    self.notice = Some(Notice::ReadFailed);
                    return;
                };
                let id = ui.core.operation(route.server(), move |client| async move {
                    let file = client
                        .read_file(
                            &cwd,
                            &path,
                            IMAGE_MAX_BYTES,
                            paseo_client::RequestOptions::default(),
                        )
                        .await?;
                    if !queue(move || {
                        let name = file.metadata.file_name.as_deref().unwrap_or(&path);
                        let prepared =
                            prepare(Vec::from(file.bytes), Some(name), Some(&file.metadata.mime));
                        publish(sender, prepared);
                    }) {
                        return Ok(serde_json::json!(false));
                    }
                    Ok(Value::Null)
                });
                Some(HostRead {
                    core: ui.core.clone(),
                    id,
                })
            }
        };
        self.pending = Some(Pending::Preparing { mode, reply, read });
    }

    /// Only the still-open lightbox may hand prepared bytes to an OS presenter.
    pub(super) fn poll(&mut self, ui: &UiContext, may_present: bool) -> bool {
        match &mut self.pending {
            Some(Pending::Preparing { mode, reply, read }) => {
                if let Some(response) = read.as_ref().and_then(|read| ui.response(&read.id)) {
                    read.take();
                    if !matches!(response, Ok(Value::Null)) {
                        self.notice = Some(if response.is_err() {
                            Notice::ReadFailed
                        } else {
                            Notice::StageFailed
                        });
                        self.pending = None;
                        return true;
                    }
                }
                if !may_present {
                    return false;
                }
                let mode = *mode;
                match reply.try_recv() {
                    Ok(Ok(staged)) => self.present(mode, staged),
                    Ok(Err(notice)) => {
                        self.pending = None;
                        self.notice = Some(notice);
                    }
                    Err(TryRecvError::Empty) => return false,
                    // A rejected/disconnected host read gets its own correlated response.
                    Err(TryRecvError::Disconnected) if read.is_some() => return false,
                    Err(TryRecvError::Disconnected) => {
                        self.pending = None;
                        self.notice = Some(Notice::StageFailed);
                    }
                }
                true
            }
            Some(Pending::Presenting(_, outcome)) => {
                let Some(notice) = outcome.take() else {
                    return false;
                };
                self.pending = None;
                self.notice = notice;
                true
            }
            None => false,
        }
    }

    fn present(&mut self, mode: Mode, staged: Staged) {
        let Some(platform) = paseo_gui_platform::platform() else {
            self.pending = None;
            self.notice = Some(Notice::StageFailed);
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        if !platform.capabilities().photo_library {
            self.pending = None;
            self.notice = Some(Notice::StageFailed);
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let request = match mode {
            Mode::Save => platform.save_image(&staged.path, staged.mime, &staged.name),
            Mode::Share => platform.share_file(&staged.path, staged.mime, &staged.name),
        };
        #[cfg(target_arch = "wasm32")]
        let request = platform.download_file(&staged.name, staged.mime, &staged.bytes);
        let outcome = Rc::new(Cell::new(None));
        PRESENTED.with_borrow_mut(|requests| {
            requests.insert(
                request,
                Presented {
                    mode,
                    #[cfg(not(target_arch = "wasm32"))]
                    staged,
                    outcome: Rc::downgrade(&outcome),
                },
            );
        });
        self.pending = Some(Pending::Presenting(mode, outcome));
    }
}

struct Presented {
    mode: Mode,
    #[cfg(not(target_arch = "wasm32"))]
    staged: Staged,
    outcome: Weak<NativeResult>,
}

thread_local! {
    static PRESENTED: RefCell<HashMap<PlatformRequestId, Presented>> = RefCell::default();
}

pub(super) fn handle_event(event: &Event) {
    let Event::Actions(actions) = event else {
        return;
    };
    for action in actions {
        let Some(PlatformAction::Completed { request, result }) = action.downcast_ref() else {
            continue;
        };
        let Some(pending) = PRESENTED.with_borrow_mut(|requests| requests.remove(request))
        else {
            continue;
        };
        #[cfg(not(target_arch = "wasm32"))]
        let mut pending = pending;
        #[cfg(not(target_arch = "wasm32"))]
        if matches!((pending.mode, result), (Mode::Share, Ok(PlatformValue::None))) {
            // Both mobile bridges acknowledge the launch only: the receiver
            // may open the URL/URI later. The bounded transfer cache expires it.
            pending.staged.retain = true;
        }
        if let Some(outcome) = pending.outcome.upgrade() {
            outcome.set(Some(native_notice(pending.mode, result)));
        }
    }
}

fn native_notice(mode: Mode, result: &Result<PlatformValue, PlatformError>) -> Option<Notice> {
    match (mode, result) {
        #[cfg(target_arch = "wasm32")]
        (Mode::Save, Ok(PlatformValue::Submitted)) => None,
        #[cfg(not(target_arch = "wasm32"))]
        (Mode::Save, Ok(PlatformValue::PhotoLibrarySaved)) => Some(Notice::SavedToPhotos),
        #[cfg(not(target_arch = "wasm32"))]
        (Mode::Save, Ok(PlatformValue::Bool(true))) => Some(Notice::SavedFile),
        #[cfg(not(target_arch = "wasm32"))]
        (Mode::Save, Ok(PlatformValue::Bool(false))) | (Mode::Share, Ok(PlatformValue::None)) => None,
        (_, Err(error)) if error.code == "cancelled" => None,
        (_, Err(error)) => Some(Notice::NativeFailed(mode, error.message.clone())),
        _ => Some(Notice::NativeFailed(
            mode,
            t("message.attachments.lightbox.unexpectedResult"),
        )),
    }
}

#[cfg(not(target_arch = "wasm32"))]
type Job = Box<dyn FnOnce() + Send>;
#[cfg(not(target_arch = "wasm32"))]
static WORKER: LazyLock<Option<SyncSender<Job>>> = LazyLock::new(|| {
    let (sender, receiver) = mpsc::sync_channel::<Job>(4);
    std::thread::Builder::new()
        .name("paseo-lightbox-transfer".into())
        .spawn(move || {
            while let Ok(job) = receiver.recv() {
                job();
            }
        })
        .ok()?;
    Some(sender)
});

#[cfg(not(target_arch = "wasm32"))]
fn queue(job: impl FnOnce() + Send + 'static) -> bool {
    WORKER
        .as_ref()
        .is_some_and(|sender| sender.try_send(Box::new(job)).is_ok())
}

#[cfg(target_arch = "wasm32")]
fn queue(job: impl FnOnce() + Send + 'static) -> bool {
    crate::common::background::spawn("paseo-lightbox-transfer", job).is_ok()
}

fn publish(sender: mpsc::Sender<Prepared>, prepared: Prepared) {
    if sender.send(prepared).is_ok() {
        SignalToUI::set_ui_signal();
    }
}

struct Staged {
    #[cfg(not(target_arch = "wasm32"))]
    id: uuid::Uuid,
    #[cfg(not(target_arch = "wasm32"))]
    path: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    retain: bool,
    #[cfg(target_arch = "wasm32")]
    bytes: ImageBytes,
    mime: &'static str,
    name: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Staged {
    fn drop(&mut self) {
        ACTIVE_FILES.lock().unwrap().remove(&self.id);
        if !self.retain {
            remove_staging_later(std::mem::take(&mut self.path));
        }
    }
}

fn decode_inline(value: &str) -> Result<(Vec<u8>, Option<&str>), Notice> {
    let mut mime = None;
    let mut encoded = value;
    let mut base64 = true;
    if is_data_uri(value) {
        let (head, data) = value[5..].split_once(',').ok_or(Notice::ReadFailed)?;
        mime = head.split(';').next().filter(|mime| !mime.is_empty());
        base64 = head
            .split(';')
            .skip(1)
            .any(|part| part.eq_ignore_ascii_case("base64"));
        encoded = data;
    }
    if base64 {
        let cleaned = if encoded.bytes().any(|b| b.is_ascii_whitespace()) {
            Cow::Owned(
                encoded.chars().filter(|c| !c.is_ascii_whitespace()).collect::<String>(),
            )
        } else {
            Cow::Borrowed(encoded)
        };
        let engine = if cleaned.ends_with('=') {
            &base64::engine::general_purpose::STANDARD
        } else {
            &base64::engine::general_purpose::STANDARD_NO_PAD
        };
        let bytes = engine
            .decode(cleaned.as_bytes())
            .map_err(|_| Notice::ReadFailed)?;
        Ok((bytes, mime))
    } else {
        if encoded.split('%').skip(1).any(|fragment| {
            fragment.as_bytes().get(..2).is_none_or(|hex| !hex.iter().all(u8::is_ascii_hexdigit))
        }) {
            return Err(Notice::ReadFailed);
        }
        let bytes = percent_encoding::percent_decode_str(encoded).collect();
        Ok((bytes, mime))
    }
}

fn export_name(name: Option<&str>, mime: &str) -> Option<String> {
    let extensions: &[&str] = match mime {
        "image/png" => &["png"],
        "image/jpeg" => &["jpg", "jpeg", "jpe"],
        "image/gif" => &["gif"],
        "image/webp" => &["webp"],
        "image/bmp" => &["bmp"],
        "image/tiff" => &["tiff", "tif"],
        "image/avif" => &["avif"],
        "image/heic" => &["heic"],
        "image/heif" => &["heif", "hif"],
        "image/heic-sequence" => &["heics"],
        "image/heif-sequence" => &["heifs"],
        _ => return None,
    };
    let name = name.map(safe_local_name);
    let name = name.as_deref().unwrap_or("image").trim_matches(['.', ' ']);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    if name.len() <= 240 && extensions.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
        return Some(name.to_owned());
    }
    let default_ext = extensions[0];
    let stem = if stem.is_empty() { "image" } else { stem };
    let stem = &stem[..stem.floor_char_boundary(240 - default_ext.len() - 1)];
    Some(format!("{stem}.{default_ext}"))
}

fn prepare(bytes: impl Into<ImageBytes>, name: Option<&str>, hint: Option<&str>) -> Prepared {
    let bytes = bytes.into();
    if bytes.len() > IMAGE_MAX_BYTES {
        return Err(Notice::ReadFailed);
    }
    let mime = paseo_gui_platform::image::sniff_image_mime_with_hint(&bytes, hint)
        .ok_or(Notice::ReadFailed)?;
    let name = export_name(name, mime).ok_or(Notice::ReadFailed)?;
    #[cfg(target_arch = "wasm32")]
    return Ok(Staged { bytes, name, mime });
    #[cfg(not(target_arch = "wasm32"))]
    {
        let root = paseo_gui_platform::paths::app_paths().temp_dir.join("paseo-files/lightbox");
        stage_original(&root, &bytes, name, mime).map_err(|_| Notice::StageFailed)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn stage_original(
    root: &Path,
    bytes: &[u8],
    name: String,
    mime: &'static str,
) -> std::io::Result<Staged> {
    use std::fs;
    fs::create_dir_all(root)?;
    let mut count = 0;
    let mut size = 0u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("transfer-"))
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
        else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_dir() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let directory = entry.path();
        // A live permission prompt, document picker or not-yet-dispatched
        // export may still own the path. Only unused cache entries expire.
        if metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > RETENTION)
            && !ACTIVE_FILES.lock().unwrap().contains(&id)
            && fs::remove_dir_all(&directory).is_ok()
        {
            continue;
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        count += 1;
        for file in entries.flatten() {
            size = size.saturating_add(file.metadata().map_or(0, |md| md.len()));
        }
    }
    if count >= MAX_STAGED_FILES || size.saturating_add(bytes.len() as u64) > MAX_STAGED_BYTES {
        return Err(std::io::Error::other("Temporary image export limit reached"));
    }
    let id = uuid::Uuid::new_v4();
    let directory = root.join(format!("transfer-{}", id.simple()));
    fs::create_dir(&directory)?;
    let path = directory.join(&name);
    if let Err(error) = fs::write(&path, bytes) {
        let _ = fs::remove_dir_all(directory);
        return Err(error);
    }
    ACTIVE_FILES.lock().unwrap().insert(id);
    Ok(Staged {
        id,
        path,
        retain: false,
        mime,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn original_sources_exclude_network_and_thumbnail_only_images() {
        assert!(Original::user(&json!({"thumbnail": "iVBORw0KGgo="})).is_none());
        assert!(Original::assistant("https://host/preview.png", Some("/repo")).is_none());
        assert!(Original::assistant("blob:local-preview", Some("/repo")).is_none());
        assert!(Original::assistant("file://other/asset.png", Some("/repo")).is_none());
        assert!(matches!(
            Original::user(&json!({"blobId": "local", "data": "preview"})),
            Some(Original::Blob { id: "local", .. })
        ));
        assert!(matches!(
            Original::assistant("assets/my%20image.png", Some("/repo")),
            Some(Original::File { cwd, path }) if cwd == "/repo" && path == "assets/my image.png"
        ));
        assert!(matches!(
            Original::assistant("data:image/png;base64,aW1hZ2U=", None),
            Some(Original::Inline { .. })
        ));
    }

    #[test]
    fn original_file_route_rejects_remapped_hosts_and_late_connection_generations() {
        let host = |id, generation, status| {
            json!({"serverId": id, "connectionGeneration": generation, "status": status})
        };
        let route = HostRoute::connected(&host("host-a", 1, "connected")).unwrap();
        assert!(route.matches(&host("host-a", 1, "connected")));
        assert!(!route.matches(&host("host-a", 2, "connected")));
        assert!(!route.matches(&host("host-b", 1, "connected")));
        assert!(!route.matches(&host("host-a", 1, "disconnected")));
    }

    #[test]
    fn staging_preserves_encoded_bytes_and_normalizes_the_name() {
        use paseo_gui_platform::image::{encode_rgba_png, sniff_image_mime_with_hint};
        let original = encode_rgba_png(1, 1, &[0, 0, 0, 255]).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&original);
        let uri = format!("data:image/png;base64,{encoded}");
        let (png, mime) = decode_inline(&uri).unwrap();
        assert_eq!(png, original);
        let detected = sniff_image_mime_with_hint(&png, Some("image/jpeg")).unwrap();
        assert_eq!(mime, Some("image/png"));
        assert_eq!(detected, "image/png");
        let name = export_name(Some("C:\\images\\민들레.JPG"), detected).unwrap();
        assert_eq!(name, "민들레.png");
        assert_eq!(
            export_name(Some("actual.JPEG"), "image/jpeg").as_deref(),
            Some("actual.JPEG")
        );
        assert_eq!(decode_inline("data:image/png,%89PNG%00").unwrap().0, b"\x89PNG\0");
        assert!(decode_inline("data:image/png,%X").is_err());
        let root = std::env::temp_dir().join(format!("paseo-lightbox-test-{}", uuid::Uuid::new_v4()));
        let mut staged = stage_original(&root, &png, name, detected).unwrap();
        staged.retain = true;
        assert_eq!(std::fs::read(&staged.path).unwrap(), png);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_result_distinguishes_save_cancel_and_share_presentation() {
        assert_eq!(
            native_notice(Mode::Save, &Ok(PlatformValue::PhotoLibrarySaved)),
            Some(Notice::SavedToPhotos)
        );
        assert_eq!(
            native_notice(Mode::Save, &Ok(PlatformValue::Bool(true))),
            Some(Notice::SavedFile)
        );
        assert_eq!(native_notice(Mode::Save, &Ok(PlatformValue::Bool(false))), None);
        assert_eq!(native_notice(Mode::Share, &Ok(PlatformValue::None)), None);
        assert_eq!(
            native_notice(Mode::Share, &Err(PlatformError::new("cancelled", ""))),
            None
        );
        assert!(matches!(
            native_notice(Mode::Save, &Err(PlatformError::native("denied"))),
            Some(Notice::NativeFailed(Mode::Save, detail)) if detail == "denied"
        ));
    }
}
