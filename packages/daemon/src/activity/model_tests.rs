use super::*;
use crate::agent::timeline::Timeline;
use std::collections::BTreeSet;

fn snapshot() -> Value {
    json!({"id":"agent","status":"running","pendingPermissions":[],"lastError":null})
}

fn read(timeline: &Timeline, snapshot: &Value, input: &Value) -> Value {
    let options = timeline_options(input).unwrap();
    activity(&timeline.fetch(&options), snapshot, input).unwrap()
}

fn item_reference(page: &Value, entry: &Value) -> Value {
    let mut input = json!({"agentId":page["agentId"],
        "detail":{"epoch":page["coverage"]["epoch"],
            "seqStart":entry["seqStart"],"seqEnd":entry["seqEnd"]}});
    if let Some(server) = page.get("serverId") {
        input["serverId"] = server.clone();
    }
    input
}

fn recover_item(timeline: &Timeline, mut input: Value) -> Value {
    let mut encoded = String::new();
    loop {
        let page = read(timeline, &snapshot(), &input);
        let part = page["chunk"].as_str().unwrap();
        assert!(part.len() <= CHUNK_BYTES);
        assert_eq!(page["offset"], encoded.len());
        encoded.push_str(part);
        if page["nextRead"].is_null() {
            assert_eq!(page["totalBytes"], encoded.len());
            break;
        }
        assert!(!part.is_empty());
        input = page["nextRead"].clone();
    }
    serde_json::from_str(&encoded).unwrap()
}

fn recover_snapshot(snapshot: &Value, mut input: Value) -> Value {
    let mut encoded = String::new();
    loop {
        let page = status(snapshot, &input).unwrap();
        let part = page["chunk"].as_str().unwrap();
        assert!(part.len() <= CHUNK_BYTES);
        assert_eq!(page["offset"], encoded.len());
        encoded.push_str(part);
        if page["nextRead"].is_null() {
            assert_eq!(page["totalBytes"], encoded.len());
            break;
        }
        assert!(!part.is_empty());
        input = page["nextRead"].clone();
    }
    serde_json::from_str(&encoded).unwrap()
}

#[test]
fn defaults_bound_large_activity_status_and_permissions_without_reasoning() {
    let long = "large\n\"界".repeat(2000);
    let mut timeline = Timeline::new();
    for i in 0..30 {
        timeline.append(
            json!({"type":"user_message","text":long,"opaque":long,"index":i}),
            &json!({}),
        );
    }
    timeline.append(
        json!({"type":"reasoning","text":"private reasoning body"}),
        &json!({}),
    );
    let permissions: Vec<_> = (0..100)
        .map(|i| json!({"id":i.to_string(),"input":long}))
        .collect();
    let snapshot = json!({"status":"error","lastError":long,"title":long,
        "pendingPermissions":permissions,"capabilities":{"large":long}});
    let result = read(&timeline, &snapshot, &json!({"agentId":"agent"}));
    let encoded = result.to_string();
    assert!(encoded.len() < 16_000);
    assert_eq!(result["entries"].as_array().unwrap().len(), DEFAULT_LIMIT);
    assert_eq!(result["coverage"]["hasOlder"], true);
    assert_eq!(result["agentStatus"]["status"], "error");
    assert_eq!(result["agentStatus"]["lastError"]["truncated"], true);
    assert_eq!(result["agentStatus"]["pendingPermissions"]["count"], 100);
    assert_eq!(
        result["agentStatus"]["pendingPermissions"]["shown"]
            .as_array()
            .unwrap()
            .len(),
        PERMISSIONS_SHOWN
    );
    assert!(!encoded.contains("private reasoning body"));
    assert!(result["agentStatus"].get("capabilities").is_none());
    let reasoning = result["entries"].as_array().unwrap().last().unwrap();
    assert_eq!(
        recover_item(&timeline, item_reference(&result, reasoning))["text"],
        "private reasoning body"
    );
    // The raw UI/SDK projection retains the full source text.
    assert_eq!(timeline.items()[0]["text"], long);
}

