use super::*;

const TIMESTAMP: &str = "2026-01-01T00:00:00.000Z";

fn upsert(store: &mut SubagentStore, root: &Path, parent: &str, id: &str) {
    store.apply_persisted(root, parent, "codex",
        &json!({"type":"upsert","id":id,"title":id,"timestamp":TIMESTAMP})).unwrap();
}
fn message_event(id: &str, n: usize) -> Value {
    json!({"type":"timeline","id":id,"timestamp":TIMESTAMP,
        "item":{"type":"assistant_message","text":format!("{id}-{n}"),"messageId":format!("{id}-{n}")}})
}
fn message(store: &mut SubagentStore, root: &Path, parent: &str, id: &str, n: usize) -> Value {
    store.apply_persisted(root, parent, "codex", &message_event(id, n)).unwrap()
}
fn tool(store: &mut SubagentStore, root: &Path, parent: &str, id: &str, status: &str) {
    store.apply_persisted(root, parent, "codex", &json!({"type":"timeline","id":id,"timestamp":TIMESTAMP,
        "item":{"type":"tool_call","callId":"call-1","name":"shell","status":status,
            "detail":{"type":"plain_text","text":format!("tool {status}")}}})).unwrap();
}
fn page(store: &mut SubagentStore, root: &Path, parent: &str, id: &str, options: Value) -> Value {
    store.fetch_persisted(root, parent, id, &options).unwrap()
}
fn listed(store: &mut SubagentStore, root: &Path, parent: &str) -> usize {
    let (subagents, unavailable) = store.list_persisted(root, parent).unwrap();
    assert!(unavailable.is_empty());
    subagents.len()
}
fn scope(error: &anyhow::Error) -> (String, Option<String>) {
    let error = error.downcast_ref::<SubagentStorageUnavailable>().expect("scoped unavailable error");
    (error.parent.clone(), error.child.clone())
}
fn open_catalogs(store: &SubagentStore) -> usize {
    store.catalogs.values().filter(|catalog| catalog.connection.is_some()).count()
}
fn child_key(parent: &str, id: &str) -> (String, String) {
    (parent.to_owned(), id.to_owned())
}

