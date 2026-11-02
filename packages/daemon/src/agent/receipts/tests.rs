use super::*;
use crate::providers::api::PromptDeliveryError;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Hooks {
    exists: AtomicBool,
    validations: AtomicUsize,
    reject_completed: AtomicBool,
}
#[async_trait]
impl CreationHooks for Hooks {
    async fn exists(&self, _: &str, _: &str) -> Result<bool> {
        Ok(self.exists.load(Ordering::SeqCst))
    }
    async fn validate_completed(&self, _: &Value) -> Result<()> {
        self.validations.fetch_add(1, Ordering::SeqCst);
        if self.reject_completed.load(Ordering::SeqCst) {
            bail!("Previously created agent no longer exists");
        }
        Ok(())
    }
}
fn input(kind: &str) -> CreationInput {
    CreationInput {
        kind: kind.to_owned(),
        key: "intent".to_owned(),
        request: json!({"prompt":"hello","config":{"cwd":"/project","provider":"custom"}}),
        workspace_id: Some("wks_0123456789abcdef".to_owned()),
        agent_id: Some("agent".to_owned()),
        has_agent: true,
        has_prompt: true,
    }
}
async fn run(
    store: &CreationReceipts,
    input: &CreationInput,
    hooks: &Hooks,
) -> Result<CreationGuard> {
    match store.begin(input, hooks).await? {
        CreationAdmission::Run(guard) => Ok(guard),
        _ => bail!("Expected creation runner"),
    }
}

async fn owned(receipts: &MessageReceipts, message_id: &str, request: &Value) -> Result<LiveGuard> {
    match receipts.admit("agent", message_id, request).await? {
        Admission::Run(owner) => Ok(owner),
        _ => bail!("Expected send owner"),
    }
}
async fn replayed(receipts: &MessageReceipts, message_id: &str, request: &Value) -> Result<Value> {
    match receipts.admit("agent", message_id, request).await? {
        Admission::Replay(receipt) => Ok(receipt),
        _ => bail!("Expected receipt replay"),
    }
}
fn send_key(message_id: &str) -> Result<String> {
    digest(&json!(["send", "agent", message_id]))
}

#[tokio::test]
async fn external_receipt_observer_preserves_live_owner_and_sees_late_ack() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"continue"});
    let owner = owned(&receipts, "resume", &request).await?;
    assert!(receipts.begin_dispatch("agent", "resume").await?);
    let before = receipts.get("agent", "resume").await?;
    let observed = MessageReceipts::observe_persisted(home.path(), "agent", "resume").await?;
    assert_eq!(observed["phase"], "dispatching");
    assert_eq!(observed["revision"], before["revision"]);
    assert_eq!(receipts.get("agent", "resume").await?, before);
    receipts.settle("agent", "resume", Settlement::Accepted {
        result: Some(json!({"disposition":"steered"})),
        correlation: json!({"nativeMessageId":"native-resume"}),
    }).await?;
    drop(owner);
    let accepted = MessageReceipts::observe_persisted(home.path(), "agent", "resume").await?;
    assert_eq!(accepted["phase"], "accepted");
    assert_eq!(accepted["correlation"]["nativeMessageId"], "native-resume");
    assert_eq!(replayed(&receipts, "resume", &request).await?["phase"], "accepted");
    assert_eq!(MessageReceipts::observe_persisted(home.path(), "agent", "absent").await?["phase"], "missing");
    Ok(())
}

