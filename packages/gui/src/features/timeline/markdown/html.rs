//! Port of `markdown_html.dart` (components/markdown/html.ts
//! normalizeMarkdownHtml): comment HTML (GitHub review bodies, agent
//! `<details>` blocks) becomes Markdown before rendering, so no content is
//! dropped and code keeps its original spelling.

use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use std::sync::LazyLock;

const ATTRIBUTE: &str = r#"\s+[A-Za-z_:][\w.:-]*(?:\s*=\s*(?:[^\s"'=<>`]+|'[^']*'|"[^"]*"))?"#;

static HTML: LazyLock<FancyRegex> = LazyLock::new(|| {
    let tag = format!(r"<(?:[A-Za-z][A-Za-z0-9-]*(?:{ATTRIBUTE})*\s*/?|/[A-Za-z][A-Za-z0-9-]*\s*)>");
    FancyRegex::new(&format!(
        r"(?i)(?<!\\)(?:<!--[\s\S]*?-->|<code\b[^>]*>[\s\S]*?</code\s*>|<summary\b[^>]*>[\s\S]*?</summary\s*>|{tag})"
    ))
    .expect("html pattern")
});
static SUMMARY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^<summary\b[^>]*>([\s\S]*?)</summary\s*>$").unwrap());
static CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^<code\b[^>]*>([\s\S]*?)</code\s*>$").unwrap());
static TAG_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^</?([\w-]+)").unwrap());
static BLANK_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n[ \t]*\n").unwrap());
static FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[ \t>]*(`{3,}|~{3,})").unwrap());
static BACKTICKS: LazyLock<FancyRegex> = LazyLock::new(|| FancyRegex::new(r"(?<!\\)`+").unwrap());
static DESTINATIONS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\]\(\s*(?:<[^>\n]*>|[^\s()<]+)?(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*\)"#).unwrap()
});
static NEXT_CLOSING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*</([\w-]+)").unwrap());
static CELL_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^<t[dh]\b").unwrap());
static TAG_START: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<[A-Za-z][A-Za-z0-9-]*").unwrap());
static ATTRIBUTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"([^\s"'>/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#).unwrap()
});

const BLOCK_TAGS: &[&str] = &["div", "p", "table", "tr", "h1", "h2", "h3", "h4", "h5", "h6", "section", "article", "ul", "ol", "li", "blockquote", "pre"];

fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

fn emphasis(name: &str) -> Option<&'static str> {
    match name {
        "b" | "strong" => Some("**"),
        "i" | "em" => Some("*"),
        _ => None,
    }
}

pub fn normalize_markdown_html(source: &str) -> String {
    if !source.contains('<') {
        return source.to_owned();
    }
    let protected = protected_ranges(source);
    let mut links: Vec<Option<String>> = Vec::new();
    let mut previous_tag: Option<(String, usize)> = None;
    let mut result = String::with_capacity(source.len());
    let mut cursor = 0;
    for found in HTML.find_iter(source).filter_map(Result::ok) {
        if found.start() < cursor {
            continue;
        }
        if protected.iter().any(|&(s, e)| found.start() >= s && found.start() < e) {
            continue;
        }
        result.push_str(&source[cursor..found.start()]);
        let converted = convert(source, found.as_str(), found.start(), &mut links, &mut previous_tag);
        result.push_str(&converted);
        cursor = found.end();
    }
    result.push_str(&source[cursor..]);
    result
}

fn convert(source: &str, raw: &str, offset: usize, links: &mut Vec<Option<String>>, previous_tag: &mut Option<(String, usize)>) -> String {
    if raw.starts_with("<!--") {
        return String::new();
    }
    if let Some(summary) = SUMMARY.captures(raw) {
        let title = normalize_markdown_html(&summary[1]);
        let inline = BLANK_SPLIT
            .split(title.trim())
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return format!("**{inline}**\n\n");
    }
    if let Some(code) = CODE.captures(raw) {
        return inline_code(&decode_entities(&code[1]));
    }
    let closing = raw.starts_with("</");
    let name = TAG_NAME.captures(raw).map(|c| c[1].to_lowercase()).unwrap_or_default();
    let starts_after_boundary = match previous_tag {
        Some((prev_name, end)) => BLOCK_TAGS.contains(&prev_name.as_str()) && is_blank(&source[*end..offset]),
        None => is_blank(&source[..offset]),
    };
    *previous_tag = Some((name.clone(), offset + raw.len()));
    if let Some(marker) = emphasis(&name) {
        return marker.to_owned();
    }
    if let Some(boundary) = layout_boundary(&name, source, raw, offset, closing, starts_after_boundary) {
        return boundary;
    }
    if name == "a" {
        return markdown_anchor(raw, closing, links);
    }
    if name == "img" && !closing {
        let attributes = attributes(raw);
        let alt = attributes.iter().find(|(k, _)| k == "alt").map(|(_, v)| v.clone()).unwrap_or_default();
        return match attributes.iter().find(|(k, _)| k == "src") {
            Some((_, src)) => {
                let escaped: String = alt
                    .chars()
                    .flat_map(|c| if matches!(c, '[' | ']' | '\\') { vec!['\\', c] } else { vec![c] })
                    .collect();
                format!("![{escaped}](<{}>)", markdown_url(src))
            }
            None => alt,
        };
    }
    String::new()
}