#[test]
fn unavailable_catalog_or_child_is_scoped_and_retains_stored_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    {
        let mut store = SubagentStore::default();
        for (parent, children) in [
            ("healthy", &["child"][..]),
            ("broken-catalog", &["child"][..]),
            ("broken-children", &["intact", "missing", "corrupt", "garbled"][..]),
        ] {
            for child in children {
                upsert(&mut store, root, parent, child);
                message(&mut store, root, parent, child, 1);
                message(&mut store, root, parent, child, 2);
            }
        }
    }
    let garbage = b"not a sqlite database; kept verbatim\n".repeat(128);
    let catalog = root.join("broken-catalog").join("index.sqlite3");
    fs::write(&catalog, &garbage).unwrap();
    let orphaned = timeline_path(root, "broken-catalog", "child");
    let orphaned_bytes = fs::read(&orphaned).unwrap();
    let missing = timeline_path(root, "broken-children", "missing");
    fs::remove_file(&missing).unwrap();
    let corrupt = timeline_path(root, "broken-children", "corrupt");
    fs::write(&corrupt, &garbage).unwrap();
    let children_catalog = root.join("broken-children").join("index.sqlite3");
    let read_garbled = || -> String {
        Connection::open(&children_catalog).unwrap()
            .query_row("SELECT descriptor FROM children WHERE id='garbled'", [], |row| row.get(0)).unwrap()
    };
    Connection::open(&children_catalog).unwrap()
        .execute("UPDATE children SET descriptor='{not json' WHERE id='garbled'", []).unwrap();

    let mut store = SubagentStore::default();
    // An unrelated parent stays fully usable.
    assert_eq!(listed(&mut store, root, "healthy"), 1);
    assert_eq!(page(&mut store, root, "healthy", "child", json!({}))["rows"].as_array().unwrap().len(), 2);

    // A corrupt catalog fails only its own parent, for reads and writes alike.
    let error = store.list_persisted(root, "broken-catalog").unwrap_err();
    assert_eq!(scope(&error), ("broken-catalog".to_owned(), None));
    let error = store.apply_persisted(root, "broken-catalog", "codex",
        &json!({"type":"upsert","id":"child","status":"completed"})).unwrap_err();
    assert_eq!(scope(&error), ("broken-catalog".to_owned(), None));
    assert!(store.fetch_persisted(root, "broken-catalog", "child", &json!({})).is_err());
    assert_eq!(fs::read(&catalog).unwrap(), garbage);
    assert_eq!(fs::read(&orphaned).unwrap(), orphaned_bytes);

    // An unreadable descriptor is reported with the listing, never dropped as
    // if it did not exist, and its stored row is never overwritten.
    let (subagents, unavailable) = store.list_persisted(root, "broken-children").unwrap();
    assert_eq!(subagents.len(), 3);
    assert_eq!(unavailable.len(), 1);
    assert_eq!(unavailable[0].child.as_deref(), Some("garbled"));
    let error = store.apply_persisted(root, "broken-children", "codex",
        &json!({"type":"upsert","id":"garbled","status":"completed"})).unwrap_err();
    assert_eq!(scope(&error), ("broken-children".to_owned(), Some("garbled".to_owned())));
    assert!(store.fetch_persisted(root, "broken-children", "garbled", &json!({})).is_err());
    assert_eq!(read_garbled(), "{not json");

    // Missing and corrupt children fail alone; siblings remain usable.
    for id in ["missing", "corrupt"] {
        let error = store.fetch_persisted(root, "broken-children", id, &json!({})).unwrap_err();
        assert_eq!(scope(&error), ("broken-children".to_owned(), Some(id.to_owned())));
        let error = store.apply_persisted(root, "broken-children", "codex", &message_event(id, 3)).unwrap_err();
        assert_eq!(scope(&error), ("broken-children".to_owned(), Some(id.to_owned())));
        let mut replay = message_event(id, 1);
        replay["historyComplete"] = json!(true);
        let outputs = store.replay_history(Some(root), "broken-children",
            vec![json!({"provider":"codex","event":replay})], false).unwrap();
        assert!(outputs.is_empty());
    }
    // No empty replacement is created, and corrupt bytes are never rewritten.
    assert!(!missing.try_exists().unwrap());
    assert_eq!(fs::read(&corrupt).unwrap(), garbage);
    assert_eq!(page(&mut store, root, "broken-children", "intact", json!({}))["rows"].as_array().unwrap().len(), 2);
    message(&mut store, root, "broken-children", "intact", 3);

    // The cross-parent listing returns every readable descriptor and names each
    // unavailable catalog or child, so the result is visibly incomplete.
    let (subagents, unavailable) = store.list_persisted_parents(root, ["healthy", "broken-catalog", "broken-children"]);
    let mut ids: Vec<_> = subagents.iter()
        .map(|descriptor| format!("{}/{}", descriptor["parentAgentId"].as_str().unwrap(), descriptor["id"].as_str().unwrap()))
        .collect();
    ids.sort();
    assert_eq!(ids, ["broken-children/corrupt", "broken-children/intact", "broken-children/missing", "healthy/child"]);
    let mut failures: Vec<_> = unavailable.iter().map(|error| (error.parent.as_str(), error.child.as_deref())).collect();
    failures.sort();
    assert_eq!(failures, [("broken-catalog", None), ("broken-children", Some("garbled"))]);
    assert_eq!(fs::read(&catalog).unwrap(), garbage);
}

