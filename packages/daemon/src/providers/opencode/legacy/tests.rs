use super::super::reader::FakeNative;
use super::*;

fn session(id: &str, parent: &str) -> Value {
    json!({"id":id,"parentID":parent,"directory":"/work","title":format!("{id} task")})
}
fn user(session: &str, id: &str, text: &str) -> Value {
    json!({"info":{"id":id,"sessionID":session,"role":"user","time":{"created":1_700_000_000_000i64}},
        "parts":[{"id":format!("{id}p"),"messageID":id,"sessionID":session,"type":"text","text":text}]})
}
fn assistant(session: &str, id: &str, text: &str, completed: bool) -> Value {
    let mut info = json!({"id":id,"sessionID":session,"role":"assistant",
        "time":{"created":1_700_000_001_000i64}});
    if completed {
        info["time"]["completed"] = json!(1_700_000_002_000i64);
    }
    json!({"info":info,"parts":[{"id":format!("{id}p"),"messageID":id,"sessionID":session,
        "type":"text","text":text,"time":{"start":1,"end":2}}]})
}
/// root spawned alpha (running) and beta; alpha spawned grand.
fn family() -> FakeNative {
    FakeNative::default()
        .reply("/session/root/children?directory=/work", json!([session("alpha", "root"), session("beta", "root")]))
        .reply("/session/alpha/children?directory=/work", json!([session("grand", "alpha")]))
        .reply("/session/beta/children?directory=/work", json!([]))
        .reply("/session/grand/children?directory=/work", json!([]))
        .reply("/session/status?directory=/work", json!({"alpha":{"type":"busy"}}))
        .reply("/session/alpha?directory=/work", session("alpha", "root"))
        .reply("/session/beta?directory=/work", session("beta", "root"))
        .reply("/session/grand?directory=/work", session("grand", "alpha"))
        .reply(
            "/session/alpha/message?directory=/work",
            json!([user("alpha", "a1", "Review the parser"), assistant("alpha", "a2", "Parser is fine", true)]),
        )
        .reply(
            "/session/beta/message?directory=/work",
            json!([assistant("beta", "b1", "sibling body", true)]),
        )
        .reply(
            "/session/grand/message?directory=/work",
            json!([assistant("grand", "g1", "grandchild body", true)]),
        )
}
fn rows<'a>(events: &'a [Value], child: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .inspect(|e| assert_eq!(e["event"]["id"], child, "{e}"))
        .map(|e| e["event"]["item"]["text"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn parent_discovery_announces_descendants_without_reading_bodies() {
    let native = family();
    let found = discover(&native, "root", "/work").await.unwrap();
    let reads = native.reads();
    assert!(reads.iter().all(|read| !read.contains("/message")), "{reads:?}");

    let mut projection = Projection::new("root".into(), "/work".into());
    let announced: Vec<Value> = found
        .iter()
        .flat_map(|child| {
            projection.detect(
                &child.info,
                Some(if child.active() { "running" } else { "completed" }),
            )
        })
        .collect();
    assert!(announced.iter().all(|e| e["type"] == "provider_subagent" && e["event"]["type"] == "upsert"), "{announced:?}");
    let ids: Vec<_> = announced.iter().map(|e| e["event"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["alpha", "beta", "grand"]);
    let statuses: Vec<_> = announced.iter().map(|e| e["event"]["status"].as_str().unwrap()).collect();
    assert_eq!(statuses, ["running", "completed", "completed"]);
    assert_eq!(announced[0]["event"]["parentSubagentId"], Value::Null);
    assert_eq!(announced[2]["event"]["parentSubagentId"], "alpha");
    assert_eq!(announced[0]["event"]["source"], json!({"sessionId":"alpha","directory":"/work"}));
}

#[tokio::test]
async fn opened_child_replays_only_its_own_body_and_announces_what_it_spawned() {
    let native = family();
    let (events, coverage) = replay_child(&native, "opencode", "root", "alpha", "/work")
        .await
        .unwrap();
    assert!(coverage == HistoryCoverage::Complete);
    let reads = native.reads();
    assert!(
        !reads.iter().any(|read| read.starts_with("/session/beta") || read.starts_with("/session/grand/message")),
        "{reads:?}"
    );
    assert_eq!(rows(&events, "alpha"), ["Review the parser", "Parser is fine"]);
    assert!(events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .all(|e| e["event"]["historyComplete"] == true && e["event"]["timestamp"].is_string()));
    let spawned: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "upsert").collect();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0]["event"]["id"], "grand");
    assert_eq!(spawned[0]["event"]["parentSubagentId"], "alpha");
    assert_eq!(spawned[0]["event"]["status"], "completed");
    assert_eq!(spawned[0]["event"]["source"], json!({"sessionId":"grand","directory":"/work"}));
}

