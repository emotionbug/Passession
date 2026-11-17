//! Port of `markdown_image.dart`: assistant-message images. HTTP(S) uses core
//! host-origin admission and the selected connection transport; `data:` decodes in place,
//! workspace files are read through the host (SDK `read_file`). Decoding runs
//! off the GUI thread on the shared image workers; the texture is created on it.
//! Inline images decode at their displayed size; full-quality callers (lightbox,
//! avatars) decode up to the Files decoder's limit. SVGs (`data:` and
//! workspace files) render through resvg on desktop as flutter_svg did; an
//! image that cannot be read or decoded falls back to its alt text.

use super::links::is_absolute_file_path;
use crate::features::files::images::{self as decoder, MAX_SIDE};
use base64::Engine;
use makepad_widgets::image_cache::ImageBuffer;
use makepad_widgets::*;
use paseo_gui_core::{CoreHandle, RequestId};
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

/// acquisition-cache.ts: at most 500 acquisitions, least recently used first.
const CAPACITY: usize = 500;
/// Decoded pixels kept across acquisitions (Flutter's image cache default), least
/// recently used evicted first; the image just settled stays even when larger.
const MAX_DECODED_BYTES: usize = 100 << 20;
/// Acquisitions fetching or decoding at once; later ones wait in first-sight order.
const MAX_ACTIVE: usize = 4;
const MAX_FILE_BYTES: usize = 32 * 1024 * 1024;
/// Height of the Markdown image box (markdown/view.rs `image_bounds`).
const INLINE_MAX_HEIGHT: f64 = 320.0;

#[derive(Clone)]
pub enum ImageState {
    Loading,
    Ready {
        texture: Texture,
        width: usize,
        height: usize,
    },
    Failed,
    Error(String),
}

/// utils/assistant-image-source.ts AssistantImageSourceResolution.
#[derive(Clone, Debug, PartialEq)]
pub enum ImageSource {
    Direct(String),
    File { cwd: String, path: String },
}

fn is_home_relative(path: &str) -> bool {
    path == "~" || path.starts_with("~/") || path.starts_with("~\\")
}

fn is_within(candidate: &str, root: &str) -> bool {
    let normalize = |value: &str| {
        let mut normalized = value.replace('\\', "/");
        let drive_root = normalized.len() <= 3 && normalized.as_bytes().get(1) == Some(&b':');
        if normalized != "/" && !drive_root {
            normalized = normalized.trim_end_matches('/').to_owned();
        }
        if normalized.len() >= 3
            && normalized.as_bytes()[1] == b':'
            && normalized.as_bytes()[2] == b'/'
        {
            let mut chars = normalized.chars();
            let first = chars.next().unwrap().to_ascii_uppercase();
            normalized = format!("{first}{}", chars.as_str());
        }
        normalized
    };
    let path = normalize(candidate);
    let base = normalize(root);
    if base == "/" {
        return path.starts_with('/');
    }
    path == base || path.starts_with(&format!("{base}/"))
}

