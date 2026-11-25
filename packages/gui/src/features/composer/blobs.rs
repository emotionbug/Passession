//! Retained composer images are immutable and content-addressed. Drafts retain
//! only `{blobId, mimeType, byteSize}` and an optional original filename. The
//! desktop keeps blobs in files; the browser retains them for the page session.
//! Decoded pixels arrive through a `ToUIReceiver` and become cached textures
//! on the GUI thread.

#[cfg(not(target_arch = "wasm32"))]
use super::attachments::raster_image_mime_type;
use super::attachments::{
    ImageBytes, ImageImportError, SelectedFile, clipboard_image_mime, read_image_file,
};
use makepad_widgets::image_cache::ImageBuffer;
use makepad_widgets::*;
use paseo_gui_platform::ClipboardImage;
#[cfg(not(target_arch = "wasm32"))]
use paseo_gui_platform::PickedFile;
#[cfg(not(target_arch = "wasm32"))]
use paseo_gui_platform::image::sniff_image_header;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Sender, channel};

/// Thumbnails are decoded at 2x the 48px pill for high-DPI displays.
pub const THUMBNAIL_PIXELS: u32 = 96;
/// Previews are capped to keep a single texture within every GPU's limits.
pub const PREVIEW_PIXELS: u32 = 4096;
const THUMBNAIL_CACHE: usize = 64;
const PREVIEW_CACHE: usize = 4;

pub enum BlobJob {
    /// Store raw image bytes (clipboard).
    StoreBytes {
        token: u64,
        image: ClipboardImage,
    },
    /// Validate and retain images from the platform picker (or desktop drop).
    ImportImages {
        token: u64,
        files: Vec<SelectedFile>,
    },
    /// Import a user-accepted inbound share for an exact draft. The source lease
    /// moves with the job, including if its requester closes before IO finishes.
    #[cfg(not(target_arch = "wasm32"))]
    ImportSharedImages {
        share: Arc<paseo_gui_platform::SharedImages>,
        inline: bool,
        reply: ToUISender<Result<Vec<Value>, String>>,
    },
    /// Inspect dropped paths before routing them to the image tray or file upload.
    #[cfg(not(target_arch = "wasm32"))]
    Stat { token: u64, paths: Vec<PathBuf> },
    Decode {
        key: String,
        source: ImageSource,
        max: u32,
    },
    /// Delete blobs nobody references (startup, after drafts load).
    Collect { keep: HashSet<String> },
}

#[derive(Clone, Debug)]
pub enum ImageSource {
    Blob(String),
    /// Legacy inline base64 (`{data, mimeType}`), with or without a `data:` prefix.
    Inline(String),
}

