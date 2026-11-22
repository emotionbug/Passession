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
        json!({"action":"cancel","content":null,"_meta":null})
    );
    assert!(permissions::reply(&elicitation, &json!({"behavior":"allow"})).is_err());
    assert_eq!(
        permissions::reply(&elicitation, &json!({"behavior":"allow","selectedActionId":"accept"})).unwrap(),
        json!({"action":"accept","content":{},"_meta":null})
    );
}

#[test]
fn approvals_offer_session_and_prefix_rule_choices() {
    let params = json!({"threadId":"thread","turnId":"turn","itemId":"cmd","command":"cargo test",
        "proposedExecpolicyAmendment":["cargo","test"]});
    let pending = permissions::request(
        "codex",
        json!(3),
        "item/commandExecution/requestApproval",
        params,
    )
    .unwrap();
    let ids: Vec<_> = pending.request["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|action| action["id"].clone())
        .collect();
    assert_eq!(
        ids,
        [
            json!("decline"),
            json!("accept"),
            json!("accept_session"),
            json!("accept_always")
        ]
    );
    let reply = |id: &str| {
        permissions::reply(&pending, &json!({"behavior":"allow","selectedActionId":id})).unwrap()
    };
    assert_eq!(
        reply("accept_session"),
        json!({"decision":"acceptForSession"})
    );
    assert_eq!(
        reply("accept_always"),
        json!({"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["cargo","test"]}}})
    );
    assert_eq!(
        permissions::reply(&pending, &json!({"behavior":"allow"})).unwrap(),
        json!({"decision":"accept"})
    );
    let files = permissions::request(
        "codex",
        json!(4),
        "item/fileChange/requestApproval",
        json!({"threadId":"thread","turnId":"turn","itemId":"patch"}),
    )
    .unwrap();
    assert_eq!(files.request["actions"].as_array().unwrap().len(), 3);
    assert_eq!(
        permissions::reply(
            &files,
            &json!({"behavior":"deny","selectedActionId":"decline"})
        )
        .unwrap(),
        json!({"decision":"decline"})
    );
}

#[test]
fn network_approval_offers_the_proposed_allow_and_deny_rules() {
    let amendments = json!([
        {"action":"allow","host":"example.test:443"},
        {"action":"deny","host":"example.test:443"},
    ]);
    let pending = permissions::request(
        "codex",
        json!(5),
        "item/commandExecution/requestApproval",
        json!({"threadId":"thread","turnId":"turn","itemId":"network",
            "reason":"Fetch package metadata",
            "networkApprovalContext":{"host":"example.test:443","protocol":"https"},
            "proposedNetworkPolicyAmendments":amendments}),
    )
    .unwrap();
    let description = pending.request["description"].as_str().unwrap();
    for expected in ["example.test:443", "https", "Fetch package metadata"] {
        assert!(description.contains(expected));
    }
    let actions = pending.request["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 5);
    for (action, amendment) in actions[3..].iter().zip(amendments.as_array().unwrap()) {
        assert_eq!(action["behavior"], amendment["action"]);
        assert_eq!(
            permissions::reply(
                &pending,
                &json!({"behavior":action["behavior"],"selectedActionId":action["id"]}),
            )
            .unwrap(),
            json!({"decision":{"applyNetworkPolicyAmendment":{"network_policy_amendment":amendment}}})
        );
    }
    assert_eq!(
        permissions::reply(
            &pending,
            &json!({"behavior":"deny","selectedActionId":actions[3]["id"]}),
        )
        .unwrap(),
        json!({"decision":"decline"})
    );
}

#[test]
fn codex_question_replies_omit_unanswered_options() {
    let params = json!({"threadId":"thread","turnId":"native-turn","itemId":"question","questions":[
        {"id":"first","header":"First","question":"Choose one.","options":[{"label":"A"},{"label":"B"}]},
        {"id":"second","header":"Second","question":"Choose another.","options":[{"label":"C"}]}
    ]});
    for method in ["item/tool/requestUserInput", "tool/requestUserInput"] {
        let pending = permissions::request("codex", json!(8), method, params.clone()).unwrap();
        for response in [
            json!({"behavior":"allow"}),
            json!({"behavior":"allow","updatedInput":{"answers":{"First":" "}}}),
            json!({"behavior":"allow","updatedInput":{"selections":{"First":{"options":[],"other":""}}}}),
            json!({"behavior":"deny","updatedInput":{"answers":{"First":"A"}}}),
        ] {
            assert_eq!(permissions::reply(&pending, &response).unwrap(), json!({"answers":{}}));
            let item = permissions::question_timeline(&pending, Some(&response)).unwrap();
            assert!(!item["detail"]["text"].as_str().unwrap().contains("Answers:"));
        }
        assert_eq!(
            permissions::reply(&pending, &json!({"behavior":"allow","updatedInput":{"answers":{"First":"B"}}})).unwrap(),
            json!({"answers":{"first":{"answers":["B"]}}})
        );
    }
}

#[cfg(unix)]
async fn start_codex_event_turn(session: &session::CodexSession, plan_mode: bool) {
    {
        let mut state = session.state.lock().unwrap();
        state.thread = Some("thread".into());
        state.foreground = Some("local-turn".into());
        state.config["featureValues"]["plan_mode"] = json!(plan_mode);
    }
    session
        .handle_frame(json!({"method":"turn/started","params":{
            "threadId":"thread","turn":{"id":"turn","status":"inProgress"}
        }}))
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn codex_mcp_progress_updates_existing_call_without_retaining_progress() {
    use crate::agent::timeline::Timeline;
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir().unwrap();
    let (session, mut events) = open_test_app_server(root.path()).await;
    start_codex_event_turn(&session, false).await;
    assert_eq!(events.try_recv().unwrap()["type"], "turn_started");
    let progress = |turn: &str, message: &str| {
        json!({"method":"item/mcpToolCall/progress","params":{
            "threadId":"thread","turnId":turn,"itemId":"call","message":message
        }})
    };
    session.handle_frame(progress("turn", "unknown owner")).await.unwrap();
    assert!(events.try_recv().is_err());
    let mut item = json!({"type":"mcpToolCall","id":"call","server":"remote","tool":"browse",
        "status":"inProgress","arguments":{"query":"needle"},"result":null,"error":null});
    let frame = |method, item: &Value| json!({"method":method,"params":{
        "threadId":"thread","turnId":"turn","item":item
    }});
    session.handle_frame(frame("item/started", &item)).await.unwrap();
    let started = events.try_recv().unwrap();
    let mut timeline = Timeline::new();
    timeline.append(started["item"].clone(), &started);
    session.handle_frame(progress("old-turn", "wrong scope")).await.unwrap();
    assert!(events.try_recv().is_err());
    for message in ["Connecting", "Reading results"] {
        session.handle_frame(progress("turn", message)).await.unwrap();
        let update = events.try_recv().unwrap();
        assert_eq!(update["item"]["callId"], started["item"]["callId"]);
        assert_eq!(update["item"]["name"], "remote.browse");
        assert_eq!(update["item"]["status"], "running");
        assert!(update["item"].get("detail").is_none());
        assert_eq!(update["item"]["metadata"]["progressMessage"], message);
        timeline.append(update["item"].clone(), &update);
        assert_eq!(timeline.items()[0]["detail"], started["item"]["detail"]);
    }
    session.handle_frame(progress("turn", "Reading results")).await.unwrap();
    assert!(events.try_recv().is_err());
    item["status"] = json!("completed");
    item["result"] = json!({"content":[{"type":"text","text":"real result"}]});
    session.handle_frame(frame("item/completed", &item)).await.unwrap();
    let completed = events.try_recv().unwrap();
    timeline.append(completed["item"].clone(), &completed);
    let rows = timeline.items();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["status"], "completed");
    assert_eq!(rows[0]["detail"]["output"], item["result"]);
    assert!(rows[0]["metadata"]["progressMessage"].is_null());
    session.handle_frame(progress("turn", "late")).await.unwrap();
    assert!(events.try_recv().is_err());
    session.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn codex_plan_preview_yields_to_authoritative_text_and_approval_on_same_row() {
    use crate::agent::timeline::Timeline;
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir().unwrap();
    let (session, mut events) = open_test_app_server(root.path()).await;
    start_codex_event_turn(&session, true).await;
    assert_eq!(events.try_recv().unwrap()["type"], "turn_started");
    let delta = |id, text| json!({"method":"item/plan/delta","params":{
        "threadId":"thread","turnId":"turn","itemId":id,"delta":text
    }});
    let mut timeline = Timeline::new();
    for (chunk, expected) in [("Draft", "Draft"), (" preview", "Draft preview")] {
        session.handle_frame(delta("plan", chunk)).await.unwrap();
        let update = events.try_recv().unwrap();
        assert_eq!(update["item"]["status"], "running");
        assert_eq!(update["item"]["detail"]["text"], expected);
        timeline.append(update["item"].clone(), &update);
    }
    session.handle_frame(json!({"method":"item/completed","params":{
        "threadId":"thread","turnId":"turn","item":{"type":"plan","id":"plan","text":"Final plan"}
    }})).await.unwrap();
    let completed = events.try_recv().unwrap();
    assert_eq!(completed["item"]["status"], "completed");
    assert_eq!(completed["item"]["detail"]["text"], "Final plan");
    timeline.append(completed["item"].clone(), &completed);
    session.handle_frame(delta("plan", " late")).await.unwrap();
    assert!(events.try_recv().is_err());
    session.handle_frame(json!({"method":"turn/plan/updated","params":{
        "threadId":"thread","turnId":"turn","explanation":null,
        "plan":[{"step":"Checklist is not the proposal","status":"completed"}]
    }})).await.unwrap();
    session.handle_frame(json!({"method":"turn/completed","params":{
        "threadId":"thread","turn":{"id":"turn","status":"completed"}
    }})).await.unwrap();
    let approval = events.try_recv().unwrap();
    assert_eq!(approval["item"]["name"], "plan_approval");
    assert_eq!(approval["item"]["detail"]["text"], "Final plan");
    timeline.append(approval["item"].clone(), &approval);
    let requested = events.try_recv().unwrap();
    assert_eq!(requested["request"]["input"]["plan"], "Final plan");
    assert_eq!(events.try_recv().unwrap()["type"], "turn_completed");
    session.respond_to_permission(
        requested["request"]["id"].as_str().unwrap(),
        json!({"behavior":"deny"}),
    ).await.unwrap();
    let dismissed = events.try_recv().unwrap();
    timeline.append(dismissed["item"].clone(), &dismissed);
    assert_eq!(timeline.items().len(), 1);
    assert_eq!(timeline.items()[0]["callId"], "plan");
    assert_eq!(timeline.items()[0]["metadata"]["approved"], false);
    session.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn codex_moderation_buffers_and_retracts_scoped_answers_without_raw_notices() {
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir().unwrap();
    let (session, mut events) = open_test_app_server(root.path()).await;
    start_codex_event_turn(&session, false).await;
    assert_eq!(events.try_recv().unwrap()["type"], "turn_started");
    let delta = |id, text| json!({"method":"item/agentMessage/delta","params":{
        "threadId":"thread","turnId":"turn","itemId":id,"delta":text
    }});
    let complete = |item| json!({"method":"item/completed","params":{
        "threadId":"thread","turnId":"turn","item":item
    }});
    let blocked = json!({"type":"agentMessage","id":"blocked","text":"withheld text",
        "moderation":{"moderated":true,"display":{"mode":"block","version":1,"reason":{"kind":"cyber"}}}});
    session.handle_frame(delta("blocked", "partial text")).await.unwrap();
    assert_eq!(events.try_recv().unwrap()["item"]["text"], "partial text");
    session.handle_frame(complete(blocked.clone())).await.unwrap();
    let correction = events.try_recv().unwrap();
    assert_eq!(correction["type"], "timeline_reconcile");
    assert_eq!(correction["turnId"], "local-turn");
    assert_eq!(correction["messageId"], "blocked");
    assert!(!correction.to_string().contains("withheld text"));
    assert_eq!(correction["item"], projection::item(&blocked, false, None).unwrap());
    session.handle_frame(delta("blocked", "late")).await.unwrap();
    assert!(events.try_recv().is_err());
    let info = json!({"method":"turn/moderationMetadata","params":{
        "threadId":"thread","turnId":"turn","metadata":{"opaque":"private moderation detail"}
    }});
    session.handle_frame(info.clone()).await.unwrap();
    assert_eq!(
        events.try_recv().unwrap()["item"]["message"],
        "Codex reported response moderation."
    );
    session.handle_frame(info).await.unwrap();
    assert!(events.try_recv().is_err());
    let buffer = |show| json!({"method":"model/safetyBuffering/updated","params":{
        "threadId":"thread","turnId":"turn","showBufferingUi":show
    }});
    session.handle_frame(buffer(true)).await.unwrap();
    assert_eq!(events.try_recv().unwrap()["item"]["type"], "notification");
    session.handle_frame(delta("safe", "buffered")).await.unwrap();
    session.handle_frame(buffer(false)).await.unwrap();
    session.handle_frame(delta("safe", " suffix")).await.unwrap();
    assert!(events.try_recv().is_err());
    session.handle_frame(complete(json!({"type":"agentMessage","id":"safe","text":"Approved answer",
        "moderation":{"moderated":true,"display":{"mode":"show","version":1,
            "annotation":{"kind":"wellbeingHelpline","subject":"answer"},"trustedContacts":"skip"}}
    }))).await.unwrap();
    assert_eq!(events.try_recv().unwrap()["item"]["text"], "Approved answer");
    assert!(
        events.try_recv().unwrap()["item"]["message"]
            .as_str().unwrap().contains("crisis helpline")
    );
    let hidden = projection::item(&json!({"type":"agentMessage","id":"hidden","text":"not rendered",
        "moderation":{"moderated":true,"display":{"mode":"hide","version":1,"replacement":{"kind":"wellbeingPromptHelpline"}}}
    }), false, None).unwrap();
    assert!(hidden["message"].as_str().unwrap().contains("crisis helpline"));
    assert!(!hidden.to_string().contains("not rendered"));
    session.close().await.unwrap();
}

#[cfg(unix)]
async fn open_test_app_server(
    root: &std::path::Path,
) -> (
    std::sync::Arc<session::CodexSession>,
    tokio::sync::mpsc::Receiver<Value>,
) {
    use crate::providers::api::{EventSink, LaunchContext, ProviderSpec};
    use tokio_util::sync::CancellationToken;

    let script = r#"
if [ "$2" = --version ]; then
    echo 'codex 0.171.0'
    exit
fi
while IFS= read -r frame; do
    case "$frame" in
        *'"method":"initialized"'*) ;;
        *'"method":'*)
            id=${frame#*\"id\":}
            printf '{"id":%s,"result":{"data":[],"config":{},"userAgent":"codex/0.171.0"}}\n' "${id%%,*}"
            ;;
        *) printf '%s\n' "$frame" > "$1" ;;
    esac
done
"#;
    let spec = ProviderSpec {
        id: "codex".into(),
        family: "codex".into(),
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            script.into(),
            "paseo-codex-test".into(),
            root.join("reply.json").to_string_lossy().into_owned(),
        ],
        env: [
            ("HOME".into(), root.to_string_lossy().into_owned()),
            ("CODEX_HOME".into(), root.to_string_lossy().into_owned()),
        ]
        .into(),
        options: json!({}),
        defaults: json!({}),
        runtime_settings: json!({}),
        configured_models: vec![],
    };
    let cancel = CancellationToken::new();
    let (sink, events) = EventSink::channel(32, cancel.clone());
    let context = LaunchContext {
        agent_id: "codex-test".into(),
        home: root.into(),
        executable: "/bin/sh".into(),
        env: Default::default(),
        mcp_servers: Value::Null,
        tool_catalog: Value::Null,
        tool_manifest: Value::Null,
        tool_executor: None,
        cancel,
    };
    let session = session::CodexSession::open(
        spec,
        json!({"cwd":root}),
        context,
        json!({}),
        None,
        sink,
        &super::metadata::MetadataCache::default(),
    )
    .await
    .unwrap();
    (session, events)
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_elicitation_round_trips_form_openai_form_and_url() -> anyhow::Result<()> {
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir()?;
    let (session, mut events) = open_test_app_server(root.path()).await;
    let file = root.path().join("reply.json");
    let method = "mcpServer/elicitation/request";
    for (id, params, response, expected) in [
        (
            json!(7),
            json!({"mode":"form","requestedSchema":{"type":"object","required":["count","enabled"],"properties":{
                "count":{"type":"integer","minimum":1,"maximum":4},
                "enabled":{"type":"boolean"},
                "opt":{"type":"string","default":"not a user answer"}
            }}}),
            json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"selections":{
                "count":{"options":[],"other":"3"},
                "enabled":{"options":["False"]}
            }}}),
            json!({"action":"accept","content":{"count":3,"enabled":false},"_meta":null}),
        ),
        (
            json!("007"),
            json!({"mode":"openaiForm","requestedSchema":{"type":"object","required":["template"],"properties":{
                "template":{"type":"openai/imagePicker","title":"Template","items":[
                    {"id":"monthly-review","title":"Monthly review","image":"data:image/png;base64,AA=="}
                ]}
            }}}),
            json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"selections":{
                "template":{"options":["Monthly review"]}
            }}}),
            json!({"action":"accept","content":{"template":"monthly-review"},"_meta":null}),
        ),
        (
            json!(8),
            json!({"mode":"openai/form","requestedSchema":{"type":"object","required":["template"],"properties":{
                "template":{"type":"string","oneOf":[
                    {"const":"monthly-review","title":"Monthly review","x-openai-preview":{"src":"data:image/png;base64,AA=="}}
                ]}
            }}}),
            json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"content":{"template":"monthly-review"}}}),
            json!({"action":"accept","content":{"template":"monthly-review"},"_meta":null}),
        ),
        (
            json!(9),
            json!({"mode":"url","url":"https://example.test/authorize?a=1","elicitationId":"oauth"}),
            json!({"behavior":"allow","selectedActionId":"accept"}),
            json!({"action":"accept","content":null,"_meta":null}),
        ),
        (
            json!(10),
            json!({"mode":"url","url":"https://example.test/authorize","elicitationId":"oauth"}),
            json!({"behavior":"deny","selectedActionId":"decline"}),
            json!({"action":"decline","content":null,"_meta":null}),
        ),
        (
            json!(11),
            json!({"mode":"url","url":"https://example.test/authorize","elicitationId":"oauth"}),
            json!({"behavior":"deny","selectedActionId":"cancel","interrupt":true}),
            json!({"action":"cancel","content":null,"_meta":null}),
        ),
    ] {
        let mut params = params;
        params["threadId"] = json!("thread");
        params["serverName"] = json!("browser");
        params["message"] = json!("Choose an action");
        session.handle_frame(json!({"id":id,"method":method,"params":params})).await?;
        let event = events.try_recv()?;
        assert_eq!(event["type"], "permission_requested");
        let request = &event["request"];
        assert_eq!(request["metadata"]["mcpElicitation"], true);
        assert_eq!(request["description"], "Choose an action");
        let public_id = request["id"].as_str().unwrap();
        if request["kind"] == "question" {
            assert!(session.respond_to_permission(public_id, json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"selections":{}}})).await.is_err());
            assert_eq!(session.pending_permissions()[0]["id"], public_id);
        } else {
            assert_eq!(request["input"]["url"], params["url"]);
        }
        assert!(session.respond_to_permission(public_id, json!({"behavior":"allow"})).await.is_err());
        assert!(!file.exists(), "request and invalid answers must not send a native reply");
        session.respond_to_permission(public_id, response).await?;
        session.client.request("test/flush", json!({})).await?;
        let native: Value = serde_json::from_slice(&std::fs::read(&file)?)?;
        assert_eq!(native, json!({"id":id,"result":expected}));
        std::fs::remove_file(&file)?;
        let resolved = events.try_recv()?;
        assert_eq!(resolved["type"], "permission_resolved");
        assert_eq!(resolved["requestId"], public_id);
        assert!(resolved["resolution"].get("updatedInput").is_none());
    }
    session.handle_frame(json!({"id":99,"method":method,"params":{
        "threadId":"thread","serverName":"browser","message":"Open it?","mode":"url",
        "url":"https://example.test/authorize","elicitationId":"oauth"
    }})).await?;
    let pending = events.try_recv()?;
    session.handle_frame(json!({"method":"serverRequest/resolved","params":{"requestId":99,"threadId":"other"}})).await?;
    assert_eq!(session.pending_permissions().len(), 1);
    session.handle_frame(json!({"method":"serverRequest/resolved","params":{"requestId":99,"threadId":"thread"}})).await?;
    let resolved = events.try_recv()?;
    assert_eq!(resolved["requestId"], pending["request"]["id"]);
    assert_eq!(resolved["resolution"]["selectedActionId"], "cancel");
    session.client.request("test/flush", json!({})).await?;
    assert!(!file.exists(), "server resolution must not send another reply");
    assert!(session.pending_permissions().is_empty());
    session.close().await
}

