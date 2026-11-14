use super::*;
fn options() -> Value {
    json!({"timestamp":"2026-10-01T00:00:00.000Z","turnId":"turn"})
}
fn tool(status: &str) -> Value {
    json!({"type":"tool_call","callId":"call","name":"read","status":status,"detail":{"type":"read","filePath":"file"},"opaque":{"keep":true}})
}
fn interleaved() -> Timeline {
    let mut timeline = Timeline::seed(&json!({"epoch":"epoch"})).unwrap();
    timeline.append(tool("running"), &options());
    timeline.append(json!({"type":"user_message","text":"middle"}), &options());
    timeline.append(tool("completed"), &options());
    timeline
}
#[test]
fn noncontiguous_tool_ranges_do_not_skip_after_pages() {
    let timeline = interleaved();
    let first =
        timeline.fetch(&json!({"direction":"after","cursor":{"epoch":"epoch","seq":0},"limit":1}));
    assert_eq!(first["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        first["rows"][0]["sourceSeqRanges"],
        json!([{"startSeq":1,"endSeq":1},{"startSeq":3,"endSeq":3}])
    );
    assert_eq!(first["endSeq"], 1);
    assert_eq!(first["hasNewer"], true);
    let second =
        timeline.fetch(&json!({"direction":"after","cursor":{"epoch":"epoch","seq":1},"limit":1}));
    assert_eq!(second["rows"][0]["item"]["type"], "user_message");
    assert_eq!(second["endSeq"], 2);
    let third =
        timeline.fetch(&json!({"direction":"after","cursor":{"epoch":"epoch","seq":2},"limit":1}));
    assert_eq!(third["rows"][0]["item"]["status"], "completed");
    assert_eq!(third["endSeq"], 3);
}
#[test]
fn tail_expands_interleaved_updates_but_before_uses_display_anchors() {
    let timeline = interleaved();
    let tail = timeline.fetch(&json!({"limit":1}));
    assert_eq!(tail["rows"].as_array().unwrap().len(), 2);
    assert_eq!(tail["startSeq"], 1);
    assert_eq!(tail["endSeq"], 3);
    let older =
        timeline.fetch(&json!({"direction":"before","cursor":{"epoch":"epoch","seq":3},"limit":1}));
    assert_eq!(older["rows"][0]["seqStart"], 2);
    assert_eq!(older["hasOlder"], true);
    let oldest =
        timeline.fetch(&json!({"direction":"before","cursor":{"epoch":"epoch","seq":2},"limit":1}));
    assert_eq!(oldest["rows"][0]["seqStart"], 1);
    assert_eq!(oldest["rows"][0]["seqEnd"], 3);
    assert_eq!(oldest["hasOlder"], false);
}
#[test]
fn epoch_and_retention_gaps_reset_to_tail() {
    let timeline = Timeline::seed(
        &json!({"epoch":"current","nextSeq":7,"items":[{"type":"user_message","text":"retained"}]}),
    )
    .unwrap();
    let stale = timeline.fetch(&json!({"direction":"before","cursor":{"epoch":"old","seq":20}}));
    assert_eq!(stale["reset"], true);
    assert_eq!(stale["staleCursor"], true);
    assert_eq!(stale["direction"], "before");
    let gap = timeline.fetch(&json!({"direction":"after","cursor":{"epoch":"current","seq":2}}));
    assert_eq!(gap["gap"], true);
    assert_eq!(gap["reset"], true);
    assert_eq!(gap["startSeq"], 7);
    let adjacent =
        timeline.fetch(&json!({"direction":"after","cursor":{"epoch":"current","seq":6}}));
    assert_eq!(adjacent["gap"], false);
    assert_eq!(adjacent["endSeq"], 7);
}
#[test]
fn text_merges_respect_turn_and_message_identity() {
    let mut timeline = Timeline::new();
    timeline.append(
        json!({"type":"assistant_message","text":"a","messageId":"m"}),
        &options(),
    );
    timeline.append(json!({"type":"assistant_message","text":"b"}), &options());
    timeline.append(
        json!({"type":"assistant_message","text":"c","messageId":"other"}),
        &options(),
    );
    timeline.append(
        json!({"type":"assistant_message","text":"d","messageId":"other"}),
        &json!({"turnId":"second"}),
    );
    assert_eq!(
        timeline
            .items()
            .iter()
            .map(|v| v["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["ab", "c", "d"]
    );
    assert_eq!(timeline.rows()[0]["collapsed"], json!(["assistant_merge"]));
}
#[test]
fn tool_merge_keeps_known_details_opaque_metadata_and_terminal_error_rules() {
    let mut existing = tool("failed");
    existing["error"] = json!({"content":"failed"});
    existing["metadata"] = json!({"a":1,"clear":true});
    let incoming = json!({"type":"tool_call","callId":"call","status":"completed","detail":{"type":"unknown","input":null},"metadata":{"b":2,"clear":null}});
    let merged = merge_tool_items(&existing, &incoming);
    assert_eq!(merged["detail"]["type"], "read");
    assert_eq!(merged["error"], Value::Null);
    assert_eq!(merged["metadata"], json!({"a":1,"clear":null,"b":2}));
    assert_eq!(merged["opaque"], json!({"keep":true}));
}
#[test]
fn tool_identity_does_not_merge_across_turns() {
    let mut timeline = Timeline::new();
    timeline.append(tool("running"), &options());
    timeline.append(tool("completed"), &json!({"turnId":"next"}));
    assert_eq!(timeline.items().len(), 2);
}
#[test]
fn checkpoint_recovery_retains_cursor_ranges_and_user_enrichment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.sqlite3");
    let mut timeline = interleaved();
    timeline.append_persisted(&path,"agent",json!({"type":"user_message","text":"accepted","clientMessageId":"client","opaque":null}),&options()).unwrap();
    let recovered = Timeline::load(&path, "agent").unwrap();
    assert_eq!(
        recovered.fetch(&json!({"limit":0})),
        timeline.fetch(&json!({"limit":0}))
    );
    assert_eq!(
        recovered.submitted_user_message("client").unwrap()["item"]["opaque"],
        Value::Null
    );
    let mut recovered = recovered;
    let enriched = recovered
        .enrich_submitted_user_message("client", "provider-id")
        .unwrap();
    assert_eq!(enriched["providerMessageId"], "provider-id");
    assert!(Timeline::load(&path, "wrong").is_err());
}
#[test]
fn corrupt_checkpoint_is_not_silently_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("timeline.sqlite3");
    std::fs::write(&path, "{broken").unwrap();
    assert!(Timeline::load(&path, "agent").is_err());
    assert!(
        Timeline::load(&dir.path().join("missing"), "agent")
            .unwrap()
            .items()
            .is_empty()
    );
}
#[test]
fn failed_checkpoint_does_not_publish_mutated_projection() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("file");
    std::fs::write(&blocker, "not a directory").unwrap();
    let mut timeline = interleaved();
    let before = timeline.fetch(&json!({"limit":0}));
    assert!(
        timeline
            .append_persisted(&blocker.join("child"), "agent", tool("failed"), &options())
            .is_err()
    );
    assert_eq!(timeline.fetch(&json!({"limit":0})), before);
}
#[test]
fn coalescer_leading_trailing_and_terminal_flush() {
    let mut coalescer = StreamCoalescer::default();
    let event = |text| json!({"type":"timeline","provider":"codex","turnId":"turn","item":{"type":"assistant_message","text":text,"messageId":"m"}});
    assert_eq!(coalescer.handle("a", &event("first"), 0).unwrap().len(), 1);
    assert!(coalescer.handle("a", &event("a"), 1).unwrap().is_empty());
    assert!(coalescer.handle("a", &event("b"), 2).unwrap().is_empty());
    assert_eq!(coalescer.next_deadline(), Some(61));
    assert!(coalescer.flush_due(60).is_empty());
    assert_eq!(coalescer.flush_due(61)[0]["item"]["text"], "ab");
    assert!(
        coalescer
            .handle(
                "a",
                &json!({"type":"timeline","provider":"codex","item":tool("running")}),
                62
            )
            .unwrap()
            .is_empty()
    );
    let out = coalescer
        .handle(
            "a",
            &json!({"type":"timeline","provider":"codex","item":tool("completed")}),
            63,
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["item"]["status"], "completed");
    assert!(coalescer.next_deadline().is_none());
}
#[test]
fn subagent_upserts_preserve_omitted_and_clear_explicit_null() {
    let mut store = SubagentStore::default();
    store.apply("a","codex",&json!({"type":"upsert","id":"child","title":"Title","status":"completed","timestamp":"2026-01-01"})).unwrap();
    store
        .apply(
            "a",
            "codex",
            &json!({"type":"upsert","id":"child","subtitle":"New","timestamp":"2026-01-02"}),
        )
        .unwrap();
    assert_eq!(store.get("a", "child").unwrap()["title"], "Title");
    assert_eq!(store.get("a", "child").unwrap()["status"], "completed");
    store
        .apply(
            "a",
            "codex",
            &json!({"type":"upsert","id":"child","title":null}),
        )
        .unwrap();
    assert_eq!(store.get("a", "child").unwrap()["title"], Value::Null);
    store.apply("a","codex",&json!({"type":"timeline","id":"child","item":{"type":"assistant_message","text":"hello"}})).unwrap();
    assert_eq!(
        store.fetch_timeline("a", "child", &json!({})).unwrap()["rows"][0]["item"]["text"],
        "hello"
    );
    assert_eq!(store.delete_parent("a").len(), 1);
    assert!(store.fetch_timeline("a", "child", &json!({})).is_err());
}
#[test]
fn curation_presents_tools_messages_and_tasks() {
    let items = vec![
        json!({"type":"assistant_message","text":" hi "}),
        json!({"type":"reasoning","text":" think "}),
        json!({"type":"tool_call","callId":"t","name":"read","detail":{"type":"read","filePath":"src/main.rs"}}),
        json!({"type":"todo","items":[{"text":"work","completed":true}]}),
        json!({"type":"tool_call","callId":"m","agentMessage":{"event":"finished","sender":{"id":"peer","title":"Peer"},"text":" done "}}),
    ];
    let entries = crate::activity::curate(&items, &json!({"labelAssistantMessages":true})).unwrap();
    assert_eq!(entries,json!([{"text":"[Assistant] hi"},{"text":"[Thought] think"},{"text":"[Read] src/main.rs"},{"text":"[Tasks]"},{"text":"- [x] work"},{"text":"[Agent finished] Peer"},{"text":"done"}]).as_array().unwrap().clone());
}
#[test]
fn fork_budget_and_checkpoint_crossing_are_enforced() {
    let timeline = interleaved();
    let bad = crate::activity::build_fork_context(
        &json!({"rows":timeline.rows(),"cursorBoundary":{"timelineEpoch":"epoch","cursor":{"epoch":"epoch","seq":2}}}),
    );
    assert!(bad.unwrap_err().to_string().contains("checkpoint changed"));
    let huge = "😀".repeat(60_000);
    let row = json!({"seq":1,"timestamp":"now","item":{"type":"assistant_message","text":huge}});
    let fork =
        crate::activity::build_fork_context(&json!({"rows":[row],"agentTitle":" Title "})).unwrap();
    let text = fork["attachment"]["text"].as_str().unwrap();
    assert!(text.encode_utf16().count() <= 100_000);
    assert!(text.contains("[Message truncated]"));
    assert!(text.contains("Source agent: Title"));
    assert_eq!(fork["itemCount"], 1);
}
#[test]
fn search_counts_rendered_blocks_and_preserves_literal_user_text() {
    let timeline=Timeline::seed(&json!({"items":[{"type":"assistant_message","text":"hello &amp; **world**\nnext line\n\n- target one\n- target two"},{"type":"user_message","text":"hello **world**"}]})).unwrap();
    assert_eq!(
        timeline.search(&json!({"query":"hello & world"})).unwrap()["locations"],
        json!([{"seq":1,"role":"assistant","count":1}])
    );
    assert!(
        timeline.search(&json!({"query":"one target"})).unwrap()["locations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        timeline.search(&json!({"query":"target"})).unwrap()["locations"][0]["count"],
        2
    );
}

#[test]
fn curated_activity_preserves_messages_and_controls_external_input() {
    let items = vec![
        json!({"type":"assistant_message","text":"a"}),
        json!({"type":"assistant_message","text":"b"}),
        json!({"type":"tool_call","callId":"external","name":"mcp__paseo__send_agent_prompt","status":"completed","detail":{"type":"unknown","input":{"prompt":"Hello peer","agentId":"peer"}}}),
    ];
    let full = crate::activity::curate(&items, &json!({})).unwrap();
    assert_eq!(full[0]["text"], "ab");
    let text = full[1]["text"].as_str().unwrap();
    let input: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(input, items[2]["detail"]["input"]);
    let concise =
        crate::activity::curate(&items, &json!({"includeExternalToolInput":false})).unwrap();
    let concise_text = concise[1]["text"].as_str().unwrap();
    assert!(concise_text.contains("Hello peer"));
    assert!(!concise_text.contains("agentId"));
}

#[test]
fn plugin_replacement_retains_anchor_and_opaque_data() {
    let mut timeline = Timeline::new();
    timeline.append(
        json!({"type":"plugin","pluginId":"plugin","id":"item","data":{"version":1}}),
        &options(),
    );
    timeline.append(
        json!({"type":"user_message","text":"separator"}),
        &options(),
    );
    timeline.append(
        json!({"type":"plugin","pluginId":"plugin","id":"item","data":{"version":2,"opaque":null}}),
        &options(),
    );
    assert_eq!(timeline.items().len(), 2);
    assert_eq!(timeline.rows()[0]["seqStart"], 1);
    assert_eq!(timeline.rows()[0]["seqEnd"], 3);
    assert_eq!(
        timeline.items()[0]["data"],
        json!({"version":2,"opaque":null})
    );
}

#[test]
fn search_paging_retains_every_location() {
    let items: Vec<Value> = (0..202)
        .map(|_| json!({"type":"user_message","text":"target"}))
        .collect();
    let timeline = Timeline::seed(&json!({"items":items})).unwrap();
    let first = timeline.search(&json!({"query":"target"})).unwrap();
    assert_eq!(first["locations"].as_array().unwrap().len(), 200);
    assert_eq!(first["nextCursor"], 200);
    let second = timeline
        .search(&json!({"query":"target","cursor":200}))
        .unwrap();
    assert_eq!(
        second["locations"],
        json!([{"seq":201,"role":"user","count":1},{"seq":202,"role":"user","count":1}])
    );
    assert!(second["nextCursor"].is_null());
}

#[test]
fn provider_message_identity_is_durable_without_new_sequence() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = Timeline::new();
    timeline.append_persisted(&path,"agent",json!({"type":"user_message","text":"hello","clientMessageId":"client","messageId":"client"}),&options()).unwrap();
    let epoch = timeline.epoch().to_owned();
    let next = timeline.next_seq();
    timeline
        .enrich_submitted_persisted(&path, "agent", "client", "native-message")
        .unwrap();
    let recovered = Timeline::load(&path, "agent").unwrap();
    assert_eq!(recovered.epoch(), epoch);
    assert_eq!(recovered.next_seq(), next);
    assert_eq!(
        recovered.submitted_user_message("client").unwrap()["providerMessageId"],
        "native-message"
    );
    assert_eq!(
        recovered.submitted_user_message("client").unwrap()["item"]["messageId"],
        "client"
    );
    let invalid = directory.path().join("absent").join("checkpoint");
    std::fs::write(directory.path().join("absent"), b"not a directory").unwrap();
    assert!(
        timeline
            .enrich_submitted_persisted(&invalid, "agent", "client", "uncommitted")
            .is_err()
    );
    assert_eq!(
        timeline.submitted_user_message("client").unwrap()["providerMessageId"],
        "native-message"
    );
}

