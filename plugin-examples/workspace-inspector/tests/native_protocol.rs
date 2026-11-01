use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn native_provider_enforces_permission_and_serves_real_file_settings_and_hooks() {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::write(directory.path().join("notes.txt"), "one two\nthree\n")
        .await
        .unwrap();
    let (host, worker) = tokio::io::duplex(1024 * 1024);
    let (input, output) = tokio::io::split(worker);
    let worker = tokio::spawn(
        paseo_workspace_inspector::plugin()
            .unwrap()
            .serve(input, output),
    );
    let (input, mut output) = tokio::io::split(host);
    let mut input = BufReader::new(input);
    let mut pending = VecDeque::<Value>::new();
    macro_rules! send {
        ($value:expr) => {{
            let mut bytes = serde_json::to_vec(&$value).unwrap();
            bytes.push(b'\n');
            output.write_all(&bytes).await.unwrap();
        }};
    }
    macro_rules! receive {
        ($predicate:expr) => {{
            let predicate = $predicate;
            if let Some(index) = pending.iter().position(&predicate) {
                pending.remove(index).unwrap()
            } else {
                loop {
                    let mut line = String::new();
                    assert_ne!(
                        tokio::time::timeout(Duration::from_secs(3), input.read_line(&mut line))
                            .await
                            .expect("native response deadline")
                            .unwrap(),
                        0
                    );
                    let message: Value = serde_json::from_str(&line).unwrap();
                    if predicate(&message) {
                        break message;
                    }
                    pending.push_back(message);
                }
            }
        }};
    }
    send!(
        json!({"type":"initialize","pluginId":"workspace-inspector","appVersion":"0.13.1","pluginDirectory":directory.path(),"settingsDirectory":directory.path().join("settings")})
    );
    let ready = receive!(|m: &Value| m["type"] == "ready");
    assert_eq!(ready["providers"][0]["id"], "workspace-inspector");
    send!(json!({"type":"invoke","requestId":"read","method":"settings.inspection.read"}));
    let settings = receive!(|m: &Value| m["requestId"] == "read")["output"].clone();
    send!(
        json!({"type":"invoke","requestId":"save","method":"settings.inspection.write","input":{"revision":settings["revision"],"values":{"maxBytes":100}}})
    );
    assert_eq!(
        receive!(|m: &Value| m["requestId"] == "save")["output"]["status"],
        "saved"
    );
    send!(
        json!({"type":"hook","requestId":"event","kind":"event","name":"agent.created","input":{"agentId":"test"}})
    );
    receive!(|m: &Value| m["requestId"] == "event");
    send!(json!({"type":"invoke","requestId":"counts","method":"lifecycle.counts"}));
    assert_eq!(
        receive!(|m: &Value| m["requestId"] == "counts")["output"]["agentsCreated"],
        1
    );
    send!(
        json!({"type":"hook","requestId":"before","kind":"before","name":"agent.session_open","input":{"agentId":"test","cwd":directory.path(),"env":{"KEEP":"yes"}}})
    );
    let transformed = receive!(|m: &Value| m["requestId"] == "before");
    assert_eq!(
        transformed["output"]["env"],
        json!({"KEEP":"yes","PASEO_INSPECT_MAX_BYTES":"100"})
    );
    send!(
        json!({"type":"invoke","requestId":"inspect","method":"file.inspect","input":{"cwd":directory.path(),"path":"notes.txt"}})
    );
    assert_eq!(
        receive!(|m: &Value| m["requestId"] == "inspect")["output"]["words"],
        3
    );
    send!(
        json!({"type":"provider.connect","providerId":"workspace-inspector","connectionId":"connection","request":{"versions":[1],"capabilities":["prompt.message","prompt.command","permission"]}})
    );
    assert_eq!(
        receive!(|m: &Value| m["type"] == "provider.connected")["version"],
        1
    );
    send!(
        json!({"type":"provider.send","connectionId":"connection","acceptanceId":"open","input":{"type":"session.open","requestId":"open","sessionId":"session","config":{"cwd":directory.path(),"model":"local-file"}}})
    );
    receive!(|m: &Value| m["event"]["type"] == "session.ready");
    receive!(|m: &Value| m["type"] == "provider.accepted" && m["acceptanceId"] == "open");
    for (index, behavior) in ["deny", "allow"].into_iter().enumerate() {
        send!(
            json!({"type":"provider.send","connectionId":"connection","acceptanceId":format!("prompt-{index}"),"input":{"type":"session.prompt","sessionId":"session","prompt":{"clientMessageId":format!("message-{index}"),"delivery":"auto","input":{"type":"message","content":[{"type":"text","text":"notes.txt"}]}}}})
        );
        let permission = receive!(|m: &Value| m["event"]["type"] == "session.permission");
        receive!(|m: &Value| m["type"] == "provider.accepted"
            && m["acceptanceId"] == format!("prompt-{index}"));
        assert!(
            !pending
                .iter()
                .any(|m| m["event"]["type"] == "timeline.item"),
            "file content emitted before consent"
        );
        send!(
            json!({"type":"provider.send","connectionId":"connection","acceptanceId":format!("permission-{index}"),"input":{"type":"session.permission","sessionId":"session","permissionId":permission["event"]["request"]["id"],"response":{"behavior":behavior}}})
        );
        let turn =
            receive!(|m: &Value| m["event"]["type"] == "session.turn"
                && m["event"]["state"] != "started");
        assert_eq!(
            turn["event"]["state"],
            if behavior == "deny" {
                "canceled"
            } else {
                "completed"
            }
        );
        if behavior == "allow" {
            let item = receive!(|m: &Value| m["event"]["type"] == "timeline.item");
            let report: Value =
                serde_json::from_str(item["event"]["item"]["text"].as_str().unwrap()).unwrap();
            assert_eq!(report["words"], 3);
            assert_eq!(report["preview"], "one two\nthree");
        } else {
            assert!(
                !pending
                    .iter()
                    .any(|m| m["event"]["type"] == "timeline.item")
            );
        }
        receive!(|m: &Value| m["type"] == "provider.accepted"
            && m["acceptanceId"] == format!("permission-{index}"));
    }
    send!(json!({"type":"provider.close","connectionId":"connection"}));
    receive!(|m: &Value| m["type"] == "provider.closed");
    send!(json!({"type":"shutdown"}));
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .expect("native shutdown deadline")
        .unwrap()
        .unwrap();
}

