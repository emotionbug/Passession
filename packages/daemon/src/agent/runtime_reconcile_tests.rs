use super::*;
use super::tests::staged_cell;

fn marked(marker: &str, ty: &str, text: &str) -> Value {
    json!({"type":ty,"text":text,"metadata":{"ompStreamMessageId":marker}})
}
fn correction(marker: &str, text: &str) -> Value {
    json!({"type":"timeline_reconcile","provider":"mock","turnId":"live-turn","messageId":marker,
        "items":[marked(marker,"assistant_message",text)],"diagnostic":{"conflicts":[{"block":0,"reason":"non_prefix"}]}})
}
async fn emit(cell: &AgentCell, events: Vec<Value>) {
    for event in events { cell.sink.emit(event).await.unwrap(); }
}
fn observed(receiver: &mut tokio::sync::broadcast::Receiver<Value>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() { events.push(event); }
    events
}

#[tokio::test]
async fn omp_message_reconcile_mapper_heading_body_reaches_canonical_consumer() {
    use crate::providers::pi_omp::Mapper;
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
    let manager = &app.agents;
    let id = "00000000-0000-4000-8000-000000000091";
    let cell = staged_cell(&app, id).await;
    manager.release_event_stage(&cell).await.unwrap();
    let user = json!({"type":"user_message","text":"sanitized question","messageId":"user"});
    let tool = json!({"type":"tool_call","callId":"tool","name":"read","status":"completed","detail":{"type":"read","filePath":"file"}});
    for item in [&user, &tool] {
        cell.sink.emit(json!({"type":"timeline","provider":"mock","turnId":"live-turn","item":item})).await.unwrap();
    }
    let mut mapper = Mapper::new("omp", "mock");
    mapper.begin_turn("live-turn");
    let heading = format!("**{}**\n\n", "h".repeat(39));
    let body = format!("Canonical summary: {}", "x".repeat(52));
    assert_eq!(heading.len(), 45);
    assert_eq!(body.len(), 71);
    emit(&cell, mapper.map(&json!({"type":"message_start","messageId":"rpc-message","message":{"role":"assistant","content":[]}}))).await;
    emit(&cell, mapper.map(&json!({"type":"message_update","messageId":"rpc-message","message":{"role":"assistant"},
        "assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":format!("{heading}{body}")}}))).await;
    manager.drain_events(&cell).await.unwrap();
    let before = manager.observe_timeline(id, &json!({"limit":0})).await.unwrap();
    assert_eq!(before["rows"][2]["item"]["text"], format!("{heading}{body}"));
    let marker = before["rows"][2]["item"]["metadata"]["ompStreamMessageId"].clone();
    assert!(marker.is_string());
    let mut notifications = manager.events.subscribe();
    let final_events = mapper.map(&json!({"type":"message_end","messageId":"rpc-message", "message":{
        "role":"assistant","responseId":"late-response","content":[{"type":"thinking","thinking":body}],"stopReason":"stop"}}));
    assert_eq!(final_events.iter().filter(|event| event["type"] == "timeline_reconcile").count(), 1);
    emit(&cell, final_events).await;
    manager.drain_events(&cell).await.unwrap();
    let after = manager.observe_timeline(id, &json!({"limit":0})).await.unwrap();
    assert_eq!(after["rows"].as_array().unwrap().len(), 3);
    assert_eq!(after["rows"][0]["item"], user);
    assert_eq!(after["rows"][1]["item"], tool);
    assert_eq!(after["rows"][2]["item"]["text"], body);
    assert_eq!(after["rows"][2]["item"]["metadata"]["ompStreamMessageId"], marker);
    assert_ne!(before["epoch"], after["epoch"]);
    let events = observed(&mut notifications);
    assert_eq!(events.iter().filter(|event| event["type"] == "timeline_replacement").count(), 1);
    assert!(events.iter().all(|event| event["type"] != "timeline_reconcile" && event["event"]["type"] != "timeline_reconcile"));
    assert!(events.iter().all(|event| event["event"]["item"]["type"] != "error"));
    let stale = manager.observe_timeline(id, &json!({"direction":"after","cursor":{"epoch":before["epoch"],"seq":before["window"]["maxSeq"]}})).await.unwrap();
    assert_eq!(stale["reset"], true);
    assert_eq!(stale["rows"][2]["item"]["text"], body);
    assert!(!manager.timeline_path(id).unwrap().exists(), "internal agents must not write checkpoints");
    manager.close_cell(id, &cell).await.unwrap();
}

