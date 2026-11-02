use super::{input, permissions, projection, session, transport::RpcError};
use serde_json::{Value, json};

#[test]
fn steer_only_reports_unavailable_for_definitive_native_rejections() {
    let error = |code: i64, message: &str, data: Value| RpcError {
        code: json!(code),
        message: message.into(),
        data,
    };
    assert!(error(-32601, "Method not found", Value::Null).definitive_steer_rejection());
    assert!(
        error(
            -32600,
            "expected active turn id `A` but found `B`",
            Value::Null
        )
        .definitive_steer_rejection()
    );
    assert!(
        error(
            -32600,
            "Not steerable",
            json!({"codexErrorInfo":{"activeTurnNotSteerable":{}}})
        )
        .definitive_steer_rejection()
    );
    assert!(!error(-32000, "connection lost", Value::Null).definitive_steer_rejection());
    assert!(
        !error(-32600, "input may have been accepted", Value::Null).definitive_steer_rejection()
    );
    assert!(
        !error(
            -32600,
            "expected active turn id `A` but found `B` then disconnected",
            Value::Null
        )
        .definitive_steer_rejection()
    );
}

#[test]
fn permission_denial_uses_each_native_response_shape() {
    let tool = json!({"threadId":"thread","turnId":"native-turn","itemId":"tool","command":"git status","questions":[]});
    for method in [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ] {
        let pending = permissions::request("custom-codex", json!(7), method, tool.clone()).unwrap();
        assert_eq!(permissions::deny(&pending), json!({"decision":"decline"}));
        assert_eq!(pending.request["provider"], "custom-codex");
    }
    let question = permissions::request("codex", json!(8), "tool/requestUserInput", tool).unwrap();
    assert_eq!(permissions::deny(&question), json!({"answers":{}}));
    let elicitation=permissions::request("codex",json!(9),"mcpServer/elicitation/request",json!({"threadId":"thread","serverName":"browser","mode":"openai/form","message":"Open page?","requestedSchema":{"type":"object","properties":{}}})).unwrap();
    assert_eq!(
        permissions::deny(&elicitation),
        json!({"action":"decline","content":null,"_meta":null})
    );
    assert!(permissions::automatic_reply(&elicitation).is_none());
}

#[test]
fn asynchronous_question_is_not_committed_before_steer_acceptance() {
    let mut questions = permissions::AsyncQuestions::default();
    let request=questions.receive("codex",&json!({"type":"agentMessage","id":"question","delivery":"async","questions":[{"title":"Choose a branch","options":["main","release"]}]})).unwrap();
    let id = request["id"].as_str().unwrap();
    let prepared = questions
        .prepare_response(
            id,
            &json!({"behavior":"allow","updatedInput":{"answers":{"Question 1":"release"}}}),
        )
        .unwrap();
    assert!(questions.has_pending(id));
    assert_eq!(
        prepared.prompt.as_deref(),
        Some("Answers to your questions:\n\nChoose a branch\nrelease")
    );
    let persisted = questions.serialize();
    assert!(permissions::AsyncQuestions::new(&persisted).has_pending(id));
    let completed = questions.complete(prepared).unwrap();
    assert!(!questions.has_pending(id));
    assert_eq!(completed["name"], "request_user_input_async");
}

#[test]
fn output_schema_requires_all_object_properties_at_every_depth() {
    let output=input::output_schema(&json!({"type":"object","properties":{"result":{"type":"array","items":{"type":"object","properties":{"name":{"type":"string"}}}}}})).unwrap();
    assert_eq!(output["additionalProperties"], false);
    assert_eq!(output["required"], json!(["result"]));
    assert_eq!(
        output["properties"]["result"]["items"]["required"],
        json!(["name"])
    );
    assert!(input::output_schema(&json!({"type":"object","additionalProperties":true})).is_err());
}

