use super::*;
use crate::providers::api::{AgentProvider, LaunchContext};
use crate::state::AppState;
use crate::transport::{OutboundFrame, RequestContext};
use std::time::Duration;

const PARENT: &str = "00000000-0000-4000-8000-0000000000a1";

fn saved_reads() -> Value {
    json!({"owned_subscriptions":true,"all_providers":true,"saved_history_reads":true})
}
fn legacy_reads() -> Value {
    json!({"owned_subscriptions":true,"all_providers":true})
}

async fn fixture() -> (tempfile::TempDir, Arc<AppState>) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let app = AppState::new(
        home.clone(),
        root.path().into(),
        std::env::current_exe().unwrap(),
    );
    app.agents
        .storage
        .get_or_init(|| async { AgentStorage::new(home) })
        .await;
    (root, app)
}

/// A stored agent of a provider this daemon has not configured: every
/// preparation attempt fails, as an unavailable provider would.
fn record(id: &str, cwd: &Path, persistence: bool) -> Value {
    let mut record = json!({
        "id":id,"provider":"omp","cwd":cwd,"config":{"model":"test/model"},"internal":false,
        "lastStatus":"idle","createdAt":"2026-10-24T00:00:00.000Z",
        "updatedAt":"2026-10-31T00:00:00.000Z","requiresAttention":false
    });
    if persistence {
        record["persistence"] =
            json!({"provider":"omp","sessionId":"native","metadata":{"cwd":cwd}});
    }
    record
}

/// Stores the agent and a checkpoint of `items`; returns the checkpoint epoch.
async fn store_agent(app: &Arc<AppState>, id: &str, cwd: &Path, items: &[Value]) -> String {
    app.agents
        .storage()
        .unwrap()
        .upsert(record(id, cwd, true))
        .await
        .unwrap();
    let mut timeline = Timeline::new();
    for item in items {
        timeline.append(item.clone(), &json!({}));
    }
    timeline
        .persist(&app.agents.timeline_path(id).unwrap(), id)
        .unwrap();
    timeline.epoch().to_owned()
}

#[tokio::test]
async fn model_activity_observations_never_start_retained_or_missing_providers() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    store_agent(&app, PARENT, root.path(), &messages(12)).await;
    let lane = manager.lane(PARENT).await;
    let _blocked = lane.lock().await;
    let options = json!({"direction":"before","limit":2});
    let local = tokio::time::timeout(
        Duration::from_secs(2),
        manager.observe_timeline(PARENT, &options),
    )
    .await
    .unwrap()
    .unwrap();
    let expected = json!(["message 10", "message 11"]);
    let actual: Vec<_> = local["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect();
    assert_eq!(json!(actual), expected);
    for capabilities in [legacy_reads(), saved_reads()] {
        let routed = timeline_request_as(
            manager,
            capabilities,
            true,
            PARENT,
            json!({"direction":"before","limit":2,"observationOnly":true}),
        )
        .await;
        assert!(routed["error"].is_null(), "{routed}");
        assert_eq!(json!(texts(&routed)), expected);
    }
    assert!(manager.preparations.status(PARENT).is_none());
    assert!(!manager.agents.read().await.contains_key(PARENT));
    let missing = "00000000-0000-4000-8000-0000000000a2";
    manager
        .storage()
        .unwrap()
        .upsert(record(missing, root.path(), true))
        .await
        .unwrap();
    assert!(manager.observe_timeline(missing, &options).await.is_err());
    assert!(manager.preparations.status(missing).is_none());
    assert!(!manager.agents.read().await.contains_key(missing));
}

fn messages(count: usize) -> Vec<Value> {
    (0..count)
        .map(|i| json!({"type":"assistant_message","text":format!("message {i}"),"messageId":format!("m{i}")}))
        .collect()
}

fn texts(page: &Value) -> Vec<String> {
    page["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["item"]["text"].as_str().unwrap().to_owned())
        .collect()
}

async fn timeline_request(
    manager: &AgentManager,
    capabilities: Value,
    id: &str,
    options: Value,
) -> Value {
    timeline_request_as(manager, capabilities, false, id, options).await
}

async fn timeline_request_as(
    manager: &AgentManager,
    capabilities: Value,
    routed: bool,
    id: &str,
    options: Value,
) -> Value {
    let (source, mut output) = RequestContext::attach(crate::transport::SourceConfig {
        id: "saved-history-source".into(),
        session_id: "saved-history-session".into(),
        client_id: "saved-history-client".into(),
        capabilities,
        principal: json!({"id":"owner","kind":"owner"}),
        permissions: Arc::new(std::sync::RwLock::new(
            ["workspace.read".to_owned()].into_iter().collect(),
        )),
        routed,
        app_version: None,
        cancel: CancellationToken::new(),
    });
    let mut message = json!({
        "type":"fetch_agent_timeline_request","requestId":"history","agentId":id,"projection":"projected"
    });
    merge(&mut message, &options);
    let request = source.for_request(message.clone());
    assert!(manager.handle(&request, &message).await.unwrap());
    let frame = output.recv().await.unwrap();
    let OutboundFrame::Text(text) = &frame.frame else {
        panic!("expected timeline response");
    };
    let response: Value = serde_json::from_str(text).unwrap();
    assert_eq!(response["message"]["type"], "fetch_agent_timeline_response");
    source.detach();
    response["message"]["payload"].clone()
}

/// The next published preparation observation for any agent.
async fn next_preparation(events: &mut broadcast::Receiver<Value>) -> Value {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("preparation update")
            .unwrap();
        if event["type"] == "agent_preparation" {
            return event["agent"].clone();
        }
    }
}

