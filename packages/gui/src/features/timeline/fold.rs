//! Port of `chat_controller.dart` merge helpers and `_TimelineFold`
//! (types/stream.ts reduceTimelineEvent as a resumable left fold).

use super::json::*;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::rc::Rc;

/// stream.ts mergeAgentToolCallStatus: failure is sticky, a completed call is
/// never reopened, and cancellation only wins over a running call.
pub fn merge_tool_call_status(existing: &str, incoming: &str) -> &'static str {
    if existing == "failed" || incoming == "failed" {
        return "failed";
    }
    if existing == "canceled" {
        return "canceled";
    }
    if incoming == "canceled" {
        return if existing == "completed" { "completed" } else { "canceled" };
    }
    if existing == "completed" || incoming == "completed" {
        return "completed";
    }
    "running"
}

fn merge_unknown_value(existing: &Value, incoming: &Value) -> Value {
    if incoming.is_null() {
        return existing.clone();
    }
    if !non_empty_map(incoming) && non_empty_map(existing) {
        return existing.clone();
    }
    incoming.clone()
}

/// stream.ts mergeToolCallDetail: a typed detail is never replaced by an
/// `unknown` one; same-typed details merge field by field.
pub fn merge_tool_call_detail(existing: &Value, incoming: &Value) -> Value {
    let incoming_map = map(incoming);
    let existing_map = map(existing);
    if incoming_map.is_empty() {
        return Value::Object(existing_map.clone());
    }
    let from = get(existing, "type");
    let to = get(incoming, "type");
    if from == "unknown" && to != "unknown" {
        return incoming.clone();
    }
    if to == "unknown" && from != "unknown" && !existing_map.is_empty() {
        return existing.clone();
    }
    if from == "unknown" && to == "unknown" {
        return json!({
            "type": "unknown",
            "input": merge_unknown_value(get(existing, "input"), get(incoming, "input")),
            "output": merge_unknown_value(get(existing, "output"), get(incoming, "output")),
        });
    }
    if from == to {
        let mut merged = existing_map.clone();
        for (k, v) in incoming_map {
            merged.insert(k.clone(), v.clone());
        }
        return Value::Object(merged);
    }
    incoming.clone()
}

