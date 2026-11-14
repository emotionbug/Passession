use super::{Operation, Phase, UpdateSource, continuation, journal};
use crate::daemon_control::managed::{self, Installation, InstallationLease};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

const STOP_DEADLINE: Duration = Duration::from_secs(20);
const READY_DEADLINE: Duration = Duration::from_secs(90);

async fn release_drain(operation: &Operation) -> Result<()> {
    let client = continuation::connect(operation).await?;
    if client.server_info()["features"]["durableDaemonUpdates"] != true { return Ok(()); }
    client.request(json!({"type":"daemon.update.drain.release.request","operationId":operation.operation_id}),
        "daemon.update.drain.release.response", Duration::from_secs(5)).await?;
    Ok(())
}

async fn drain(operation: &Operation) -> Result<Value> {
    let client = continuation::connect(operation).await?;
    if client.server_info()["features"]["durableDaemonUpdates"] != true {
        // The first migration cannot fabricate an admission barrier in an old
        // executable. It requires the operator's explicit interruption policy.
        ensure!(operation.request.force_running,
            "LEGACY_DRAIN_UNSUPPORTED: this daemon predates admission drain; first migration requires --force-running with permission to interrupt its running work");
        return Ok(json!({"ready":true,"forced":true,"admissionBarrier":"legacy-owner-stop"}));
    }
    let result = client.request(json!({"type":"daemon.update.drain.request","operationId":operation.operation_id}),
        "daemon.update.drain.response", Duration::from_millis(operation.request.idle_timeout_ms) + Duration::from_secs(10)).await?;
    ensure!(result["ready"] == true, "UPDATE_BUSY: other work did not drain before the idle deadline; the current daemon is retained");
    Ok(result)
}

async fn stage(operation: &mut Operation, installation: &Installation, lease: &InstallationLease) -> Result<()> {
    let observed = managed::discover_owner(&operation.home).await?;
    ensure!(observed.supervisor.pid == operation.launch_owner.supervisor.pid
        && observed.supervisor.process_start_identity == operation.launch_owner.supervisor.process_start_identity
        && observed.server_id == operation.server_id
        && observed.executable_sha256 == operation.launch_owner.executable_sha256,
        "Launch owner changed before runtime preparation");
    operation.phase = Phase::Preparing;
    journal::save(operation).await?;
    let selected = installation.prepare_previous(lease, &operation.launch_owner).await?;
    operation.previous = Some(selected.active);
    journal::save(operation).await?;
    let destination = installation.staging_path(&operation.operation_id)?;
    if let Some(parent) = destination.parent() { tokio::fs::create_dir_all(parent).await?; }
    let candidate = if let Some(candidate) = &operation.candidate {
        candidate.clone()
    } else if tokio::fs::try_exists(&destination).await? {
        // A publication may have completed immediately before the executor died.
        // Only a staged identity recorded by this operation is reusable; an
        // unclaimed directory is retained rather than silently replaced.
        bail!("Unclaimed staged runtime remains from interrupted preparation; original daemon is retained")
    } else {
        match &operation.request.source {
            UpdateSource::Local(source) => super::super::updater::prepare_local_pinned(source, &destination,
                &operation.launch_owner.executable, operation.input_digest.as_deref().context("Local input digest missing")?).await?,
            UpdateSource::Release => super::super::updater::prepare_release(&operation.launch_owner.executable, &destination).await?,
        }
    };
    operation.candidate = Some(candidate.clone());
    journal::save(operation).await?;
    operation.candidate = Some(installation.admit_runtime(lease, &candidate).await?);
    operation.phase = Phase::Staged;
    journal::save(operation).await
}

