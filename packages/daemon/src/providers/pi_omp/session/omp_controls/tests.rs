use super::*;
use crate::providers::api::{AgentSession, EventSink, LaunchContext, ProviderSpec};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    collections::BTreeMap,
    io::{self, BufRead},
    path::Path,
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const FIXTURE_FILTER: &str = "omp_controls::tests::fake_native_omp";

#[test]
#[ignore = "subprocess protocol fixture"]
fn fake_native_omp() {
    let Some(root) = std::env::var_os("PASEO_FAKE_OMP_ROOT").map(std::path::PathBuf::from) else {
        return;
    };
    let args = std::env::var("PASEO_FAKE_OMP_ARGS").unwrap();
    let arg = |flag| args.lines().skip_while(|part| *part != flag).nth(1);
    let secondary = arg("--session").is_some();
    if args.lines().any(|arg| {
        matches!(arg, "--no-ui" | "--no-extensions" | "--trusted-extension")
            || arg.starts_with("--trusted-extension=")
    }) {
        assert!(
            arg("--extension").is_none(),
            "must not inject the account extension"
        );
    }
    let bridge = arg("--extension")
        .and_then(|path| {
            let source = std::fs::read_to_string(path).unwrap();
            source
                .lines()
                .find_map(|line| line.strip_prefix("const command = \"")?.strip_suffix("\";"))
                .map(str::to_owned)
        })
        .filter(|_| std::env::var_os("PASEO_FAKE_OMP_NO_ACCOUNTS").is_none());
    let mut native_id = "source";
    let mut file = root.join("source.jsonl");
    let mut provider = "kiro";
    let mut level = "low";
    let mut fast = false;
    if let Some(path) = arg("--session").filter(|path| path.ends_with("imported-fork.jsonl")) {
        assert_eq!(arg("--approval-mode"), Some("always-ask"));
        assert_eq!(arg("--thinking"), Some("low"));
        native_id = "imported-fork";
        file = path.into();
        provider = "initial";
        level = arg("--thinking").unwrap();
    }
    let mut account = Some(22);
    let mut signed_out = false;
    let mut models_changed = false;
    let mut login = None;
    let mut bash = None;
    let mut removed = false;
    let mut promoted = false;
    let mut todo_phases = json!([]);
    let notice =
        |id, text| json!({"type":"extension_ui_request","method":"notify","id":id,"message":text});
    let respond = |q: &Value, data: Value| {
        println!(
            "{}",
            json!({"type":"response","command":q["type"],"id":q["id"],"success":true,"data":data})
        )
    };
    let error = |q: &Value, message: &str| {
        println!(
            "{}",
            json!({"type":"response","command":q["type"],"id":q["id"],"success":false,"error":message})
        )
    };
    let command_list = || {
        let mut commands = vec![
            json!({"name":"todo","source":"builtin"}),
            json!({"name":"switch","source":"builtin"}),
            json!({"name":"kiro-accounts","source":"extension"}),
            json!({"name":"session","source":"builtin"}),
        ];
        if let Some(bridge) = &bridge {
            commands.push(json!({"name":bridge,"source":"extension"}));
        }
        commands
    };
    println!("{}", json!({"type":"ready"}));
    for line in io::stdin().lock().lines() {
        let q: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let native_command = q["type"].as_str().unwrap();
        match native_command {
            "get_state" => {
                let steering = if removed {
                    json!(["claimed", "same", "same"])
                } else {
                    json!(["claimed", "same", "same", "unique"])
                };
                let follow_up = if promoted {
                    json!(["other"])
                } else {
                    json!(["other", "promote me"])
                };
                let mut state = json!({"sessionId":native_id,"sessionFile":file,"model":{"provider":provider,"id":"one","reasoning":true},"isSettled":true,"isStreaming":false,"isCompacting":false,"thinkingLevel":level,
                    "steeringMode":"all","followUpMode":"one-at-a-time","queuedMessages":{"steering":steering,"followUp":follow_up,"liveSteered":1},"todoPhases":todo_phases});
                if native_id == "imported-fork" {
                    state["fastModeEnabled"] = json!(fast);
                    state["fastModeActive"] = json!(fast);
                }
                respond(&q, state);
            }
            "set_event_filter" => respond(&q, json!({"messageUpdates":"delta"})),
            "get_available_commands" => respond(&q, json!({"commands":command_list()})),
            "get_available_models" => {
                let mut models = vec![json!({"provider":provider,"id":"one","name":"One"})];
                if native_id == "imported-fork" {
                    models.push(json!({"provider":"kiro","id":"one","name":"Inherited"}));
                }
                if models_changed {
                    models.push(
                        json!({"provider":"kiro-work-team","id":"two","name":"Pinned model"}),
                    );
                }
                respond(&q, json!({"models":models}));
            }
            "set_model" if native_id == "imported-fork" => {
                assert_eq!(q["provider"], "kiro");
                assert_eq!(q["modelId"], "one");
                provider = "kiro";
                respond(&q, Value::Null);
            }
            "set_fast_mode" if native_id == "imported-fork" => {
                assert_eq!(q["enabled"], true);
                fast = true;
                respond(&q, json!({"enabled":fast,"active":fast}));
            }
            "get_login_providers" => respond(
                &q,
                json!({"providers":[{"id":"kiro","name":"Kiro","available":true}]}),
            ),
            "get_logout_accounts" => {
                assert_eq!(q["providerId"], "kiro");
                respond(
                    &q,
                    json!({"accounts":[{"credentialId":22,"type":"oauth","label":"Second account","active":true}]}),
                );
            }
            "login" if q["providerId"] == "kiro" => {
                println!(
                    "{}",
                    json!({"type":"extension_ui_request","method":"open_url","id":"open","url":"https://example.invalid/login"})
                );
                println!(
                    "{}",
                    json!({"type":"extension_ui_request","method":"input","id":"paste","title":"Paste redirect URL","placeholder":"URL"})
                );
                login = Some(q);
            }
            "login" => error(
                &q,
                "This provider requires a secret input unsupported in RPC mode",
            ),
            "extension_ui_response" => {
                assert_eq!(q["id"], "paste");
                assert_eq!(q["value"], "fixture-auth-code");
                models_changed = true;
                respond(&login.take().unwrap(), json!({"providerId":"kiro"}));
            }
            "logout" => {
                assert_eq!(q["credentialId"], 22);
                account = None;
                signed_out = true;
                models_changed = false;
                respond(&q, json!({}));
            }
            "prompt"
                if q["message"] == "prompt during shell"
                    || q["message"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("<paseo-system ")) =>
            {
                let message = q["message"].as_str().unwrap();
                if message == "prompt during shell" {
                    assert!(bash.is_some(), "shell must still be awaiting a stop");
                } else {
                    assert!(message.contains("source-agent-id="));
                    assert!(message.contains("/queue"));
                    println!(
                        "{}",
                        notice(
                            "provenance",
                            "Native fixture received the agent envelope".into()
                        )
                    );
                }
                respond(&q, json!({"agentInvoked":true}));
                println!("{}", json!({"type":"agent_start"}));
                println!(
                    "{}",
                    json!({"type":"message_end","message":{"role":"user","content":message,"id":"fake-user"}})
                );
                println!(
                    "{}",
                    json!({"type":"agent_end","messages":[{"role":"assistant","content":[],"stopReason":"stop"}]})
                );
                println!(
                    "{}",
                    json!({"type":"prompt_result","id":q["id"],"agentInvoked":true})
                );
            }
            "prompt" => {
                let (name, args) = slash(q["message"].as_str().unwrap()).unwrap();
                respond(&q, Value::Null);
                if bridge.as_deref() == Some(name) {
                    let payload: Value =
                        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(args).unwrap()).unwrap();
                    assert_eq!(payload["provider"], provider);
                    assert_eq!(payload["sessionId"], native_id);
                    if let Some(selected) = payload.get("credentialId") {
                        account = selected.as_u64();
                    }
                    let accounts = match provider {
                        "kiro" if !signed_out => {
                            json!([{"id":11,"label":"First account"},{"id":22,"label":"Second account"}])
                        }
                        "openai-codex" => json!([{"id":31,"label":"Codex account"}]),
                        _ => json!([]),
                    };
                    let result = json!({"requestId":payload["requestId"],"result":{"explicit":account,"accounts":accounts}});
                    // A command's own output precedes the reply that ends it.
                    println!(
                        "{}",
                        json!({"type":"command_output","text":"internal extension output"})
                    );
                    println!(
                        "{}",
                        notice("bridge", format!("PASEO_COMMAND_RESULT {result}"))
                    );
                } else {
                    match (name, args) {
                        ("switch", "anthropic/one") => {
                            provider = "anthropic";
                            println!(
                                "{}",
                                json!({"type":"config_update","model":{"provider":provider,"id":"one"},"thinkingLevel":level})
                            );
                        }
                        ("kiro-accounts", "") => {
                            models_changed = true;
                            println!(
                                "{}",
                                json!({"type":"available_commands_update","commands":command_list()})
                            );
                        }
                        ("kiro-profile", "next") => {
                            models_changed = false;
                        }
                        ("todo", "append check fixture") => {
                            todo_phases = json!([{"name":"Default","tasks":[{"content":"check fixture","status":"pending","notes":["preserve"]}]}]);
                            println!(
                                "{}",
                                json!({"type":"command_output","text":"Appended check fixture"})
                            );
                        }
                        _ => panic!("unexpected fake prompt: {q}"),
                    }
                }
                println!(
                    "{}",
                    json!({"type":"prompt_result","id":q["id"],"agentInvoked":false})
                );
            }
            "handoff" => match q["customInstructions"].as_str() {
                Some("in place") => {
                    // OMP 19: a handoff is a manual compaction in place.
                    println!(
                        "{}",
                        json!({"type":"manual_compaction_start","mode":"handoff"})
                    );
                    println!(
                        "{}",
                        json!({"type":"manual_compaction_end","aborted":false,"result":{"tokensBefore":42000}})
                    );
                    respond(&q, json!({}));
                }
                Some("invalid") => {
                    println!(
                        "{}",
                        json!({"type":"manual_compaction_start","mode":"handoff"})
                    );
                    println!(
                        "{}",
                        json!({"type":"manual_compaction_end","aborted":false,
                        "errorMessage":"Handoff generation produced an invalid document"})
                    );
                    error(&q, "Handoff generation produced an invalid document");
                }
                Some("busy") => error(&q, "Cannot hand off while a response is in progress"),
                Some("legacy") => {
                    native_id = "replacement";
                    file = root.join("replacement.jsonl");
                    std::fs::write(&file, "replacement transcript").unwrap();
                    respond(&q, json!({"savedPath":root.join("saved-plan.md")}));
                }
                _ => panic!("unexpected handoff request: {q}"),
            },
            "compact" => {
                println!("{}", json!({"type":"manual_compaction_start"}));
                println!(
                    "{}",
                    json!({"type":"manual_compaction_end","aborted":false,"skipped":true,
                    "errorMessage":"Nothing to compact (session too small)"})
                );
                error(&q, "Nothing to compact (session too small)");
            }
            "set_todos" => {
                todo_phases = q["phases"].clone();
                assert_eq!(todo_phases[0]["tasks"][0]["notes"], json!(["preserve"]));
                respond(&q, json!({"todoPhases":todo_phases}));
            }
            "remove_queued_message" => {
                assert_eq!(q["queue"], "steering");
                assert_eq!(q["message"], "unique");
                removed = true;
                respond(&q, json!({"removed":true}));
            }
            "promote_queued_message" => {
                assert_eq!(q["message"], "promote me");
                promoted = true;
                respond(&q, json!({"promoted":true}));
            }
            "set_steering_mode" => {
                assert_eq!(q["mode"], "all");
                respond(&q, Value::Null);
            }
            "set_follow_up_mode" => {
                assert_eq!(q["mode"], "one-at-a-time");
                respond(&q, Value::Null);
            }
            "set_auto_retry" => {
                assert_eq!(q["enabled"], false);
                respond(&q, Value::Null);
            }
            "get_subagents" => respond(
                &q,
                json!({"subagents":[{"id":"child-42","agent":"Scout","status":"running"}]}),
            ),
            "steer_subagent" => {
                assert_eq!(q["subagentId"], "child-42");
                assert_eq!(q["message"], "save  two spaces");
                respond(&q, Value::Null);
            }
            "cancel_subagent" => {
                assert_eq!(q["subagentId"], "child-42");
                respond(&q, json!({"cancelled":true}));
            }
            "bash" => {
                assert_eq!(q["command"], "fake shell command");
                assert_ne!(q["excludeFromContext"], true);
                bash = Some(q);
                println!("{}", notice("shell", "Shell started".into()));
            }
            "abort_bash" => {
                respond(&q, Value::Null);
                respond(
                    &bash.take().unwrap(),
                    json!({"output":"captured output","exitCode":130,"cancelled":true}),
                );
            }
            "cycle_model" => {
                provider = "openai-codex";
                account = None;
                respond(&q, json!({"model":{"provider":provider,"id":"one"}}));
            }
            "get_available_thinking_levels" => respond(&q, json!({"levels":["off","low","high"]})),
            "cycle_thinking_level" => {
                level = "high";
                respond(&q, json!({"level":level}));
            }
            "get_session_lease" => {
                assert!(!secondary);
                respond(&q, json!({"held":q["sessionIds"],"unknown":[]}));
            }
            "fork" => {
                assert!(secondary, "never run fork on the source runtime");
                native_id = "child";
                file = root.join("fork.jsonl");
                std::fs::write(&file, q["entryId"].as_str().unwrap_or("whole")).unwrap();
                respond(&q, json!({"cancelled":false}));
            }
            "abort_retry" | "set_subagent_subscription" => respond(&q, Value::Null),
            _ => error(&q, &format!("Unknown command: {native_command}")),
        }
    }
}

