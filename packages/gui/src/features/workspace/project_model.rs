//! Pure parts of the project settings form (`project_settings_panel.dart`,
//! original `project-config-form.ts`): lifecycle hook text, scripts editing with position
//! preserved, metadata instructions, service port allocation and icon URL import.

use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::rpc::{object_of, str_of};

pub const METADATA_KEYS: [&str; 3] = ["branchName", "commitMessage", "pullRequest"];
pub const WORKTREE_DOCS_URL: &str = "https://paseo.sh/docs/worktrees";
pub const MAX_ICON_BYTES: usize = 512 * 1024;

/// A lifecycle hook (`setup`/`teardown`) as one text area: a string stays a string, an
/// array joins by lines.
pub fn lifecycle_text(config: &Value, field: &str) -> String {
    match object_of(config, "worktree").get(field) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// Writes a lifecycle hook back in the shape it was loaded with (`string`, `array` or
/// `missing`, remembered per field in `kinds` until the next load).
pub fn set_lifecycle_text(config: &mut Value, loaded: &Value, kinds: &mut HashMap<String, &'static str>, field: &str, text: &str) {
    let lines: Vec<&str> = text.split('\n').filter(|line| !line.trim().is_empty()).collect();
    let kind = *kinds.entry(field.to_owned()).or_insert_with(|| match object_of(loaded, "worktree").get(field) {
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        _ => "missing",
    });
    let worktree = ensure_object(config, "worktree");
    if lines.is_empty() {
        worktree.remove(field);
    } else {
        let value = match kind {
            "string" => Value::String(lines.join("\n")),
            "array" => json!(lines),
            _ if lines.len() == 1 => Value::String(lines[0].to_owned()),
            _ => json!(lines),
        };
        worktree.insert(field.to_owned(), value);
    }
}

/// The object at `key` of `config`, created when missing or not an object.
pub fn ensure_object<'a>(config: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !config.is_object() {
        *config = Value::Object(Map::new());
    }
    let map = config.as_object_mut().expect("object ensured");
    let entry = map.entry(key.to_owned()).or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        *entry = Value::Object(Map::new());
    }
    entry.as_object_mut().expect("object ensured")
}

