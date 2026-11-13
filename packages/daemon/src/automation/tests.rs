use super::*;

async fn service() -> (tempfile::TempDir, AutomationService) {
    let dir = tempfile::tempdir().unwrap();
    let now = date("2026-10-01T12:00:00Z").unwrap();
    let service = AutomationService::with_clock(Weak::new(), Arc::new(move || now));
    assert!(
        service
            .store
            .set(Store::open(dir.path().join("schedules")).await.unwrap())
            .is_ok()
    );
    (dir, service)
}
fn input() -> Value {
    json!({"name":" heartbeat ","prompt":" check status ","cadence":{"type":"every","everyMs":60000},"target":{"type":"agent","agentId":"00000000-0000-4000-8000-000000000001"}})
}
#[tokio::test]
async fn named_upsert_keeps_identity_and_history_but_rpc_create_does_not() {
    let (_dir, service) = service().await;
    let created = service.create_or_replace(&input()).await.unwrap();
    let id = created["id"].as_str().unwrap();
    let (_, run_id) = service
        .claim(id, true, service.now())
        .await
        .unwrap()
        .unwrap();
    service
        .finish(id, &run_id, true, Err(anyhow!("fixture failure")))
        .await
        .unwrap();
    let mut refreshed = input();
    refreshed["prompt"] = json!("new prompt");
    let updated = service.create_or_replace(&refreshed).await.unwrap();
    assert_eq!(updated["id"], created["id"]);
    assert_eq!(updated["runs"].as_array().unwrap().len(), 1);
    assert_eq!(updated["runs"][0]["error"], "fixture failure");
    assert_ne!(
        service.create(&refreshed).await.unwrap()["id"],
        created["id"]
    );
}
#[tokio::test]
async fn claim_is_durable_and_cannot_replay_after_uncertain_finalization() {
    let (dir, service) = service().await;
    let created = service.create(&input()).await.unwrap();
    let id = created["id"].as_str().unwrap();
    let (first, second) = tokio::join!(
        service.claim(id, false, service.now()),
        service.claim(id, false, service.now())
    );
    assert_eq!(
        usize::from(first.unwrap().is_some()) + usize::from(second.unwrap().is_some()),
        1
    );
    assert!(service.claim(id, true, service.now()).await.is_err());
    let now = date("2026-10-02T12:00:00Z").unwrap();
    let restarted = AutomationService::with_clock(Weak::new(), Arc::new(move || now));
    assert!(
        restarted
            .store
            .set(Store::open(dir.path().join("schedules")).await.unwrap())
            .is_ok()
    );
    restarted.recover().await.unwrap();
    let schedule = restarted.inspect(id).await.unwrap();
    assert_eq!(schedule["runs"][0]["status"], "failed");
    assert_eq!(
        schedule["runs"][0]["error"],
        "Daemon restarted before the scheduled run completed"
    );
    assert_eq!(schedule["nextRunAt"], "2026-10-02T12:01:00.000Z");
    assert!(restarted.claim(id, false, now).await.unwrap().is_none());
}
#[tokio::test]
async fn manual_paused_runs_do_not_advance_or_complete_cadence() {
    let (_dir, service) = service().await;
    let mut value = input();
    value["maxRuns"] = json!(1);
    let created = service.create(&value).await.unwrap();
    let id = created["id"].as_str().unwrap();
    service.pause(id).await.unwrap();
    let (_, run_id) = service
        .claim(id, true, service.now())
        .await
        .unwrap()
        .unwrap();
    service
        .finish(id, &run_id, true, Err(anyhow!("manual")))
        .await
        .unwrap();
    let paused = service.inspect(id).await.unwrap();
    assert_eq!(paused["status"], "paused");
    assert!(paused["nextRunAt"].is_null());
    service.resume(id).await.unwrap();
    service.tick().await.unwrap();
    assert_eq!(service.inspect(id).await.unwrap()["status"], "completed");
    assert!(service.resume(id).await.is_err());
    assert!(service.claim(id, true, service.now()).await.is_err());
}
#[tokio::test]
async fn update_and_upsert_preserve_timezone_and_clear_nullable_launch_fields() {
    let (_dir, service) = service().await;
    let mut value = input();
    value["cadence"] =
        json!({"type":"cron","expression":"0 9 * * *","timezone":"America/New_York"});
    value["target"] = json!({"type":"new-agent","config":{"provider":"codex","cwd":"/isolated","model":"model-a"}});
    let created = service.create(&value).await.unwrap();
    let id = created["id"].as_str().unwrap();
    let updated=service.update(&json!({"id":id,"cadence":{"type":"cron","expression":"0 10 * * *"},"newAgentConfig":{"model":null}})).await.unwrap();
    assert_eq!(updated["cadence"]["timezone"], "America/New_York");
    assert!(updated["target"]["config"].get("model").is_none());
    assert_eq!(updated["nextRunAt"], "2026-10-01T14:00:00.000Z");
}

#[tokio::test]
async fn manual_run_outlives_its_request_and_commits_its_outcome() {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(
        root.path().join("home"),
        root.path().into(),
        std::env::current_exe().unwrap(),
    );
    let service = app.automation.clone();
    assert!(
        service
            .store
            .set(Store::open(app.home.join("schedules")).await.unwrap())
            .is_ok()
    );
    app.agents.storage.get_or_init(|| async {
        crate::agent::storage::AgentStorage::new(app.home.clone())
    }).await;
    let schedule = service.create(&input()).await.unwrap();
    let id = schedule["id"].as_str().unwrap().to_owned();
    let writer = service.store().unwrap().writer.lock().await;
    let requester = {
        let service = service.clone();
        let id = id.clone();
        tokio::spawn(async move { service.run_once(&id).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !service.running.lock().unwrap().contains(&id) {
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    requester.abort();
    assert!(requester.await.unwrap_err().is_cancelled());
    drop(writer);
    let runs = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let runs = service.logs(&id).await.unwrap();
            if runs.first().is_some_and(|run| run["status"] == "failed") {
                return runs;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert!(runs[0]["endedAt"].is_string());
    assert!(runs[0]["error"].as_str().unwrap().contains("no longer exists"));
    assert_eq!(service.inspect(&id).await.unwrap()["status"], "completed");
}
