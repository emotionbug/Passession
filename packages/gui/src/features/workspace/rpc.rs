//! Request plumbing shared by the workspace, schedules, logs and command-center features.
//!
//! Ports `features/workspace/workspace_rpc.dart`, the `settingsRequest` rule from
//! `features/settings/settings_forms.dart` and the response checks of `core/client.dart`
//! (`_domainError`). Requests go through [`CoreHandle::request`]; their answers arrive as
//! [`CoreEvent::Response`] in the shell's event batch and are matched by [`Pending`].

use std::collections::HashMap;

use paseo_gui_core::{CoreError, CoreEvent, CoreHandle, RequestId};
use serde_json::{json, Map, Value};

/// A failed request as shown to the user.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    pub message: String,
    pub code: String,
    /// The host may have applied a mutation even though no answer arrived.
    pub outcome_unknown: bool,
}

impl RpcError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), code: String::new(), outcome_unknown: false }
    }

    pub fn coded(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { message: message.into(), code: code.into(), outcome_unknown: false }
    }

    fn from_core(error: &CoreError) -> Self {
        Self {
            message: error.message.clone(),
            code: error.code.clone(),
            outcome_unknown: error.outcome_unknown,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.trim().is_empty() {
            f.write_str(&self.code)
        } else {
            f.write_str(&self.message)
        }
    }
}

pub type RpcResult = Result<Value, RpcError>;

/// `workspacePayload`: the payload object of an envelope, or the message itself.
pub fn payload(message: &Value) -> &Value {
    match message.get("payload") {
        Some(inner) if inner.is_object() => inner,
        _ => message,
    }
}

/// `requireWorkspaceSuccess`: an `error`, `ok: false` or `accepted: false` rejects with the
/// host's code (error object) or text.
pub fn require_success(response: &Value) -> Result<(), RpcError> {
    let error = response.get("error").filter(|value| !value.is_null());
    let rejected = response.get("ok") == Some(&Value::Bool(false))
        || response.get("accepted") == Some(&Value::Bool(false));
    if error.is_none() && !rejected {
        return Ok(());
    }
    Err(match error {
        Some(Value::Object(map)) => {
            let code = text(map.get("code"));
            let message = text(map.get("message"));
            RpcError::coded(code.clone(), if message.is_empty() { code } else { message })
        }
        Some(Value::String(message)) => RpcError::new(message.clone()),
        Some(other) => RpcError::new(other.to_string()),
        None => RpcError::new("The host rejected the operation"),
    })
}

/// `core/client.dart _domainError` plus `settingsRequest`: the response-level failures every
/// daemon answer may carry.
pub fn domain_error(body: &Value, request_type: &str) -> Option<RpcError> {
    let result = body.get("result");
    let result_status = result.and_then(|r| r.get("status")).and_then(Value::as_str);
    let failed = body.get("error").is_some_and(|e| !e.is_null())
        || body.get("accepted") == Some(&Value::Bool(false))
        || body.get("success") == Some(&Value::Bool(false))
        || body.get("ok") == Some(&Value::Bool(false))
        || matches!(result_status, Some("conflict") | Some("error"));
    if !failed {
        return None;
    }
    let message = match body.get("error") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(map)) => map.get("message").map(value_text),
        _ => None,
    }
    .or_else(|| result.and_then(|r| r.get("error")).map(value_text))
    .unwrap_or_else(|| format!("Daemon rejected {request_type}"));
    let code = body
        .get("errorCode")
        .map(value_text)
        .or_else(|| body.get("code").map(value_text))
        .or_else(|| result_status.map(str::to_owned))
        .or_else(|| {
            body.get("error")
                .and_then(|e| e.get("code"))
                .map(value_text)
        })
        .unwrap_or_default();
    let outcome_unknown = body.get("outcomeUnknown") == Some(&Value::Bool(true));
    Some(RpcError { message, code, outcome_unknown })
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Reads a string field; missing, null and non-string values read as empty.
pub fn text(value: Option<&Value>) -> String {
    value.and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// `JsonMap.text(key, fallback)`: the string at `key`, or `fallback` when missing or empty.
pub fn str_or<'a>(value: &'a Value, key: &str, fallback: &'a str) -> &'a str {
    match value.get(key).and_then(Value::as_str) {
        Some(text) if !text.is_empty() => text,
        _ => fallback,
    }
}

pub fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

pub fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub fn int_of(value: &Value, key: &str) -> i64 {
    match value.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

pub fn array_of<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

static EMPTY_OBJECT: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| Value::Object(Map::new()));

/// `JsonMap.object(key)`: the object at `key`, or an empty object.
pub fn object_of<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(inner) if inner.is_object() => inner,
        _ => &EMPTY_OBJECT,
    }
}

pub fn empty_object() -> &'static Value {
    &EMPTY_OBJECT
}

/// Builds the wire message for [`CoreHandle::request`]: the fields plus the request `type`.
pub fn message(request_type: &str, fields: Value) -> Value {
    let mut message = match fields {
        Value::Object(map) => Value::Object(map),
        Value::Null => json!({}),
        other => json!({ "value": other }),
    };
    message["type"] = Value::String(request_type.to_owned());
    message
}

/// Issues one request. The response type follows the protocol's `_request`/`.request`
/// suffix rule unless given.
pub fn send(core: &CoreHandle, host_id: &str, request_type: &str, fields: Value) -> RequestId {
    let response = response_type(request_type);
    core.request(host_id, message(request_type, fields), &response)
}