async fn mock_session(
    app: &Arc<AppState>,
    id: &str,
    cell: &Arc<AgentCell>,
) -> Arc<dyn AgentSession> {
    crate::providers::development::DevelopmentProvider::new("mock")
        .unwrap()
        .create_session(
            json!({}),
            LaunchContext {
                agent_id: id.into(),
                home: app.home.clone(),
                executable: app.executable.clone(),
                env: BTreeMap::new(),
                mcp_servers: json!({}),
                tool_catalog: json!([]),
                tool_manifest: json!([]),
                tool_executor: None,
                cancel: cell.cancel.child_token(),
            },
            json!({}),
            cell.sink.clone(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn saved_tail_answers_before_blocked_preparation_and_survives_its_failure() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let epoch = store_agent(&app, PARENT, root.path(), &messages(50)).await;
    let mut events = manager.subscribe_events();
    // Provider startup cannot progress while the agent's lifecycle lane is held.
    let lane = manager.lane(PARENT).await;
    let blocked = lane.lock().await;

    let tail = tokio::time::timeout(
        Duration::from_secs(10),
        timeline_request(
            manager,
            saved_reads(),
            PARENT,
            json!({"direction":"tail","limit":40}),
        ),
    )
    .await
    .expect("saved tail must not wait for the provider");
    assert!(tail["error"].is_null(), "{tail}");
    assert_eq!(tail["historySource"], "saved");
    assert_eq!(tail["epoch"], epoch);
    assert_eq!(tail["hasOlder"], true);
    assert_eq!(tail["agent"]["preparation"]["status"], "preparing");
    let expected: Vec<String> = (10..50).map(|i| format!("message {i}")).collect();
    assert_eq!(texts(&tail), expected);
    let older = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({
            "direction":"before","limit":40,"cursor":tail["startCursor"]
        }),
    )
    .await;
    assert_eq!(older["epoch"], epoch);
    assert_eq!(older["hasOlder"], false);
    assert_eq!(
        texts(&older),
        (0..10).map(|i| format!("message {i}")).collect::<Vec<_>>()
    );
    let newer = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({
            "direction":"after","cursor":tail["endCursor"]
        }),
    )
    .await;
    assert_eq!(newer["reset"], false);
    assert_eq!(newer["hasNewer"], false);
    assert!(newer["entries"].as_array().unwrap().is_empty());

    // Three reads joined one attempt: its failure is the next update.
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "preparing"
    );
    drop(blocked);
    let failed = next_preparation(&mut events).await;
    assert_eq!(failed["preparation"]["status"], "failed");
    assert!(
        failed["preparation"]["error"]
            .as_str()
            .unwrap()
            .contains("unavailable provider")
    );
    // Observation never reaches the authoritative snapshot used for mutations.
    let authoritative = manager.agent_snapshot(PARENT).await.unwrap().unwrap();
    assert!(authoritative.get("preparation").is_none());

    // The failure hides nothing; the next read retries preparation.
    let retried = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({"direction":"tail","limit":40}),
    )
    .await;
    assert!(retried["error"].is_null(), "{retried}");
    assert_eq!(retried["entries"], tail["entries"]);
    assert_eq!(retried["historySource"], "saved");
    assert_eq!(retried["agent"]["preparation"]["status"], "preparing");
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "preparing"
    );
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "failed"
    );
    let stored = Timeline::load(&manager.timeline_path(PARENT).unwrap(), PARENT).unwrap();
    assert_eq!(stored.items().len(), 50);
    assert_eq!(stored.epoch(), epoch);
}

