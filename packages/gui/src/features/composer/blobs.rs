//! Retained composer images. Pasted, picked and dropped images become immutable,
//! content-addressed files (`<blob_dir>/<sha256>`); drafts keep only
//! `{blobId, mimeType, byteSize}`. All file IO, hashing and image decoding runs on one worker
//! thread; results return through a `ToUIReceiver` (waking the UI with a signal). Decoded
//! pixels become GPU textures on the GUI thread and are cached, so redraws never reload or
//! flicker an attachment.

use super::attachments::{raster_image_mime_type, sniff_image_mime};
use makepad_widgets::image_cache::ImageBuffer;
use makepad_widgets::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

/// Thumbnails are decoded at 2x the 48px pill for high-DPI displays.
pub const THUMBNAIL_PIXELS: u32 = 96;
/// Previews are capped to keep a single texture within every GPU's limits.
pub const PREVIEW_PIXELS: u32 = 4096;
const THUMBNAIL_CACHE: usize = 64;
const PREVIEW_CACHE: usize = 4;

pub enum BlobJob {
    /// Store raw image bytes (clipboard).
    StoreBytes { token: u64, bytes: Arc<Vec<u8>>, mime: Option<String> },
    /// Read image files from disk and store them (picker/drop).
    ImportImages { token: u64, files: Vec<(PathBuf, String)> },
    /// Stat dropped/picked paths: (path, name, size, raster image mime).
    Stat { token: u64, paths: Vec<PathBuf> },
    Decode { key: String, source: ImageSource, max: u32 },
    /// Delete blobs nobody references (startup, after drafts load).
    Collect { keep: HashSet<String> },
}

#[derive(Clone, Debug)]
pub enum ImageSource {
    Blob(String),
    /// Legacy inline base64 (`{data, mimeType}`), with or without a `data:` prefix.
    Inline(String),
}

pub struct DecodedImage {
    pub width: usize,
    pub height: usize,
    /// Packed `a<<24 | r<<16 | g<<8 | b`, Makepad's `VecBGRAu8_32`.
    pub pixels: Vec<u32>,
}

pub enum BlobResult {
    /// Draft image entries `{blobId, mimeType, byteSize}` or the error text.
    Stored { token: u64, images: Result<Vec<Value>, String> },
    Stat { token: u64, files: Result<Vec<StatFile>, String> },
    Decoded { key: String, result: Result<DecodedImage, String> },
}

#[derive(Clone, Debug)]
pub struct StatFile {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub image_mime: Option<&'static str>,
}

fn blob_path(dir: &Path, id: &str) -> Option<PathBuf> {
    // Ids are lowercase sha256 hex; anything else never reaches the filesystem.
    (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
        .then(|| dir.join(id))
}

fn store(dir: &Path, bytes: &[u8], mime: Option<&str>) -> Result<Value, String> {
    let mime = sniff_image_mime(bytes)
        .map(str::to_owned)
        .or_else(|| mime.map(str::to_owned))
        .unwrap_or_else(|| "image/png".to_owned());
    let id = format!("{:x}", Sha256::digest(bytes));
    let path = blob_path(dir, &id).ok_or("invalid blob id")?;
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
    Ok(json!({"blobId": id, "mimeType": mime, "byteSize": bytes.len()}))
}

/// Read a retained blob (also used by send operations on the core runtime).
pub fn read_blob(dir: &Path, id: &str) -> Result<Vec<u8>, String> {
    let path = blob_path(dir, id).ok_or_else(|| format!("Invalid image reference {id}"))?;
    std::fs::read(&path).map_err(|e| format!("Attached image is no longer available: {e}"))
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
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(cleaned.trim_end_matches('=')))
        .map_err(|e| e.to_string())
}

fn decode(dir: &Path, source: &ImageSource, max: u32) -> Result<DecodedImage, String> {
    let bytes = match source {
        ImageSource::Blob(id) => read_blob(dir, id)?,
        ImageSource::Inline(data) => decode_inline(data)?,
    };
    let image = ::image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
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
    Ok(DecodedImage { width, height, pixels })
}