#[tokio::test]
async fn completed_and_rejected_sends_replay_without_dispatch() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"hello","activeTurnBehavior":"interrupt"});
    let owner = owned(&receipts, "message", &request).await?;
    assert!(receipts.begin_dispatch("agent", "message").await?);
    let result = json!({"disposition":"turn_started","turnId":"turn-1"});
    receipts
        .settle(
            "agent",
            "message",
            Settlement::Submitted { result: result.clone(), correlation: json!({"turnId":"turn-1"}), ack_pending: false },
        )
        .await?;
    drop(owner);
    let restarted = MessageReceipts::new(home.path().to_owned());
    let receipt = replayed(&restarted, "message", &request).await?;
    assert_eq!(receipt["phase"], "submitted");
    assert_eq!(receipt["correlation"]["acknowledgementPending"], false);
    let replay_result = replay(receipt)?;
    assert_eq!(replay_result["turnId"], "turn-1");
    assert_eq!(replay_result["deduplicated"], true);
    let conflict = restarted
        .admit("agent", "message", &json!({"prompt":"changed"}))
        .await
        .err()
        .unwrap();
    assert_eq!(conflict.to_string(), "agent_request_key_conflict");
    assert_eq!(
        conflict.downcast_ref::<SendReceiptError>().unwrap().receipt["phase"],
        "submitted"
    );

    let owner = owned(&receipts, "rejected", &request).await?;
    assert!(receipts.begin_dispatch("agent", "rejected").await?);
    let refused = PromptDeliveryError::rejected("Provider did not accept steering");
    receipts
        .settle("agent", "rejected", Settlement::failure(&refused, true))
        .await?;
    drop(owner);
    let receipt = replayed(&restarted, "rejected", &request).await?;
    assert_eq!(receipt["phase"], "rejected");
    assert_eq!(receipt["mayHaveExecuted"], false);
    assert_eq!(receipt["canRetry"], false);
    assert_eq!(
        replay(receipt).unwrap_err().to_string(),
        "Provider did not accept steering"
    );
    Ok(())
}

#[tokio::test]
async fn live_duplicate_waits_while_queries_and_dismissal_do_not() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"hello"});
    let owner = owned(&receipts, "message", &request).await?;
    let Admission::Wait(mut settled) = receipts.admit("agent", "message", &request).await? else {
        panic!("a live duplicate must join its owner");
    };
    // The owner holds no receipt lock while it prepares or dispatches.
    let receipt = tokio::time::timeout(Duration::from_secs(1), receipts.get("agent", "message")).await??;
    assert_eq!(receipt["phase"], "preparing");
    assert_eq!(receipt["mayHaveExecuted"], false);
    assert!(receipts.begin_dispatch("agent", "message").await?);
    let dismissed =
        tokio::time::timeout(Duration::from_secs(1), receipts.dismiss("agent", "message")).await??;
    assert_eq!(dismissed["phase"], "dispatching");
    assert_eq!(dismissed["dismissed"], true);
    assert_eq!(dismissed["mayHaveExecuted"], true);
    receipts
        .settle(
            "agent",
            "message",
            Settlement::Accepted { result: Some(json!({"disposition":"steered"})), correlation: json!({}) },
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(1), settled.wait_for(|done| *done)).await??;
    drop(owner);
    let receipt = replayed(&receipts, "message", &request).await?;
    assert_eq!(receipt["phase"], "accepted");
    assert_eq!(receipt["dismissed"], true);
    assert_eq!(replay(receipt).unwrap_err().to_string(), DISMISSED);
    Ok(())
}