#[cfg(unix)]
#[tokio::test]
async fn native_approval_replies_echo_string_and_integer_ids_without_coercion() {
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir().unwrap();
    let (session, mut events) = open_test_app_server(root.path()).await;
    let profile = json!({"network":{"enabled":true},"fileSystem":{"entries":[
        {"access":"write","path":{"type":"path","path":"/work/cache"}}
    ]}});
    for (id, response, result) in [
        (
            json!("007"),
            json!({"behavior":"allow","selectedActionId":"accept_session"}),
            json!({"permissions":profile,"scope":"session"}),
        ),
        (
            json!(7),
            json!({"behavior":"allow","selectedActionId":"accept"}),
            json!({"permissions":profile,"scope":"turn"}),
        ),
        (
            json!(-7),
            json!({"behavior":"deny","selectedActionId":"decline"}),
            json!({"permissions":{},"scope":"turn"}),
        ),
    ] {
        session
            .handle_frame(json!({"id":id,"method":"item/permissions/requestApproval","params":{
                "threadId":"thread","turnId":"turn","itemId":"access","cwd":"/work","startedAtMs":0,
                "permissions":profile,"reason":"Package cache access",
            }}))
            .await
            .unwrap();
        let event = events.try_recv().unwrap();
        assert_eq!(event["type"], "permission_requested");
        let request = &event["request"];
        assert_eq!(request["description"], "Package cache access");
        let text = request["detail"]["text"].as_str().unwrap();
        for expected in [
            "Working directory: /work",
            "Network access:",
            "Write: /work/cache",
        ] {
            assert!(text.contains(expected));
        }
        assert!(
            request["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["id"] == response["selectedActionId"])
        );
        session
            .respond_to_permission(request["id"].as_str().unwrap(), response)
            .await
            .unwrap();
        // The stub answers this after reading and recording the previous reply.
        session
            .client
            .request("test/flush", json!({}))
            .await
            .unwrap();
        let reply: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("reply.json")).unwrap())
                .unwrap();
        assert_eq!(reply, json!({"id":id,"result":result}));
        while events.try_recv().is_ok() {}
    }
    session.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn retries_are_visible_and_terminal_errors_only_surface_once() {
    use crate::providers::api::AgentSession;

    let root = tempfile::tempdir().unwrap();
    let (session, mut events) = open_test_app_server(root.path()).await;
    session.state.lock().unwrap().thread = Some("thread".into());
    session
        .handle_frame(json!({"method":"turn/started","params":{
            "threadId":"thread","turn":{"id":"turn","status":"inProgress"}
        }}))
        .await
        .unwrap();
    assert_eq!(events.try_recv().unwrap()["type"], "turn_started");
    let error = json!({"message":"Stream disconnected","additionalDetails":"Connection reset\nextra context"});
    let frame = |will_retry| {
        json!({"method":"error","params":{
            "threadId":"thread","turnId":"turn","error":error,"willRetry":will_retry
        }})
    };
    session.handle_frame(frame(true)).await.unwrap();
    let retry = events.try_recv().unwrap();
    assert_eq!(retry["item"]["level"], "warning");
    let message = retry["item"]["message"].as_str().unwrap();
    assert!(message.contains("Stream disconnected") && message.contains("Connection reset"));
    session.handle_frame(frame(false)).await.unwrap();
    assert!(events.try_recv().is_err());
    session
        .handle_frame(json!({"method":"turn/completed","params":{
            "threadId":"thread","turn":{"id":"turn","status":"failed","error":error}
        }}))
        .await
        .unwrap();
    let failed = events.try_recv().unwrap();
    assert_eq!(failed["type"], "turn_failed");
    assert_eq!(failed["error"], error["message"]);
    assert!(events.try_recv().is_err());
    session.handle_frame(frame(false)).await.unwrap();
    let idle = events.try_recv().unwrap();
    assert_eq!(idle["item"]["level"], "error");
    assert!(
        idle["item"]["message"]
            .as_str()
            .unwrap()
            .contains("Stream disconnected")
    );
    session.close().await.unwrap();
}

