use super::*;

fn item(marker: &str, ty: &str, text: &str) -> Value {
    json!({"type":ty,"text":text,"metadata":{"ompStreamMessageId":marker}})
}
fn authority(marker: &str, items: Vec<Value>) -> Value {
    json!({"type":"timeline_reconcile","provider":"omp","turnId":"turn","messageId":marker,"items":items})
}
fn options() -> Value {
    json!({"turnId":"turn","timestamp":"2026-11-14T01:02:03.000Z"})
}
fn page(timeline: &Timeline) -> Value { timeline.fetch(&json!({"limit":0})) }

#[test]
fn omp_message_reconcile_shortens_rewrites_unicode_and_invalidates_pages() {
    for (before, after) in [("long answer", "long"), ("wrong answer", "right answer"), ("안녕하세요🙂", "안녕🌍")] {
        let mut timeline = Timeline::new();
        let mut original = item("message", "assistant_message", before);
        original["messageId"] = json!("durable-answer");
        timeline.append(original, &options());
        let previous = page(&timeline);
        let event = authority("message", vec![item("message", "assistant_message", after)]);
        let epoch = timeline.reconcile_message(&event, None).unwrap().unwrap();
        let updated = page(&timeline);
        assert_eq!(updated["rows"][0]["item"]["text"], after);
        assert_eq!(updated["rows"][0]["item"]["messageId"], "durable-answer");
        assert_eq!(updated["rows"][0]["turnId"], previous["rows"][0]["turnId"]);
        assert_eq!(updated["rows"][0]["timestamp"], previous["rows"][0]["timestamp"]);
        assert_ne!(updated["epoch"], previous["epoch"]);
        for direction in ["before", "after"] {
            let stale = timeline.fetch(&json!({"direction":direction,"cursor":{"epoch":previous["epoch"],"seq":1}}));
            assert_eq!(stale["reset"], true);
            assert_eq!(stale["staleCursor"], true);
            assert_eq!(stale["rows"][0]["item"]["text"], after);
        }
        assert!(timeline.reconcile_message(&event, None).unwrap().is_none());
        assert_eq!(timeline.epoch(), epoch);
        assert_eq!(page(&timeline), updated);
    }
}

#[test]
fn omp_message_reconcile_reorders_changes_and_drops_blocks_preserving_neighbors() {
    let mut timeline = Timeline::new();
    let user = json!({"type":"user_message","text":"question","messageId":"client","clientMessageId":"client"});
    let tool = json!({"type":"tool_call","callId":"tool","name":"read","status":"completed","detail":{"type":"read","filePath":"file"}});
    timeline.append(user.clone(), &json!({"turnId":"turn","timestamp":"user-time","providerMessageId":"native-user"}));
    timeline.append(item("message", "reasoning", "old thought"), &options());
    let mut answer = item("message", "assistant_message", "old answer");
    answer["messageId"] = json!("answer");
    timeline.append(answer, &json!({"turnId":"turn","timestamp":"answer-time","providerMessageId":"native-answer"}));
    timeline.append(tool.clone(), &json!({"turnId":"turn","timestamp":"tool-time"}));
    let event = authority("message", vec![item("message", "assistant_message", "new answer"), item("message", "reasoning", "new thought")]);
    timeline.reconcile_message(&event, None).unwrap().unwrap();
    let rows = timeline.rows();
    assert_eq!(rows[0]["item"], user);
    assert_eq!(rows[0]["providerMessageId"], "native-user");
    assert_eq!(rows[0]["timestamp"], "user-time");
    assert_eq!(rows[1]["item"]["messageId"], "answer");
    assert_eq!(rows[1]["providerMessageId"], "native-answer");
    assert_eq!(rows[1]["timestamp"], "answer-time");
    assert_eq!(rows[1]["item"]["text"], "new answer");
    assert_eq!(rows[2]["item"]["text"], "new thought");
    assert_eq!(rows[3]["item"], tool);
    assert_eq!(rows[3]["timestamp"], "tool-time");
    assert_eq!(timeline.submitted_user_message("client").unwrap()["providerMessageId"], "native-user");

    // Drop the reasoning block, then the whole native message. Never drop its neighbors.
    timeline.reconcile_message(&authority("message", vec![item("message", "assistant_message", "final")]), None).unwrap().unwrap();
    assert_eq!(timeline.items().len(), 3);
    let empty = authority("message", vec![]);
    timeline.reconcile_message(&empty, None).unwrap().unwrap();
    assert_eq!(timeline.items(), vec![user, tool]);
    let epoch = timeline.epoch().to_owned();
    assert!(timeline.reconcile_message(&empty, None).unwrap().is_none());
    assert_eq!(timeline.epoch(), epoch);

    let mut changed = Timeline::new();
    let mut old_block = item("changed", "reasoning", "not thinking");
    old_block["messageId"] = json!("changed-durable");
    changed.append(old_block, &options());
    changed.reconcile_message(&authority("changed", vec![item("changed", "assistant_message", "answer")]), None).unwrap().unwrap();
    assert_eq!(changed.items()[0]["type"], "assistant_message");
    assert_eq!(changed.items()[0]["text"], "answer");
    assert_eq!(changed.items()[0]["messageId"], "changed-durable");
}

