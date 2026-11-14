use super::{Operation, Phase, journal, validate_id};
use crate::agent::admission::WorkGuard;
use crate::daemon_control::managed::{self, ExecutorHandle};
use crate::state::AppState;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

pub(crate) fn wire_admission(app: &AppState, message: &Value) -> Result<Option<WorkGuard>> {
    let kind = message["type"].as_str().unwrap_or("");
    // Receipt-aware agent sends/creates gate at their durable provider boundary;
    // a duplicate must still be able to replay its completed receipt while paused.
    if matches!(kind,
        "send_agent_message_request" | "create_agent_request" | "agent.create.request"
        | "workspace.create.request" | "cancel_agent_request" | "kill_agent_request"
        | "agent.execution.cancel.request" | "agent_permission_response"
        | "restart_server_request" | "shutdown_server_request")
        || kind.starts_with("daemon.update.")
    {
        return Ok(None);
    }
    let mutating = crate::transport::permissions::inbound(kind)
        .is_some_and(|permissions| permissions.iter().any(|permission| !permission.ends_with(".read")));
    if mutating { Ok(Some(app.agents.update_admission.enter()?)) } else { Ok(None) }
}

pub(crate) async fn initialize(app: &Arc<AppState>) -> Result<()> {
    if let Some(operation) = journal::current(&app.home).await?
        && matches!(operation.phase, Phase::Draining | Phase::Activating | Phase::Verifying | Phase::RollingBack) {
            pause(app, &operation)?;
        }
    Ok(())
}

fn announce(app: &Arc<AppState>, operation_id: &str, active: bool) {
    let app = app.clone();
    let message = json!({"type":"daemon.update.maintenance","payload":{"operationId":operation_id,"active":active}});
    tokio::spawn(async move {
        match tokio::time::timeout(Duration::from_secs(2), app.transport.publish_event(message)).await {
            Ok(Ok(())) => {},
            Ok(Err(error)) => tracing::warn!(%error, "Update maintenance notice delivery failed"),
            Err(_) => tracing::warn!("Update maintenance notice delivery exceeded its deadline"),
        }
    });
}

fn release(app: &Arc<AppState>, operation_id: &str) -> Result<()> {
    if app.agents.update_admission.release(operation_id)? {
        announce(app, operation_id, false);
    }
    Ok(())
}

fn pause(app: &Arc<AppState>, operation: &Operation) -> Result<()> {
    if !app.agents.update_admission.pause(&operation.operation_id)? {
        return Ok(());
    }
    announce(app, &operation.operation_id, true);
    let app = Arc::downgrade(app);
    let id = operation.operation_id.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let Some(app) = app.upgrade() else { return; };
            if app.cancel.is_cancelled() { return; }
            let result = async {
                let current = journal::current(&app.home).await?;
                let release = match current {
                    None => true,
                    Some(operation) if operation.operation_id != id => true,
                    Some(operation) if operation.terminal() => true,
                    Some(operation) if matches!(operation.phase, Phase::Admitted | Phase::Preparing | Phase::Staged | Phase::Draining) => {
                        match operation.executor {
                            Some(handle) => !managed::executor_alive(&serde_json::from_value::<ExecutorHandle>(handle)?)?,
                            None => true,
                        }
                    }
                    _ => false,
                };
                if release { self::release(&app, &id)?; }
                Ok::<_, anyhow::Error>(release)
            }.await;
            match result {
                Ok(true) => return,
                Ok(false) => {},
                Err(error) => {
                    tracing::error!(operation_id = id, %error, "Cannot reconcile native update admission pause");
                    return;
                }
            }
        }
    });
    Ok(())
}

pub async fn handle_drain(app: &Arc<AppState>, request: &Value) -> Result<Value> {
    let id = request["operationId"].as_str().context("operationId is required")?;
    validate_id(id)?;
    let operation = journal::current(&app.home).await?.context("No admitted native update operation")?;
    ensure!(operation.operation_id == id, "Native update drain identity mismatch");
    if request["type"] == "daemon.update.drain.release.request" {
        ensure!(operation.terminal() || operation.phase == Phase::Committed,
            "An active update still owns admission");
        release(app, id)?;
        return Ok(json!({"operationId":id,"released":true}));
    }
    ensure!(operation.phase == Phase::Draining, "Native update is not in its drain phase");
    let executor: ExecutorHandle = serde_json::from_value(operation.executor.clone().context("Update has no executor acknowledgement")?)?;
    ensure!(managed::executor_alive(&executor)?, "Native update executor is no longer alive");
    pause(app, &operation)?;
    let timeout = Duration::from_millis(operation.request.idle_timeout_ms);
    let deadline = tokio::time::Instant::now() + timeout;
    let exempt = operation.continuation.as_ref().map(|value| value.agent_id.as_str());
    let mut busy;
    loop {
        busy = app.agents.update_busy_agents(exempt).await?;
        let pending = app.agents.update_admission.active()?;
        if busy.is_empty() && pending == 0 {
            match tokio::time::timeout_at(deadline, app.agents.flush_update_events()).await {
                Ok(Ok(())) => {
                    busy = app.agents.update_busy_agents(exempt).await?;
                    if busy.is_empty() && app.agents.update_admission.active()? == 0 {
                        return Ok(json!({"operationId":id,"ready":true,"forced":false,"busyAgents":[],"inflightAdmissions":0}));
                    }
                }
                Ok(Err(error)) => return Ok(json!({"operationId":id,"ready":operation.request.force_running,"forced":operation.request.force_running,"busyAgents":busy,"drainError":error.to_string(),"inflightAdmissions":pending})),
                Err(_) => break,
            }
        }
        if tokio::time::Instant::now() >= deadline { break; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(json!({"operationId":id,"ready":operation.request.force_running,"forced":operation.request.force_running,
        "busyAgents":busy,"inflightAdmissions":app.agents.update_admission.active()?,"deadlineExceeded":true}))
}