#[tokio::test]
async fn connected_host_sessions_read_saved_history_by_their_origin_capability() {
    let (root, app) = fixture().await;
    store_agent(&app, PARENT, root.path(), &messages(3)).await;
    let lane = app.agents.lane(PARENT).await;
    let _blocked = lane.lock().await;
    // A Flutter session reaching this daemon through a connected host keeps
    // its own hello capabilities (see hosts::routes::routed_hello).
    let page = tokio::time::timeout(
        Duration::from_secs(10),
        timeline_request_as(
            &app.agents,
            saved_reads(),
            true,
            PARENT,
            json!({"direction":"tail","limit":40}),
        ),
    )
    .await
    .expect("routed saved read must not wait for the provider");
    assert!(page["error"].is_null(), "{page}");
    assert_eq!(page["historySource"], "saved");
    assert_eq!(texts(&page), ["message 0", "message 1", "message 2"]);
    // A routed legacy client keeps the authoritative read.
    let manager = app.agents.clone();
    let mut legacy = tokio::spawn(async move {
        timeline_request_as(
            &manager,
            legacy_reads(),
            true,
            PARENT,
            json!({"direction":"tail","limit":40}),
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut legacy)
            .await
            .is_err()
    );
    legacy.abort();
}

#[tokio::test]
async fn clients_without_saved_reads_keep_authoritative_blocking_reads() {
    let (root, app) = fixture().await;
    store_agent(&app, PARENT, root.path(), &messages(3)).await;
    let lane = app.agents.lane(PARENT).await;
    let blocked = lane.lock().await;
    let manager = app.agents.clone();
    let mut read = tokio::spawn(async move {
        timeline_request(
            &manager,
            legacy_reads(),
            PARENT,
            json!({"direction":"tail","limit":40}),
        )
        .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut read)
            .await
            .is_err()
    );
    drop(blocked);
    let page = read.await.unwrap();
    assert!(
        page["error"]
            .as_str()
            .unwrap()
            .contains("unavailable provider")
    );
    assert!(page.get("historySource").is_none());
}

#[tokio::test]
async fn missing_checkpoint_waits_for_preparation_and_returns_its_error() {
    let (root, app) = fixture().await;
    app.agents
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let mut events = app.agents.subscribe_events();
    let lane = app.agents.lane(PARENT).await;
    let blocked = lane.lock().await;
    let manager = app.agents.clone();
    let mut read = tokio::spawn(async move {
        timeline_request(
            &manager,
            saved_reads(),
            PARENT,
            json!({"direction":"tail","limit":40}),
        )
        .await
    });
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "preparing"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut read)
            .await
            .is_err()
    );
    drop(blocked);
    let page = read.await.unwrap();
    // No fabricated empty history: the provider's error is the answer.
    assert!(
        page["error"]
            .as_str()
            .unwrap()
            .contains("unavailable provider"),
        "{page}"
    );
    assert!(page["entries"].as_array().unwrap().is_empty());
    assert!(!app.agents.timeline_path(PARENT).unwrap().exists());
}

#[tokio::test]
async fn shutdown_cancels_owned_preparation_and_its_waiting_read() {
    let (root, app) = fixture().await;
    app.agents
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let mut events = app.agents.subscribe_events();
    let lane = app.agents.lane(PARENT).await;
    let _blocked = lane.lock().await;
    let manager = app.agents.clone();
    let read = tokio::spawn(async move {
        timeline_request(
            &manager,
            saved_reads(),
            PARENT,
            json!({"direction":"tail","limit":40}),
        )
        .await
    });
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "preparing"
    );
    app.agents.preparations.abort_all();
    let page = tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .unwrap()
        .unwrap();
    assert!(
        page["error"].as_str().unwrap().contains("cancelled"),
        "{page}"
    );
    let observed = app
        .agents
        .observe_agent_snapshot(PARENT)
        .await
        .unwrap()
        .unwrap();
    assert!(observed.get("preparation").is_none());
}

#[tokio::test]
async fn corrupt_checkpoint_is_an_error_and_starts_no_preparation() {
    let (root, app) = fixture().await;
    app.agents
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let path = app.agents.timeline_path(PARENT).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not a timeline database").unwrap();
    let page = timeline_request(
        &app.agents,
        saved_reads(),
        PARENT,
        json!({"direction":"tail","limit":40}),
    )
    .await;
    assert!(page["error"].is_string(), "{page}");
    assert!(page["entries"].as_array().unwrap().is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), b"not a timeline database");
    let observed = app
        .agents
        .observe_agent_snapshot(PARENT)
        .await
        .unwrap()
        .unwrap();
    assert!(observed.get("preparation").is_none());
}

#[tokio::test]
async fn settled_preparation_serves_the_live_projection() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let epoch = store_agent(&app, PARENT, root.path(), &messages(3)).await;
    let mut events = manager.subscribe_events();
    let lane = manager.lane(PARENT).await;
    let blocked = lane.lock().await;
    let saved = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({"direction":"tail","limit":40}),
    )
    .await;
    assert_eq!(saved["historySource"], "saved");
    assert_eq!(
        next_preparation(&mut events).await["preparation"]["status"],
        "preparing"
    );

    // Another owner finishes provider startup and history replay first.
    let (cell, receiver) = manager
        .make_cell(PARENT, record(PARENT, root.path(), true), true)
        .await
        .unwrap();
    *cell.task.lock().await = Some(manager.start_events(PARENT, &cell, receiver));
    let session = mock_session(&app, PARENT, &cell).await;
    {
        let mut data = cell.data.lock().await;
        data.session = Some(session);
        data.ready = true;
    }
    drop(blocked);
    let settled = next_preparation(&mut events).await;
    assert!(settled.get("preparation").is_none(), "{settled}");

    let live = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({"direction":"tail","limit":40}),
    )
    .await;
    assert_eq!(live["historySource"], "live");
    assert_eq!(live["epoch"], epoch);
    assert_eq!(live["entries"], saved["entries"]);
    assert!(live["agent"].get("preparation").is_none());
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

