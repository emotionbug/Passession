//! Lossless JSON accessors for daemon/config payloads. Settings never
//! re-serializes a typed struct, so unknown fields survive every write.

use serde_json::{Map, Value};
use std::sync::LazyLock;

static EMPTY_MAP: LazyLock<Map<String, Value>> = LazyLock::new(Map::new);
static NULL: Value = Value::Null;

/// `value[key]`, or `null`.
pub fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&NULL)
}

/// The string at `key`, or `""` (Dart `stringValue`).
pub fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The string at `key`, or `fallback` when missing or empty.
pub fn str_or<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    match value.get(key).and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text,
        _ => fallback,
    }
}

/// Dart `stringValue(value)` for scalars: strings as is, numbers/bools printed.
pub fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

pub fn obj<'a>(value: &'a Value, key: &str) -> &'a Map<String, Value> {
    value.get(key).and_then(Value::as_object).unwrap_or(&EMPTY_MAP)
}

pub fn as_obj(value: &Value) -> &Map<String, Value> {
    value.as_object().unwrap_or(&EMPTY_MAP)
}

pub fn arr<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

pub fn bool_of(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

pub fn is_true(value: &Value, key: &str) -> bool {
    bool_of(value, key) == Some(true)
}

/// `features[name] == true` inside a `server_info`.
pub fn feature(server_info: &Value, name: &str) -> bool {
    obj(server_info, "features").get(name).and_then(Value::as_bool) == Some(true)
}

/// Trimmed non-empty lines (Dart `textLines`).
pub fn text_lines(value: &str) -> Vec<String> {
    value.split('\n').map(str::trim).filter(|line| !line.is_empty()).map(String::from).collect()
}

/// Lines of `NAME=value` into an object; a line without a name fails.
pub fn parse_pairs(value: &str, message: &str) -> Result<Map<String, Value>, String> {
    let mut result = Map::new();
    for line in text_lines(value) {
        match line.find('=') {
            Some(at) if at >= 1 => {
                result.insert(line[..at].to_string(), Value::String(line[at + 1..].to_string()));
            }
            _ => return Err(message.to_string()),
        }
    }
    Ok(result)
}

pub fn pairs_text(map: &Map<String, Value>) -> String {
    map.iter().map(|(key, value)| format!("{key}={}", scalar_text(value))).collect::<Vec<_>>().join("\n")
}

pub fn lines_text(values: &[Value]) -> String {
    values.iter().map(scalar_text).collect::<Vec<_>>().join("\n")
}

/// Pretty JSON for report sheets (Dart `JsonEncoder.withIndent('  ')`).
pub fn pretty(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}
