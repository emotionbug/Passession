mod diagnostics;
#[cfg(test)]
mod tests;
pub(crate) mod update_operation;
pub(crate) mod updater;

use crate::{state::AppState, transport::RequestContext};
use anyhow::{Context, Result, anyhow};
use paseo_client::ClientError;
use serde_json::{Value, json};
use std::{
    sync::Weak,
    time::{Duration, Instant},
};
use update_operation::{SubmitRequest, UpdateSource};

/// Bounded wait for other running turns before cutover when a caller names none.
pub(crate) const DEFAULT_UPDATE_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const MAX_UPDATE_IDLE_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const LEGACY_OBSERVE_INTERVAL: Duration = Duration::from_millis(500);
const LEGACY_STATUS_ATTEMPTS: u32 = 5;

pub struct DaemonAdminService {
    app: Weak<AppState>,
    started: Instant,
}
impl DaemonAdminService {
    pub fn new(app: Weak<AppState>) -> Self {
        Self {
            app,
            started: Instant::now(),
        }
    }
    pub async fn handle(&self, context: &RequestContext, message: &Value) -> Result<bool> {
        let kind = message["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "diagnostics.request"
                | "daemon.update.request"
                | "daemon.update.submit.request"
                | "daemon.update.status.request"
                | "daemon.update.drain.request"
                | "daemon.update.drain.release.request"
                | "restart_server_request"
                | "shutdown_server_request"
        ) {
            return Ok(false);
        }
        let app = self
            .app
            .upgrade()
            .ok_or_else(|| anyhow!("Daemon is shutting down"))?;
        let request = crate::transport::request_id(message).context("Missing requestId")?;
        match kind{
            "diagnostics.request"=>{
                match diagnostics::collect(&app,self.started).await{
                    Ok(diagnostic)=>context.send(json!({"type":"diagnostics.response","payload":{"requestId":request,"diagnostic":diagnostic}})).await?,
                    Err(error)=>context.send(json!({"type":"rpc_error","payload":{"requestId":request,"requestType":kind,"error":error.to_string()}})).await?,
                }
            }
            "restart_server_request"|"shutdown_server_request"=>{
                let restart=kind=="restart_server_request";
                let mut payload=json!({"status":if restart{"restart_requested"}else{"shutdown_requested"},"clientId":context.client_id(),"requestId":request});
                if restart{if let Some(reason)=message["reason"].as_str().filter(|s|!s.trim().is_empty()){payload["reason"]=json!(reason)}}
                context.send_flushed(json!({"type":"status","payload":payload})).await?;
                app.request_shutdown(restart);
            }
            "daemon.update.submit.request"=>{
                let admitted=match remote_submit_request(message){
                    Ok(submit)=>admit(&app,submit).await,
                    Err(error)=>Err(error),
                };
                match admitted{
                    Ok(operation)=>context.send(json!({"type":"daemon.update.submit.response","payload":{"requestId":request,"operation":operation}})).await?,
                    Err(error)=>rpc_error(context,request,kind,&error).await?,
                }
            }
            "daemon.update.status.request"=>{
                let observed=async{
                    let operation=update_operation_id(message["operationId"].as_str().unwrap_or(""))?;
                    update_operation::status(&app.home,&operation).await
                }.await;
                match observed{
                    Ok(operation)=>context.send(json!({"type":"daemon.update.status.response","payload":{"requestId":request,"operation":operation}})).await?,
                    Err(error)=>rpc_error(context,request,kind,&error).await?,
                }
            }
            "daemon.update.drain.request"|"daemon.update.drain.release.request"=>{
                match update_operation::handle_drain(&app,message).await{
                    Ok(result)=>{
                        let mut payload=match result{Value::Object(map)=>Value::Object(map),other=>json!({"result":other})};
                        payload["requestId"]=json!(request);
                        let response=format!("{}.response",kind.strip_suffix(".request").unwrap_or(kind));
                        // The executor stops this worker after drain; flush so the acknowledgement precedes it.
                        context.send_flushed(json!({"type":response,"payload":payload})).await?;
                    }
                    Err(error)=>rpc_error(context,request,kind,&error).await?,
                }
            }
            "daemon.update.request"=>legacy_update(&app,context,request).await?,
            _=>unreachable!(),
        }
        Ok(true)
    }
}

