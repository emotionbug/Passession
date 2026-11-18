//! Give an old desktop supervisor's durable drain RPC an authenticated, live
//! executor record. No update candidate is selected and no force is permitted.
use super::{Operation, Phase, SubmitRequest, UpdateSource, journal, now};
use crate::daemon_control::{self, DaemonInstance, StopOptions, managed};
use crate::transport::{MIGRATION_CREDENTIAL, read_daemon_credential, storage};
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{path::Path, time::Duration};
use tokio_util::sync::CancellationToken;

pub(super) const LEGACY: &str = "desktop-legacy-migration";

pub(crate) async fn handover(
    home: &Path,
    instance: &DaemonInstance,
    installation_root: &Path,
    cancel: &CancellationToken,
) -> Result<bool> {
    ensure!(instance.desktop_managed, "Not a legacy desktop supervisor");
    let mut connect = crate::client::ConnectOptions::new(paseo_client::DaemonTarget::Instance {
        home: home.to_owned(),
    });
    connect.timeout = Duration::from_secs(5);
    connect.instance = Some(instance.clone());
    let client = crate::client::connect(connect, cancel.clone()).await?;
    let result = async {
        if client.server_info()["features"]["durableDaemonUpdates"] != true {
            return Ok(false); // Cannot establish a safe admission barrier on older releases.
        }
        let server_id = instance.server_id.as_deref().context("Legacy daemon has no server ID")?;
        ensure!(client.server_info()["serverId"] == server_id, "Legacy server identity changed");
        let owner = managed::legacy_migration_owner(home, instance, &client).await?;
        let _admission = journal::Lease::acquire(home, "admission")?;
        // Old desktop updaters take this lane for their cutover. Never interrupt one.
        let _old_app_cutover = match journal::Lease::acquire(home, "desktop-cutover") {
            Ok(lease) => lease,
            Err(_) => return Ok(false),
        };
        let _execution = journal::Lease::acquire(home, "executor")?;
        let mut previous = journal::current(home).await?;
        if let Some(operation) = &previous
            && operation.owner["kind"] == LEGACY
        {
            if let Some(handle) = &operation.executor
                && managed::executor_alive(&serde_json::from_value(handle.clone())?)?
            {
                return Ok(false);
            }
            // A prior ensure died. The old daemon's drain watcher releases its
            // pause when this executor dies, independently of this cleanup.
            journal::restore_migration(operation).await?;
            previous = journal::current(home).await?;
        }
        if previous.as_ref().is_some_and(|operation| !operation.terminal()) {
            return Ok(false);
        }

        let id = uuid::Uuid::new_v4().to_string();
        let timestamp = now();
        let mut record = Operation {
            schema_version: 2,
            operation_id: id.clone(),
            request_fingerprint: id.clone(),
            request: SubmitRequest {
                operation_id: id.clone(),
                source: UpdateSource::Release,
                expected_release: None,
                resume: None,
                idle_timeout_ms: 500,
                force_running: false,
            },
            input_digest: None,
            home: home.to_owned(),
            server_id: server_id.to_owned(),
            installation_root: installation_root.to_owned(),
            owner: json!({"kind":LEGACY,"previousOperationId":previous.as_ref().map(|op| &op.operation_id)}),
            launch_owner: owner,
            phase: Phase::Draining,
            previous: None,
            candidate: None,
            executor: Some(serde_json::to_value(managed::executor_identity()?)?),
            worker: None,
            original_worker: serde_json::to_value(instance)?,
            continuation: None,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            error: None,
            error_code: None,
            rollback: None,
        };
        journal::admit(&mut record).await?;
        let credential_path = home.join(MIGRATION_CREDENTIAL);
        let migrate = async {
            let drained = client
                .request(
                    json!({"type":"daemon.update.drain.request","operationId":id}),
                    "daemon.update.drain.response",
                    Duration::from_secs(5),
                )
                .await?;
            ensure!(drained["operationId"] == id, "Legacy drain acknowledged a different operation");
            if drained["ready"] != true || drained["forced"] != false {
                return Ok(false);
            }
            ensure!(!cancel.is_cancelled(), "Operation cancelled");
            let credential = read_daemon_credential(&home.join("local-credential"))
                .await?
                .context("Legacy daemon has no local credential")?;
            // Persist the exact identity advertised by the verified supervisor,
            // even if the old GUI selected it through an environment override.
            let server_path = home.join("server-id");
            if storage::read_private(&server_path).await?.as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .is_none_or(|stored| stored.trim() != server_id)
            {
                storage::write_private(&server_path, format!("{server_id}\n").as_bytes()).await?;
            }
            storage::write_private(&credential_path, credential.as_bytes()).await?;
            // Once shutdown is requested, complete it even if the invoking app
            // disappears. No new work can enter between drain and shutdown.
            daemon_control::stop_daemon_instance(
                home,
                StopOptions { instance: Some(instance.clone()), ..Default::default() },
                &CancellationToken::new(),
            )
            .await?;
            Ok::<_, anyhow::Error>(true)
        }
        .await;

        if !matches!(migrate, Ok(true)) {
            // Restore the running legacy daemon's admission even if the drain
            // response was lost; the watcher also releases an orphaned pause.
            record.phase = Phase::Failed;
            journal::save(&mut record).await?;
            let _ = client
                .request(
                    json!({"type":"daemon.update.drain.release.request","operationId":id}),
                    "daemon.update.drain.release.response",
                    Duration::from_secs(5),
                )
                .await;
        }
        journal::restore_migration(&record).await?;
        migrate
    }
    .await;
    client.close().await;
    result
}