/// `PaseoClient._responseType`.
pub fn response_type(request_type: &str) -> String {
    match request_type {
        "subscribe_terminals_request" => return "terminals_changed".into(),
        "create_agent_request" | "resume_agent_request" | "refresh_agent_request" => {
            return "status".into()
        }
        _ => {}
    }
    for separator in ['.', '_', '/'] {
        let suffix = format!("{separator}request");
        if let Some(stem) = request_type.strip_suffix(&suffix) {
            return format!("{stem}{separator}response");
        }
    }
    // `schedule/list` style requests answer with `<type>/response`.
    format!("{request_type}/response")
}

/// Turns a core response into the response body, applying the domain error checks.
pub fn settle(result: &Result<Value, CoreError>, request_type: &str) -> RpcResult {
    match result {
        Ok(value) => {
            let body = payload(value).clone();
            match domain_error(&body, request_type) {
                Some(error) => Err(error),
                None => Ok(body),
            }
        }
        Err(error) => Err(RpcError::from_core(error)),
    }
}

/// Like [`settle`] without the domain checks, for answers whose `error` field is a
/// structured state the caller interprets (project config reads and writes).
pub fn settle_raw(result: &Result<Value, CoreError>) -> RpcResult {
    match result {
        Ok(value) => Ok(payload(value).clone()),
        Err(error) => Err(RpcError::from_core(error)),
    }
}

/// In-flight requests of one widget, keyed by the core's request id.
pub struct Pending<K> {
    entries: HashMap<RequestId, (String, K)>,
}

impl<K> Default for Pending<K> {
    fn default() -> Self {
        Self { entries: HashMap::new() }
    }
}

impl<K> Pending<K> {
    /// Sends `request_type` and remembers `key` for its answer.
    pub fn send(&mut self, core: &CoreHandle, host_id: &str, request_type: &str, fields: Value, key: K) -> RequestId {
        let id = send(core, host_id, request_type, fields);
        self.entries.insert(id.clone(), (request_type.to_owned(), key));
        id
    }

    /// Sends a fully formed message with an explicit response type.
    pub fn send_typed(&mut self, core: &CoreHandle, host_id: &str, request_type: &str, fields: Value, response: &str, key: K) -> RequestId {
        let id = core.request(host_id, message(request_type, fields), response);
        self.entries.insert(id.clone(), (request_type.to_owned(), key));
        id
    }

    /// Tracks a request id produced elsewhere (for example a [`CoreHandle::operation`]).
    pub fn track(&mut self, id: RequestId, label: &str, key: K) {
        self.entries.insert(id, (label.to_owned(), key));
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn any(&self, predicate: impl Fn(&K) -> bool) -> bool {
        self.entries.values().any(|(_, key)| predicate(key))
    }

    /// Forgets every request (their late answers are ignored).
    pub fn clear(&mut self, core: &CoreHandle) {
        for id in self.entries.keys() {
            core.cancel_request(id);
        }
        self.entries.clear();
    }

    /// Forgets requests matching `predicate` without cancelling them: a submitted mutation
    /// must not be cancelled merely because its view moved on.
    pub fn forget(&mut self, predicate: impl Fn(&K) -> bool) {
        self.entries.retain(|_, (_, key)| !predicate(key));
    }

    /// Takes every answer in `events` that belongs to this widget, with domain checks.
    pub fn take(&mut self, events: &[CoreEvent]) -> Vec<(K, RpcResult)> {
        self.take_with(events, settle)
    }

    /// Like [`Pending::take`] but without the domain checks.
    pub fn take_raw(&mut self, events: &[CoreEvent]) -> Vec<(K, RpcResult)> {
        self.take_with(events, |result, _| settle_raw(result))
    }

    fn take_with(
        &mut self,
        events: &[CoreEvent],
        settle: impl Fn(&Result<Value, CoreError>, &str) -> RpcResult,
    ) -> Vec<(K, RpcResult)> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let mut answers = Vec::new();
        for event in events {
            if let CoreEvent::Response { request_id, result, .. } = event && let Some((request_type, key)) = self.entries.remove(request_id) {
                answers.push((key, settle(result, &request_type)));
            }
        }
        answers
    }
}

/// A fresh v4 identity for idempotency keys and client message ids.
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_success_rejects_error_ok_and_accepted() {
        assert!(require_success(&json!({"ok": true})).is_ok());
        assert_eq!(
            require_success(&json!({"error": {"code": "stale"}})).unwrap_err().code,
            "stale"
        );
        assert_eq!(require_success(&json!({"error": "nope"})).unwrap_err().message, "nope");
        assert!(require_success(&json!({"accepted": false})).is_err());
        assert!(require_success(&json!({"ok": false})).is_err());
        assert!(require_success(&json!({"error": null})).is_ok());
    }

    #[test]
    fn domain_error_reads_result_conflict_and_codes() {
        let error = domain_error(&json!({"result": {"status": "conflict", "error": "busy"}}), "x").unwrap();
        assert_eq!(error.message, "busy");
        assert_eq!(error.code, "conflict");
        assert!(domain_error(&json!({"success": true}), "x").is_none());
        let fallback = domain_error(&json!({"success": false}), "schedule/list").unwrap();
        assert_eq!(fallback.message, "Daemon rejected schedule/list");
    }

    #[test]
    fn response_types_follow_separator_rule() {
        assert_eq!(response_type("workspace.setup.run.request"), "workspace.setup.run.response");
        assert_eq!(response_type("checkout_status_request"), "checkout_status_response");
        assert_eq!(response_type("schedule/list"), "schedule/list/response");
        assert_eq!(response_type("schedule/run-once"), "schedule/run-once/response");
    }

    #[test]
    fn payload_unwraps_envelopes() {
        let message = json!({"type": "x", "payload": {"a": 1}});
        assert_eq!(payload(&message), &json!({"a": 1}));
        let flat = json!({"a": 1});
        assert_eq!(payload(&flat), &flat);
    }
}