/// Canonical hyphenated UUID; equal operation IDs must compare equal across retries.
pub(crate) fn update_operation_id(raw: &str) -> Result<String> {
    let invalid = || {
        anyhow::Error::from(ClientError::new(
            "INVALID_OPERATION_ID",
            "Update operation IDs are UUIDs, for example 0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11.",
        ))
    };
    let id = uuid::Uuid::parse_str(raw.trim()).map_err(|_| invalid())?;
    if id.is_nil() {
        return Err(invalid());
    }
    Ok(id.hyphenated().to_string())
}
pub(crate) fn update_idle_timeout_ms(timeout: Duration) -> Result<u64> {
    if timeout.is_zero() || timeout > MAX_UPDATE_IDLE_TIMEOUT {
        return Err(ClientError::new(
            "INVALID_IDLE_TIMEOUT",
            "The idle timeout must be greater than zero and at most 24 hours.",
        )
        .into());
    }
    Ok(timeout.as_millis() as u64)
}
pub(crate) fn update_terminal(operation: &Value) -> bool {
    operation["terminal"] == true
}

/// Remote callers may install official releases only: a path in a request names a
/// file on the caller's computer, never one the daemon should read.
fn remote_submit_request(message: &Value) -> Result<SubmitRequest> {
    let reject = |code: &str, text: &str| anyhow::Error::from(ClientError::new(code, text));
    let operation_id = match message["operationId"].as_str() {
        Some(raw) => update_operation_id(raw)?,
        None => {
            return Err(reject(
                "INVALID_OPERATION_ID",
                "operationId is required so a lost acknowledgement can be retried with the same identity.",
            ));
        }
    };
    let release = match message.get("source") {
        None | Some(Value::Null) => true,
        Some(source) => source == "release",
    };
    if !release || ["from", "path", "localPath"].iter().any(|key| message.get(*key).is_some_and(|v| !v.is_null())) {
        return Err(reject(
            "LOCAL_SOURCE_REJECTED",
            "Local runtime paths are accepted only by paseo daemon update --from with a local --home; remote requests install official releases.",
        ));
    }
    if message.get("resume").is_some_and(|v| !v.is_null()) {
        return Err(reject(
            "RESUME_REQUIRES_LOCAL_HOME",
            "Session continuation is accepted only by paseo daemon update --resume-agent with a local --home.",
        ));
    }
    let idle = match message.get("idleTimeoutMs") {
        None | Some(Value::Null) => DEFAULT_UPDATE_IDLE_TIMEOUT,
        Some(value) => Duration::from_millis(value.as_u64().ok_or_else(|| {
            reject("INVALID_IDLE_TIMEOUT", "idleTimeoutMs must be a positive integer.")
        })?),
    };
    let force_running = match message.get("forceRunning") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(force)) => *force,
        Some(_) => return Err(reject("INVALID_REQUEST", "forceRunning must be a boolean.")),
    };
    Ok(SubmitRequest {
        operation_id,
        source: UpdateSource::Release,
        resume: None,
        idle_timeout_ms: update_idle_timeout_ms(idle)?,
        force_running,
    })
}

/// Admission runs detached: transport aborts a request task when its session ends,
/// and a disconnect must neither abandon a half-finished durable handoff nor reach
/// the executor. Only the observer belongs to the request.
async fn admit(app: &AppState, request: SubmitRequest) -> Result<Value> {
    let home = app.home.clone();
    let executable = app.executable.clone();
    tokio::spawn(async move { update_operation::submit(&home, &executable, request).await })
        .await
        .map_err(|error| anyhow!("Update admission task ended unexpectedly: {error}"))?
}

async fn rpc_error(
    context: &RequestContext,
    request: &str,
    kind: &str,
    error: &anyhow::Error,
) -> Result<()> {
    let code = error
        .downcast_ref::<ClientError>()
        .map(|error| error.code.as_str())
        .unwrap_or("UPDATE_FAILED");
    context
        .send(json!({"type":"rpc_error","payload":{"requestId":request,"requestType":kind,"code":code,"error":format!("{error:#}")}}))
        .await
}

