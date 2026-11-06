//! App preference rules from `app_preferences.dart` (original
//! `use-settings/storage.ts`, `sidebar-nav/model.ts`, `files-section.tsx`).

use serde_json::{Map, Value, json};

use crate::features::settings::json::str_of;

pub const MIN_UI_FONT: i64 = 10;
pub const MAX_UI_FONT: i64 = 21;
pub const MIN_CONTENT_FONT: i64 = 10;
pub const MAX_CONTENT_FONT: i64 = 21;
pub const MIN_CODE_FONT: i64 = 9;
pub const MAX_CODE_FONT: i64 = 22;
pub const MIN_CONTENT_WIDTH: i64 = 600;
pub const MAX_CONTENT_WIDTH: i64 = 4000;
pub const DEFAULT_CONTENT_WIDTH: i64 = 820;
const MAX_FONT_FAMILY: usize = 200;

pub fn parse_clamped(value: &Value, min: i64, max: i64) -> Option<i64> {
    let number = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    if !number.is_finite() {
        return None;
    }
    Some((number.floor() as i64).clamp(min, max))
}

pub fn parse_clamped_text(text: &str, min: i64, max: i64) -> Option<i64> {
    parse_clamped(&Value::String(text.to_string()), min, max)
}

/// Original `sanitizeFontFamily`: empty means default, invalid is `None`.
pub fn sanitize_font_family(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    if trimmed.chars().count() > MAX_FONT_FAMILY || trimmed.contains([';', '{', '}', '<', '>']) || trimmed.chars().any(|c| (c as u32) <= 0x1f) {
        return None;
    }
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in trimmed.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if "()[]/*".contains(c) {
            return None;
        }
    }
    if quote.is_none() && !escaped { Some(trimmed.to_string()) } else { None }
}

/// Original `toPreferences`: resolved rows lead in the slots of the stored keys
/// they occupy; entries for unavailable keys keep their place.
pub fn sidebar_nav_preferences(items: &[Value], previous: &[Value]) -> Vec<Value> {
    let mut remaining: Vec<Value> = items
        .iter()
        .map(|item| json!({"key": item.get("key").cloned().unwrap_or(Value::Null), "visible": item.get("visible").and_then(Value::as_bool) != Some(false)}))
        .collect();
    let available: Vec<Value> = remaining.iter().map(|item| item["key"].clone()).collect();
    let mut preferences = Vec::new();
    let mut seen: Vec<Value> = Vec::new();
    for preference in previous {
        let key = preference.get("key").cloned().unwrap_or(Value::Null);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        if available.contains(&key) {
            if !remaining.is_empty() {
                preferences.push(remaining.remove(0));
            }
            continue;
        }
        preferences.push(json!({"key": key, "visible": preference.get("visible").and_then(Value::as_bool) == Some(true)}));
    }
    preferences.extend(remaining);
    preferences
}

/// The settings nav card rows: stored order for known keys, then defaults.
pub fn nav_rows(stored: &[Value], defaults: &[&str]) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut placed: Vec<String> = Vec::new();
    for row in stored {
        let id = str_of(row, "key");
        if defaults.contains(&id) && !placed.iter().any(|p| p == id) {
            placed.push(id.to_string());
            rows.push(json!({"key": id, "visible": row.get("visible").and_then(Value::as_bool) != Some(false)}));
        }
    }
    for id in defaults {
        if !placed.iter().any(|p| p == id) {
            placed.push(id.to_string());
            rows.push(json!({"key": id, "visible": *id != "usage"}));
        }
    }
    rows
}

pub const THEMES: &[(&str, Option<u32>)] = &[
    ("light", Some(0xffffff)),
    ("dark", Some(0x2d8b62)),
    ("auto", None),
    ("zinc", Some(0x808080)),
    ("midnight", Some(0x4a6ba8)),
    ("claude", Some(0xd97757)),
    ("ghostty", Some(0x8caaee)),
    ("pureBlack", Some(0x000000)),
];

pub const SYNTAX_THEMES: &[(&str, &str)] = &[
    ("github", "GitHub"),
    ("catppuccin", "Catppuccin"),
    ("dracula", "Dracula"),
    ("tokyo-night", "Tokyo Night"),
    ("one", "One"),
    ("nord", "Nord"),
    ("gruvbox", "Gruvbox"),
    ("solarized", "Solarized"),
];

/// `_normalizedTheme`: legacy names map onto THEME_OPTIONS; unknown is auto.
pub fn normalized_theme(value: &Value) -> String {
    let theme = match value.as_str() {
        Some("dark-zinc") => "zinc",
        Some("dark-midnight") => "midnight",
        Some("dark-claude") => "claude",
        Some("dark-ghostty") => "ghostty",
        Some("dark-pure-black") => "pureBlack",
        Some("system") => "auto",
        Some(name) => name,
        None => "auto",
    };
    if THEMES.iter().any(|(t, _)| *t == theme) { theme.to_string() } else { "auto".into() }
}

/// `paseoLanguagePreference`.
pub fn language_preference(value: &Value) -> &'static str {
    let Some(stored) = value.as_str() else { return "system" };
    let trimmed = stored.trim();
    if trimmed.is_empty() || trimmed == "system" || trimmed == "auto" {
        return "system";
    }
    let tag = trimmed.split(['-', '_']).next().unwrap_or("").to_lowercase();
    if tag == "ko" { "ko" } else { "en" }
}