#[test]
fn failure_pages_discover_old_errors_and_recover_every_item_byte() {
    let failure = json!({"type":"tool_call","callId":"old-failure","name":"external",
        "status":"failed","errorText":"raw error 界\n".repeat(1500),
        "detail":{"type":"unknown","input":{"command":"run"},"output":"result🙂".repeat(5000)},
        "opaque":{"retained":"extra\u{0000}metadata".repeat(1000)}});
    let mut timeline = Timeline::new();
    timeline.append(failure.clone(), &json!({}));
    for i in 0..25 {
        timeline.append(
            json!({"type":"user_message","text":format!("later {i}")}),
            &json!({}),
        );
    }
    let latest = read(&timeline, &snapshot(), &json!({"agentId":"agent"}));
    assert_eq!(latest["coverage"]["hasOlder"], true);
    let mut input = latest["failuresRead"].clone();
    let mut found = Vec::new();
    let mut empty_pages = 0;
    loop {
        let page = read(&timeline, &snapshot(), &input);
        let entries = page["entries"].as_array().unwrap();
        empty_pages += usize::from(entries.is_empty());
        found.extend(entries.iter().map(|entry| (entry.clone(), item_reference(&page, entry))));
        if page["nextCursor"].is_null() {
            break;
        }
        input["cursor"] = page["nextCursor"].clone();
    }
    assert!(empty_pages > 0);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0["seqStart"], 1);
    assert_eq!(found[0].0["status"], "failed");
    assert_eq!(found[0].0["errorText"]["truncated"], true);
    assert_eq!(recover_item(&timeline, found[0].1.clone()), failure);
}

#[test]
fn crossing_tool_lifecycle_never_expands_or_duplicates_backward_pages() {
    let mut timeline = Timeline::new();
    timeline.append(
        json!({"type":"tool_call","callId":"span","name":"read","status":"running"}),
        &json!({}),
    );
    for i in 0..30 {
        timeline.append(
            json!({"type":"user_message","text":format!("message {i}")}),
            &json!({}),
        );
    }
    timeline.append(
        json!({"type":"tool_call","callId":"span","name":"read","status":"failed",
        "error":{"content":"cross-boundary failure"}}),
        &json!({}),
    );
    let mut input = json!({"agentId":"agent","limit":2});
    let mut seen = BTreeSet::new();
    let mut failure = None;
    loop {
        let page = read(&timeline, &snapshot(), &input);
        assert!(page["entries"].as_array().unwrap().len() <= 2);
        for row in page["entries"].as_array().unwrap() {
            assert!(seen.insert(row["seqStart"].as_i64().unwrap()));
            if row["seqStart"] == 1 {
                failure = Some((row.clone(), item_reference(&page, row)));
            }
        }
        if page["nextCursor"].is_null() {
            break;
        }
        input["cursor"] = page["nextCursor"].clone();
    }
    assert_eq!(seen.len(), 31);
    let (failure, reference) = failure.unwrap();
    assert_eq!(failure["seqEnd"], 32);
    assert_eq!(failure["errorText"], "cross-boundary failure");
    assert_eq!(
        recover_item(&timeline, reference)["status"],
        "failed"
    );
}

#[test]
fn history_mutations_reject_cursor_and_detail_instead_of_drifting() {
    let mut timeline = Timeline::new();
    timeline.append(
        json!({"type":"tool_call","callId":"span","status":"running"}),
        &json!({}),
    );
    timeline.append(json!({"type":"user_message","text":"newer"}), &json!({}));
    let first = read(
        &timeline,
        &snapshot(),
        &json!({"agentId":"agent","limit":1}),
    );
    let cursor = json!({"agentId":"agent","cursor":first["nextCursor"],"limit":1});
    let older = read(&timeline, &snapshot(), &cursor);
    let detail = item_reference(&older, &older["entries"][0]);
    timeline.append(
        json!({"type":"tool_call","callId":"span","status":"failed","error":"failed"}),
        &json!({}),
    );
    for input in [&cursor, &detail] {
        let page = timeline.fetch(&timeline_options(input).unwrap());
        assert!(activity(&page, &snapshot(), input).is_err());
    }
    let restarted = read(
        &timeline,
        &snapshot(),
        &json!({"agentId":"agent","limit":1}),
    );
    let cursor = json!({"agentId":"agent","cursor":restarted["nextCursor"],"limit":1});
    timeline
        .reset(&json!({"items":[{"type":"user_message","text":"replacement"}]}))
        .unwrap();
    let page = timeline.fetch(&timeline_options(&cursor).unwrap());
    assert!(activity(&page, &snapshot(), &cursor).is_err());
}