#[tokio::test]
async fn nested_child_resolves_through_its_ancestry_and_strangers_are_refused() {
    let native = family();
    let (events, _) = replay_child(&native, "opencode", "root", "grand", "/work")
        .await
        .unwrap();
    assert_eq!(rows(&events, "grand"), ["grandchild body"]);

    let native = FakeNative::default()
        .reply("/session/stray?directory=/work", session("stray", "elsewhere"))
        .reply("/session/elsewhere?directory=/work", json!({"id":"elsewhere"}))
        .reply("/session/stray/message?directory=/work", json!([assistant("stray", "s1", "foreign", true)]));
    let error = replay_child(&native, "opencode", "root", "stray", "/work")
        .await
        .err()
        .expect("a session outside the parent family is refused");
    assert!(error.to_string().contains("not a subagent of root"), "{error}");
    assert!(!native.reads().iter().any(|read| read.contains("/message")));
}

#[tokio::test]
async fn unreadable_child_body_is_an_error_not_an_empty_replay() {
    let native = FakeNative::default()
        .reply("/session/beta?directory=/work", session("beta", "root"))
        .reply("/session/beta/children?directory=/work", json!([]));
    let error = replay_child(&native, "opencode", "root", "beta", "/work")
        .await
        .err()
        .expect("a missing body fails");
    assert!(error.to_string().contains("404"), "{error}");

    let missing = FakeNative::default();
    assert!(replay_child(&missing, "opencode", "root", "beta", "/work").await.is_err());
}

#[tokio::test]
async fn running_child_replay_is_unverified_and_keys_the_partial_turn_by_message_id() {
    let native = family().reply(
        "/session/alpha/message?directory=/work",
        json!([user("alpha", "a1", "Review the parser"), assistant("alpha", "a2", "Pars", false)]),
    );
    let (events, coverage) = replay_child(&native, "opencode", "root", "alpha", "/work")
        .await
        .unwrap();
    assert!(coverage == HistoryCoverage::Unverified);
    assert_eq!(rows(&events, "alpha"), ["Review the parser", "Pars"]);
    let partial = events
        .iter()
        .find(|e| e["event"]["item"]["text"] == "Pars")
        .unwrap();
    assert_eq!(partial["event"]["item"]["messageId"], "a2");
    assert!(events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .all(|e| e["event"]["historyComplete"] == false));
}

#[test]
fn live_child_events_stream_after_descriptor_only_discovery() {
    let mut projection = Projection::new("root".into(), "/work".into());
    projection.detect(&session("alpha", "root"), Some("running"));
    projection.detect(&session("beta", "root"), Some("running"));
    let mut live = Vec::new();
    for event in [
        json!({"type":"message.updated","properties":{"info":{"id":"m1","sessionID":"alpha","role":"assistant","time":{"created":1}}}}),
        json!({"type":"message.part.delta","properties":{"sessionID":"alpha","messageID":"m1","partID":"p1","field":"text","delta":"Hel"}}),
        json!({"type":"message.part.updated","properties":{"part":{"id":"p1","messageID":"m1","sessionID":"alpha","type":"text","text":"Hello","time":{"start":1,"end":2}}}}),
        json!({"type":"session.idle","properties":{"sessionID":"alpha"}}),
        json!({"type":"session.error","properties":{"sessionID":"beta","error":{"name":"ProviderError","data":{"message":"boom"}}}}),
    ] {
        live.extend(projection.translate(&event));
    }
    assert_eq!(rows(&live, "alpha"), ["Hel", "lo"]);
    let terminal: Vec<_> = live
        .iter()
        .filter(|e| e["event"]["status"].is_string() && e["event"]["status"] != "running")
        .map(|e| (e["event"]["id"].as_str().unwrap(), e["event"]["status"].as_str().unwrap()))
        .collect();
    assert_eq!(terminal, [("alpha", "completed"), ("beta", "failed")]);
}

