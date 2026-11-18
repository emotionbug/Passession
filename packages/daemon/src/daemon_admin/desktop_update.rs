//! Forward a remote approval to this daemon home's desktop updater. The daemon
//! never chooses an executable, release source, channel or installation path.
use super::rpc_error;
use crate::transport::RequestContext;
use anyhow::{Result, ensure};
use paseo_client::ClientError;
use paseo_client::desktop_app_bridge::{self, Method};
use paseo_protocol::wire::{Direction, validate_session};
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn method(kind: &str) -> Option<Method> {
    match kind {
        "desktop.app.update.check.request" => Some(Method::Check),
        "desktop.app.update.submit.request" => Some(Method::Submit),
        "desktop.app.update.status.request" => Some(Method::Status),
        _ => None,
    }
}

pub(super) async fn respond(
    home: &Path,
    context: &RequestContext,
    request: &str,
    kind: &str,
    method: Method,
    message: &Value,
) -> Result<()> {
    if validate_session(message, Direction::Inbound).is_err() {
        let error =
            ClientError::new("INVALID_REQUEST", "Invalid desktop app update request.").into();
        return rpc_error(context, request, kind, &error).await;
    }
    let (response, field, args) = match method {
        Method::Check => ("desktop.app.update.check.response", "check", json!({})),
        Method::Submit => (
            "desktop.app.update.submit.response",
            "operation",
            json!({"operationId":message["operationId"], "expectedRelease":message["expectedRelease"],
                "selectedComponents":message["selectedComponents"]}),
        ),
        Method::Status => (
            "desktop.app.update.status.response",
            "operation",
            json!({"operationId":message["operationId"]}),
        ),
    };
    let result = desktop_app_bridge::request(home, method, &args)
        .await
        .and_then(|value| project(value, args["operationId"].as_str()));
    match result {
        Ok(public) => {
            let reply = json!({"type":response,"payload":{"requestId":request,(field):public}});
            if let Err(error) = validate_session(&reply, Direction::Outbound) {
                return rpc_error(context, request, kind, &public_error(&error.into())).await;
            }
            context.send(reply).await
        }
        Err(error) => rpc_error(context, request, kind, &public_error(&error)).await,
    }
}

// Local errors can include filesystem paths and helper diagnostics. Never put
// their cause chain on the wire, including when a journal contains the error.
fn public_error(error: &anyhow::Error) -> anyhow::Error {
    let code = error.downcast_ref::<ClientError>().map(|e| e.code.as_str());
    let (code, text) = match code {
        Some("DESKTOP_APP_UNAVAILABLE") => (
            "DESKTOP_APP_UNAVAILABLE",
            "Open an updated Passession desktop app on this host, then refresh.",
        ),
        Some("DESKTOP_APP_OPERATION_NOT_FOUND") => (
            "DESKTOP_APP_OPERATION_NOT_FOUND",
            "This desktop app has no record of that update operation.",
        ),
        // The request reached the app; it may have started. Callers must
        // observe status for the same operation instead of resubmitting.
        Some("DESKTOP_APP_OUTCOME_UNKNOWN") => (
            "DESKTOP_APP_OUTCOME_UNKNOWN",
            "The desktop app stopped responding after receiving the request. Its update status will show whether it started.",
        ),
        _ => (
            "DESKTOP_APP_UPDATE_FAILED",
            "The desktop app could not complete this request. Check its update status on the host PC.",
        ),
    };
    ClientError::new(code, text).into()
}

fn project(mut value: Value, operation_id: Option<&str>) -> Result<Value> {
    ensure!(
        value["targetKind"] == "app",
        "Unexpected desktop update target"
    );
    let fields: &[&str] = if let Some(id) = operation_id {
        ensure!(
            value["operationId"] == id,
            "Unexpected desktop operation identity"
        );
        if !value["error"].is_null() {
            value["error"] =
                json!("Desktop app update failed. Check the update status on the host PC.");
        }
        // The local helper's `message` may name an installation path. Remote
        // clients render progress from the structured phase instead.
        value["message"] = value["phase"].clone();
        &[
            "operationId",
            "targetKind",
            "accepted",
            "phase",
            "terminal",
            "outcome",
            "candidate",
            "message",
            "error",
            "recoveryRequired",
            "actionRequired",
            "expectedRelease",
            "selectedComponents",
            "targetComponents",
            "observedComponents",
            "restartGroup",
        ]
    } else {
        if !value["supportError"].is_null() {
            value["supportError"] = json!(
                "This desktop installation cannot update itself. Check for a packaged upgrade or recovery on the host PC."
            );
        }
        if !value["checkError"].is_null() {
            value["checkError"] = json!(if value["errorCode"] == "github_rate_limited" {
                "GitHub is limiting update checks from this network."
            } else {
                "The desktop app could not check for updates."
            });
        }
        &[
            "targetKind",
            "current",
            "latest",
            "supported",
            "updateAvailable",
            "owner",
            "supportError",
            "checkError",
            "components",
            "componentRequirements",
            "availableComponents",
            "restartGroup",
            "atomicGroups",
            "errorCode",
            "retryAt",
        ]
    };
    value
        .as_object_mut()
        .expect("object with targetKind")
        .retain(|key, _| fields.contains(&key.as_str()));
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_app_status_is_bound_to_the_approved_operation_and_redacts_local_diagnostics() {
        let journal = json!({"targetKind":"app", "operationId":"wanted", "phase":"failed", "terminal":true,
            "error":"private path in stderr", "message":"private path in stdout", "ownerSession":"secret", "helperProcess":{"pid":100}});
        assert!(project(journal.clone(), Some("another")).is_err());
        let status = project(journal, Some("wanted")).unwrap();
        assert_eq!(status["operationId"], "wanted");
        assert_eq!(status["phase"], "failed");
        assert_eq!(status["terminal"], true);
        assert!(status["error"].is_string());
        assert!(!status.to_string().contains("private path"));
        assert!(status.get("ownerSession").is_none() && status.get("helperProcess").is_none());
    }
}