#[test]
fn executable_validation_reports_registered_contract_and_rejects_incompatible_manifests() {
    let directory = tempfile::tempdir().unwrap();
    let executable = std::path::Path::new(env!("CARGO_BIN_EXE_paseo-workspace-inspector"));
    let filename = executable.file_name().unwrap().to_str().unwrap();
    let local = directory.path().join(filename);
    std::fs::copy(executable, &local).unwrap();
    let manifest = json!({"id":"workspace-inspector","name":"Workspace Inspector","runtime":"native","requirements":{"paseo":">=0.13.1"},"server":{"executable":filename,"args":[]}});
    let path = directory.path().join("paseo-plugin.json");
    std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let output = std::process::Command::new(&local)
        .arg("--validate")
        .arg(directory.path())
        .arg("0.13.1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let catalog: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(catalog["valid"], true);
    assert_eq!(catalog["plugin"]["id"], "workspace-inspector");
    assert!(
        catalog["methods"]
            .as_array()
            .unwrap()
            .contains(&json!("file.inspect"))
    );
    assert_eq!(catalog["hooks"]["before"], json!(["agent.session_open"]));
    assert_eq!(catalog["providers"][0]["id"], "workspace-inspector");
    let incompatible = std::process::Command::new(&local)
        .arg("--validate")
        .arg(directory.path())
        .arg("0.12.0")
        .output()
        .unwrap();
    assert!(!incompatible.status.success());
    assert!(incompatible.stdout.is_empty());
    let mut escaped = manifest;
    escaped["server"]["executable"] = json!("../outside");
    std::fs::write(&path, serde_json::to_vec(&escaped).unwrap()).unwrap();
    let unsafe_path = std::process::Command::new(&local)
        .arg("--validate")
        .arg(directory.path())
        .arg("0.13.1")
        .output()
        .unwrap();
    assert!(!unsafe_path.status.success());
    assert!(unsafe_path.stdout.is_empty());
}