#[test]
fn absent_database_is_empty_and_ignores_retired_json_checkpoints() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let retired = path.with_extension("json");
    let bytes = br#"{"version":1,"agentId":"agent","epoch":"saved","nextSeq":8,"rows":[]}"#;
    std::fs::write(&retired, bytes).unwrap();
    let loaded = Timeline::load(&path, "agent").unwrap();
    assert_eq!(loaded.next_seq(), Timeline::new().next_seq());
    assert_ne!(loaded.epoch(), "saved");
    assert!(!path.exists());
    assert_eq!(std::fs::read(&retired).unwrap(), bytes);
}

#[test]
fn incremental_commits_write_only_changed_projection_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = interleaved();
    timeline.persist(&path, "agent").unwrap();
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute_batch(
            "CREATE TABLE row_writes(position INTEGER);
         CREATE TRIGGER record_row_insert AFTER INSERT ON timeline_rows
         BEGIN INSERT INTO row_writes VALUES (new.position); END;
         CREATE TRIGGER record_row_update AFTER UPDATE ON timeline_rows
         BEGIN INSERT INTO row_writes VALUES (new.position); END;",
        )
        .unwrap();
    timeline
        .append_persisted(&path, "agent", tool("failed"), &options())
        .unwrap();
    timeline
        .append_persisted(
            &path,
            "agent",
            json!({"type":"assistant_message","text":"a"}),
            &options(),
        )
        .unwrap();
    timeline
        .append_persisted(
            &path,
            "agent",
            json!({"type":"assistant_message","text":"b"}),
            &options(),
        )
        .unwrap();
    timeline
        .append_persisted(
            &path,
            "agent",
            json!({"type":"user_message","text":"hello","clientMessageId":"client"}),
            &options(),
        )
        .unwrap();
    let next_seq = timeline.next_seq();
    timeline
        .enrich_submitted_persisted(&path, "agent", "client", "provider")
        .unwrap();
    assert_eq!(timeline.next_seq(), next_seq);
    let mut statement = database
        .prepare("SELECT position FROM row_writes ORDER BY rowid")
        .unwrap();
    let positions: Vec<i64> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(positions, vec![0, 2, 2, 3, 3]);
    assert_eq!(timeline.items()[2]["text"], "ab");
    let expected = timeline.fetch(&json!({"limit":0}));
    assert_saved_pages(&path, &timeline);
    drop(timeline);
    assert_eq!(
        Timeline::load(&path, "agent")
            .unwrap()
            .fetch(&json!({"limit":0})),
        expected
    );
}

