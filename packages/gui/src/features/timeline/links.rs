//! Port of `assistant_file_links.dart` (assistant-file-links parse.ts,
//! resolver.ts, use-file-link.ts, path-open.ts): which assistant-message text,
//! inline code and markdown links name workspace files, and how a press opens them.

use regex::Regex;
use std::sync::LazyLock;

/// assistant-file-links/extensions.ts.
pub const ASSISTANT_FILE_EXTENSIONS: &[&str] = &[
    "astro", "bash", "c", "cc", "cjs", "cpp", "cs", "css", "cts", "cxx", "env", "fish", "go", "gql", "gradle", "graphql", "h",
    "hpp", "htm", "html", "ini", "java", "js", "json", "jsonc", "jsx", "kt", "kts", "less", "lock", "lua", "md", "mdx", "mjs",
    "mts", "php", "proto", "py", "rb", "rs", "sass", "scss", "sh", "sql", "svelte", "swift", "toml", "ts", "tsx", "txt", "vue",
    "xml", "yaml", "yml", "zsh",
];

/// parse.ts InlinePathTarget.
#[derive(Clone, Debug, PartialEq)]
pub struct InlinePathTarget {
    pub raw: String,
    pub path: String,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
}

impl InlinePathTarget {
    pub fn with_path(&self, path: String) -> Self {
        Self { path, ..self.clone() }
    }
}

/// Where a pressed link came from (markdown/source.ts markdownLinkSource).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileLinkSourceType {
    Markdown,
    Linkify,
    InlineCode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AssistantFileLinkSource {
    pub href: String,
    pub text: Option<String>,
    pub kind: FileLinkSourceType,
}

type Lines = (Option<i64>, Option<i64>);

static INLINE_LINE_FRAGMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^L([0-9]+)(?:C[0-9]+)?(?:-L?([0-9]+)(?:C[0-9]+)?)?$").unwrap());
static COLON_LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?):([0-9]+)(?::[0-9]+)?(?:-([0-9]+)(?::[0-9]+)?)?$").unwrap());
static PAREN_LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(.+?)\(([0-9]+)(?:,[0-9]+)?(?:-([0-9]+)(?:,[0-9]+)?)?\)$").unwrap());
static WORD_LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(.+?)\s+lines?\s+([0-9]+)(?:-([0-9]+))?$").unwrap());
static DRIVE_ABSOLUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z]:[\\/]").unwrap());
static WINDOWS_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z]:[\\/][^?#]*)(#[^?]+)?$").unwrap());
static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:").unwrap());
static DOMAIN_SEGMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)+$").unwrap());

/// utils/path.ts isAbsolutePath.
pub fn is_absolute_file_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with("\\\\") || DRIVE_ABSOLUTE.is_match(value)
}

fn safe_decode(value: &str) -> String {
    percent_decode(value).unwrap_or_else(|| value.to_owned())
}

/// `Uri.decodeComponent`: fails (None) on malformed escapes or invalid UTF-8.
pub fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn strip_quotes(value: &str) -> &str {
    let mut s = value;
    if s.starts_with(['\'', '"', '`']) {
        s = &s[1..];
    }
    if s.ends_with(['\'', '"', '`']) {
        s = &s[..s.len() - 1];
    }
    s
}

fn normalize_path_token(value: &str) -> Option<String> {
    let trimmed = strip_quotes(value.trim());
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.replace('\\', "/"))
}

fn parse_line_fragment(value: &str) -> Option<Lines> {
    let fragment = value.strip_prefix('#').unwrap_or(value);
    if fragment.is_empty() {
        return Some((None, None));
    }
    let captures = INLINE_LINE_FRAGMENT.captures(fragment);
    let start = captures.as_ref().and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<i64>().ok());
    let end = captures.as_ref().and_then(|c| c.get(2)).and_then(|m| m.as_str().parse::<i64>().ok());
    if start.is_some_and(|s| s <= 0) || end.is_some_and(|e| e <= 0) || matches!((start, end), (Some(s), Some(e)) if e < s) {
        return None;
    }
    Some((start, end))
}