#[test]
fn unrelated_tail_growth_preserves_older_pages_and_exact_item_continuations() {
    let mut timeline = Timeline::new();
    for i in 0..12 {
        timeline.append(
            json!({"type":"user_message","text":format!("{i}: {}", "界".repeat(3000))}),
            &json!({}),
        );
    }
    let first = read(
        &timeline,
        &snapshot(),
        &json!({"agentId":"agent","limit":2}),
    );
    let detail = item_reference(&first, &first["entries"][0]);
    let first_chunk = read(&timeline, &snapshot(), &detail);
    let mut encoded = first_chunk["chunk"].as_str().unwrap().to_owned();
    let mut reference = first_chunk["nextRead"].clone();
    let mut cursor = first["nextCursor"].clone();
    let initial_max = first["coverage"]["maxSeq"].clone();
    let mut seen: BTreeSet<_> = first["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["seqStart"].as_i64().unwrap())
        .collect();
    while !cursor.is_null() {
        timeline.append(
            json!({"type":"user_message","text":"unrelated new tail"}),
            &json!({}),
        );
        let page = read(
            &timeline,
            &snapshot(),
            &json!({"agentId":"agent","limit":2,"cursor":cursor}),
        );
        assert_eq!(page["coverage"]["maxSeq"], initial_max);
        for row in page["entries"].as_array().unwrap() {
            assert!(seen.insert(row["seqStart"].as_i64().unwrap()));
        }
        cursor = page["nextCursor"].clone();
    }
    assert_eq!(seen, (1..=12).collect());
    while !reference.is_null() {
        timeline.append(
            json!({"type":"user_message","text":"another unrelated tail"}),
            &json!({}),
        );
        let page = read(&timeline, &snapshot(), &reference);
        encoded.push_str(page["chunk"].as_str().unwrap());
        reference = page["nextRead"].clone();
    }
    let recovered: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(recovered, timeline.items()[10]);
}

#[test]
fn status_recovers_long_errors_all_permissions_and_complete_snapshot() {
    let permissions: Vec<_> = (0..7)
        .map(|i| {
            json!({"id":format!("p{i}"),
        "prompt":"permission🙂".repeat(600),"options":["allow","deny"]})
        })
        .collect();
    let snapshot = json!({"status":"running","attention":{"requiresAttention":true,"attentionReason":"permission"},
        "pendingPermissions":permissions,"lastError":"previous failure 界".repeat(500),
        "preparation":{"status":"failed","error":"provider failure".repeat(500)},
        "snapshotAvailability":"last_persisted","capabilities":{"all":"preserved"}});
    let compact = status(&snapshot, &json!({"agentId":"agent"})).unwrap();
    assert_eq!(compact["pendingPermissions"]["count"], 7);
    assert_eq!(compact["pendingPermissions"]["omitted"], 4);
    assert_eq!(compact["requiresAttention"], true);
    assert_eq!(compact["attentionReason"], "permission");
    assert_eq!(compact["preparation"]["truncated"], true);
    assert_eq!(compact["snapshotAvailability"], "last_persisted");
    let root = compact["detail"].clone();
    assert_eq!(recover_snapshot(&snapshot, root.clone()), snapshot);
    for path in [
        "/pendingPermissions",
        "/pendingPermissions/6",
        "/lastError",
        "/preparation/error",
    ] {
        let mut reference = root.clone();
        reference["detail"]["path"] = json!(path);
        assert_eq!(
            recover_snapshot(&snapshot, reference),
            *snapshot.pointer(path).unwrap()
        );
    }
    let mut changed = snapshot.clone();
    changed["pendingPermissions"] = json!([]);
    let continuation = status(&snapshot, &root).unwrap()["nextRead"].clone();
    assert!(!continuation.is_null());
    assert!(status(&changed, &continuation).is_err());
}

#[test]
fn permission_detail_survives_unrelated_status_updates_but_not_permission_changes() {
    let permission = json!({"id":"p","prompt":"permission 界".repeat(2000)});
    let mut snapshot =
        json!({"status":"running","title":"before","pendingPermissions":[permission]});
    let compact = status(&snapshot, &json!({"agentId":"agent"})).unwrap();
    let summary = &compact["pendingPermissions"]["shown"][0]["request"];
    let input =
        json!({"agentId":"agent","detail":{"path":summary["path"],"revision":summary["revision"]}});
    let first = status(&snapshot, &input).unwrap();
    let next = first["nextRead"].clone();
    let mut reference = next.clone();
    let mut encoded = first["chunk"].as_str().unwrap().to_owned();
    while !reference.is_null() {
        snapshot["title"] = json!(format!("changed {}", encoded.len()));
        let page = status(&snapshot, &reference).unwrap();
        encoded.push_str(page["chunk"].as_str().unwrap());
        reference = page["nextRead"].clone();
    }
    assert_eq!(serde_json::from_str::<Value>(&encoded).unwrap(), permission);
    snapshot["pendingPermissions"][0]["prompt"] = json!("different permission".repeat(2000));
    assert!(status(&snapshot, &next).is_err());
}

#[test]
fn local_and_remote_pages_share_bounded_contract_and_routed_recovery() {
    let timeline = Timeline::seed(&json!({"epoch":"same","items":[
        {"type":"user_message","text":"old"},
        {"type":"error","message":"remote error","extra":"界".repeat(3000)}
    ]}))
    .unwrap();
    let input = json!({"agentId":"agent","serverId":"remote-host","limit":1});
    let local_page = timeline.fetch(&timeline_options(&input).unwrap());
    let mut remote_page = local_page.clone();
    remote_page["entries"] = remote_page.as_object_mut().unwrap().remove("rows").unwrap();
    let local = activity(&local_page, &snapshot(), &input).unwrap();
    let remote = activity(&remote_page, &snapshot(), &input).unwrap();
    assert_eq!(local, remote);
    let reference = item_reference(&remote, &remote["entries"][0]);
    assert_eq!(reference["serverId"], "remote-host");
    let detail_page = read(&timeline, &snapshot(), &reference);
    assert_eq!(detail_page["nextRead"]["serverId"], "remote-host");
    assert_eq!(
        recover_item(&timeline, reference)["message"],
        "remote error"
    );
    let status_ref = remote["agentStatus"]["detail"].clone();
    assert_eq!(status_ref["serverId"], "remote-host");
    assert_eq!(recover_snapshot(&snapshot(), status_ref), snapshot());
}

#[test]
fn status_normalizes_internal_and_wire_lifecycle_and_attention() {
    let internal = json!({"status":"idle","lifecycle":"running",
        "config":{"title":"Agent"},"attention":{"requiresAttention":true,"attentionReason":"permission"},
        "pendingPermissions":[{"id":"p","prompt":"approve"}]});
    let wire = json!({"status":"running","title":"Agent","requiresAttention":true,
        "attentionReason":"permission","pendingPermissions":[{"id":"p","prompt":"approve"}]});
    let input = json!({"agentId":"agent"});
    let mut local = status(&internal, &input).unwrap();
    let mut remote = status(&wire, &input).unwrap();
    // Each transport retains its original full authorized snapshot for detail.
    local.as_object_mut().unwrap().remove("detail");
    remote.as_object_mut().unwrap().remove("detail");
    assert_eq!(local, remote);
}
#[test]
fn json_chunks_preserve_unicode_and_reject_invalid_offsets_and_limits() {
    let value = json!({"text":"🙂界\n\u{0000}\\\"".repeat(2000)});
    let snapshot = json!({"pendingPermissions":[],"custom":value});
    let compact = status(&snapshot, &json!({"agentId":"agent"})).unwrap();
    assert_eq!(
        recover_snapshot(&snapshot, compact["detail"].clone()),
        snapshot
    );
    let text = json!("🙂");
    assert!(serialize(&text, 2, CHUNK_BYTES, false).is_err());
    assert!(serialize(&text, 100, CHUNK_BYTES, false).is_err());
    for limit in [json!(0), json!(-1), json!(1.5), json!(21), Value::Null] {
        assert!(timeline_options(&json!({"agentId":"agent","limit":limit})).is_err());
    }
    let input = json!({"agentId":"agent","limit":1});
    let page =
        json!({"epoch":"e","window":{"maxSeq":2,"minSeq":1},"hasOlder":false,"rows":[{},{}]});
    assert!(activity(&page, &snapshot, &input).is_err());
}
