//! Queued image decoding (raster via `image`, SVG via `resvg`) and GUI textures, plus the
//! material file icon cache (Flutter `MaterialFileIcon`, `SvgPicture`, `Image.memory`).
//! Jobs are shared with assistant images. Finished pixels arrive through a `ToUISender`
//! (Signal wake) and become textures in [`poll`].

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Condvar, Mutex};

use makepad_widgets::image_cache::ImageBuffer;
use makepad_widgets::*;

use super::icons_data::{EXTENSION_TO_ICON, SVG_ICONS};

/// Decoded RGBA pixels (straight alpha).
pub struct Decoded {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// The largest side a decoded image keeps (textures above this are downscaled).
pub const MAX_SIDE: u32 = 4096;

pub fn is_svg(bytes: &[u8], mime: &str) -> bool {
    if mime.split(';').next().map(str::trim) == Some("image/svg+xml") {
        return true;
    }
    let head = &bytes[..bytes.len().min(512)];
    let head = String::from_utf8_lossy(head);
    head.contains("<svg")
}

/// Decodes `bytes`; SVGs render at their intrinsic size, scaled so the longer side is at least
/// `min_side` (for crisp icons) and at most [`MAX_SIDE`].
pub fn decode(bytes: &[u8], mime: &str, min_side: u32) -> Result<Decoded, String> {
    if is_svg(bytes, mime) {
        return decode_svg(bytes, |w, h| (min_side as f32 / w.max(h)).max(1.0));
    }
    decode_raster(bytes, MAX_SIDE, MAX_SIDE).map(|(decoded, _)| decoded)
}

/// Decodes `bytes` to fit inside `max_width` x `max_height` pixels (each at most [`MAX_SIDE`]):
/// rasters only shrink, SVGs render at the scale that fills the box. The flag tells whether the
/// pixels are the source's full resolution (a raster that already fit), so a larger box could
/// not sharpen them.
pub fn decode_to_fit(
    bytes: &[u8],
    mime: &str,
    max_width: u32,
    max_height: u32,
) -> Result<(Decoded, bool), String> {
    let (max_width, max_height) = (max_width.clamp(1, MAX_SIDE), max_height.clamp(1, MAX_SIDE));
    if is_svg(bytes, mime) {
        let decoded = decode_svg(bytes, |w, h| {
            (max_width as f32 / w).min(max_height as f32 / h)
        })?;
        return Ok((decoded, false));
    }
    decode_raster(bytes, max_width, max_height)
}

fn decode_raster(bytes: &[u8], max_width: u32, max_height: u32) -> Result<(Decoded, bool), String> {
    let image = ::image::load_from_memory(bytes).map_err(|error| error.to_string())?;
    let fits = image.width() <= max_width && image.height() <= max_height;
    let image = if fits {
        image
    } else {
        // Keeps the aspect ratio, as large as fits the box.
        image.resize(
            max_width,
            max_height,
            ::image::imageops::FilterType::Triangle,
        )
    };
    let rgba = image.into_rgba8();
    Ok((
        Decoded {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            rgba: rgba.into_raw(),
        },
        fits,
    ))
}

/// Renders an SVG at `scale(width, height)` of its intrinsic size, the longer side capped at
/// [`MAX_SIDE`].
fn decode_svg(bytes: &[u8], scale: impl Fn(f32, f32) -> f32) -> Result<Decoded, String> {
    use resvg::{tiny_skia, usvg};
    let options = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &options).map_err(|error| error.to_string())?;
    let size = tree.size();
    let (w, h) = (size.width().max(1.0), size.height().max(1.0));
    let longer = w.max(h);
    let scale = scale(w, h).min(MAX_SIDE as f32 / longer);
    let (pw, ph) = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
    let mut pixmap = tiny_skia::Pixmap::new(pw.max(1), ph.max(1)).ok_or("Invalid SVG size")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    let mut rgba = pixmap.take();
    // tiny-skia stores premultiplied alpha; textures take straight alpha.
    for pixel in rgba.chunks_exact_mut(4) {
        let a = pixel[3] as u32;
        if a > 0 && a < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    Ok(Decoded {
        width: pw.max(1) as usize,
        height: ph.max(1) as usize,
        rgba,
    })
}

type DecodeJob = Box<dyn FnOnce() + Send>;

#[cfg(not(target_arch = "wasm32"))]
struct DecodeQueue {
    jobs: VecDeque<(u64, DecodeJob)>,
    next: u64,
    workers: usize,
}

/// Worker threads decoding images; started on first use and kept for the process.
#[cfg(not(target_arch = "wasm32"))]
const DECODE_WORKERS: usize = 2;
#[cfg(not(target_arch = "wasm32"))]
static DECODE_QUEUE: Mutex<DecodeQueue> = Mutex::new(DecodeQueue {
    jobs: VecDeque::new(),
    next: 0,
    workers: 0,
});
#[cfg(not(target_arch = "wasm32"))]
static DECODE_READY: Condvar = Condvar::new();

/// Queues decoding work behind earlier jobs; returns its tag for [`cancel_decode`].
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_decode(job: impl FnOnce() + Send + 'static) -> u64 {
    let mut queue = DECODE_QUEUE.lock().unwrap();
    queue.next += 1;
    let tag = queue.next;
    queue.jobs.push_back((tag, Box::new(job)));
    if queue.workers < DECODE_WORKERS
        && std::thread::Builder::new()
            .name("paseo-image-decode".into())
            .spawn(decode_worker)
            .is_ok()
    {
        queue.workers += 1;
    }
    drop(queue);
    DECODE_READY.notify_one();
    tag
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static NEXT_DECODE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static DECODE_JOBS: RefCell<HashMap<u64, DecodeJob>> = RefCell::default();
}

#[cfg(target_arch = "wasm32")]
pub fn spawn_decode(job: impl FnOnce() + Send + 'static) -> u64 {
    let tag = NEXT_DECODE.get() + 1;
    NEXT_DECODE.set(tag);
    DECODE_JOBS.with(|jobs| jobs.borrow_mut().insert(tag, Box::new(job)));
    crate::common::background::spawn("paseo-image-decode", move || {
        let job = DECODE_JOBS.with(|jobs| jobs.borrow_mut().remove(&tag));
        if let Some(job) = job {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        }
    })
    .expect("queue image decode");
    tag
}

/// Drops a queued job that has not started; a running one finishes.
#[cfg(not(target_arch = "wasm32"))]
pub fn cancel_decode(tag: u64) {
    DECODE_QUEUE
        .lock()
        .unwrap()
        .jobs
        .retain(|(queued, _)| *queued != tag);
}

#[cfg(target_arch = "wasm32")]
pub fn cancel_decode(tag: u64) {
    DECODE_JOBS.with(|jobs| jobs.borrow_mut().remove(&tag));
}

#[cfg(not(target_arch = "wasm32"))]
fn decode_worker() {
    loop {
        let job = {
            let mut queue = DECODE_QUEUE.lock().unwrap();
            loop {
                if let Some((_, job)) = queue.jobs.pop_front() {
                    break job;
                }
                queue = DECODE_READY.wait(queue).unwrap();
            }
        };
        // A panicking decoder must not take the worker with it.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
    }
}

pub fn texture(cx: &mut Cx, decoded: &Decoded) -> Option<Texture> {
    ImageBuffer::new(&decoded.rgba, decoded.width, decoded.height)
        .ok()
        .map(|buffer| buffer.into_new_texture(cx))
}

/// A texture keyed by an owner key, decoded off-thread.
#[derive(Clone)]
pub enum ImageSlot {
    Decoding,
    Ready {
        texture: Texture,
        width: usize,
        height: usize,
    },
    Failed(String),
}

struct Job {
    key: String,
    result: Result<Decoded, String>,
}

struct Cache {
    receiver: ToUIReceiver<Job>,
    slots: HashMap<String, ImageSlot>,
    /// Decoded web images, least recently used first, with their texture bytes.
    web: VecDeque<(String, usize)>,
}

thread_local! {
    static CACHE: RefCell<Cache> =
        RefCell::new(Cache { receiver: ToUIReceiver::default(), slots: HashMap::new(), web: VecDeque::new() });
}

/// Web images (Flutter `Image.network` through `PaintingBinding.imageCache`) are shared by
/// every panel and kept for the process under the image cache defaults: 1000 images and
/// 100 MiB of decoded pixels, least recently used evicted first. Failures are not kept.
const WEB_PREFIX: &str = "web\u{0}";
const WEB_MAX_IMAGES: usize = 1000;
const WEB_MAX_BYTES: usize = 100 << 20;

/// The shared cache key of a web image URL.
pub fn web_key(url: &str) -> String {
    format!("{WEB_PREFIX}{url}")
}

/// Whether the caller should fetch `url`: nothing is cached or in flight for it, or the last
/// fetch failed. Marks it in flight.
pub fn begin_web_fetch(url: &str) -> bool {
    let key = web_key(url);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        match cache.slots.get(&key) {
            Some(ImageSlot::Decoding | ImageSlot::Ready { .. }) => false,
            Some(ImageSlot::Failed(_)) | None => {
                cache.slots.insert(key, ImageSlot::Decoding);
                true
            }
        }
    })
}