async fn activate(operation: &mut Operation, installation: &Installation, lease: &InstallationLease) -> Result<()> {
    let candidate = operation.candidate.clone().context("Update has no staged runtime")?;
    super::super::updater::verify_runtime(&candidate).await?;
    if operation.phase == Phase::Staged || operation.phase == Phase::Draining {
        operation.phase = Phase::Draining;
        journal::save(operation).await?;
        drain(operation).await?;
        journal::ensure_no_legacy_update(&operation.home).await?;
        // This durable intent precedes every action that can stop the old owner.
        operation.phase = Phase::Activating;
        journal::save(operation).await?;
    }
    let selected = installation.load()?.context("Managed runtime selection disappeared")?;
    let trial_selected = selected.trial.as_ref().is_some_and(|trial| trial.operation_id == operation.operation_id);
    if !trial_selected {
        managed::stop_owner(installation, &operation.launch_owner, STOP_DEADLINE).await?;
        journal::ensure_no_legacy_update(&operation.home).await?;
        managed::bind_service(installation, lease, &operation.launch_owner).await?;
        installation.select_trial(lease, &operation.operation_id, &candidate).await?;
    }
    // start_owner is owner-idempotent, including a retry after activation's ACK
    // was lost. The fixed launcher observes this helper's installation lease.
    managed::start_owner(installation, &operation.launch_owner, READY_DEADLINE).await?;
    operation.phase = Phase::Verifying;
    journal::save(operation).await?;
    let evidence = managed::verify_ready(&operation.launch_owner, &candidate, READY_DEADLINE, &CancellationToken::new()).await?;
    operation.worker = Some(serde_json::to_value(evidence)?);
    // The operation journal is the commit authority. If power fails before the
    // selection promotion, bootstrap completes exactly this verified commit.
    operation.phase = Phase::Committed;
    operation.error = None;
    operation.error_code = None;
    journal::save(operation).await?;
    installation.commit_trial(lease, &operation.operation_id, &candidate).await?;
    Ok(())
}

async fn rollback(operation: &mut Operation, installation: &Installation, lease: &InstallationLease) -> Result<()> {
    let previous = operation.previous.clone().context("No known-good runtime was recorded")?;
    operation.phase = Phase::RollingBack;
    journal::save(operation).await?;
    // Recovery can re-enter after the previous runtime already started. Verify
    // that state before doing another stop/start.
    if let Ok(owner) = managed::discover_owner(&operation.home).await {
        if owner.executable_sha256 == previous.executable_sha256
            && owner.supervisor.pid == operation.launch_owner.supervisor.pid
            && owner.supervisor.process_start_identity == operation.launch_owner.supervisor.process_start_identity
        {
            installation.abandon_trial(lease, Some(&operation.operation_id)).await?;
            operation.phase = Phase::Failed;
            operation.rollback = Some(json!({"preservedOriginal":true,"owner":owner.public_summary()}));
            return journal::save(operation).await;
        }
        if owner.executable_sha256 == previous.executable_sha256
            && let Ok(evidence) = managed::verify_ready(&operation.launch_owner, &previous, READY_DEADLINE, &CancellationToken::new()).await {
                installation.abandon_trial(lease, Some(&operation.operation_id)).await?;
                operation.phase = Phase::RolledBack;
                operation.rollback = Some(serde_json::to_value(evidence)?);
                return journal::save(operation).await;
            }
    }
    managed::stop_owner(installation, &operation.launch_owner, STOP_DEADLINE).await?;
    installation.abandon_trial(lease, Some(&operation.operation_id)).await?;
    let restored = async {
        managed::start_owner(installation, &operation.launch_owner, READY_DEADLINE).await?;
        managed::verify_ready(&operation.launch_owner, &previous, READY_DEADLINE, &CancellationToken::new()).await
    }.await;
    let evidence = match restored {
        Ok(evidence) => evidence,
        Err(error) if operation.launch_owner.service.is_some()
            && !operation.launch_owner.executable.starts_with(installation.root().join("runtimes")) => {
            // Only first migration may restore the original service binding.
            // Later operations must not revive a much older installation.
            managed::stop_owner(installation, &operation.launch_owner, STOP_DEADLINE).await?;
            managed::restore_service(installation, lease, &operation.launch_owner).await?;
            managed::start_owner(installation, &operation.launch_owner, READY_DEADLINE).await?;
            let original = managed::original_runtime(&operation.launch_owner, &previous);
            managed::verify_ready(&operation.launch_owner, &original, READY_DEADLINE, &CancellationToken::new()).await
                .with_context(|| format!("Managed rollback failed ({error}); original service recovery also failed"))?
        }
        Err(error) => return Err(error),
    };
    operation.phase = Phase::RolledBack;
    operation.rollback = Some(serde_json::to_value(evidence)?);
    journal::save(operation).await
}