#[test]
fn failed_sqlite_commit_rolls_back_rows_metadata_and_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = interleaved();
    timeline.append(
        json!({"type":"user_message","text":"hello","clientMessageId":"client"}),
        &options(),
    );
    timeline.append(
        json!({"type":"assistant_message","text":"start"}),
        &options(),
    );
    timeline.persist(&path, "agent").unwrap();
    let before = timeline.fetch(&json!({"limit":0}));
    let database = rusqlite::Connection::open(&path).unwrap();
    // The row statement has already succeeded when this trigger aborts metadata.
    database
        .execute_batch(
            "CREATE TRIGGER fail_metadata BEFORE UPDATE ON timeline_metadata
         BEGIN SELECT RAISE(ABORT, 'injected commit failure'); END;",
        )
        .unwrap();
    for item in [
        json!({"type":"assistant_message","text":"uncommitted"}),
        tool("failed"),
        json!({"type":"user_message","text":"uncommitted","clientMessageId":"new-client"}),
    ] {
        assert!(
            timeline
                .append_persisted(&path, "agent", item, &options())
                .is_err()
        );
        assert_eq!(timeline.fetch(&json!({"limit":0})), before);
        assert_eq!(Timeline::fetch_saved(&path, "agent", &json!({"limit":0})).unwrap(), before);
        assert_eq!(
            Timeline::load(&path, "agent")
                .unwrap()
                .fetch(&json!({"limit":0})),
            before
        );
    }
    assert!(timeline.submitted_user_message("new-client").is_none());
    assert!(
        timeline
            .enrich_submitted_persisted(&path, "agent", "client", "uncommitted")
            .is_err()
    );
    assert_eq!(timeline.fetch(&json!({"limit":0})), before);
    assert_eq!(Timeline::fetch_saved(&path, "agent", &json!({"limit":0})).unwrap(), before);
    database
        .execute_batch("DROP TRIGGER fail_metadata")
        .unwrap();
    timeline
        .append_persisted(&path, "agent", tool("completed"), &options())
        .unwrap();
    timeline
        .enrich_submitted_persisted(&path, "agent", "client", "committed")
        .unwrap();
    let expected = timeline.fetch(&json!({"limit":0}));
    assert_saved_pages(&path, &timeline);
    drop(timeline);
    let recovered = Timeline::load(&path, "agent").unwrap();
    assert_eq!(recovered.fetch(&json!({"limit":0})), expected);
    assert_eq!(
        recovered.submitted_user_message("client").unwrap()["providerMessageId"],
        "committed"
    );
}

