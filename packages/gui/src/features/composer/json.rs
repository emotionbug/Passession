//! Lossless JSON helpers mirroring Flutter `core/json.dart` (`jsonMap`, `jsonList`,
//! `stringValue`, `maps`). Every helper reads; none drops unknown fields of the value it
//! returns.

use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;

/// `jsonMap(value)`: the object, or an empty one.
pub fn obj(value: &Value) -> JsonMap {
    value.as_object().cloned().unwrap_or_default()
}

/// Borrowing variant of [`obj`].
pub fn obj_ref(value: &Value) -> Option<&JsonMap> {
    value.as_object()
}

/// `jsonList(value)`: the array, or an empty one.
pub fn list(value: &Value) -> Vec<Value> {
    value.as_array().cloned().unwrap_or_default()
}

pub fn list_ref(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// `maps(value)`: the array's object entries.
pub fn maps(value: &Value) -> Vec<Value> {
    list_ref(value)
        .iter()
        .filter(|item| item.is_object())
        .cloned()
        .collect()
}

/// `stringValue(value)`: the string, or empty.
pub fn s(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

/// `stringValue(value, fallback)`.
pub fn s_or<'a>(value: &'a Value, fallback: &'a str) -> &'a str {
    match value.as_str() {
        Some(text) if !text.is_empty() => text,
        _ => fallback,
    }
}

pub fn owned(value: &Value) -> String {
    s(value).to_owned()
}

/// Whether the value is a non-empty object (`jsonMap(x).isNotEmpty`).
pub fn non_empty_obj(value: &Value) -> bool {
    value.as_object().is_some_and(|map| !map.is_empty())
}

pub fn non_empty_list(value: &Value) -> bool {
    value.as_array().is_some_and(|items| !items.is_empty())
}

/// Dart `'$value'` interpolation of a scalar for display.
pub fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// `jsonEncode(a) == jsonEncode(b)` — serde_json maps keep insertion order with
/// `preserve_order`; without it key order is sorted, which only makes equal values compare
/// equal more often. Structural equality matches the intent of every call site.
pub fn same(a: &Value, b: &Value) -> bool {
    a == b
}

pub fn int(value: &Value) -> Option<i64> {
    value.as_i64()
}

pub fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// Remove `key` from an object value; no-op on other values.
pub fn remove(value: &mut Value, key: &str) {
    if let Some(map) = value.as_object_mut() {
        map.remove(key);
    }
}

/// Set `key` on an object value, turning a non-object into an object first.
pub fn set(value: &mut Value, key: &str, field: Value) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    if let Some(map) = value.as_object_mut() {
        map.insert(key.to_owned(), field);
    }
}

/// `{...a, ...b}`.
pub fn merged(base: &Value, fields: &Value) -> Value {
    let mut out = obj(base);
    if let Some(extra) = fields.as_object() {
        for (key, value) in extra {
            out.insert(key.clone(), value.clone());
        }
    }
    Value::Object(out)
}