pub(crate) fn fixture_provider_spec(root: &Path) -> ProviderSpec {
    let path = root.to_str().unwrap();
    let exe = std::env::current_exe().unwrap();
    ProviderSpec {
        id: "configured-omp".into(),
        family: "omp".into(),
        command: vec!["sh".into(), "-c".into(), "export PASEO_FAKE_OMP_ARGS=\"$(printf '%s\\n' \"$@\")\"; exec \"$PASEO_FAKE_OMP_EXE\" \"$PASEO_FAKE_OMP_FILTER\" --nocapture --ignored".into(), "fake-omp".into()],
        env: BTreeMap::from([
            ("HOME".into(), path.into()),
            ("PI_CODING_AGENT_DIR".into(), path.into()),
            ("PASEO_FAKE_OMP_ROOT".into(), path.into()),
            ("PASEO_FAKE_OMP_EXE".into(), exe.to_str().unwrap().into()),
            ("PASEO_FAKE_OMP_FILTER".into(), FIXTURE_FILTER.into()),
        ]),
        options: json!({"rpcTimeoutMs":3000}), defaults: json!({}), runtime_settings: json!({}), configured_models: Vec::new(),
    }
}

async fn open_fixture(
    hide_accounts: bool,
) -> (tempfile::TempDir, Arc<Session>, mpsc::Receiver<Value>) {
    open_fixture_with_args(hide_accounts, &[]).await
}

