use paseo_plugin::{Plugin, Result, SettingsDefinition, UsageSource, ensure};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf},
    sync::Notify,
    task::JoinHandle,
};

struct Host {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    pending: VecDeque<Value>,
    worker: JoinHandle<Result<()>>,
    directory: tempfile::TempDir,
}
impl Host {
    async fn start(plugin: Plugin) -> Self {
        let directory = tempfile::tempdir().unwrap();
        Self::with_directory(plugin, directory).await
    }
    async fn with_directory(plugin: Plugin, directory: tempfile::TempDir) -> Self {
        let (host, worker) = tokio::io::duplex(1024 * 1024);
        let (input, output) = tokio::io::split(worker);
        let worker = tokio::spawn(plugin.serve(input, output));
        let (reader, writer) = tokio::io::split(host);
        let mut host = Self {
            reader: BufReader::new(reader),
            writer,
            worker,
            directory,
            pending: VecDeque::new(),
        };
        host.send(json!({"type":"initialize","pluginId":"integration","appVersion":"0.13.1","pluginDirectory":host.directory.path(),"settingsDirectory":host.directory.path()})).await;
        host.take(|m| m["type"] == "ready").await;
        host
    }
    async fn send(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.writer.write_all(&bytes).await.unwrap();
    }
    async fn take(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.pending.iter().position(&predicate) {
            return self.pending.remove(index).unwrap();
        }
        loop {
            let mut line = String::new();
            let count =
                tokio::time::timeout(Duration::from_secs(3), self.reader.read_line(&mut line))
                    .await
                    .expect("worker response deadline")
                    .unwrap();
            assert_ne!(count, 0, "worker exited before response");
            let message: Value = serde_json::from_str(&line).unwrap();
            if predicate(&message) {
                return message;
            }
            self.pending.push_back(message);
        }
    }
    async fn invoke(&mut self, id: &str, method: &str, input: Value) -> Value {
        self.send(json!({"type":"invoke","requestId":id,"method":method,"input":input}))
            .await;
        let reply = self.take(|m| m["requestId"] == id).await;
        assert_eq!(reply["type"], "result", "{reply}");
        reply["output"].clone()
    }
    async fn stop(mut self) {
        self.send(json!({"type":"shutdown"})).await;
        tokio::time::timeout(Duration::from_secs(3), self.worker)
            .await
            .expect("orderly shutdown deadline")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn concurrent_completion_preserves_partial_input_and_cancellation_runs_cleanup() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dropped = paseo_plugin::CancellationToken::new();
    let cleaned = Arc::new(AtomicBool::new(false));
    let mut plugin = Plugin::new();
    let (started, unblock) = (entered.clone(), release.clone());
    plugin
        .rpc("slow", move |_, _| {
            let (started, unblock) = (started.clone(), unblock.clone());
            async move {
                started.notify_one();
                unblock.notified().await;
                Ok(json!("finished"))
            }
        })
        .unwrap();
    plugin
        .rpc("sum", |input, _| async move {
            Ok(json!(
                input["a"].as_i64().unwrap() + input["b"].as_i64().unwrap()
            ))
        })
        .unwrap();
    let (started, marker) = (entered.clone(), dropped.clone());
    plugin
        .before("agent.create", move |_, _| {
            let (started, marker) = (started.clone(), marker.clone());
            async move {
                let _guard = marker.drop_guard();
                started.notify_one();
                std::future::pending::<Result<Value>>().await
            }
        })
        .unwrap();
    let cleanup = cleaned.clone();
    plugin
        .cleanup(move |_, context| {
            let cleanup = cleanup.clone();
            async move {
                assert!(context.cancellation.is_cancelled());
                cleanup.store(true, Ordering::SeqCst);
                Ok(Value::Null)
            }
        })
        .unwrap();
    let mut host = Host::start(plugin).await;
    host.send(json!({"type":"invoke","requestId":"slow","method":"slow"}))
        .await;
    entered.notified().await;
    host.writer
        .write_all(b"{\"type\":\"invoke\",\"requestId\":\"sum\",")
        .await
        .unwrap();
    release.notify_one();
    assert_eq!(
        host.take(|m| m["requestId"] == "slow").await["output"],
        "finished"
    );
    host.writer
        .write_all(b"\"method\":\"sum\",\"input\":{\"a\":17,\"b\":25}}\n")
        .await
        .unwrap();
    assert_eq!(host.take(|m| m["requestId"] == "sum").await["output"], 42);
    host.send(json!({"type":"hook","kind":"before","name":"agent.create","requestId":"cancel","input":{}})).await;
    entered.notified().await;
    host.send(json!({"type":"hook.cancel","requestId":"cancel"}))
        .await;
    assert_eq!(
        host.take(|m| m["requestId"] == "cancel").await["type"],
        "error"
    );
    assert!(dropped.is_cancelled());
    host.stop().await;
    assert!(cleaned.load(Ordering::SeqCst));
}

fn settings_plugin() -> Plugin {
    let mut plugin = Plugin::new();
    plugin
        .settings(
            SettingsDefinition::new("preferences", 2, |input| {
                let count = input.get("count").and_then(Value::as_u64).unwrap_or(5);
                ensure!(count <= 100, "count is too large");
                Ok(json!({"count":count}))
            })
            .migrate(|old, version| async move {
                assert_eq!(version, 1);
                Ok(json!({"count":old["oldCount"]}))
            }),
        )
        .unwrap();
    plugin
}

#[tokio::test]
async fn settings_migrate_persist_conflict_and_preserve_invalid_bytes_until_reset() {
    let directory = tempfile::tempdir().unwrap();
    tokio::fs::write(
        directory.path().join("preferences.json"),
        br#"{"version":1,"values":{"oldCount":9}}"#,
    )
    .await
    .unwrap();
    let mut host = Host::with_directory(settings_plugin(), directory).await;
    let initial = host
        .invoke("read", "settings.preferences.read", json!({}))
        .await;
    assert_eq!(initial["values"]["count"], 9);
    let raw = tokio::fs::read(host.directory.path().join("preferences.json"))
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap()["version"], 2);
    let saved = host
        .invoke(
            "write",
            "settings.preferences.write",
            json!({"revision":initial["revision"],"values":{"count":12}}),
        )
        .await;
    assert_eq!(saved["status"], "saved");
    let stale = host
        .invoke(
            "stale",
            "settings.preferences.write",
            json!({"revision":initial["revision"],"values":{"count":30}}),
        )
        .await;
    assert_eq!(stale["status"], "conflict");
    let invalid = host
        .invoke(
            "invalid",
            "settings.preferences.write",
            json!({"revision":saved["revision"],"values":{"count":101}}),
        )
        .await;
    assert_eq!(invalid["status"], "invalid");
    assert_eq!(
        host.invoke("again", "settings.preferences.read", json!({}))
            .await["values"]["count"],
        12
    );
    tokio::fs::write(
        host.directory.path().join("preferences.json"),
        b"broken user file",
    )
    .await
    .unwrap();
    let broken = host
        .invoke("broken", "settings.preferences.read", json!({}))
        .await;
    assert_eq!(broken["status"], "invalid");
    assert_eq!(
        tokio::fs::read(host.directory.path().join("preferences.json"))
            .await
            .unwrap(),
        b"broken user file"
    );
    let reset = host
        .invoke(
            "reset",
            "settings.preferences.reset",
            json!({"revision":broken["revision"]}),
        )
        .await;
    assert_eq!(reset["status"], "saved");
    assert_eq!(reset["values"]["count"], 5);
    host.stop().await;
}

