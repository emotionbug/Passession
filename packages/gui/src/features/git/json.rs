//! Lossless accessors over wire JSON, matching the Dart `JsonMap` helpers the
//! Flutter git feature used (`text`, `integer`, `flag`, `array`, `object`).

use serde_json::Value;

/// `map.text(key, fallback)`: a string field, else the fallback.
pub fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn text_or<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or(fallback)
}

/// `map.integer(key)`: numbers truncate toward zero; anything else is 0.
pub fn int(value: &Value, key: &str) -> i64 {
    match value.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

/// A numeric field or `None` when absent or not a number (`value is num ? … : null`).
pub fn opt_int(value: &Value, key: &str) -> Option<i64> {
    match value.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        _ => None,
    }
}

/// `map.flag(key)`: only a literal `true` is true.
pub fn flag(value: &Value, key: &str) -> bool {
    value.get(key) == Some(&Value::Bool(true))
}

/// `map.flag(key, true)`: absent or non-boolean values use the fallback.
pub fn flag_or(value: &Value, key: &str, fallback: bool) -> bool {
    match value.get(key) {
        Some(Value::Bool(b)) => *b,
        _ => fallback,
    }
}

pub fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

static NULL: Value = Value::Null;

/// `map.object(key)`: an object field, else `Null` (every accessor treats it as empty).
pub fn object<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(found @ Value::Object(_)) => found,
        _ => &NULL,
    }
}

pub fn is_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

/// The message of an `error` field: `error.message` for objects, the string
/// itself, or the JSON text of anything else. `None` when absent or null.
pub fn error_message(payload: &Value) -> Option<String> {
    match payload.get("error") {
        None | Some(Value::Null) => None,
        Some(Value::String(message)) => Some(message.clone()),
        Some(error @ Value::Object(_)) => Some(
            error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
        ),
        Some(other) => Some(other.to_string()),
    }
}