async fn open_fixture_with_args(
    hide_accounts: bool,
    args: &[&str],
) -> (tempfile::TempDir, Arc<Session>, mpsc::Receiver<Value>) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().to_str().unwrap();
    tokio::fs::write(root.path().join("source.jsonl"), "source transcript")
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (sink, events) = EventSink::channel(128, cancel.clone());
    let exe = std::env::current_exe().unwrap();
    let mut spec = fixture_provider_spec(root.path());
    spec.command
        .extend(args.iter().map(|arg| (*arg).to_owned()));
    if hide_accounts {
        spec.env
            .insert("PASEO_FAKE_OMP_NO_ACCOUNTS".into(), "1".into());
    }
    let session = Session::open(
        spec,
        json!({"cwd":path}),
        LaunchContext {
            agent_id: "source-agent".into(),
            home: root.path().into(),
            executable: exe,
            env: BTreeMap::new(),
            mcp_servers: json!({}),
            tool_catalog: json!([]),
            tool_manifest: json!([]),
            tool_executor: None,
            cancel,
        },
        None,
        json!({}),
        sink,
        &launch::CommandProbes::default(),
    )
    .await
    .unwrap();
    (root, session, events)
}

async fn oob(session: &Arc<Session>, command: &str) -> Result<()> {
    session
        .clone()
        .prepare_out_of_band(&json!(command))
        .expect("Passession command")
        .await
}