/// parse.ts parseInlinePathToken: `file:line`, `file:line:col`,
/// `file:start-end`, `file(line)` and `file line N` markers only.
pub fn parse_inline_path_token(value: &str) -> Option<InlinePathTarget> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let captures = COLON_LINE_SUFFIX
        .captures(trimmed)
        .or_else(|| PAREN_LINE_SUFFIX.captures(trimmed))
        .or_else(|| WORD_LINE_SUFFIX.captures(trimmed))?;
    let base = captures.get(1)?.as_str().trim();
    if base.is_empty() || base.contains("://") {
        return None;
    }
    let path = normalize_path_token(base)?;
    let line_start: i64 = captures.get(2)?.as_str().parse().ok()?;
    if line_start <= 0 {
        return None;
    }
    let line_end = match captures.get(3) {
        Some(m) => {
            let end: i64 = m.as_str().parse().ok()?;
            if end <= 0 || end < line_start {
                return None;
            }
            Some(end)
        }
        None => None,
    };
    Some(InlinePathTarget { raw: value.to_owned(), path, line_start: Some(line_start), line_end })
}

fn normalize_file_url_path(pathname: &str) -> Option<String> {
    if pathname.is_empty() {
        return None;
    }
    let decoded = safe_decode(pathname).replace('\\', "/");
    if decoded.is_empty() {
        return None;
    }
    let bytes = decoded.as_bytes();
    if bytes.len() >= 4 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' && bytes[3] == b'/' {
        return Some(decoded[1..].to_owned());
    }
    Some(decoded)
}

fn parse_file_protocol_url(value: &str) -> Option<InlinePathTarget> {
    let trimmed = value.trim();
    let rest = trimmed.strip_prefix("file:").or_else(|| trimmed.strip_prefix("FILE:"))?;
    // file://host/path or file:/path; the authority is ignored like Uri.path.
    let (path_and_rest, fragment) = match rest.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (rest, None),
    };
    let path_and_rest = path_and_rest.split_once('?').map(|(p, _)| p).unwrap_or(path_and_rest);
    let path = if let Some(after) = path_and_rest.strip_prefix("//") {
        match after.find('/') {
            Some(slash) => &after[slash..],
            None => "",
        }
    } else {
        path_and_rest
    };
    let path = normalize_file_url_path(path)?;
    let lines = parse_line_fragment(&fragment.map(|f| format!("#{f}")).unwrap_or_default())?;
    Some(InlinePathTarget { raw: value.to_owned(), path, line_start: lines.0, line_end: lines.1 })
}

fn parse_assistant_inline_path_link(value: &str, decode_href: bool) -> Option<InlinePathTarget> {
    let target = parse_inline_path_token(value)?;
    let decoded = if decode_href { safe_decode(&target.path) } else { target.path.clone() };
    let path = normalize_path_token(&decoded)?;
    if !is_absolute_file_path(&path) {
        return None;
    }
    Some(target.with_path(path))
}

#[derive(Clone, Debug, PartialEq)]
pub enum FileLinkClassification {
    External(String),
    Direct(InlinePathTarget),
    Ambiguous(InlinePathTarget),
}

/// parse.ts classifyAssistantFileLink.
pub fn classify_assistant_file_link(value: &str, workspace_root: Option<&str>, decode_href: bool, explicit_link: bool) -> Option<FileLinkClassification> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if is_external_href(trimmed) {
        return Some(FileLinkClassification::External(value.to_owned()));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return None;
    }
    let target = parse_assistant_file_link(trimmed, workspace_root, decode_href, explicit_link)?;
    if !explicit_link && is_ambiguous_workspace_candidate(trimmed, &target, workspace_root, decode_href) {
        return Some(FileLinkClassification::Ambiguous(target));
    }
    Some(FileLinkClassification::Direct(target))
}

/// Dart `Uri.parse('http://paseo.invalid').resolve(href)` for absolute paths:
/// dot segments removed, fragment split.
fn resolve_href_path(href: &str) -> (String, String) {
    let (before_hash, hash) = match href.find('#') {
        Some(i) => (&href[..i], href[i..].to_owned()),
        None => (href, String::new()),
    };
    let path_part = before_hash.split('?').next().unwrap_or("");
    let mut segments: Vec<&str> = Vec::new();
    let trailing = path_part.ends_with('/') || path_part.ends_with("/.") || path_part.ends_with("/..");
    for segment in path_part.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    let mut path = format!("/{}", segments.join("/"));
    if trailing && path != "/" {
        path.push('/');
    }
    (path, hash)
}