/// Fenced code, inline code spans and inline link destinations/titles,
/// which CommonMark never parses as HTML.
fn protected_ranges(source: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut open: Option<String> = None;
    let mut open_start = 0;
    for line in source.split('\n') {
        let found = FENCE.captures(line);
        match &open {
            None => {
                if let Some(c) = &found {
                    let marker = c.get(1).unwrap();
                    let tail = &line[c.get(0).unwrap().end()..];
                    if !(marker.as_str().starts_with('`') && tail.contains('`')) {
                        open = Some(marker.as_str().to_owned());
                        open_start = offset;
                    }
                }
            }
            Some(fence) => {
                if let Some(c) = &found {
                    let marker = c.get(1).unwrap().as_str();
                    let tail = &line[c.get(0).unwrap().end()..];
                    if marker.as_bytes()[0] == fence.as_bytes()[0] && marker.len() >= fence.len() && tail.trim().is_empty() {
                        ranges.push((open_start, offset + line.len()));
                        open = None;
                    }
                }
            }
        }
        offset += line.len() + 1;
    }
    if open.is_some() {
        ranges.push((open_start, source.len()));
    }
    let fenced_ranges = ranges.clone();
    let fenced = |index: usize| fenced_ranges.iter().any(|&(s, e)| index >= s && index < e);
    let runs: Vec<(usize, usize)> = BACKTICKS.find_iter(source).filter_map(Result::ok).map(|m| (m.start(), m.end())).collect();
    let mut i = 0;
    while i < runs.len() {
        let run = runs[i];
        if fenced(run.0) {
            i += 1;
            continue;
        }
        let length = run.1 - run.0;
        let mut j = i + 1;
        while j < runs.len() {
            let candidate = runs[j];
            if fenced(candidate.0) {
                break;
            }
            if candidate.1 - candidate.0 == length {
                ranges.push((run.0, candidate.1));
                i = j;
                break;
            }
            j += 1;
        }
        i += 1;
    }
    for found in DESTINATIONS.find_iter(source) {
        if !fenced(found.start()) {
            ranges.push((found.start(), found.end()));
        }
    }
    ranges
}

fn layout_boundary(name: &str, source: &str, raw: &str, offset: usize, closing: bool, starts_after_boundary: bool) -> Option<String> {
    if name == "br" {
        return Some(if closing { String::new() } else { "  \n".into() });
    }
    if name == "summary" {
        return Some(if closing { "**\n\n".into() } else { "**".into() });
    }
    if name == "details" {
        let at_edge = if closing { offset + raw.len() == source.len() } else { offset == 0 };
        return Some(if at_edge { String::new() } else { "\n\n".into() });
    }
    if BLOCK_TAGS.contains(&name) {
        return Some(block_boundary(source, raw, offset, closing, starts_after_boundary));
    }
    if (name == "td" || name == "th") && closing {
        return Some(if CELL_OPEN.is_match(&source[offset + raw.len()..]) { " ".into() } else { String::new() });
    }
    None
}

fn block_boundary(source: &str, raw: &str, offset: usize, closing: bool, starts_after_boundary: bool) -> String {
    if closing {
        let following = &source[offset + raw.len()..];
        if is_blank(following) || following.starts_with("\n\n") {
            return String::new();
        }
        if let Some(next) = NEXT_CLOSING.captures(following)
            && BLOCK_TAGS.contains(&next[1].to_lowercase().as_str())
        {
            return String::new();
        }
    } else if starts_after_boundary {
        return String::new();
    }
    "\n\n".into()
}

fn markdown_anchor(raw: &str, closing: bool, links: &mut Vec<Option<String>>) -> String {
    if closing {
        return match links.pop() {
            Some(Some(href)) => format!("](<{}>)", markdown_url(&href)),
            _ => String::new(),
        };
    }
    let href = attributes(raw).into_iter().find(|(k, _)| k == "href").map(|(_, v)| v);
    let opening = if href.is_some() { "[" } else { "" };
    links.push(href);
    opening.to_owned()
}

fn markdown_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            c => out.push(c),
        }
    }
    out
}

fn inline_code(content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let marker = "`".repeat(longest + 1);
    let padding = if content.starts_with('`') || content.ends_with('`') { " " } else { "" };
    format!("{marker}{padding}{content}{padding}{marker}")
}

/// htmlparser2 `onopentag` attributes: lower-cased names, first value wins,
/// entity-decoded values (order kept).
fn attributes(tag: &str) -> Vec<(String, String)> {
    let Some(name) = TAG_START.find(tag) else { return Vec::new() };
    let mut result: Vec<(String, String)> = Vec::new();
    for c in ATTRIBUTES.captures_iter(&tag[name.end()..]) {
        let key = c[1].to_lowercase();
        if result.iter().any(|(k, _)| *k == key) {
            continue;
        }
        let value = c.get(2).or(c.get(3)).or(c.get(4)).map(|m| m.as_str()).unwrap_or("");
        result.push((key, decode_entities(value)));
    }
    result
}

/// micromark decodeString: named, decimal and hex character references.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail[1..].find(';').map(|i| i + 1).filter(|&i| i <= 33) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let entity = &tail[1..semi];
        let decoded = if let Some(num) = entity.strip_prefix('#') {
            let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                u32::from_str_radix(hex, 16).ok()
            } else {
                num.parse::<u32>().ok()
            };
            code.map(|c| char::from_u32(c).filter(|&c| c != '\0').unwrap_or('\u{FFFD}'))
        } else {
            named_entity(entity)
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn named_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bull" => '•',
        "middot" => '·',
        "times" => '×',
        "divide" => '÷',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "check" => '✓',
        "deg" => '°',
        "plusmn" => '±',
        "laquo" => '«',
        "raquo" => '»',
        "sect" => '§',
        "para" => '¶',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        _ => return None,
    })
}