#[tokio::test]
async fn live_events_received_during_preparation_follow_the_saved_page() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let epoch = store_agent(
        &app,
        PARENT,
        root.path(),
        &[
            json!({"type":"user_message","text":"question","messageId":"q"}),
            json!({"type":"assistant_message","text":"answer","messageId":"a"}),
        ],
    )
    .await;
    // Keep the background attempt from replacing the runtime under test.
    let lane = manager.lane(PARENT).await;
    let blocked = lane.lock().await;
    let (cell, receiver) = manager
        .make_cell(PARENT, record(PARENT, root.path(), true), false)
        .await
        .unwrap();
    *cell.task.lock().await = Some(manager.start_events(PARENT, &cell, receiver));
    let session = mock_session(&app, PARENT, &cell).await;
    {
        let mut data = cell.data.lock().await;
        data.session = Some(session);
        data.ready = true;
    }
    cell.sink
        .emit(json!({"type":"timeline","provider":"omp","item":{
            "type":"assistant_message","text":"live answer","messageId":"live"
        }}))
        .await
        .unwrap();
    manager.drain_events(&cell).await.unwrap();

    let saved = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({"direction":"tail","limit":40}),
    )
    .await;
    assert_eq!(saved["historySource"], "saved");
    assert_eq!(saved["epoch"], epoch);
    assert_eq!(texts(&saved), ["question", "answer"]);

    // The replay cannot account for the saved rows, so the checkpoint is
    // retained and staged live output is applied after it.
    manager.hydrate_history(PARENT, &cell, false).await.unwrap();
    let after = timeline_request(
        manager,
        saved_reads(),
        PARENT,
        json!({
            "direction":"after","cursor":saved["endCursor"]
        }),
    )
    .await;
    assert_eq!(after["historySource"], "live");
    assert_eq!(after["epoch"], epoch);
    assert_eq!(after["reset"], false);
    assert_eq!(texts(&after), ["live answer"]);
    drop(blocked);
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

fn child_event(id: &str, event: Value) -> Value {
    let mut event = event;
    event["id"] = json!(id);
    event
}

/// Stores `children` (id, row texts) under `parent` through the persisted catalog.
fn store_children(root: &Path, parent: &str, children: &[(&str, &[&str])]) {
    let mut store = SubagentStore::default();
    for (id, rows) in children {
        store.apply_persisted(root, parent, "omp", &child_event(id, json!({
            "type":"upsert","timestamp":"2026-10-31T00:00:00.000Z","title":format!("child {id}"),
            "source":{"thread":format!("native-{id}")}
        }))).unwrap();
        for text in rows.iter() {
            store.apply_persisted(root, parent, "omp", &child_event(id, json!({
                "type":"timeline","item":{"type":"assistant_message","text":text,"messageId":text}
            }))).unwrap();
        }
    }
}

#[tokio::test]
async fn opening_a_child_reads_it_without_starting_the_parent() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    manager
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let subagents = app.home.join("agent-subagents");
    store_children(&subagents, PARENT, &[("a", &["a1", "a2"]), ("b", &["b1"])]);

    let listed = manager.subagents(PARENT).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|child| child.get("source").is_none()));
    let page = manager
        .subagent_timeline(PARENT, "a", &json!({"direction":"tail","limit":40}))
        .await
        .unwrap();
    let rows: Vec<_> = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect();
    assert_eq!(rows, [json!("a1"), json!("a2")]);
    assert_eq!(page["provider"], "omp");
    assert!(manager.agents.read().await.get(PARENT).is_none());
    let observed = manager
        .observe_agent_snapshot(PARENT)
        .await
        .unwrap()
        .unwrap();
    assert!(observed.get("preparation").is_none());
}