/// Scripts in config order.
pub fn scripts(config: &Value) -> Vec<(String, Value)> {
    object_of(config, "scripts")
        .as_object()
        .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// Saving is blocked while any script has an empty name or command.
pub fn invalid_scripts(config: &Value) -> bool {
    scripts(config).iter().any(|(name, script)| name.trim().is_empty() || str_of(script, "command").trim().is_empty())
}

/// The script row hint: type · port · first command line.
pub fn script_hint(script: &Value, port_label: impl Fn(&str) -> String) -> String {
    let mut parts = Vec::new();
    let kind = str_of(script, "type");
    if !kind.is_empty() {
        parts.push(kind.to_owned());
    }
    let port = match script.get("port") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if !port.is_empty() {
        parts.push(port_label(&port));
    }
    let command = str_of(script, "command");
    if !command.is_empty() {
        parts.push(command.split('\n').next().unwrap_or("").to_owned());
    }
    parts.join(" · ")
}

/// Validation of the script editor (`ScriptEditModal`).
pub fn script_name_error(name: &str, old_name: Option<&str>, config: &Value) -> Option<ScriptNameError> {
    let key = name.trim();
    if key.is_empty() {
        return Some(ScriptNameError::Required);
    }
    if Some(key) != old_name && object_of(config, "scripts").get(key).is_some() {
        return Some(ScriptNameError::Duplicate);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptNameError {
    Required,
    Duplicate,
}

/// The edited script entry: unknown fields of the existing entry kept, `type: service`
/// toggled, `port` as an integer when it parses, else the text (`$PORT`).
pub fn script_entry(existing: &Value, command: &str, port: &str, service: bool) -> Value {
    let mut entry = existing.as_object().cloned().unwrap_or_default();
    entry.insert("command".into(), Value::String(command.to_owned()));
    if service {
        entry.insert("type".into(), Value::String("service".into()));
    } else if entry.get("type").and_then(Value::as_str) == Some("service") {
        entry.remove("type");
    }
    let port = port.trim();
    if port.is_empty() {
        entry.remove("port");
    } else {
        entry.insert(
            "port".into(),
            port.parse::<i64>().map(Value::from).unwrap_or_else(|_| Value::String(port.to_owned())),
        );
    }
    Value::Object(entry)
}

/// Puts the edited script back in its original position (or appends a new one).
pub fn replace_script(config: &mut Value, old_name: Option<&str>, name: &str, entry: Value) {
    let current = object_of(config, "scripts").as_object().cloned().unwrap_or_default();
    let mut next = Map::new();
    for (key, value) in current {
        if Some(key.as_str()) == old_name {
            next.insert(name.to_owned(), entry.clone());
        } else if key != name {
            next.insert(key, value);
        }
    }
    if old_name.is_none() {
        next.insert(name.to_owned(), entry);
    }
    config.as_object_mut().map(|map| map.insert("scripts".into(), Value::Object(next)));
}

pub fn remove_script(config: &mut Value, name: &str) {
    ensure_object(config, "scripts").remove(name);
}

/// The metadata generation instructions for `key`.
pub fn metadata_text(config: &Value, key: &str) -> String {
    str_of(object_of(object_of(config, "metadataGeneration"), key), "instructions").to_owned()
}

pub fn set_metadata_text(config: &mut Value, key: &str, text: &str) {
    let metadata = ensure_object(config, "metadataGeneration");
    let mut entry = metadata.get(key).and_then(Value::as_object).cloned().unwrap_or_default();
    if text.trim().is_empty() {
        entry.remove("instructions");
    } else {
        entry.insert("instructions".into(), Value::String(text.to_owned()));
    }
    if entry.is_empty() {
        metadata.remove(key);
    } else {
        metadata.insert(key.to_owned(), Value::Object(entry));
    }
}

/// An inclusive port range `start-end` within 1..=65535.
pub fn valid_port_range(range: &str) -> bool {
    let range = range.trim();
    if range.is_empty() {
        return true;
    }
    let Some((start, end)) = range.split_once('-') else { return false };
    if end.contains('-') {
        return false;
    }
    match (start.parse::<u32>(), end.parse::<u32>()) {
        (Ok(start), Ok(end)) => start >= 1 && end <= 65535 && start <= end,
        _ => false,
    }
}

pub fn set_service_ports(config: &mut Value, range: &str, script: &str) {
    let worktree = ensure_object(config, "worktree");
    if range.trim().is_empty() && script.trim().is_empty() {
        worktree.remove("servicePorts");
        return;
    }
    let mut ports = Map::new();
    if !range.trim().is_empty() {
        ports.insert("range".into(), Value::String(range.trim().to_owned()));
    }
    if !script.trim().is_empty() {
        ports.insert("portScript".into(), Value::String(script.to_owned()));
    }
    worktree.insert("servicePorts".into(), Value::Object(ports));
}

/// `importProjectIconFromUrl` URL rule: http(s) with a host and no credentials.
pub fn icon_url_error(value: &str) -> Option<&'static str> {
    let value = value.trim();
    let Some((scheme, rest)) = value.split_once("://") else { return Some("Enter a valid website or image URL") };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Some("Enter a valid website or image URL");
    }
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") || authority.contains('@') {
        return Some("URL must use HTTP or HTTPS without credentials");
    }
    None
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut search = 0;
    while let Some(found) = lower[search..].find(name) {
        let start = search + found;
        search = start + name.len();
        let before_ok = start == 0 || !lower.as_bytes()[start - 1].is_ascii_alphanumeric();
        if !before_ok {
            continue;
        }
        let rest = tag[search..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { continue };
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let value = &rest[1..];
        let end = value.find(quote)?;
        let value = value[..end].trim();
        return (!value.is_empty()).then(|| value.to_owned());
    }
    None
}

/// Declared non-SVG `<link rel=icon>` hrefs of a page, in document order.
pub fn declared_icons(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut search = 0;
    while let Some(found) = lower[search..].find("<link") {
        let start = search + found;
        let Some(end) = lower[start..].find('>') else { break };
        let tag = &html[start..start + end + 1];
        search = start + end + 1;
        let rel = attribute(tag, "rel").unwrap_or_default().to_ascii_lowercase();
        let kind = attribute(tag, "type").unwrap_or_default().to_ascii_lowercase();
        let Some(href) = attribute(tag, "href") else { continue };
        if !rel.contains("icon") || kind.contains("svg") || href.ends_with(".svg") {
            continue;
        }
        out.push(href);
    }
    out
}

/// Resolves `href` against `base` (absolute, scheme-relative, root-relative, relative).
pub fn resolve_url(base: &str, href: &str) -> String {
    if href.contains("://") {
        return href.to_owned();
    }
    let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if let Some(rest) = href.strip_prefix("//") {
        return format!("{scheme}://{rest}");
    }
    if href.starts_with('/') {
        return format!("{scheme}://{authority}{href}");
    }
    let path = &rest[authority.len()..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let directory = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "/",
    };
    format!("{scheme}://{authority}{directory}{href}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_keeps_loaded_shape() {
        let loaded = json!({"worktree": {"setup": ["a"], "teardown": "x"}});
        let mut config = loaded.clone();
        let mut kinds = HashMap::new();
        set_lifecycle_text(&mut config, &loaded, &mut kinds, "setup", "a\n\nb");
        assert_eq!(config["worktree"]["setup"], json!(["a", "b"]));
        set_lifecycle_text(&mut config, &loaded, &mut kinds, "teardown", "y\nz");
        assert_eq!(config["worktree"]["teardown"], json!("y\nz"));
        set_lifecycle_text(&mut config, &loaded, &mut kinds, "other", "one");
        assert_eq!(config["worktree"]["other"], json!("one"));
        set_lifecycle_text(&mut config, &loaded, &mut kinds, "setup", "  ");
        assert!(config["worktree"].get("setup").is_none());
    }

    #[test]
    fn scripts_replace_in_place_and_validate() {
        let mut config = json!({"scripts": {"a": {"command": "x"}, "b": {"command": "y", "extra": 1}}});
        let entry = script_entry(&config["scripts"]["b"], "z", "3000", true);
        replace_script(&mut config, Some("b"), "c", entry);
        let names: Vec<_> = scripts(&config).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["a", "c"]);
        assert_eq!(config["scripts"]["c"], json!({"command": "z", "extra": 1, "type": "service", "port": 3000}));
        assert_eq!(script_name_error("a", None, &config), Some(ScriptNameError::Duplicate));
        assert_eq!(script_name_error(" ", None, &config), Some(ScriptNameError::Required));
        assert_eq!(script_entry(&json!({"type": "service"}), "c", "$PORT", false), json!({"command": "c", "port": "$PORT"}));
    }

    #[test]
    fn port_ranges_and_metadata() {
        assert!(valid_port_range("3000-3999"));
        assert!(!valid_port_range("0-10"));
        assert!(!valid_port_range("10-5"));
        assert!(valid_port_range(""));
        let mut config = json!({});
        set_metadata_text(&mut config, "branchName", "short");
        assert_eq!(metadata_text(&config, "branchName"), "short");
        set_metadata_text(&mut config, "branchName", "");
        assert!(config["metadataGeneration"].get("branchName").is_none());
    }

    #[test]
    fn icon_url_rules() {
        assert!(icon_url_error("https://example.com").is_none());
        assert!(icon_url_error("ftp://example.com").is_some());
        assert!(icon_url_error("https://user@example.com").is_some());
        assert_eq!(declared_icons("<link rel=\"icon\" href=\"/a.png\"><link rel='icon' type='image/svg+xml' href='/b.svg'>"), ["/a.png"]);
        assert_eq!(resolve_url("https://e.com/x/y", "/favicon.ico"), "https://e.com/favicon.ico");
        assert_eq!(resolve_url("https://e.com/x/y", "i.png"), "https://e.com/x/i.png");
    }
}