#[test]
fn child_history_hydrates_only_for_writes_under_bounded_residency_with_durable_identity() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let children = ["a", "b", "c", "d"];
    let mut store = SubagentStore::with_residency(2, u64::MAX, 1);
    for id in children {
        upsert(&mut store, root, "parent", id);
        for n in 1..=5 {
            message(&mut store, root, "parent", id, n);
            assert!(store.timelines.len() <= 2);
        }
    }
    upsert(&mut store, root, "other", "x");
    assert!(store.timelines.len() <= 2);
    assert_eq!(open_catalogs(&store), 1);
    let expected: Vec<Value> = children.iter()
        .map(|id| page(&mut store, root, "parent", id, json!({"limit":0})))
        .collect();
    for page in &expected {
        assert_eq!(page["rows"].as_array().unwrap().len(), 5);
    }
    drop(store);

    // A fresh store indexes descriptors and pages history without hydrating it.
    let mut fresh = SubagentStore::with_residency(2, u64::MAX, 1);
    assert_eq!(listed(&mut fresh, root, "parent"), 4);
    let tail = page(&mut fresh, root, "parent", "a", json!({"direction":"tail","limit":2}));
    assert_eq!(tail["epoch"], expected[0]["epoch"]);
    assert_eq!(tail["hasOlder"], true);
    assert_eq!(tail["rows"].as_array().unwrap()[..], expected[0]["rows"].as_array().unwrap()[3..]);
    let before = page(&mut fresh, root, "parent", "a",
        json!({"direction":"before","limit":2,"cursor":{"epoch":tail["epoch"],"seq":tail["startSeq"]}}));
    assert_eq!(before["rows"].as_array().unwrap()[..], expected[0]["rows"].as_array().unwrap()[1..3]);
    assert!(fresh.timelines.is_empty());

    // Writes hydrate under the count bound; an evicted child continues its
    // stored sequence and epoch.
    message(&mut fresh, root, "parent", "a", 6);
    message(&mut fresh, root, "parent", "b", 6);
    message(&mut fresh, root, "parent", "c", 6);
    assert_eq!(fresh.timelines.len(), 2);
    assert!(!fresh.timelines.contains_key(&child_key("parent", "a")));
    let appended = message(&mut fresh, root, "parent", "a", 7);
    assert_eq!(appended["epoch"], expected[0]["epoch"]);
    assert_eq!(appended["row"]["seq"], 7);

    // A descriptor upsert of an evicted child never resets its history.
    message(&mut fresh, root, "parent", "b", 7);
    message(&mut fresh, root, "parent", "c", 7);
    assert!(!fresh.timelines.contains_key(&child_key("parent", "a")));
    fresh.apply_persisted(root, "parent", "codex",
        &json!({"type":"upsert","id":"a","status":"completed","timestamp":"2026-01-02T00:00:00.000Z"})).unwrap();
    assert!(!fresh.timelines.contains_key(&child_key("parent", "a")));
    assert_eq!(fresh.get("parent", "a").unwrap()["createdAt"], TIMESTAMP);
    let reloaded = page(&mut fresh, root, "parent", "a", json!({"limit":0}));
    let rows = reloaded["rows"].as_array().unwrap();
    assert_eq!(reloaded["epoch"], expected[0]["epoch"]);
    assert_eq!(rows.len(), 7);
    assert_eq!(rows[..5], expected[0]["rows"].as_array().unwrap()[..]);
    assert_eq!(rows[5]["item"]["text"], "a-6");
    assert_eq!(rows[6]["item"]["text"], "a-7");

    // Inactive release drops hydrated children and the catalog connection only.
    fresh.release_parent("parent");
    assert!(fresh.timelines.is_empty());
    assert_eq!(fresh.residency.bytes, 0);
    assert_eq!(open_catalogs(&fresh), 0);
    assert_eq!(listed(&mut fresh, root, "parent"), 4);

    // Explicit removal of an unhydrated child still deletes its storage.
    let removed = timeline_path(root, "parent", "d");
    fresh.apply_persisted(root, "parent", "codex", &json!({"type":"remove","id":"d"})).unwrap();
    assert!(!removed.try_exists().unwrap());
    assert!(fresh.fetch_persisted(root, "parent", "d", &json!({})).is_err());

    let mut restarted = SubagentStore::default();
    assert_eq!(listed(&mut restarted, root, "parent"), 3);
    assert_eq!(restarted.get("parent", "a").unwrap()["status"], "completed");
    assert_eq!(page(&mut restarted, root, "parent", "a", json!({"limit":0})), reloaded);
}