/// parse.ts parseAssistantFileLink.
pub fn parse_assistant_file_link(value: &str, workspace_root: Option<&str>, decode_href: bool, explicit_link: bool) -> Option<InlinePathTarget> {
    if let Some(file_url) = parse_file_protocol_url(value) {
        return Some(file_url);
    }
    let trimmed = value.trim();
    if trimmed.is_empty() || is_external_href(trimmed) {
        return None;
    }
    if let Some(inline) = parse_assistant_inline_path_link(trimmed, decode_href) {
        return Some(inline);
    }
    if let Some(windows) = parse_windows_file_link(trimmed, value, decode_href) {
        return Some(windows);
    }
    if let Some(relative) = parse_workspace_relative_file_link(trimmed, workspace_root, decode_href, explicit_link) {
        return Some(relative);
    }
    if !is_absolute_file_path(trimmed) {
        return None;
    }
    let (path_part, hash_part) = if decode_href {
        if trimmed.starts_with("\\\\") {
            return None;
        }
        let (path, hash) = resolve_href_path(trimmed);
        (safe_decode(&path), hash)
    } else {
        match trimmed.find('#') {
            Some(i) => (trimmed[..i].to_owned(), trimmed[i..].to_owned()),
            None => (trimmed.to_owned(), String::new()),
        }
    };
    let path = normalize_path_token(&path_part)?;
    if !is_absolute_file_path(&path) {
        return None;
    }
    let lines = parse_line_fragment(&hash_part)?;
    Some(InlinePathTarget { raw: value.to_owned(), path, line_start: lines.0, line_end: lines.1 })
}

/// parse.ts isFileLookingAssistantToken.
pub fn is_file_looking_assistant_token(value: &str) -> bool {
    let Some(normalized) = normalize_path_token(value) else { return false };
    if normalized.chars().any(char::is_whitespace) || normalized.contains('?') || normalized.contains("://") {
        return false;
    }
    heuristic_local_path(&normalized).is_some_and(|p| is_plausible_local_path(&p))
}

fn parse_windows_file_link(trimmed: &str, raw: &str, decode_href: bool) -> Option<InlinePathTarget> {
    let captures = WINDOWS_LINK.captures(trimmed)?;
    let path_part = captures.get(1).map(|m| m.as_str()).unwrap_or("");
    let path = normalize_path_token(&if decode_href { safe_decode(path_part) } else { path_part.to_owned() })?;
    let lines = parse_line_fragment(captures.get(2).map(|m| m.as_str()).unwrap_or(""))?;
    Some(InlinePathTarget { raw: raw.to_owned(), path, line_start: lines.0, line_end: lines.1 })
}

fn parse_workspace_relative_file_link(value: &str, workspace_root: Option<&str>, decode_href: bool, explicit_link: bool) -> Option<InlinePathTarget> {
    let (path, lines) = parse_local_path_parts(value, decode_href, explicit_link)?;
    if is_absolute_file_path(&path) {
        return None;
    }
    let at = |path: String| InlinePathTarget { raw: value.to_owned(), path, line_start: lines.0, line_end: lines.1 };
    if is_home_relative_path(&path) {
        return Some(at(path));
    }
    let root = normalize_path_input(workspace_root)?;
    resolve_relative_path_under_root(&path, &root).map(at)
}

fn parse_local_path_parts(value: &str, decode_href: bool, explicit_link: bool) -> Option<(String, Lines)> {
    let normalized = normalize_path_token(value)?;
    if normalized.contains('?') {
        return None;
    }
    let (before_hash, hash) = match normalized.find('#') {
        Some(i) => (normalized[..i].to_owned(), normalized[i..].to_owned()),
        None => (normalized.clone(), String::new()),
    };
    let fragment_lines = parse_line_fragment(&hash)?;
    if let Some(inline) = parse_inline_path_token(&before_hash) {
        let path = if decode_href { safe_decode(&inline.path) } else { inline.path.clone() };
        if !explicit_link && !is_plausible_local_path(&path) {
            return None;
        }
        return Some((path, (inline.line_start, inline.line_end)));
    }
    if before_hash.is_empty() || before_hash.contains(':') {
        return None;
    }
    let decoded = if decode_href { safe_decode(&before_hash) } else { before_hash.clone() };
    let plausible = parse_inline_path_token(&decoded).map(|t| t.path).unwrap_or_else(|| decoded.clone());
    if !explicit_link && !is_plausible_local_path(&plausible) {
        return None;
    }
    Some((decoded, fragment_lines))
}

