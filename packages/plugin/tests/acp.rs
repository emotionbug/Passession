use paseo_plugin::{
    CancellationToken, Plugin, Result,
    acp::{AcpContext, AcpProvider, AcpStream, AcpTransformer, ConfigChange},
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf},
    task::JoinHandle,
};

async fn frame(writer: &mut (impl AsyncWriteExt + Unpin), value: Value) {
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
}
fn options(model: &str) -> Value {
    json!([{"id":"model","name":"Model","type":"select","category":"model","currentValue":model,"options":[{"value":"small","name":"Small"},{"value":"large","name":"Large"}]}])
}
async fn agent(stream: DuplexStream, cancel: CancellationToken, seen: Arc<Mutex<Vec<Value>>>) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut active = None;
    let mut model = "small".to_owned();
    loop {
        let mut line = String::new();
        let n = tokio::select! {_=cancel.cancelled()=>break,result=reader.read_line(&mut line)=>result.unwrap()};
        if n == 0 {
            break;
        }
        let request: Value = serde_json::from_str(&line).unwrap();
        seen.lock().unwrap().push(request.clone());
        if request.get("result").is_some() {
            if let Some(id) = active.take() {
                frame(
                    &mut writer,
                    json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                )
                .await;
            }
            continue;
        }
        let result = match request["method"].as_str().unwrap() {
            "initialize" => {
                json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{}},"promptCapabilities":{"image":true}}})
            }
            "session/new" => {
                json!({"sessionId":"native","modes":{"currentModeId":"safe","availableModes":[{"id":"safe","name":"Safe"}]},"configOptions":options(&model)})
            }
            "session/load" => {
                for text in ["old ", "answer"] {
                    frame(&mut writer,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}})).await;
                }
                json!({"configOptions":options(&model)})
            }
            "session/list" => {
                json!({"sessions":[{"sessionId":"native","cwd":"/workspace","title":"Saved"}]})
            }
            "session/set_config_option" => {
                if request["params"]["configId"] == "reject" {
                    frame(&mut writer,json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32602,"message":"rejected option"}})).await;
                    continue;
                }
                model = request["params"]["value"].as_str().unwrap().into();
                json!({"configOptions":options(&model)})
            }
            "session/prompt" => {
                let text = request["params"]["prompt"][0]["text"].as_str().unwrap();
                if text == "wait" {
                    active = Some(request["id"].clone());
                    continue;
                }
                for text in ["Hello ", "world"] {
                    frame(&mut writer,json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}})).await;
                }
                if text == "permission" {
                    active = Some(request["id"].clone());
                    frame(&mut writer,json!({"jsonrpc":"2.0","id":"permission-request","method":"session/request_permission","params":{"sessionId":"native","toolCall":{"toolCallId":"edit","title":"Edit","rawInput":{"path":"a"}},"options":[{"optionId":"yes","name":"Approve","kind":"allow_once"},{"optionId":"no","name":"Reject","kind":"reject_once"}]}})).await;
                    continue;
                }
                json!({"stopReason":"end_turn"})
            }
            "session/cancel" => {
                if let Some(id) = active.take() {
                    frame(
                        &mut writer,
                        json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"cancelled"}}),
                    )
                    .await;
                }
                continue;
            }
            "session/close" | "session/set_mode" => json!({}),
            other => panic!("unexpected ACP method {other}"),
        };
        frame(
            &mut writer,
            json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
        )
        .await;
    }
}
struct Harness {
    reader: BufReader<ReadHalf<DuplexStream>>,
    writer: WriteHalf<DuplexStream>,
    pending: VecDeque<Value>,
    worker: JoinHandle<Result<()>>,
    seen: Arc<Mutex<Vec<Value>>>,
    _directory: tempfile::TempDir,
}
impl Harness {
    async fn start() -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let provider = AcpProvider::connector("acp-test", "ACP test", move |cancel| {
            let observed = observed.clone();
            async move {
                let (client, server) = tokio::io::duplex(1024 * 1024);
                let (reader, writer) = tokio::io::split(client);
                let task = tokio::spawn(agent(server, cancel.clone(), observed));
                let mut stream = AcpStream::new(reader, writer);
                stream.cancellation = cancel;
                stream.task = Some(task);
                Ok(stream)
            }
        })
        .unwrap()
        .transformer(Alias);
        let mut plugin = Plugin::new();
        plugin.provider(provider).unwrap();
        let (host, worker) = tokio::io::duplex(1024 * 1024);
        let (input, output) = tokio::io::split(worker);
        let worker = tokio::spawn(plugin.serve(input, output));
        let (reader, writer) = tokio::io::split(host);
        let directory = tempfile::tempdir().unwrap();
        let mut this = Self {
            reader: BufReader::new(reader),
            writer,
            pending: VecDeque::new(),
            worker,
            seen,
            _directory: directory,
        };
        this.send(json!({"type":"initialize","pluginId":"acp-test","appVersion":"0.13.1","pluginDirectory":this._directory.path(),"settingsDirectory":this._directory.path()})).await;
        this.take(|v| v["type"] == "ready").await;
        this.send(json!({"type":"provider.connect","connectionId":"c","providerId":"acp-test","request":{"versions":[1],"capabilities":["prompt.message","prompt.command","prompt.image","session.configure","permission","session.persistence","session.list"]}})).await;
        this.take(|v| v["type"] == "provider.connected").await;
        this
    }
    async fn send(&mut self, value: Value) {
        frame(&mut self.writer, value).await;
    }
    async fn take(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.pending.iter().position(&predicate) {
            return self.pending.remove(i).unwrap();
        }
        loop {
            let mut line = String::new();
            let count =
                tokio::time::timeout(Duration::from_secs(5), self.reader.read_line(&mut line))
                    .await
                    .expect("native ACP response deadline")
                    .unwrap();
            assert_ne!(count, 0);
            let value: Value = serde_json::from_str(&line).unwrap();
            if predicate(&value) {
                return value;
            }
            self.pending.push_back(value);
        }
    }
    async fn event(&mut self, kind: &str) -> Value {
        self.take(|v| v["type"] == "provider.event" && v["event"]["type"] == kind)
            .await["event"]
            .clone()
    }
    async fn input(&mut self, input: Value) {
        self.send(json!({"type":"provider.send","connectionId":"c","acceptanceId":"accept","input":input})).await;
        assert_eq!(
            self.take(|v| v["acceptanceId"] == "accept").await["type"],
            "provider.accepted"
        );
    }
    async fn open(&mut self) {
        self.input(json!({"type":"session.open","requestId":"open","sessionId":"s","config":{"cwd":"/workspace"}})).await;
        self.event("session.ready").await;
    }
    async fn prompt(&mut self, text: &str) {
        self.input(json!({"type":"session.prompt","sessionId":"s","prompt":{"clientMessageId":text,"input":{"type":"message","content":[{"type":"text","text":text}]}}})).await;
    }
    async fn terminal(&mut self) -> Value {
        self.take(|v| v["event"]["type"] == "session.turn" && v["event"]["state"] != "started")
            .await["event"]
            .clone()
    }
    async fn stop(mut self) {
        self.send(json!({"type":"shutdown"})).await;
        tokio::time::timeout(Duration::from_secs(5), self.worker)
            .await
            .expect("owned ACP shutdown deadline")
            .unwrap()
            .unwrap();
    }
}
struct Alias;
#[paseo_plugin::async_trait]
impl AcpTransformer for Alias {
    async fn discover(&self, mut catalog: Value, _context: AcpContext) -> Result<Value> {
        catalog["extension"] = json!("transformed");
        Ok(catalog)
    }
    async fn configure(&self, change: &ConfigChange, context: AcpContext) -> Result<bool> {
        if let ConfigChange::Setting { id, value } = change {
            if id == "alias" {
                context.config.set("model", value.clone()).await?;
                assert_eq!(context.config.read().await?["model"], *value);
                return Ok(true);
            }
        }
        Ok(false)
    }
}
#[tokio::test]
async fn discovery_listing_stream_order_and_transformer_configuration_use_native_protocol() {
    let mut host = Harness::start().await;
    host.input(json!({"type":"catalog","requestId":"catalog"}))
        .await;
    assert_eq!(
        host.event("catalog").await["catalog"]["extension"],
        "transformed"
    );
    host.input(json!({"type":"sessions","requestId":"list","limit":1}))
        .await;
    assert_eq!(
        host.event("sessions").await["sessions"][0]["persistence"]["data"]["sessionId"],
        "native"
    );
    host.open().await;
    host.input(json!({"type":"session.configure","sessionId":"s","requestId":"configure","changes":{"settings":{"alias":"large"}}})).await;
    host.event("request.completed").await;
    host.prompt("hello").await;
    assert_eq!(host.terminal().await["state"], "completed");
    assert!(
        host.pending
            .iter()
            .any(|v| v["event"]["item"]["text"] == "Hello world"),
        "stream deltas must be applied before prompt completion"
    );
    host.stop().await;
}
#[tokio::test]
async fn permission_round_trip_and_interrupt_remain_live_while_prompt_waits() {
    let mut host = Harness::start().await;
    host.open().await;
    host.prompt("permission").await;
    let permission = host.event("session.permission").await;
    assert_eq!(permission["request"]["actions"][0]["behavior"], "allow");
    host.input(json!({"type":"session.permission","sessionId":"s","permissionId":"permission:edit","response":{"behavior":"allow","selectedActionId":"no"}})).await;
    assert!(
        host.event("session.runtime_failed").await["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not match")
    );
    host.input(json!({"type":"session.permission","sessionId":"s","permissionId":"permission:edit","response":{"behavior":"allow","selectedActionId":"yes"}})).await;
    host.event("session.permission_resolved").await;
    assert_eq!(host.terminal().await["state"], "completed");
    assert!(
        host.seen
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["result"]["outcome"]["optionId"] == "yes")
    );
    host.prompt("wait").await;
    host.take(|v| {
        v["event"]["type"] == "session.prompt_result" && v["event"]["clientMessageId"] == "wait"
    })
    .await;
    host.input(json!({"type":"session.interrupt","sessionId":"s","requestId":"interrupt"}))
        .await;
    assert_eq!(host.terminal().await["state"], "canceled");
    host.stop().await;
}
#[tokio::test]
async fn loading_replays_before_readiness_and_failed_configuration_rolls_back() {
    let mut host = Harness::start().await;
    host.input(json!({"type":"session.open","sessionId":"s","requestId":"load","config":{"cwd":"/workspace"},"persistence":{"version":1,"data":{"sessionId":"native"}},"history":"replay"})).await;
    host.event("session.ready").await;
    assert!(
        host.pending
            .iter()
            .any(|v| v["event"]["item"]["text"] == "old answer")
    );
    host.input(json!({"type":"session.configure","sessionId":"s","requestId":"bad-config","changes":{"model":"large","settings":{"reject":"bad"}}})).await;
    assert_eq!(
        host.event("request.failed").await["requestId"],
        "bad-config"
    );
    let values: Vec<_> = host
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|v| {
            v["method"] == "session/set_config_option" && v["params"]["configId"] == "model"
        })
        .map(|v| v["params"]["value"].clone())
        .collect();
    assert_eq!(values, vec![json!("large"), json!("small")]);
    host.stop().await;
}
