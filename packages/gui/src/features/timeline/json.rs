//! Lossless JSON accessors mirroring `core/json.dart` (stringValue, intValue,
//! jsonMap, jsonList). Timeline rows stay `serde_json::Value` so unknown
//! provider/plugin fields survive every fold and reconciliation.

use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;

static EMPTY_MAP: std::sync::LazyLock<JsonMap> = std::sync::LazyLock::new(JsonMap::new);
static EMPTY_LIST: Vec<Value> = Vec::new();

pub fn map(value: &Value) -> &JsonMap {
    value.as_object().unwrap_or(&EMPTY_MAP)
}

pub fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&EMPTY_LIST)
}

pub fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// `stringValue(value)`: the string, or "" for anything else.
pub fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

pub fn str_or<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    value.as_str().unwrap_or(fallback)
}

pub fn opt_str(value: &Value) -> Option<&str> {
    value.as_str()
}

pub fn non_empty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

/// `intValue(value, fallback)`: integral numbers only (doubles truncate like Dart's `num.toInt`).
pub fn int_or(value: &Value, fallback: i64) -> i64 {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_u64().map(|v| v as i64))
            .or_else(|| n.as_f64().map(|v| v as i64))
            .unwrap_or(fallback),
        _ => fallback,
    }
}

pub fn int_of(value: &Value) -> i64 {
    int_or(value, 0)
}

/// Dart `value is int`.
pub fn is_int(value: &Value) -> bool {
    value.as_i64().is_some() || value.as_u64().is_some()
}

pub fn is_true(value: &Value) -> bool {
    value.as_bool() == Some(true)
}

pub fn is_map(value: &Value) -> bool {
    value.is_object()
}

pub fn non_empty_map(value: &Value) -> bool {
    value.as_object().is_some_and(|m| !m.is_empty())
}

pub fn feature(server_info: &Value, name: &str) -> bool {
    is_true(get(get(server_info, "features"), name))
}

/// Pretty JSON for structured detail sections (never a raw editor).
pub fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// Deep structural equality, used to keep unchanged projected rows identical.
pub fn same(a: &Value, b: &Value) -> bool {
    a == b
}

pub fn obj(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    let mut map = JsonMap::new();
    for (key, value) in pairs {
        map.insert(key.to_owned(), value);
    }
    Value::Object(map)
}