/// parse.ts normalizeInlinePathTarget: a workspace-relative file (or the
/// directory a trailing slash names).
pub fn normalize_inline_path_target(raw_path: &str, cwd: Option<&str>) -> Option<(String, Option<String>)> {
    let input = normalize_path_input(Some(raw_path))?;
    let mut normalized = resolve_path_against_cwd(&input, cwd).unwrap_or(input);
    if let Some(rest) = normalized.strip_prefix("./") {
        normalized = if rest.is_empty() { ".".into() } else { rest.to_owned() };
    }
    if normalized.is_empty() {
        normalized = ".".into();
    }
    if normalized == "." {
        return Some((".".into(), None));
    }
    if normalized.ends_with('/') {
        let dir = normalized.trim_end_matches('/');
        return Some((if dir.is_empty() { ".".into() } else { dir.to_owned() }, None));
    }
    let directory = match normalized.rfind('/') {
        Some(slash) => normalized[..slash].to_owned(),
        None => ".".into(),
    };
    Some((if directory.is_empty() { ".".into() } else { directory }, Some(normalized)))
}

fn is_allowed_absolute_path(path: &str, workspace_root: Option<&str>) -> bool {
    let Some(root) = normalize_path_input(workspace_root) else { return true };
    let compare_path = normalize_path_for_compare(path);
    let trimmed_root = root.trim_end_matches('/');
    let compare_root = normalize_path_for_compare(if trimmed_root.is_empty() { "/" } else { trimmed_root });
    let prefix = if compare_root == "/" { "/".to_owned() } else { format!("{compare_root}/") };
    compare_path == compare_root || compare_path.starts_with(&prefix)
}

fn is_home_relative_path(path: &str) -> bool {
    path == "~" || path.starts_with("~/") || path.starts_with("~\\")
}

fn is_external_href(value: &str) -> bool {
    if value.contains("://") {
        return !value.to_lowercase().starts_with("file://");
    }
    if parse_inline_path_token(value).is_some() {
        return false;
    }
    SCHEME.is_match(value) && !DRIVE_ABSOLUTE.is_match(value)
}

fn is_ambiguous_workspace_candidate(value: &str, target: &InlinePathTarget, workspace_root: Option<&str>, decode_href: bool) -> bool {
    let Some(root) = normalize_path_input(workspace_root) else { return false };
    if !is_allowed_absolute_path(&target.path, Some(&root)) {
        return false;
    }
    let Some((path, _)) = parse_local_path_parts(value, decode_href, false) else { return false };
    if is_absolute_file_path(&path) {
        return false;
    }
    !path.contains('/')
}

fn heuristic_local_path(value: &str) -> Option<String> {
    let (before_hash, hash) = match value.find('#') {
        Some(i) => (&value[..i], &value[i..]),
        None => (value, ""),
    };
    parse_line_fragment(hash)?;
    if let Some(inline) = parse_inline_path_token(before_hash) {
        return Some(inline.path);
    }
    if before_hash.is_empty() || before_hash.contains(':') {
        return None;
    }
    Some(before_hash.to_owned())
}

fn is_plausible_local_path(value: &str) -> bool {
    let Some(normalized) = normalize_path_token(value) else { return false };
    if is_absolute_file_path(&normalized) {
        return true;
    }
    if normalized.starts_with("./") || normalized.starts_with("../") || normalized.starts_with("~/") {
        return true;
    }
    let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    match segments.len() {
        0 => false,
        1 => is_plausible_file_name(segments[0]),
        _ => !DOMAIN_SEGMENT.is_match(segments[0]) && is_plausible_file_name(segments[segments.len() - 1]),
    }
}

fn is_plausible_file_name(file_name: &str) -> bool {
    if file_name.starts_with('.') && file_name.len() > 1 {
        return true;
    }
    let Some(dot) = file_name.rfind('.') else { return false };
    ASSISTANT_FILE_EXTENSIONS.contains(&file_name[dot + 1..].to_lowercase().as_str())
}

fn resolve_relative_path_under_root(value: &str, workspace_root: &str) -> Option<String> {
    let path = normalize_path_token(value)?;
    if is_absolute_file_path(&path) {
        return None;
    }
    let trimmed_root = workspace_root.trim_end_matches('/');
    let root = if trimmed_root.is_empty() { "/" } else { trimmed_root };
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            s => segments.push(s),
        }
    }
    if segments.is_empty() {
        return Some(root.to_owned());
    }
    Some(if root == "/" { format!("/{}", segments.join("/")) } else { format!("{root}/{}", segments.join("/")) })
}