#[test]
fn history_retains_native_turn_ids_separate_from_user_message_ids() {
    let history = projection::history_with_images(
        &json!({"turns":[{"id":"native-turn-1","startedAt":1700000000,"completedAt":1700000001,"items":[{"id":"native-message-1","type":"userMessage","content":[{"type":"text","text":"hello"}]},{"id":"assistant-1","type":"agentMessage","text":"world"}]}]}),
        None,
        &mut |_, _| None,
    );
    assert_eq!(history[0]["item"]["messageId"], "native-message-1");
    assert_eq!(history[0]["turnId"], "native-turn-1");
    assert!(
        history[0]["timestamp"]
            .as_str()
            .unwrap()
            .starts_with("2023-11-14")
    );
    assert_eq!(history[1]["item"]["text"], "world");
}

#[test]
fn speed_options_come_from_native_catalog_not_model_name_guesses() {
    let models = vec![
        json!({"id":"future-provider-model","serviceTiers":[{"id":"burst","name":"Fast","description":"Native tier"}]}),
    ];
    let config = json!({"model":"future-provider-model","featureValues":{"service_tier":"burst"}});
    let features = session::features(&config, &models, &[]);
    assert_eq!(features[0]["value"], "burst");
    assert_eq!(
        features[0]["options"][1],
        json!({"id":"burst","label":"Fast"})
    );
    assert!(session::features(&json!({"model":"gpt-familiar"}), &[], &[]).is_empty());
}