#[test]
fn multi_select_answers_keep_labels_that_contain_commas() {
    let params = json!({"threadId":"thread","turnId":"native-turn","itemId":"tool","questions":[
        {"id":"q","header":"Pick","question":"Which?","multiSelect":true,"isOther":true,
         "options":[{"label":"SQLite: one file, no service"},{"label":"Redis"}]}
    ]});
    let pending = permissions::request("codex", json!(8), "tool/requestUserInput", params).unwrap();
    let legacy = json!({"behavior":"allow","updatedInput":{"answers":{"Pick":"SQLite: one file, no service, Redis, my, own"}}});
    assert_eq!(
        permissions::reply(&pending, &legacy).unwrap(),
        json!({"answers":{"q":{"answers":["SQLite: one file, no service","Redis","my, own"]}}})
    );
    let structured = json!({"behavior":"allow","updatedInput":{
        "answers":{"Pick":"ignored"},
        "selections":{"Pick":{"options":["SQLite: one file, no service"],"other":""}}
    }});
    assert_eq!(
        permissions::reply(&pending, &structured).unwrap(),
        json!({"answers":{"q":{"answers":["SQLite: one file, no service"]}}})
    );
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
        thread(vec![
            message("p1", "parent text"),
            spawn("spawn", &["child-a", "child-b"], "Review"),
        ]),
    );
    let mut child = vec![message("a1", "child a text")];
    child.extend((0..120).map(|n| message(&format!("a-row-{n}"), &format!("row {n}"))));
    child.push(spawn("nested", &["grandchild"], "Dig deeper"));
    threads.replies.insert("child-a".into(), thread(child));
    threads.replies.insert(
        "child-b".into(),
        thread(vec![message("b1", "child b text")]),
    );
    threads.replies.insert(
        "grandchild".into(),
        thread(vec![message("g1", "grandchild text")]),
    );
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
    use crate::providers::api::{
        ChildRowHold, EventSink, HistoryCoverage, ReplayOutcome, SubagentReplay,
    };
    let row = |id: &str, text: &str| {
        json!({"type":"provider_subagent","provider":"codex","event":{"type":"timeline","id":"child-b",
            "item":{"type":"assistant_message","text":text,"messageId":id}}})
    };
    for outcome in [ReplayOutcome::Applied, ReplayOutcome::Retained] {
        let (sink, mut published) =
            EventSink::channel(64, tokio_util::sync::CancellationToken::new());
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
                    snapshot
                        .iter()
                        .any(|row| row["messageId"] == item["messageId"])
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
        assert_eq!(
            marker["event"]["historyComplete"], false,
            "a running child is never authoritative"
        );
        assert_eq!(
            marker["event"]["events"][0]["event"]["item"]["text"],
            "done"
        );
        assert!(
            published.try_recv().is_err(),
            "held rows wait for the daemon"
        );
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
            ReplayOutcome::Applied => {
                assert_eq!(rest, ["new"], "the applied snapshot already has b1")
            }
            ReplayOutcome::Retained => {
                assert_eq!(rest, ["done", "new"], "kept state continues as it was")
            }
        }
        // The hold is lifted: later rows flow straight through.
        hold.emit(&sink, row("b3", "after")).await.unwrap();
        assert_eq!(
            published.recv().await.unwrap()["event"]["item"]["text"],
            "after"
        );
    }
}