#[tokio::test]
async fn dismissal_is_sticky_through_late_acknowledgement_and_unknown_ids() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"hello"});
    // Before the boundary, dismissal proves the prompt never dispatches.
    let owner = owned(&receipts, "early", &request).await?;
    receipts.dismiss("agent", "early").await?;
    let gate = DispatchGate::new("agent", vec!["early".to_owned()], true);
    let error = receipts.begin(&gate).await.unwrap_err();
    receipts
        .settle("agent", "early", Settlement::failure(&error, gate.invoked()))
        .await?;
    drop(owner);
    let early = receipts.get("agent", "early").await?;
    assert_eq!(early["phase"], "not_delivered");
    assert_eq!(early["error"], DISMISSED);
    assert_eq!(early["mayHaveExecuted"], false);
    assert_eq!(early["canRetry"], false);
    assert_eq!(
        replay(replayed(&receipts, "early", &request).await?).unwrap_err().to_string(),
        DISMISSED
    );

    // After submission, a late native ACK improves evidence but cannot revive it.
    let owner = owned(&receipts, "late", &request).await?;
    assert!(receipts.begin_dispatch("agent", "late").await?);
    receipts
        .settle(
            "agent",
            "late",
            Settlement::Submitted { result: json!({"disposition":"turn_started","turnId":"t"}), correlation: json!({"turnId":"t"}), ack_pending: true },
        )
        .await?;
    let follower = receipts.claim("agent", "late")?;
    drop(owner);
    let pending = receipts.dismiss("agent", "late").await?;
    assert_eq!(pending["phase"], "submitted");
    assert_eq!(pending["correlation"]["acknowledgementPending"], true);
    receipts
        .settle(
            "agent",
            "late",
            crate::providers::api::PromptAck::Accepted(json!({"nativeRequestId":"req_7"})).into(),
        )
        .await?;
    drop(follower);
    let late = replayed(&receipts, "late", &request).await?;
    assert_eq!(late["phase"], "accepted");
    assert_eq!(late["dismissed"], true);
    assert_eq!(late["correlation"]["acknowledgement"]["nativeRequestId"], "req_7");
    assert_eq!(replay(late).unwrap_err().to_string(), DISMISSED);

    // A request the host never saw can be tombstoned before a retry arrives.
    let tombstone = receipts.dismiss("agent", "never-seen").await?;
    assert_eq!(tombstone["phase"], "missing");
    assert_eq!(tombstone["dismissed"], true);
    assert_eq!(tombstone["revision"], 1);
    assert_eq!(
        replay(replayed(&receipts, "never-seen", &request).await?).unwrap_err().to_string(),
        DISMISSED
    );
    // The same request under a new message ID is a distinct, valid send.
    owned(&receipts, "new-id", &request).await?;
    Ok(())
}

#[tokio::test]
async fn orphaned_phases_reconcile_conservatively_after_restart() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = MessageReceipts::new(home.path().to_owned());
    let request = json!({"prompt":"hello"});
    let _preparing = owned(&receipts, "preparing", &request).await?;
    let _dispatching = owned(&receipts, "dispatching", &request).await?;
    assert!(receipts.begin_dispatch("agent", "dispatching").await?);
    let _acknowledging = owned(&receipts, "acknowledging", &request).await?;
    assert!(receipts.begin_dispatch("agent", "acknowledging").await?);
    receipts
        .settle(
            "agent",
            "acknowledging",
            Settlement::Submitted { result: json!({"disposition":"turn_started"}), correlation: json!({}), ack_pending: true },
        )
        .await?;
    let restarted = MessageReceipts::new(home.path().to_owned());
    let preparing = restarted.get("agent", "preparing").await?;
    assert_eq!(preparing["phase"], "not_delivered");
    assert_eq!(preparing["canRetry"], true);
    owned(&restarted, "preparing", &request).await?;
    for id in ["dispatching", "acknowledging"] {
        let unknown = restarted.get("agent", id).await?;
        assert_eq!(unknown["phase"], "outcome_unknown", "{id}");
        assert_eq!(unknown["mayHaveExecuted"], true, "{id}");
        assert_eq!(unknown["canRetry"], false, "{id}");
        assert_eq!(
            replay(replayed(&restarted, id, &request).await?).unwrap_err().to_string(),
            OUTCOME_UNKNOWN
        );
    }
    Ok(())
}

