use super::*;
use crate::agent::receipts::{Admission, MessageReceipts, Settlement};
use crate::daemon_control::managed::{LaunchOwner, OwnerKind, StandaloneLaunch};

fn fixture(home: &Path) -> Operation {
    let home = home.canonicalize().unwrap();
    let owner = LaunchOwner {
        kind: OwnerKind::Standalone,
        home: home.clone(),
        server_id: "server-one".into(),
        version: "0.13.1".into(),
        source_sha: "a".repeat(40),
        supervisor: crate::daemon_control::DaemonInstance {
            pid: 1,
            started_at: "2026-01-01T00:00:00Z".into(),
            hostname: "fixture".into(),
            uid: 0,
            listen: Some("127.0.0.1:1".into()),
            server_id: Some("server-one".into()),
            desktop_managed: false,
            heartbeat: false,
            boot_id: None,
            process_start_identity: Some("birth".into()),
        },
        executable: home.join("outside-runtime/paseo"),
        executable_sha256: "b".repeat(64),
        service: None,
        launch: Some(StandaloneLaunch {
            environment: vec![("SECRET".into(), "private-environment".into())],
            working_directory: None,
        }),
    };
    let id = uuid::Uuid::new_v4().to_string();
    Operation {
        schema_version: 2,
        operation_id: id.clone(),
        request_fingerprint: "request-digest".into(),
        request: SubmitRequest {
            operation_id: id,
            source: UpdateSource::Release,
            expected_release: None,
            resume: None,
            idle_timeout_ms: 1000,
            force_running: false,
        },
        input_digest: None,
        home: home.clone(),
        server_id: owner.server_id.clone(),
        installation_root: home.join("outside-installation"),
        owner: owner.public_summary(),
        original_worker: serde_json::to_value(&owner.supervisor).unwrap(),
        launch_owner: owner,
        phase: Phase::Admitted,
        previous: None,
        candidate: None,
        executor: None,
        worker: None,
        continuation: None,
        created_at: now(),
        updated_at: now(),
        error: None,
        error_code: None,
        rollback: None,
    }
}

#[test]
fn release_pin_preserves_unpinned_requests_and_changes_retry_fingerprint() -> Result<()> {
    let unpinned_request = json!({"operationId":uuid::Uuid::new_v4().to_string(),
        "source":{"kind":"release"},"resume":null,"idleTimeoutMs":300000,"forceRunning":false});
    let mut request: SubmitRequest = serde_json::from_value(unpinned_request.clone())?;
    assert!(request.expected_release.is_none());
    assert_eq!(serde_json::to_value(&request)?, unpinned_request);
    let unpinned = crate::json::fingerprint(&json!({"request":request,"inputDigest":null}))?;
    request.expected_release = Some(super::super::updater::ReleaseIdentity {
        version: "2.0.0".into(),
        manifest_sha512: "pinned-metadata".into(),
    });
    let pinned = crate::json::fingerprint(&json!({"request":request,"inputDigest":null}))?;
    assert_ne!(pinned, unpinned);
    let restored: SubmitRequest = serde_json::from_value(serde_json::to_value(&request)?)?;
    assert_eq!(restored.expected_release, request.expected_release);
    Ok(())
}

#[tokio::test]
async fn missing_operation_is_explicitly_retryable_without_mutation() -> Result<()> {
    let home = tempfile::tempdir()?;
    let id = uuid::Uuid::new_v4().to_string();
    let error = status(home.path(), &id).await.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<paseo_client::ClientError>()
            .unwrap()
            .code,
        "UPDATE_OPERATION_NOT_FOUND"
    );
    assert!(!home.path().join("native-update-operation.json").exists());
    Ok(())
}

