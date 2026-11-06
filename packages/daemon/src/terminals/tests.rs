use super::*;
use std::time::Duration;

#[cfg(unix)]
async fn shell(home: &tempfile::TempDir, args: Vec<String>) -> Arc<Terminal> {
    let mut env = BTreeMap::new();
    env.insert("HOME".into(), home.path().to_string_lossy().into_owned());
    env.insert("PATH".into(), "/usr/bin:/bin".into());
    env.insert("SHELL".into(), "/bin/sh".into());
    let (changed, _) = broadcast::channel(8);
    Terminal::spawn(
        CreateTerminal {
            cwd: home.path().to_path_buf(),
            workspace_id: "wks_isolated".into(),
            name: None,
            command: Some("/bin/sh".into()),
            args,
            env,
            rows: 24,
            cols: 80,
        },
        "test-token".into(),
        changed,
    )
    .await
    .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn real_pty_has_terminal_descriptors_and_drains_final_capture() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(
        &home,
        vec![
            "-c".into(),
            r"test -t 0 && test -t 1 && printf '\033[31mPASEO_PTY_FINAL\033[0m\r\n'; exit 7".into(),
        ],
    )
    .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), terminal.wait())
            .await
            .unwrap()
            .unwrap(),
        7
    );
    let (lines, total) = terminal.capture(None, None, true).await.unwrap();
    assert!(total >= 24);
    assert!(lines.iter().any(|line| line.contains("PASEO_PTY_FINAL")));
    assert!(!lines.iter().any(|line| line.contains('\x1b')));
    assert_eq!(terminal.info()["workspaceId"], "wks_isolated");
    terminal.terminate().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn source_resize_claims_and_real_shell_completion() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(&home, vec![]).await;
    terminal.resize("one", 31, 97, true).await.unwrap();
    terminal.resize("two", 20, 40, false).await.unwrap();
    let snapshot = terminal.snapshot(true).await.unwrap();
    assert_eq!(snapshot["rows"], 31);
    assert_eq!(snapshot["cols"], 97);
    terminal.resize("two", 22, 70, true).await.unwrap();
    let mut lifecycle = terminal.subscribe_lifecycle();
    terminal
        .input(b"stty size; printf '\\033]633;D;9\\007'\r")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                lifecycle.recv().await.unwrap(),
                TerminalEvent::CommandFinished(Some(9))
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let (lines, _) = terminal.capture(None, None, true).await.unwrap();
    assert!(lines.iter().any(|line| line.trim() == "22 70"));
    terminal.input(b"exit\r").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), terminal.wait())
        .await
        .unwrap()
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn noisy_output_does_not_drop_command_lifecycle() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(&home, vec![]).await;
    let mut lifecycle = terminal.subscribe_lifecycle();
    terminal.input(b"i=0; while [ $i -lt 6000 ]; do printf 'output-%s\\r\\n' $i; i=$((i+1)); done; printf '\\033]633;D;0\\007'\r").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if matches!(
                lifecycle.recv().await.unwrap(),
                TerminalEvent::CommandFinished(Some(0))
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    terminal.terminate().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn blocked_input_does_not_prevent_owned_termination() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(
        &home,
        vec!["-c".into(), "stty -echo -icanon; sleep 60".into()],
    )
    .await;
    let writer = terminal.clone();
    let task = tokio::spawn(async move { writer.input(&vec![b'x'; 1024 * 1024]).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tokio::time::timeout(Duration::from_secs(5), terminal.terminate())
        .await
        .unwrap()
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn attention_unknown_finished_and_interrupt_are_distinct() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(&home, vec![]).await;
    assert!(terminal.info()["activity"].is_null());
    terminal.set_activity("working");
    terminal.set_activity("idle");
    assert_eq!(terminal.info()["activity"]["attentionReason"], "finished");
    assert!(!terminal.set_activity("idle"));
    terminal.set_activity("clear_attention");
    assert!(terminal.info()["activity"].get("attentionReason").is_none());
    terminal.set_activity("attention");
    assert_eq!(
        terminal.info()["activity"]["attentionReason"],
        "needs_input"
    );
    terminal.set_activity("working");
    terminal.input(&[3]).await.unwrap();
    assert!(terminal.info()["activity"].is_null());
    terminal.terminate().await.unwrap();
}

#[test]
fn binary_frames_keep_exact_five_opcode_layout() {
    for opcode in 1..=5 {
        let bytes = streams::frame(opcode, 255, b"payload");
        assert_eq!(&bytes[..2], &[opcode, 255]);
        assert_eq!(&bytes[2..], b"payload");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn physical_source_slots_cannot_write_each_others_pty() {
    use crate::transport::OutboundFrame;
    use std::collections::HashSet;
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(&home, vec![]).await;
    let service = TerminalService::new(Weak::new());
    service
        .terminals
        .write()
        .unwrap()
        .insert(terminal.id.clone(), terminal.clone());
    let permissions = Arc::new(RwLock::new(HashSet::from([
        "workspace.read".to_owned(),
        "workspace.write".to_owned(),
    ])));
    let (owner, mut frames) = RequestContext::attach(
        "source-one".into(),
        "session".into(),
        "client-one".into(),
        json!({"owned_subscriptions":true}),
        json!({"kind":"owner"}),
        permissions.clone(),
        false,
        None,
        tokio_util::sync::CancellationToken::new(),
    );
    let (other, _) = RequestContext::attach(
        "source-two".into(),
        "session".into(),
        "client-two".into(),
        json!({"owned_subscriptions":true}),
        json!({"kind":"owner"}),
        permissions,
        false,
        None,
        tokio_util::sync::CancellationToken::new(),
    );
    let message = json!({"type":"subscribe_terminal_request","terminalId":terminal.id,"requestId":"owned-subscribe"});
    let request = owner.for_request(message.clone());
    streams::subscribe(&service, &request, &message)
        .await
        .unwrap();
    let first = frames.recv().await.unwrap();
    let slot = match &first.frame {
        OutboundFrame::Text(text) => {
            serde_json::from_str::<Value>(text).unwrap()["message"]["payload"]["slot"]
                .as_u64()
                .unwrap() as u8
        }
        _ => panic!("subscription acknowledgement must precede binary state"),
    };
    let second = frames.recv().await.unwrap();
    assert!(matches!(&second.frame,OutboundFrame::Binary(bytes) if bytes[0]==4 && bytes[1]==slot));
    service
        .handle_binary(
            &other,
            &streams::frame(2, slot, b"printf 'FOREIGN_SOURCE_EXECUTED\\n'\r"),
        )
        .await
        .unwrap();
    let mut lifecycle = terminal.subscribe_lifecycle();
    service
        .handle_binary(
            &owner,
            &streams::frame(
                2,
                slot,
                b"printf 'OWNED_SOURCE_EXECUTED\\n'; printf '\\033]633;D;0\\007'\r",
            ),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                lifecycle.recv().await.unwrap(),
                TerminalEvent::CommandFinished(Some(0))
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let (lines, _) = terminal.capture(None, None, true).await.unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("OWNED_SOURCE_EXECUTED"))
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("FOREIGN_SOURCE_EXECUTED"))
    );
    terminal.terminate().await.unwrap();
    owner.cancellation().cancel();
    other.cancellation().cancel();
}

#[cfg(unix)]
#[tokio::test]
async fn update_without_claim_does_not_take_terminal_size() {
    let home = tempfile::tempdir().unwrap();
    let terminal = shell(&home, vec![]).await;
    terminal.resize("observer", 10, 20, false).await.unwrap();
    let snapshot = terminal.snapshot(false).await.unwrap();
    assert_eq!(snapshot["rows"], 24);
    assert_eq!(snapshot["cols"], 80);
    terminal.resize("owner", 30, 90, true).await.unwrap();
    terminal.release_size("owner");
    terminal.resize("observer", 10, 20, false).await.unwrap();
    assert_eq!(terminal.snapshot(false).await.unwrap()["cols"], 90);
    terminal.terminate().await.unwrap();
}

#[cfg(unix)]
async fn script_workspace(config: Value) -> (tempfile::TempDir, Arc<AppState>, String) {
    use crate::projects::store;

    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("workspace");
    let home = root.path().join("home");
    tokio::fs::create_dir(&cwd).await.unwrap();
    tokio::fs::write(cwd.join("paseo.json"), config.to_string())
        .await
        .unwrap();
    let workspace_id = "wks_scripts".to_owned();
    let store = store::Store::load(&home).await.unwrap();
    store
        .upsert_project(
            store::create_project(json!({
                "projectId":"prj_scripts", "rootPath":cwd, "kind":"non_git",
                "displayName":"Scripts", "createdAt":"2026-01-01T00:00:00.000Z",
                "updatedAt":"2026-01-01T00:00:00.000Z"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    store
        .upsert_workspace(
            store::create_workspace(json!({
                "workspaceId":workspace_id, "projectId":"prj_scripts", "cwd":cwd,
                "kind":"directory", "displayName":"Scripts",
                "createdAt":"2026-01-01T00:00:00.000Z",
                "updatedAt":"2026-01-01T00:00:00.000Z"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let app = AppState::new(home, root.path().into(), std::env::current_exe().unwrap());
    app.config.initialize().await.unwrap();
    app.projects.initialize().await.unwrap();
    // Deliberately use a shell without OSC completion integration.
    app.terminals.register_workspace_env(
        &workspace_id,
        BTreeMap::from([
            ("SHELL".into(), "/bin/sh".into()),
            ("HOME".into(), root.path().to_string_lossy().into_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]),
    );
    (root, app, workspace_id)
}

#[cfg(unix)]
async fn subscribe_to_finished_script(service: &TerminalService, terminal_id: &str) {
    use crate::transport::OutboundFrame;
    use std::collections::HashSet;

    let (context, mut frames) = RequestContext::attach(
        "finished-log-reader".into(),
        "finished-log-session".into(),
        "finished-log-client".into(),
        json!({"owned_subscriptions":true,"terminal_reflowable_snapshot":true}),
        json!({"kind":"owner"}),
        Arc::new(RwLock::new(HashSet::from(["workspace.read".to_owned()]))),
        false,
        None,
        tokio_util::sync::CancellationToken::new(),
    );
    let message = json!({
        "type":"subscribe_terminal_request", "terminalId":terminal_id,
        "requestId":"finished-log"
    });
    let request = context.for_request(message.clone());
    service.handle(&request, &message).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let mut restored = false;
        let mut acknowledged = false;
        loop {
            match &frames.recv().await.unwrap().frame {
                OutboundFrame::Text(text) => {
                    let value: Value = serde_json::from_str(text).unwrap();
                    let message = &value["message"];
                    match message["type"].as_str() {
                        Some("subscribe_terminal_response") => {
                            assert!(message["payload"]["error"].is_null());
                            acknowledged = true;
                        }
                        Some("terminal_stream_exit") => {
                            assert!(acknowledged && restored);
                            break;
                        }
                        Some("terminal_stream_error") => panic!("{message}"),
                        _ => {}
                    }
                }
                OutboundFrame::Binary(bytes) => {
                    assert!(acknowledged);
                    restored |= bytes[0] == 4;
                }
                _ => {}
            }
        }
    })
    .await;
    context.cancellation().cancel();
    result.expect("finished script must restore logs and report stream exit");
}

#[cfg(unix)]
#[tokio::test]
async fn configured_script_exit_tracks_process_and_retains_logs() {
    let (root, app, workspace_id) = script_workspace(json!({})).await;
    let cwd = root.path().join("workspace");
    let mut previous_terminal: Option<String> = None;
    for expected_code in [0, 7] {
        let command = format!(
            "test -t 0 && test -t 1 || exit 99\n\
             printf '\\033]633;D;91\\007'\n\
             printf 'READY\\n'\n\
             while [ ! -f release-{expected_code} ]; do sleep 0.01; done\n\
             i=1; while [ \"$i\" -le 240 ]; do printf 'script-log-%s\\n' \"$i\"; i=$((i+1)); done\n\
             printf 'FINAL-{expected_code}\\n'\n{}",
            if expected_code == 0 { "true" } else { "exit 7" },
        );
        tokio::fs::write(
            cwd.join("paseo.json"),
            json!({"scripts":{"build":{"command":command,"type":"script"}}}).to_string(),
        )
        .await
        .unwrap();
        let launched = app.terminals.script_start(&workspace_id, "build").await.unwrap();
        let terminal_id = launched["terminalId"].as_str().unwrap();
        let terminal = app.terminals.get(terminal_id).await.unwrap();
        if let Some(previous) = previous_terminal.take() {
            assert_ne!(terminal_id, previous);
            assert!(app.terminals.get(&previous).await.is_err());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (lines, _) = terminal.capture(None, None, true).await.unwrap();
                if lines.iter().any(|line| line.trim() == "READY") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let running = app.terminals.script_list(&workspace_id).await.unwrap();
        assert_eq!(running[0]["lifecycle"], "running");
        assert!(running[0]["exitCode"].is_null());
        assert!(app.terminals.script_start(&workspace_id, "build").await.is_err());
        tokio::fs::write(cwd.join(format!("release-{expected_code}")), b"")
            .await
            .unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let scripts = app.terminals.script_list(&workspace_id).await.unwrap();
                if scripts[0]["lifecycle"] == "stopped" {
                    break scripts[0].clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("script must finish without interactive-shell completion hooks");
        assert_eq!(stopped["exitCode"], expected_code);
        assert_eq!(stopped["terminalId"], terminal_id);
        assert_eq!(terminal.wait().await.unwrap(), expected_code);
        assert!(app.terminals.list(Some(&workspace_id), None).await.is_empty());
        let retained = app.terminals.get(terminal_id).await.unwrap();
        let (lines, _) = retained.capture(None, None, true).await.unwrap();
        for line in 1..=240 {
            assert!(lines.iter().any(|text| text.trim() == format!("script-log-{line}")));
        }
        assert!(lines.iter().any(|line| line.trim() == format!("FINAL-{expected_code}")));
        subscribe_to_finished_script(&app.terminals, terminal_id).await;
        previous_terminal = Some(terminal_id.to_owned());
    }
    app.terminals.kill_workspace(&workspace_id).await.unwrap();
    assert!(app.terminals.get(&previous_terminal.unwrap()).await.is_err());
    app.terminals.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn configured_service_exit_and_stop_release_routes_but_retain_logs() {
    let (root, app, workspace_id) = script_workspace(json!({
        "scripts":{"web":{
            "type":"service",
            "command":"printf 'SERVICE_LOG\\n'; while [ ! -f release ]; do sleep 0.01; done; exit 7"
        }}
    }))
    .await;
    for stop_requested in [false, true] {
        let launched = app.terminals.script_start(&workspace_id, "web").await.unwrap();
        let terminal_id = launched["terminalId"].as_str().unwrap();
        let hostname = launched["hostname"].as_str().unwrap();
        let terminal = app.terminals.get(terminal_id).await.unwrap();
        assert_eq!(launched["lifecycle"], "running");
        assert!(launched["health"].is_null());
        assert!(app.terminals.scripts.route(hostname).is_some());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (lines, _) = terminal.capture(None, None, true).await.unwrap();
                if lines.iter().any(|line| line.trim() == "SERVICE_LOG") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if stop_requested {
            app.terminals.script_stop(&workspace_id, "web").await.unwrap();
        } else {
            tokio::fs::write(root.path().join("workspace/release"), b"")
                .await
                .unwrap();
        }
        let stopped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let scripts = app.terminals.script_list(&workspace_id).await.unwrap();
                if scripts[0]["lifecycle"] == "stopped" {
                    break scripts[0].clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let code = terminal.wait().await.unwrap();
        assert_eq!(stopped["exitCode"], code);
        if !stop_requested {
            assert_eq!(code, 7);
            tokio::fs::remove_file(root.path().join("workspace/release"))
                .await
                .unwrap();
        }
        assert!(stopped["health"].is_null());
        assert!(app.terminals.scripts.route(hostname).is_none());
        let retained = app.terminals.get(terminal_id).await.unwrap();
        let (lines, _) = retained.capture(None, None, true).await.unwrap();
        assert!(lines.iter().any(|line| line.trim() == "SERVICE_LOG"));
        subscribe_to_finished_script(&app.terminals, terminal_id).await;
    }
    let scripts = app.terminals.script_list(&workspace_id).await.unwrap();
    let terminal_id = scripts[0]["terminalId"].as_str().unwrap();
    app.terminals.remove_workspace_runtime(&workspace_id);
    assert!(app.terminals.get(terminal_id).await.is_err());
    app.terminals.shutdown().await.unwrap();
}