/// Native `thread/read` replies by thread ID, recording every read. A thread with
/// a queued failure fails once per queued entry, then answers.
#[derive(Default)]
struct Threads {
    replies: std::collections::HashMap<String, Value>,
    failures: std::sync::Mutex<Vec<String>>,
    reads: std::sync::Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl session::ThreadReader for Threads {
    async fn read_thread(&self, id: &str) -> anyhow::Result<Value> {
        self.reads.lock().unwrap().push(id.to_owned());
        tokio::task::yield_now().await;
        let mut failures = self.failures.lock().unwrap();
        if let Some(index) = failures.iter().position(|failed| failed == id) {
            failures.remove(index);
            anyhow::bail!("thread {id} temporarily unavailable");
        }
        self.replies
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no rollout found for thread id {id}"))
    }
}
fn spawn(id: &str, children: &[&str], prompt: &str) -> Value {
    json!({"id":id,"type":"collabAgentToolCall","tool":"spawnAgent","status":"completed",
        "prompt":prompt,"receiverThreadIds":children})
}
fn message(id: &str, text: &str) -> Value {
    json!({"id":id,"type":"agentMessage","text":text})
}
fn thread(items: Vec<Value>) -> Value {
    json!({"thread":{"turns":[{"id":"turn","items":items}]}})
}
fn family() -> Threads {
    let mut threads = Threads::default();
    threads.replies.insert(
        "parent".into(),
        thread(vec![message("p1", "parent text"), spawn("spawn", &["child-a", "child-b"], "Review")]),
    );
    let mut child = vec![message("a1", "child a text")];
    child.extend((0..120).map(|n| message(&format!("a-row-{n}"), &format!("row {n}"))));
    child.push(spawn("nested", &["grandchild"], "Dig deeper"));
    threads.replies.insert("child-a".into(), thread(child));
    threads.replies.insert("child-b".into(), thread(vec![message("b1", "child b text")]));
    threads.replies.insert("grandchild".into(), thread(vec![message("g1", "grandchild text")]));
    threads
}
fn request(child: &str, source: Value) -> crate::providers::api::SubagentHistoryRequest {
    crate::providers::api::SubagentHistoryRequest {
        persistence: json!({"provider":"codex","sessionId":"parent","nativeHandle":"parent"}),
        config: json!({"cwd":null}),
        child_id: child.into(),
        source,
        status: json!("completed"),
        replay: crate::providers::api::SubagentReplay::new(
            "replay".into(),
            tokio::sync::watch::channel(None).1,
        ),
    }
}

#[tokio::test]
async fn in_stream_replay_holds_live_rows_until_the_daemon_decides() {
    use crate::providers::api::{ChildRowHold, EventSink, HistoryCoverage, ReplayOutcome, SubagentReplay};
    let row = |id: &str, text: &str| {
        json!({"type":"provider_subagent","provider":"codex","event":{"type":"timeline","id":"child-b",
            "item":{"type":"assistant_message","text":text,"messageId":id}}})
    };
    for outcome in [ReplayOutcome::Applied, ReplayOutcome::Retained] {
        let (sink, mut published) = EventSink::channel(64, tokio_util::sync::CancellationToken::new());
        let hold = std::sync::Arc::new(ChildRowHold::default());
        let (decide, decision) = tokio::sync::watch::channel(None);
        let mut selected = request("child-b", Value::Null);
        selected.status = json!("running");
        selected.replay = SubagentReplay::new("k".into(), decision);
        hold.emit(&sink, row("b0", "before")).await.unwrap();
        let (reading, read_started) = tokio::sync::oneshot::channel::<()>();
        let (finish, read_finished) = tokio::sync::oneshot::channel::<()>();
        let snapshot = vec![crate::providers::api::subagent_row(
            "codex",
            "child-b",
            json!({"type":"assistant_message","text":"done","messageId":"b1"}),
            None,
            HistoryCoverage::Complete,
        )];
        let task = {
            let (hold, sink) = (hold.clone(), sink.clone());
            tokio::spawn(async move {
                let read = async move {
                    reading.send(()).unwrap();
                    read_finished.await.unwrap();
                    Ok::<_, anyhow::Error>((snapshot, HistoryCoverage::Complete))
                };
                hold.publish_replay(&selected, "codex", &sink, read, |item, snapshot| {
                    snapshot.iter().any(|row| row["messageId"] == item["messageId"])
                })
                .await
            })
        };
        read_started.await.unwrap();
        // Live rows from the moment the snapshot is taken wait for the decision.
        hold.emit(&sink, row("b1", "done")).await.unwrap();
        hold.emit(&sink, row("b2", "new")).await.unwrap();
        finish.send(()).unwrap();
        let before = published.recv().await.unwrap();
        assert_eq!(before["event"]["item"]["text"], "before");
        let marker = published.recv().await.unwrap();
        assert_eq!(marker["event"]["type"], "replay");
        assert_eq!(marker["event"]["historyComplete"], false, "a running child is never authoritative");
        assert_eq!(marker["event"]["events"][0]["event"]["item"]["text"], "done");
        assert!(published.try_recv().is_err(), "held rows wait for the daemon");
        decide.send(Some(outcome)).unwrap();
        assert!(matches!(
            task.await.unwrap().unwrap(),
            crate::providers::api::SubagentHistory::Published
        ));
        let mut rest = Vec::new();
        while let Ok(event) = published.try_recv() {
            rest.push(event["event"]["item"]["text"].as_str().unwrap().to_owned());
        }
        match outcome {
            ReplayOutcome::Applied => assert_eq!(rest, ["new"], "the applied snapshot already has b1"),
            ReplayOutcome::Retained => assert_eq!(rest, ["done", "new"], "kept state continues as it was"),
        }
        // The hold is lifted: later rows flow straight through.
        hold.emit(&sink, row("b3", "after")).await.unwrap();
        assert_eq!(published.recv().await.unwrap()["event"]["item"]["text"], "after");
    }
}

#[test]
fn a_child_not_recorded_as_finished_never_gets_an_authoritative_replay() {
    use crate::providers::api::HistoryCoverage;
    let rows = || {
        vec![crate::providers::api::subagent_row("codex", "child-b", json!({"type":"assistant_message","text":"x"}),
            None, HistoryCoverage::Complete)]
    };
    for (status, expected) in [
        (json!("completed"), true),
        (json!("failed"), true),
        (json!("canceled"), true),
        (json!("running"), false),
        (Value::Null, false),
    ] {
        let mut selected = request("child-b", Value::Null);
        selected.status = status.clone();
        let mut events = rows();
        let coverage = selected.settle(&mut events, HistoryCoverage::Complete);
        assert_eq!(coverage == HistoryCoverage::Complete, expected, "{status}");
        assert_eq!(events[0]["event"]["historyComplete"], expected, "{status}");
    }
    let mut events = rows();
    let coverage = request("child-b", Value::Null).settle(&mut events, HistoryCoverage::Unverified);
    assert!(coverage == HistoryCoverage::Unverified, "a partial read stays partial");
    assert_eq!(events[0]["event"]["historyComplete"], false);
}

#[tokio::test]
async fn parent_history_announces_children_without_reading_their_threads() {
    let threads = family();
    let history = session::load_history(&threads, "parent", None, "codex").await.unwrap();
    assert_eq!(*threads.reads.lock().unwrap(), ["parent"]);
    let children: Vec<_> = history
        .iter()
        .filter(|event| event["type"] == "provider_subagent")
        .map(|event| &event["event"])
        .collect();
    assert!(children.iter().all(|event| event["type"] == "upsert"), "{children:?}");
    let ids: Vec<_> = children.iter().map(|event| event["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["child-a", "child-b"]);
    assert_eq!(children[0]["source"], json!({"threadId":"child-a"}));
    assert_eq!(children[0]["toolCallId"], "spawn");
    assert_eq!(children[0]["parentSubagentId"], Value::Null);
    assert!(history.iter().any(|event| event["item"]["text"] == "parent text"));
}

#[tokio::test]
async fn opened_child_reads_only_its_thread_and_announces_grandchildren() {
    let threads = family();
    let (events, coverage) =
        session::subagent_history(&threads, &request("child-a", json!({"threadId":"child-a"})), "codex", false)
            .await
            .unwrap();
    assert!(coverage == crate::providers::api::HistoryCoverage::Complete);
    assert_eq!(*threads.reads.lock().unwrap(), ["child-a"]);
    let rows: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "timeline").collect();
    assert_eq!(rows.len(), 122, "every row of the selected child is replayed for paging");
    assert!(rows.iter().all(|row| row["event"]["id"] == "child-a"
        && row["event"]["historyComplete"] == true
        && row["type"] == "provider_subagent"));
    assert_eq!(rows[0]["event"]["item"]["text"], "child a text");
    let nested: Vec<_> = events.iter().filter(|e| e["event"]["type"] == "upsert").collect();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0]["event"]["id"], "grandchild");
    assert_eq!(nested[0]["event"]["parentSubagentId"], "child-a");
    assert_eq!(nested[0]["event"]["source"], json!({"threadId":"grandchild"}));
}