#[test]
fn full_history_replacement_is_atomic_with_an_existing_connection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut current = interleaved();
    current.persist(&path, "agent").unwrap();
    let before = current.fetch(&json!({"limit":0}));
    let old_epoch = current.epoch().to_owned();
    let mut replacement = Timeline::new();
    replacement.append(
        json!({"type":"user_message","text":"full native history","clientMessageId":"native"}),
        &options(),
    );
    replacement.append(
        json!({"type":"assistant_message","text":"restored response"}),
        &options(),
    );
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER fail_replacement BEFORE UPDATE ON timeline_metadata
         BEGIN SELECT RAISE(ABORT, 'injected replacement failure'); END;",
        )
        .unwrap();
    assert!(replacement.persist(&path, "agent").is_err());
    assert_eq!(current.fetch(&json!({"limit":0})), before);
    assert_eq!(
        Timeline::load(&path, "agent")
            .unwrap()
            .fetch(&json!({"limit":0})),
        before
    );
    assert_eq!(Timeline::fetch_saved(&path, "agent", &json!({"limit":0})).unwrap(), before);
    database
        .execute_batch("DROP TRIGGER fail_replacement")
        .unwrap();
    replacement.persist(&path, "agent").unwrap();
    current = replacement;
    assert_ne!(current.epoch(), old_epoch);
    current
        .append_persisted(
            &path,
            "agent",
            json!({"type":"assistant_message","text":" continued"}),
            &options(),
        )
        .unwrap();
    current
        .enrich_submitted_persisted(&path, "agent", "native", "provider")
        .unwrap();
    let expected = current.fetch(&json!({"limit":0}));
    assert_saved_pages(&path, &current);
    drop(current);
    let recovered = Timeline::load(&path, "agent").unwrap();
    assert_eq!(recovered.fetch(&json!({"limit":0})), expected);
    assert_eq!(recovered.rows().len(), 2);
    assert_eq!(recovered.items()[1]["text"], "restored response continued");
    assert_eq!(
        recovered.fetch(&json!({"cursor":{"epoch":old_epoch,"seq":1}}))["reset"],
        true
    );
}