async fn until(events: &mut mpsc::Receiver<Value>, predicate: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(9), async {
        loop {
            let event = events.recv().await.expect("session event");
            assert_ne!(event["item"]["text"], "internal extension output");
            if predicate(&event) {
                return event;
            }
        }
    })
    .await
    .expect("expected session event")
}

#[tokio::test]
async fn omp_native_controls_account_pin_auto_and_unsupported_runtime() {
    let (_root, session, mut events) = open_fixture(false).await;
    assert_eq!(session.features()[0]["value"], "kiro/22");
    assert_eq!(
        session.features()[0]["options"][1]["label"],
        "First account"
    );
    session
        .set_feature("oauth_account", json!("kiro/11"))
        .await
        .unwrap();
    assert_eq!(session.features()[0]["value"], "kiro/11");
    session
        .set_feature("oauth_account", json!("automatic"))
        .await
        .unwrap();
    assert_eq!(session.features()[0]["value"], "automatic");
    assert!(
        session
            .list_commands()
            .await
            .unwrap()
            .iter()
            .all(|c| !c["name"].as_str().unwrap().starts_with("paseo-account-"))
    );
    let usage = session.usage_session().unwrap();
    assert_eq!(usage["provider"], "configured-omp");
    assert_eq!(usage["providerFamily"], "omp");
    assert_eq!(usage["command"], json!(session.inner.spec.command));
    assert_eq!(
        until(&mut events, |e| e["type"] == "thread_started").await["sessionId"],
        "source"
    );
    assert!(
        events.try_recv().is_err(),
        "account commands must not publish rows or turns"
    );
    session.close().await.unwrap();
    let (_root, old, _events) = open_fixture(true).await;
    assert!(old.features().is_empty());
    assert!(
        old.set_feature("oauth_account", json!("automatic"))
            .await
            .is_err()
    );
    old.close().await.unwrap();
}

