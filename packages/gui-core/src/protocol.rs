//! Request/response rules of the Flutter `PaseoClient`: response type derivation, structural
//! response validation, domain rejections and server-reported outcome uncertainty.
use serde_json::Value;

use crate::CoreError;

/// Flutter `PaseoClient._responseType`. `None` means the caller must name the response.
pub fn response_type_for(request_type: &str) -> Option<String> {
    let special = match request_type {
        "subscribe_terminals_request" => Some("terminals_changed"),
        "agent_permission_response" => Some("agent_permission_resolved"),
        "delete_agent_request" => Some("agent_deleted"),
        "archive_agent_request" => Some("agent_archived"),
        "create_agent_request"
        | "resume_agent_request"
        | "import_agent_request"
        | "refresh_agent_request"
        | "restart_server_request"
        | "shutdown_server_request" => Some("status"),
        "set_voice_mode" => Some("set_voice_mode_response"),
        "ping" => Some("pong"),
        _ => None,
    };
    if let Some(special) = special {
        return Some(special.to_owned());
    }
    for separator in ['.', '_', '/'] {
        let suffix = format!("{separator}request");
        if let Some(stem) = request_type.strip_suffix(&suffix) {
            return Some(format!("{stem}{separator}response"));
        }
    }
    None
}

/// Flutter `PaseoClient._serverOutcomeUnknown`.
pub(crate) fn server_outcome_unknown(body: &Value) -> bool {
    fn reports(value: &Value) -> bool {
        const CODES: [&str; 2] = [
            "agent_request_outcome_unknown",
            "workspace_request_outcome_unknown",
        ];
        let coded = |key: &str| value[key].as_str().is_some_and(|code| CODES.contains(&code));
        value["outcomeUnknown"] == true || coded("errorCode") || coded("code") || coded("error")
    }
    if !body.is_object() {
        return false;
    }
    reports(body)
        || ["creation", "snapshot", "error", "result"]
            .iter()
            .any(|field| body[*field].is_object() && reports(&body[*field]))
}