#[cfg(unix)]
#[test]
fn sqlite_checkpoint_and_live_sidecars_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = Timeline::new();
    timeline
        .append_persisted(
            &path,
            "agent",
            json!({"type":"user_message","text":"private"}),
            &options(),
        )
        .unwrap();
    for name in [
        "timeline.sqlite3",
        "timeline.sqlite3-wal",
        "timeline.sqlite3-shm",
    ] {
        let mode = std::fs::metadata(directory.path().join(name))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{name}");
    }
}

#[test]
fn failed_empty_replacement_retries_before_incremental_append() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut original = interleaved();
    original.persist(&path, "agent").unwrap();
    let database = rusqlite::Connection::open(&path).unwrap();
    database
        .execute_batch(
            "CREATE TRIGGER fail_replacement BEFORE UPDATE ON timeline_metadata
         BEGIN SELECT RAISE(ABORT, 'injected empty replacement failure'); END;",
        )
        .unwrap();
    let mut replacement = Timeline::new();
    assert!(replacement.persist(&path, "agent").is_err());
    assert_eq!(
        Timeline::load(&path, "agent").unwrap().rows(),
        original.rows()
    );
    database
        .execute_batch("DROP TRIGGER fail_replacement")
        .unwrap();
    replacement
        .append_persisted(
            &path,
            "agent",
            json!({"type":"user_message","text":"new history"}),
            &options(),
        )
        .unwrap();
    replacement.append(
        json!({"type":"assistant_message","text":"buffered"}),
        &options(),
    );
    replacement
        .append_persisted(
            &path,
            "agent",
            json!({"type":"assistant_message","text":" committed"}),
            &options(),
        )
        .unwrap();
    let recovered = Timeline::load(&path, "agent").unwrap();
    assert_eq!(
        recovered.fetch(&json!({"limit":0})),
        replacement.fetch(&json!({"limit":0}))
    );
    assert_eq!(recovered.rows().len(), 2);
    assert_eq!(recovered.items()[1]["text"], "buffered committed");
}

