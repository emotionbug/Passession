//! Read-only access to the Flutter client's `shared_preferences` values, so the
//! native client keeps preference-backed identities after the upgrade.
//! Keys are returned without the plugin's `flutter.` prefix.

use serde_json::{Map, Value};

const PREFIX: &str = "flutter.";

/// Values the Flutter plugin stored; empty when the client never ran.
pub fn load_legacy_preferences() -> Map<String, Value> {
    let mut values = Map::new();
    for (key, value) in platform_values() {
        if let Some(stripped) = key.strip_prefix(PREFIX) {
            values.insert(stripped.to_owned(), value);
        }
    }
    values
}

#[cfg(any(target_os = "linux", windows))]
fn platform_values() -> Vec<(String, Value)> {
    // shared_preferences_linux / _windows: <support>/shared_preferences.json.
    let file = crate::paths::app_paths().support_dir.join("shared_preferences.json");
    match std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) {
        Some(Value::Object(map)) => map.into_iter().collect(),
        _ => Vec::new(),
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn platform_values() -> Vec<(String, Value)> {
    crate::apple::user_defaults_with_prefix(PREFIX)
}

#[cfg(target_os = "android")]
fn platform_values() -> Vec<(String, Value)> {
    // shared_preferences_android legacy API: <dataDir>/shared_prefs/FlutterSharedPreferences.xml.
    let Some(data_dir) = crate::paths::app_paths().support_dir.parent() else {
        return Vec::new();
    };
    let file = data_dir.join("shared_prefs").join("FlutterSharedPreferences.xml");
    std::fs::read_to_string(file)
        .map(|xml| parse_android_preferences(&xml))
        .unwrap_or_default()
}

/// Parses Android's `SharedPreferences` XML (`<map>` of typed entries).
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn parse_android_preferences(xml: &str) -> Vec<(String, Value)> {
    let mut values = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('>') else { break };
        let tag = &rest[..end];
        rest = &rest[end + 1..];
        let (kind, attributes, self_closing) = match tag.split_once(char::is_whitespace) {
            Some((kind, attributes)) => {
                let closing = attributes.trim_end().ends_with('/');
                (kind, attributes.trim_end().trim_end_matches('/'), closing)
            }
            None => continue,
        };
        let Some(name) = attribute(attributes, "name") else { continue };
        match kind {
            "string" if !self_closing => {
                let Some(close) = rest.find("</string>") else { break };
                values.push((name, Value::String(unescape(&rest[..close]))));
                rest = &rest[close + "</string>".len()..];
            }
            "boolean" => {
                if let Some(value) = attribute(attributes, "value") {
                    values.push((name, Value::Bool(value == "true")));
                }
            }
            "int" | "long" => {
                if let Some(value) = attribute(attributes, "value").and_then(|v| v.parse::<i64>().ok()) {
                    values.push((name, Value::from(value)));
                }
            }
            "float" => {
                if let Some(value) = attribute(attributes, "value").and_then(|v| v.parse::<f64>().ok()) {
                    values.push((name, Value::from(value)));
                }
            }
            "set" if !self_closing => {
                let Some(close) = rest.find("</set>") else { break };
                let body = &rest[..close];
                let items = body
                    .split("<string>")
                    .skip(1)
                    .filter_map(|item| item.split_once("</string>").map(|(v, _)| Value::String(unescape(v))))
                    .collect();
                values.push((name, Value::Array(items)));
                rest = &rest[close + "</set>".len()..];
            }
            _ => {}
        }
    }
    values
}

fn attribute(attributes: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = attributes.find(&needle)? + needle.len();
    let end = attributes[start..].find('"')? + start;
    Some(unescape(&attributes[start..end]))
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';') else { break };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32),
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_xml_entries_are_typed() {
        let xml = r#"<?xml version='1.0' encoding='utf-8' standalone='yes' ?>
<map>
    <string name="flutter.mobile.expo.installationId">a&amp;b</string>
    <boolean name="flutter.flag" value="true" />
    <long name="flutter.count" value="12" />
    <set name="flutter.items"><string>x</string><string>y</string></set>
</map>"#;
        let values = parse_android_preferences(xml);
        assert_eq!(values[0], ("flutter.mobile.expo.installationId".into(), Value::String("a&b".into())));
        assert_eq!(values[1], ("flutter.flag".into(), Value::Bool(true)));
        assert_eq!(values[2], ("flutter.count".into(), Value::from(12)));
        assert_eq!(values[3].1, Value::Array(vec!["x".into(), "y".into()]));
    }
}