/// The fetching panel went away before its answer: the next panel to show the image fetches it.
pub fn abandon_web_fetch(url: &str) {
    let key = web_key(url);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if matches!(cache.slots.get(&key), Some(ImageSlot::Decoding)) {
            cache.slots.remove(&key);
        }
    });
}

/// A web fetch finished: decodes the body, or records the failure.
pub fn finish_web_fetch(url: &str, body: Result<Vec<u8>, String>) {
    let key = web_key(url);
    match body {
        Ok(bytes) => {
            CACHE.with(|cache| {
                cache.borrow_mut().slots.remove(&key);
            });
            request(&key, Arc::new(bytes), "", 0);
        }
        Err(error) => CACHE.with(|cache| {
            cache
                .borrow_mut()
                .slots
                .insert(key, ImageSlot::Failed(error));
        }),
    }
}

/// Starts decoding `bytes` under `key` unless the key is known.
pub fn request(key: &str, bytes: Arc<Vec<u8>>, mime: &str, min_side: u32) {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.slots.contains_key(key) {
            return;
        }
        cache.slots.insert(key.to_owned(), ImageSlot::Decoding);
        let sender = cache.receiver.sender();
        let key = key.to_owned();
        let mime = mime.to_owned();
        spawn_decode(move || {
            let result = std::panic::catch_unwind(|| decode(&bytes, &mime, min_side))
                .unwrap_or_else(|_| Err("Invalid image data".to_owned()));
            let _ = sender.send(Job { key, result });
        });
    });
}