fn assert_saved_pages(path: &Path, timeline: &Timeline) {
    for direction in ["tail", "before", "after"] {
        for limit in [0, 1, 2, 200] {
            let options = json!({"direction":direction,"limit":limit});
            assert_eq!(Timeline::fetch_saved(path, "agent", &options).unwrap(), timeline.fetch(&options), "{options}");
            for epoch in [timeline.epoch(), "stale"] {
                for seq in [-1, 0, 1, 2, 3, timeline.next_seq() - 1, timeline.next_seq(), timeline.next_seq() + 1] {
                    let options = json!({"direction":direction,"limit":limit,"cursor":{"epoch":epoch,"seq":seq}});
                    assert_eq!(Timeline::fetch_saved(path, "agent", &options).unwrap(), timeline.fetch(&options), "{options}");
                }
            }
        }
    }
}

fn legacy_timeline_database(path: &Path, timeline: &Timeline) {
    let mut database = rusqlite::Connection::open(path).unwrap();
    let transaction = database.transaction().unwrap();
    transaction.execute_batch(
        "CREATE TABLE timeline_metadata (
            id INTEGER PRIMARY KEY CHECK(id = 1),
            version INTEGER NOT NULL,
            agent_id TEXT NOT NULL,
            epoch TEXT NOT NULL,
            next_seq INTEGER NOT NULL CHECK(next_seq > 0),
            min_seq INTEGER NOT NULL CHECK(min_seq >= 0)
        );
        CREATE TABLE timeline_rows (
            position INTEGER PRIMARY KEY CHECK(position >= 0),
            row_json TEXT NOT NULL
        );",
    ).unwrap();
    transaction.execute(
        "INSERT INTO timeline_metadata VALUES (1, 1, 'agent', ?1, ?2, ?3)",
        rusqlite::params![timeline.epoch, timeline.next_seq, timeline.min_seq],
    ).unwrap();
    for (position, row) in timeline.rows.iter().enumerate() {
        transaction.execute(
            "INSERT INTO timeline_rows(position, row_json) VALUES (?1, ?2)",
            rusqlite::params![position as i64, row.to_string()],
        ).unwrap();
    }
    transaction.commit().unwrap();
}

