//! Port of `lib/features/chat/chat_drafts.dart`: durable client-owned drafts and queues. A
//! queue is never a daemon API. Draft JSON objects keep every field they were loaded with.
//!
//! Ownership rules that protect newer input (latest main):
//! - a late acknowledgement clears the composer only when its `editorGeneration` still equals
//!   the submitted snapshot's generation; identical retyping is a new edit;
//! - a late failure records `sendError`/`failedMessage` beside the editor and never writes the
//!   failed content over it;
//! - receipts apply monotonically by revision per message id; a dismissal is sticky.

use super::attachments::wire_attachment;
use super::json::*;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const DRAFTS_STORAGE_KEY: &str = "paseo.chat.drafts.v1";
/// A send that was in flight when the drafts were last saved (`sending` on load).
fn unconfirmed() -> String {
    super::i18n::t("composer.errors.deliveryUnconfirmed")
}

/// `ChatDrafts.key`: `jsonEncode([serverId, workspaceId, id])`.
pub fn draft_key(server_id: &str, workspace_id: &str, id: &str) -> String {
    serde_json::to_string(&json!([server_id, workspace_id, id])).unwrap_or_default()
}

/// `ChatDrafts.workspaceKey`.
pub fn workspace_key(server_id: &str, workspace_id: &str) -> String {
    serde_json::to_string(&json!([server_id, workspace_id])).unwrap_or_default()
}

/// The decoded `[serverId, workspaceId, id]` identity of a draft key.
pub fn key_identity(key: &str) -> Vec<String> {
    serde_json::from_str::<Value>(key)
        .ok()
        .map(|value| list_ref(&value).iter().map(display).collect())
        .unwrap_or_default()
}

/// The host whose delivery owns `key`; the local creation composer ('draft') has none.
pub fn delivery_server(key: &str) -> Option<String> {
    let identity = key_identity(key);
    if identity.len() != 3 || identity[2] == "draft" {
        return None;
    }
    Some(identity[0].clone())
}

fn default_draft() -> Value {
    json!({"text": "", "attachments": [], "images": [], "queue": [], "config": {}})
}

/// receipt is accepted, or submitted with acknowledgement no longer pending.
pub fn receipt_resolved(receipt: &Value) -> bool {
    let phase = s(get(receipt, "phase"));
    phase == "accepted"
        || (phase == "submitted"
            && get(get(receipt, "correlation"), "acknowledgementPending") == &Value::Bool(false))
}

pub fn receipt_can_retry(receipt: &Value) -> bool {
    s(get(receipt, "phase")) == "not_delivered"
        && get(receipt, "canRetry") == &Value::Bool(true)
        && get(receipt, "dismissed") != &Value::Bool(true)
}

/// The behavior is part of the daemon's durable request fingerprint. Once an attempt has
/// begun, even an omitted behavior must be replayed unchanged.
pub fn queued_message_for_delivery(message: &Value, active_turn_behavior: &str) -> Value {
    let attempted = get(message, "deliveryAttempted") == &Value::Bool(true)
        || matches!(s(get(message, "state")), "sending" | "unacknowledged" | "rejected");
    let mut out = obj(message);
    if !attempted {
        out.insert("activeTurnBehavior".into(), json!(active_turn_behavior));
    }
    out.insert("deliveryAttempted".into(), json!(true));
    Value::Object(out)
}

/// Wire fields of `send_agent_message_request` without images. Images go through
/// `ops::wire_images` (retained blobs are read and encoded off the GUI thread).
pub fn agent_message_fields(agent_id: &str, message: &Value) -> Value {
    let mut out = Map::new();
    out.insert("agentId".into(), json!(agent_id));
    out.insert("text".into(), get(message, "text").clone());
    out.insert("messageId".into(), get(message, "messageId").clone());
    if let Some(behavior) = message.get("activeTurnBehavior").filter(|v| !v.is_null()) {
        out.insert("activeTurnBehavior".into(), behavior.clone());
    }
    out.insert(
        "attachments".into(),
        Value::Array(maps(get(message, "attachments")).iter().map(wire_attachment).collect()),
    );
    Value::Object(out)
}