#[tokio::test]
async fn selected_child_failures_are_errors_and_retry_reads_again() {
    let threads = family();
    threads.failures.lock().unwrap().push("child-b".into());
    let selected = request("child-b", Value::Null);
    let (first, second) = tokio::join!(
        session::subagent_history(&threads, &selected, "codex", false),
        session::subagent_history(&threads, &selected, "codex", false),
    );
    assert!(first.is_err() != second.is_err(), "exactly one concurrent read saw the failure");
    let succeeded = first.or(second).unwrap().0;
    let retried = session::subagent_history(&threads, &selected, "codex", false).await.unwrap().0;
    assert_eq!(succeeded, retried, "a failure is never cached as the child's history");
    assert_eq!(*threads.reads.lock().unwrap(), ["child-b", "child-b", "child-b"]);
    let missing = session::subagent_history(&threads, &request("gone", Value::Null), "codex", false).await;
    assert!(missing.unwrap_err().to_string().contains("no rollout found"));
    let mut torn = family();
    torn.replies.insert("child-a".into(), json!({"thread":{"turns":[{"id":"turn"}]}}));
    let incomplete = session::subagent_history(&torn, &request("child-a", Value::Null), "codex", false).await;
    assert!(incomplete.is_err(), "an incomplete thread is not an empty replay");
}

