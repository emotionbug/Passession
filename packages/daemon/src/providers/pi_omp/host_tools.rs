mod history;

use super::transport::Transport;
use crate::{
    mcp_client::McpConnection,
    providers::api::{LaunchContext, NativeToolExecutor},
};
use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct BridgedTool {
    client: Arc<McpConnection>,
    server: String,
    name: String,
}
pub struct Router {
    runtime: Arc<Transport>,
    executor: Option<Arc<dyn NativeToolExecutor>>,
    tools: HashMap<String, BridgedTool>,
    catalog: history::Catalog,
    history: history::History,
    clients: Vec<Arc<McpConnection>>,
    pending: Mutex<HashMap<String, CancellationToken>>,
    closed: AtomicBool,
    cancel: CancellationToken,
}
impl Router {
    pub async fn register(
        runtime: Arc<Transport>,
        context: &LaunchContext,
        cwd: &Path,
        env: &BTreeMap<String, String>,
        session_options: &Value,
    ) -> Result<Arc<Self>> {
        let native = context
            .tool_catalog
            .as_array()
            .or_else(|| context.tool_catalog["tools"].as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if !native.is_empty() && context.tool_executor.is_none() {
            bail!("OMP native tool catalog has no executable tool dispatcher");
        }
        let mut definitions = Vec::new();
        for tool in native {
            let name = tool["name"]
                .as_str()
                .ok_or_else(|| anyhow!("Native tool definition requires a name"))?;
            let mut definition = json!({"name":name,"description":tool["description"].as_str().unwrap_or(""),"loadMode":"essential","parameters":tool.get("inputSchema").or_else(||tool.get("parameters")).cloned().unwrap_or_else(||json!({"type":"object","properties":{}}))});
            if let Some(title) = tool["title"].as_str() {
                definition["label"] = json!(title);
            }
            definitions.push(definition);
        }
        let cancel = context.cancel.child_token();
        let servers = crate::mcp::strip_internal_servers(context.mcp_servers.clone());
        let mut connections = futures_util::stream::FuturesUnordered::new();
        for (server, config) in servers.as_object().into_iter().flatten() {
            let server = server.clone();
            let config = config.clone();
            let cancel = cancel.child_token();
            connections.push(async move {
                let opened = tokio::time::timeout(
                    Duration::from_secs(10),
                    McpConnection::connect(&server, &config, cwd, env, cancel.clone()),
                )
                .await;
                let client = match opened {
                    Ok(Ok(client)) => client,
                    Ok(Err(error)) => {
                        tracing::warn!(%server,%error,"OMP MCP server unavailable");
                        return None;
                    }
                    Err(_) => {
                        cancel.cancel();
                        tracing::warn!(%server,"OMP MCP server connection timed out");
                        return None;
                    }
                };
                let listed = tokio::time::timeout(
                    Duration::from_secs(10),
                    client.list_tools(cancel.clone()),
                )
                .await;
                match listed {
                    Ok(Ok(tools)) => Some((server, client, tools)),
                    _ => {
                        let _ = client.close().await;
                        tracing::warn!(%server,"OMP MCP server tool listing failed");
                        None
                    }
                }
            });
        }
        let mut tools = HashMap::new();
        let mut clients = Vec::new();
        while let Some(connection) = connections.next().await {
            let Some((server, client, listed)) = connection else {
                continue;
            };
            for tool in listed {
                let Some(name) = tool["name"].as_str() else {
                    continue;
                };
                let mapped = bridge_name(&server, name);
                definitions.push(json!({"name":mapped,"label":format!("{server} / {name}"),"description":tool["description"].as_str().filter(|s|!s.is_empty()).map(str::to_owned).unwrap_or_else(||format!("{name} from {server}")),"loadMode":"essential","parameters":tool["inputSchema"]}));
                tools.insert(
                    mapped,
                    BridgedTool {
                        client: client.clone(),
                        server: server.clone(),
                        name: name.into(),
                    },
                );
            }
            clients.push(client);
        }
        let mut catalog = history::Catalog::default();
        for definition in &definitions {
            let name = definition["name"].as_str().unwrap();
            let identity = tools
                .get(name)
                .map(|tool| (tool.server.as_str(), tool.name.as_str()));
            catalog.insert(name, identity);
        }
        let mut history_options = session_options.clone();
        history_options["cwd"] = json!(cwd);
        if history_options["homeDir"].is_null() {
            history_options["homeDir"] = json!(env.get("HOME"));
        }
        let sessions = super::history::sessions_dir("omp", env, &history_options).await?;
        let roots = history::session_roots(env, cwd, sessions);
        let history = history::History::load(&context.home, roots).await;
        {
            let observed = catalog.observed(&history);
            for definition in &mut definitions {
                let identity = catalog
                    .identity(definition["name"].as_str().unwrap())
                    .unwrap();
                definition["loadMode"] = json!(if observed.contains(identity) {
                    "essential"
                } else {
                    "discoverable"
                });
            }
        }
        // Registered names are unique, so no tie-breaker or allocated sort keys
        // are needed. Sort only after history has assigned every load mode.
        definitions.sort_unstable_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap()
                .cmp(right["name"].as_str().unwrap())
        });
        let router = Arc::new(Self {
            runtime,
            executor: context.tool_executor.clone(),
            tools,
            catalog,
            history,
            clients,
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            cancel,
        });
        // Controls are dispatched by the stdout reader, independently of the
        // ordered transcript consumer. A weak reference avoids a runtime cycle.
        router.runtime.set_host_tools(Arc::downgrade(&router));
        if (!native.is_empty() || servers.as_object().is_some_and(|s| !s.is_empty()))
            && let Err(error) = router
                .runtime
                .request(json!({"type":"set_host_tools","tools":definitions}))
                .await
        {
            router.close().await;
            return Err(error);
        }
        Ok(router)
    }
    pub fn identities(&self) -> Value {
        json!(
            self.tools
                .iter()
                .map(|(name, tool)| json!([name,{"server":tool.server,"tool":tool.name}]))
                .collect::<Vec<_>>()
        )
    }
    pub fn handle(self: &Arc<Self>, frame: &Value) -> bool {
        match frame["type"].as_str() {
            Some("host_tool_call") => {
                let (Some(id), Some(name)) = (frame["id"].as_str(), frame["toolName"].as_str())
                else {
                    return true;
                };
                if !frame["toolCallId"].is_string() || !frame["arguments"].is_object() {
                    tracing::debug!("Dropped malformed OMP host tool call");
                    return true;
                }
                let (id, name, args) = (id.to_owned(), name.to_owned(), frame["arguments"].clone());
                if self.closed.load(Ordering::Acquire) {
                    return true;
                }
                let cancel = self.cancel.child_token();
                self.pending
                    .lock()
                    .unwrap()
                    .insert(id.clone(), cancel.clone());
                let router = self.clone();
                tokio::spawn(async move {
                    router.call(id, name, args, cancel).await;
                });
                true
            }
            Some("host_tool_cancel") => {
                if frame["id"].is_string()
                    && let Some(id) = frame["targetId"].as_str()
                    && let Some(cancel) = self.pending.lock().unwrap().get(id)
                {
                    cancel.cancel();
                }
                true
            }
            Some("host_tool_update") => true,
            _ => false,
        }
    }
    async fn call(
        self: Arc<Self>,
        id: String,
        name: String,
        args: Value,
        cancel: CancellationToken,
    ) {
        if let Some(identity) = self.catalog.identity(&name)
            && let Err(error) = self.history.record(identity).await
        {
            tracing::warn!(%name, %error, "OMP host tool invocation could not be retained for future catalogs");
        }
        let (updates, receive) = mpsc::channel(32);
        let (finished, completion) = oneshot::channel();
        let update_runtime = self.runtime.clone();
        let update_id = id.clone();
        let update_cancel = cancel.clone();
        let update_task = tokio::spawn(async move {
            forward_updates(receive, completion, update_cancel, |result| {
                let runtime = &update_runtime;
                let frame =
                    json!({"type":"host_tool_update","id":update_id,"partialResult":result});
                async move { runtime.send(&frame).await }
            })
            .await
        });
        let result = if let Some(tool) = self.tools.get(&name) {
            drop(updates);
            tool.client
                .call_tool(&tool.name, args, cancel.clone())
                .await
        } else if let Some(executor) = &self.executor {
            executor
                .execute_streaming(&name, args, cancel.clone(), Some(updates))
                .await
        } else {
            drop(updates);
            Ok(
                json!({"content":[{"type":"text","text":format!("Unknown host tool: {name}")}],"isError":true}),
            )
        };
        let _ = finished.send(());
        if let Err(error) = update_task.await {
            tracing::warn!(%error,"OMP host tool progress forwarding failed");
        }
        if !cancel.is_cancelled() && !self.closed.load(Ordering::Acquire) {
            let result=result.and_then(map_result).unwrap_or_else(|error|json!({"content":[{"type":"text","text":error.to_string()}],"details":{},"isError":true}));
            let mut frame = json!({"type":"host_tool_result","id":id,"result":result});
            if let Some(is_error) = frame["result"].get("isError").cloned() {
                frame["isError"] = is_error;
            }
            if let Err(error) = self.runtime.send(&frame).await {
                tracing::warn!(%error,"OMP host tool result delivery failed");
            }
        }
        self.pending.lock().unwrap().remove(&id);
    }
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancel.cancel();
        for (_, cancel) in self.pending.lock().unwrap().drain() {
            cancel.cancel();
        }
        for client in &self.clients {
            if let Err(error) = client.close().await {
                tracing::debug!(%error,"OMP MCP cleanup failed");
            }
        }
    }
}

