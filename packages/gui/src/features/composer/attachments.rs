//! Ports of `composer/attachment_files.dart`, `forge_attachments.dart` and the descriptive
//! helpers of `attachment_pill.dart`.

use super::json::*;
use serde_json::{json, Map, Value};

/// attachments/upload-limit.ts FILE_UPLOAD_MAX_BYTES.
pub const FILE_UPLOAD_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// attachments/file-types.ts RASTER_IMAGE_MIME_TYPE_BY_EXTENSION.
const RASTER_IMAGE_MIME_TYPES: &[(&str, &str)] = &[
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".bmp", "image/bmp"),
    (".heic", "image/heic"),
    (".heif", "image/heif"),
    (".avif", "image/avif"),
    (".tif", "image/tiff"),
    (".tiff", "image/tiff"),
];

/// The extensions an image picker accepts (RASTER_IMAGE_FILE_EXTENSIONS).
pub fn raster_image_extensions() -> Vec<String> {
    RASTER_IMAGE_MIME_TYPES.iter().map(|(ext, _)| ext[1..].to_owned()).collect()
}

/// file-types.ts getFileExtension: lower-cased extension with its dot, without query/fragment.
pub fn file_extension(path: &str) -> String {
    let normalized = path.split('#').next().unwrap_or("");
    let normalized = normalized.split('?').next().unwrap_or("");
    match normalized.rfind('.') {
        Some(index) => normalized[index..].to_lowercase(),
        None => String::new(),
    }
}

/// file-types.ts getRasterImageMimeTypeFromPath.
pub fn raster_image_mime_type(path: &str) -> Option<&'static str> {
    let extension = file_extension(path);
    RASTER_IMAGE_MIME_TYPES
        .iter()
        .find(|(ext, _)| *ext == extension)
        .map(|(_, mime)| *mime)
}

/// file-types.ts getMimeTypeFromPath.
pub fn mime_type_for_path(path: &str) -> &'static str {
    raster_image_mime_type(path).unwrap_or("application/octet-stream")
}

/// actions.ts uploadFileAttachments: a file over the upload limit.
#[derive(Clone, Debug)]
pub struct FileTooLarge {
    pub file_name: String,
}

/// Every selected file is size-checked before any upload starts.
pub fn check_upload_sizes<'a>(files: impl IntoIterator<Item = (&'a str, u64)>) -> Result<(), FileTooLarge> {
    for (name, size) in files {
        if size > FILE_UPLOAD_MAX_BYTES {
            return Err(FileTooLarge { file_name: name.to_owned() });
        }
    }
    Ok(())
}

/// Hosts without forge search take the legacy GitHub attachment kinds.
pub fn uses_legacy_forge(server_info: &Value) -> bool {
    get(get(server_info, "features"), "forgeSearch") != &Value::Bool(true)
}