#[tokio::test]
async fn cold_child_read_shows_the_current_unfinished_turn() {
    let mut threads = family();
    threads.replies.insert(
        "child-b".into(),
        json!({"thread":{"turns":[
            {"id":"live","status":"inProgress","items":[message("b2", "partial snapshot")]}
        ]}}),
    );
    let (events, coverage) =
        session::subagent_history(&threads, &request("child-b", Value::Null), "codex", false).await.unwrap();
    let texts: Vec<_> = events.iter().filter_map(|event| event["event"]["item"]["text"].as_str()).collect();
    assert_eq!(texts, ["partial snapshot"], "a cold read has no live stream to supply this turn");
    assert!(coverage == crate::providers::api::HistoryCoverage::Unverified);
}

#[tokio::test]
async fn child_turn_in_progress_is_left_to_the_live_stream() {
    let mut threads = family();
    threads.replies.insert(
        "child-b".into(),
        json!({"thread":{"turns":[
            {"id":"done","status":"completed","items":[message("b1", "finished reply")]},
            {"id":"live","status":"inProgress","items":[message("b2", "partial snapsh")]}
        ]}}),
    );
    let (events, coverage) =
        session::subagent_history(&threads, &request("child-b", Value::Null), "codex", true).await.unwrap();
    assert!(
        coverage == crate::providers::api::HistoryCoverage::Unverified,
        "a growing child cannot be replayed authoritatively"
    );
    let texts: Vec<_> = events.iter().map(|e| e["event"]["item"]["text"].as_str().unwrap()).collect();
    assert_eq!(
        texts,
        ["finished reply"],
        "an in-progress snapshot never replaces the live text it lags behind"
    );
    assert_eq!(events[0]["event"]["historyComplete"], false);
}

/// Parent recovery as `CodexSession::open` performs it, from the stored card hints
/// the daemon passes in resume options.
async fn resumed(threads: &Threads, options: &Value) -> (Vec<Value>, session::State) {
    let mut history = session::load_history(threads, "parent", None, "codex").await.unwrap();
    let logs = session::card_logs(options);
    session::apply_card_logs(&mut history, &logs);
    let mut state = session::State::default();
    for event in history.iter().filter(|event| event["type"] == "provider_subagent") {
        let id = event["event"]["id"].as_str().unwrap().to_owned();
        let card = history
            .iter()
            .find(|e| e["type"] == "timeline" && e["item"]["callId"] == event["event"]["toolCallId"])
            .unwrap()["item"]
            .clone();
        state.children.insert(id.clone(), event["event"].clone());
        state.child_tools.insert(id, card);
    }
    state.card_logs = session::seed_card_logs(&logs, &mut state.child_items);
    (history, state)
}
fn live(text: &str, id: &str) -> Value {
    json!({"type":"provider_subagent","event":{"type":"timeline","id":"child-b",
        "item":{"type":"assistant_message","text":text,"messageId":id}}})
}

