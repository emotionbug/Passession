use super::super::reader::FakeNative;
use super::*;

fn session(id: &str, parent: &str) -> Value {
    json!({"id":id,"parentID":parent,"title":format!("{id} task"),
        "location":{"directory":"/work"},"time":{"updated":1_700_000_000_000i64}})
}
fn page(items: Vec<Value>) -> Value {
    json!({"data":items,"cursor":{}})
}
fn user(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"user","text":text,"time":{"created":1_700_000_000_000i64}})
}
fn assistant(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"assistant","content":[{"type":"text","text":text}],
        "time":{"created":1_700_000_001_000i64}})
}
/// root spawned alpha and beta; alpha spawned grand. Nothing is running.
fn family() -> FakeNative {
    FakeNative::default()
        .reply("/api/session?limit=100&parentID=root", page(vec![session("alpha", "root"), session("beta", "root")]))
        .reply("/api/session?limit=100&parentID=alpha", page(vec![session("grand", "alpha")]))
        .reply("/api/session?limit=100&parentID=beta", page(vec![]))
        .reply("/api/session?limit=100&parentID=grand", page(vec![]))
        .reply("/api/session/active", json!({"data":{}}))
        .reply("/api/session/alpha", json!({"data":session("alpha", "root")}))
        .reply("/api/session/beta", json!({"data":session("beta", "root")}))
        .reply("/api/session/grand", json!({"data":session("grand", "alpha")}))
        .reply(
            "/api/session/alpha/message?limit=100&order=asc",
            page(vec![user("a1", "Review the parser"), assistant("a2", "Parser is fine")]),
        )
        .reply("/api/session/beta/message?limit=100&order=asc", page(vec![assistant("b1", "sibling body")]))
        .reply("/api/session/grand/message?limit=100&order=asc", page(vec![assistant("g1", "grandchild body")]))
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
async fn parent_discovery_lists_descendants_without_reading_bodies() {
    let native = family();
    let found = descendants(&native, "root").await.unwrap();
    let reads = native.reads();
    assert!(reads.iter().all(|read| !read.contains("/message")), "{reads:?}");
    let ids: Vec<_> = found.iter().map(|info| s(info, "id")).collect();
    assert_eq!(ids, ["alpha", "beta", "grand"]);

    let active = json!({"alpha":true});
    let alpha = descriptor(&found[0], "root", &active);
    assert_eq!(alpha["type"], "upsert");
    assert_eq!(alpha["parentSubagentId"], Value::Null);
    assert_eq!(alpha["status"], "running");
    assert_eq!(alpha["source"], json!({"sessionId":"alpha","directory":"/work"}));
    let grand = descriptor(&found[2], "root", &active);
    assert_eq!(grand["parentSubagentId"], "alpha");
    assert_eq!(grand["status"], "completed");
}

#[tokio::test]
async fn opened_child_replays_only_its_own_body_and_announces_what_it_spawned() {
    let native = family();
    let (events, coverage) = replay_child(&native, "opencode", "root", "alpha").await.unwrap();
    assert!(coverage == HistoryCoverage::Complete);
    let reads = native.reads();
    assert!(
        !reads.iter().any(|read| read.starts_with("/api/session/beta") || read.starts_with("/api/session/grand/message")),
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
    assert_eq!(spawned[0]["event"]["source"], json!({"sessionId":"grand","directory":"/work"}));
}

#[tokio::test]
async fn nested_child_resolves_through_its_ancestry_and_strangers_are_refused() {
    let native = family();
    let (events, _) = replay_child(&native, "opencode", "root", "grand").await.unwrap();
    assert_eq!(rows(&events, "grand"), ["grandchild body"]);

    let native = FakeNative::default()
        .reply("/api/session/stray", json!({"data":session("stray", "elsewhere")}))
        .reply("/api/session/elsewhere", json!({"data":{"id":"elsewhere"}}))
        .reply("/api/session/stray/message?limit=100&order=asc", page(vec![assistant("s1", "foreign")]));
    let error = replay_child(&native, "opencode", "root", "stray")
        .await
        .err()
        .expect("a session outside the parent family is refused");
    assert!(error.to_string().contains("not a subagent of root"), "{error}");
    assert!(!native.reads().iter().any(|read| read.contains("/message")));
}

#[tokio::test]
async fn unreadable_child_body_is_an_error_not_an_empty_replay() {
    let native = FakeNative::default()
        .reply("/api/session/beta", json!({"data":session("beta", "root")}))
        .reply("/api/session?limit=100&parentID=beta", page(vec![]))
        .reply("/api/session/active", json!({"data":{}}));
    let error = replay_child(&native, "opencode", "root", "beta")
        .await
        .err()
        .expect("a missing body fails");
    assert!(error.to_string().contains("404"), "{error}");

    let malformed = FakeNative::default()
        .reply("/api/session/beta", json!({"data":session("beta", "root")}))
        .reply("/api/session/beta/message?limit=100&order=asc", json!({"error":"store unavailable"}));
    assert!(replay_child(&malformed, "opencode", "root", "beta").await.is_err());

    let missing = FakeNative::default().reply("/api/session/beta", json!({"data":null}));
    assert!(replay_child(&missing, "opencode", "root", "beta").await.is_err());
}

#[tokio::test]
async fn running_child_replay_is_unverified_for_live_collection() {
    let native = family().reply("/api/session/active", json!({"data":{"alpha":true}}));
    let (events, coverage) = replay_child(&native, "opencode", "root", "alpha").await.unwrap();
    assert!(coverage == HistoryCoverage::Unverified);
    assert!(events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .all(|e| e["event"]["historyComplete"] == false));
}

#[test]
fn live_child_collection_emits_only_new_content_and_terminal_status() {
    let mut live = Timeline::default();
    let first = live.messages(&[assistant("m1", "Hel")], true).unwrap();
    let second = live.messages(&[assistant("m1", "Hello")], true).unwrap();
    let texts: Vec<_> = first
        .iter()
        .chain(&second)
        .map(|e| e["item"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["Hel", "lo"]);

    let mut failed = session("alpha", "root");
    failed["outcome"] = json!("failed");
    assert_eq!(descriptor(&failed, "root", &json!({}))["status"], "failed");
    failed["outcome"] = json!("interrupted");
    assert_eq!(descriptor(&failed, "root", &json!({}))["status"], "canceled");
    assert_eq!(descriptor(&failed, "root", &json!({"alpha":true}))["status"], "running");
}

fn thinking(id: &str, reasoning: &str, text: &str) -> Value {
    json!({"id":id,"type":"assistant","time":{"created":1_700_000_001_000i64},
        "content":[{"type":"reasoning","text":reasoning},{"type":"text","text":text}]})
}
/// `(type, messageId, text)` of every timeline item, in order.
fn identified<'a>(items: impl IntoIterator<Item = &'a Value>) -> Vec<(String, String, String)> {
    items
        .into_iter()
        .map(|item| {
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
    // Live child collection re-reads the child through its own timeline.
    let mut live = Timeline::default();
    let before = live.messages(&[thinking("m1", "Th", "Pa")], true).unwrap();
    let before = identified(before.iter().map(|e| &e["item"]));
    assert_eq!(
        before.iter().map(|(k, i, _)| (k.as_str(), i.as_str())).collect::<Vec<_>>(),
        [("reasoning", "m1:reasoning:0"), ("assistant_message", "m1")]
    );

    let native = family()
        .reply("/api/session/active", json!({"data":{"alpha":true}}))
        .reply(
            "/api/session/alpha/message?limit=100&order=asc",
            page(vec![user("a1", "Review the parser"), thinking("m1", "Th", "Pa")]),
        );
    let (events, coverage) = replay_child(&native, "opencode", "root", "alpha").await.unwrap();
    assert!(coverage == HistoryCoverage::Unverified);
    let replayed = identified(
        events
            .iter()
            .filter(|e| e["event"]["type"] == "timeline")
            .map(|e| &e["event"]["item"]),
    );
    for (kind, id, _) in &before {
        let matches = replayed.iter().filter(|(k, i, _)| k == kind && i == id).count();
        assert_eq!(matches, 1, "{kind} {id} in {replayed:?}");
    }

    let after = live.messages(&[thinking("m1", "Think", "Parsed")], true).unwrap();
    let after = identified(after.iter().map(|e| &e["item"]));
    for (kind, id, full) in [("reasoning", "m1:reasoning:0", "Think"), ("assistant_message", "m1", "Parsed")] {
        let text: String = replayed
            .iter()
            .chain(&after)
            .filter(|(k, i, _)| k == kind && i == id)
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, full);
    }
}

#[test]
fn streamed_reasoning_deltas_carry_the_identity_message_replay_derives() {
    let mut stream = Timeline::default();
    let data = json!({"assistantMessageID":"m1","ordinal":0,"delta":"Th"});
    stream.start(&data, "reasoning");
    let streamed = stream.delta(&data, "reasoning").unwrap().unwrap();
    assert_eq!(streamed["item"]["messageId"], "m1:reasoning:0");
    let replayed = Timeline::default().messages(&[thinking("m1", "Th", "")], false).unwrap();
    assert_eq!(replayed[0]["item"]["messageId"], streamed["item"]["messageId"]);
}

#[tokio::test]
async fn live_timeline_rebased_on_a_replay_continues_after_the_snapshot() {
    let mut live = Timeline::default();
    live.messages(&[thinking("m1", "Th", "Pa")], true).unwrap();
    // The native store is ahead of this live timeline when the child is opened.
    let native = family()
        .reply("/api/session/active", json!({"data":{"alpha":true}}))
        .reply(
            "/api/session/alpha/message?limit=100&order=asc",
            page(vec![thinking("m1", "Think", "Pars")]),
        );
    let (events, coverage, history) = read_child(&native, "opencode", "root", "alpha").await.unwrap();
    assert!(coverage == HistoryCoverage::Unverified);
    live.messages(&history, true).unwrap();
    let replayed = identified(
        events
            .iter()
            .filter(|e| e["event"]["type"] == "timeline")
            .map(|e| &e["event"]["item"]),
    );
    let after = live.messages(&[thinking("m1", "Think", "Parsed")], true).unwrap();
    let after = identified(after.iter().map(|e| &e["item"]));
    assert_eq!(after, [("assistant_message".to_owned(), "m1".to_owned(), "ed".to_owned())]);
    for (kind, id, full) in [("reasoning", "m1:reasoning:0", "Think"), ("assistant_message", "m1", "Parsed")] {
        let text: String = replayed
            .iter()
            .chain(&after)
            .filter(|(k, i, _)| k == kind && i == id)
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, full);
    }
}

#[test]
fn retained_replay_keeps_live_timeline_on_its_previous_state() {
    let mut live = Timeline::default();
    let stored = live.messages(&[thinking("m1", "Th", "Pa")], true).unwrap();
    // The daemon retained its rows for a snapshot holding "Think"/"Pars": no rebase,
    // so the next live re-read continues the stored rows.
    let after = live.messages(&[thinking("m1", "Think", "Parsed")], true).unwrap();
    let rows = identified(stored.iter().chain(&after).map(|e| &e["item"]));
    for (kind, id, full) in [("reasoning", "m1:reasoning:0", "Think"), ("assistant_message", "m1", "Parsed")] {
        let text: String = rows
            .iter()
            .filter(|(k, i, _)| k == kind && i == id)
            .map(|(_, _, text)| text.as_str())
            .collect();
        assert_eq!(text, full);
    }
}

/// Drives the real v2 session actor over a loopback OpenCode server: while an
/// opened child's in-stream replay awaits the daemon's outcome, the actor keeps
/// serving parent history and turn admission; the outcome then resumes that
/// child's live collection from the snapshot, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_parent_keeps_serving_while_an_opened_child_replay_awaits_its_outcome() {
    use super::super::loopback::{self, Loopback};
    use crate::providers::api::{ReplayOutcome, SubagentReplay};
    use std::time::Duration;

    let server = Loopback::start("/api/event").await;
    for (route, reply) in [
        ("GET /api/plugin", json!({"data":[{"id":"other"}]})),
        ("GET /api/agent", json!({"data":[]})),
        ("GET /api/session/root/message", page(vec![])),
        ("GET /api/session/root", json!({"data":{"id":"root","location":{"directory":"/work"}}})),
        ("GET /api/session/root/permission", json!({"data":[]})),
        ("GET /api/session/root/form", json!({"data":[]})),
        ("GET /api/session?limit=100&parentID=root", page(vec![session("alpha", "root")])),
        ("GET /api/session?limit=100&parentID=alpha", page(vec![])),
        ("GET /api/session/active", json!({"data":{"alpha":true}})),
        ("GET /api/session/alpha/permission", json!({"data":[]})),
        ("GET /api/session/alpha/form", json!({"data":[]})),
        ("GET /api/session/alpha", json!({"data":session("alpha", "root")})),
        ("GET /api/session/alpha/message", page(vec![thinking("m1", "Th", "Pa")])),
    ] {
        server.reply(route, reply);
    }
    let home = tempfile::tempdir().unwrap();
    let runtime = Runtime::loopback(
        crate::providers::opencode::runtime::Generation::V2,
        server.base_url.clone(),
        home.path(),
    )
    .await
    .unwrap();
    let manager = Arc::new(RuntimeManager::new(loopback::spec()));
    let (sink, mut live) = EventSink::channel(256, CancellationToken::new());
    let attaching = tokio::spawn(attach(
        runtime,
        manager.clone(),
        loopback::spec(),
        json!({"id":"root","location":{"directory":"/work"}}),
        json!({"cwd":"/work"}),
        loopback::context(home.path()),
        json!({}),
        sink,
    ));
    server.streaming().await;
    server.push(json!({"type":"server.connected"}));
    let parent = attaching.await.unwrap().unwrap();
    loopback::until(&mut live, |e| e["event"]["type"] == "upsert" && e["event"]["id"] == "alpha").await;
    server.push(json!({"type":"session.text.delta","data":{"sessionID":"alpha"}}));
    loopback::until(&mut live, |e| e["event"]["item"]["text"] == "Pa").await;

    // The native store is already at "Think"/"Pars" when the child is opened.
    server.reply("GET /api/session/alpha/message", page(vec![thinking("m1", "Think", "Pars")]));
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
    assert_eq!(
        loopback::child_rows(marker["events"].as_array().unwrap(), "alpha"),
        [
            ("reasoning".to_owned(), "m1:reasoning:0".to_owned(), "Think".to_owned()),
            ("assistant_message".to_owned(), "m1".to_owned(), "Pars".to_owned()),
        ]
    );

    // The child advances behind the marker; its live re-read waits.
    server.reply("GET /api/session/alpha/message", page(vec![thinking("m1", "Think", "Parsed")]));
    server.push(json!({"type":"session.text.delta","data":{"sessionID":"alpha"}}));
    let (history, _replayed) = EventSink::channel(256, CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), parent.stream_history(history))
        .await
        .expect("parent history completes while the child is held")
        .unwrap();
    let turn = tokio::time::timeout(Duration::from_secs(5), parent.start_turn(json!("go"), json!({})))
        .await
        .expect("turn admission completes while the child is held")
        .unwrap();
    let during = loopback::until(&mut live, |e| e["type"] == "turn_started" && e["turnId"] == turn["turnId"]).await;
    assert!(loopback::child_rows(&during, "alpha").is_empty(), "{during:?}");

    outcome.send(Some(ReplayOutcome::Applied)).unwrap();
    let mut settled = loopback::until(&mut live, |e| e["event"]["item"]["text"] == "ed").await;
    // A later actor call is served only after the settlement finished.
    let (history, _replayed) = EventSink::channel(256, CancellationToken::new());
    parent.stream_history(history).await.unwrap();
    while let Ok(event) = live.try_recv() {
        settled.push(event);
    }
    assert_eq!(
        loopback::child_rows(&settled, "alpha"),
        [("assistant_message".to_owned(), "m1".to_owned(), "ed".to_owned())]
    );
}