/// Legacy `daemon.update.request`: submits an official release to the shared engine
/// and observes that operation. `complete`/`success:true` mean committed, never
/// merely admitted or installed. The executor replaces this worker, so an old
/// client usually loses the connection before the outcome; it then observes the
/// replacement worker as before.
async fn legacy_update(app: &AppState, context: &RequestContext, request: &str) -> Result<()> {
    let operation_id = uuid::Uuid::new_v4().hyphenated().to_string();
    progress(context, request, "starting", &operation_id, None).await?;
    let submit = SubmitRequest {
        operation_id: operation_id.clone(),
        source: UpdateSource::Release,
        resume: None,
        idle_timeout_ms: DEFAULT_UPDATE_IDLE_TIMEOUT.as_millis() as u64,
        force_running: false,
    };
    let mut operation = match admit(app, submit).await {
        Ok(operation) => operation,
        Err(error) => {
            return context
                .send(legacy_response(request, &operation_id, None, Some(format!("{error:#}"))))
                .await;
        }
    };
    let cancel = context.cancellation();
    let mut reported = "starting";
    let mut failures = 0;
    while !update_terminal(&operation) {
        let phase = legacy_phase(operation["phase"].as_str().unwrap_or(""));
        if phase != reported {
            progress(context, request, phase, &operation_id, operation["phase"].as_str()).await?;
            reported = phase;
        }
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(LEGACY_OBSERVE_INTERVAL) => {}
        }
        match update_operation::status(&app.home, &operation_id).await {
            Ok(next) => {
                operation = next;
                failures = 0;
            }
            Err(error) => {
                failures += 1;
                if failures >= LEGACY_STATUS_ATTEMPTS {
                    let message = format!(
                        "Update operation {operation_id} continues, but its status could not be read: {error:#}"
                    );
                    return context
                        .send(legacy_response(request, &operation_id, Some(&operation), Some(message)))
                        .await;
                }
            }
        }
    }
    if operation["phase"] == "committed" {
        progress(context, request, "complete", &operation_id, Some("committed")).await?;
    }
    context
        .send_flushed(legacy_response(request, &operation_id, Some(&operation), None))
        .await
}
/// Legacy progress vocabulary is a closed enum; `complete` is reserved for committed.
fn legacy_phase(phase: &str) -> &'static str {
    match phase {
        "admitted" => "starting",
        "preparing" => "downloading",
        "committed" => "complete",
        _ => "installing",
    }
}
fn legacy_response(
    request: &str,
    operation_id: &str,
    operation: Option<&Value>,
    error: Option<String>,
) -> Value {
    let committed = error.is_none() && operation.is_some_and(|op| op["phase"] == "committed");
    let error = error.or_else(|| {
        (!committed).then(|| {
            operation
                .and_then(|op| op["error"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    format!(
                        "Update operation {operation_id} ended as {}",
                        operation.and_then(|op| op["phase"].as_str()).unwrap_or("unknown")
                    )
                })
        })
    });
    let previous = operation
        .and_then(|op| op["previous"]["version"].as_str())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let new_version = committed
        .then(|| operation.and_then(|op| op["candidate"]["version"].as_str()))
        .flatten();
    let mut payload = json!({"requestId":request,"success":committed,"error":error,"previousVersion":previous,"newVersion":new_version,"operationId":operation_id});
    if let Some(operation) = operation {
        payload["accepted"] = json!(operation["accepted"] == true);
        payload["operationPhase"] = operation["phase"].clone();
    }
    json!({"type":"daemon.update.response","payload":payload})
}
async fn progress(
    context: &RequestContext,
    request: &str,
    phase: &str,
    operation_id: &str,
    operation_phase: Option<&str>,
) -> Result<()> {
    context
        .send(json!({"type":"daemon.update.progress","payload":{"requestId":request,"phase":phase,"operationId":operation_id,"operationPhase":operation_phase}}))
        .await
}

#[cfg(test)]
mod frontend_tests {
    use super::*;