/// resolveAssistantImageSource.
pub fn resolve_image_source(source: &str, workspace_root: Option<&str>) -> Option<ImageSource> {
    let trimmed = source.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("http:")
        || lower.starts_with("https:")
        || lower.starts_with("data:")
        || lower.starts_with("blob:")
    {
        return Some(ImageSource::Direct(trimmed.to_owned()));
    }
    let path_source = trimmed.split(['?', '#']).next().unwrap_or("");
    let mut path = if let Some(rest) = path_source.strip_prefix("file://") {
        let path = match rest.find('/') {
            Some(slash) => &rest[slash..],
            None => "",
        };
        let decoded = super::links::percent_decode(path).unwrap_or_else(|| path.to_owned());
        let bytes = decoded.as_bytes();
        if bytes.len() >= 4
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
            && bytes[3] == b'/'
        {
            decoded[1..].to_owned()
        } else {
            decoded
        }
    } else {
        super::links::percent_decode(path_source).unwrap_or_else(|| path_source.to_owned())
    };
    path = path.trim().to_owned();
    if path.is_empty() {
        return None;
    }
    if is_home_relative(&path) {
        return Some(ImageSource::File {
            cwd: "~".into(),
            path,
        });
    }
    let root = workspace_root.unwrap_or("").trim();
    if !is_absolute_file_path(&path) {
        if root.is_empty() || !is_absolute_file_path(root) {
            return None;
        }
        return Some(ImageSource::File {
            cwd: root.to_owned(),
            path,
        });
    }
    if !root.is_empty() && is_absolute_file_path(root) && is_within(&path, root) {
        return Some(ImageSource::File {
            cwd: root.to_owned(),
            path,
        });
    }
    let bytes = path.as_bytes();
    let fs_root = if path.starts_with('/') {
        Some("/".to_owned())
    } else if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        Some(format!("{}/", &path[..2]))
    } else if let Some(rest) = path.strip_prefix("\\\\") {
        let mut parts = rest.splitn(3, '\\');
        match (parts.next(), parts.next()) {
            (Some(server), Some(share)) if !server.is_empty() && !share.is_empty() => {
                Some(format!("\\\\{server}\\{share}"))
            }
            _ => None,
        }
    } else {
        None
    };
    fs_root.map(|cwd| ImageSource::File { cwd, path })
}

fn is_svg(source: &str) -> bool {
    let lower = source.trim().to_lowercase();
    lower.starts_with("data:image/svg+xml") || {
        let path = lower.split(['?', '#']).next().unwrap_or("");
        path.ends_with(".svg")
    }
}

/// The pixel box an acquisition decodes into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fit {
    width: u32,
    height: u32,
}

impl Fit {
    /// Full quality (lightbox, avatars): the Files decoder's own limit.
    const FULL: Fit = Fit {
        width: MAX_SIDE,
        height: MAX_SIDE,
    };

    /// The inline Markdown box, `width` by the 320px height, in physical pixels; rounded
    /// up so small width changes reuse the decode.
    fn inline(width: f64, dpi: f64) -> Fit {
        let side = |logical: f64, step: u32| {
            let pixels = (logical.max(1.0) * dpi).ceil() as u32;
            pixels
                .div_ceil(step)
                .saturating_mul(step)
                .clamp(1, MAX_SIDE)
        };
        Fit {
            width: side(width, 256),
            height: side(INLINE_MAX_HEIGHT, 64),
        }
    }

    fn covers(self, other: Fit) -> bool {
        self.width >= other.width && self.height >= other.height
    }

    fn union(self, other: Fit) -> Fit {
        Fit {
            width: self.width.max(other.width),
            height: self.height.max(other.height),
        }
    }
}

struct Decoded {
    key: String,
    id: u64,
    fit: Fit,
    /// The pixels, and whether they are the source's full resolution.
    result: Result<(ImageBuffer, bool), String>,
}

/// Off-thread decode results, picked up on `Event::Signal`.
static DECODED: LazyLock<Mutex<Vec<Decoded>>> = LazyLock::new(|| Mutex::new(Vec::new()));
/// File bytes and their mime type.
type FileBytes = (Vec<u8>, String);

/// Bytes read by a core operation, keyed by the operation's request id.
static FILE_BYTES: LazyLock<Mutex<HashMap<String, FileBytes>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct Entry {
    state: ImageState,
    /// Texture bytes of a ready image.
    bytes: usize,
    last_used: u64,
    /// The box a ready image was decoded for while a larger box would sharpen it;
    /// `None` for full-resolution pixels or once a larger decode failed.
    fit: Option<Fit>,
}

/// Where an acquisition's bytes come from.
enum Source {
    Data {
        bytes: Vec<u8>,
        mime: String,
    },
    Http {
        core: CoreHandle,
        host: String,
        uri: String,
    },
    File {
        core: CoreHandle,
        host: String,
        cwd: String,
        path: String,
    },
}

struct Queued {
    key: String,
    fit: Fit,
    source: Source,
}

/// A started acquisition; responses and decodes of any other `id` are stale.
struct Active {
    id: u64,
    fit: Fit,
    /// The submitted decode, cancelled if the key leaves the cache first.
    decode: Option<u64>,
}