#[derive(Default)]
pub struct DraftStore {
    drafts: HashMap<String, Value>,
    inbox: BTreeMap<String, Vec<Value>>,
    intents: Vec<String>,
    dirty_drafts: HashSet<String>,
    loaded: bool,
    /// Bumped on every change; the persisted revision trails it.
    revision: u64,
    saved_revision: u64,
    /// Bumped whenever some draft's pending/queue changes (timeline outgoing rows).
    outgoing_revision: u64,
    pub persistence_error: Option<String>,
    /// Draft keys whose editor-visible fields changed since the last drain.
    changed: HashSet<String>,
    /// Hosts whose delivery-relevant state changed since the last drain.
    delivery_changed: HashSet<String>,
    /// Workspace inbox keys that received attachments since the last drain.
    inbox_changed: HashSet<String>,
}

impl DraftStore {
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn outgoing_revision(&self) -> u64 {
        self.outgoing_revision
    }

    /// Agent draft keys owned by `server_id`'s delivery. Creation drafts (`'draft'` ids, or
    /// drafts marked `draft: true` by a creation composer with a generated tab id) have no
    /// daemon agent or message receipts.
    pub fn agent_keys_for_server(&self, server_id: &str) -> Vec<String> {
        self.drafts
            .iter()
            .filter(|(key, draft)| {
                delivery_server(key).as_deref() == Some(server_id) && get(draft, "draft") != &Value::Bool(true)
            })
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// Load the persisted payload (`{drafts, inbox, intents}`, object or JSON string). Editor
    /// edits made before stored drafts load take precedence over them.
    pub fn load(&mut self, saved: &Value) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let parsed;
        let saved = match saved {
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(value) => {
                    parsed = value;
                    &parsed
                }
                Err(error) => {
                    self.persistence_error = Some(error.to_string());
                    return;
                }
            },
            other => other,
        };
        if let Some(drafts) = get(saved, "drafts").as_object() {
            for (key, value) in drafts {
                if self.dirty_drafts.contains(key) {
                    continue;
                }
                let mut value = Value::Object(obj(value));
                let queue: Vec<Value> = list_ref(get(&value, "queue"))
                    .iter()
                    .map(|item| {
                        let mut queued = obj(item);
                        if queued.get("state").and_then(Value::as_str) == Some("sending") {
                            queued.insert("state".into(), json!("unacknowledged"));
                            queued.insert("error".into(), json!(unconfirmed()));
                        }
                        Value::Object(queued)
                    })
                    .collect();
                set(&mut value, "queue", Value::Array(queue));
                self.drafts.insert(key.clone(), value);
                self.changed.insert(key.clone());
                if let Some(server) = delivery_server(key) {
                    self.delivery_changed.insert(server);
                }
            }
        }
        if let Some(inbox) = get(saved, "inbox").as_object() {
            for (key, items) in inbox {
                let current = self.inbox.entry(key.clone()).or_default();
                for item in list_ref(items) {
                    let item = Value::Object(obj(item));
                    if !current.contains(&item) {
                        current.push(item);
                    }
                }
                self.inbox_changed.insert(key.clone());
            }
        }
        for intent in list_ref(get(saved, "intents")) {
            if let Some(intent) = intent.as_str()
                && !self.intents.iter().any(|i| i == intent)
            {
                self.intents.push(intent.to_owned());
            }
        }
        self.outgoing_revision += 1;
    }

    /// `draft(key)`: the draft, created with defaults when absent.
    pub fn draft(&mut self, key: &str) -> &Value {
        self.drafts.entry(key.to_owned()).or_insert_with(default_draft)
    }

    fn draft_mut(&mut self, key: &str) -> &mut Value {
        self.drafts.entry(key.to_owned()).or_insert_with(default_draft)
    }

    /// Read-only view; None when the draft was never touched.
    pub fn peek(&self, key: &str) -> Option<&Value> {
        self.drafts.get(key)
    }

    pub fn field(&mut self, key: &str, field: &str) -> Value {
        get(self.draft(key), field).clone()
    }

    pub fn update(&mut self, key: &str, fields: Value) {
        self.dirty_drafts.insert(key.to_owned());
        let editor = ["text", "attachments", "images"].iter().any(|f| fields.get(*f).is_some());
        let delivery = ["queue", "pending", "receipts", "dismissedMessageIds"]
            .iter()
            .any(|f| fields.get(*f).is_some());
        let current = self.draft_mut(key);
        if editor {
            let generation = get(current, "editorGeneration").as_i64().unwrap_or(0) + 1;
            set(current, "editorGeneration", json!(generation));
        }
        if let (Some(target), Some(source)) = (current.as_object_mut(), fields.as_object()) {
            for (field, value) in source {
                target.insert(field.clone(), value.clone());
            }
        }
        self.save(Some(key), delivery);
    }

    pub fn add_attachments(&mut self, server_id: &str, workspace_id: &str, attachments: Vec<Value>, intent_id: Option<&str>) {
        if let Some(intent) = intent_id {
            if self.intents.iter().any(|i| i == intent) {
                return;
            }
            self.intents.push(intent.to_owned());
        }
        let key = workspace_key(server_id, workspace_id);
        let inbox = self.inbox.entry(key.clone()).or_default();
        for attachment in attachments {
            if !inbox.contains(&attachment) {
                inbox.push(attachment);
            }
        }
        self.inbox_changed.insert(key);
        self.save(None, false);
    }

    pub fn add_draft_attachments(&mut self, key: &str, attachments: Vec<Value>, intent_id: Option<&str>) {
        if let Some(intent) = intent_id {
            if self.intents.iter().any(|i| i == intent) {
                return;
            }
            self.intents.push(intent.to_owned());
        }
        let mut current = maps(get(self.draft(key), "attachments"));
        for attachment in attachments {
            if !current.contains(&attachment) {
                current.push(attachment);
            }
        }
        self.update(key, json!({"attachments": current}));
    }

    pub fn take_attachments(&mut self, server_id: &str, workspace_id: &str, draft_key: &str) {
        let Some(pending) = self.inbox.remove(&workspace_key(server_id, workspace_id)) else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        let mut current = maps(get(self.draft(draft_key), "attachments"));
        for attachment in pending {
            if !current.contains(&attachment) {
                current.push(attachment);
            }
        }
        self.update(draft_key, json!({"attachments": current}));
    }

    pub fn inbox_pending(&self, server_id: &str, workspace_id: &str) -> bool {
        self.inbox
            .get(&workspace_key(server_id, workspace_id))
            .is_some_and(|items| !items.is_empty())
    }

    pub fn owns_pending(&mut self, key: &str, submitted: &Value) -> bool {
        let id = owned(get(submitted, "messageId"));
        get(get(self.draft(key), "pending"), "messageId") == get(submitted, "messageId")
            && !self.is_dismissed(key, &id)
    }

    pub fn is_dismissed(&self, key: &str, id: &str) -> bool {
        self.drafts
            .get(key)
            .is_some_and(|draft| list_ref(get(draft, "dismissedMessageIds")).iter().any(|v| v.as_str() == Some(id)))
    }

    pub fn acknowledge(&mut self, key: &str, submitted: &Value) {
        let id = owned(get(submitted, "messageId"));
        if self.is_dismissed(key, &id) {
            return;
        }
        let current = self.draft_mut(key);
        let pending = get(current, "pending").clone();
        if non_empty_obj(&pending) && s(get(&pending, "messageId")) != id {
            return;
        }
        // Equality of content is not ownership: identical retyping is a new edit.
        let submitted_generation = get(submitted, "editorGeneration");
        if !submitted_generation.is_null() && get(current, "editorGeneration") == submitted_generation {
            let generation = get(current, "editorGeneration").as_i64().unwrap_or(0) + 1;
            set(current, "text", json!(""));
            set(current, "attachments", json!([]));
            set(current, "images", json!([]));
            set(current, "editorGeneration", json!(generation));
        }
        if s(get(&pending, "messageId")) == id && !id.is_empty() {
            remove(current, "pending");
        }
        if s(get(current, "sendErrorMessageId")) == id && !id.is_empty() {
            remove(current, "sendError");
            remove(current, "sendErrorMessageId");
        }
        self.save(Some(key), true);
    }

    /// A local tombstone also guards legacy hosts, but never claims remote cancel.
    pub fn dismiss(&mut self, key: &str, id: &str) {
        let current = self.draft_mut(key);
        let mut dismissed: Vec<Value> = list_ref(get(current, "dismissedMessageIds"))
            .iter()
            .filter(|v| v.is_string())
            .cloned()
            .collect();
        if !dismissed.iter().any(|v| v.as_str() == Some(id)) {
            dismissed.push(json!(id));
        }
        set(current, "dismissedMessageIds", Value::Array(dismissed));
        if s(get(get(current, "pending"), "messageId")) == id {
            remove(current, "pending");
        }
        let queue: Vec<Value> = maps(get(current, "queue"))
            .into_iter()
            .filter(|item| s(get(item, "messageId")) != id)
            .collect();
        set(current, "queue", Value::Array(queue));
        if s(get(current, "sendErrorMessageId")) == id {
            remove(current, "sendError");
            remove(current, "sendErrorMessageId");
        }
        self.save(Some(key), true);
    }

    /// Monotonic exact-ID delivery evidence. A canonical user row is not a receipt.
    pub fn apply_receipt(&mut self, key: &str, receipt: &Value) -> bool {
        let identity = key_identity(key);
        let id = owned(get(receipt, "messageId"));
        let Some(revision) = get(receipt, "revision").as_i64() else { return false };
        if id.is_empty() || identity.get(2).map(String::as_str) != get(receipt, "agentId").as_str() {
            return false;
        }
        let dismissed_locally = self.is_dismissed(key, &id);
        let current = self.draft_mut(key);
        let mut receipts = obj(get(current, "receipts"));
        let previous = receipts.get(&id).cloned().unwrap_or(Value::Null);
        if get(&previous, "revision").as_i64().unwrap_or(-1) >= revision {
            return false;
        }
        let sticky = dismissed_locally || get(&previous, "dismissed") == &Value::Bool(true);
        let mut stored = obj(receipt);
        if sticky {
            stored.insert("dismissed".into(), json!(true));
        }
        receipts.insert(id.clone(), Value::Object(stored));
        set(current, "receipts", Value::Object(receipts));
        if sticky || get(receipt, "dismissed") == &Value::Bool(true) {
            self.dismiss(key, &id);
            return true;
        }
        if receipt_resolved(receipt) {
            let pending = get(self.draft(key), "pending").clone();
            if s(get(&pending, "messageId")) == id {
                self.acknowledge(key, &pending);
            }
            let current = self.draft_mut(key);
            let queue: Vec<Value> = maps(get(current, "queue"))
                .into_iter()
                .filter(|item| s(get(item, "messageId")) != id)
                .collect();
            set(current, "queue", Value::Array(queue));
        } else if matches!(s(get(receipt, "phase")), "rejected" | "not_delivered") {
            let current = self.draft_mut(key);
            let pending = get(current, "pending").clone();
            if s(get(&pending, "messageId")) == id {
                set(current, "failedMessage", pending);
                remove(current, "pending");
                set(current, "sendError", get(receipt, "error").clone());
                set(current, "sendErrorMessageId", json!(id));
            }
            let queue: Vec<Value> = maps(get(current, "queue"))
                .into_iter()
                .map(|item| {
                    if s(get(&item, "messageId")) == id {
                        merged(&item, &json!({"state": "rejected", "error": get(receipt, "error").clone()}))
                    } else {
                        item
                    }
                })
                .collect();
            set(current, "queue", Value::Array(queue));
        }
        self.save(Some(key), true);
        true
    }

    /// Record a failed send of `submitted` beside the editor; never overwrites newer editor
    /// content. `rejected` (definite) moves the message to `failedMessage`.
    pub fn fail_send(&mut self, key: &str, submitted: &Value, error: &str, rejected: bool) -> bool {
        if !self.owns_pending(key, submitted) {
            return false;
        }
        let id = owned(get(submitted, "messageId"));
        let current = self.draft_mut(key);
        let receipt = get(get(current, "receipts"), &id).clone();
        if receipt_resolved(&receipt) || get(&receipt, "dismissed") == &Value::Bool(true) {
            return false;
        }
        set(current, "sendError", json!(error));
        set(current, "sendErrorMessageId", json!(id));
        if rejected && !non_empty_obj(&receipt) {
            set(current, "failedMessage", submitted.clone());
            remove(current, "pending");
        }
        self.save(Some(key), true);
        true
    }

    /// The message a submit sends: the composer text trimmed and the attachments.
    pub fn snapshot(&mut self, key: &str) -> Value {
        let draft = self.draft(key);
        json!({
            "messageId": uuid::Uuid::new_v4().to_string(),
            "editorGeneration": get(draft, "editorGeneration").as_i64().unwrap_or(0),
            "text": s(get(draft, "text")).trim(),
            "attachments": list(get(draft, "attachments")),
            "images": list(get(draft, "images")),
        })
    }

    /// Commit only against the snapshot shown by the editor. Returns whether the queue entry
    /// itself was edited; otherwise the edited content lands in the composer for review.
    pub fn edit_queued_message(&mut self, key: &str, original: &Value, text: &str) -> bool {
        let current = self.draft(key).clone();
        let mut queue = maps(get(&current, "queue"));
        let index = queue.iter().position(|item| get(item, "messageId") == get(original, "messageId"));
        if let Some(index) = index {
            let state = s(get(&queue[index], "state"));
            if matches!(state, "queued" | "blocked" | "rejected") && same(&queue[index], original) {
                let mut edited = obj(&queue[index]);
                edited.insert("text".into(), json!(text));
                edited.insert("state".into(), json!("queued"));
                edited.insert("error".into(), Value::Null);
                if s(get(original, "state")) == "rejected" {
                    // An explicit edit of a definitely rejected request is a new message.
                    edited.insert("messageId".into(), json!(uuid::Uuid::new_v4().to_string()));
                    edited.remove("deliveryAttempted");
                    edited.remove("activeTurnBehavior");
                }
                queue[index] = Value::Object(edited);
                self.update(key, json!({"queue": queue}));
                return true;
            }
        }
        let mut parts = Vec::new();
        let existing = s(get(&current, "text"));
        if !existing.is_empty() {
            parts.push(existing.to_owned());
        }
        if !text.is_empty() {
            parts.push(text.to_owned());
        }
        let mut fields = Map::new();
        fields.insert("text".into(), json!(parts.join("\n\n")));
        for field in ["attachments", "images"] {
            let mut values = list(get(&current, field));
            for value in list_ref(get(original, field)) {
                if !values.contains(value) {
                    values.push(value.clone());
                }
            }
            fields.insert(field.into(), Value::Array(values));
        }
        self.update(key, Value::Object(fields));
        false
    }

    fn save(&mut self, key: Option<&str>, delivery: bool) {
        self.revision += 1;
        if let Some(key) = key {
            self.changed.insert(key.to_owned());
            if delivery {
                self.outgoing_revision += 1;
                if let Some(server) = delivery_server(key) {
                    self.delivery_changed.insert(server);
                }
            }
        }
    }

    /// The payload to persist when unsaved changes exist (`{drafts, inbox, intents}`).
    pub fn take_save(&mut self) -> Option<Value> {
        if !self.loaded || self.saved_revision == self.revision {
            return None;
        }
        self.saved_revision = self.revision;
        let drafts: Map<String, Value> = self.drafts.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let inbox: Map<String, Value> = self
            .inbox
            .iter()
            .map(|(k, v)| (k.clone(), Value::Array(v.clone())))
            .collect();
        Some(json!({"drafts": drafts, "inbox": inbox, "intents": self.intents}))
    }

    /// Force the next `take_save` to produce a payload (a flush needs its own durable write).
    pub fn mark_dirty(&mut self) {
        self.revision += 1;
    }

    /// Whether every change up to now has been handed to persistence.
    pub fn flushed(&self) -> bool {
        self.saved_revision == self.revision
    }

    pub fn drain_changed(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.changed)
    }

    pub fn drain_delivery_changed(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.delivery_changed)
    }

    pub fn drain_inbox_changed(&mut self) -> HashSet<String> {
        std::mem::take(&mut self.inbox_changed)
    }

    /// Every retained image blob referenced by any draft, pending, queued or failed message.
    pub fn referenced_blobs(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut collect = |images: &Value| {
            for image in list_ref(images) {
                if let Some(id) = image.get("blobId").and_then(Value::as_str) {
                    out.insert(id.to_owned());
                }
            }
        };
        for draft in self.drafts.values() {
            collect(get(draft, "images"));
            collect(get(get(draft, "pending"), "images"));
            collect(get(get(draft, "failedMessage"), "images"));
            for item in list_ref(get(draft, "queue")) {
                collect(get(item, "images"));
            }
        }
        out
    }
}
