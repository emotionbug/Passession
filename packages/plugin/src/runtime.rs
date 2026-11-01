use crate::{Context, Plugin, Result, anyhow, bail, daemon::Daemon, ensure, provider::Events};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const QUEUE_LIMIT: usize = 32 * 1024 * 1024;
const REQUEST_LIMIT: usize = 256;
const CONNECTION_LIMIT: usize = 1024;
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
enum Outgoing {
    Frame(Vec<u8>, OwnedSemaphorePermit),
    Flush(oneshot::Sender<Result<()>>),
    Shutdown,
}
#[derive(Clone)]
pub(crate) struct Output {
    tx: mpsc::Sender<Outgoing>,
    budget: Arc<Semaphore>,
    failed: CancellationToken,
}
impl Output {
    pub(crate) async fn send(&self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value)?;
        ensure!(
            bytes.len() < FRAME_LIMIT,
            "Plugin IPC output exceeds frame limit"
        );
        bytes.push(b'\n');
        let permit = self
            .budget
            .clone()
            .acquire_many_owned(bytes.len() as u32)
            .await?;
        self.tx
            .send(Outgoing::Frame(bytes, permit))
            .await
            .map_err(|_| anyhow!("Plugin output closed"))
    }
    pub(crate) async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Outgoing::Flush(tx))
            .await
            .map_err(|_| anyhow!("Plugin output closed"))?;
        rx.await.map_err(|_| anyhow!("Plugin output closed"))?
    }
    async fn shutdown(&self) -> Result<()> {
        self.tx
            .send(Outgoing::Shutdown)
            .await
            .map_err(|_| anyhow!("Plugin output closed"))
    }
}

pub(crate) async fn run(plugin: Plugin) -> Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if let Some(index) = arguments.iter().position(|value| value == "--validate") {
        ensure!(
            arguments.len() == index + 3,
            "Usage: --validate <directory> <daemon-version>"
        );
        let manifest = manifest(Path::new(&arguments[index + 1]), &arguments[index + 2]).await?;
        let mut metadata = plugin.metadata();
        metadata["valid"] = json!(true);
        metadata["plugin"] = json!({"id":manifest["id"]});
        let mut bytes = serde_json::to_vec(&metadata)?;
        ensure!(
            bytes.len() < 1024 * 1024,
            "Plugin validation metadata exceeds limit"
        );
        bytes.push(b'\n');
        let mut stdout = tokio::io::stdout();
        stdout.write_all(&bytes).await?;
        stdout.flush().await?;
        return Ok(());
    }
    serve(plugin, tokio::io::stdin(), tokio::io::stdout()).await
}
async fn manifest(directory: &Path, version: &str) -> Result<Value> {
    let bytes = tokio::fs::read(directory.join("paseo-plugin.json")).await?;
    ensure!(bytes.len() <= 1024 * 1024, "Plugin manifest exceeds limit");
    let manifest: Value = serde_json::from_slice(&bytes)?;
    crate::valid_id(string(&manifest, "id")?)?;
    ensure!(
        manifest["runtime"] == "native",
        "Plugin must declare runtime: native"
    );
    let executable = string(&manifest["server"], "executable")?;
    let path = Path::new(executable);
    ensure!(
        !path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "Plugin executable must be a contained relative path"
    );
    let root = tokio::fs::canonicalize(directory).await?;
    let target = tokio::fs::canonicalize(root.join(path)).await?;
    ensure!(
        target.starts_with(&root) && tokio::fs::metadata(&target).await?.is_file(),
        "Plugin executable escapes plugin directory"
    );
    if let Some(requirement) = manifest.pointer("/requirements/paseo") {
        let range: node_semver::Range = requirement
            .as_str()
            .ok_or_else(|| anyhow!("Invalid Paseo requirement"))?
            .parse()?;
        let version: node_semver::Version = version.parse()?;
        ensure!(
            range.satisfies(&version),
            "Plugin requires Paseo {range}; daemon is {version}"
        );
    }
    Ok(manifest)
}

