//! Durable daemon update rules from `daemon_update.dart` and
//! `core/daemon_maintenance.dart`.

use serde_json::Value;

use crate::features::settings::json::{get, str_of};

pub const DURABLE_FEATURE: &str = "durableDaemonUpdates";

pub fn supports_durable(server_info: &Value) -> bool {
    crate::features::settings::json::feature(server_info, DURABLE_FEATURE)
}

/// The operation's own result; a session resume never changes it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Running,
    Committed,
    RolledBack,
    Failed,
}

pub fn outcome(operation: &Value) -> Outcome {
    if operation.get("terminal").and_then(Value::as_bool) != Some(true) {
        return Outcome::Running;
    }
    match str_of(operation, "phase") {
        "committed" => Outcome::Committed,
        "rolled_back" => Outcome::RolledBack,
        _ => Outcome::Failed,
    }
}

/// `continuation.state` when a session resume was requested and is not accepted.
pub fn unconfirmed_continuation(operation: &Value) -> Option<String> {
    let continuation = operation.get("continuation")?;
    if !continuation.is_object() {
        return None;
    }
    let state = match str_of(continuation, "state") {
        "" => "unknown",
        other => other,
    };
    if state == "accepted" { None } else { Some(state.to_string()) }
}

/// Whether to keep the operation id to inspect it again (never resubmits).
pub fn retain_operation(operation: &Value) -> bool {
    if operation.get("terminal").and_then(Value::as_bool) != Some(true) {
        return true;
    }
    if outcome(operation) != Outcome::Committed {
        return false;
    }
    matches!(unconfirmed_continuation(operation).as_deref(), Some("pending") | Some("unknown"))
}

/// Locale key suffix under `settings.host.daemon.update`.
pub fn phase_key(phase: &str) -> &'static str {
    match phase {
        "admitted" => "phaseStarting",
        "preparing" => "phaseDownloading",
        "staged" => "phaseInstalling",
        "draining" => "phaseDraining",
        "activating" => "phaseActivating",
        "verifying" => "phaseVerifying",
        "rolling_back" | "rolled_back" => "phaseRollingBack",
        "committed" => "phaseComplete",
        _ => "phaseInstalling",
    }
}

pub fn new_operation_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `operation` from a submit/status response, refusing another operation.
pub fn operation_of(response: &Value, operation_id: &str) -> Result<Value, String> {
    let operation = get(response, "operation").clone();
    if str_of(&operation, "operationId") != operation_id {
        return Err(crate::features::settings::i18n::t("settingsNative.otherOperation"));
    }
    Ok(operation)
}

/// `"v1.2.3"` → `"1.2.3"`; empty → `None`.
pub fn normalize_version(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn committed_with_pending_resume_is_retained_but_rollback_is_not() {
        let committed = json!({"terminal": true, "phase": "committed", "continuation": {"state": "pending"}});
        assert_eq!(outcome(&committed), Outcome::Committed);
        assert!(retain_operation(&committed));
        assert!(!retain_operation(&json!({"terminal": true, "phase": "committed"})));
        assert!(!retain_operation(&json!({"terminal": true, "phase": "rolled_back"})));
        assert!(retain_operation(&json!({"terminal": false, "phase": "draining"})));
        assert_eq!(unconfirmed_continuation(&json!({"continuation": {}})).as_deref(), Some("unknown"));
        assert_eq!(unconfirmed_continuation(&json!({"continuation": {"state": "accepted"}})), None);
    }
}