#[tokio::test]
async fn stored_receipts_replay_and_versionless_records_are_errors() -> Result<()> {
    let home = tempfile::tempdir()?;
    let directory = home.path().join("agent-requests");
    tokio::fs::create_dir_all(&directory).await?;
    let request = json!({"prompt":"hello"});
    let fingerprint = digest(&request)?;
    // Receipts written by earlier native daemons also carry a `state` field.
    for (id, phase, dismissed) in [("submitted", "submitted", false), ("dismissed", "outcome_unknown", true)] {
        tokio::fs::write(
            directory.join(format!("{}.json", send_key(id)?)),
            serde_json::to_vec(&json!({
                "version":2,"fingerprint":fingerprint,"agentId":"agent","messageId":id,
                "state":"pending","phase":phase,"revision":2,"dismissed":dismissed,
                "result":null,"error":null,"updatedAt":null,"correlation":null,"ackPending":false
            }))?,
        )
        .await?;
    }
    let receipts = MessageReceipts::new(home.path().to_owned());
    let submitted = replay(replayed(&receipts, "submitted", &request).await?)?;
    assert_eq!(submitted["deduplicated"], true);
    assert_eq!(submitted["receipt"]["revision"], 2);
    assert_eq!(
        replay(replayed(&receipts, "dismissed", &request).await?).unwrap_err().to_string(),
        DISMISSED
    );

    let versionless = directory.join(format!("{}.json", send_key("versionless")?));
    let bytes = serde_json::to_vec(&json!({"fingerprint":fingerprint,"agentId":"agent","state":"completed"}))?;
    tokio::fs::write(&versionless, &bytes).await?;
    assert!(receipts.get("agent", "versionless").await.is_err());
    assert_eq!(tokio::fs::read(&versionless).await?, bytes);
    Ok(())
}
#[tokio::test]
async fn duplicate_joins_and_conflict_is_rejected_before_runner_finishes() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let hooks = Hooks::default();
    let input = input("workspace");
    let mut runner = run(&receipts, &input, &hooks).await?;
    runner.begin_stage("workspace").await?;
    runner
        .workspace_ready(json!({"id":"wks_0123456789abcdef"}), None)
        .await?;
    let CreationAdmission::Running(mut duplicate) = receipts.begin(&input, &hooks).await? else {
        panic!("duplicate must join");
    };
    assert_eq!(
        duplicate.snapshot.as_ref().unwrap()["phase"],
        "workspace_ready"
    );
    let mut conflict = input.clone();
    conflict.request["prompt"] = json!("different");
    assert_eq!(
        receipts
            .begin(&conflict, &hooks)
            .await
            .err()
            .unwrap()
            .to_string(),
        "workspace_request_key_conflict"
    );
    runner.begin_stage("agent").await?;
    runner.agent_ready(json!({"id":"agent"}), true).await?;
    runner.prompt_started(json!({"id":"agent"})).await?;
    runner.complete(None).await?;
    assert_eq!(duplicate.completion().await?["phase"], "completed");
    drop(runner);
    let CreationAdmission::Replay(replayed) = CreationReceipts::new(home.path().to_owned())
        .begin(&input, &hooks)
        .await?
    else {
        panic!("must replay");
    };
    assert_eq!(replayed["phase"], "completed");
    assert_eq!(hooks.validations.load(Ordering::SeqCst), 1);
    Ok(())
}
#[tokio::test]
async fn claims_reject_different_intents_and_survive_restart() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let hooks = Hooks::default();
    let first = input("workspace");
    let runner = run(&receipts, &first, &hooks).await?;
    let mut other = first.clone();
    other.key = "other".to_owned();
    assert_eq!(
        receipts
            .begin(&other, &hooks)
            .await
            .err()
            .unwrap()
            .to_string(),
        "workspace_id_conflict"
    );
    drop(runner);
    let mut restarted = run(
        &CreationReceipts::new(home.path().to_owned()),
        &first,
        &hooks,
    )
    .await?;
    assert_eq!(
        restarted.snapshot()["workspaceId"],
        first.workspace_id.unwrap()
    );
    restarted.complete(None).await?;
    Ok(())
}
#[tokio::test]
async fn retryable_agent_failure_preserves_workspace_and_generated_ids() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let hooks = Hooks::default();
    let mut input = input("workspace");
    input.agent_id = None;
    input.workspace_id = None;
    let mut runner = run(&receipts, &input, &hooks).await?;
    let agent_id = runner.snapshot()["agentId"].clone();
    let workspace_id = runner.snapshot()["workspaceId"].clone();
    assert_eq!(workspace_id.as_str().unwrap().len(), 20);
    runner.begin_stage("workspace").await?;
    runner
        .workspace_ready(json!({"id":workspace_id}), None)
        .await?;
    runner.begin_stage("agent").await?;
    runner.fail("provider unavailable", None, &hooks).await?;
    assert_eq!(runner.snapshot()["outcomeUnknown"], false);
    drop(runner);
    let retry = run(
        &CreationReceipts::new(home.path().to_owned()),
        &input,
        &hooks,
    )
    .await?;
    assert_eq!(retry.snapshot()["phase"], "workspace_ready");
    assert_eq!(retry.snapshot()["agentId"], agent_id);
    assert_eq!(retry.snapshot()["workspaceId"], workspace_id);
    assert!(retry.snapshot().get("failedStage").is_none());
    assert!(retry.snapshot().get("outcomeUnknown").is_none());
    Ok(())
}
#[tokio::test]
async fn interrupted_prompt_and_partial_workspace_remain_unknown() -> Result<()> {
    for stage in ["workspace", "prompt"] {
        let home = tempfile::tempdir()?;
        let receipts = CreationReceipts::new(home.path().to_owned());
        let hooks = Hooks::default();
        let input = input("workspace");
        let mut runner = run(&receipts, &input, &hooks).await?;
        runner.begin_stage(stage).await?;
        drop(runner);
        let recovered = CreationReceipts::new(home.path().to_owned());
        let subscription = recovered.subscribe(&input.kind, &input.key, &hooks).await?;
        let snapshot = subscription.snapshot.unwrap();
        assert_eq!(snapshot["phase"], "failed");
        assert_eq!(snapshot["failedStage"], stage);
        assert_eq!(snapshot["outcomeUnknown"], true);
        let CreationAdmission::Replay(replayed) = recovered.begin(&input, &hooks).await? else {
            panic!("ambiguous work must not run");
        };
        assert_eq!(replayed, snapshot);
    }
    Ok(())
}
#[tokio::test]
async fn ready_agent_without_prompt_can_complete_after_restart() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let hooks = Hooks::default();
    let mut input = input("agent");
    input.has_prompt = false;
    let mut runner = run(&receipts, &input, &hooks).await?;
    runner.begin_stage("agent").await?;
    runner.agent_ready(json!({"id":"agent"}), false).await?;
    drop(runner);
    let mut recovered = run(
        &CreationReceipts::new(home.path().to_owned()),
        &input,
        &hooks,
    )
    .await?;
    assert_eq!(recovered.snapshot()["agent"]["id"], "agent");
    recovered.complete(None).await?;
    hooks.reject_completed.store(true, Ordering::SeqCst);
    drop(recovered);
    assert_eq!(
        receipts
            .begin(&input, &hooks)
            .await
            .err()
            .unwrap()
            .to_string(),
        "Previously created agent no longer exists"
    );
    assert!(
        receipts
            .subscribe(&input.kind, &input.key, &hooks)
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn corrupt_receipts_are_errors_not_missing_receipts() -> Result<()> {
    let home = tempfile::tempdir()?;
    let input = input("agent");
    let path = home
        .path()
        .join("creations")
        .join(format!("{}.json", identity_for(&input.kind, &input.key)?));
    atomic_json(&path, &json!({"snapshot":{}})).await?;
    assert!(
        CreationReceipts::new(home.path().to_owned())
            .begin(&input, &Hooks::default())
            .await
            .is_err()
    );
    Ok(())
}
#[cfg(unix)]
#[tokio::test]
async fn receipts_and_claims_are_private() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let input = input("workspace");
    let _runner = run(&receipts, &input, &Hooks::default()).await?;
    let mut entries = tokio::fs::read_dir(home.path().join("creations")).await?;
    while let Some(entry) = entries.next_entry().await? {
        assert_eq!(entry.metadata().await?.permissions().mode() & 0o777, 0o600);
    }
    Ok(())
}

#[tokio::test]
async fn failed_milestone_commit_does_not_publish_or_mutate_snapshot() -> Result<()> {
    let home = tempfile::tempdir()?;
    let receipts = CreationReceipts::new(home.path().to_owned());
    let input = input("agent");
    let mut runner = run(&receipts, &input, &Hooks::default()).await?;
    let mut events = receipts.subscribe_events();
    let before = runner.snapshot().clone();
    let receipt_path = home
        .path()
        .join("creations")
        .join(format!("{}.json", identity_for(&input.kind, &input.key)?));
    tokio::fs::remove_file(&receipt_path).await?;
    tokio::fs::create_dir(&receipt_path).await?;
    assert!(
        runner
            .agent_ready(json!({"id":"agent"}), false)
            .await
            .is_err()
    );
    assert_eq!(runner.snapshot(), &before);
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    Ok(())
}