pub(crate) async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            ensure!(frame.is_empty(), "Truncated plugin IPC frame");
            return Ok(None);
        }
        let length = chunk
            .iter()
            .position(|b| *b == b'\n')
            .map_or(chunk.len(), |i| i + 1);
        ensure!(
            frame.len() + length <= FRAME_LIMIT,
            "Plugin IPC input exceeds frame limit"
        );
        let complete = chunk[length - 1] == b'\n';
        frame.extend_from_slice(&chunk[..length]);
        reader.consume(length);
        if complete {
            return Ok(Some(serde_json::from_slice(&frame)?));
        }
    }
}

struct ConnectionControl {
    sender: mpsc::Sender<Value>,
    cancel: CancellationToken,
}
pub(crate) async fn serve<R, W>(plugin: Plugin, reader: R, mut writer: W) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::channel(256);
    let output = Output {
        tx,
        budget: Arc::new(Semaphore::new(QUEUE_LIMIT)),
        failed: CancellationToken::new(),
    };
    let writer_failed = output.failed.clone();
    let mut writer_task = tokio::spawn(async move {
        let _failure_guard = writer_failed.drop_guard();
        while let Some(message) = rx.recv().await {
            match message {
                Outgoing::Frame(bytes, _permit) => {
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                }
                Outgoing::Flush(reply) => {
                    let result = writer.flush().await.map_err(Into::into);
                    let _ = reply.send(result);
                }
                Outgoing::Shutdown => break,
            }
        }
        writer.flush().await?;
        Ok::<_, anyhow::Error>(())
    });
    let result = run_loop(plugin, BufReader::new(reader), output.clone()).await;
    if let Err(error) = &result {
        let _ = tokio::time::timeout(
            STOP_TIMEOUT,
            output.send(json!({"type":"fatal","error":error.to_string()})),
        )
        .await;
    }
    let flush = tokio::time::timeout(STOP_TIMEOUT, output.flush()).await;
    let shutdown = tokio::time::timeout(STOP_TIMEOUT, output.shutdown()).await;
    drop(output);
    match tokio::time::timeout(STOP_TIMEOUT, &mut writer_task).await {
        Ok(joined) => {
            joined??;
        }
        Err(_) => {
            writer_task.abort();
            let _ = writer_task.await;
            bail!("Plugin output shutdown timed out");
        }
    }
    result?;
    flush.map_err(|_| anyhow!("Plugin output flush timed out"))??;
    shutdown.map_err(|_| anyhow!("Plugin output shutdown timed out"))??;
    Ok(())
}
async fn run_loop<R: AsyncBufRead + Unpin>(
    plugin: Plugin,
    mut reader: R,
    output: Output,
) -> Result<()> {
    let initial = tokio::select! {
        _=output.failed.cancelled()=>bail!("Plugin output closed"),
        value=read_frame(&mut reader)=>value?.ok_or_else(||anyhow!("Plugin requires initialize first"))?,
    };
    ensure!(
        initial["type"] == "initialize",
        "Plugin requires initialize first"
    );
    let id = string(&initial, "pluginId")?.to_owned();
    let directory = std::path::PathBuf::from(string(&initial, "pluginDirectory")?);
    let version = string(&initial, "appVersion")?;
    let stop = CancellationToken::new();
    let daemon = Daemon::new(output.clone(), id.clone(), stop.child_token());
    let context = Context {
        plugin_id: id.into(),
        plugin_directory: Arc::new(directory),
        app_version: version.into(),
        cancellation: stop.clone(),
        daemon,
        output: output.clone(),
    };
    for settings in &plugin.settings {
        let directory = initial["settingsDirectory"]
            .as_str()
            .ok_or_else(|| anyhow!("Plugin settings storage is unavailable"))?;
        settings.bind(directory.into(), output.clone())?;
    }
    let mut ready = plugin.metadata();
    ready["type"] = json!("ready");
    output.send(ready).await?;
    let plugin = Arc::new(plugin);
    let mut requests: HashMap<String, CancellationToken> = HashMap::new();
    let mut connections: HashMap<String, ConnectionControl> = HashMap::new();
    let mut tasks = JoinSet::<(bool, String)>::new();
    let result:Result<()>=async {
        loop {
            // A task completion must not drop a partially consumed input frame.
            let reading=read_frame(&mut reader);
            tokio::pin!(reading);
            let message=loop {
                tokio::select! {
                    _=output.failed.cancelled()=>bail!("Plugin output closed"),
                    joined=tasks.join_next(),if !tasks.is_empty()=> {
                        let (connection,id)=joined.ok_or_else(||anyhow!("Plugin task disappeared"))??;
                        if connection {connections.remove(&id);} else {requests.remove(&id);}
                    },
                    message=&mut reading=>break message?,
                }
            };
            let Some(message)=message else {break};
            let kind=string(&message,"type")?;
            match kind {
                "shutdown"=>break,
                "hook.cancel"=> {if let Some(cancel)=requests.get(string(&message,"requestId")?){cancel.cancel();}},
                "paseo_frame"|"paseo_close"=>context.daemon.incoming(message)?,
                "provider.connect"=> {
                    let id=string(&message,"connectionId")?.to_owned();
                    let provider_id=string(&message,"providerId")?;
                    if connections.contains_key(&id) || connections.len()>=CONNECTION_LIMIT {
                        output.send(json!({"type":"provider.connect_failed","connectionId":id,"error":"Duplicate connection or connection limit exceeded"})).await?;continue;
                    }
                    let Some(provider)=plugin.providers.get(provider_id).cloned() else {
                        output.send(json!({"type":"provider.connect_failed","connectionId":id,"error":format!("Unknown provider: {provider_id}")})).await?;continue;
                    };
                    let cancel=stop.child_token();
                    let (sender,receiver)=mpsc::channel(32);
                    connections.insert(id.clone(),ConnectionControl{sender,cancel:cancel.clone()});
                    let context=context.clone();
                    tasks.spawn(async move {connection(provider,id.clone(),message["request"].clone(),receiver,cancel,context).await;(true,id)});
                },
                "provider.send"=> {
                    let id=string(&message,"connectionId")?.to_owned();
                    let acceptance=string(&message,"acceptanceId")?.to_owned();
                    let sent=connections.get(&id).filter(|c|!c.cancel.is_cancelled()).is_some_and(|c|c.sender.try_send(message).is_ok());
                    if !sent {output.send(json!({"type":"provider.rejected","connectionId":id,"acceptanceId":acceptance,"error":"Connection closed or input queue full"})).await?;}
                },
                "provider.close"=> {
                    let id=string(&message,"connectionId")?;
                    if let Some(connection)=connections.get(id){connection.cancel.cancel();}
                    else {output.send(json!({"type":"provider.closed","connectionId":id})).await?;}
                },
                "invoke"|"hook"|"usage.fetch"|"usage.discover"|"provider.status"|"provider.catalog_key"=> {
                    let id=string(&message,"requestId")?.to_owned();
                    if requests.contains_key(&id) {bail!("Duplicate request ID: {id}");}
                    if requests.len()>=REQUEST_LIMIT {
                        output.send(json!({"type":"error","requestId":id,"error":"Plugin concurrent request limit exceeded"})).await?;continue;
                    }
                    let cancel=stop.child_token();requests.insert(id.clone(),cancel.clone());
                    let mut context=context.clone();context.cancellation=cancel.clone();
                    let plugin=plugin.clone();
                    tasks.spawn(async move {
                        let result=tokio::select! {_=cancel.cancelled()=>Err(anyhow!("Plugin request canceled")),result=dispatch(&plugin,message,&context)=>result};
                        let reply=match result {Ok(output)=>json!({"type":"result","requestId":id,"output":output}),Err(error)=>json!({"type":"error","requestId":id,"error":error.to_string()})};
                        let _=tokio::time::timeout(STOP_TIMEOUT,context.output.send(reply)).await;
                        (false,id)
                    });
                },
                other=>bail!("Unknown plugin request: {other}"),
            }
        }
        Ok(())
    }.await;
    stop.cancel();
    // Cancellation reaches hooks and connection actors before cleanup. Wait for
    // all actor closes; abort only a plugin callback that violates shutdown bounds.
    let drained = tokio::time::timeout(STOP_TIMEOUT, async {
        while let Some(joined) = tasks.join_next().await {
            joined?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    let mut cleanup_error = None;
    if drained.is_err() {
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        cleanup_error = Some(anyhow!(
            "Plugin callbacks did not stop within shutdown deadline"
        ));
    } else if let Ok(Err(error)) = drained {
        cleanup_error = Some(error);
    }
    if let Some(cleanup) = &plugin.cleanup {
        match tokio::time::timeout(STOP_TIMEOUT, cleanup(Value::Null, context.clone())).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => cleanup_error = Some(error),
            Err(_) => cleanup_error = Some(anyhow!("Plugin cleanup timed out")),
        }
    }
    let _ = tokio::time::timeout(STOP_TIMEOUT, context.daemon.close()).await;
    result?;
    if let Some(error) = cleanup_error {
        return Err(error);
    }
    Ok(())
}
async fn dispatch(plugin: &Plugin, message: Value, context: &Context) -> Result<Value> {
    let kind = string(&message, "type")?;
    match kind {
        "invoke" => {
            let name = string(&message, "method")?;
            let handler = plugin
                .methods
                .get(name)
                .ok_or_else(|| anyhow!("Unknown RPC method: {name}"))?;
            handler(
                message.get("input").cloned().unwrap_or(Value::Null),
                context.clone(),
            )
            .await
        }
        "usage.fetch" | "usage.discover" => {
            let id = string(&message, "sourceId")?;
            let source = plugin
                .usage
                .get(id)
                .ok_or_else(|| anyhow!("Unknown usage source: {id}"))?;
            if kind == "usage.discover" {
                (source.discover)(message["scope"].clone(), context.clone()).await
            } else {
                (source.fetch)(message["input"].clone(), context.clone()).await
            }
        }
        "provider.status" | "provider.catalog_key" => {
            let id = string(&message, "providerId")?;
            let provider = plugin
                .providers
                .get(id)
                .ok_or_else(|| anyhow!("Unknown provider: {id}"))?;
            if kind == "provider.status" {
                provider
                    .status(message["request"].clone(), context.clone())
                    .await
            } else {
                Ok(json!(
                    provider
                        .catalog_cache_key(message["options"].clone(), context.clone())
                        .await?
                ))
            }
        }
        "hook" => {
            let name = string(&message, "name")?;
            let before = match string(&message, "kind")? {
                "before" => true,
                "event" => false,
                _ => bail!("Invalid hook kind"),
            };
            ensure!(
                (if before { crate::BEFORE } else { crate::EVENTS }).contains(&name),
                "Unknown lifecycle hook: {name}"
            );
            let mut value = message["input"].clone();
            let registry = if before {
                &plugin.before
            } else {
                &plugin.events
            };
            if let Some(handlers) = registry.get(name) {
                for handler in handlers {
                    let input = if before {
                        json!({"request":value})
                    } else {
                        value.clone()
                    };
                    match handler(input, context.clone()).await {
                        Ok(next) if before && !next.is_null() => {
                            validate_transform(name, &value, &next)?;
                            value = next;
                        }
                        Ok(_) => {}
                        Err(error) if before => return Err(error),
                        Err(error) => {
                            context
                                .log(format!("Lifecycle hook {name} failed: {error}"))
                                .await?
                        }
                    }
                }
            }
            Ok(if before { value } else { Value::Null })
        }
        _ => bail!("Unsupported plugin request"),
    }
}
fn validate_transform(name: &str, previous: &Value, next: &Value) -> Result<()> {
    ensure!(next.is_object(), "Before hook must return a request object");
    if name == "agent.session_open" {
        for key in [
            "agentId",
            "workspaceId",
            "provider",
            "cwd",
            "reason",
            "purpose",
        ] {
            ensure!(
                next.get(key) == previous.get(key),
                "agent.session_open hooks can only change env"
            );
        }
        ensure!(
            next["env"]
                .as_object()
                .is_some_and(|env| env.values().all(Value::is_string)),
            "Hook env must contain strings"
        );
    }
    if name == "agent.create" {
        ensure!(
            next.pointer("/config/cwd") == previous.pointer("/config/cwd"),
            "agent.create hooks cannot change the workspace directory"
        );
    }
    Ok(())
}
async fn connection(
    provider: Arc<dyn crate::provider::Provider>,
    id: String,
    request: Value,
    mut input: mpsc::Receiver<Value>,
    cancel: CancellationToken,
    mut context: Context,
) {
    context.cancellation = cancel.clone();
    let events = Events {
        connection_id: id.clone().into(),
        output: context.output.clone(),
        cancellation: cancel.clone(),
    };
    let result = tokio::select! {
        biased;
        _=cancel.cancelled()=>Err(anyhow!("Provider connection canceled")),
        result=provider.connect(request.clone(),events,context.clone())=>result,
    };
    let connection = match result {
        Ok(connection) => connection,
        Err(error) => {
            let _=context.output.send(json!({"type":"provider.connect_failed","connectionId":id,"error":error.to_string()})).await;
            return;
        }
    };
    let capabilities = connection.capabilities();
    let valid = request["versions"]
        .as_array()
        .is_some_and(|v| v.iter().any(|v| v.as_u64() == Some(connection.version())))
        && capabilities.iter().all(|capability| {
            request["capabilities"]
                .as_array()
                .is_some_and(|v| v.iter().any(|v| v.as_str() == Some(capability.as_str())))
        });
    let result:Result<()>=async {
        ensure!(valid,"Provider returned an unnegotiated version or capability");
        ensure!(!cancel.is_cancelled(),"Provider connection canceled");
        context.output.send(json!({"type":"provider.connected","connectionId":id,"version":connection.version(),"capabilities":capabilities})).await?;
        loop {
            let message=tokio::select! {biased;_=cancel.cancelled()=>break,message=input.recv()=>match message {Some(value)=>value,None=>break}};
            let acceptance=message["acceptanceId"].clone();
            let result=tokio::select! {biased;_=cancel.cancelled()=>Err(anyhow!("Provider connection closed")),result=connection.send(message["input"].clone())=>result};
            let response=match result {Ok(())=>json!({"type":"provider.accepted","connectionId":id,"acceptanceId":acceptance}),Err(error)=>json!({"type":"provider.rejected","connectionId":id,"acceptanceId":acceptance,"error":error.to_string()})};
            context.output.send(response).await?;
        }
        Ok(())
    }.await;
    cancel.cancel();
    let close = tokio::time::timeout(STOP_TIMEOUT, connection.close())
        .await
        .map_err(|_| anyhow!("Provider close timed out"))
        .and_then(|result| result);
    let mut response = json!({"type":if valid {"provider.closed"}else{"provider.connect_failed"},"connectionId":id});
    if let Err(error) = result.and(close) {
        response["error"] = json!(error.to_string());
    }
    let _ = context.output.send(response).await;
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Plugin request requires {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn frames_reject_oversize_and_truncated_json() {
        let mut bytes = std::io::Cursor::new(vec![b'x'; FRAME_LIMIT + 1]);
        assert!(
            read_frame(&mut bytes)
                .await
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );
        let mut bytes = std::io::Cursor::new(b"{\"type\":\"shutdown\"}");
        assert!(
            read_frame(&mut bytes)
                .await
                .unwrap_err()
                .to_string()
                .contains("Truncated")
        );
        let mut bytes = std::io::Cursor::new(b"{\"n\":1}\n{\"n\":2}\n");
        assert_eq!(read_frame(&mut bytes).await.unwrap().unwrap()["n"], 1);
        assert_eq!(read_frame(&mut bytes).await.unwrap().unwrap()["n"], 2);
    }
    #[test]
    fn session_hooks_cannot_change_identity_or_workspace() {
        let original = json!({"agentId":"one","cwd":"/work","env":{}});
        let mut changed = original.clone();
        changed["env"] = json!({"KEY":"value"});
        assert!(validate_transform("agent.session_open", &original, &changed).is_ok());
        changed["agentId"] = json!("two");
        assert!(validate_transform("agent.session_open", &original, &changed).is_err());
    }
}