#[tokio::test]
async fn resumed_child_card_keeps_recorded_log_without_reading_child_threads() {
    use super::session::CodexSession;
    let threads = family();
    let options = json!({"purpose":"interactive",
        "subagentCardLogs":{"spawn":"[Assistant] reviewed the parser"}});
    let (history, mut state) = resumed(&threads, &options).await;
    let card = history
        .iter()
        .find(|e| e["type"] == "timeline" && e["item"]["callId"] == "spawn")
        .unwrap();
    assert_eq!(
        card["item"]["detail"]["log"], "[Assistant] reviewed the parser",
        "the replayed card keeps the recorded log, so reconciling it by call ID loses nothing"
    );
    let updates =
        CodexSession::aggregate(&mut state, &[live("new progress", "b2")], Some("b2"), true).unwrap();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["item"]["callId"], "spawn");
    assert_eq!(
        updates[0]["item"]["detail"]["log"],
        "[Assistant] reviewed the parser\n[Assistant] new progress"
    );
    let updates =
        CodexSession::aggregate(&mut state, &[live(" and more", "b2")], Some("b2"), true).unwrap();
    assert_eq!(
        updates[0]["item"]["detail"]["log"],
        "[Assistant] reviewed the parser\n[Assistant] new progress and more",
        "new text is appended exactly once"
    );
    assert_eq!(*threads.reads.lock().unwrap(), ["parent"], "no child thread is read before its pane opens");
    session::subagent_history(&threads, &request("child-b", Value::Null), "codex", false).await.unwrap();
    assert_eq!(*threads.reads.lock().unwrap(), ["parent", "child-b"]);
}

#[tokio::test]
async fn card_without_recorded_log_shows_only_observed_activity() {
    use super::session::CodexSession;
    let threads = family();
    let (history, mut state) = resumed(&threads, &json!({"purpose":"interactive"})).await;
    let card = history
        .iter()
        .find(|e| e["type"] == "timeline" && e["item"]["callId"] == "spawn")
        .unwrap();
    assert_eq!(card["item"]["detail"]["log"], "");
    let updates =
        CodexSession::aggregate(&mut state, &[live("new progress", "b2")], Some("b2"), true).unwrap();
    assert_eq!(updates[0]["item"]["detail"]["log"], "[Assistant] new progress");
    assert_eq!(*threads.reads.lock().unwrap(), ["parent"]);
}

#[tokio::test]
async fn resumed_partial_child_item_continues_without_repeating() {
    use super::session::CodexSession;
    let threads = family();
    // The previous session recorded a finished item and one still streaming.
    let (_, mut previous) = resumed(&threads, &json!({})).await;
    CodexSession::aggregate(&mut previous, &[live("First.", "b1")], Some("b1"), true).unwrap();
    let updates =
        CodexSession::aggregate(&mut previous, &[live("Hello ", "b2")], Some("b2"), true).unwrap();
    let card = updates[0]["item"].clone();
    assert_eq!(card["detail"]["log"], "[Assistant] First.\n[Assistant] Hello");
    // The daemon stores that card and hands its log and identities back on resume.
    let options = json!({"subagentCardLogs":{"spawn":card["detail"]["log"]},
        "subagentCardEntries":{"spawn":card["metadata"]["subagentLog"]}});
    for full in [false, true] {
        let (history, mut state) = resumed(&threads, &options).await;
        let replayed = history
            .iter()
            .find(|e| e["type"] == "timeline" && e["item"]["callId"] == "spawn")
            .unwrap();
        assert_eq!(replayed["item"]["detail"]["log"], card["detail"]["log"]);
        assert_eq!(replayed["item"]["metadata"]["subagentLog"], card["metadata"]["subagentLog"]);
        // The same native item continues: by its next delta, or by its full text.
        let continued = if full {
            CodexSession::aggregate(&mut state, &[live("Hello world", "b2")], Some("b2"), false)
        } else {
            CodexSession::aggregate(&mut state, &[live("world", "b2")], Some("b2"), true)
        }
        .unwrap();
        assert_eq!(
            continued[0]["item"]["detail"]["log"],
            "[Assistant] First.\n[Assistant] Hello world",
            "full={full}: the resumed item is neither repeated nor truncated"
        );
        // A distinct item, even with text repeating an earlier one, appends once.
        let next =
            CodexSession::aggregate(&mut state, &[live("First.", "b3")], Some("b3"), true).unwrap();
        assert_eq!(
            next[0]["item"]["detail"]["log"],
            "[Assistant] First.\n[Assistant] Hello world\n[Assistant] First."
        );
    }
    assert_eq!(*threads.reads.lock().unwrap(), ["parent", "parent", "parent"], "no child thread read");
}