fn normalize_path_input(value: Option<&str>) -> Option<String> {
    let trimmed = strip_quotes(value?.trim());
    if trimmed.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut last_slash = false;
    for c in trimmed.chars() {
        let c = if c == '\\' { '/' } else { c };
        if c == '/' {
            if !last_slash {
                out.push(c);
            }
            last_slash = true;
        } else {
            out.push(c);
            last_slash = false;
        }
    }
    Some(out)
}

fn resolve_path_against_cwd(path: &str, cwd: Option<&str>) -> Option<String> {
    let normalized_cwd = normalize_path_input(cwd)?;
    if !is_absolute_file_path(path) || !is_absolute_file_path(&normalized_cwd) {
        return None;
    }
    let trimmed = normalized_cwd.trim_end_matches('/');
    let base = if trimmed.is_empty() { "/" } else { trimmed };
    let compare_path = normalize_path_for_compare(path);
    let prefix = if base == "/" { "/".to_owned() } else { format!("{base}/") };
    if compare_path == normalize_path_for_compare(base) {
        return Some(".".into());
    }
    if compare_path.starts_with(&normalize_path_for_compare(&prefix)) {
        let rest = &path[prefix.len()..];
        return Some(if rest.is_empty() { ".".into() } else { rest.to_owned() });
    }
    None
}

fn normalize_path_for_compare(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

// ---- Linkification (components/markdown/source.ts) ----

/// source.ts FILE_PATTERN: http(s) URLs and file paths (with an optional
/// `:line[:col][-line[:col]]` suffix) that stand alone in prose.
pub static ASSISTANT_LINKIFY_PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
    let path_character = r"[a-zA-Z0-9\x{80}-\x{FFFF}_@.%+-]";
    let path_letter = r"[a-zA-Z]";
    let path_segment = format!(r"{path_character}*[a-zA-Z0-9\x{{80}}-\x{{FFFF}}_@%+-]");
    let line_suffix = r"(?::\d+(?::\d+)?(?:-\d+(?::\d+)?)?)?";
    let extensions = ASSISTANT_FILE_EXTENSIONS
        .iter()
        .map(|e| e.chars().map(|c| if c.is_ascii_lowercase() { format!("[{c}{}]", c.to_ascii_uppercase()) } else { c.to_string() }).collect::<String>())
        .collect::<Vec<_>>()
        .join("|");
    let path_prefix = r"(?:[A-Za-z]:[\\/]|\.{0,2}/|~/)";
    let has_path_letter = format!(r"(?={path_character}*(?:[\\/]{path_character}*)*{path_letter})");
    let separated_path = format!(r"{path_segment}(?:[\\/]{path_segment})+");
    let file_name = format!(r"{path_segment}\.(?:{extensions})");
    let prose_path = format!(
        r"(?:{path_prefix}{has_path_letter}{separated_path}|{has_path_letter}(?:{path_segment}(?:[\\/]{path_segment})*[\\/]{file_name}|{path_segment}(?:[\\/]{path_segment}){{2,}}|{separated_path}(?=:)))"
    );
    let pattern = format!(
        r"(?:https?://[^\s<>]+|(?<![a-zA-Z0-9\x{{80}}-\x{{FFFF}}_@.%+~:/\\-])(?:{prose_path}|{file_name}){line_suffix}(?![a-zA-Z0-9\x{{80}}-\x{{FFFF}}_@%+:/\\-]|\.{path_character}))"
    );
    fancy_regex::Regex::new(&pattern).expect("linkify pattern")
});

static INLINE_CODE_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://[^\s<>]+$").unwrap());

/// source.ts linkMetadata `linkRecognition.inlineCode`.
pub fn is_inline_code_file_link(code: &str, workspace_root: Option<&str>) -> bool {
    if INLINE_CODE_URL.is_match(code) {
        return true;
    }
    matches!(
        classify_assistant_file_link(code, workspace_root, false, false),
        Some(FileLinkClassification::Direct(_) | FileLinkClassification::Ambiguous(_))
    )
}

/// Every linkified range of `text` (byte offsets).
pub fn linkify_ranges(text: &str) -> Vec<(usize, usize)> {
    ASSISTANT_LINKIFY_PATTERN
        .find_iter(text)
        .filter_map(Result::ok)
        .map(|m| (m.start(), m.end()))
        .collect()
}