/// The wire attachment for a forge search item.
pub fn forge_attachment(item: &Value, legacy: bool) -> Value {
    let kind = s(get(item, "kind"));
    let change = kind == "change_request" || kind == "pr";
    let mut out = Map::new();
    let kind_name = match (legacy, change) {
        (true, true) => "github_pr",
        (true, false) => "github_issue",
        (false, true) => "forge_change_request",
        (false, false) => "forge_issue",
    };
    let mime = match (legacy, change) {
        (true, true) => "application/github-pr",
        (true, false) => "application/github-issue",
        (false, true) => "application/paseo-forge-change-request",
        (false, false) => "application/paseo-forge-issue",
    };
    out.insert("type".into(), json!(kind_name));
    out.insert("mimeType".into(), json!(mime));
    if !legacy {
        let forge = item.get("forge").filter(|v| !v.is_null()).cloned().unwrap_or(json!("github"));
        out.insert("forge".into(), forge);
    }
    for key in ["number", "title", "url"] {
        out.insert(key.into(), get(item, key).clone());
    }
    let mut extra = vec!["body"];
    if !legacy {
        extra.push("projectPath");
    }
    if change {
        extra.extend(["baseRefName", "headRefName"]);
    }
    for key in extra {
        if let Some(value) = item.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// forge_picker.dart forgeItemKey: `change_request:N` / `issue:N` of an item or attachment.
pub fn forge_item_key(item: &Value) -> String {
    let kind = item.get("kind").filter(|v| !v.is_null()).unwrap_or(get(item, "type"));
    let change = matches!(s(kind), "change_request" | "pr" | "github_pr" | "forge_change_request");
    format!("{}:{}", if change { "change_request" } else { "issue" }, display(get(item, "number")))
}

/// git/forge.ts getForgePresentation: GitLab says "MR" with "!", others "PR" with "#".
pub fn forge_change_request(forge: &Value) -> (&'static str, &'static str) {
    if forge.as_str() == Some("gitlab") { ("MR", "!") } else { ("PR", "#") }
}

/// Accessibility kind and number of a forge attachment, None for other attachments.
pub fn forge_labels(attachment: &Value) -> Option<(String, String)> {
    let kind = attachment.get("kind").filter(|v| !v.is_null()).unwrap_or(get(attachment, "type"));
    let kind = s(kind);
    let change = kind == "github_pr" || kind == "forge_change_request";
    if !change && kind != "github_issue" && kind != "forge_issue" {
        return None;
    }
    let (abbrev, prefix) = forge_change_request(get(attachment, "forge"));
    let number = display(get(attachment, "number"));
    Some(if change {
        (abbrev.to_owned(), format!("{prefix}{number}"))
    } else {
        ("issue".to_owned(), format!("#{number}"))
    })
}

/// Which glyph an attachment pill shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentGlyph {
    FileText,
    CircleDot,
    GitPullRequest,
}

/// attachment_pill.dart describe: title, type subtitle (None: localized "File"), glyph.
pub fn describe(attachment: &Value) -> (String, Option<String>, AttachmentGlyph) {
    let path = s(get(attachment, "path"));
    let kind = attachment.get("kind").filter(|v| !v.is_null()).unwrap_or(get(attachment, "type"));
    match s(kind) {
        "workspace_file" => {
            let selection = get(attachment, "selection");
            let subtitle = if s(get(selection, "kind")) == "line_range" {
                format!("{path} · {}-{}", display(get(selection, "startLine")), display(get(selection, "endLine")))
            } else {
                path.to_owned()
            };
            (remote_basename(path), Some(subtitle), AttachmentGlyph::FileText)
        }
        "github_issue" | "forge_issue" => {
            let number = display(get(attachment, "number"));
            (
                s_or(get(attachment, "title"), &format!("#{number}")).to_owned(),
                Some(format!("Issue #{number}")),
                AttachmentGlyph::CircleDot,
            )
        }
        "github_pr" | "forge_change_request" => {
            let (abbrev, prefix) = forge_change_request(get(attachment, "forge"));
            let number = display(get(attachment, "number"));
            (
                s_or(get(attachment, "title"), &format!("{prefix}{number}")).to_owned(),
                Some(format!("{abbrev} {prefix}{number}")),
                AttachmentGlyph::GitPullRequest,
            )
        }
        _ => {
            let fallback = if path.is_empty() {
                "Attachment".to_owned()
            } else {
                path.rsplit('/').next().unwrap_or(path).to_owned()
            };
            let name = s_or(get(attachment, "fileName"), s_or(get(attachment, "title"), &fallback)).to_owned();
            let label = file_type_label(&name);
            (name, label, AttachmentGlyph::FileText)
        }
    }
}

/// attachments/file-types.ts getFileTypeLabel: the upper-cased extension.
pub fn file_type_label(path: &str) -> Option<String> {
    let normalized = path.split('#').next().unwrap_or("");
    let normalized = normalized.split('?').next().unwrap_or("");
    let dot = normalized.rfind('.')?;
    if dot == normalized.len() - 1 {
        return None;
    }
    Some(normalized[dot + 1..].to_uppercase())
}

/// files/remote_path.dart remoteBasename: last path segment of a POSIX or Windows path.
pub fn remote_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return path.to_owned();
    }
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed)
        .to_owned()
}

/// chat_drafts.dart wireAttachment: workspace files are sent as text references.
pub fn wire_attachment(attachment: &Value) -> Value {
    if s(get(attachment, "kind")) != "workspace_file" {
        return attachment.clone();
    }
    let path = s(get(attachment, "path"));
    let selection = get(attachment, "selection");
    let lines = if s(get(selection, "kind")) == "line_range" {
        format!("\nLines: {}-{}", display(get(selection, "startLine")), display(get(selection, "endLine")))
    } else {
        String::new()
    };
    json!({
        "type": "text",
        "mimeType": "text/plain",
        "title": remote_basename(path),
        "text": format!("Workspace file: {path}{lines}"),
    })
}

/// Sniff an image's mime type from its leading bytes (image_clipboard.dart `_imageMime`).
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[..8] == [0x89, 0x50, 0x4e, 0x47, 13, 10, 26, 10] {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes[..3] == [0xff, 0xd8, 0xff] {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && bytes[..4] == [71, 73, 70, 56] {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes[..4] == [82, 73, 70, 70] && bytes[8..12] == [87, 69, 66, 80] {
        return Some("image/webp");
    }
    None
}