#[test]
fn saved_page_index_migrates_v1_without_changing_read_only_legacy_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let timeline = interleaved();
    legacy_timeline_database(&path, &timeline);
    let before = std::fs::read(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    }
    assert_saved_pages(&path, &timeline);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let loaded = Timeline::load(&path, "agent").unwrap();
    assert_eq!(loaded.rows(), timeline.rows());
    assert_saved_pages(&path, &timeline);
    let database = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(database.query_row("SELECT version FROM timeline_metadata", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(database.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'index' AND name = 'timeline_rows_page'",
        [], |row| row.get::<_, i64>(0),
    ).unwrap(), 1);
}

#[test]
fn saved_page_index_preserves_overlapping_ranges_retention_and_empty_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = Timeline::seed(&json!({
        "epoch":"ranges",
        "rows":[
            {"seq":9,"seqStart":4,"seqEnd":9,"sourceSeqRanges":[{"startSeq":4,"endSeq":5},{"startSeq":8,"endSeq":9}],
             "timestamp":"time","item":{"type":"user_message","text":"first"}},
            {"seq":7,"seqStart":5,"seqEnd":7,"sourceSeqRanges":[{"startSeq":5,"endSeq":7}],
             "timestamp":"time","item":{"type":"user_message","text":"overlap"}},
            {"seq":10,"seqStart":10,"seqEnd":10,"sourceSeqRanges":[{"startSeq":10,"endSeq":10}],
             "timestamp":"time","item":{"type":"user_message","text":"last"}}
        ]
    })).unwrap();
    timeline.persist(&path, "agent").unwrap();
    assert_saved_pages(&path, &timeline);
    let mut empty = Timeline::new();
    empty.persist(&path, "agent").unwrap();
    assert_saved_pages(&path, &empty);
}

#[test]
fn saved_page_index_tracks_legacy_v1_writers_and_transaction_rollback() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let mut timeline = interleaved();
    timeline.persist(&path, "agent").unwrap();
    let mut database = rusqlite::Connection::open(&path).unwrap();
    timeline.append(tool("failed"), &options());
    timeline.append(json!({"type":"user_message","text":"legacy insert"}), &options());
    {
        let transaction = database.transaction().unwrap();
        // Exact old-writer SQL: no explicit metadata-index writes.
        for (position, row) in timeline.rows.iter().enumerate() {
            transaction.execute(
                "INSERT INTO timeline_rows(position, row_json) VALUES (?1, ?2) \
                 ON CONFLICT(position) DO UPDATE SET row_json = excluded.row_json",
                rusqlite::params![position as i64, row.to_string()],
            ).unwrap();
        }
        transaction.execute(
            "UPDATE timeline_metadata SET epoch = ?1, next_seq = ?2, min_seq = ?3 WHERE id = 1",
            rusqlite::params![timeline.epoch, timeline.next_seq, timeline.min_seq],
        ).unwrap();
        transaction.commit().unwrap();
    }
    assert_saved_pages(&path, &timeline);
    {
        let transaction = database.transaction().unwrap();
        transaction.execute("DELETE FROM timeline_rows", []).unwrap();
        transaction.rollback().unwrap();
    }
    assert_saved_pages(&path, &timeline);
    let empty = Timeline::new();
    {
        let transaction = database.transaction().unwrap();
        transaction.execute("DELETE FROM timeline_rows", []).unwrap();
        transaction.execute(
            "UPDATE timeline_metadata SET epoch = ?1, next_seq = ?2, min_seq = ?3 WHERE id = 1",
            rusqlite::params![empty.epoch, empty.next_seq, empty.min_seq],
        ).unwrap();
        transaction.commit().unwrap();
    }
    assert_saved_pages(&path, &empty);
}