#[tokio::test]
async fn omp_account_bridge_skips_trusted_and_disabled_extension_launches() {
    for flags in [
        &["--trusted-extension", "/private/trusted.mjs"][..],
        &["--trusted-extension=/private/trusted.mjs"][..],
        &["--no-extensions"][..],
        &["--no-ui"][..],
    ] {
        let (_root, session, _events) =
            tokio::time::timeout(Duration::from_secs(5), open_fixture_with_args(false, flags))
                .await
                .expect("must not wait for an extension UI result");
        assert!(session.features().is_empty());
        assert!(session.inner.assets.lock().unwrap().is_none());
        tokio::time::timeout(Duration::from_secs(5), oob(&session, "/next-thinking"))
            .await
            .unwrap()
            .unwrap();
        assert!(session.features().is_empty());
        assert!(
            !session
                .inner
                .state
                .lock()
                .unwrap()
                .account_command_available
        );
        session.close().await.unwrap();
    }
    let (_root, session, _events) =
        tokio::time::timeout(Duration::from_secs(5), open_fixture(true))
            .await
            .expect("a runtime that does not advertise the UI bridge must not be probed");
    assert!(session.inner.assets.lock().unwrap().is_some());
    assert!(
        !session
            .inner
            .state
            .lock()
            .unwrap()
            .account_command_available
    );
    tokio::time::timeout(Duration::from_secs(5), oob(&session, "/next-thinking"))
        .await
        .unwrap()
        .unwrap();
    assert!(session.features().is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_login_dialog_logout_and_catalog_refresh() {
    let (_root, session, mut events) = open_fixture(false).await;
    oob(&session, "/login").await.unwrap();
    assert!(
        until(&mut events, |e| e["item"]["type"] == "notification").await["item"]["message"]
            .as_str()
            .unwrap()
            .contains("kiro — Kiro")
    );
    assert!(
        oob(&session, "/login unsupported")
            .await
            .unwrap_err()
            .to_string()
            .contains("secret input")
    );
    let login = tokio::spawn(
        session
            .clone()
            .prepare_out_of_band(&json!("/login kiro"))
            .unwrap(),
    );
    let link = until(&mut events, |e| e["item"]["type"] == "assistant_message").await;
    assert!(
        link["item"]["text"]
            .as_str()
            .unwrap()
            .contains("https://example.invalid/login")
    );
    let prompt = until(&mut events, |e| e["type"] == "permission_requested").await;
    assert_eq!(prompt["request"]["id"], "paste");
    let rejection = tokio::time::timeout(
        Duration::from_secs(2),
        session.start_turn(json!("not while logging in"), json!({})),
    )
    .await
    .expect("a competing prompt must not block the login response")
    .unwrap_err();
    assert!(rejection.to_string().contains("Wait for OMP login"));
    let rejection =
        tokio::time::timeout(Duration::from_secs(2), session.set_model(Some("kiro/one")))
            .await
            .expect("a competing model change must not block the login response")
            .unwrap_err();
    assert!(rejection.to_string().contains("Wait for OMP login"));
    session
        .respond_to_permission(
            "paste",
            json!({"behavior":"allow","updatedInput":{"answers":{"Response":"fixture-auth-code"}}}),
        )
        .await
        .unwrap();
    let resolution = until(&mut events, |e| e["type"] == "permission_resolved").await;
    assert_eq!(resolution["resolution"], json!({"behavior":"allow"}));
    login.await.unwrap().unwrap();
    let update = until(&mut events, |e| e["type"] == "omp_catalog_changed").await;
    assert_eq!(update["models"][1]["id"], "kiro-work-team/two");
    oob(&session, "/logout").await.unwrap();
    let account = until(&mut events, |e| {
        e["item"]["message"]
            .as_str()
            .is_some_and(|s| s.contains("OMP credentials for kiro"))
    })
    .await;
    assert!(
        account["item"]["message"]
            .as_str()
            .unwrap()
            .contains("#22: Second account")
    );
    oob(&session, "/logout kiro 22").await.unwrap();
    assert!(session.features().is_empty());
    assert_eq!(
        until(&mut events, |e| e["type"] == "omp_catalog_changed").await["models"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_queue_never_edits_ambiguous_or_claimed_entries() {
    let (_root, session, mut events) = open_fixture(false).await;
    oob(&session, "/queue").await.unwrap();
    assert!(
        until(&mut events, |e| e["item"]["type"] == "notification").await["item"]["message"]
            .as_str()
            .unwrap()
            .contains("1. [steering, already delivering] claimed")
    );
    assert!(
        oob(&session, "/queue remove 1")
            .await
            .unwrap_err()
            .to_string()
            .contains("already delivering")
    );
    assert!(
        oob(&session, "/queue remove 3")
            .await
            .unwrap_err()
            .to_string()
            .contains("identical text")
    );
    assert!(
        oob(&session, "/queue promote 4")
            .await
            .unwrap_err()
            .to_string()
            .contains("Only follow-up")
    );
    oob(&session, "/queue remove 4").await.unwrap();
    assert!(
        oob(&session, "/queue promote 5")
            .await
            .unwrap_err()
            .to_string()
            .contains("changed since")
    );
    oob(&session, "/queue").await.unwrap();
    oob(&session, "/queue promote 5").await.unwrap();
    oob(&session, "/queue mode steering all").await.unwrap();
    oob(&session, "/queue mode follow-up one").await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_retry_subagents_and_cancelled_shell_row() {
    let (_root, session, mut events) = open_fixture(false).await;
    oob(&session, "/autoretry off").await.unwrap();
    oob(&session, "/autoretry").await.unwrap();
    oob(&session, "/retry-stop").await.unwrap();
    oob(&session, "/subagents").await.unwrap();
    oob(&session, "/subagent-steer child-42 save  two spaces")
        .await
        .unwrap();
    oob(&session, "/subagent-cancel child-42").await.unwrap();
    let subagents = until(&mut events, |e| {
        e["item"]["message"]
            .as_str()
            .is_some_and(|s| s.starts_with("OMP subagents:"))
    })
    .await;
    assert!(
        subagents["item"]["message"]
            .as_str()
            .unwrap()
            .contains("child-42 — Scout (running)")
    );
    let bash = tokio::spawn(
        session
            .clone()
            .prepare_out_of_band(&json!("/bash fake shell command"))
            .unwrap(),
    );
    until(&mut events, |e| e["item"]["message"] == "Shell started").await;
    let mutation = tokio::sync::Mutex::new(());
    tokio::time::timeout(Duration::from_secs(3), async {
        // The manager holds this lane while admitting a prompt. The shell's
        // response cannot be required for admission, since stop also uses it.
        let _mutation = mutation.lock().await;
        let prompt = session
            .start_turn(json!("prompt during shell"), json!({}))
            .await
            .unwrap();
        let acknowledgement = session
            .native_acknowledgement(prompt["turnId"].as_str().unwrap())
            .unwrap();
        assert!(matches!(
            acknowledgement.await,
            crate::providers::api::PromptAck::Accepted(_)
        ));
    })
    .await
    .expect("shell is still running and must not hold the operation gate");
    let _mutation = mutation.lock().await;
    oob(&session, "/bash-stop").await.unwrap();
    bash.await.unwrap().unwrap();
    let shell = until(&mut events, |e| e["item"]["detail"]["type"] == "shell").await;
    assert_eq!(shell["item"]["detail"]["output"], "captured output");
    assert_eq!(shell["item"]["detail"]["exitCode"], 130);
    assert_eq!(shell["item"]["status"], "canceled");
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_cycle_updates_model_thinking_and_account_options() {
    let (_root, session, mut events) = open_fixture(false).await;
    oob(&session, "/next-model").await.unwrap();
    let model = until(&mut events, |e| e["type"] == "model_changed").await;
    assert_eq!(model["model"], "openai-codex/one");
    assert_eq!(session.features()[0]["options"][1]["id"], "openai-codex/31");
    assert!(
        session
            .set_feature("oauth_account", json!("kiro/11"))
            .await
            .is_err()
    );
    oob(&session, "/next-thinking").await.unwrap();
    assert_eq!(
        until(&mut events, |e| e["type"] == "model_changed").await["thinkingOptionId"],
        "high"
    );
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_fork_runs_in_separate_session_and_blocks_identity_changes() {
    let (root, session, mut events) = open_fixture(false).await;
    for command in [
        "/fresh",
        "/session delete",
        "/session:delete",
        "/resume old",
    ] {
        assert!(oob(&session, command).await.is_err(), "{command}");
    }
    oob(&session, "/fork user-entry").await.unwrap();
    let fork = until(&mut events, |e| e["type"] == "omp_fork_ready").await;
    assert_eq!(fork["sessionFile"], json!(root.path().join("fork.jsonl")));
    assert_eq!(
        tokio::fs::read_to_string(root.path().join("fork.jsonl"))
            .await
            .unwrap(),
        "user-entry"
    );
    assert_eq!(
        tokio::fs::read_to_string(root.path().join("source.jsonl"))
            .await
            .unwrap(),
        "source transcript"
    );
    assert_eq!(session.native_id().as_deref(), Some("source"));
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_kiro_commands_refresh_models_and_keep_native_todo_editing() {
    let (_root, session, mut events) = open_fixture(false).await;
    session
        .start_turn(json!("/kiro-accounts"), json!({}))
        .await
        .unwrap();
    let added = until(&mut events, |e| e["type"] == "omp_catalog_changed").await;
    assert_eq!(added["models"][1]["id"], "kiro-work-team/two");
    until(&mut events, |e| e["type"] == "turn_completed").await;
    session
        .start_turn(json!("/kiro-profile next"), json!({}))
        .await
        .unwrap();
    let removed = until(&mut events, |e| e["type"] == "omp_catalog_changed").await;
    assert_eq!(removed["models"].as_array().unwrap().len(), 1);
    session
        .start_turn(json!("/todo append check fixture"), json!({}))
        .await
        .unwrap();
    until(&mut events, |e| {
        e["item"]["text"] == "Appended check fixture"
    })
    .await;
    until(&mut events, |e| e["type"] == "turn_completed").await;
    assert_eq!(
        session.inner.refresh().await.unwrap()["todoPhases"][0]["tasks"][0]["content"],
        "check fixture"
    );
    oob(&session, "/todo edit").await.unwrap();
    let list = until(&mut events, |e| {
        e["item"]["message"]
            .as_str()
            .is_some_and(|s| s.starts_with("OMP todos"))
    })
    .await;
    assert!(
        list["item"]["message"]
            .as_str()
            .unwrap()
            .contains("1.1 [pending] Default: check fixture")
    );
    assert!(
        oob(&session, "/todo edit 2.1 missing")
            .await
            .unwrap_err()
            .to_string()
            .contains("does not exist")
    );
    oob(&session, "/todo edit 1.1 verify redacted login")
        .await
        .unwrap();
    let edit = until(&mut events, |e| e["item"]["type"] == "todo").await;
    assert_eq!(edit["item"]["items"][0]["text"], "verify redacted login");
    assert_eq!(edit["item"]["items"][0]["status"], "pending");
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_switch_is_model_only_not_a_session_switch() {
    let (_root, session, mut events) = open_fixture(false).await;
    assert!(
        session
            .clone()
            .prepare_out_of_band(&json!("/switch anthropic/one"))
            .is_none()
    );
    session
        .start_turn(json!("/switch anthropic/one"), json!({}))
        .await
        .unwrap();
    let switched = until(&mut events, |e| e["type"] == "model_changed").await;
    assert_eq!(switched["model"], "anthropic/one");
    assert_eq!(switched["runtimeInfo"]["sessionId"], "source");
    assert!(session.features().is_empty());
    session.close().await.unwrap();
}

#[tokio::test]
async fn omp_native_controls_handoff_keeps_working_and_adopts_replacement_identity() {
    let (root, session, mut events) = open_fixture(false).await;
    until(&mut events, |e| e["type"] == "thread_started").await;
    oob(&session, "/handoff in place").await.unwrap();
    let marker = until(&mut events, |e| {
        e["item"]["type"] == "compaction" && e["item"]["status"] == "completed"
    })
    .await;
    assert_eq!(marker["item"]["trigger"], "handoff");
    // OMP already showed a failed compacting handoff with its reason: no second error.
    oob(&session, "/handoff invalid").await.unwrap();
    let reason = until(&mut events, |e| e["item"]["level"] == "error").await;
    assert_eq!(
        reason["item"]["message"],
        "Compaction failed: Handoff generation produced an invalid document"
    );
    // A refusal before compacting keeps OMP's reason in the command error.
    let refused = oob(&session, "/handoff busy").await.unwrap_err();
    assert_eq!(
        format!("{refused:#}"),
        "Failed to hand off the OMP conversation: Cannot hand off while a response is in progress"
    );
    // A skipped /compact is not an error.
    oob(&session, "/compact").await.unwrap();
    let skipped = until(&mut events, |e| e["item"]["status"] == "skipped").await;
    assert_eq!(skipped["item"]["trigger"], "manual");
    let note = until(&mut events, |e| e["item"]["type"] == "notification").await;
    assert_eq!(note["item"]["level"], "info");
    assert_eq!(
        note["item"]["message"],
        "Nothing to compact (session too small)"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(event) = events.try_recv() {
        let text = event["item"]["text"].as_str().unwrap_or_default();
        assert!(
            !text.contains("Failed to compact"),
            "duplicate error: {event}"
        );
    }
    assert_eq!(session.native_id().as_deref(), Some("source"));
    oob(&session, "/handoff legacy").await.unwrap();
    let thread = until(&mut events, |e| e["type"] == "thread_started").await;
    assert_eq!(thread["sessionId"], "replacement");
    assert_eq!(
        session.describe_persistence().unwrap()["nativeHandle"],
        json!(root.path().join("replacement.jsonl"))
    );
    session.close().await.unwrap();
}
