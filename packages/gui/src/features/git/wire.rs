//! Request plumbing over `CoreHandle` (original git_controller.dart
//! `gitRequest`, `checkoutPayload`, `checkoutPayloadError`).
//!
//! The core delivers domain rejections as `Ok(payload)`, which is exactly the
//! Flutter `checkoutPayload` contract; `git_result` adds the stricter
//! `gitRequest` reading on top.

use super::json::error_message;
use paseo_gui_core::{CoreError, CoreHandle, RequestId};
use serde_json::{Value, json};

/// A failed request: the user-facing message plus the daemon code.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl Failure {
    pub fn message(message: impl Into<String>) -> Self {
        Self {
            code: String::new(),
            message: message.into(),
        }
    }
}

impl From<&CoreError> for Failure {
    fn from(error: &CoreError) -> Self {
        Self {
            code: error.code.clone(),
            message: error.message.clone(),
        }
    }
}

/// Sends `type` with `fields` (an object) for `host`; the response type is
/// derived by the core (`x_request`→`x_response`, `x.request`→`x.response`).
pub fn send(core: &CoreHandle, host: &str, kind: &str, fields: Value) -> RequestId {
    let mut message = match fields {
        Value::Object(map) => Value::Object(map),
        _ => json!({}),
    };
    message["type"] = json!(kind);
    core.request(host, message, "")
}

/// `checkoutPayload`: the payload even when it carries a domain error;
/// transport and `rpc_error` failures stay failures.
pub fn checkout_result(result: &Result<Value, CoreError>) -> Result<Value, Failure> {
    match result {
        Ok(payload) => Ok(payload.clone()),
        Err(error) => Err(error.into()),
    }
}

/// `gitRequest`: a payload `error` or a rejected operation is a failure.
pub fn git_result(result: &Result<Value, CoreError>) -> Result<Value, Failure> {
    let payload = checkout_result(result)?;
    if let Some(message) = error_message(&payload) {
        return Err(Failure::message(message));
    }
    if payload.get("success") == Some(&Value::Bool(false))
        || payload.get("accepted") == Some(&Value::Bool(false))
    {
        return Err(Failure::message(super::text::t("workspace.git.toasts.notAccepted")));
    }
    Ok(payload)
}

/// `checkoutPayloadError`: the payload's error message, if any.
pub fn payload_error(payload: &Value) -> Option<String> {
    error_message(payload)
}

/// A failure's message, or `fallback` when it is blank (original
/// `message.trim().isEmpty ? failure : message`).
pub fn message_or(failure: &Failure, fallback: String) -> String {
    if failure.message.trim().is_empty() {
        fallback
    } else {
        failure.message.clone()
    }
}