/// The decode input borrowed from a draft; owning it is only necessary for a queued job.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImageContent<'a> {
    Blob(&'a str),
    Inline(&'a str),
}

impl<'a> ImageContent<'a> {
    pub fn from_value(image: &'a Value) -> Option<Self> {
        if let Some(id) = image.get("blobId").and_then(Value::as_str) {
            return Some(Self::Blob(id));
        }
        image
            .get("data")
            .and_then(Value::as_str)
            .filter(|data| !data.is_empty())
            .map(Self::Inline)
    }

    pub fn identity(self) -> ImageIdentity {
        let base = match self {
            Self::Blob(id) => format!("blob:{id}"),
            Self::Inline(data) => {
                #[cfg(test)]
                INLINE_HASHES.with(|count| count.set(count.get() + 1));
                format!("inline:{:x}", Sha256::digest(data.as_bytes()))
            }
        };
        ImageIdentity {
            thumbnail: format!("{base}:thumb"),
            preview: format!("{base}:full"),
        }
    }

    fn into_source(self) -> ImageSource {
        match self {
            Self::Blob(id) => ImageSource::Blob(id.to_owned()),
            Self::Inline(data) => ImageSource::Inline(data.to_owned()),
        }
    }
}

#[derive(Debug)]
pub struct ImageIdentity {
    thumbnail: String,
    preview: String,
}

impl ImageIdentity {
    fn key(&self, thumbnail: bool) -> &str {
        if thumbnail {
            &self.thumbnail
        } else {
            &self.preview
        }
    }
}

#[cfg(test)]
thread_local! {
    static INLINE_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    /// Packed `a<<24 | r<<16 | g<<8 | b`, Makepad's `VecBGRAu8_32`.
    pub pixels: Vec<u32>,
}

pub enum BlobResult {
    /// Draft entries `{blobId, mimeType, byteSize}` or an import failure.
    Stored {
        token: u64,
        images: Result<Vec<Value>, ImageImportError>,
    },
    #[cfg(not(target_arch = "wasm32"))]
    Stat {
        token: u64,
        files: Result<Vec<StatFile>, String>,
    },
    Decoded {
        key: String,
        result: Result<DecodedImage, String>,
    },
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct StatFile {
    pub file: PickedFile,
    pub is_image: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl StatFile {
    pub(crate) fn inspect(path: PathBuf) -> Result<Self, String> {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let error = |error: std::io::Error| format!("{name}: {error}");
        let metadata = std::fs::metadata(&path).map_err(error)?;
        let size = metadata.len();
        let mut is_image = raster_image_mime_type(&name).is_some();
        if !is_image && metadata.is_file() && let Ok(len) = usize::try_from(size) {
            let mut header = [0; 512];
            let count = header.len().min(len);
            std::fs::File::open(&path)
                .and_then(|mut file| file.read_exact(&mut header[..count]))
                .map_err(error)?;
            is_image = sniff_image_header(&header[..count], len).is_some();
        }
        Ok(Self {
            file: PickedFile {
                path,
                name,
                size,
                mime_type: None,
                temporary: false,
            },
            is_image,
        })
    }
}

fn valid_blob_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static BLOBS: std::cell::RefCell<HashMap<String, ImageBytes>> = Default::default();
}

fn store(
    dir: &Path,
    #[cfg(not(target_arch = "wasm32"))] bytes: &[u8],
    #[cfg(target_arch = "wasm32")] bytes: ImageBytes,
    mime: &str,
) -> Result<Value, String> {
    let id = format!("{:x}", Sha256::digest(&bytes[..]));
    let image = json!({"blobId": id, "mimeType": mime, "byteSize": bytes.len()});
    #[cfg(target_arch = "wasm32")]
    {
        let _ = dir;
        BLOBS.with_borrow_mut(|blobs| {
            blobs.entry(id).or_insert(bytes);
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    let path = dir.join(&id);
    #[cfg(not(target_arch = "wasm32"))]
    if !path.exists() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // Write to a temporary sibling and rename, so a crash never leaves a torn blob under
        // its content address.
        let temp = dir.join(format!(".{id}.{}", std::process::id()));
        let mut file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        std::fs::rename(&temp, &path).map_err(|e| e.to_string())?;
    }
    Ok(image)
}

/// Retain a validated image with its original bytes. The caller owns the source file.
pub(crate) fn import_image(dir: &Path, file: &SelectedFile) -> Result<Value, ImageImportError> {
    let (bytes, mime) = read_image_file(file)?;
    #[cfg(not(target_arch = "wasm32"))]
    let bytes = bytes.as_slice();
    let mut image = store(dir, bytes, mime).map_err(ImageImportError::Io)?;
    set_image_file_name(&mut image, &file.name);
    Ok(image)
}

/// The New Session form keeps inline images until its creation request is saved.
pub(crate) fn encode_picked_image(file: &SelectedFile) -> Result<Value, ImageImportError> {
    let (bytes, mime) = read_image_file(file)?;
    let mut image = encode_image(&bytes, mime);
    set_image_file_name(&mut image, &file.name);
    Ok(image)
}

fn set_image_file_name(image: &mut Value, name: &str) {
    if !name.is_empty() {
        image["fileName"] = json!(name);
    }
}

pub(crate) fn encode_image(bytes: &[u8], mime: &str) -> Value {
    use base64::Engine;
    json!({
        "data": base64::engine::general_purpose::STANDARD.encode(bytes),
        "mimeType": mime,
    })
}

/// Read a retained blob (also used by send operations on the core runtime).
pub fn read_blob(dir: &Path, id: &str) -> Result<ImageBytes, String> {
    if !valid_blob_id(id) {
        return Err(format!("Invalid image reference {id}"));
    }
    #[cfg(not(target_arch = "wasm32"))]
    return std::fs::read(dir.join(id))
        .map_err(|e| format!("Attached image is no longer available: {e}"));
    #[cfg(target_arch = "wasm32")]
    {
        let _ = dir;
        BLOBS
            .with_borrow(|blobs| blobs.get(id).cloned())
            .ok_or_else(|| crate::common::tr("message.attachments.imageUnavailable"))
    }
}

fn decode_inline(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let data = match data.strip_prefix("data:") {
        Some(rest) => rest.split_once(',').map(|(_, d)| d).unwrap_or(""),
        None => data,
    };
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.trim_end_matches('='))
        .or_else(|_| {
            base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned.trim_end_matches('='))
        })
        .map_err(|e| e.to_string())
}

fn decode(dir: &Path, source: &ImageSource, max: u32) -> Result<DecodedImage, String> {
    let image = match source {
        ImageSource::Blob(id) => ::image::load_from_memory(&read_blob(dir, id)?),
        ImageSource::Inline(data) => ::image::load_from_memory(&decode_inline(data)?),
    }
    .map_err(|e| e.to_string())?;
    let image = if image.width() > max || image.height() > max {
        image.thumbnail(max, max)
    } else {
        image
    };
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let pixels = rgba
        .pixels()
        .map(|p| {
            let [r, g, b, a] = p.0;
            ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | b as u32
        })
        .collect();
    Ok(DecodedImage {
        width,
        height,
        pixels,
    })
}

fn run_job(dir: &Path, job: BlobJob) -> Option<BlobResult> {
    let result = match job {
        BlobJob::StoreBytes { token, image } => BlobResult::Stored {
            token,
            images: clipboard_image_mime(&image)
                .and_then(|mime| {
                    #[cfg(not(target_arch = "wasm32"))]
                    let bytes = image.bytes.as_slice();
                    #[cfg(target_arch = "wasm32")]
                    let bytes = image.bytes;
                    store(dir, bytes, mime).map_err(ImageImportError::Io)
                })
                .map(|image| vec![image]),
        },
        BlobJob::ImportImages { token, files } => {
            let images = files.iter().map(|file| import_image(dir, file)).collect();
            BlobResult::Stored { token, images }
        }
        #[cfg(not(target_arch = "wasm32"))]
        BlobJob::ImportSharedImages {
            share,
            inline,
            reply,
        } => {
            let images = share
                .files()
                .iter()
                .map(|file| {
                    if inline {
                        encode_picked_image(file)
                    } else {
                        import_image(dir, file)
                    }
                })
                .collect::<Result<_, ImageImportError>>()
                .map_err(|error| format!("{error:?}"));
            drop(share);
            let _ = reply.send(images);
            return None;
        }
        #[cfg(not(target_arch = "wasm32"))]
        BlobJob::Stat { token, paths } => {
            let files = paths.into_iter().map(StatFile::inspect).collect();
            BlobResult::Stat { token, files }
        }
        BlobJob::Decode { key, source, max } => BlobResult::Decoded {
            key,
            result: decode(dir, &source, max),
        },
        BlobJob::Collect { keep } => {
            #[cfg(not(target_arch = "wasm32"))]
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if valid_blob_id(&name) && !keep.contains(&name) {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            #[cfg(target_arch = "wasm32")]
            BLOBS.with_borrow_mut(|blobs| blobs.retain(|id, _| keep.contains(id)));
            return None;
        }
    };
    Some(result)
}

#[cfg(not(target_arch = "wasm32"))]
fn run_worker(
    dir: PathBuf,
    jobs: std::sync::mpsc::Receiver<BlobJob>,
    results: ToUISender<BlobResult>,
) {
    while let Ok(job) = jobs.recv() {
        if let Some(result) = run_job(&dir, job)
            && results.send(result).is_err()
        {
            break;
        }
    }
}

/// A decoded texture and its pixel size.
pub type CachedTexture = (Texture, (usize, usize));

struct TextureEntry {
    texture: Option<Texture>,
    size: (usize, usize),
    failed: Option<String>,
    pending: bool,
}

/// GUI-thread side: job submission and the texture cache.
pub struct BlobStore {
    dir: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    jobs: Option<Sender<BlobJob>>,
    results: ToUIReceiver<BlobResult>,
    textures: HashMap<String, TextureEntry>,
    thumb_order: VecDeque<String>,
    preview_order: VecDeque<String>,
    next_token: u64,
    #[cfg(test)]
    decode_sources: usize,
}

impl BlobStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            #[cfg(not(target_arch = "wasm32"))]
            jobs: None,
            results: ToUIReceiver::default(),
            textures: HashMap::new(),
            thumb_order: VecDeque::new(),
            preview_order: VecDeque::new(),
            next_token: 0,
            #[cfg(test)]
            decode_sources: 0,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn next_token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn submit(&mut self, job: BlobJob) -> bool {
        if self.jobs.is_none() {
            let (sender, receiver) = channel();
            let dir = self.dir.clone();
            let results = self.results.sender();
            let spawned = std::thread::Builder::new()
                .name("paseo-composer-blobs".into())
                .spawn(move || run_worker(dir, receiver, results));
            if spawned.is_err() {
                return false;
            }
            self.jobs = Some(sender);
        }
        self.jobs.as_ref().is_some_and(|jobs| jobs.send(job).is_ok())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn submit(&mut self, job: BlobJob) -> bool {
        let dir = self.dir.clone();
        let results = self.results.sender();
        crate::common::background::spawn("paseo-composer-blobs", move || {
            if let Some(result) = run_job(&dir, job) {
                let _ = results.send(result);
            }
        })
        .is_ok()
    }

    pub fn try_recv(&mut self) -> Option<BlobResult> {
        self.results.try_recv().ok()
    }

    /// A cached texture for an image reference, or None while it decodes (the decode is
    /// queued once). `Err` when it cannot be decoded.
    pub fn texture(
        &mut self,
        image: &Value,
        identity: Option<&ImageIdentity>,
        thumbnail: bool,
    ) -> Result<Option<CachedTexture>, String> {
        let Some(identity) = identity else {
            return Err("Image has no content".into());
        };
        let key = identity.key(thumbnail);
        if let Some(entry) = self.textures.get(key) {
            if let Some(error) = &entry.failed {
                return Err(error.clone());
            }
            return Ok(entry.texture.clone().map(|t| (t, entry.size)));
        }
        let Some(content) = ImageContent::from_value(image) else {
            return Err("Image has no content".into());
        };
        #[cfg(test)]
        {
            self.decode_sources += 1;
        }
        let source = content.into_source();
        let key = key.to_owned();
        self.textures.insert(
            key.clone(),
            TextureEntry {
                texture: None,
                size: (0, 0),
                failed: None,
                pending: true,
            },
        );
        let max = if thumbnail {
            THUMBNAIL_PIXELS
        } else {
            PREVIEW_PIXELS
        };
        self.submit(BlobJob::Decode { key, source, max });
        Ok(None)
    }

    /// Turn a decoded image into a texture (GUI thread) and evict old ones.
    pub fn install(&mut self, cx: &mut Cx, key: String, result: Result<DecodedImage, String>) {
        let entry = match result {
            Ok(decoded) => {
                let size = (decoded.width, decoded.height);
                let buffer = ImageBuffer {
                    width: decoded.width,
                    height: decoded.height,
                    data: decoded.pixels,
                    animation: None,
                };
                TextureEntry {
                    texture: Some(buffer.into_new_texture(cx)),
                    size,
                    failed: None,
                    pending: false,
                }
            }
            Err(error) => TextureEntry {
                texture: None,
                size: (0, 0),
                failed: Some(error),
                pending: false,
            },
        };
        let thumb = key.ends_with(":thumb");
        self.textures.insert(key.clone(), entry);
        let (order, cap) = if thumb {
            (&mut self.thumb_order, THUMBNAIL_CACHE)
        } else {
            (&mut self.preview_order, PREVIEW_CACHE)
        };
        order.retain(|k| k != &key);
        order.push_back(key);
        while order.len() > cap {
            if let Some(old) = order.pop_front()
                && self.textures.get(&old).is_some_and(|e| !e.pending)
            {
                self.textures.remove(&old);
            }
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn retained_image_import_preserves_picker_mime_and_original_bytes() {
        let dir = std::env::temp_dir().join(format!("paseo-image-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("opaque");
        let blobs = dir.join("blobs");
        let bytes = b"\0\0\0\x10ftypmif1\0\0\0\0\0\0\0\x14meta\0\0\0\0\0\0\0\x08free";
        std::fs::write(&source, bytes).unwrap();
        let mut picked = StatFile::inspect(source).unwrap();
        assert!(picked.is_image);
        picked.file.mime_type = Some("image/heic".into());
        let image = import_image(&blobs, &picked.file).unwrap();
        assert_eq!(image["mimeType"], "image/heic");
        assert_eq!(image["fileName"], "opaque");
        assert_eq!(image["byteSize"], bytes.len());
        let id = image["blobId"].as_str().unwrap();
        assert_eq!(read_blob(&blobs, id).unwrap(), bytes.as_slice());

        std::fs::write(&picked.file.path, b"invalid").unwrap();
        assert!(import_image(&blobs, &picked.file).is_err());
        assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn inline_texture_hits_reuse_identity_without_opening_decode_source() {
        let image = json!({"data": "A".repeat(128 * 1024)});
        let before = INLINE_HASHES.with(|count| count.get());
        let identity = ImageContent::from_value(&image).unwrap().identity();
        assert_eq!(INLINE_HASHES.with(|count| count.get()), before + 1);

        let mut store = BlobStore::new(PathBuf::new());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let (jobs, queued) = channel();
        store.jobs = Some(jobs);
        assert!(
            store
                .texture(&image, Some(&identity), true)
                .unwrap()
                .is_none()
        );
        let BlobJob::Decode {
            key,
            source: ImageSource::Inline(data),
            max,
        } = queued.try_recv().unwrap()
        else {
            panic!("one inline thumbnail decode")
        };
        assert_eq!(data, image["data"].as_str().unwrap());
        assert_eq!(max, THUMBNAIL_PIXELS);
        assert_eq!(store.decode_sources, 1);
        store.install(
            &mut cx,
            key,
            Ok(DecodedImage {
                width: 1,
                height: 1,
                pixels: vec![0],
            }),
        );
        for _ in 0..5 {
            let (_, size) = store
                .texture(&image, Some(&identity), true)
                .unwrap()
                .unwrap();
            assert_eq!(size, (1, 1));
        }
        assert_eq!(INLINE_HASHES.with(|count| count.get()), before + 1);
        assert_eq!(store.decode_sources, 1);
        assert!(queued.try_recv().is_err());
    }
}