/// Built-in file panels in resolver order (`builtinFilePanels`).
pub struct FilePanel {
    pub id: &'static str,
    pub extensions: Option<&'static [&'static str]>,
    pub filenames: &'static [&'static str],
}

pub const BUILTIN_FILE_PANELS: &[FilePanel] = &[
    FilePanel { id: "builtin:markdown", extensions: Some(&["md", "markdown"]), filenames: &[] },
    FilePanel { id: "builtin:html", extensions: Some(&["html", "htm"]), filenames: &[] },
    FilePanel { id: "builtin:image", extensions: Some(&["png", "jpg", "jpeg", "gif", "webp", "svg"]), filenames: &[] },
    FilePanel { id: "builtin:source", extensions: None, filenames: &[] },
];

fn parse_selector(key: &str) -> (&'static str, &str) {
    match key.find(':') {
        Some(at) if at > 0 && &key[..at] == "name" => ("name", &key[at + 1..]),
        Some(at) => ("ext", &key[at + 1..]),
        None => ("ext", key),
    }
}

/// `filePanelSelectorLabel`: a filename as is, an extension dotted.
pub fn selector_label(key: &str) -> String {
    match parse_selector(key) {
        ("name", value) => value.to_string(),
        (_, value) => format!(".{value}"),
    }
}

fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) => name[dot + 1..].to_lowercase(),
        None => String::new(),
    }
}

fn claims(panel: &FilePanel, key: &str) -> bool {
    let Some(extensions) = panel.extensions else { return false };
    let (kind, value) = parse_selector(key);
    let name = if kind == "name" { value } else { "" };
    let extension = if kind == "name" { extension_of(value) } else { value.to_string() };
    panel.filenames.contains(&name) || extensions.iter().any(|e| e.to_lowercase() == extension)
}

/// Original `offeredCandidates`: the panels claiming the key; Source only
/// when no built-in claims it.
pub fn offered_panels(key: &str) -> Vec<&'static FilePanel> {
    let claiming: Vec<&FilePanel> = BUILTIN_FILE_PANELS.iter().filter(|p| claims(p, key)).collect();
    BUILTIN_FILE_PANELS
        .iter()
        .filter(|p| if p.extensions.is_none() { claiming.is_empty() } else { claiming.iter().any(|c| c.id == p.id) })
        .collect()
}

/// Original `buildFilePanelSettingsRows`: remembered keys, extensions before
/// filenames, each alphabetically.
pub fn file_panel_rows(associations: &Map<String, Value>) -> Vec<(String, Vec<&'static FilePanel>, Option<String>)> {
    let mut keys: Vec<&String> = associations.keys().collect();
    keys.sort_by(|a, b| {
        let (ka, va) = parse_selector(a);
        let (kb, vb) = parse_selector(b);
        if ka != kb {
            return if ka == "ext" { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }
        va.to_lowercase().cmp(&vb.to_lowercase())
    });
    keys.into_iter()
        .map(|key| (key.clone(), offered_panels(key), associations.get(key).and_then(Value::as_str).map(String::from)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_preferences_keep_unavailable_keys_in_place() {
        let previous = vec![json!({"key": "plugin-row", "visible": true}), json!({"key": "history", "visible": true}), json!({"key": "search", "visible": false})];
        let items = vec![json!({"key": "search", "visible": true}), json!({"key": "history", "visible": false}), json!({"key": "schedules", "visible": true})];
        let next = sidebar_nav_preferences(&items, &previous);
        assert_eq!(
            next,
            vec![
                json!({"key": "plugin-row", "visible": true}),
                json!({"key": "search", "visible": true}),
                json!({"key": "history", "visible": false}),
                json!({"key": "schedules", "visible": true}),
            ]
        );
    }

    #[test]
    fn font_family_sanitizing_rejects_css_injection() {
        assert_eq!(sanitize_font_family("  "), Some(String::new()));
        assert_eq!(sanitize_font_family("'JetBrains Mono', monospace").as_deref(), Some("'JetBrains Mono', monospace"));
        assert_eq!(sanitize_font_family("a; color: red"), None);
        assert_eq!(sanitize_font_family("url(x)"), None);
        assert_eq!(sanitize_font_family("'unterminated"), None);
        assert_eq!(sanitize_font_family("'a(b)'").as_deref(), Some("'a(b)'"));
    }

    #[test]
    fn file_panels_offer_claimers_or_source() {
        assert_eq!(offered_panels("ext:md").iter().map(|p| p.id).collect::<Vec<_>>(), vec!["builtin:markdown"]);
        assert_eq!(offered_panels("ext:rs").iter().map(|p| p.id).collect::<Vec<_>>(), vec!["builtin:source"]);
        assert_eq!(selector_label("name:Makefile"), "Makefile");
        assert_eq!(selector_label("ext:rs"), ".rs");
    }

    #[test]
    fn sizes_clamp_and_floor() {
        assert_eq!(parse_clamped(&json!(30), MIN_UI_FONT, MAX_UI_FONT), Some(21));
        assert_eq!(parse_clamped(&json!("12.9"), MIN_UI_FONT, MAX_UI_FONT), Some(12));
        assert_eq!(parse_clamped(&json!(""), MIN_UI_FONT, MAX_UI_FONT), None);
    }
}