struct HttpImage {
    key: String,
    id: u64,
    core: CoreHandle,
}

#[derive(Default)]
struct Images {
    entries: HashMap<String, Entry>,
    /// Use clock ordering entries for least-recently-used eviction.
    tick: u64,
    /// Texture bytes of every ready entry.
    bytes: usize,
    /// Acquisitions waiting for an active slot, in first-sight order.
    queued: VecDeque<Queued>,
    active: HashMap<String, Active>,
    next_id: u64,
    http: HashMap<RequestId, HttpImage>,
    operations: HashMap<RequestId, (String, u64)>,
    /// Changes when an acquisition settles or a route is invalidated, never on a
    /// new `Loading` entry.
    version: u64,
    generations: HashMap<String, Option<u64>>,
    /// Draw lists that drew each still-loading key; its outcome repaints them.
    waiting: HashMap<String, Vec<DrawListId>>,
    /// Owners of loading keys evicted (possibly mid-draw), repainted by `handle_event`
    /// so they acquire again.
    retry: Vec<DrawListId>,
}

thread_local! {
    static IMAGES: std::cell::RefCell<Images> = std::cell::RefCell::new(Images::default());
}

pub fn version() -> u64 {
    IMAGES.with(|i| i.borrow().version)
}

fn host_generations(state: &serde_json::Value) -> HashMap<String, Option<u64>> {
    let mut generations = HashMap::new();
    for host in state["hosts"].as_array().into_iter().flatten() {
        let generation = host["httpGeneration"].as_u64();
        for id in [host["serverId"].as_str(), host["config"]["id"].as_str()]
            .into_iter()
            .flatten()
            .chain(
                host["connections"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|config| config["id"].as_str()),
            )
        {
            generations.insert(id.to_string(), generation);
        }
    }
    generations
}

fn http_cache_key(host: &str, generation: Option<u64>, uri: &str) -> String {
    format!("http:{host}\0{generation:?}\0{uri}")
}

/// Invalidate before dispatching responses: a prior route must not satisfy the
/// same URL after a normal/tailnet switch, reconfiguration or account change.
/// Owners still waiting on a dropped route repaint to acquire through the new one.
pub fn sync_hosts(cx: &mut Cx, state: &serde_json::Value) {
    IMAGES.with(|cell| {
        let images = &mut *cell.borrow_mut();
        let generations = host_generations(state);
        let changed: Vec<String> = images
            .generations
            .iter()
            .filter(|(host, generation)| generations.get(*host) != Some(*generation))
            .map(|(host, _)| host.clone())
            .collect();
        for host in changed {
            let prefix = format!("http:{host}\0");
            let keys: Vec<String> = images
                .entries
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect();
            for key in keys {
                forget(images, &key);
            }
            images.waiting.retain(|key, owners| {
                if !key.starts_with(&prefix) {
                    return true;
                }
                for owner in owners.drain(..) {
                    cx.redraw_list(owner);
                }
                false
            });
            images.version = images.version.wrapping_add(1);
        }
        images.generations = generations;
        pump(images);
    });
}

/// Queues the decode on the shared image workers; returns its tag.
fn submit_decode(key: String, id: u64, fit: Fit, bytes: Vec<u8>, mime: String) -> u64 {
    decoder::spawn_decode(move || {
        let result = std::panic::catch_unwind(|| decode(&bytes, &mime, fit))
            .unwrap_or_else(|_| Err("Image decoding failed".into()));
        DECODED.lock().unwrap().push(Decoded {
            key,
            id,
            fit,
            result,
        });
        SignalToUI::set_ui_signal();
    })
}

/// image-decoding.ts: an image counts only once it decodes to positive
/// dimensions. Raster formats and SVG (rendered by resvg, as flutter_svg
/// draws `SvgPicture.memory` on desktop) share the Files decoder: rasters
/// shrink into `fit`, SVGs rasterize to fill it so they stay crisp.
fn decode(bytes: &[u8], mime: &str, fit: Fit) -> Result<(ImageBuffer, bool), String> {
    let (decoded, full) = decoder::decode_to_fit(bytes, mime, fit.width, fit.height)?;
    if decoded.width == 0 || decoded.height == 0 {
        return Err("Image has no decodable dimensions".into());
    }
    let buffer = ImageBuffer::new(&decoded.rgba, decoded.width, decoded.height)
        .map_err(|e| format!("{e:?}"))?;
    Ok((buffer, full))
}