fn part(kind: &str, id: &str, text: &str, done: bool) -> Value {
    let mut time = json!({"start":1});
    if done {
        time["end"] = json!(2);
    }
    json!({"id":id,"messageID":"a2","sessionID":"alpha","type":kind,"text":text,"time":time})
}
fn part_event(kind: &str, id: &str, text: &str, done: bool) -> Value {
    json!({"type":"message.part.updated","properties":{"part":part(kind, id, text, done)}})
}
fn delta(id: &str, text: &str) -> Value {
    json!({"type":"message.part.delta","properties":{"sessionID":"alpha","messageID":"a2","partID":id,"field":"text","delta":text}})
}
/// `(type, messageId)` of every child timeline row, in order, with its text.
fn identified(events: &[Value]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .map(|e| {
            let item = &e["event"]["item"];
            let id = item["messageId"].as_str().filter(|id| !id.is_empty());
            (
                item["type"].as_str().unwrap().to_owned(),
                id.expect("every replayed or live row carries native identity").to_owned(),
                item["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn running_child_replay_and_later_live_updates_share_native_identity() {
    let mut projection = Projection::new("root".into(), "/work".into());
    projection.detect(&session("alpha", "root"), Some("running"));
    let mut before = Vec::new();
    for event in [
        json!({"type":"message.updated","properties":{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1}}}}),
        part_event("reasoning", "r1", "", false),
        delta("r1", "Th"),
        part_event("text", "t1", "", false),
        delta("t1", "Pa"),
    ] {
        before.extend(projection.translate(&event));
    }
    let before = identified(&before);

    let running = json!([user("alpha", "a1", "Review the parser"),
        {"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1_700_000_001_000i64}},
         "parts":[part("reasoning", "r1", "Th", false), part("text", "t1", "Pa", false)]}]);
    let native = family().reply("/session/alpha/message?directory=/work", running);
    let (replayed, coverage) = replay_child(&native, "opencode", "root", "alpha", "/work")
        .await
        .unwrap();
    assert!(coverage == HistoryCoverage::Unverified);
    let replayed = identified(&replayed);
    // Each live row resolves to exactly one replay row of its type by native id.
    for (kind, id, _) in &before {
        let matches = replayed.iter().filter(|(k, i, _)| k == kind && i == id).count();
        assert_eq!(matches, 1, "{kind} {id} in {replayed:?}");
    }
    assert_eq!(
        before.iter().map(|(k, i, _)| (k.as_str(), i.as_str())).collect::<Vec<_>>(),
        [("reasoning", "r1"), ("assistant_message", "a2")]
    );

    let mut after = Vec::new();
    for event in [
        delta("r1", "ink"),
        part_event("reasoning", "r1", "Think", true),
        delta("t1", "rsed"),
        part_event("text", "t1", "Parsed", true),
    ] {
        after.extend(projection.translate(&event));
    }
    let after = identified(&after);
    // The replayed snapshot followed by later live rows rebuilds each native part
    // exactly once: nothing duplicated, nothing truncated.
    for (kind, id, full) in [("reasoning", "r1", "Think"), ("assistant_message", "a2", "Parsed")] {
        let text: String = replayed
            .iter()
            .chain(&after)
            .filter(|(k, i, _)| k == kind && i == id)
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, full);
    }
}

#[tokio::test]
async fn live_collection_rebased_on_a_replay_continues_after_the_snapshot() {
    let mut projection = Projection::new("root".into(), "/work".into());
    projection.detect(&session("alpha", "root"), Some("running"));
    for event in [
        json!({"type":"message.updated","properties":{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1}}}}),
        part_event("reasoning", "r1", "", false),
        delta("r1", "Th"),
        part_event("text", "t1", "", false),
        delta("t1", "Pa"),
    ] {
        projection.translate(&event);
    }
    // The native store is ahead of the events this session has handled: "ink"
    // and "rs" are still in flight when the opened child is read.
    let snapshot = json!([{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1_700_000_001_000i64}},
        "parts":[part("reasoning", "r1", "Think", true), part("text", "t1", "Pars", false)]}]);
    let native = family().reply("/session/alpha/message?directory=/work", snapshot);
    let (events, messages_coverage, messages) =
        read_child(&native, "opencode", "root", "alpha", "/work").await.unwrap();
    assert!(messages_coverage == HistoryCoverage::Unverified);
    projection.rebase("alpha", &messages);
    let replayed = identified(&events);

    let mut after = Vec::new();
    for event in [
        delta("r1", "ink"),
        part_event("reasoning", "r1", "Think", true),
        delta("t1", "rs"),
        delta("t1", "ed"),
        part_event("text", "t1", "Parsed", true),
    ] {
        after.extend(projection.translate(&event));
    }
    let after = identified(&after);
    for (kind, id, full) in [("reasoning", "r1", "Think"), ("assistant_message", "a2", "Parsed")] {
        let text: String = replayed
            .iter()
            .chain(&after)
            .filter(|(k, i, _)| k == kind && i == id)
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, full, "{replayed:?} then {after:?}");
    }

    // A child replayed before this session detected it starts from the snapshot.
    let mut projection = Projection::new("root".into(), "/work".into());
    projection.rebase("alpha", &messages);
    projection.detect(&session("alpha", "root"), Some("running"));
    let late = projection.translate(&part_event("text", "t1", "Parsed", true));
    assert_eq!(rows(&late, "alpha"), ["ed"]);
}