#[tokio::test]
async fn journal_preserves_current_admission_and_archived_results_offline() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut first = fixture(home.path());
    journal::admit(&mut first).await?;
    let mut second = fixture(home.path());
    assert!(journal::admit(&mut second).await.is_err());
    assert_eq!(
        journal::current(home.path()).await?.unwrap().operation_id,
        first.operation_id
    );
    first.phase = Phase::Failed;
    first.error = Some("candidate was rejected before stop".into());
    journal::save(&mut first).await?;
    journal::admit(&mut second).await?;
    let archived = status(home.path(), &first.operation_id).await?;
    assert_eq!(archived["phase"], "failed");
    assert_eq!(archived["error"], "candidate was rejected before stop");
    assert_eq!(
        status(home.path(), &second.operation_id).await?["phase"],
        "admitted"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(home.path().join("native-update-operation.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    Ok(())
}

#[tokio::test]
async fn late_ack_changes_observed_continuation_without_rewriting_or_resending() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut operation = fixture(home.path());
    operation.phase = Phase::Committed;
    operation.request.resume = Some(ResumeRequest {
        agent_id: "agent".into(),
        text: "private-continuation-text".into(),
    });
    operation.continuation = Some(Continuation {
        agent_id: "agent".into(),
        message_id: "original-message".into(),
        provider: "omp".into(),
        session_id: json!("same-native-session"),
        native_handle: json!("original-file"),
        text_digest: "digest".into(),
        state: ContinuationState::Unknown,
        attempted_at: Some(now()),
        error: Some("acknowledgement unknown".into()),
    });
    journal::admit(&mut operation).await?;
    let before = std::fs::read(home.path().join("native-update-operation.json"))?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"private-continuation-text"});
    let Admission::Run(owner) = receipts
        .admit("agent", "original-message", &request)
        .await?
    else {
        panic!("new receipt")
    };
    assert!(receipts.begin_dispatch("agent", "original-message").await?);
    let unknown = status(home.path(), &operation.operation_id).await?;
    assert_eq!(unknown["phase"], "committed");
    assert_eq!(unknown["continuation"]["state"], "unknown");
    assert!(!unknown.to_string().contains("private-continuation-text"));
    assert!(!unknown.to_string().contains("private-environment"));
    receipts
        .settle(
            "agent",
            "original-message",
            Settlement::Accepted {
                result: Some(json!({"disposition":"steered"})),
                correlation: json!({"nativeMessageId":"native-message"}),
            },
        )
        .await?;
    drop(owner);
    let accepted = status(home.path(), &operation.operation_id).await?;
    assert_eq!(accepted["continuation"]["state"], "accepted");
    assert_eq!(accepted["continuation"]["messageId"], "original-message");
    assert!(accepted["continuation"]["error"].is_null());
    assert_eq!(
        std::fs::read(home.path().join("native-update-operation.json"))?,
        before
    );
    assert!(matches!(
        receipts
            .admit("agent", "original-message", &request)
            .await?,
        Admission::Replay(_)
    ));
    Ok(())
}