fn touch(images: &mut Images) -> u64 {
    images.tick += 1;
    images.tick
}

/// Drops `key` with whatever acquisition it has queued or running. Its waiting
/// owners stay for the caller to repaint.
fn forget(images: &mut Images, key: &str) {
    if let Some(entry) = images.entries.remove(key) {
        images.bytes -= entry.bytes;
    }
    images.queued.retain(|queued| queued.key != key);
    if let Some(active) = images.active.remove(key) {
        if let Some(tag) = active.decode {
            decoder::cancel_decode(tag);
        }
        images.http.retain(|id, request| {
            if request.key != key {
                return true;
            }
            request.core.cancel_request(id);
            false
        });
    }
}

/// Evicts least recently used entries beyond [`CAPACITY`], then ready images
/// until the decoded bytes fit [`MAX_DECODED_BYTES`]; `keep` always stays.
fn evict(images: &mut Images, keep: &str) {
    loop {
        let over_count = images.entries.len() > CAPACITY;
        if !over_count && images.bytes <= MAX_DECODED_BYTES {
            break;
        }
        let victim = images
            .entries
            .iter()
            .filter(|(key, entry)| key.as_str() != keep && (over_count || entry.bytes > 0))
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone());
        let Some(victim) = victim else {
            break;
        };
        forget(images, &victim);
        if let Some(owners) = images.waiting.remove(&victim) {
            images.retry.extend(owners);
        }
    }
}

/// Stores a settled acquisition and repaints only the draw lists that showed it loading.
fn settle(
    cx: &mut Cx,
    images: &mut Images,
    key: &str,
    state: ImageState,
    bytes: usize,
    fit: Option<Fit>,
) {
    let last_used = touch(images);
    let entry = Entry {
        state,
        bytes,
        last_used,
        fit,
    };
    if let Some(previous) = images.entries.insert(key.to_owned(), entry) {
        images.bytes -= previous.bytes;
    }
    images.bytes += bytes;
    images.version = images.version.wrapping_add(1);
    evict(images, key);
    for owner in images.waiting.remove(key).into_iter().flatten() {
        cx.redraw_list(owner);
    }
}

/// A failed acquisition settles as `state`, unless it was sharpening a ready
/// image: that stays as decoded and is not acquired again.
fn settle_failure(cx: &mut Cx, images: &mut Images, key: &str, state: ImageState) {
    if let Some(entry) = images.entries.get_mut(key)
        && matches!(entry.state, ImageState::Ready { .. })
    {
        entry.fit = None;
        images.waiting.remove(key);
        return;
    }
    settle(cx, images, key, state, 0, None);
}

/// Records the draw list showing `key`'s placeholder, repainted once it settles.
fn wait(owner: Option<DrawListId>, images: &mut Images, key: &str) {
    let Some(owner) = owner else {
        return;
    };
    let owners = images.waiting.entry(key.to_owned()).or_default();
    if !owners.contains(&owner) {
        owners.push(owner);
    }
}