/// stream.ts mergeAgentToolCallItem: a later update for the same call.
pub fn merge_tool_call_item(existing: &Value, incoming: &Value) -> Value {
    let status = merge_tool_call_status(
        str_or(get(existing, "status"), "running"),
        str_or(get(incoming, "status"), "running"),
    );
    let mut merged = map(existing).clone();
    for (k, v) in map(incoming) {
        merged.insert(k.clone(), v.clone());
    }
    merged.insert("status".into(), Value::String(status.into()));
    let error = if status == "failed" {
        let incoming_error = get(incoming, "error");
        let existing_error = get(existing, "error");
        if !incoming_error.is_null() {
            incoming_error.clone()
        } else if !existing_error.is_null() {
            existing_error.clone()
        } else {
            json!({"message": "Tool call failed"})
        }
    } else {
        Value::Null
    };
    merged.insert("error".into(), error);
    merged.insert(
        "detail".into(),
        merge_tool_call_detail(get(existing, "detail"), get(incoming, "detail")),
    );
    let existing_metadata = get(existing, "metadata");
    let incoming_metadata = get(incoming, "metadata");
    let metadata = if !incoming_metadata.is_object() {
        existing_metadata.clone()
    } else if !existing_metadata.is_object() {
        incoming_metadata.clone()
    } else {
        let mut m = map(existing_metadata).clone();
        for (k, v) in map(incoming_metadata) {
            m.insert(k.clone(), v.clone());
        }
        Value::Object(m)
    };
    merged.insert("metadata".into(), metadata);
    Value::Object(merged)
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// stream.ts agentToolCallIdentity: a call id is scoped to its turn.
fn tool_identity(call_id: &Value, turn_id: &Value) -> String {
    let call = str_of(call_id);
    match turn_id.as_str() {
        Some(turn) if !turn.is_empty() => {
            format!("turn:{}/{}", encode_component(turn), encode_component(call))
        }
        _ => call.to_owned(),
    }
}


/// types/stream.ts reduceTimelineEvent as a resumable left fold over the
/// stored rows: live rows only ever append, so a streamed chunk folds into the
/// projection without refolding the whole history.
#[derive(Default)]
pub struct TimelineFold {
    pub entries: Vec<Rc<Value>>,
    tools: HashMap<String, usize>,
    plugins: HashMap<String, usize>,
    users: HashMap<String, usize>,
    /// How many stored rows are folded in.
    pub consumed: usize,
}

/// Consecutive chunks cover consecutive sequences: their ranges coalesce.
fn join_ranges(previous: &Value, next: &Value) -> Value {
    let mut joined: Vec<Value> = list(previous).to_vec();
    for range in list(next) {
        let start = int_of(get(range, "startSeq"));
        let end = int_of(get(range, "endSeq"));
        let extend = joined.last().is_some_and(|last| {
            start >= int_of(get(last, "startSeq")) && start <= int_of(get(last, "endSeq")) + 1
        });
        if extend {
            let last = joined.last_mut().expect("checked above");
            if end > int_of(get(last, "endSeq")) {
                *last = json!({"startSeq": get(last, "startSeq").clone(), "endSeq": end});
            }
        } else {
            joined.push(range.clone());
        }
    }
    Value::Array(joined)
}

fn grow(previous: &Value, row: &Value, item: Value) -> Value {
    let mut next = map(previous).clone();
    next.insert("item".into(), item);
    next.insert("seqEnd".into(), get(row, "seqEnd").clone());
    let timestamp = if get(row, "timestamp").is_null() {
        get(previous, "timestamp").clone()
    } else {
        get(row, "timestamp").clone()
    };
    next.insert("timestamp".into(), timestamp);
    if !get(row, "turnId").is_null() {
        next.insert("turnId".into(), get(row, "turnId").clone());
    }
    next.insert(
        "sourceSeqRanges".into(),
        join_ranges(get(previous, "sourceSeqRanges"), get(row, "sourceSeqRanges")),
    );
    Value::Object(next)
}

fn with_item(row: &Value, item: Value) -> Value {
    let mut next = map(row).clone();
    next.insert("item".into(), item);
    Value::Object(next)
}

impl TimelineFold {
    pub fn reset(&mut self) {
        self.entries.clear();
        self.tools.clear();
        self.plugins.clear();
        self.users.clear();
        self.consumed = 0;
    }

    pub fn add(&mut self, agent_id: &str, row: &Value) {
        let item = get(row, "item");
        let kind = str_of(get(item, "type"));
        let mut next_item = item.clone();
        match kind {
            "tool_call" => {
                let identity = tool_identity(get(item, "callId"), get(row, "turnId"));
                if let Some(&index) = self.tools.get(&identity) {
                    let previous = self.entries[index].clone();
                    let merged = merge_tool_call_item(get(&previous, "item"), item);
                    let mut grown = grow(&previous, row, merged);
                    let obj = grown.as_object_mut().expect("object");
                    obj.insert("turnId".into(), get(&previous, "turnId").clone());
                    if obj.get("turnId").is_some_and(Value::is_null) {
                        obj.remove("turnId");
                    }
                    let timestamp = if get(row, "timestamp").is_null() {
                        get(&previous, "timestamp").clone()
                    } else {
                        get(row, "timestamp").clone()
                    };
                    obj.insert("timestamp".into(), timestamp);
                    self.entries[index] = Rc::new(grown);
                    return;
                }
                self.tools.insert(identity, self.entries.len());
                let mut m = map(item).clone();
                let error = if get(item, "status") == "failed" {
                    get(item, "error").clone()
                } else {
                    Value::Null
                };
                m.insert("error".into(), error);
                next_item = Value::Object(m);
            }
            "plugin" => {
                let identity = format!(
                    "{}/{}",
                    value_text(get(item, "pluginId")),
                    value_text(get(item, "id"))
                );
                if let Some(&index) = self.plugins.get(&identity) {
                    let previous = self.entries[index].clone();
                    self.entries[index] = Rc::new(grow(&previous, row, item.clone()));
                    return;
                }
                self.plugins.insert(identity, self.entries.len());
            }
            "user_message" => {
                // stream.ts produceUserMessage "existing": the daemon echoes a
                // prompt without its images/attachments; the sending composer kept them.
                if let Some(local) = super::composer_bridge::sent_attachments(agent_id, str_of(get(item, "clientMessageId"))) {
                    let mut m = map(item).clone();
                    if list(get(item, "images")).is_empty() {
                        m.insert("images".into(), get(&local, "images").clone());
                    }
                    if list(get(item, "attachments")).is_empty() {
                        m.insert("attachments".into(), get(&local, "attachments").clone());
                    }
                    next_item = Value::Object(m);
                }
                let id = {
                    let client = str_of(get(item, "clientMessageId"));
                    if client.is_empty() { str_of(get(item, "messageId")) } else { client }
                }
                .to_owned();
                if !id.is_empty() {
                    if let Some(&index) = self.users.get(&id) {
                        // produceUserMessage "existing": the shown row keeps its text,
                        // time, images and attachments; the echo adds identity and seq.
                        let previous = self.entries[index].clone();
                        let shown = get(&previous, "item");
                        let mut merged_item = map(&next_item).clone();
                        merged_item.insert("text".into(), get(shown, "text").clone());
                        if !get(shown, "images").is_null() {
                            merged_item.insert("images".into(), get(shown, "images").clone());
                        }
                        if !get(shown, "attachments").is_null() {
                            merged_item.insert("attachments".into(), get(shown, "attachments").clone());
                        }
                        let mut merged = map(row).clone();
                        merged.insert("item".into(), Value::Object(merged_item));
                        let timestamp = if get(&previous, "timestamp").is_null() {
                            get(row, "timestamp").clone()
                        } else {
                            get(&previous, "timestamp").clone()
                        };
                        merged.insert("timestamp".into(), timestamp);
                        self.entries[index] = Rc::new(Value::Object(merged));
                        return;
                    }
                    self.users.insert(id, self.entries.len());
                }
            }
            "compaction" if get(item, "status") == "completed" => {
                // A completion settles the pending compaction marker.
                let pending = self.entries.iter().position(|entry| {
                    let existing = get(entry, "item");
                    get(existing, "type") == "compaction" && get(existing, "status") == "loading"
                });
                if let Some(index) = pending {
                    let previous = self.entries[index].clone();
                    let existing = get(&previous, "item");
                    let mut m = map(existing).clone();
                    m.insert("status".into(), json!("completed"));
                    let trigger = if get(item, "trigger").is_null() {
                        get(existing, "trigger").clone()
                    } else {
                        get(item, "trigger").clone()
                    };
                    m.insert("trigger".into(), trigger);
                    let pre = if get(item, "preTokens").is_null() {
                        get(existing, "preTokens").clone()
                    } else {
                        get(item, "preTokens").clone()
                    };
                    m.insert("preTokens".into(), pre);
                    self.entries[index] = Rc::new(grow(&previous, row, Value::Object(m)));
                    return;
                }
            }
            "assistant_message" | "reasoning" => {
                // normalizeChunk: carriage returns dropped; an empty chunk adds nothing.
                let chunk = str_of(get(item, "text")).replace('\r', "");
                if chunk.is_empty() {
                    return;
                }
                if let Some(previous) = self.entries.last().cloned() {
                    let old = get(&previous, "item");
                    let extend = get(old, "type") == kind
                        && (kind == "reasoning"
                            || get(item, "messageId").is_null()
                            || get(item, "messageId") == get(old, "messageId"));
                    if extend {
                        let mut m = map(old).clone();
                        let text = format!("{}{}", str_of(get(old, "text")), chunk);
                        m.insert("text".into(), Value::String(text));
                        let last = self.entries.len() - 1;
                        self.entries[last] = Rc::new(grow(&previous, row, Value::Object(m)));
                        return;
                    }
                }
                // A whitespace-only chunk never starts a message.
                if chunk.trim().is_empty() {
                    return;
                }
                let mut m = map(item).clone();
                m.insert("text".into(), Value::String(chunk));
                next_item = Value::Object(m);
            }
            _ => {}
        }
        self.entries.push(Rc::new(with_item(row, next_item)));
    }
}

/// `'${value}'` interpolation for identity keys.
fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}