#[tokio::test]
async fn usage_discovery_fetch_and_daemon_handshake_reconnect_cross_real_ipc() {
    let mut plugin = Plugin::new();
    plugin
        .usage_source(UsageSource::new(
            "meter",
            "Meter",
            |scope, _| async move {
                assert_eq!(scope["kind"], "global");
                Ok(json!([{"key":"account","input":{"used":25}}]))
            },
            |input, _| async move {
                Ok(serde_json::to_value(
                    paseo_plugin::usage::Report::Available {
                        plan_label: Some("Local".into()),
                        windows: vec![paseo_plugin::usage::window_from_used_pct(
                            "daily",
                            "Daily",
                            input["used"].as_f64(),
                            None,
                        )],
                        balances: None,
                        details: None,
                    },
                )?)
            },
        ))
        .unwrap();
    plugin
        .rpc("daemon.info", |_, context| async move {
            Ok(context.daemon.client().await?.server_info())
        })
        .unwrap();
    let mut host = Host::start(plugin).await;
    host.send(json!({"type":"usage.discover","requestId":"discover","sourceId":"meter","scope":{"kind":"global"}})).await;
    let account = host.take(|m| m["requestId"] == "discover").await["output"][0].clone();
    host.send(json!({"type":"usage.fetch","requestId":"fetch","sourceId":"meter","input":account["input"]})).await;
    let usage = host.take(|m| m["requestId"] == "fetch").await;
    assert_eq!(usage["output"]["status"], "available");
    assert_eq!(usage["output"]["windows"][0]["remainingPct"], 75.0);
    for id in ["first", "reconnected"] {
        host.send(json!({"type":"invoke","requestId":id,"method":"daemon.info"}))
            .await;
        let hello = host.take(|m| m["type"] == "paseo_frame").await;
        assert_eq!(hello["isBinary"], false);
        let hello: Value = serde_json::from_str(hello["data"].as_str().unwrap()).unwrap();
        assert!(hello.to_string().contains("plugin:integration"));
        let wire = json!({"type":"session","message":{"type":"status","payload":{"status":"server_info","protocolVersion":1,"marker":id}}});
        host.send(json!({"type":"paseo_frame","isBinary":false,"data":wire.to_string()}))
            .await;
        let response = host.take(|m| m["requestId"] == id).await;
        assert_eq!(response["type"], "result", "{response}");
        assert_eq!(response["output"]["marker"], id);
        host.send(json!({"type":"paseo_close"})).await;
    }
    host.stop().await;
}

#[tokio::test]
async fn retained_settings_handles_do_not_keep_worker_output_alive_after_shutdown() {
    let mut plugin = Plugin::new();
    let settings = plugin
        .settings(SettingsDefinition::new("retained", 1, |input| Ok(input)))
        .unwrap();
    let mut host = Host::start(plugin).await;
    assert_eq!(
        host.invoke("read", "settings.retained.read", json!({}))
            .await["status"],
        "ready"
    );
    host.stop().await;
    // Retaining an API handle is not ownership of the runtime's writer task.
    drop(settings);
}