/// What acquiring `resolved` takes; `Err` is the state shown without acquiring
/// (`Loading` until a core exists).
fn acquisition(
    resolved: ImageSource,
    svg: bool,
    core: Option<&CoreHandle>,
    host_id: &str,
) -> Result<Source, ImageState> {
    match resolved {
        ImageSource::Direct(uri) => {
            let lower = uri.to_lowercase();
            if lower.starts_with("data:") {
                let Some((head, data)) = uri.split_once(',') else {
                    return Err(ImageState::Failed);
                };
                let head = head.to_lowercase();
                let bytes = if head.contains(";base64") {
                    base64::engine::general_purpose::STANDARD
                        .decode(data.trim())
                        .ok()
                } else {
                    super::links::percent_decode(data).map(String::into_bytes)
                };
                let Some(bytes) = bytes else {
                    return Err(ImageState::Failed);
                };
                let mime = head
                    .trim_start_matches("data:")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .to_owned();
                return Ok(Source::Data { bytes, mime });
            }
            if !(lower.starts_with("http:") || lower.starts_with("https:")) || svg {
                // blob: URLs belong to a browser page; remote SVGs show their alt text.
                return Err(ImageState::Failed);
            }
            let Some(core) = core else {
                return Err(ImageState::Loading);
            };
            Ok(Source::Http {
                core: core.clone(),
                host: host_id.to_owned(),
                uri,
            })
        }
        ImageSource::File { cwd, path } => {
            let Some(core) = core else {
                return Err(ImageState::Loading);
            };
            Ok(Source::File {
                core: core.clone(),
                host: host_id.to_owned(),
                cwd,
                path,
            })
        }
    }
}

/// Starts queued acquisitions while fewer than [`MAX_ACTIVE`] fetch or decode.
fn pump(images: &mut Images) {
    while images.active.len() < MAX_ACTIVE {
        let Some(Queued { key, fit, source }) = images.queued.pop_front() else {
            break;
        };
        images.next_id += 1;
        let id = images.next_id;
        let decode = match source {
            Source::Data { bytes, mime } => Some(submit_decode(key.clone(), id, fit, bytes, mime)),
            Source::Http { core, host, uri } => {
                let request_id = core.host_http_get(&host, &uri, MAX_FILE_BYTES);
                images.http.insert(
                    request_id,
                    HttpImage {
                        key: key.clone(),
                        id,
                        core,
                    },
                );
                None
            }
            Source::File {
                core,
                host,
                cwd,
                path,
            } => {
                // The bytes stay in a private side buffer keyed by a token;
                // the response carries only the token (no base64 copy).
                let token = uuid::Uuid::new_v4().to_string();
                let request = core.operation(&host, move |client| async move {
                    let file = client
                        .read_file(
                            &cwd,
                            &path,
                            MAX_FILE_BYTES,
                            paseo_client::RequestOptions::default(),
                        )
                        .await?;
                    // The mime travels with the bytes so an SVG decodes as one.
                    let mime = file
                        .metadata
                        .mime
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    FILE_BYTES
                        .lock()
                        .unwrap()
                        .insert(token.clone(), (file.bytes.to_vec(), mime));
                    Ok(serde_json::json!({"token": token}))
                });
                images.operations.insert(request, (key.clone(), id));
                None
            }
        };
        images.active.insert(key, Active { id, fit, decode });
    }
}

/// Whether `id` is the acquisition `key` is still waiting for.
fn current(images: &Images, key: &str, id: u64) -> bool {
    images.active.get(key).is_some_and(|active| active.id == id)
}

/// Fetched bytes of the current acquisition go to the image workers.
fn start_decode(images: &mut Images, key: &str, id: u64, bytes: Vec<u8>, mime: String) {
    let Some(active) = images.active.get_mut(key).filter(|active| active.id == id) else {
        return;
    };
    active.decode = Some(submit_decode(key.to_owned(), id, active.fit, bytes, mime));
}

/// The current acquisition of `key` failed to fetch.
fn fetch_failed(cx: &mut Cx, images: &mut Images, key: &str, id: u64, state: ImageState) {
    if !current(images, key, id) {
        return;
    }
    images.active.remove(key);
    settle_failure(cx, images, key, state);
}

/// The state of one assistant image occurrence at full quality (lightbox,
/// avatars), starting its acquisition on first sight. `occurrence` scopes file
/// reads to the rendered message. A `Loading` records the explicit owner
/// (including during dry measurement), or the current draw list for callers
/// without a retained row.
pub fn image(
    cx: &mut Cx2d,
    core: Option<&CoreHandle>,
    host_id: &str,
    workspace_root: Option<&str>,
    source: &str,
    occurrence: &str,
    owner: Option<DrawListId>,
) -> ImageState {
    acquire(
        cx,
        core,
        host_id,
        workspace_root,
        source,
        occurrence,
        owner,
        Fit::FULL,
    )
}