pub fn slot(key: &str) -> Option<ImageSlot> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let slot = cache.slots.get(key).cloned();
        // A shown web image is the most recently used.
        if matches!(slot, Some(ImageSlot::Ready { .. }))
            && key.starts_with(WEB_PREFIX)
            && let Some(index) = cache.web.iter().position(|(k, _)| k == key)
            && let Some(entry) = cache.web.remove(index)
        {
            cache.web.push_back(entry);
        }
        slot
    })
}

/// Drops every slot whose key starts with `prefix` (a closed panel's images).
pub fn forget_prefix(prefix: &str) {
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .slots
            .retain(|key, _| !key.starts_with(prefix))
    });
}

fn evict_web(cache: &mut Cache) {
    let mut total: usize = cache.web.iter().map(|(_, bytes)| bytes).sum();
    while cache.web.len() > WEB_MAX_IMAGES || (total > WEB_MAX_BYTES && cache.web.len() > 1) {
        let Some((key, bytes)) = cache.web.pop_front() else {
            break;
        };
        total -= bytes;
        cache.slots.remove(&key);
    }
}

/// Turns finished decodes into textures; returns whether anything arrived.
pub fn poll(cx: &mut Cx) -> bool {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut any = false;
        while let Ok(job) = cache.receiver.try_recv() {
            any = true;
            let slot = match job.result {
                Ok(decoded) => match texture(cx, &decoded) {
                    Some(texture) => ImageSlot::Ready {
                        texture,
                        width: decoded.width,
                        height: decoded.height,
                    },
                    None => ImageSlot::Failed("Invalid image data".to_owned()),
                },
                Err(error) => ImageSlot::Failed(error),
            };
            // A slot forgotten meanwhile stays forgotten.
            if cache.slots.contains_key(&job.key) {
                if let (true, ImageSlot::Ready { width, height, .. }) =
                    (job.key.starts_with(WEB_PREFIX), &slot)
                {
                    let bytes = width * height * 4;
                    cache.web.push_back((job.key.clone(), bytes));
                    cache.slots.insert(job.key, slot);
                    evict_web(&mut cache);
                } else {
                    cache.slots.insert(job.key, slot);
                }
            }
        }
        any
    })
}

/// The material icon name for a file name (Flutter `_rawFileIconSvg`).
pub fn file_icon_name(file_name: &str) -> &'static str {
    if let Some(index) = file_name.rfind('.')
        && index + 1 < file_name.len()
    {
        let extension = file_name[index + 1..].to_lowercase();
        if let Some((_, icon)) = EXTENSION_TO_ICON.iter().find(|(ext, _)| *ext == extension)
            && SVG_ICONS.iter().any(|(name, _)| name == icon)
        {
            return icon;
        }
    }
    "_default"
}

/// The icon texture for a file name; starts its decode on first use (the caller redraws once
/// [`poll`] reports arrivals).
pub fn file_icon(file_name: &str) -> Option<Texture> {
    let name = file_icon_name(file_name);
    let key = format!("icon:{name}");
    match slot(&key) {
        Some(ImageSlot::Ready { texture, .. }) => Some(texture),
        Some(_) => None,
        None => {
            let svg = SVG_ICONS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, svg)| *svg)
                .unwrap_or("");
            request(&key, Arc::new(svg.as_bytes().to_vec()), "image/svg+xml", 64);
            None
        }
    }
}