fn run_worker(dir: PathBuf, jobs: std::sync::mpsc::Receiver<BlobJob>, results: ToUISender<BlobResult>) {
    while let Ok(job) = jobs.recv() {
        let result = match job {
            BlobJob::StoreBytes { token, bytes, mime } => BlobResult::Stored {
                token,
                images: store(&dir, &bytes, mime.as_deref()).map(|image| vec![image]),
            },
            BlobJob::ImportImages { token, files } => {
                let images = files
                    .iter()
                    .map(|(path, name)| {
                        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
                        store(&dir, &bytes, raster_image_mime_type(name))
                    })
                    .collect();
                BlobResult::Stored { token, images }
            }
            BlobJob::Stat { token, paths } => {
                let files = paths
                    .into_iter()
                    .map(|path| {
                        let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.to_string_lossy().into_owned());
                        let image_mime = raster_image_mime_type(&name);
                        Ok(StatFile { path, name, size: metadata.len(), image_mime })
                    })
                    .collect();
                BlobResult::Stat { token, files }
            }
            BlobJob::Decode { key, source, max } => BlobResult::Decoded { key, result: decode(&dir, &source, max) },
            BlobJob::Collect { keep } => {
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if blob_path(&dir, &name).is_some() && !keep.contains(&name) {
                            let _ = std::fs::remove_file(entry.path());
                        }
                    }
                }
                continue;
            }
        };
        if results.send(result).is_err() {
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
    jobs: Option<Sender<BlobJob>>,
    results: ToUIReceiver<BlobResult>,
    textures: HashMap<String, TextureEntry>,
    thumb_order: VecDeque<String>,
    preview_order: VecDeque<String>,
    next_token: u64,
}

impl BlobStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            jobs: None,
            results: ToUIReceiver::default(),
            textures: HashMap::new(),
            thumb_order: VecDeque::new(),
            preview_order: VecDeque::new(),
            next_token: 0,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn next_token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    pub fn submit(&mut self, job: BlobJob) {
        if self.jobs.is_none() {
            let (sender, receiver) = channel();
            let dir = self.dir.clone();
            let results = self.results.sender();
            let spawned = std::thread::Builder::new()
                .name("paseo-composer-blobs".into())
                .spawn(move || run_worker(dir, receiver, results));
            if spawned.is_err() {
                return;
            }
            self.jobs = Some(sender);
        }
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    pub fn try_recv(&mut self) -> Option<BlobResult> {
        self.results.try_recv().ok()
    }

    fn cache_key(image: &Value, thumbnail: bool) -> Option<(String, ImageSource)> {
        let suffix = if thumbnail { "thumb" } else { "full" };
        if let Some(id) = image.get("blobId").and_then(Value::as_str) {
            return Some((format!("blob:{id}:{suffix}"), ImageSource::Blob(id.to_owned())));
        }
        let data = image.get("data").and_then(Value::as_str).filter(|d| !d.is_empty())?;
        let digest = format!("{:x}", Sha256::digest(data.as_bytes()));
        Some((format!("inline:{digest}:{suffix}"), ImageSource::Inline(data.to_owned())))
    }

    /// A cached texture for an image reference, or None while it decodes (the decode is
    /// queued once). `Err` when it cannot be decoded.
    pub fn texture(&mut self, image: &Value, thumbnail: bool) -> Result<Option<CachedTexture>, String> {
        let Some((key, source)) = Self::cache_key(image, thumbnail) else {
            return Err("Image has no content".into());
        };
        if let Some(entry) = self.textures.get(&key) {
            if let Some(error) = &entry.failed {
                return Err(error.clone());
            }
            return Ok(entry.texture.clone().map(|t| (t, entry.size)));
        }
        self.textures.insert(key.clone(), TextureEntry { texture: None, size: (0, 0), failed: None, pending: true });
        let max = if thumbnail { THUMBNAIL_PIXELS } else { PREVIEW_PIXELS };
        self.submit(BlobJob::Decode { key, source, max });
        Ok(None)
    }

    /// Turn a decoded image into a texture (GUI thread) and evict old ones.
    pub fn install(&mut self, cx: &mut Cx, key: String, result: Result<DecodedImage, String>) {
        let entry = match result {
            Ok(decoded) => {
                let size = (decoded.width, decoded.height);
                let buffer = ImageBuffer { width: decoded.width, height: decoded.height, data: decoded.pixels, animation: None };
                TextureEntry { texture: Some(buffer.into_new_texture(cx)), size, failed: None, pending: false }
            }
            Err(error) => TextureEntry { texture: None, size: (0, 0), failed: Some(error), pending: false },
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
