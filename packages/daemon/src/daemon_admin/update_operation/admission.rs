use super::{Operation, Phase, SubmitRequest, UpdateSource, continuation, journal, now, validate_id};
use crate::daemon_control::managed::{self, ExecutorHandle, Installation};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::path::Path;
use std::time::Duration;

pub async fn submit(home: &Path, executor_executable: &Path, mut request: SubmitRequest) -> Result<serde_json::Value> {
    validate_id(&request.operation_id)?;
    super::super::update_idle_timeout_ms(Duration::from_millis(request.idle_timeout_ms))?;
    if let Some(resume) = &request.resume {
        ensure!(!resume.text.trim().is_empty(), "Continuation text must not be empty");
        ensure!(resume.text.len() <= 256 * 1024, "Continuation text exceeds 256 KiB");
        uuid::Uuid::parse_str(&resume.agent_id).context("Continuation agentId must be a UUID")?;
    }
    let home = tokio::fs::canonicalize(home).await.context("Daemon home does not exist")?;
    let _admission = journal::Lease::acquire(&home, "admission")?;
    journal::ensure_no_legacy_update(&home).await?;
    let digest = match &mut request.source {
        UpdateSource::Local(source) => {
            ensure!(!tokio::fs::symlink_metadata(&*source).await?.file_type().is_symlink(),
                "Update input must not be a symlink");
            *source = tokio::fs::canonicalize(&*source).await.context("Update input does not exist")?;
            ensure!(!source.starts_with(&home) && !home.starts_with(&*source),
                "Runtime input and daemon data must not overlap");
            Some(super::super::updater::input_digest(source).await?)
        }
        UpdateSource::Release => None,
    };
    let fingerprint = crate::json::fingerprint(&json!({"request":request,"inputDigest":digest}))?;
    let operation = if let Some(existing) = journal::read(&home, &request.operation_id).await? {
        ensure!(existing.request_fingerprint == fingerprint,
            "Update operationId conflicts with different input, policy or continuation");
        if existing.terminal() {
            return Ok(existing.view());
        }
        if let Some(handle) = &existing.executor {
            let handle: ExecutorHandle = serde_json::from_value(handle.clone())?;
            if managed::executor_alive(&handle)? {
                return Ok(existing.view());
            }
        }
        existing
    } else {
        if let Some(current) = journal::current(&home).await? {
            ensure!(current.terminal(), "Another native update operation is still active");
        }
        // Discovery checks real launch ownership, process birth and home before
        // creating any installation or modifying a service.
        let owner = managed::discover_owner(&home).await?;
        let installation = Installation::for_home(&home)?;
        // A terminal journal can precede the executor's final cleanup write.
        // Do not archive/replace it while that executor still owns selection.
        let _selection = installation.try_lease()?
            .context("The preceding update executor is still finishing")?;
        let continuation = continuation::prepare(&home, request.resume.as_ref()).await?;
        let timestamp = now();
        let mut operation = Operation {
            schema_version: 2,
            operation_id: request.operation_id.clone(),
            request_fingerprint: fingerprint,
            request,
            input_digest: digest,
            home: home.clone(),
            server_id: owner.server_id.clone(),
            installation_root: installation.root().to_owned(),
            owner: owner.public_summary(),
            original_worker: serde_json::to_value(&owner.supervisor)?,
            launch_owner: owner,
            phase: Phase::Admitted,
            previous: None,
            candidate: None,
            executor: None,
            worker: None,
            continuation,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            error: None,
            error_code: None,
            rollback: None,
        };
        journal::admit(&mut operation).await?;
        operation
    };
    // spawn_executor uses the actual launch owner (not this request's process
    // tree). It is idempotent for the persisted operation ID.
    managed::spawn_executor(&home, &operation.operation_id, &operation.launch_owner, executor_executable).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let observed = journal::read(&home, &operation.operation_id).await?
            .context("Admitted native update journal disappeared")?;
        // The executor itself writes this identity only after acquiring both
        // execution leases. A successful spawn alone is not an accepted update.
        if observed.executor.is_some() {
            return Ok(observed.view());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("Native update {} is durably recorded but executor acknowledgement is unknown; query status or retry the same operationId", operation.operation_id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
