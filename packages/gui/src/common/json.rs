//! Flutter `core/json.dart` field readers: wrong-typed or missing values read as the
//! fallback, never panic.

use serde_json::Value;

static NULL: Value = Value::Null;
static EMPTY_ARRAY: Vec<Value> = Vec::new();

pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn text_or<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(fallback)
}

/// `jsonMap(value[key])`: the object, or `Null` for anything else.
pub fn object<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(v @ Value::Object(_)) => v,
        _ => &NULL,
    }
}

pub fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    match value.get(key) {
        Some(Value::Array(items)) => items,
        _ => &EMPTY_ARRAY,
    }
}

pub fn int(value: &Value, key: &str, fallback: i64) -> i64 {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(fallback),
        _ => fallback,
    }
}

pub fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
}

pub fn boolean(value: &Value, key: &str, fallback: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

pub fn is_null_or_missing(value: &Value, key: &str) -> bool {
    matches!(value.get(key), None | Some(Value::Null))
}

/// Agent directory rows are either the agent object or `{agent: {...}}` (Flutter
/// `row.object('agent').isEmpty ? row : row.object('agent')`).
pub fn agent_of(row: &Value) -> &Value {
    match row.get("agent") {
        Some(agent @ Value::Object(map)) if !map.is_empty() => agent,
        _ => row,
    }
}