#[test]
fn retained_replay_keeps_live_collection_on_its_previous_state() {
    let mut projection = Projection::new("root".into(), "/work".into());
    projection.detect(&session("alpha", "root"), Some("running"));
    let mut stored = Vec::new();
    for event in [
        json!({"type":"message.updated","properties":{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1}}}}),
        part_event("text", "t1", "", false),
        delta("t1", "Pa"),
    ] {
        stored.extend(projection.translate(&event));
    }
    // The daemon retained its rows for a snapshot holding "Pars": no rebase, so the
    // same-id deltas held behind the marker continue the stored rows.
    for event in [delta("t1", "rs"), delta("t1", "ed"), part_event("text", "t1", "Parsed", true)] {
        stored.extend(projection.translate(&event));
    }
    let text: String = identified(&stored)
        .iter()
        .filter(|(kind, id, _)| kind == "assistant_message" && id == "a2")
        .map(|(_, _, text)| text.as_str())
        .collect();
    assert_eq!(text, "Parsed");
}

/// What `Inner::consume` does before translating: a held child's events wait.
fn deliver(projection: &mut Projection, held: &mut family::HeldEvents, event: &Value) -> Vec<Value> {
    if held.intercept(session_id(event), s(&event["properties"]["info"]["parentID"]), event) {
        return vec![];
    }
    projection.translate(event)
}