#[tokio::test]
async fn omp_message_reconcile_sink_keeps_markers_separate_and_scopes_agent_turn() {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
    let manager = &app.agents;
    let id = "00000000-0000-4000-8000-000000000092";
    let other_id = "00000000-0000-4000-8000-000000000093";
    let cell = staged_cell(&app, id).await;
    let other = staged_cell(&app, other_id).await;
    manager.release_event_stage(&cell).await.unwrap();
    manager.release_event_stage(&other).await.unwrap();
    for (marker, text) in [("one", "first"), ("two", "second")] {
        cell.sink.emit(json!({"type":"timeline","provider":"mock","turnId":"live-turn","item":marked(marker,"reasoning",text)})).await.unwrap();
    }
    other.sink.emit(json!({"type":"timeline","provider":"mock","turnId":"live-turn","item":marked("two","assistant_message","other agent")})).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    manager.drain_events(&other).await.unwrap();
    let before = manager.observe_timeline(id, &json!({"limit":0})).await.unwrap();
    let other_before = manager.observe_timeline(other_id, &json!({"limit":0})).await.unwrap();
    assert_eq!(before["rows"].as_array().unwrap().len(), 2);
    let mut notifications = manager.events.subscribe();
    let mut event = correction("two", "corrected");
    event["agentId"] = json!(other_id);
    event["turnId"] = json!("wrong-turn");
    cell.sink.emit(event.clone()).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    assert_eq!(manager.observe_timeline(id, &json!({"limit":0})).await.unwrap(), before);
    let errors = observed(&mut notifications);
    assert!(errors.iter().any(|event| event["event"]["item"]["type"] == "error"
        && event["event"]["item"]["metadata"]["recovery"]["canonicalPreserved"] == true));
    assert!(errors.iter().all(|event| event["type"] != "timeline_replacement"));
    assert!(cell.data.lock().await.event_error.is_none());
    event["turnId"] = json!("live-turn");
    cell.sink.emit(event.clone()).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    let corrected = manager.observe_timeline(id, &json!({"limit":0})).await.unwrap();
    assert_eq!(corrected["rows"][0]["item"]["text"], "first");
    assert_eq!(corrected["rows"][1]["item"]["text"], "corrected");
    assert_eq!(manager.observe_timeline(other_id, &json!({"limit":0})).await.unwrap(), other_before);
    observed(&mut notifications);
    cell.sink.emit(event.clone()).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    assert_eq!(manager.observe_timeline(id, &json!({"limit":0})).await.unwrap(), corrected);
    assert!(observed(&mut notifications).is_empty());
    event["provider"] = json!("unrelated-provider");
    cell.sink.emit(event).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    assert_eq!(manager.observe_timeline(id, &json!({"limit":0})).await.unwrap(), corrected);
    assert!(observed(&mut notifications).iter().any(|event| event["event"]["item"]["type"] == "error"));
    manager.close_cell(id, &cell).await.unwrap();
    manager.close_cell(other_id, &other).await.unwrap();
}

#[tokio::test]
async fn omp_message_reconcile_failed_commit_reports_error_without_losing_owner() {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
    let manager = &app.agents;
    let id = "00000000-0000-4000-8000-000000000094";
    let cell = staged_cell(&app, id).await;
    manager.release_event_stage(&cell).await.unwrap();
    cell.data.lock().await.snapshot["internal"] = json!(false);
    let path = manager.timeline_path(id).unwrap();
    {
        let mut timeline = cell.timeline.lock().unwrap();
        timeline.append_persisted(&path, id, marked("message","assistant_message","old"), &json!({"turnId":"live-turn"})).unwrap();
    }
    let before = manager.observe_timeline(id, &json!({"limit":0})).await.unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_replacement BEFORE DELETE ON timeline_rows BEGIN SELECT RAISE(ABORT, 'replacement refused'); END;").unwrap();
    let mut notifications = manager.events.subscribe();
    cell.sink.emit(correction("message", "new")).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    assert_eq!(manager.observe_timeline(id, &json!({"limit":0})).await.unwrap(), before);
    assert_eq!(Timeline::load(&path, id).unwrap().fetch(&json!({"limit":0})), before);
    assert!(cell.data.lock().await.event_error.is_none());
    let events = observed(&mut notifications);
    assert!(events.iter().any(|event| event["event"]["item"]["type"] == "error"));
    assert!(events.iter().all(|event| event["type"] != "timeline_replacement"));
    connection.execute_batch("DROP TRIGGER reject_replacement").unwrap();
    cell.sink.emit(correction("message", "new")).await.unwrap();
    manager.drain_events(&cell).await.unwrap();
    let events = observed(&mut notifications);
    let replacement = events.iter().find(|event| event["type"] == "timeline_replacement").unwrap();
    let loaded = Timeline::load(&path, id).unwrap();
    assert_eq!(replacement["epoch"], loaded.epoch());
    assert_eq!(loaded.items()[0]["text"], "new");
    cell.data.lock().await.snapshot["internal"] = json!(true);
    manager.close_cell(id, &cell).await.unwrap();
}

#[tokio::test]
async fn omp_message_reconcile_admitted_commit_survives_waiter_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
    let manager = &app.agents;
    let id = "00000000-0000-4000-8000-000000000095";
    let cell = staged_cell(&app, id).await;
    manager.release_event_stage(&cell).await.unwrap();
    cell.data.lock().await.snapshot["internal"] = json!(false);
    let path = manager.timeline_path(id).unwrap();
    let mut notifications = manager.events.subscribe();
    let previous_epoch = {
        let mut guard = cell.timeline.lock().unwrap();
        guard.append_persisted(&path, id, marked("message","assistant_message","old"), &json!({"turnId":"live-turn"})).unwrap();
        let previous_epoch = guard.epoch().to_owned();
        let mut apply = Box::pin(manager.apply_event(id, &cell, correction("message", "committed"), false));
        // Poll admission synchronously: the first pending point is the blocking
        // commit, which cannot acquire the timeline until this scope releases it.
        let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(std::future::Future::poll(apply.as_mut(), &mut context).is_pending());
        drop(apply);
        previous_epoch
    };
    let event = tokio::time::timeout(Duration::from_secs(5), notifications.recv()).await.unwrap().unwrap();
    assert_eq!(event["type"], "timeline_replacement");
    let loaded = Timeline::load(&path, id).unwrap();
    assert_eq!(loaded.items()[0]["text"], "committed");
    assert_eq!(event["epoch"], loaded.epoch());
    assert_ne!(loaded.epoch(), previous_epoch);
    assert_eq!(cell.timeline.lock().unwrap().epoch(), loaded.epoch());
    cell.data.lock().await.snapshot["internal"] = json!(true);
    manager.close_cell(id, &cell).await.unwrap();
}
