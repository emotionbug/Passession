//! Images an `ask` form names by file. OMP lets a form's `<img src>` be a path relative to the
//! agent's working directory; the client may run on another machine, so the card carries
//! those images inline as `data:` URIs.

use base64::Engine;
use percent_encoding::percent_decode_str;
use std::path::{Path, PathBuf};

/// The largest image inlined, and the most all of one form's images may add (base64).
const MAX_IMAGE: u64 = 4 << 20;
const MAX_TOTAL: usize = 16 << 20;

/// `html` with each `<img src>` that names an image file (relative to `cwd`, absolute, or a
/// `file:` URL) replaced by a `data:` URI. Other URLs, missing or unreadable files, files
/// that are not images by extension, and images past the size limits stay as written.
pub(super) fn inline(html: &str, cwd: &Path) -> String {
    let pattern =
        cached_regex!(r#"(?is)<img\b[^>]*?\bsrc\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#);
    let mut out = String::new();
    let mut last = 0;
    let mut total = 0;
    for captures in pattern.captures_iter(html) {
        let Some(value) = captures.get(1).or(captures.get(2)).or(captures.get(3)) else {
            continue;
        };
        let Some(uri) = data_uri(&unescape(value.as_str()), cwd, &mut total) else {
            continue;
        };
        out.push_str(&html[last..value.start()]);
        out.push_str(&uri);
        last = value.end();
    }
    if last == 0 {
        return html.to_owned();
    }
    out.push_str(&html[last..]);
    out
}

fn data_uri(src: &str, cwd: &Path, total: &mut usize) -> Option<String> {
    let (path, mime) = candidates(src)?.into_iter().find_map(|path| {
        let path = if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        };
        let mime = mime(&path)?;
        std::fs::metadata(&path)
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_IMAGE)
            .map(|_| (path, mime))
    })?;
    let bytes = std::fs::read(path)
        .ok()
        .filter(|bytes| bytes.len() as u64 <= MAX_IMAGE)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    if *total + encoded.len() > MAX_TOTAL {
        return None;
    }
    *total += encoded.len();
    Some(format!("data:{mime};base64,{encoded}"))
}

/// The files `src` may name: the path as written and, if it differs, percent-decoded (an
/// HTML relative URL). `None` for a URL with another scheme, a fragment or a network path.
fn candidates(src: &str) -> Option<Vec<PathBuf>> {
    let src = src.trim();
    if src.is_empty() || src.starts_with('#') || src.starts_with("//") {
        return None;
    }
    if let Some(path) = src.strip_prefix("file://") {
        return Some(vec![PathBuf::from(
            percent_decode_str(path).decode_utf8().ok()?.as_ref(),
        )]);
    }
    // A URL scheme (`https:`, `data:`, `blob:`…); one letter is a Windows drive.
    if let Some(colon) = src.find(':') {
        let scheme = &src[..colon];
        if scheme.len() > 1
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return None;
        }
    }
    let mut paths = vec![PathBuf::from(src)];
    let path = src.split(['?', '#']).next().unwrap_or(src);
    if let Ok(decoded) = percent_decode_str(path).decode_utf8()
        && decoded != src
    {
        paths.push(PathBuf::from(decoded.as_ref()));
    }
    Some(paths)
}

fn mime(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

/// The character references an attribute value commonly carries.
fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_owned();
    }
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_image_files_become_data_uris() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chart.png"), b"png-bytes").unwrap();
        std::fs::create_dir(dir.path().join("my dir")).unwrap();
        std::fs::write(dir.path().join("my dir/pic.svg"), b"<svg/>").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"secret").unwrap();
        let absolute = dir.path().join("chart.png");
        let html = format!(
            r#"<p><img src="chart.png" alt=a><IMG alt='b' SRC='my%20dir/pic.svg'><img src={abs}>
            <img src="file://{abs}"><img src="https://x.test/a.png"><img src="data:image/png;base64,AA">
            <img src="missing.png"><img src="notes.txt"></p>"#,
            abs = absolute.display()
        );
        let png = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(b"png-bytes")
        );
        let svg = format!(
            "data:image/svg+xml;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(b"<svg/>")
        );
        let out = inline(&html, dir.path());
        assert_eq!(
            out.matches(&png).count(),
            3,
            "relative, absolute and file: URL"
        );
        assert!(
            out.contains(&format!("SRC='{svg}'")),
            "percent-decoded relative URL"
        );
        for kept in [
            r#"src="https://x.test/a.png""#,
            r#"src="data:image/png;base64,AA""#,
            r#"src="missing.png""#,
            r#"src="notes.txt""#,
        ] {
            assert!(out.contains(kept), "{kept} stays as written");
        }
        assert!(
            !out.contains("c2VjcmV0"),
            "a file that is not an image is never read"
        );
    }

    #[test]
    fn oversized_images_stay_as_written() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::File::create(dir.path().join("big.png")).unwrap();
        file.set_len(MAX_IMAGE + 1).unwrap();
        let html = r#"<img src="big.png">"#;
        assert_eq!(inline(html, dir.path()), html);
    }
}
