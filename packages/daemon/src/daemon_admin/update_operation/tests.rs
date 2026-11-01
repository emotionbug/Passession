use super::*;
use crate::agent::receipts::{Admission, MessageReceipts, Settlement};
use crate::daemon_control::managed::{LaunchOwner, OwnerKind, StandaloneLaunch};

fn fixture(home: &Path) -> Operation {
    let home = home.canonicalize().unwrap();
    let owner = LaunchOwner {
        kind: OwnerKind::Standalone,
        home: home.clone(), server_id: "server-one".into(), version: "0.13.1".into(), source_sha: "a".repeat(40),
        supervisor: crate::daemon_control::DaemonInstance {
            pid: 1, started_at: "2026-01-01T00:00:00Z".into(), hostname: "fixture".into(), uid: 0,
            listen: Some("127.0.0.1:1".into()), server_id: Some("server-one".into()),
            desktop_managed: false, heartbeat: false, boot_id: None, process_start_identity: Some("birth".into()),
        },
        executable: home.join("outside-runtime/paseo"), executable_sha256: "b".repeat(64), service: None,
        launch: Some(StandaloneLaunch { environment: vec![("SECRET".into(), "private-environment".into())], working_directory: None }),
    };
    let id = uuid::Uuid::new_v4().to_string();
    Operation {
        schema_version: 2, operation_id: id.clone(), request_fingerprint: "request-digest".into(),
        request: SubmitRequest { operation_id: id, source: UpdateSource::Release,
            resume: None, idle_timeout_ms: 1000, force_running: false },
        input_digest: None, home: home.clone(), server_id: owner.server_id.clone(),
        installation_root: home.join("outside-installation"), owner: owner.public_summary(),
        original_worker: serde_json::to_value(&owner.supervisor).unwrap(), launch_owner: owner,
        phase: Phase::Admitted, previous: None, candidate: None, executor: None, worker: None,
        continuation: None, created_at: now(), updated_at: now(), error: None, error_code: None, rollback: None,
    }
}

#[tokio::test]
async fn journal_preserves_current_admission_and_archived_results_offline() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut first = fixture(home.path());
    journal::admit(&mut first).await?;
    let mut second = fixture(home.path());
    assert!(journal::admit(&mut second).await.is_err());
    assert_eq!(journal::current(home.path()).await?.unwrap().operation_id, first.operation_id);
    first.phase = Phase::Failed;
    first.error = Some("candidate was rejected before stop".into());
    journal::save(&mut first).await?;
    journal::admit(&mut second).await?;
    let archived = status(home.path(), &first.operation_id).await?;
    assert_eq!(archived["phase"], "failed");
    assert_eq!(archived["error"], "candidate was rejected before stop");
    assert_eq!(status(home.path(), &second.operation_id).await?["phase"], "admitted");
    assert!(!home.path().join("native-update.json").exists());
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(home.path().join("native-update-operation.json"))?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

#[tokio::test]
async fn legacy_recovery_is_never_overwritten_by_the_new_engine() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("native-update.json");
    let original = b"legacy or damaged transaction bytes";
    std::fs::write(&path, original)?;
    assert!(journal::ensure_no_legacy_update(home.path()).await.is_err());
    assert_eq!(std::fs::read(&path)?, original);
    assert!(!home.path().join("native-update-operation.json").exists());
    Ok(())
}

#[tokio::test]
async fn late_ack_changes_observed_continuation_without_rewriting_or_resending() -> Result<()> {
    let home = tempfile::tempdir()?;
    let mut operation = fixture(home.path());
    operation.phase = Phase::Committed;
    operation.request.resume = Some(ResumeRequest { agent_id: "agent".into(), text: "private-continuation-text".into() });
    operation.continuation = Some(Continuation {
        agent_id: "agent".into(), message_id: "original-message".into(), provider: "omp".into(),
        session_id: json!("same-native-session"), native_handle: json!("original-file"),
        text_digest: "digest".into(), state: ContinuationState::Unknown,
        attempted_at: Some(now()), error: Some("acknowledgement unknown".into()),
    });
    journal::admit(&mut operation).await?;
    let before = std::fs::read(home.path().join("native-update-operation.json"))?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"private-continuation-text"});
    let Admission::Run(owner) = receipts.admit("agent", "original-message", &request).await? else { panic!("new receipt") };
    assert!(receipts.begin_dispatch("agent", "original-message").await?);
    let unknown = status(home.path(), &operation.operation_id).await?;
    assert_eq!(unknown["phase"], "committed");
    assert_eq!(unknown["continuation"]["state"], "unknown");
    assert!(!unknown.to_string().contains("private-continuation-text"));
    assert!(!unknown.to_string().contains("private-environment"));
    receipts.settle("agent", "original-message", Settlement::Accepted {
        result: Some(json!({"disposition":"steered"})), correlation: json!({"nativeMessageId":"native-message"}),
    }).await?;
    drop(owner);
    let accepted = status(home.path(), &operation.operation_id).await?;
    assert_eq!(accepted["continuation"]["state"], "accepted");
    assert_eq!(accepted["continuation"]["messageId"], "original-message");
    assert!(accepted["continuation"]["error"].is_null());
    assert_eq!(std::fs::read(home.path().join("native-update-operation.json"))?, before);
    assert!(matches!(receipts.admit("agent", "original-message", &request).await?, Admission::Replay(_)));
    Ok(())
}

#[tokio::test]
async fn continuation_preserves_provider_identity_with_or_without_an_optional_native_handle() -> Result<()> {
    let home = tempfile::tempdir()?;
    let storage = crate::agent::storage::AgentStorage::new(home.path().to_owned());
    let request = ResumeRequest { agent_id: uuid::Uuid::new_v4().to_string(), text: "continue this session".into() };
    let mut record = json!({
        "id":request.agent_id,"provider":"mock","cwd":home.path(),
        "createdAt":now(),"updatedAt":now(),
        "persistence":{"provider":"mock","sessionId":"original-session"}
    });
    storage.upsert(record.clone()).await?;
    let pinned = continuation::prepare(home.path(), Some(&request)).await?.unwrap();
    assert_eq!(pinned.provider, "mock");
    assert_eq!(pinned.session_id, "original-session");
    assert!(pinned.native_handle.is_null());
    record["persistence"]["nativeHandle"] = json!("provider-owned-handle");
    storage.upsert(record.clone()).await?;
    assert_eq!(continuation::prepare(home.path(), Some(&request)).await?.unwrap().native_handle, "provider-owned-handle");
    record["persistence"] = Value::Null;
    storage.upsert(record).await?;
    assert!(continuation::prepare(home.path(), Some(&request)).await.is_err());
    Ok(())
}