/// Flutter `PaseoClient._domainError`: the daemon answered but rejected the operation.
/// Payloads are delivered as `Ok` so receipts and creation snapshots stay available; this
/// converts one into the error the Flutter client would have thrown.
pub fn response_error(payload: &Value, request_type: &str) -> Option<CoreError> {
    let result = &payload["result"];
    let result_failed = result.is_object()
        && matches!(result["status"].as_str(), Some("conflict") | Some("error"));
    let rejected = !payload["error"].is_null()
        || payload["accepted"] == false
        || payload["success"] == false
        || payload["ok"] == false
        || result_failed;
    if !rejected {
        return None;
    }
    let error = &payload["error"];
    let message = error
        .as_str()
        .map(str::to_owned)
        .or_else(|| error["message"].as_str().map(str::to_owned))
        .or_else(|| {
            error
                .get("message")
                .filter(|v| !v.is_null())
                .map(|v| v.to_string())
        })
        .or_else(|| result["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("Daemon rejected {request_type}"));
    let code = text(&payload["errorCode"])
        .or_else(|| text(&payload["code"]))
        .or_else(|| {
            if result.is_object() {
                text(&result["status"])
            } else {
                None
            }
        })
        .unwrap_or_else(|| "DAEMON_RPC_ERROR".to_owned());
    Some(CoreError {
        code,
        message,
        outcome_unknown: server_outcome_unknown(payload),
    })
}

/// Dart `toString()` of a JSON scalar used as an error code.
fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// `rpc_error` correlated to a request (Flutter `_receive`).
pub(crate) fn rpc_error(body: &Value, request_type: &str) -> CoreError {
    if body["requestType"] != request_type || !body["error"].is_string() {
        return invalid_response("Malformed RPC error");
    }
    CoreError {
        code: body["code"]
            .as_str()
            .unwrap_or("DAEMON_RPC_ERROR")
            .to_owned(),
        message: body["error"].as_str().unwrap_or_default().to_owned(),
        outcome_unknown: server_outcome_unknown(body),
    }
}

pub(crate) fn invalid_response(message: impl Into<String>) -> CoreError {
    CoreError::new("DAEMON_INVALID_RESPONSE", message, true)
}

/// Flutter `PaseoClient._validateResponse`.
pub(crate) fn validate_response(
    body: &Value,
    request_type: &str,
    response_type: &str,
) -> Result<(), CoreError> {
    if !body["requestId"].is_string() {
        return Err(invalid_response("Response omitted requestId"));
    }
    let error = &body["error"];
    if !error.is_null() && !error.is_string() && !error.is_object() {
        return Err(invalid_response("Invalid response error"));
    }
    for flag in ["accepted", "success", "ok"] {
        if body.get(flag).is_some_and(|value| !value.is_boolean()) {
            return Err(invalid_response(format!("Invalid response {flag}")));
        }
    }
    if !body["subscriptionId"].is_null() && !body["subscriptionId"].is_string() {
        return Err(invalid_response("Invalid subscription identity"));
    }
    for key in [
        "entries",
        "projects",
        "agents",
        "workspaces",
        "models",
        "terminals",
    ] {
        if !body[key].is_null() && !body[key].is_array() {
            return Err(invalid_response(format!("Invalid response {key}")));
        }
    }
    if !error.is_null() {
        return Ok(());
    }
    let require = |field: &str, valid: bool| -> Result<(), CoreError> {
        if body.get(field).is_none() || !valid {
            Err(invalid_response(format!(
                "Malformed {response_type}: {field}"
            )))
        } else {
            Ok(())
        }
    };
    match response_type {
        "fetch_agents_response" | "fetch_workspaces_response" | "fetch_agent_history_response" => {
            require("entries", body["entries"].is_array())?;
            require("pageInfo", body["pageInfo"].is_object())?;
        }
        "fetch_recent_provider_sessions_response" => {
            require("entries", body["entries"].is_array())?;
        }
        "project.list.response" => require("projects", body["projects"].is_array())?,
        "fetch_agent_response" => require(
            "agent",
            body["agent"].is_null() || body["agent"].is_object(),
        )?,
        "fetch_agent_timeline_response" => {
            require("entries", body["entries"].is_array())?;
            require("epoch", body["epoch"].is_string())?;
            require("window", body["window"].is_object())?;
            for field in ["reset", "staleCursor", "gap", "hasOlder", "hasNewer"] {
                require(field, body[field].is_boolean())?;
            }
        }
        "subscribe_terminal_response" => {
            require("terminalId", body["terminalId"].is_string())?;
            require("slot", body["slot"].as_u64().is_some_and(|slot| slot <= 255))?;
        }
        "send_agent_message_response" => require("accepted", body["accepted"].is_boolean())?,
        "file.upload.response" => {
            require("file", body["file"].is_null() || body["file"].is_object())?
        }
        "hosts.route.open.response" | "hosts.link.open.response" => require(
            "routeId",
            body["routeId"].as_str().is_some_and(|id| !id.is_empty()),
        )?,
        "status" => {
            let expected = match request_type {
                "create_agent_request" => Some("agent_created"),
                "resume_agent_request" | "import_agent_request" => Some("agent_resumed"),
                "refresh_agent_request" => Some("agent_refreshed"),
                "restart_server_request" => Some("restart_requested"),
                "shutdown_server_request" => Some("shutdown_requested"),
                _ => None,
            };
            if let Some(expected) = expected {
                require("status", body["status"] == expected)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// True for wire message types that end in a response suffix (Flutter `[._/]response$`).
pub(crate) fn is_response_type(message_type: &str) -> bool {
    [".response", "_response", "/response"]
        .iter()
        .any(|suffix| message_type.ends_with(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn response_types_follow_the_flutter_derivation() {
        assert_eq!(
            response_type_for("project.list.request").as_deref(),
            Some("project.list.response")
        );
        assert_eq!(
            response_type_for("fetch_agents_request").as_deref(),
            Some("fetch_agents_response")
        );
        assert_eq!(
            response_type_for("archive_agent_request").as_deref(),
            Some("agent_archived")
        );
        assert_eq!(response_type_for("ping").as_deref(), Some("pong"));
        assert_eq!(response_type_for("schedule/create"), None);
    }

    #[test]
    fn rejections_keep_their_uncertainty() {
        let payload = json!({"requestId":"r","accepted":false,"error":"busy","creation":{"outcomeUnknown":true}});
        let error = response_error(&payload, "agent.create.request").unwrap();
        assert_eq!(error.message, "busy");
        assert!(error.outcome_unknown);
        assert!(response_error(&json!({"requestId":"r","accepted":true}), "x").is_none());
        let conflict = response_error(
            &json!({"requestId":"r","result":{"status":"conflict","error":"stale"}}),
            "fs.file.write.request",
        )
        .unwrap();
        assert_eq!(
            (conflict.code.as_str(), conflict.message.as_str()),
            ("conflict", "stale")
        );
    }

    #[test]
    fn paged_directories_require_page_info_but_recent_sessions_do_not() {
        let entries = json!({"requestId":"r","entries":[]});
        assert!(
            validate_response(&entries, "fetch_agents_request", "fetch_agents_response").is_err()
        );
        assert!(
            validate_response(
                &entries,
                "fetch_recent_provider_sessions_request",
                "fetch_recent_provider_sessions_response"
            )
            .is_ok()
        );
    }
}
