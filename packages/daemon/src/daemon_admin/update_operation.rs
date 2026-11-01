//! One durable operation for local and published native runtime updates.
//! Request connections observe this journal; the launch-owner helper executes it.
mod admission;
mod continuation;
mod drain;
mod execution;
mod journal;
#[cfg(test)]
mod tests;

pub(crate) use admission::submit;
pub(crate) use drain::{handle_drain, initialize as initialize_admission, wire_admission};
pub(crate) use execution::{execute, recover_bootstrap};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
pub enum UpdateSource {
    Local(PathBuf),
    Release,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeRequest {
    pub agent_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitRequest {
    pub operation_id: String,
    pub source: UpdateSource,
    pub resume: Option<ResumeRequest>,
    pub idle_timeout_ms: u64,
    pub force_running: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentity {
    pub root: PathBuf,
    pub version: String,
    pub source_sha: String,
    pub platform: String,
    pub arch: String,
    pub release: bool,
    pub executable_sha256: String,
    pub tree_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Admitted,
    Preparing,
    Staged,
    Draining,
    Activating,
    Verifying,
    Committed,
    RollingBack,
    RolledBack,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ContinuationState {
    Pending,
    Accepted,
    NotDelivered,
    Unknown,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Continuation {
    agent_id: String,
    message_id: String,
    provider: String,
    session_id: Value,
    native_handle: Value,
    text_digest: String,
    state: ContinuationState,
    attempted_at: Option<String>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Operation {
    schema_version: u32,
    operation_id: String,
    request_fingerprint: String,
    request: SubmitRequest,
    input_digest: Option<String>,
    home: PathBuf,
    server_id: String,
    installation_root: PathBuf,
    owner: Value,
    launch_owner: crate::daemon_control::managed::LaunchOwner,
    phase: Phase,
    previous: Option<RuntimeIdentity>,
    candidate: Option<RuntimeIdentity>,
    executor: Option<Value>,
    worker: Option<Value>,
    original_worker: Value,
    continuation: Option<Continuation>,
    created_at: String,
    updated_at: String,
    error: Option<String>,
    error_code: Option<String>,
    rollback: Option<Value>,
}

impl Operation {
    fn terminal(&self) -> bool {
        match self.phase {
            Phase::Failed | Phase::RolledBack => true,
            Phase::Committed => self.continuation.as_ref()
                .is_none_or(|value| value.state != ContinuationState::Pending),
            _ => false,
        }
    }

    fn view(&self) -> Value {
        // In particular, request.resume.text and private launch environment must
        // never be returned by status or copied into CLI progress messages.
        let continuation = self.continuation.as_ref().map(|value| json!({
            "agentId":value.agent_id,"messageId":value.message_id,
            "state":value.state,"error":value.error,
        }));
        json!({
            "schemaVersion":self.schema_version,"operationId":self.operation_id,
            "phase":self.phase,"terminal":self.terminal(),"accepted":self.executor.is_some(),
            "home":self.home,"serverId":self.server_id,"owner":self.owner,
            "previous":self.previous,"candidate":self.candidate,"worker":self.worker,
            "createdAt":self.created_at,"updatedAt":self.updated_at,
            "error":self.error,"errorCode":self.error_code,"rollback":self.rollback,
            "continuation":continuation,
        })
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn validate_id(id: &str) -> Result<()> {
    let uuid = uuid::Uuid::parse_str(id).context("Update operationId must be a UUID")?;
    if uuid.is_nil() || uuid.to_string() != id {
        bail!("Update operationId must be a non-nil, canonical lowercase UUID");
    }
    Ok(())
}

pub async fn status(home: &Path, operation_id: &str) -> Result<Value> {
    validate_id(operation_id)?;
    let mut operation = journal::read(home, operation_id).await?
        .with_context(|| format!("Unknown native update operation {operation_id}"))?;
    let observation = continuation::observe(&mut operation).await;
    let mut view = operation.view();
    if let Err(error) = observation {
        view["continuation"]["observationError"] = json!(error.to_string());
    }
    Ok(view)
}

pub async fn wait(home: &Path, operation_id: &str, cancel: &CancellationToken) -> Result<Value> {
    loop {
        let view = status(home, operation_id).await?;
        if view["terminal"] == true {
            return Ok(view);
        }
        tokio::select! {
            _ = cancel.cancelled() => bail!("Stopped observing update {operation_id}; the admitted operation continues"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {},
        }
    }
}