/// [`image`] for an inline Markdown image fit inside `width` logical pixels by
/// the 320px box: decodes at that size times the display scale, and again only
/// when a larger box needs more pixels (the sharper decode replaces the shown
/// one when it lands).
#[allow(clippy::too_many_arguments)]
pub fn fitted_image(
    cx: &mut Cx2d,
    core: Option<&CoreHandle>,
    host_id: &str,
    workspace_root: Option<&str>,
    source: &str,
    occurrence: &str,
    owner: Option<DrawListId>,
    width: f64,
) -> ImageState {
    let dpi = if cx.inside_pass() {
        cx.current_dpi_factor()
    } else {
        2.0
    };
    acquire(
        cx,
        core,
        host_id,
        workspace_root,
        source,
        occurrence,
        owner,
        Fit::inline(width, dpi),
    )
}

#[allow(clippy::too_many_arguments)]
fn acquire(
    cx: &mut Cx2d,
    core: Option<&CoreHandle>,
    host_id: &str,
    workspace_root: Option<&str>,
    source: &str,
    occurrence: &str,
    owner: Option<DrawListId>,
    fit: Fit,
) -> ImageState {
    let owner = owner.or_else(|| cx.get_current_draw_list_id());
    // source.ts: SVGs show their alt text where the renderer cannot draw
    // them (Android/iOS) and are never fetched over http(s).
    let svg = is_svg(source);
    if svg && super::paint::MOBILE {
        return ImageState::Failed;
    }
    let Some(resolved) = resolve_image_source(source, workspace_root) else {
        return ImageState::Failed;
    };
    let key = match &resolved {
        ImageSource::Direct(uri)
            if uri
                .get(..5)
                .is_some_and(|p| p.eq_ignore_ascii_case("http:"))
                || uri
                    .get(..6)
                    .is_some_and(|p| p.eq_ignore_ascii_case("https:")) =>
        {
            let generation =
                IMAGES.with(|images| images.borrow().generations.get(host_id).copied().flatten());
            http_cache_key(host_id, generation, uri)
        }
        ImageSource::Direct(uri) => format!("direct:{uri}"),
        ImageSource::File { cwd, path } => {
            format!("{host_id}\u{0}{occurrence}\u{0}{cwd}\u{0}{path}")
        }
    };
    IMAGES.with(|cell| {
        let images = &mut *cell.borrow_mut();
        let last_used = touch(images);
        if let Some(entry) = images.entries.get_mut(&key) {
            entry.last_used = last_used;
            let state = entry.state.clone();
            let sharper = match (&state, entry.fit) {
                (ImageState::Ready { .. }, Some(decoded)) if !decoded.covers(fit) => {
                    Some(decoded.union(fit))
                }
                _ => None,
            };
            if matches!(state, ImageState::Loading) {
                wait(owner, images, &key);
            }
            if let Some(sharper) = sharper
                && !images.active.contains_key(&key)
                && !images.queued.iter().any(|queued| queued.key == key)
                && let Ok(source) = acquisition(resolved, svg, core, host_id)
            {
                wait(owner, images, &key);
                images.queued.push_back(Queued {
                    key,
                    fit: sharper,
                    source,
                });
                pump(images);
            }
            return state;
        }
        let source = match acquisition(resolved, svg, core, host_id) {
            Ok(source) => source,
            Err(state) => return state,
        };
        // A new acquisition shows the placeholder its requester already drew.
        images.entries.insert(
            key.clone(),
            Entry {
                state: ImageState::Loading,
                bytes: 0,
                last_used,
                fit: None,
            },
        );
        evict(images, &key);
        wait(owner, images, &key);
        images.queued.push_back(Queued { key, fit, source });
        pump(images);
        ImageState::Loading
    })
}