#[tokio::test]
async fn incomplete_child_reads_are_not_reused_and_a_later_complete_read_replaces_them() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    manager
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let subagents = app.home.join("agent-subagents");
    let mut seed = SubagentStore::default();
    seed.apply_persisted(
        &subagents,
        PARENT,
        "omp",
        &json!({"type":"upsert","id":"x","status":"completed"}),
    )
    .unwrap();
    for item in [
        json!({"type":"reasoning","text":"thinking"}),
        json!({"type":"assistant_message","text":"a1","messageId":"a1"}),
    ] {
        seed.apply_persisted(
            &subagents,
            PARENT,
            "omp",
            &json!({"type":"timeline","id":"x","item":item}),
        )
        .unwrap();
    }
    drop(seed);
    let row = |item: Value, coverage| {
        crate::providers::api::subagent_row("omp", "x", item, None, coverage)
    };
    let stored = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();

    let full = |coverage| {
        vec![
            row(json!({"type":"reasoning","text":"thinking"}), coverage),
            row(
                json!({"type":"assistant_message","text":"a1","messageId":"a1"}),
                coverage,
            ),
            row(
                json!({"type":"assistant_message","text":"a2","messageId":"a2"}),
                coverage,
            ),
        ]
    };
    let without_reasoning = |coverage| full(coverage).split_off(1);

    // An unverified replay that cannot account for the stored reasoning
    // leaves the stored rows and is not reusable.
    let retained = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Unverified),
            without_reasoning(HistoryCoverage::Unverified),
        )
        .await
        .unwrap();
    assert_eq!(retained, BodyRead::Incomplete);
    assert!(
        settled_read(&Ok(retained), false).is_none(),
        "the next open reads again"
    );
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(page["rows"], stored["rows"]);

    // An unverified replay can be applied but is still not reused.
    let unverified = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Unverified),
            full(HistoryCoverage::Unverified),
        )
        .await
        .unwrap();
    assert_eq!(unverified, BodyRead::Incomplete);
    assert!(settled_read(&Ok(unverified), false).is_none());

    // The next complete read replaces the rows, keeps the earlier ones, and is reused.
    let complete = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Complete),
            full(HistoryCoverage::Complete),
        )
        .await
        .unwrap();
    assert_eq!(complete, BodyRead::Current);
    assert!(matches!(
        settled_read(&Ok(complete), false),
        Some(Flight::Done)
    ));
    assert!(
        settled_read(&Ok(complete), true).is_none(),
        "a running child reads again"
    );
    assert!(matches!(
        settled_read(&Err("gone".into()), false),
        Some(Flight::Failed(_))
    ));
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    let texts: Vec<_> = page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect();
    assert_eq!(texts, [json!("thinking"), json!("a1"), json!("a2")]);

    // A complete native body is authoritative: differing stored history resets to it.
    let reset = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Complete),
            without_reasoning(HistoryCoverage::Complete),
        )
        .await
        .unwrap();
    assert_eq!(reset, BodyRead::Current);
    let after = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    let texts: Vec<_> = after["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect();
    assert_eq!(texts, [json!("a1"), json!("a2")]);
    assert_ne!(after["epoch"], page["epoch"]);
}

#[tokio::test]
async fn complete_native_history_cannot_drop_a_running_childs_live_rows() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    manager
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let subagents = app.home.join("agent-subagents");
    let apply = |event: Value| {
        manager
            .subagent_store
            .lock()
            .unwrap()
            .apply_persisted(&subagents, PARENT, "omp", &event)
            .unwrap();
    };
    apply(json!({"type":"upsert","id":"x","status":"running"}));
    // A streamed item the native file does not contain yet.
    apply(json!({"type":"timeline","id":"x","item":{"type":"reasoning","text":"in flight"}}));
    apply(
        json!({"type":"timeline","id":"x","item":{"type":"assistant_message","text":"a1","messageId":"a1"}}),
    );
    let native = || {
        vec![crate::providers::api::subagent_row(
            "omp",
            "x",
            json!({"type":"assistant_message","text":"a1","messageId":"a1"}),
            None,
            HistoryCoverage::Complete,
        )]
    };
    let running = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Complete),
            native(),
        )
        .await
        .unwrap();
    assert_eq!(
        running,
        BodyRead::Incomplete,
        "a running child's read is never reused"
    );
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(texts_of(&page), [json!("in flight"), json!("a1")]);
    // Once finished, the same complete native history is authoritative.
    apply(json!({"type":"upsert","id":"x","status":"completed"}));
    let finished = manager
        .apply_subagent_body(
            subagents.clone(),
            PARENT,
            "x",
            SubagentHistory::Replayed(HistoryCoverage::Complete),
            native(),
        )
        .await
        .unwrap();
    assert_eq!(finished, BodyRead::Current);
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(texts_of(&page), [json!("a1")]);
}

#[tokio::test]
async fn child_read_while_the_parent_prepares_waits_for_it_and_loses_nothing() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    // A parent mid-preparation: its runtime cell exists, its history replay
    // stage holds live events, and it holds the child-read gate exclusively.
    let cell = live_parent(&app, root.path(), true).await;
    let preparing = manager.child_read_gate(PARENT).await.write_owned().await;
    cell.sink
        .emit(json!({"type":"provider_subagent","provider":"omp",
        "event":{"type":"upsert","id":"x","status":"completed"}}))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":"a1","messageId":"a1"}),
        ))
        .await
        .unwrap();
    // The child read defers to the preparation instead of racing its stage.
    let settled = async { manager.child_read_gate(PARENT).await.read_owned().await };
    tokio::pin!(settled);
    assert!(futures_util::poll!(&mut settled).is_pending());
    // Even a marker already published behind the stage only waits for it:
    // the parent's own replay never depends on the child's outcome.
    let (key, replay, applied) = manager.child_replays.begin();
    let row = |text: &str| {
        crate::providers::api::subagent_row(
            "omp",
            "x",
            json!({"type":"assistant_message","text":text,"messageId":text}),
            None,
            HistoryCoverage::Complete,
        )
    };
    cell.sink
        .emit(replay.marker(
            "omp",
            "x",
            vec![row("a1"), row("a2")],
            HistoryCoverage::Complete,
        ))
        .await
        .unwrap();
    let outcome = replay.outcome();
    tokio::pin!(outcome);
    assert!(futures_util::poll!(&mut outcome).is_pending());
    // The preparation finishes: history replay releases the stage, then the lane.
    let session = mock_session(&app, PARENT, &cell).await;
    cell.data.lock().await.session = Some(session);
    manager.hydrate_history(PARENT, &cell, false).await.unwrap();
    drop(preparing);
    let reading = settled.await;
    // While that child read runs, the parent cannot start another session.
    let restart = async { manager.child_read_gate(PARENT).await.write_owned().await };
    tokio::pin!(restart);
    assert!(futures_util::poll!(&mut restart).is_pending());
    drop(reading);
    drop(restart.await);
    assert_eq!(outcome.await, Some(ReplayOutcome::Applied));
    assert_eq!(
        manager
            .await_published_replay(PARENT, &key, applied)
            .await
            .unwrap(),
        BodyRead::Current
    );
    manager.drain_events(&cell).await.unwrap();
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(texts_of(&page), [json!("a1"), json!("a2")]);
    // The parent's own preparation completed as well.
    assert!(cell.data.lock().await.serves_runtime());
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