pub async fn execute(home: &Path, operation_id: &str) -> Result<i32> {
    super::validate_id(operation_id)?;
    let home = tokio::fs::canonicalize(home).await?;
    let _execution = journal::Lease::acquire(&home, "executor")?;
    let installation = Installation::for_home(&home)?;
    let lease = installation.try_lease()?.context("Another executor owns runtime selection")?;
    let mut operation = journal::current(&home).await?.context("Native update was not durably admitted")?;
    ensure!(operation.operation_id == operation_id && operation.installation_root == installation.root(),
        "Native update executor identity or installation mismatch");
    if matches!(operation.phase, Phase::Failed | Phase::RolledBack) { return Ok(1); }
    let _runtime_lease = super::super::updater::lease(operation.launch_owner.executable.parent().context("Runtime executable has no parent")?)?;
    operation.executor = Some(serde_json::to_value(managed::executor_identity()?)?);
    journal::save(&mut operation).await?;
    let result = async {
        journal::ensure_no_legacy_update(&home).await?;
        match operation.phase {
            Phase::Admitted | Phase::Preparing => stage(&mut operation, &installation, &lease).await?,
            Phase::RollingBack => return rollback(&mut operation, &installation, &lease).await,
            _ => {},
        }
        if operation.phase != Phase::Committed {
            activate(&mut operation, &installation, &lease).await?;
        } else {
            let candidate = operation.candidate.as_ref().context("Committed runtime identity missing")?;
            installation.commit_trial(&lease, operation_id, candidate).await?;
        }
        Ok(())
    }.await;
    if let Err(error) = result {
        operation.error_code = Some(if error.to_string().starts_with("UPDATE_BUSY:") { "UPDATE_BUSY" } else { "UPDATE_FAILED" }.into());
        operation.error = Some(error.to_string());
        if operation.phase == Phase::Committed {
            // Verification was durable already; a selection/cleanup/continuation
            // failure must not roll back the healthy committed runtime.
            journal::save(&mut operation).await?;
            return Err(error);
        }
        if matches!(operation.phase, Phase::Activating | Phase::Verifying | Phase::RollingBack) {
            if let Err(rollback_error) = rollback(&mut operation, &installation, &lease).await {
                operation.phase = Phase::Failed;
                operation.rollback = Some(json!({"error":rollback_error.to_string()}));
            }
        } else {
            operation.phase = Phase::Failed;
        }
        journal::save(&mut operation).await?;
    }
    if let Err(error) = release_drain(&operation).await {
        tracing::warn!(operation_id, %error, "Native update admission release was not acknowledged");
    }
    if operation.phase == Phase::Committed {
        if let Err(error) = continuation::resume(&mut operation).await {
            if let Some(continuation) = &mut operation.continuation {
                continuation.state = if continuation.attempted_at.is_some() { super::ContinuationState::Unknown } else { super::ContinuationState::NotDelivered };
                continuation.error = Some(error.to_string());
            }
            journal::save(&mut operation).await?;
        }
        let cleanup = async {
            let (operations, runtimes) = journal::retained(&home).await?;
            installation.cleanup(&lease, &operations, &runtimes).await
        }.await;
        if let Err(error) = cleanup {
            operation.error = Some(format!("Runtime committed; owned cleanup failed: {error}"));
            journal::save(&mut operation).await?;
        }
        Ok(0)
    } else {
        Ok(1)
    }
}

pub async fn recover_bootstrap(home: &Path) -> Result<()> {
    let Some(mut operation) = journal::current(home).await? else { return Ok(()); };
    let installation = Installation::for_home(home)?;
    let Some(lease) = installation.try_lease()? else { return Ok(()); };
    match operation.phase {
        Phase::Committed => {
            let candidate = operation.candidate.as_ref().context("Committed runtime identity missing")?;
            installation.commit_trial(&lease, &operation.operation_id, candidate).await?;
            if operation.continuation.as_ref().is_none_or(|value| value.state != super::ContinuationState::Pending) { return Ok(()); }
        }
        Phase::Activating | Phase::Verifying | Phase::RollingBack => {
            installation.abandon_trial(&lease, Some(&operation.operation_id)).await?;
            operation.phase = Phase::RollingBack;
            operation.error = Some("Executor ended before runtime commit; bootstrap selected the known-good runtime".into());
            journal::save(&mut operation).await?;
        }
        Phase::Admitted | Phase::Preparing | Phase::Staged | Phase::Draining => {
            operation.phase = Phase::Failed;
            operation.error = Some("Executor ended before activation; the selected runtime is unchanged".into());
            journal::save(&mut operation).await?;
            return Ok(());
        }
        Phase::Failed | Phase::RolledBack => {
            installation.abandon_trial(&lease, Some(&operation.operation_id)).await?;
            return Ok(());
        }
    }
    // Release before handoff: the independently launched executor must acquire
    // this same lease. It observes the launcher starting the selected runtime.
    drop(lease);
    managed::spawn_executor(home, &operation.operation_id, &operation.launch_owner, &std::env::current_exe()?).await?;
    Ok(())
}