/// Module hook: finished decodes and core-admitted HTTP/file reads. A settled
/// image repaints only the draw lists that showed it loading, so their pane measures
/// the visible rows again (a completed decode replaces the Markdown loading
/// placeholder with the raster's aspect-ratio height, including images in table
/// cells). Responses that only start a decode, and decodes of keys no longer
/// cached, repaint nothing.
pub fn handle_event(
    cx: &mut Cx,
    event: &Event,
    responses: &[(RequestId, Result<serde_json::Value, String>)],
) {
    IMAGES.with(|cell| {
        let images = &mut *cell.borrow_mut();
        for owner in images.retry.drain(..) {
            cx.redraw_list(owner);
        }
        for (request, result) in responses {
            if let Some(http) = images.http.remove(request) {
                let body = result
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(crate::common::host_http::image_body);
                match body {
                    Ok((bytes, mime)) => start_decode(images, &http.key, http.id, bytes, mime),
                    Err(error) => {
                        fetch_failed(cx, images, &http.key, http.id, ImageState::Error(error))
                    }
                }
                continue;
            }
            let Some((key, id)) = images.operations.remove(request) else {
                continue;
            };
            let bytes = result.as_ref().ok().and_then(|value| {
                let token = value.get("token")?.as_str()?;
                FILE_BYTES.lock().unwrap().remove(token)
            });
            match bytes {
                Some((bytes, mime)) => start_decode(images, &key, id, bytes, mime),
                None => fetch_failed(cx, images, &key, id, ImageState::Failed),
            }
        }
        if let Event::Signal = event {
            let finished: Vec<Decoded> = std::mem::take(&mut *DECODED.lock().unwrap());
            for done in finished {
                if !current(images, &done.key, done.id) {
                    continue;
                }
                images.active.remove(&done.key);
                match done.result {
                    Ok((buffer, full)) => {
                        let (width, height) = (buffer.width, buffer.height);
                        let fit = (!full && !done.fit.covers(Fit::FULL)).then_some(done.fit);
                        let state = ImageState::Ready {
                            texture: buffer.into_new_texture(cx),
                            width,
                            height,
                        };
                        settle(cx, images, &done.key, state, width * height * 4, fit);
                    }
                    Err(_) => settle_failure(cx, images, &done.key, ImageState::Failed),
                }
            }
        }
        pump(images);
    });
}

/// Whether a core response belongs to an image read.
pub fn owns(request: &str) -> bool {
    IMAGES.with(|cell| {
        let images = cell.borrow();
        images.operations.contains_key(request) || images.http.contains_key(request)
    })
}

#[cfg(test)]
mod http_tests {
    use super::*;

    #[test]
    fn host_http_cache_isolates_connection_generations_and_host_aliases() {
        let state = serde_json::json!({"hosts": [{
            "serverId": "daemon", "config": {"id": "active"},
            "connections": [{"id": "normal"}, {"id": "embedded"}],
            "httpGeneration": 42, "connectionGeneration": 99
        }]});
        let generations = host_generations(&state);
        for id in ["daemon", "active", "normal", "embedded"] {
            assert_eq!(generations.get(id), Some(&Some(42)));
        }
        let uri = "http://same-host/image.png";
        assert_ne!(
            http_cache_key("daemon", Some(41), uri),
            http_cache_key("daemon", Some(42), uri)
        );
        assert_ne!(
            http_cache_key("other", Some(42), uri),
            http_cache_key("daemon", Some(42), uri)
        );
    }

    #[test]
    fn decoded_images_evict_least_recently_used_over_byte_budget() {
        let mib = 1 << 20;
        let mut images = Images::default();
        for (key, last_used, bytes) in [
            ("loading", 0, 0),
            ("old", 1, 60 * mib),
            ("mid", 2, 30 * mib),
            ("new", 3, 30 * mib),
        ] {
            let state = if bytes == 0 {
                ImageState::Loading
            } else {
                ImageState::Failed
            };
            images.entries.insert(
                key.to_owned(),
                Entry {
                    state,
                    bytes,
                    last_used,
                    fit: None,
                },
            );
            images.bytes += bytes;
        }
        evict(&mut images, "new");
        let mut kept: Vec<&str> = images.entries.keys().map(String::as_str).collect();
        kept.sort_unstable();
        assert_eq!(kept, ["loading", "mid", "new"]);
        assert_eq!(images.bytes, 60 * mib);
    }
}