    #[test]
    fn operation_ids_are_canonical_uuids_so_retries_match() {
        let id = "0F8B6C5E-2A71-4C5B-9A43-7F2A1D0C9E11";
        assert_eq!(
            update_operation_id(id).unwrap(),
            "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11"
        );
        assert_eq!(
            update_operation_id("0f8b6c5e2a714c5b9a437f2a1d0c9e11").unwrap(),
            update_operation_id(id).unwrap()
        );
        for invalid in ["", "latest", "00000000-0000-0000-0000-000000000000", "../x"] {
            let error = update_operation_id(invalid).unwrap_err();
            assert_eq!(error.downcast_ref::<ClientError>().unwrap().code, "INVALID_OPERATION_ID");
        }
    }

    #[test]
    fn remote_submit_installs_releases_only_and_requires_a_retryable_identity() {
        let id = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
        let accepted = remote_submit_request(&json!({"type":"daemon.update.submit.request","operationId":id})).unwrap();
        assert_eq!(accepted.operation_id, id);
        assert!(matches!(accepted.source, UpdateSource::Release));
        assert!(accepted.resume.is_none());
        assert!(!accepted.force_running);
        assert_eq!(accepted.idle_timeout_ms, DEFAULT_UPDATE_IDLE_TIMEOUT.as_millis() as u64);
        let explicit = remote_submit_request(&json!({"operationId":id,"source":"release","idleTimeoutMs":1500,"forceRunning":true})).unwrap();
        assert_eq!(explicit.idle_timeout_ms, 1500);
        assert!(explicit.force_running);
        for (message, code) in [
            (json!({}), "INVALID_OPERATION_ID"),
            (json!({"operationId":id,"source":{"local":"/tmp/runtime"}}), "LOCAL_SOURCE_REJECTED"),
            (json!({"operationId":id,"source":"local"}), "LOCAL_SOURCE_REJECTED"),
            (json!({"operationId":id,"from":"/tmp/runtime"}), "LOCAL_SOURCE_REJECTED"),
            (json!({"operationId":id,"resume":{"agentId":"a","text":"t"}}), "RESUME_REQUIRES_LOCAL_HOME"),
            (json!({"operationId":id,"idleTimeoutMs":0}), "INVALID_IDLE_TIMEOUT"),
            (json!({"operationId":id,"idleTimeoutMs":-1}), "INVALID_IDLE_TIMEOUT"),
            (json!({"operationId":id,"forceRunning":"yes"}), "INVALID_REQUEST"),
        ] {
            let error = remote_submit_request(&message).unwrap_err();
            assert_eq!(error.downcast_ref::<ClientError>().unwrap().code, code, "{message}");
        }
    }

    #[test]
    fn legacy_observer_reports_complete_and_success_only_for_committed() {
        for phase in ["admitted", "preparing", "staged", "draining", "activating", "verifying", "rolling_back", "rolled_back", "failed"] {
            assert_ne!(legacy_phase(phase), "complete", "{phase}");
        }
        assert_eq!(legacy_phase("committed"), "complete");
        let id = "0f8b6c5e-2a71-4c5b-9a43-7f2a1d0c9e11";
        let committed = json!({"operationId":id,"phase":"committed","terminal":true,"accepted":true,"previous":{"version":"1.0.0"},"candidate":{"version":"1.1.0"},"continuation":{"state":"unknown"}});
        let payload = legacy_response("r", id, Some(&committed), None)["payload"].clone();
        assert_eq!(payload["success"], true);
        assert_eq!(payload["error"], Value::Null);
        assert_eq!(payload["newVersion"], "1.1.0");
        assert_eq!(payload["previousVersion"], "1.0.0");
        assert_eq!(payload["operationId"], id);
        let rolled = json!({"operationId":id,"phase":"rolled_back","terminal":true,"accepted":true,"error":"readiness failed","candidate":{"version":"1.1.0"}});
        let payload = legacy_response("r", id, Some(&rolled), None)["payload"].clone();
        assert_eq!(payload["success"], false);
        assert_eq!(payload["newVersion"], Value::Null);
        assert_eq!(payload["error"], "readiness failed");
        assert_eq!(payload["operationPhase"], "rolled_back");
        let unread = legacy_response("r", id, Some(&committed), Some("status unavailable".into()))["payload"].clone();
        assert_eq!(unread["success"], false);
        let rejected = legacy_response("r", id, None, Some("owner unsupported".into()))["payload"].clone();
        assert_eq!(rejected["success"], false);
        assert_eq!(rejected["operationId"], id);
        assert!(rejected.get("accepted").is_none());
    }
}