#[tokio::test]
async fn held_child_settles_once_while_its_parent_keeps_streaming() {
    for applied in [true, false] {
        let mut projection = Projection::new("root".into(), "/work".into());
        let mut held = family::HeldEvents::default();
        projection.detect(&session("alpha", "root"), Some("running"));
        let mut stored = Vec::new();
        for event in [
            json!({"type":"message.updated","properties":{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1}}}}),
            part_event("text", "t1", "", false),
            delta("t1", "Pa"),
        ] {
            stored.extend(deliver(&mut projection, &mut held, &event));
        }
        // Marker published from a snapshot that already holds "Pars"; only alpha waits.
        let snapshot = json!([{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1_700_000_001_000i64}},
            "parts":[part("text", "t1", "Pars", false)]}]);
        let native = family().reply("/session/alpha/message?directory=/work", snapshot);
        let (replayed, _, messages) =
            read_child(&native, "opencode", "root", "alpha", "/work").await.unwrap();
        held.hold("alpha");

        let mut parent = Vec::new();
        for event in [
            json!({"type":"message.updated","properties":{"info":{"id":"p1","sessionID":"root","role":"assistant","time":{"created":1}}}}),
            json!({"type":"message.part.delta","properties":{"sessionID":"root","messageID":"p1","partID":"pt","field":"text","delta":"Hi"}}),
        ] {
            parent.extend(deliver(&mut projection, &mut held, &event));
        }
        let mut early = Vec::new();
        for event in [delta("t1", "rs"), delta("t1", "ed"), part_event("text", "t1", "Parsed", true)] {
            early.extend(deliver(&mut projection, &mut held, &event));
        }
        assert!(parent.iter().any(|e| e["item"]["text"] == "Hi"), "{parent:?}");
        assert!(early.is_empty(), "held child rows wait for the outcome: {early:?}");

        // Settlement: rebase only when applied, then translate the held events once.
        if applied {
            projection.rebase("alpha", &messages);
        }
        let mut after = Vec::new();
        for event in held.release("alpha") {
            after.extend(deliver(&mut projection, &mut held, &event));
        }
        let kept = if applied { identified(&replayed) } else { identified(&stored) };
        let text: String = kept
            .iter()
            .chain(&identified(&after))
            .filter(|(kind, id, _)| kind == "assistant_message" && id == "a2")
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, "Parsed", "applied={applied}");
    }
}

fn root_event(kind: &str, properties: Value) -> Value {
    json!({"type":kind,"properties":properties})
}