#[test]
fn a_child_not_recorded_as_finished_never_gets_an_authoritative_replay() {
    use crate::providers::api::HistoryCoverage;
    let rows = || {
        vec![crate::providers::api::subagent_row(
            "codex",
            "child-b",
            json!({"type":"assistant_message","text":"x"}),
            None,
            HistoryCoverage::Complete,
        )]
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
    assert!(
        coverage == HistoryCoverage::Unverified,
        "a partial read stays partial"
    );
    assert_eq!(events[0]["event"]["historyComplete"], false);
}

#[tokio::test]
async fn parent_history_announces_children_without_reading_their_threads() {
    let threads = family();
    let history = session::load_history(&threads, "parent", None, "codex")
        .await
        .unwrap();
    assert_eq!(*threads.reads.lock().unwrap(), ["parent"]);
    let children: Vec<_> = history
        .iter()
        .filter(|event| event["type"] == "provider_subagent")
        .map(|event| &event["event"])
        .collect();
    assert!(
        children.iter().all(|event| event["type"] == "upsert"),
        "{children:?}"
    );
    let ids: Vec<_> = children
        .iter()
        .map(|event| event["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["child-a", "child-b"]);
    assert_eq!(children[0]["source"], json!({"threadId":"child-a"}));
    assert_eq!(children[0]["toolCallId"], "spawn");
    assert_eq!(children[0]["parentSubagentId"], Value::Null);
    assert!(
        history
            .iter()
            .any(|event| event["item"]["text"] == "parent text")
    );
}

#[tokio::test]
async fn opened_child_reads_only_its_thread_and_announces_grandchildren() {
    let threads = family();
    let (events, coverage) = session::subagent_history(
        &threads,
        &request("child-a", json!({"threadId":"child-a"})),
        "codex",
        false,
    )
    .await
    .unwrap();
    assert!(coverage == crate::providers::api::HistoryCoverage::Complete);
    assert_eq!(*threads.reads.lock().unwrap(), ["child-a"]);
    let rows: Vec<_> = events
        .iter()
        .filter(|e| e["event"]["type"] == "timeline")
        .collect();
    assert_eq!(
        rows.len(),
        122,
        "every row of the selected child is replayed for paging"
    );
    assert!(rows.iter().all(|row| row["event"]["id"] == "child-a"
        && row["event"]["historyComplete"] == true
        && row["type"] == "provider_subagent"));
    assert_eq!(rows[0]["event"]["item"]["text"], "child a text");
    let nested: Vec<_> = events
        .iter()
        .filter(|e| e["event"]["type"] == "upsert")
        .collect();
    assert_eq!(nested.len(), 1);
    assert_eq!(nested[0]["event"]["id"], "grandchild");
    assert_eq!(nested[0]["event"]["parentSubagentId"], "child-a");
    assert_eq!(
        nested[0]["event"]["source"],
        json!({"threadId":"grandchild"})
    );
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
    assert!(
        first.is_err() != second.is_err(),
        "exactly one concurrent read saw the failure"
    );
    let succeeded = first.or(second).unwrap().0;
    let retried = session::subagent_history(&threads, &selected, "codex", false)
        .await
        .unwrap()
        .0;
    assert_eq!(
        succeeded, retried,
        "a failure is never cached as the child's history"
    );
    assert_eq!(
        *threads.reads.lock().unwrap(),
        ["child-b", "child-b", "child-b"]
    );
    let missing =
        session::subagent_history(&threads, &request("gone", Value::Null), "codex", false).await;
    assert!(
        missing
            .unwrap_err()
            .to_string()
            .contains("no rollout found")
    );
    let mut torn = family();
    torn.replies.insert(
        "child-a".into(),
        json!({"thread":{"turns":[{"id":"turn"}]}}),
    );
    let incomplete =
        session::subagent_history(&torn, &request("child-a", Value::Null), "codex", false).await;
    assert!(
        incomplete.is_err(),
        "an incomplete thread is not an empty replay"
    );
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
        session::subagent_history(&threads, &request("child-b", Value::Null), "codex", false)
            .await
            .unwrap();
    let texts: Vec<_> = events
        .iter()
        .filter_map(|event| event["event"]["item"]["text"].as_str())
        .collect();
    assert_eq!(
        texts,
        ["partial snapshot"],
        "a cold read has no live stream to supply this turn"
    );
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
        session::subagent_history(&threads, &request("child-b", Value::Null), "codex", true)
            .await
            .unwrap();
    assert!(
        coverage == crate::providers::api::HistoryCoverage::Unverified,
        "a growing child cannot be replayed authoritatively"
    );
    let texts: Vec<_> = events
        .iter()
        .map(|e| e["event"]["item"]["text"].as_str().unwrap())
        .collect();
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
    let mut history = session::load_history(threads, "parent", None, "codex")
        .await
        .unwrap();
    let logs = session::card_logs(options);
    session::apply_card_logs(&mut history, &logs);
    let mut state = session::State::default();
    for event in history
        .iter()
        .filter(|event| event["type"] == "provider_subagent")
    {
        let id = event["event"]["id"].as_str().unwrap().to_owned();
        let card = history
            .iter()
            .find(|e| {
                e["type"] == "timeline" && e["item"]["callId"] == event["event"]["toolCallId"]
            })
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
        CodexSession::aggregate(&mut state, &[live("new progress", "b2")], Some("b2"), true)
            .unwrap();
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
    assert_eq!(
        *threads.reads.lock().unwrap(),
        ["parent"],
        "no child thread is read before its pane opens"
    );
    session::subagent_history(&threads, &request("child-b", Value::Null), "codex", false)
        .await
        .unwrap();
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
        CodexSession::aggregate(&mut state, &[live("new progress", "b2")], Some("b2"), true)
            .unwrap();
    assert_eq!(
        updates[0]["item"]["detail"]["log"],
        "[Assistant] new progress"
    );
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
    assert_eq!(
        card["detail"]["log"],
        "[Assistant] First.\n[Assistant] Hello"
    );
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
        assert_eq!(
            replayed["item"]["metadata"]["subagentLog"],
            card["metadata"]["subagentLog"]
        );
        // The same native item continues: by its next delta, or by its full text.
        let continued = if full {
            CodexSession::aggregate(&mut state, &[live("Hello world", "b2")], Some("b2"), false)
        } else {
            CodexSession::aggregate(&mut state, &[live("world", "b2")], Some("b2"), true)
        }
        .unwrap();
        assert_eq!(
            continued[0]["item"]["detail"]["log"], "[Assistant] First.\n[Assistant] Hello world",
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
    assert_eq!(
        *threads.reads.lock().unwrap(),
        ["parent", "parent", "parent"],
        "no child thread read"
    );
}