#[test]
fn omp_message_reconcile_marker_boundaries_and_adjacent_blocks() {
    for ty in ["reasoning", "assistant_message"] {
        let mut timeline = Timeline::new();
        timeline.append(item("one", ty, "a"), &options());
        timeline.append(item("two", ty, "b"), &options());
        timeline.append(item("two", ty, "c"), &options());
        timeline.append(json!({"type":ty,"text":"unmarked"}), &options());
        timeline.append(json!({"type":ty,"text":" tail"}), &options());
        assert_eq!(timeline.items().len(), 3);
        assert_eq!(timeline.items()[1]["text"], "bc");
        assert_eq!(timeline.items()[2]["text"], "unmarked tail");
        let event = authority("two", vec![item("two", ty, "B"), item("two", ty, "C")]);
        timeline.reconcile_message(&event, None).unwrap().unwrap();
        assert_eq!(timeline.items()[0]["text"], "a");
        assert_eq!(timeline.items()[1]["text"], "BC");
        assert_eq!(timeline.items()[2]["text"], "unmarked tail");
        assert!(timeline.reconcile_message(&event, None).unwrap().is_none());
    }
}

#[test]
fn omp_message_reconcile_rejects_wrong_turn_interleaving_and_invalid_items() {
    let mut timeline = Timeline::new();
    timeline.append(item("message", "assistant_message", "keep"), &options());
    let event = authority("message", vec![item("message", "assistant_message", "replace")]);
    let before = page(&timeline);
    let mut wrong_turn = event.clone();
    wrong_turn["turnId"] = json!("other-turn");
    let mut wrong_marker = event.clone();
    wrong_marker["items"][0]["metadata"][MARKER] = json!("other");
    let mut wrong_type = event.clone();
    wrong_type["items"][0]["type"] = json!("user_message");
    let mut wrong_text = event.clone();
    wrong_text["items"][0]["text"] = Value::Null;
    let mut missing = event.clone();
    missing["messageId"] = json!("missing");
    missing["items"][0]["metadata"][MARKER] = json!("missing");
    for bad in [wrong_turn, wrong_marker, wrong_type, wrong_text, missing] {
        assert!(timeline.reconcile_message(&bad, None).is_err());
        assert_eq!(page(&timeline), before);
    }
    timeline.append(item("message", "reasoning", "other turn"), &json!({"turnId":"other-turn"}));
    let before = page(&timeline);
    assert!(timeline.reconcile_message(&event, None).is_err());
    assert_eq!(page(&timeline), before);

    for middle in [json!({"type":"user_message","text":"unrelated"}), json!({"type":"tool_call","callId":"tool","status":"completed"})] {
        let mut timeline = Timeline::new();
        timeline.append(item("message", "assistant_message", "first"), &options());
        timeline.append(middle, &options());
        timeline.append(item("message", "reasoning", "second"), &options());
        let before = page(&timeline);
        assert!(timeline.reconcile_message(&event, None).is_err());
        assert_eq!(page(&timeline), before);
    }
    // Identity-collapsed tool updates may hide the interleaving in display order.
    let mut timeline = Timeline::new();
    timeline.append(json!({"type":"tool_call","callId":"tool","status":"running"}), &options());
    timeline.append(item("message", "assistant_message", "first"), &options());
    timeline.append(json!({"type":"tool_call","callId":"tool","status":"completed"}), &options());
    timeline.append(item("message", "reasoning", "second"), &options());
    let before = page(&timeline);
    assert!(timeline.reconcile_message(&event, None).is_err());
    assert_eq!(page(&timeline), before);
}

#[test]
fn omp_message_reconcile_persistence_failure_retains_checkpoint_and_retry_reloads() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("timeline.sqlite3");
    let mut timeline = Timeline::new();
    timeline.append(item("message", "assistant_message", "old text"), &options());
    timeline.persist(&path, "agent").unwrap();
    let before = page(&timeline);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_replacement BEFORE DELETE ON timeline_rows BEGIN SELECT RAISE(ABORT, 'replacement refused'); END;").unwrap();
    let event = authority("message", vec![item("message", "assistant_message", "final🙂")]);
    assert!(timeline.reconcile_message(&event, Some((&path, "agent"))).is_err());
    assert_eq!(page(&timeline), before);
    assert_eq!(page(&Timeline::load(&path, "agent").unwrap()), before);
    connection.execute_batch("DROP TRIGGER reject_replacement").unwrap();
    let epoch = timeline.reconcile_message(&event, Some((&path, "agent"))).unwrap().unwrap();
    let mut loaded = Timeline::load(&path, "agent").unwrap();
    assert_eq!(page(&loaded), page(&timeline));
    assert_eq!(loaded.epoch(), epoch);
    assert!(loaded.reconcile_message(&event, Some((&path, "agent"))).unwrap().is_none());
    let stale = Timeline::fetch_saved(&path, "agent", &json!({"direction":"after","cursor":{"epoch":before["epoch"],"seq":1}})).unwrap();
    assert_eq!(stale["reset"], true);
    assert_eq!(stale["rows"][0]["item"]["text"], "final🙂");
    loaded.append_persisted(&path, "agent", item("next", "assistant_message", "next message"), &options()).unwrap();
    assert_eq!(Timeline::load(&path, "agent").unwrap().items().len(), 2);
}