/// Drives the real legacy session over a loopback OpenCode server: while an opened
/// child's in-stream replay awaits the daemon's outcome, the parent's history,
/// turn admission and live rows keep flowing; the child's held events (including
/// a session it spawns meanwhile) are then translated once against the snapshot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_parent_keeps_serving_while_an_opened_child_replay_awaits_its_outcome() {
    use super::super::loopback::{self, Loopback};
    use crate::providers::api::{ReplayOutcome, SubagentHistoryRequest, SubagentReplay};
    use std::time::Duration;

    let server = Loopback::start("/global/event").await;
    for (route, reply) in [
        ("GET /session/root/message", json!([])),
        ("GET /provider", json!({"all":[],"connected":[]})),
        ("GET /session/root/children", json!([session("alpha", "root")])),
        ("GET /session/alpha/children", json!([])),
        ("GET /session/grand/children", json!([])),
        ("GET /session/status", json!({"alpha":{"type":"busy"}})),
        ("GET /permission", json!([])),
        ("GET /question", json!([])),
        ("GET /session/root", json!({"id":"root","directory":"/work"})),
        ("GET /session/alpha", session("alpha", "root")),
    ] {
        server.reply(route, reply);
    }
    let home = tempfile::tempdir().unwrap();
    let runtime = Runtime::loopback(Generation::Legacy, server.base_url.clone(), home.path())
        .await
        .unwrap();
    let manager = Arc::new(RuntimeManager::new(loopback::spec()));
    let (sink, mut live) = EventSink::channel(256, CancellationToken::new());
    let parent = attach(
        runtime,
        manager.clone(),
        loopback::spec(),
        json!({"id":"root","directory":"/work"}),
        json!({"cwd":"/work"}),
        loopback::context(home.path()),
        json!({}),
        sink,
    )
    .await
    .unwrap();
    server.streaming().await;
    server.push(root_event("server.connected", json!({})));
    loopback::until(&mut live, |e| e["event"]["type"] == "upsert" && e["event"]["id"] == "alpha").await;
    server.push(json!({"type":"message.updated","properties":{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1}}}}));
    server.push(part_event("text", "t1", "", false));
    server.push(delta("t1", "Pa"));
    loopback::until(&mut live, |e| e["event"]["item"]["text"] == "Pa").await;

    // The native store is already at "Pars" when the child is opened.
    server.reply(
        "GET /session/alpha/message",
        json!([{"info":{"id":"a2","sessionID":"alpha","role":"assistant","time":{"created":1_700_000_001_000i64}},
            "parts":[part("text", "t1", "Pars", false)]}]),
    );
    let (outcome, decided) = tokio::sync::watch::channel(None);
    let request = SubagentHistoryRequest {
        persistence: json!({"provider":"opencode","sessionId":"root","nativeHandle":"root","metadata":{"cwd":"/work"}}),
        config: json!({"cwd":"/work"}),
        child_id: "alpha".into(),
        source: json!({"sessionId":"alpha","directory":"/work"}),
        status: json!("running"),
        replay: SubagentReplay::new("k".into(), decided),
    };
    manager
        .families
        .live("root")
        .expect("the live parent is registered")
        .publish_replay("opencode", request, "/work", CancellationToken::new())
        .await
        .unwrap();
    let marked = loopback::until(&mut live, |e| e["event"]["type"] == "replay").await;
    let marker = &marked.last().unwrap()["event"];
    assert_eq!(marker["id"], "alpha");
    assert_eq!(
        loopback::child_rows(marker["events"].as_array().unwrap(), "alpha"),
        [("assistant_message".to_owned(), "a2".to_owned(), "Pars".to_owned())]
    );

    // Behind the marker: same-id deltas already in the snapshot, the final text,
    // a task the child starts, and the session that task spawns.
    server.reply("GET /session/alpha/children", json!([session("grand", "alpha")]));
    for event in [
        delta("t1", "rs"),
        delta("t1", "ed"),
        part_event("text", "t1", "Parsed", true),
        json!({"type":"message.part.updated","properties":{"part":{"id":"k1","messageID":"a2","sessionID":"alpha",
            "type":"tool","tool":"task","callID":"call1","state":{"status":"running",
            "input":{"description":"Dig","subagent_type":"explore"}}}}}),
        json!({"type":"message.updated","properties":{"info":{"id":"g1","sessionID":"grand","role":"assistant","time":{"created":2}}}}),
    ] {
        server.push(event);
    }
    // The parent keeps serving: history replay and turn admission complete, and
    // its own live rows flow, while the child's outcome is still pending.
    let (history, _replayed) = EventSink::channel(256, CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), parent.stream_history(history))
        .await
        .expect("parent history completes while the child is held")
        .unwrap();
    let turn = tokio::time::timeout(Duration::from_secs(5), parent.start_turn(json!("go"), json!({})))
        .await
        .expect("turn admission completes while the child is held")
        .unwrap();
    assert!(turn["turnId"].is_string());
    server.push(json!({"type":"message.updated","properties":{"info":{"id":"p1","sessionID":"root","role":"assistant","time":{"created":3}}}}));
    server.push(json!({"type":"message.part.delta","properties":{"sessionID":"root","messageID":"p1","partID":"pt","field":"text","delta":"Hi"}}));
    let during = loopback::until(&mut live, |e| e["item"]["text"] == "Hi").await;
    assert!(loopback::child_rows(&during, "alpha").is_empty(), "{during:?}");
    assert!(!during.iter().any(|e| e["event"]["id"] == "grand"), "{during:?}");

    outcome.send(Some(ReplayOutcome::Applied)).unwrap();
    let mut settled =
        loopback::until(&mut live, |e| e["event"]["type"] == "upsert" && e["event"]["id"] == "grand").await;
    server.push(json!({"type":"message.part.delta","properties":{"sessionID":"root","messageID":"p1","partID":"pt","field":"text","delta":"!"}}));
    settled.extend(loopback::until(&mut live, |e| e["item"]["text"] == "!").await);
    let text: Vec<_> = loopback::child_rows(&settled, "alpha")
        .into_iter()
        .filter(|(kind, _, _)| kind == "assistant_message")
        .collect();
    assert_eq!(text, [("assistant_message".to_owned(), "a2".to_owned(), "ed".to_owned())]);
    let grand = settled
        .iter()
        .find(|e| e["event"]["type"] == "upsert" && e["event"]["id"] == "grand")
        .unwrap();
    assert_eq!(grand["event"]["parentSubagentId"], "alpha");
    assert!(
        settled.iter().any(|e| e["event"]["id"] == "alpha"
            && e["event"]["item"]["detail"]["childSessionId"] == "grand"),
        "the task row still links the session it spawned: {settled:?}"
    );
}
