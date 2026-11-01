use super::{Continuation, ContinuationState, Operation, ResumeRequest, journal, now};
use crate::agent::storage::AgentStorage;
use anyhow::{Context, Result, ensure};
use paseo_client::{DaemonClient, DaemonTarget};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

fn native_identity(record: &Value) -> Value {
    json!({"provider":record["provider"],"sessionId":record["persistence"]["sessionId"],
        "nativeHandle":record["persistence"]["nativeHandle"]})
}

pub(super) async fn prepare(home: &Path, request: Option<&ResumeRequest>) -> Result<Option<Continuation>> {
    let Some(request) = request else { return Ok(None); };
    let record = AgentStorage::new(home.to_owned()).get(&request.agent_id).await?
        .context("Continuation agent does not exist in this daemon home")?;
    ensure!(record["archivedAt"].is_null(), "An archived agent cannot be automatically resumed");
    ensure!(record["internal"] != true, "An internal agent has no durable continuation identity");
    let identity = native_identity(&record);
    ensure!(identity["provider"].is_string() && identity["sessionId"].is_string(),
        "Continuation requires the original provider session identity");
    Ok(Some(Continuation {
        agent_id: request.agent_id.clone(), message_id: uuid::Uuid::new_v4().to_string(),
        provider: identity["provider"].as_str().unwrap().to_owned(),
        session_id: identity["sessionId"].clone(), native_handle: identity["nativeHandle"].clone(),
        text_digest: crate::json::fingerprint(&json!(request.text))?,
        state: ContinuationState::Pending, attempted_at: None, error: None,
    }))
}

pub(super) async fn connect(operation: &Operation) -> Result<DaemonClient> {
    let client = crate::client::connect(
        crate::client::ConnectOptions::new(DaemonTarget::Instance { home: operation.home.clone() }),
        CancellationToken::new(),
    ).await?;
    ensure!(client.server_info()["serverId"] == operation.server_id, "Updated daemon server identity changed");
    Ok(client)
}

async fn receipt(client: &DaemonClient, continuation: &Continuation) -> Result<Value> {
    let response = client.request(json!({
        "type":"agent.message_receipt.get.request","agentId":continuation.agent_id,
        "messageId":continuation.message_id,
    }), "agent.message_receipt.get.response", Duration::from_secs(5)).await?;
    let receipt = &response["receipt"];
    ensure!(receipt["agentId"] == continuation.agent_id && receipt["messageId"] == continuation.message_id,
        "Continuation receipt identity mismatch");
    Ok(receipt.clone())
}

fn reconcile(continuation: &mut Continuation, receipt: &Value) -> bool {
    continuation.state = match receipt["phase"].as_str() {
        Some("accepted") => ContinuationState::Accepted,
        Some("not_delivered") => ContinuationState::NotDelivered,
        Some("rejected") => ContinuationState::Failed,
        _ if receipt["dismissed"] == true => ContinuationState::Unknown,
        Some("outcome_unknown" | "missing") => ContinuationState::Unknown,
        Some("submitted") if receipt["correlation"]["acknowledgementPending"] == false => ContinuationState::Unknown,
        _ => return false,
    };
    continuation.error = match continuation.state {
        ContinuationState::Accepted => None,
        ContinuationState::NotDelivered => Some("The provider did not receive this continuation".into()),
        ContinuationState::Failed => Some("The provider rejected this continuation".into()),
        _ => Some("Continuation acknowledgement is unknown; this messageId will not be resent automatically".into()),
    };
    true
}

/// Late native ACKs live in the existing durable receipt, not a second sender.
/// Status and retention can observe them without rewriting an executor's journal.
pub(super) async fn observe(operation: &mut Operation) -> Result<()> {
    let Some(continuation) = &mut operation.continuation else { return Ok(()); };
    if continuation.attempted_at.is_some()
        && matches!(continuation.state, ContinuationState::Pending | ContinuationState::Unknown)
    {
        let receipt = crate::agent::receipts::MessageReceipts::observe_persisted(
            &operation.home, &continuation.agent_id, &continuation.message_id,
        ).await?;
        if matches!(receipt["phase"].as_str(), Some("accepted" | "not_delivered" | "rejected")) {
            reconcile(continuation, &receipt);
        }
    }
    Ok(())
}

/// A committed runtime is never rolled back because its continuation is unknown.
/// The dispatch fence is persisted before the only send; recovery reads receipts.
pub(super) async fn resume(operation: &mut Operation) -> Result<()> {
    let Some(continuation) = &operation.continuation else { return Ok(()); };
    if !matches!(continuation.state, ContinuationState::Pending | ContinuationState::Unknown) {
        return Ok(());
    }
    let client = match connect(operation).await {
        Ok(client) => client,
        Err(error) => {
            let continuation = operation.continuation.as_mut().unwrap();
            continuation.state = if continuation.attempted_at.is_none() { ContinuationState::NotDelivered } else { ContinuationState::Unknown };
            continuation.error = Some(format!("Continuation connection failed: {error}"));
            return journal::save(operation).await;
        }
    };
    if operation.continuation.as_ref().unwrap().attempted_at.is_some() {
        operation.continuation.as_mut().unwrap().state = ContinuationState::Unknown;
        let value = receipt(&client, operation.continuation.as_ref().unwrap()).await;
        if let Ok(value) = value { reconcile(operation.continuation.as_mut().unwrap(), &value); }
        return journal::save(operation).await;
    }
    let continuation = operation.continuation.as_ref().unwrap();
    let original = json!({"provider":continuation.provider,"sessionId":continuation.session_id,"nativeHandle":continuation.native_handle});
    let record = AgentStorage::new(operation.home.clone()).get(&continuation.agent_id).await?;
    if record.as_ref().is_none_or(|record| record["archivedAt"].is_string() || native_identity(record) != original) {
        let continuation = operation.continuation.as_mut().unwrap();
        continuation.state = ContinuationState::NotDelivered;
        continuation.error = Some("The original agent was archived, removed or changed provider session; no message was sent".into());
        return journal::save(operation).await;
    }
    let text = &operation.request.resume.as_ref().context("Continuation text missing from private journal")?.text;
    ensure!(crate::json::fingerprint(&json!(text))? == continuation.text_digest, "Pinned continuation text digest mismatch");
    let message = json!({"type":"send_agent_message_request","agentId":continuation.agent_id,
        "messageId":continuation.message_id,"text":text,"activeTurnBehavior":"steer",
        "unarchive":false,"expectedPersistence":original});
    let continuation = operation.continuation.as_mut().unwrap();
    continuation.attempted_at = Some(now());
    continuation.state = ContinuationState::Pending;
    continuation.error = Some("Awaiting the original message's acknowledgement".into());
    journal::save(operation).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let _send_result = client.request(message, "send_agent_message_response", Duration::from_secs(180)).await;
    loop {
        match receipt(&client, operation.continuation.as_ref().unwrap()).await {
            Ok(value) if reconcile(operation.continuation.as_mut().unwrap(), &value) => break,
            Ok(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(250)).await,
            _ => {
                operation.continuation.as_mut().unwrap().state = ContinuationState::Unknown;
                operation.continuation.as_mut().unwrap().error = Some(
                    "Continuation acknowledgement is unknown; query this operation and the same message receipt, not a new send".into());
                break;
            }
        }
    }
    journal::save(operation).await
}
