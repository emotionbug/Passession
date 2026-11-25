use super::{Body, HeaderValue, Method, Response, StatusCode, plain};
use http::HeaderMap;
use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::fs;
use tokio_util::io::ReaderStream;

pub(super) fn reserved_path(path: &str) -> bool {
    matches!(path.split('/').nth(1), Some("api" | "mcp" | "ws"))
}

fn asset_path(path: &str) -> Option<PathBuf> {
    if path == "/" {
        return Some("index.html".into());
    }
    let mut relative = PathBuf::new();
    for segment in path.strip_prefix('/')?.split('/') {
        let segment = percent_decode_str(segment).decode_utf8().ok()?;
        if segment.is_empty()
            || matches!(segment.as_ref(), "." | "..")
            || segment.contains(['/', '\\', ':', '\0'])
        {
            return None;
        }
        relative.push(segment.as_ref());
    }
    Some(relative)
}

pub(super) async fn serve(
    root: Option<&Path>,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Response<Body> {
    let mut response = match open_asset(root, path, method, headers).await {
        Some(response) => response,
        None if matches!(path, "/" | "/index.html") => plain(
            StatusCode::NOT_FOUND,
            "Web UI bundle not found. Install a daemon runtime with web assets or set PASEO_WEB_DIR to a bundle built with cargo xtask build-web.",
        ),
        None => plain(StatusCode::NOT_FOUND, "404 Not Found"),
    };
    // Bundle filenames are stable across releases: every load revalidates (the ETag
    // answers 304 when unchanged), so stale JS or Wasm never runs with a newer index.
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// `file` when it is a regular file inside `root` (symlinks resolved).
async fn contained_file(root: &Path, file: &Path) -> Option<(PathBuf, std::fs::Metadata)> {
    let file = fs::canonicalize(file).await.ok()?;
    if !file.starts_with(root) {
        return None;
    }
    let metadata = fs::metadata(&file).await.ok()?;
    metadata.is_file().then_some((file, metadata))
}

/// A weak validator from the file's size and modification time.
fn entity_tag(metadata: &std::fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("W/\"{:x}-{modified:x}\"", metadata.len())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
}

/// Whether `Accept-Encoding` admits gzip (a `q=0` refuses it).
fn accepts_gzip(headers: &HeaderMap) -> bool {
    header(headers, "accept-encoding").split(',').any(|coding| {
        let mut parts = coding.split(';').map(str::trim);
        parts
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("gzip"))
            && !parts.any(|parameter| {
                parameter
                    .strip_prefix("q=")
                    .and_then(|q| q.parse::<f32>().ok())
                    .is_some_and(|q| q == 0.0)
            })
    })
}

async fn open_asset(
    root: Option<&Path>,
    path: &str,
    method: &Method,
    headers: &HeaderMap,
) -> Option<Response<Body>> {
    let relative = asset_path(path)?;
    let root = fs::canonicalize(root?).await.ok()?;
    let (file, metadata) = contained_file(&root, &root.join(&relative)).await?;
    let mime = match relative
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("woff") => "font/woff",
        _ => mime_guess::from_path(&relative)
            .first_raw()
            .unwrap_or("application/octet-stream"),
    };
    let tag = entity_tag(&metadata);
    let unchanged = header(headers, "if-none-match")
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == tag || candidate == "*");
    let response = Response::builder()
        .header("content-type", mime)
        .header("etag", &tag)
        .header("vary", "accept-encoding");
    if unchanged {
        return response
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .ok();
    }
    // `cargo xtask build-web` writes `<file>.gz` next to large assets.
    let mut compressed = None;
    if accepts_gzip(headers) {
        let mut sibling = file.clone().into_os_string();
        sibling.push(".gz");
        compressed = contained_file(&root, Path::new(&sibling)).await;
    }
    let (served, length, response) = match compressed {
        Some((gzip, gzip_metadata)) => (
            gzip,
            gzip_metadata.len(),
            response.header("content-encoding", "gzip"),
        ),
        None => (file, metadata.len(), response),
    };
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from_stream(ReaderStream::new(fs::File::open(&served).await.ok()?))
    };
    response.header("content-length", length).body(body).ok()
}