async fn forward_updates<F, FF>(
    mut receive: mpsc::Receiver<Value>,
    mut completion: oneshot::Receiver<()>,
    cancel: CancellationToken,
    mut send: F,
) where
    F: FnMut(Value) -> FF,
    FF: Future<Output = Result<()>>,
{
    let mut completed = false;
    loop {
        let update = if completed {
            // Only already accepted progress belongs before the final result.
            // recv() after close can still wait on a retained sender permit.
            receive.try_recv().ok()
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = &mut completion => {
                    receive.close();
                    completed = true;
                    continue;
                }
                update = receive.recv() => update,
            }
        };
        let Some(update) = update else { break };
        let result = match map_result(update) {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(%error,"Invalid native tool update");
                continue;
            }
        };
        let sent = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            result = send(result) => result,
        };
        if let Err(error) = sent {
            tracing::warn!(%error,"OMP host tool progress delivery failed");
            break;
        }
    }
}
fn map_result(result: Value) -> Result<Value> {
    let Value::Object(mut result) = result else {
        bail!("Native tool result requires content");
    };
    let Some(Value::Array(mut content)) = result.remove("content") else {
        bail!("Native tool result requires content");
    };
    let details = result.remove("structuredContent");
    if content.is_empty()
        && let Some(structured) = &details
    {
        let mut block = json!({"type":"text"});
        block["text"] = Value::String(serde_json::to_string(structured)?);
        content.push(block);
    }
    let mut mapped = serde_json::Map::new();
    mapped.insert("content".into(), Value::Array(content));
    if let Some(details) = details {
        mapped.insert("details".into(), details);
    }
    if let Some(error) = result.remove("isError") {
        mapped.insert("isError".into(), error);
    }
    Ok(Value::Object(mapped))
}
fn bridge_name(server: &str, tool: &str) -> String {
    let hash = hex::encode(Sha256::digest(format!("{server}\0{tool}")));
    let clean = |name: &str, length: usize| {
        name.encode_utf16()
            .take(length)
            .map(|c| {
                if (c < 128 && (c as u8).is_ascii_alphanumeric())
                    || c == b'_' as u16
                    || c == b'-' as u16
                {
                    char::from_u32(c as u32).unwrap()
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    format!(
        "mcp_{}__{}_{}",
        clean(server, 20),
        clean(tool, 25),
        &hash[..12]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_result_text_recovers_structured_values_without_loss() {
        for details in [
            json!({"models":[{"id":"한글","text":"line one\n  line two","value":null}],
                "empty":[],"nested":{"flag":true,"fraction":0.25}}),
            json!([null, "spaces  remain", {"quoted":"\"value\""}]),
            json!("literal\n  text"),
            Value::Null,
        ] {
            let result = map_result(json!({"content":[],"structuredContent":details})).unwrap();
            let text = result["content"][0]["text"].as_str().unwrap();
            assert_eq!(serde_json::from_str::<Value>(text).unwrap(), details);
            assert_eq!(result["details"], details);
            assert!(result.get("isError").is_none());
        }
    }

    #[test]
    fn supplied_content_takes_precedence_without_losing_details_or_error_state() {
        let content = json!([
            {"type":"text","text":"A deliberately formatted result:\n  unchanged"},
            {"type":"image","data":"AA==","mimeType":"image/png"}
        ]);
        for error in [None, Some(Value::Null), Some(json!(false)), Some(json!(true))] {
            let mut input = json!({"content":content,"structuredContent":{"failure":{"code":32}}});
            if let Some(error) = &error {
                input["isError"] = error.clone();
            }
            let result = map_result(input).unwrap();
            assert_eq!(result["content"], content);
            assert_eq!(result["details"]["failure"]["code"], 32);
            assert_eq!(result.get("isError"), error.as_ref());
        }
        let empty = map_result(json!({"content":[]})).unwrap();
        assert_eq!(empty["content"], json!([]));
        assert!(empty.get("details").is_none());
    }

    #[test]
    fn malformed_content_is_not_converted_to_an_empty_success() {
        for input in [
            Value::Null,
            json!([]),
            json!({"structuredContent":{"valid":true}}),
            json!({"content":null,"structuredContent":{"valid":true}}),
            json!({"content":{},"isError":true}),
        ] {
            assert!(map_result(input).is_err());
        }
    }
    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingExecutor {
        calls: Mutex<Vec<(String, Value)>>,
    }

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl NativeToolExecutor for RecordingExecutor {
        async fn execute_streaming(
            &self,
            name: &str,
            input: Value,
            _: CancellationToken,
            _: Option<mpsc::Sender<Value>>,
        ) -> Result<Value> {
            self.calls.lock().unwrap().push((name.into(), input));
            Ok(json!({"content":[{"type":"text","text":name}]}))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registration_order_preserves_catalog_routes_and_history_load_modes() -> Result<()> {
        use axum::{
            extract::{Path as AxumPath, State},
            http::StatusCode,
            response::IntoResponse,
            routing::post,
        };
        use tokio::sync::Semaphore;

        struct Server {
            gates: [Semaphore; 2],
            progress: mpsc::Sender<(String, usize)>,
            reverse_tools: bool,
            calls: Mutex<Vec<(String, Value)>>,
        }

        async fn handle(
            State(server): State<Arc<Server>>,
            AxumPath(name): AxumPath<String>,
            axum::Json(message): axum::Json<Value>,
        ) -> axum::response::Response {
            let index = usize::from(name == "beta");
            let method = message["method"].as_str().unwrap();
            let result = match method {
                "initialize" => {
                    server.progress.send((method.into(), index)).await.unwrap();
                    server.gates[index].acquire().await.unwrap().forget();
                    json!({
                        "protocolVersion":"2024-11-05",
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":name,"version":"1"}
                    })
                }
                "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
                "tools/list" => {
                    let mut tools = vec![
                        json!({"name":"same","description":"Shared name, distinct server",
                            "inputSchema":{"type":"object","properties":{"value":{"type":"integer"}}}}),
                        json!({"name":"unused","inputSchema":{"type":"object","properties":{}}}),
                    ];
                    if server.reverse_tools {
                        tools.reverse();
                    }
                    server.progress.send((method.into(), index)).await.unwrap();
                    json!({"tools":tools})
                }
                "tools/call" => {
                    server
                        .calls
                        .lock()
                        .unwrap()
                        .push((name.clone(), message["params"].clone()));
                    json!({"content":[{"type":"text","text":format!("{name}/{}", message["params"]["name"].as_str().unwrap())}]})
                }
                _ => panic!("unexpected MCP request: {message}"),
            };
            axum::Json(json!({"jsonrpc":"2.0","id":message["id"],"result":result})).into_response()
        }

        let mut baseline = None;
        for permutation in 0..8 {
            let home = tempfile::tempdir()?;
            let cancel = CancellationToken::new();
            let _cleanup = cancel.clone().drop_guard();
            let history = history::History::load(home.path(), Vec::new()).await;
            history
                .record(&json!(["native", "z_native"]).to_string())
                .await?;
            history
                .record(&json!(["mcp", "alpha", "same"]).to_string())
                .await?;

            let (progress, mut progress_rx) = mpsc::channel(4);
            let server = Arc::new(Server {
                gates: [Semaphore::new(0), Semaphore::new(0)],
                progress,
                reverse_tools: permutation & 4 != 0,
                calls: Mutex::new(Vec::new()),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let app = axum::Router::new()
                .route("/{server}", post(handle))
                .with_state(server.clone());
            let shutdown = cancel.clone();
            let serving = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await
            });
            // Reuse the Router's transport harness to capture the actual wire
            // catalog and results; all registration and dispatch run normally.
            let (runtime, mut events) = Transport::spawn(
                paseo_client::process::CommandSpec {
                    program: "cat".into(),
                    args: Vec::new(),
                    cwd: None,
                    env: BTreeMap::new(),
                    clear_env: false,
                },
                Duration::from_secs(5),
                cancel.clone(),
            )
            .await?;
            let executor = Arc::new(RecordingExecutor::default());
            let mut native = vec![
                json!({"name":"z_native","description":"Observed native","title":"Native label",
                    "inputSchema":{"type":"object","properties":{"value":{"type":"integer"}}}}),
                json!({"name":"a_native","description":"Unobserved native",
                    "parameters":{"type":"object","properties":{}}}),
            ];
            if permutation & 1 != 0 {
                native.reverse();
            }
            let env = BTreeMap::from([("HOME".into(), home.path().display().to_string())]);
            let context = LaunchContext {
                agent_id: "catalog-order".into(),
                home: home.path().into(),
                executable: "cat".into(),
                env: env.clone(),
                mcp_servers: json!({
                    "alpha":{"type":"http","url":format!("http://{address}/alpha")},
                    "beta":{"type":"http","url":format!("http://{address}/beta")}
                }),
                tool_catalog: json!(native),
                tool_manifest: Value::Null,
                tool_executor: Some(executor.clone()),
                cancel: cancel.clone(),
            };
            let options = json!({"homeDir":home.path(),"sessionDir":home.path().join("sessions")});
            let register = Router::register(runtime.clone(), &context, home.path(), &env, &options);
            let capture = async {
                // Both connections must initialize concurrently before either
                // is released. Reverse which server reaches tools/list first.
                let mut initialized = Vec::new();
                for _ in 0..2 {
                    let (method, index) = progress_rx.recv().await.unwrap();
                    assert_eq!(method, "initialize");
                    initialized.push(index);
                }
                initialized.sort_unstable();
                assert_eq!(initialized, [0, 1]);
                let first = usize::from(permutation & 2 != 0);
                server.gates[first].add_permits(1);
                assert_eq!(
                    progress_rx.recv().await.unwrap(),
                    ("tools/list".into(), first)
                );
                server.gates[1 - first].add_permits(1);
                assert_eq!(
                    progress_rx.recv().await.unwrap(),
                    ("tools/list".into(), 1 - first)
                );
                let frame = events.recv().await.unwrap();
                assert_eq!(frame["type"], "set_host_tools");
                runtime
                    .send(&json!({"type":"response","id":frame["id"],"success":true}))
                    .await?;
                Ok::<_, anyhow::Error>(frame["tools"].clone())
            };
            let (router, definitions) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(register, capture)
            })
            .await?;
            let router = router?;
            let definitions = definitions?;
            let serialized = serde_json::to_vec(&definitions)?;
            if let Some(expected) = &baseline {
                assert_eq!(&serialized, expected, "permutation {permutation}");
            } else {
                baseline = Some(serialized);
            }
            let definitions = definitions.as_array().unwrap();
            assert_eq!(definitions.len(), 6);
            assert!(definitions.windows(2).all(|pair| {
                pair[0]["name"].as_str().unwrap() < pair[1]["name"].as_str().unwrap()
            }));
            let observed_external = bridge_name("alpha", "same");
            for (index, definition) in definitions.iter().enumerate() {
                let name = definition["name"].as_str().unwrap();
                let observed = name == "z_native" || name == observed_external;
                assert_eq!(
                    definition["loadMode"],
                    if observed {
                        "essential"
                    } else {
                        "discoverable"
                    }
                );
                let args = json!({"value":index});
                let expected = if let Some(tool) = router.tools.get(name) {
                    assert_eq!(name, bridge_name(&tool.server, &tool.name));
                    assert_eq!(
                        definition["label"],
                        format!("{} / {}", tool.server, tool.name)
                    );
                    format!("{}/{}", tool.server, tool.name)
                } else {
                    assert!(name == "z_native" || name == "a_native");
                    name.to_owned()
                };
                assert!(router.handle(&json!({
                    "type":"host_tool_call","id":format!("call-{index}"),
                    "toolCallId":format!("tool-{index}"),"toolName":name,"arguments":args
                })));
                let frame = tokio::time::timeout(Duration::from_secs(5), events.recv())
                    .await?
                    .unwrap();
                assert_eq!(frame["type"], "host_tool_result");
                assert_eq!(frame["id"], format!("call-{index}"));
                assert_eq!(frame["result"]["content"][0]["text"], expected);
                if let Some(tool) = router.tools.get(name) {
                    assert_eq!(
                        server.calls.lock().unwrap().last(),
                        Some(&(
                            tool.server.clone(),
                            json!({"name":tool.name,"arguments":args})
                        ))
                    );
                } else {
                    assert_eq!(
                        executor.calls.lock().unwrap().last(),
                        Some(&(name.into(), args))
                    );
                }
            }
            assert_eq!(executor.calls.lock().unwrap().len(), 2);
            assert_eq!(server.calls.lock().unwrap().len(), 4);
            router.close().await;
            runtime.close().await?;
            cancel.cancel();
            serving.await??;
        }
        Ok(())
    }

    #[cfg(unix)]
    struct RetainedProgress {
        sender: Mutex<Option<mpsc::Sender<Value>>>,
    }

    #[cfg(unix)]
    #[async_trait::async_trait]
    impl NativeToolExecutor for RetainedProgress {
        async fn execute_streaming(
            &self,
            _: &str,
            _: Value,
            _: CancellationToken,
            updates: Option<mpsc::Sender<Value>>,
        ) -> Result<Value> {
            let updates = updates.unwrap();
            for index in 0..3 {
                updates
                    .send(json!({"content":[{"type":"text","text":index.to_string()}]}))
                    .await?;
            }
            *self.sender.lock().unwrap() = Some(updates);
            Ok(json!({"content":[{"type":"text","text":"done"}]}))
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn router_emits_final_result_after_progress_with_retained_sender() -> Result<()> {
        let home = tempfile::tempdir()?;
        let cancel = CancellationToken::new();
        let _cleanup = cancel.clone().drop_guard();
        let (runtime, mut events) = Transport::spawn(
            paseo_client::process::CommandSpec {
                program: "cat".into(),
                args: Vec::new(),
                cwd: None,
                env: BTreeMap::new(),
                clear_env: false,
            },
            Duration::from_secs(1),
            cancel.clone(),
        )
        .await?;
        let executor = Arc::new(RetainedProgress {
            sender: Mutex::new(None),
        });
        let router = Arc::new(Router {
            runtime: runtime.clone(),
            executor: Some(executor.clone()),
            tools: HashMap::new(),
            catalog: history::Catalog::default(),
            history: history::History::load(home.path(), Vec::new()).await,
            clients: Vec::new(),
            pending: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            cancel,
        });
        assert!(router.handle(&json!({
            "type":"host_tool_call","id":"call","toolCallId":"native-call",
            "toolName":"retained","arguments":{}
        })));
        for index in 0..3 {
            let frame = tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await?
                .unwrap();
            assert_eq!(frame["type"], "host_tool_update");
            assert_eq!(
                frame["partialResult"]["content"][0]["text"],
                index.to_string()
            );
        }
        let frame = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await?
            .unwrap();
        assert_eq!(frame["type"], "host_tool_result");
        assert_eq!(frame["result"]["content"][0]["text"], "done");
        assert!(
            executor
                .sender
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_closed()
        );
        let canceled = router.cancel.child_token();
        canceled.cancel();
        tokio::time::timeout(
            Duration::from_secs(1),
            router
                .clone()
                .call("canceled".into(), "retained".into(), json!({}), canceled),
        )
        .await?;
        runtime.send(&json!({"type":"after_cancellation"})).await?;
        let frame = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await?
            .unwrap();
        assert_eq!(
            frame["type"], "after_cancellation",
            "canceled calls must not emit a final result"
        );
        router.close().await;
        runtime.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn completed_tool_drains_fifo_without_waiting_for_retained_progress_sender() {
        let (updates, receive) = mpsc::channel(32);
        let retained = updates.clone();
        let permit = retained.clone().reserve_owned().await.unwrap();
        let (finished, completion) = oneshot::channel();
        for index in 0..3 {
            updates
                .send(json!({"content":[{"type":"text","text":index.to_string()}]}))
                .await
                .unwrap();
        }
        finished.send(()).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let forwarded = frames.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            forward_updates(receive, completion, CancellationToken::new(), |frame| {
                forwarded.lock().unwrap().push(frame);
                async { Ok(()) }
            }),
        )
        .await
        .expect("executor completion, not sender lifetime, ends progress");
        frames.lock().unwrap().push(json!({"final":true}));
        assert_eq!(
            *frames.lock().unwrap(),
            vec![
                json!({"content":[{"type":"text","text":"0"}]}),
                json!({"content":[{"type":"text","text":"1"}]}),
                json!({"content":[{"type":"text","text":"2"}]}),
                json!({"final":true}),
            ]
        );
        assert!(updates.is_closed());
        drop(permit);
    }

    #[tokio::test]
    async fn cancellation_unblocks_progress_delivery_with_retained_sender() {
        let (updates, receive) = mpsc::channel(32);
        let (_finished, completion) = oneshot::channel();
        updates
            .send(json!({"content":[{"type":"text","text":"progress"}]}))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let canceled = cancel.clone();
        tokio::time::timeout(
            Duration::from_secs(1),
            forward_updates(receive, completion, cancel, move |_| {
                canceled.cancel();
                std::future::pending::<Result<()>>()
            }),
        )
        .await
        .expect("cancellation must interrupt a blocked progress write");
        assert!(updates.is_closed());
    }
}