#[tokio::test]
async fn an_abandoned_child_read_resolves_the_provider_and_its_late_marker_is_ignored() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let cell = live_parent(&app, root.path(), false).await;
    cell.sink
        .emit(json!({"type":"provider_subagent","provider":"omp",
        "event":{"type":"upsert","id":"x","status":"completed"}}))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":"kept","messageId":"k"}),
        ))
        .await
        .unwrap();
    let (key, replay, _applied) = manager.child_replays.begin();
    // The reader ends (error, cancellation or shutdown abort) before the marker arrives.
    drop(WaiterGuard {
        waiters: &manager.child_replays,
        key: &key,
    });
    assert_eq!(
        replay.outcome().await,
        None,
        "the provider keeps its previous state"
    );
    cell.sink
        .emit(replay.marker(
            "omp",
            "x",
            vec![crate::providers::api::subagent_row(
                "omp",
                "x",
                json!({"type":"assistant_message","text":"other","messageId":"o"}),
                None,
                HistoryCoverage::Complete,
            )],
            HistoryCoverage::Complete,
        ))
        .await
        .unwrap();
    manager.drain_events(&cell).await.unwrap();
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(texts_of(&page), [json!("kept")]);
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

/// A live parent runtime whose event consumer applies provider child events.
async fn live_parent(app: &Arc<AppState>, root: &Path, staged: bool) -> Arc<AgentCell> {
    let manager = &app.agents;
    manager
        .storage()
        .unwrap()
        .upsert(record(PARENT, root, true))
        .await
        .unwrap();
    let (cell, receiver) = manager
        .make_cell(PARENT, record(PARENT, root, true), !staged)
        .await
        .unwrap();
    *cell.task.lock().await = Some(manager.start_events(PARENT, &cell, receiver));
    cell.data.lock().await.ready = true;
    cell
}

fn live_child(item: Value) -> Value {
    json!({"type":"provider_subagent","provider":"omp","event":{"type":"timeline","id":"x","item":item}})
}

fn texts_of(page: &Value) -> Vec<Value> {
    page["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect()
}

#[tokio::test]
async fn published_replay_orders_with_live_rows_and_same_id_deltas_exactly_once() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let cell = live_parent(&app, root.path(), false).await;
    cell.sink
        .emit(json!({"type":"provider_subagent","provider":"omp",
        "event":{"type":"upsert","id":"x","status":"completed"}}))
        .await
        .unwrap();
    // Rows published before the provider's snapshot: the replay covers them.
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":"abc","messageId":"m"}),
        ))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":" def","messageId":"m"}),
        ))
        .await
        .unwrap();
    let (key, replay, applied) = manager.child_replays.begin();
    let row = |text: &str, id: &str| {
        crate::providers::api::subagent_row(
            "omp",
            "x",
            json!({"type":"assistant_message","text":text,"messageId":id}),
            None,
            HistoryCoverage::Complete,
        )
    };
    // The provider publishes its replay itself, in order with its live rows,
    // so no row can fall between its snapshot and the marker.
    cell.sink
        .emit(replay.marker(
            "omp",
            "x",
            vec![row("abc def", "m"), row("next", "n")],
            HistoryCoverage::Complete,
        ))
        .await
        .unwrap();
    // A same-id delta and a new row published right behind the marker.
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":" more","messageId":"n"}),
        ))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":"later","messageId":"o"}),
        ))
        .await
        .unwrap();
    assert_eq!(replay.outcome().await, Some(ReplayOutcome::Applied));
    assert_eq!(
        manager
            .await_published_replay(PARENT, &key, applied)
            .await
            .unwrap(),
        BodyRead::Current
    );
    manager.drain_events(&cell).await.unwrap();
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(
        texts_of(&page),
        [json!("abc def"), json!("next more"), json!("later")]
    );
    // The replacement started a new child epoch; later live rows extend it.
    let rows = page["rows"].as_array().unwrap();
    assert!(
        rows.windows(2)
            .all(|pair| pair[0]["seqEnd"].as_i64() < pair[1]["seqStart"].as_i64())
    );
    assert_eq!(page["window"]["maxSeq"], rows.last().unwrap()["seqEnd"]);
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