#[tokio::test]
async fn continuation_preserves_provider_identity_with_or_without_an_optional_native_handle()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let storage = crate::agent::storage::AgentStorage::new(home.path().to_owned());
    let request = ResumeRequest {
        agent_id: uuid::Uuid::new_v4().to_string(),
        text: "continue this session".into(),
    };
    let mut record = json!({
        "id":request.agent_id,"provider":"mock","cwd":home.path(),
        "createdAt":now(),"updatedAt":now(),
        "persistence":{"provider":"mock","sessionId":"original-session"}
    });
    storage.upsert(record.clone()).await?;
    let pinned = continuation::prepare(home.path(), Some(&request))
        .await?
        .unwrap();
    assert_eq!(pinned.provider, "mock");
    assert_eq!(pinned.session_id, "original-session");
    assert!(pinned.native_handle.is_null());
    record["persistence"]["nativeHandle"] = json!("provider-owned-handle");
    storage.upsert(record.clone()).await?;
    assert_eq!(
        continuation::prepare(home.path(), Some(&request))
            .await?
            .unwrap()
            .native_handle,
        "provider-owned-handle"
    );
    record["persistence"] = Value::Null;
    storage.upsert(record).await?;
    assert!(
        continuation::prepare(home.path(), Some(&request))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn paused_updates_allow_read_subscriptions_but_still_block_work() -> Result<()> {
    let home = tempfile::tempdir()?;
    let user = tempfile::tempdir()?;
    let app = crate::state::AppState::new(
        home.path().to_owned(),
        user.path().to_owned(),
        std::env::current_exe()?,
    );
    app.agents.update_admission.pause("update-one")?;
    for kind in [
        "session.events.set_subscription.request",
        "fetch_agents_request",
        "get_providers_snapshot_request",
    ] {
        assert!(drain::wire_admission(&app, &json!({"type":kind}))?.is_none());
    }
    assert_eq!(app.agents.update_admission.active()?, 0);
    for kind in ["fs.file.write.request", "hub.execution.control.request"] {
        assert!(drain::wire_admission(&app, &json!({"type":kind})).is_err());
    }
    assert_eq!(
        app.agents.update_admission.operation()?.as_deref(),
        Some("update-one")
    );
    app.agents.update_admission.release("update-one")?;
    let admitted = drain::wire_admission(&app, &json!({"type":"fs.file.write.request"}))?;
    assert!(admitted.is_some());
    assert_eq!(app.agents.update_admission.active()?, 1);
    drop(admitted);
    assert_eq!(app.agents.update_admission.active()?, 0);
    app.cancel.cancel();
    Ok(())
}

#[tokio::test]
async fn legacy_migration_drain_remains_pending_while_agent_admission_is_active() -> Result<()> {
    let home = tempfile::tempdir()?;
    let user = tempfile::tempdir()?;
    let app = crate::state::AppState::new(
        home.path().to_owned(),
        user.path().to_owned(),
        std::env::current_exe()?,
    );
    let mut previous = fixture(home.path());
    previous.phase = Phase::Committed;
    journal::admit(&mut previous).await?;
    let mut migration = fixture(home.path());
    migration.owner = json!({"kind":migration::LEGACY,"previousOperationId":previous.operation_id});
    migration.phase = Phase::Draining;
    migration.request.idle_timeout_ms = 1;
    migration.executor = Some(serde_json::to_value(
        crate::daemon_control::managed::executor_identity()?,
    )?);
    journal::admit(&mut migration).await?;
    let request =
        json!({"type":"daemon.update.drain.request","operationId":migration.operation_id});
    let running_work = app.agents.update_admission.enter()?;
    let drained = drain::handle_drain(&app, &request).await?;
    assert_eq!(drained["ready"], false);
    assert_eq!(drained["forced"], false);
    assert_eq!(drained["inflightAdmissions"], 1);
    assert!(app.agents.update_admission.enter().is_err());
    drop(running_work);
    let drained = drain::handle_drain(&app, &request).await?;
    assert_eq!(drained["ready"], true);
    migration.phase = Phase::Failed;
    journal::save(&mut migration).await?;
    drain::handle_drain(
        &app,
        &json!({"type":"daemon.update.drain.release.request","operationId":migration.operation_id}),
    )
    .await?;
    journal::restore_migration(&migration).await?;
    assert!(app.agents.update_admission.enter().is_ok());
    assert_eq!(
        journal::current(home.path()).await?.unwrap().operation_id,
        previous.operation_id
    );
    app.cancel.cancel();
    Ok(())
}

fn runtime(root: &Path, version: &str) -> RuntimeIdentity {
    RuntimeIdentity {
        root: root.to_owned(),
        version: version.into(),
        source_sha: String::new(),
        platform: "linux".into(),
        arch: "x64".into(),
        release: false,
        executable_sha256: format!("{version}-sha"),
        tree_digest: format!("{version}-tree"),
    }
}

#[tokio::test]
async fn committed_update_keeps_no_backup_runtime_while_rollback_keeps_known_good() -> Result<()> {
    use crate::daemon_control::managed::Installation;
    let base = tempfile::tempdir()?;
    let base = base.path().canonicalize()?;
    let home = base.join("home");
    std::fs::create_dir(&home)?;
    let root = base.join("install");
    std::fs::write(
        home.join("native-installation.json"),
        serde_json::to_vec(&json!({"schemaVersion":1,"root":root}))?,
    )?;
    let installation = Installation::for_home(&home)?;
    let runtimes = root.join("runtimes");
    for name in ["older", "old", "new"] {
        std::fs::create_dir_all(runtimes.join(name))?;
    }
    let old = runtime(&runtimes.join("old"), "1.0.0");
    let new = runtime(&runtimes.join("new"), "1.1.0");
    let retry = runtime(&runtimes.join("retry"), "1.2.0");
    // An earlier executor's committed update left its replaced runtime selected
    // as `previous`; the next cleanup must drop it.
    std::fs::write(
        root.join("installation.json"),
        serde_json::to_vec(&json!({
            "schemaVersion":1,"home":home,"serverId":"server-one",
            "control":{"path":root.join("bin/paseo"),"sha256":"control","protocol":1},
            "active":old,"previous":runtime(&runtimes.join("older"), "0.9.0"),
            "updatedAt":""
        }))?,
    )?;

    // Commit: once verified, the replaced runtime is no longer recovery material.
    let mut committed = fixture(&home);
    committed.installation_root = root.clone();
    committed.phase = Phase::Verifying;
    committed.previous = Some(old.clone());
    committed.candidate = Some(new.clone());
    journal::admit(&mut committed).await?;
    let lease = installation.try_lease()?.unwrap();
    installation
        .select_trial(&lease, &committed.operation_id, &new)
        .await?;
    assert!(journal::retained(&home).await?.1.contains(&old.root));
    committed.phase = Phase::Committed;
    journal::save(&mut committed).await?;
    drop(lease);
    recover_bootstrap(&home).await?;
    let selection = installation.load()?.unwrap();
    assert_eq!(selection.active.root, new.root);
    assert!(selection.trial.is_none());
    assert!(!std::fs::read_to_string(root.join("installation.json"))?.contains("previous"));
    assert!(new.root.exists());
    assert!(!old.root.exists());
    assert!(!runtimes.join("older").exists());

    // Rollback before commit: the known-good runtime stays selected and on disk.
    std::fs::create_dir_all(&retry.root)?;
    let mut rolled_back = fixture(&home);
    rolled_back.installation_root = root.clone();
    rolled_back.phase = Phase::Verifying;
    rolled_back.previous = Some(new.clone());
    rolled_back.candidate = Some(retry.clone());
    journal::admit(&mut rolled_back).await?;
    let lease = installation.try_lease()?.unwrap();
    installation
        .select_trial(&lease, &rolled_back.operation_id, &retry)
        .await?;
    installation
        .abandon_trial(&lease, Some(&rolled_back.operation_id))
        .await?;
    rolled_back.phase = Phase::RolledBack;
    journal::save(&mut rolled_back).await?;
    drop(lease);
    recover_bootstrap(&home).await?;
    let selection = installation.load()?.unwrap();
    assert_eq!(selection.active.root, new.root);
    assert!(selection.trial.is_none());
    assert!(new.root.exists());
    // Failed or rolled-back candidates remain for diagnosis.
    assert!(retry.root.exists());
    Ok(())
}