#[test]
fn saved_page_index_rejects_corrupt_metadata_and_rolls_back_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("timeline.sqlite3");
    let timeline = interleaved();
    legacy_timeline_database(&path, &timeline);
    let database = rusqlite::Connection::open(&path).unwrap();
    let original = timeline.rows[0].clone();
    for (field, invalid) in [
        ("seqStart", json!(null)),
        ("seqStart", json!("1")),
        ("seqEnd", json!(0)),
        ("sourceSeqRanges", json!({})),
        ("sourceSeqRanges", json!([])),
        ("sourceSeqRanges", json!([{"startSeq":1,"endSeq":"3"}])),
        ("sourceSeqRanges", json!([{"startSeq":3,"endSeq":1}])),
    ] {
        let mut corrupt = original.clone();
        corrupt[field] = invalid;
        database.execute("UPDATE timeline_rows SET row_json = ?1 WHERE position = 0", [corrupt.to_string()]).unwrap();
        assert!(Timeline::fetch_saved(&path, "agent", &json!({"direction":"after","cursor":{"epoch":"epoch","seq":3}})).is_err());
        assert!(Timeline::load(&path, "agent").is_err());
        assert_eq!(database.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'timeline_rows_page'",
            [], |row| row.get::<_, i64>(0),
        ).unwrap(), 0);
    }
    database.execute("UPDATE timeline_rows SET row_json = '{' WHERE position = 0", []).unwrap();
    assert!(Timeline::fetch_saved(&path, "agent", &json!({"limit":1})).is_err());
    assert!(Timeline::load(&path, "agent").is_err());
    assert_eq!(database.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name = 'timeline_rows_page'",
        [], |row| row.get::<_, i64>(0),
    ).unwrap(), 0);
    database.execute("UPDATE timeline_rows SET row_json = ?1 WHERE position = 0", [original.to_string()]).unwrap();
    drop(Timeline::load(&path, "agent").unwrap());
    // Even an unselected corrupt row must not silently change page selection.
    database.execute(
        "UPDATE timeline_rows SET row_json = json_set(row_json, '$.sourceSeqRanges', json('[{}]')) WHERE position = 0",
        [],
    ).unwrap();
    assert!(Timeline::fetch_saved(&path, "agent", &json!({"direction":"after","cursor":{"epoch":"epoch","seq":3}})).is_err());
    assert!(Timeline::load(&path, "agent").is_err());
    database.execute_batch("DROP INDEX timeline_rows_page; CREATE INDEX timeline_rows_page ON timeline_rows(position);").unwrap();
    assert!(Timeline::fetch_saved(&path, "agent", &json!({"limit":1})).is_err());
    assert!(Timeline::load(&path, "agent").is_err());
}

#[test]
fn history_recovery_preserves_notices_without_guessing_user_message_identity() {
    use crate::providers::api::HistoryCoverage;
    let mut previous = Timeline::new();
    previous.append(
        json!({"type":"notification","level":"info","message":"daemon notice"}),
        &options(),
    );
    previous.append(json!({"type":"user_message","text":"same text","messageId":"client-id","clientMessageId":"client-id"}), &options());
    previous
        .enrich_submitted_user_message("client-id", "native-id")
        .unwrap();
    let before = previous.fetch(&json!({"limit":0}));
    let mut uncorrelated = Timeline::new();
    uncorrelated.append(
        json!({"type":"user_message","text":"same text","messageId":"different-id"}),
        &options(),
    );
    assert!(!uncorrelated.reconcile_history(&previous, HistoryCoverage::Complete, false));
    let mut replacement = Timeline::new();
    replacement.append(
        json!({"type":"user_message","text":"older","messageId":"older-id"}),
        &options(),
    );
    replacement.append(
        json!({"type":"user_message","text":"same text","messageId":"native-id"}),
        &options(),
    );
    assert!(!replacement.reconcile_history(&previous, HistoryCoverage::Unverified, false));
    assert_eq!(previous.fetch(&json!({"limit":0})), before);
    assert!(replacement.reconcile_history(&previous, HistoryCoverage::Complete, false));
    let rows = replacement.rows();
    assert_eq!(rows[0]["item"]["messageId"], "older-id");
    assert_eq!(rows[1]["item"], previous.rows()[0]["item"]);
    assert_eq!(rows[2]["item"]["messageId"], "client-id");
    assert_eq!(rows[2]["providerMessageId"], "native-id");
    assert!(
        rows.windows(2)
            .all(|pair| pair[0]["seqEnd"].as_i64() < pair[1]["seqStart"].as_i64())
    );
    let last_seq = rows.last().unwrap()["seqEnd"].as_i64().unwrap();
    let appended = replacement.append(
        json!({"type":"assistant_message","text":"next"}),
        &options(),
    );
    assert_eq!(appended["seq"].as_i64(), Some(last_seq + 1));
}