#[tokio::test]
async fn retained_published_replay_keeps_the_provider_state_for_its_next_delta() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    let cell = live_parent(&app, root.path(), false).await;
    cell.sink
        .emit(json!({"type":"provider_subagent","provider":"omp",
        "event":{"type":"upsert","id":"x","status":"running"}}))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(json!({"type":"reasoning","text":"think"})))
        .await
        .unwrap();
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":"abc","messageId":"m"}),
        ))
        .await
        .unwrap();
    let (key, replay, applied) = manager.child_replays.begin();
    // An unverified snapshot that cannot account for the unidentified reasoning.
    let snapshot = vec![crate::providers::api::subagent_row(
        "omp",
        "x",
        json!({"type":"assistant_message","text":"abc","messageId":"m"}),
        None,
        HistoryCoverage::Unverified,
    )];
    cell.sink
        .emit(replay.marker("omp", "x", snapshot, HistoryCoverage::Unverified))
        .await
        .unwrap();
    // The provider holds the child's rows until the outcome, then continues
    // from the state it kept because the replay was retained.
    assert_eq!(replay.outcome().await, Some(ReplayOutcome::Retained));
    cell.sink
        .emit(live_child(
            json!({"type":"assistant_message","text":" def","messageId":"m"}),
        ))
        .await
        .unwrap();
    assert_eq!(
        manager
            .await_published_replay(PARENT, &key, applied)
            .await
            .unwrap(),
        BodyRead::Incomplete
    );
    manager.drain_events(&cell).await.unwrap();
    let page = manager
        .subagent_timeline(PARENT, "x", &json!({"limit":0}))
        .await
        .unwrap();
    assert_eq!(texts_of(&page), [json!("think"), json!("abc def")]);
    cell.cancel.cancel();
    cell.task.lock().await.take().unwrap().await.unwrap();
}

#[tokio::test]
async fn published_replay_on_a_closed_parent_fails_both_sides_and_stays_retryable() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    // A staged parent (still replaying its own history) holds the marker.
    let cell = live_parent(&app, root.path(), true).await;
    let (key, replay, applied) = manager.child_replays.begin();
    cell.sink
        .emit(replay.marker(
            "omp",
            "x",
            vec![crate::providers::api::subagent_row(
                "omp",
                "x",
                json!({"type":"assistant_message","text":"a","messageId":"a"}),
                None,
                HistoryCoverage::Complete,
            )],
            HistoryCoverage::Complete,
        ))
        .await
        .unwrap();
    let read = manager.await_published_replay(PARENT, &key, applied);
    tokio::pin!(read);
    assert!(futures_util::poll!(&mut read).is_pending());
    cell.cancel.cancel();
    let error = read.await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("closed during the subagent replay"),
        "{error:#}"
    );
    assert!(
        settled_read(&Err(error.to_string()), false)
            .is_some_and(|flight| matches!(flight, Flight::Failed(_)))
    );
    // The provider learns the replay was never applied and keeps its own state.
    assert_eq!(replay.outcome().await, None);
    cell.task.lock().await.take().unwrap().await.unwrap();
}

#[test]
fn resume_carries_only_the_stored_card_logs() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("parent.sqlite3");
    let card = |log: &str, status: &str| {
        json!({
            "type":"tool_call","callId":"spawn","name":"spawn_agent","status":status,
            "detail":{"type":"sub_agent","subAgentType":"Sub-agent","log":log,"actions":[]}
        })
    };
    let mut timeline = Timeline::new();
    timeline.append(
        json!({"type":"user_message","text":"delegate","messageId":"u"}),
        &json!({}),
    );
    timeline.append(card("[Assistant] child work", "running"), &json!({}));
    timeline.append(
        json!({"type":"tool_call","callId":"shell","name":"shell","status":"completed",
        "detail":{"type":"shell","command":"ls","output":"big output"}}),
        &json!({}),
    );
    timeline.append(
        card("[Assistant] child work\n[Assistant] more", "running"),
        &json!({}),
    );
    timeline.append(json!({"type":"tool_call","callId":"idle","name":"spawn_agent","status":"running",
        "detail":{"type":"sub_agent","log":"","actions":[]},
        "metadata":{"subagentLog":[{"child":"t-idle","key":"k","kind":"assistant_message","lines":0}]}}), &json!({}));
    let entries =
        json!([{"child":"t-other","key":"m1","kind":"assistant_message","lines":1,"trailing":" "}]);
    timeline.append(
        json!({"type":"tool_call","callId":"other","name":"spawn_agent","status":"completed",
        "detail":{"type":"sub_agent","log":"[Assistant] other","actions":[]},
        "metadata":{"subagentLog":entries}}),
        &json!({}),
    );
    timeline.persist(&path, PARENT).unwrap();
    // The cell loads the stored projection before the provider resumes.
    let stored = Timeline::load(&path, PARENT).unwrap();
    let options = crate::agent::runtime::resume_options(false, &stored);
    assert_eq!(
        options,
        json!({
            "purpose":"interactive",
            "subagentCardLogs":{"spawn":"[Assistant] child work\n[Assistant] more","other":"[Assistant] other"},
            "subagentCardEntries":{"other":entries}
        })
    );
    let history = crate::agent::runtime::resume_options(true, &Timeline::new());
    assert_eq!(history, json!({"purpose":"history"}));
}