#[test]
fn stored_pages_match_hydrated_pages_and_oversized_history_is_not_retained() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut hydrated = SubagentStore::default();
    upsert(&mut hydrated, root, "parent", "child");
    // Rows: m1 | tool (seq 2 and 5, non-contiguous source ranges) | m2 | m3 | m4.
    message(&mut hydrated, root, "parent", "child", 1);
    tool(&mut hydrated, root, "parent", "child", "running");
    message(&mut hydrated, root, "parent", "child", 2);
    message(&mut hydrated, root, "parent", "child", 3);
    tool(&mut hydrated, root, "parent", "child", "completed");
    message(&mut hydrated, root, "parent", "child", 4);
    assert!(hydrated.timelines.contains_key(&child_key("parent", "child")));
    let epoch = page(&mut hydrated, root, "parent", "child", json!({"limit":1}))["epoch"].clone();

    // One byte of residency: nothing written or read stays hydrated.
    let mut bounded = SubagentStore::with_residency(16, 1, 8);
    for options in [
        json!({}),
        json!({"limit":0}),
        json!({"direction":"tail","limit":2}),
        json!({"direction":"tail","limit":1}),
        json!({"direction":"before","limit":2,"cursor":{"epoch":epoch,"seq":4}}),
        json!({"direction":"before","limit":0,"cursor":{"epoch":epoch,"seq":3}}),
        json!({"direction":"after","limit":2,"cursor":{"epoch":epoch,"seq":1}}),
        json!({"direction":"after","limit":1,"cursor":{"epoch":epoch,"seq":3}}),
        json!({"direction":"after","cursor":{"epoch":epoch,"seq":6}}),
        json!({"direction":"after","limit":2,"cursor":{"epoch":"stale","seq":2}}),
    ] {
        assert_eq!(page(&mut bounded, root, "parent", "child", options.clone()),
            page(&mut hydrated, root, "parent", "child", options), "page parity");
        assert!(bounded.timelines.is_empty());
    }
    drop(hydrated);

    // A write to an oversized child hydrates it only for that write.
    let appended = message(&mut bounded, root, "parent", "child", 5);
    assert_eq!(appended["epoch"], epoch);
    assert_eq!(appended["row"]["seq"], 7);
    assert!(bounded.timelines.is_empty());
    assert_eq!(bounded.residency.bytes, 0);
    upsert(&mut bounded, root, "parent", "fresh");
    message(&mut bounded, root, "parent", "fresh", 1);
    assert!(bounded.timelines.is_empty());

    // So does a covering replay replacement.
    let replay: Vec<Value> = (1..=6).map(|n| {
        let mut event = message_event("fresh", n);
        event["historyComplete"] = json!(true);
        json!({"provider":"codex","event":event})
    }).collect();
    assert_eq!(bounded.replay_history(Some(root), "parent", replay, false).unwrap().len(), 1);
    assert!(bounded.timelines.is_empty());
    assert_eq!(bounded.residency.bytes, 0);
    assert_eq!(page(&mut bounded, root, "parent", "fresh", json!({"limit":0}))["rows"].as_array().unwrap().len(), 6);
    let rows = page(&mut bounded, root, "parent", "child", json!({"limit":0}));
    assert_eq!(rows["rows"].as_array().unwrap().len(), 6);
    assert_eq!(rows["rows"][5]["item"]["text"], "child-5");
}

#[test]
fn replay_reconciles_against_stored_child_that_is_not_hydrated() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut store = SubagentStore::default();
    upsert(&mut store, root, "parent", "child");
    for n in 1..=3 {
        message(&mut store, root, "parent", "child", n);
    }
    let stored = page(&mut store, root, "parent", "child", json!({"limit":0}));
    drop(store);
    let replay = |count: usize, complete: bool| -> Vec<Value> {
        (1..=count).map(|n| {
            let mut event = message_event("child", n);
            event["historyComplete"] = json!(complete);
            json!({"provider":"codex","event":event})
        }).collect()
    };

    // A partial replay cannot shrink canonical rows that were never hydrated.
    let mut fresh = SubagentStore::default();
    assert!(fresh.replay_history(Some(root), "parent", replay(1, false), false).unwrap().is_empty());
    assert_eq!(page(&mut fresh, root, "parent", "child", json!({"limit":0})), stored);
    // Matching identities do not authorize an older body to replace live text.
    for (complete, text) in [
        (false, "child"), (true, "child"),
        (false, "different child body"), (true, "different child body"),
    ] {
        let mut stale = replay(3, complete);
        stale[0]["event"]["item"]["text"] = json!(text);
        assert!(fresh.replay_history(Some(root), "parent", stale, false).unwrap().is_empty());
        assert_eq!(page(&mut fresh, root, "parent", "child", json!({"limit":0})), stored);
    }
    let mut restarted = SubagentStore::default();
    assert_eq!(page(&mut restarted, root, "parent", "child", json!({"limit":0})), stored);

    // A complete covering replay replaces the history durably.
    let mut fresh = SubagentStore::default();
    assert_eq!(fresh.replay_history(Some(root), "parent", replay(4, true), false).unwrap().len(), 1);
    let mut restarted = SubagentStore::default();
    let replaced = page(&mut restarted, root, "parent", "child", json!({"limit":0}));
    assert_eq!(replaced["rows"].as_array().unwrap().len(), 4);
    assert_eq!(listed(&mut restarted, root, "parent"), 1);
}