// ---- Resolution (resolver.ts) ----

#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    Ignored,
    External(String),
    File(InlinePathTarget),
    NeedsLookup { query: String, token: String, target: InlinePathTarget },
}

pub fn classify_for_resolution(source: &AssistantFileLinkSource, workspace_root: Option<&str>) -> Resolution {
    // Markdown hrefs are percent-encoded by the parser; inline code and
    // linkified prose are raw paths that must not be decoded.
    let mut token = source.href.clone();
    let mut from_href = true;
    match source.kind {
        FileLinkSourceType::InlineCode => {
            let text = source.text.as_deref().unwrap_or("").trim();
            token = if text.is_empty() { source.href.clone() } else { text.to_owned() };
            from_href = false;
        }
        FileLinkSourceType::Linkify => {
            let text = source.text.as_deref().unwrap_or("").trim();
            if !text.is_empty() && is_file_looking_assistant_token(text) {
                token = text.to_owned();
                from_href = false;
            }
        }
        FileLinkSourceType::Markdown => {}
    }
    let token = token.trim().to_owned();
    if token.is_empty() {
        return Resolution::Ignored;
    }
    let classification = classify_assistant_file_link(&token, workspace_root, from_href, source.kind == FileLinkSourceType::Markdown);
    let target = match classification {
        None => return Resolution::Ignored,
        Some(FileLinkClassification::External(raw)) => return Resolution::External(raw),
        Some(FileLinkClassification::Direct(target)) => {
            if !resolve_through_suggestions(source, &token, &target, workspace_root) {
                return Resolution::File(target);
            }
            target
        }
        Some(FileLinkClassification::Ambiguous(target)) => target,
    };
    let root = workspace_root.unwrap_or("").trim();
    if root.is_empty() {
        return Resolution::Ignored;
    }
    Resolution::NeedsLookup { query: ambiguous_query(&target, root), token, target }
}

/// resolver.ts shouldResolveDirectFileThroughSuggestions.
fn resolve_through_suggestions(source: &AssistantFileLinkSource, token: &str, target: &InlinePathTarget, workspace_root: Option<&str>) -> bool {
    if source.kind != FileLinkSourceType::InlineCode {
        return false;
    }
    if token.starts_with('/') || token.to_lowercase().starts_with("file://") || DRIVE_ABSOLUTE.is_match(token) {
        return false;
    }
    let root = workspace_root.unwrap_or("").trim();
    if root.is_empty() {
        return false;
    }
    let normalized_root = root.replace('\\', "/");
    let normalized_root = normalized_root.trim_end_matches('/');
    target.path.replace('\\', "/").starts_with(&format!("{normalized_root}/"))
}

fn ambiguous_query(target: &InlinePathTarget, workspace_root: &str) -> String {
    let root = workspace_root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    let path = target.path.replace('\\', "/");
    if let Some(rest) = path.strip_prefix(&format!("{root}/")) {
        return rest.to_owned();
    }
    match path.rfind('/') {
        Some(slash) => path[slash + 1..].to_owned(),
        None => path,
    }
}

/// assistant-file-links tooltip-path.ts formatFileLinkTooltipPath.
pub fn link_tooltip(href: &str, cwd: Option<&str>) -> Option<String> {
    let has_scheme = SCHEME.is_match(href) && !DRIVE_ABSOLUTE.is_match(href);
    if has_scheme && !href.to_lowercase().starts_with("file:") {
        return None;
    }
    static LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:#L|:)(\d+)(?:-L?(\d+))?$").unwrap());
    let captures = LINE.captures(href);
    let mut path = match &captures {
        Some(c) => href[..c.get(0).unwrap().start()].to_owned(),
        None => href.to_owned(),
    };
    if path.starts_with("file://") {
        path = parse_file_protocol_url(&path).map(|t| t.path).unwrap_or(path);
    }
    if path.is_empty() {
        return None;
    }
    let mut result = super::tools::strip_cwd_prefix(&path, cwd);
    if let Some(c) = captures {
        let start = c.get(1).unwrap().as_str();
        result.push(':');
        result.push_str(start);
        if let Some(end) = c.get(2)
            && end.as_str() != start
        {
            result.push('-');
            result.push_str(end.as_str());
        }
    }
    Some(result)
}