#[tokio::test]
async fn child_body_read_failures_are_reported_and_retried_on_next_open() {
    let (root, app) = fixture().await;
    let manager = &app.agents;
    manager
        .storage()
        .unwrap()
        .upsert(record(PARENT, root.path(), true))
        .await
        .unwrap();
    let collected = "00000000-0000-4000-8000-0000000000a2";
    manager
        .storage()
        .unwrap()
        .upsert(record(collected, root.path(), false))
        .await
        .unwrap();
    let subagents = app.home.join("agent-subagents");
    store_children(&subagents, PARENT, &[("empty", &[]), ("kept", &["k1"])]);
    store_children(&subagents, collected, &[("empty", &[])]);

    for _ in 0..2 {
        let error = manager
            .subagent_timeline(PARENT, "empty", &json!({"limit":40}))
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("Unknown provider 'omp'"),
            "{error:#}"
        );
    }
    let mut events = manager.subscribe_events();
    let kept = manager
        .subagent_timeline(PARENT, "kept", &json!({"limit":40}))
        .await
        .unwrap();
    assert_eq!(kept["rows"][0]["item"]["text"], "k1");
    assert_eq!(kept["nativeRefresh"]["status"], "refreshing");
    // The background failure reaches the open pane as a descriptor upsert,
    // never by replacing the stored rows.
    let failed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            let child = &event["event"]["subagent"];
            if event["type"] == "provider_subagent"
                && child["id"] == "kept"
                && child["nativeRefresh"]["status"] == "failed"
            {
                return child.clone();
            }
        }
    })
    .await
    .expect("failed native refresh is published");
    assert!(
        failed["nativeRefresh"]["error"]
            .as_str()
            .unwrap()
            .contains("Unknown provider 'omp'")
    );
    assert!(failed.get("source").is_none());
    let listed = manager.subagents(PARENT).await.unwrap();
    let listed = listed.iter().find(|child| child["id"] == "kept").unwrap();
    assert_eq!(listed["nativeRefresh"], failed["nativeRefresh"]);
    // Opening it again keeps the rows and retries the refresh.
    let retried = manager
        .subagent_timeline(PARENT, "kept", &json!({"limit":40}))
        .await
        .unwrap();
    assert_eq!(retried["rows"], kept["rows"]);
    assert_eq!(retried["nativeRefresh"]["status"], "refreshing");
    // A parent without a native session has nothing more to read than its collected rows.
    let page = manager
        .subagent_timeline(collected, "empty", &json!({"limit":40}))
        .await
        .unwrap();
    assert!(page["rows"].as_array().unwrap().is_empty());
}

#[test]
fn child_replay_reads_only_the_opened_child() {
    let root = tempfile::tempdir().unwrap();
    store_children(root.path(), PARENT, &[("a", &["a1"]), ("b", &["b1"])]);
    let mut store = SubagentStore::default();
    let before = store
        .fetch_persisted(root.path(), PARENT, "b", &json!({"limit":0}))
        .unwrap();
    let envelope = |id: &str, text: &str| {
        crate::providers::api::subagent_row(
            "omp",
            id,
            json!({"type":"assistant_message","text":text,"messageId":text}),
            None,
            HistoryCoverage::Complete,
        )
    };
    let events = vec![
        envelope("a", "a1"),
        envelope("a", "a2"),
        envelope("b", "replaced b"),
        json!({"type":"provider_subagent","provider":"omp","event":{"type":"upsert","id":"a-child","parentSubagentId":"a"}}),
        json!({"type":"timeline","provider":"omp","item":{"type":"assistant_message","text":"parent row"}}),
    ];
    let partial = child_replay(events.clone(), "a", HistoryCoverage::Unverified);
    assert_eq!(partial.len(), 3);
    assert!(
        partial
            .iter()
            .filter(|e| e["event"]["type"] == "timeline")
            .all(|e| e["event"]["historyComplete"] == false)
    );

    let outputs = store
        .replay_history(
            Some(root.path()),
            PARENT,
            child_replay(events, "a", HistoryCoverage::Complete),
            true,
        )
        .unwrap();
    assert!(
        outputs
            .iter()
            .filter(|o| o["type"] == "timeline")
            .all(|o| o["subagentId"] == "a")
    );
    assert!(
        outputs
            .iter()
            .any(|o| o["type"] == "upsert" && o["subagent"]["id"] == "a-child")
    );
    let a = store
        .fetch_persisted(root.path(), PARENT, "a", &json!({"limit":0}))
        .unwrap();
    let texts: Vec<_> = a["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["item"]["text"].clone())
        .collect();
    assert_eq!(texts, [json!("a1"), json!("a2")]);
    let after = store
        .fetch_persisted(root.path(), PARENT, "b", &json!({"limit":0}))
        .unwrap();
    assert_eq!(after, before);
}