fn finished(store: &mut SubagentStore, root: &Path, parent: &str, id: &str, status: &str) {
    store.apply_persisted(root, parent, "omp",
        &json!({"type":"upsert","id":id,"title":id,"status":status,"timestamp":TIMESTAMP})).unwrap();
}

#[test]
fn deleting_finished_children_removes_them_and_skips_running_or_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let mut store = SubagentStore::default();
    for (id, status) in [("done", "completed"), ("broke", "failed"), ("busy", "running")] {
        finished(&mut store, root, "parent", id, status);
        message(&mut store, root, "parent", id, 1);
    }
    let ids = ["done", "broke", "busy", "ghost", "done"].map(str::to_owned);
    let result = store.delete_persisted(root, "parent", &ids).unwrap();
    assert_eq!(result.deleted, ["done", "broke"]);
    assert_eq!(result.skipped, [("busy".to_owned(), "running".to_owned()), ("ghost".to_owned(), "unknown".to_owned())]);
    assert!(!timeline_path(root, "parent", "done").try_exists().unwrap());
    assert!(timeline_path(root, "parent", "busy").try_exists().unwrap());
    assert!(store.fetch_persisted(root, "parent", "done", &json!({})).is_err());
    assert_eq!(listed(&mut store, root, "parent"), 1);
    let mut restarted = SubagentStore::default();
    assert_eq!(listed(&mut restarted, root, "parent"), 1);
    // A parent without stored children has nothing to delete.
    let result = restarted.delete_persisted(root, "nobody", &["x".to_owned()]).unwrap();
    assert!(result.deleted.is_empty());
    assert_eq!(result.skipped, [("x".to_owned(), "unknown".to_owned())]);
}

/// Provider history re-ingested after a daemon restart or agent resume, and
/// late provider events, must not restore a deleted child; only a live
/// `running` upsert (the child genuinely runs again) brings it back.
#[test]
fn deleted_children_are_not_resurrected_by_replay_or_late_events() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    {
        let mut store = SubagentStore::default();
        finished(&mut store, root, "parent", "done", "completed");
        message(&mut store, root, "parent", "done", 1);
        store.delete_persisted(root, "parent", &["done".to_owned()]).unwrap();
    }
    let mut restarted = SubagentStore::default();
    let mut timeline = message_event("done", 1);
    timeline["historyComplete"] = json!(true);
    let replay = vec![
        json!({"provider":"omp","event":{"type":"upsert","id":"done","status":"completed","timestamp":TIMESTAMP}}),
        // Replayed spawn declarations report `running`: still history, not a new run.
        json!({"provider":"omp","event":{"type":"upsert","id":"done","status":"running","timestamp":TIMESTAMP}}),
        json!({"provider":"omp","event":timeline}),
    ];
    assert!(restarted.replay_history(Some(root), "parent", replay, false).unwrap().is_empty());
    assert!(restarted.replay_history(Some(root), "parent",
        vec![json!({"provider":"omp","event":timeline})], true).unwrap().is_empty());
    assert!(restarted.apply_persisted(root, "parent", "omp", &message_event("done", 2)).unwrap().is_null());
    assert!(restarted.apply_persisted(root, "parent", "omp",
        &json!({"type":"upsert","id":"done","source":{"file":"/tmp/done.jsonl"}})).unwrap().is_null());
    assert!(restarted.apply_persisted_event(root, "parent", "omp",
        &json!({"type":"upsert","id":"done","status":"running"}), false).unwrap().is_null());
    assert_eq!(listed(&mut restarted, root, "parent"), 0);
    assert!(!timeline_path(root, "parent", "done").try_exists().unwrap());

    // The child runs again: the tombstone is lifted durably.
    let output = restarted.apply_persisted(root, "parent", "omp",
        &json!({"type":"upsert","id":"done","status":"running","timestamp":TIMESTAMP})).unwrap();
    assert_eq!(output["subagent"]["status"], "running");
    message(&mut restarted, root, "parent", "done", 3);
    let mut again = SubagentStore::default();
    assert_eq!(listed(&mut again, root, "parent"), 1);
    assert_eq!(page(&mut again, root, "parent", "done", json!({"limit":0}))["rows"].as_array().unwrap().len(), 1);
}
