//! OMP's fire-and-forget RPC chrome. The state belongs to one live process,
//! not to its turn, native transcript, or a pending extension dialog.
use serde_json::{Value, json};

pub(crate) const EVENT: &str = "omp_extension_ui";
pub(crate) const SNAPSHOT_KEY: &str = "_ompExtensionUi";
const MAX_KEYS: usize = 16;
const MAX_KEY_BYTES: usize = 256;
const MAX_LINE_BYTES: usize = 512;
const MAX_WIDGET_LINES: usize = 10;

pub(super) fn patch(frame: &Value) -> Option<Value> {
    let (slot, key, value) = match frame["method"].as_str()? {
        "setStatus" => (
            "statuses",
            key(&frame["statusKey"])?,
            match &frame["statusText"] {
                Value::Null => Value::Null,
                Value::String(text) => Value::String(plain_line(text)),
                _ => return None,
            },
        ),
        "setWidget" => (
            "widgets",
            key(&frame["widgetKey"])?,
            match &frame["widgetLines"] {
                Value::Null => Value::Null,
                Value::Array(lines) => {
                    let placement = match &frame["widgetPlacement"] {
                        Value::Null => "aboveEditor",
                        Value::String(value)
                            if matches!(value.as_str(), "aboveEditor" | "belowEditor") =>
                        {
                            value
                        }
                        _ => return None,
                    };
                    let mut plain = lines
                        .iter()
                        .take(MAX_WIDGET_LINES)
                        .map(|line| line.as_str().map(|s| Value::String(plain_line(s))))
                        .collect::<Option<Vec<_>>>()?;
                    if lines.len() > MAX_WIDGET_LINES {
                        plain.push(json!("…"));
                    }
                    json!({"lines":plain,"placement":placement})
                }
                _ => return None,
            },
        ),
        _ => return None,
    };
    let mut event = json!({"type":EVENT,"slot":slot,"key":key});
    event["value"] = value;
    Some(event)
}

fn key(value: &Value) -> Option<&str> {
    value.as_str().filter(|key| key.len() <= MAX_KEY_BYTES)
}

pub(super) fn replace(state: Value) -> Value {
    let mut event = json!({"type":EVENT});
    event["replacement"] = state;
    event
}

/// Apply a normalized patch, leaving the other namespace and keys untouched.
pub(crate) fn apply(state: &mut Value, mut event: Value) -> bool {
    if let Some(replacement) = event.get_mut("replacement") {
        if (!replacement.is_null() && !replacement.is_object()) || *state == *replacement {
            return false;
        }
        *state = replacement.take();
        return true;
    }
    let Some(value) = event.get_mut("value").map(Value::take) else {
        return false;
    };
    let (Some(slot @ ("statuses" | "widgets")), Some(key)) =
        (event["slot"].as_str(), event["key"].as_str())
    else {
        return false;
    };
    if value.is_null() {
        let Some(slots) = state.as_object_mut() else {
            return false;
        };
        let Some(entries) = slots.get_mut(slot).and_then(Value::as_object_mut) else {
            return false;
        };
        if entries.remove(key).is_none() {
            return false;
        }
        if entries.is_empty() {
            slots.remove(slot);
        }
        if slots.is_empty() {
            *state = Value::Null;
        }
        return true;
    }
    if state[slot].is_null() {
        state[slot] = json!({});
    }
    let Some(entries) = state[slot].as_object_mut() else {
        return false;
    };
    let full = entries.len() >= MAX_KEYS;
    match entries.get_mut(key) {
        Some(previous) if *previous == value => false,
        Some(previous) => {
            *previous = value;
            true
        }
        None if full => false,
        None => {
            entries.insert(key.to_owned(), value);
            true
        }
    }
}

struct PlainLine(String);

impl vte::Perform for PlainLine {
    fn print(&mut self, ch: char) {
        if !ch.is_control() && self.0.len() + ch.len_utf8() <= MAX_LINE_BYTES {
            self.0.push(ch);
        }
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.print(' ');
        }
    }
}

fn plain_line(text: &str) -> String {
    let mut line = PlainLine(String::with_capacity(text.len().min(MAX_LINE_BYTES)));
    // Bound parser work and temporary captures of long terminal sequences too.
    let raw = &text[..text.floor_char_boundary(MAX_LINE_BYTES * 8)];
    vte::Parser::new().advance(&mut line, raw.as_bytes());
    line.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::pi_omp::Mapper;

    #[test]
    fn omp_extension_ui_maps_only_recognized_fire_and_forget_frames() {
        let mut omp = Mapper::new("omp", "custom-omp");
        let mut pi = Mapper::new("pi", "pi");
        let status = json!({"type":"extension_ui_request","id":"rpc-1","method":"setStatus",
            "statusKey":"ext","statusText":"\u{1b}[32mReady\u{1b}[0m"});
        let widget = json!({"type":"extension_ui_request","id":"rpc-2","method":"setWidget",
            "widgetKey":"ext","widgetLines":["one","two"]});
        assert_eq!(
            omp.map(&status),
            vec![json!({"type":EVENT,"slot":"statuses","key":"ext","value":"Ready"})]
        );
        assert_eq!(
            omp.map(&widget),
            vec![json!({"type":EVENT,"slot":"widgets","key":"ext",
                "value":{"lines":["one","two"],"placement":"aboveEditor"}})]
        );
        assert!(omp.drain_responses().is_empty());
        assert!(omp.pending_permissions().is_empty());
        assert!(pi.map(&status).is_empty());
        assert!(pi.map(&widget).is_empty());
        assert!(
            patch(&json!({"method":"setStatus","statusKey":"ext","statusText":42})).is_none()
        );
        assert!(
            patch(&json!({"method":"setWidget","widgetKey":"ext","widgetLines":[true]})).is_none()
        );
    }
}
