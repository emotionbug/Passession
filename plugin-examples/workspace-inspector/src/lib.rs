//! A real local-file provider: every provider read requires a permission response.
//! RPC reads are explicitly requested by the caller and use the same bounded reader.
use paseo_plugin::provider::{Connection, Events, Metadata, Provider};
use paseo_plugin::{
    Context, Plugin, Result, Settings, SettingsDefinition, anyhow, async_trait, bail, ensure,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{io::AsyncReadExt, sync::Mutex};

pub fn plugin() -> Result<Plugin> {
    let mut plugin = Plugin::new();
    let settings = plugin.settings(SettingsDefinition::new("inspection", 1, |value| {
        ensure!(value.is_object(), "Settings must be an object");
        let max = value
            .get("maxBytes")
            .map_or(Some(65536), Value::as_u64)
            .ok_or_else(|| anyhow!("maxBytes must be an integer"))?;
        ensure!(
            (1..=1048576).contains(&max),
            "maxBytes must be between 1 and 1048576"
        );
        Ok(json!({"maxBytes":max}))
    }))?;
    let rpc_settings = settings.clone();
    plugin.rpc("file.inspect", move |input, _| {
        let settings = rpc_settings.clone();
        async move {
            inspect(
                Path::new(text(&input, "cwd")?),
                text(&input, "path")?,
                limit(&settings).await?,
            )
            .await
        }
    })?;
    let created = Arc::new(AtomicU64::new(0));
    let counter = created.clone();
    plugin.on("agent.created", move |_, context| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::Relaxed);
            context.log("Observed agent.created").await?;
            Ok(Value::Null)
        }
    })?;
    plugin.rpc("lifecycle.counts", move |_, _| {
        let count = created.load(Ordering::Relaxed);
        async move { Ok(json!({"agentsCreated":count})) }
    })?;
    let hook_settings = settings.clone();
    plugin.before("agent.session_open", move |input, _| {
        let settings = hook_settings.clone();
        async move {
            let mut request = input["request"].clone();
            let maximum = limit(&settings).await?;
            request["env"]
                .as_object_mut()
                .ok_or_else(|| anyhow!("Missing session environment"))?
                .insert("PASEO_INSPECT_MAX_BYTES".into(), json!(maximum.to_string()));
            Ok(request)
        }
    })?;
    plugin.provider(Inspector { settings })?;
    Ok(plugin)
}

async fn limit(settings: &Settings) -> Result<u64> {
    let state = settings.read().await?;
    ensure!(
        state["status"] == "ready",
        "Inspection settings need repair: {}",
        state
    );
    state["values"]["maxBytes"]
        .as_u64()
        .ok_or_else(|| anyhow!("Invalid inspection limit"))
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Missing {key}"))
}

/// Reject lexical traversal and symlinks, read at most max+1 bytes, and require UTF-8.
/// This is workspace containment, not a sandbox against concurrently malicious filesystem changes.
async fn inspect(cwd: &Path, relative: &str, max: u64) -> Result<Value> {
    let relative = Path::new(relative);
    ensure!(
        !relative.is_absolute()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "Expected a workspace-relative path without traversal"
    );
    let root = tokio::fs::canonicalize(cwd).await?;
    let mut path = root.clone();
    for component in relative.components() {
        path.push(component);
        ensure!(
            !tokio::fs::symlink_metadata(&path)
                .await?
                .file_type()
                .is_symlink(),
            "Symbolic links are not permitted"
        );
    }
    let path = tokio::fs::canonicalize(path).await?;
    ensure!(path.starts_with(root), "Path escapes workspace");
    let file = tokio::fs::File::open(path).await?;
    ensure!(file.metadata().await?.is_file(), "Expected a regular file");
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() as u64 <= max,
        "File exceeds configured {max}-byte limit"
    );
    let content = std::str::from_utf8(&bytes)?;
    Ok(
        json!({"path":relative,"bytes":bytes.len(),"lines":content.lines().count(),"words":content.split_whitespace().count(),"preview":content.lines().take(10).collect::<Vec<_>>().join("\n")}),
    )
}

struct Inspector {
    settings: Settings,
}
#[async_trait]
impl Provider for Inspector {
    fn metadata(&self) -> Metadata {
        let mut metadata = Metadata::new("workspace-inspector", "Workspace Inspector");
        metadata.description =
            Some("Permission-gated local text file statistics; no model service".into());
        metadata.has_status = true;
        metadata
    }
    async fn status(&self, _: Value, _: Context) -> Result<Value> {
        Ok(json!({"available":true}))
    }
    async fn connect(
        &self,
        request: Value,
        events: Events,
        _: Context,
    ) -> Result<Arc<dyn Connection>> {
        ensure!(
            request["versions"]
                .as_array()
                .is_some_and(|v| v.contains(&json!(1))),
            "Protocol version 1 required"
        );
        let supported = ["prompt.message", "prompt.command", "permission"];
        let capabilities: Vec<String> = supported
            .into_iter()
            .filter(|cap| {
                request["capabilities"]
                    .as_array()
                    .is_some_and(|v| v.contains(&json!(cap)))
            })
            .map(str::to_owned)
            .collect();
        ensure!(
            capabilities.iter().any(|v| v == "permission"),
            "Permission capability required"
        );
        Ok(Arc::new(InspectorConnection {
            events,
            settings: self.settings.clone(),
            capabilities,
            sessions: Mutex::new(HashMap::new()),
        }))
    }
}
struct Session {
    cwd: PathBuf,
    pending: Option<Pending>,
}
struct Pending {
    permission: String,
    turn: String,
    path: String,
}
struct InspectorConnection {
    events: Events,
    settings: Settings,
    capabilities: Vec<String>,
    sessions: Mutex<HashMap<String, Session>>,
}
impl InspectorConnection {
    async fn emit(&self, value: Value) -> Result<()> {
        self.events.emit(value).await
    }
    async fn handle(&self, input: &Value) -> Result<()> {
        let kind = text(input, "type")?;
        if kind == "catalog" {
            return self.emit(json!({"type":"catalog","requestId":text(input,"requestId")?,"catalog":{"models":[{"id":"local-file","label":"Local text files"}],"modes":[],"defaultModel":"local-file"}})).await;
        }
        let id = text(input, "sessionId")?;
        let mut sessions = self.sessions.lock().await;
        if kind == "session.open" {
            ensure!(!sessions.contains_key(id), "Session already open");
            ensure!(
                input["config"]["model"].is_null() || input["config"]["model"] == "local-file",
                "Unknown model"
            );
            let cwd = tokio::fs::canonicalize(text(&input["config"], "cwd")?).await?;
            ensure!(
                tokio::fs::metadata(&cwd).await?.is_dir(),
                "Workspace must be a directory"
            );
            sessions.insert(
                id.into(),
                Session {
                    cwd: cwd.clone(),
                    pending: None,
                },
            );
            self.emit(json!({"type":"session.opened","sessionId":id,"requestId":text(input,"requestId")?,"cwd":cwd,"capabilities":self.capabilities,"restoration":"core"})).await?;
            self.emit(json!({"type":"session.commands","sessionId":id,"commands":[{"name":"inspect","description":"Read text file statistics after approval","argumentHint":"relative/path.txt"}]})).await?;
            return self.emit(json!({"type":"session.ready","sessionId":id,"requestId":text(input,"requestId")?})).await;
        }
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| anyhow!("Unknown session"))?;
        match kind {
            "session.prompt" => {
                ensure!(
                    session.pending.is_none(),
                    "Resolve the pending permission first"
                );
                let prompt = &input["prompt"];
                ensure!(
                    prompt["delivery"] != "steer" && prompt["outputSchema"].is_null(),
                    "Steering and output schemas are unsupported"
                );
                let content = &prompt["input"];
                let path = match text(content, "type")? {
                    "command" => {
                        ensure!(
                            self.capabilities.iter().any(|v| v == "prompt.command"),
                            "Command capability not negotiated"
                        );
                        ensure!(content["name"] == "inspect", "Unknown command");
                        text(content, "arguments")?.trim().to_owned()
                    }
                    "message" => {
                        ensure!(
                            self.capabilities.iter().any(|v| v == "prompt.message"),
                            "Message capability not negotiated"
                        );
                        let parts = content["content"]
                            .as_array()
                            .ok_or_else(|| anyhow!("Missing message content"))?;
                        ensure!(
                            parts.len() == 1 && parts[0]["type"] == "text",
                            "Supply exactly one text path"
                        );
                        text(&parts[0], "text")?.trim().to_owned()
                    }
                    _ => bail!("Unsupported prompt input"),
                };
                ensure!(!path.is_empty(), "Supply a relative file path");
                let pending = Pending {
                    permission: uuid::Uuid::new_v4().to_string(),
                    turn: uuid::Uuid::new_v4().to_string(),
                    path,
                };
                self.emit(json!({"type":"session.prompt_result","sessionId":id,"clientMessageId":text(prompt,"clientMessageId")?,"result":{"type":"turn","turnId":pending.turn}})).await?;
                self.emit(json!({"type":"session.turn","sessionId":id,"turnId":pending.turn,"state":"started"})).await?;
                self.emit(json!({"type":"session.permission","sessionId":id,"request":{"id":pending.permission,"name":"Read workspace file","kind":"tool","description":format!("Inspect {}",pending.path),"detail":{"type":"read","filePath":pending.path},"actions":[{"id":"read","label":"Read file","behavior":"allow"},{"id":"deny","label":"Deny","behavior":"deny"}]}})).await?;
                session.pending = Some(pending);
            }
            "session.permission" => {
                let pending = session
                    .pending
                    .as_ref()
                    .ok_or_else(|| anyhow!("No pending permission"))?;
                ensure!(
                    input["permissionId"] == pending.permission,
                    "Unknown permission"
                );
                let behavior = text(&input["response"], "behavior")?;
                ensure!(
                    behavior == "allow" || behavior == "deny",
                    "Unknown permission response"
                );
                let pending = session.pending.take().unwrap();
                self.emit(json!({"type":"session.permission_resolved","sessionId":id,"permissionId":pending.permission})).await?;
                if behavior == "deny" {
                    self.emit(json!({"type":"session.turn","sessionId":id,"turnId":pending.turn,"state":"canceled"})).await?;
                } else {
                    let result = async {
                        inspect(&session.cwd, &pending.path, limit(&self.settings).await?).await
                    }
                    .await;
                    match result {
                        Ok(report)=> {
                            self.emit(json!({"type":"timeline.item","sessionId":id,"item":{"id":uuid::Uuid::new_v4().to_string(),"type":"assistant_message","text":serde_json::to_string_pretty(&report)?}})).await?;
                            self.emit(json!({"type":"session.turn","sessionId":id,"turnId":pending.turn,"state":"completed"})).await?;
                        },
                        Err(error)=>self.emit(json!({"type":"session.turn","sessionId":id,"turnId":pending.turn,"state":"failed","error":{"message":error.to_string()}})).await?,
                    }
                }
            }
            "session.interrupt" | "session.close" => {
                if let Some(pending) = session.pending.take() {
                    self.emit(json!({"type":"session.permission_resolved","sessionId":id,"permissionId":pending.permission})).await?;
                    self.emit(json!({"type":"session.turn","sessionId":id,"turnId":pending.turn,"state":"canceled"})).await?;
                }
                if kind == "session.close" {
                    sessions.remove(id);
                    self.emit(json!({"type":"session.closed","sessionId":id}))
                        .await?;
                }
                self.emit(json!({"type":"request.completed","requestId":text(input,"requestId")?}))
                    .await?;
            }
            _ => bail!("Unsupported provider operation: {kind}"),
        }
        Ok(())
    }
}
#[async_trait]
impl Connection for InspectorConnection {
    fn version(&self) -> u64 {
        1
    }
    fn capabilities(&self) -> Vec<String> {
        self.capabilities.clone()
    }
    async fn send(&self, input: Value) -> Result<()> {
        if let Err(error) = self.handle(&input).await {
            if let Some(request) = input["requestId"].as_str() {
                return self.emit(json!({"type":"request.failed","requestId":request,"error":{"message":error.to_string()}})).await;
            }
            if input["type"] == "session.prompt" {
                return self.emit(json!({"type":"session.prompt_result","sessionId":input["sessionId"],"clientMessageId":input["prompt"]["clientMessageId"],"result":{"type":"failed","error":{"message":error.to_string()}}})).await;
            }
            return Err(error);
        }
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        self.sessions.lock().await.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn bounded_reader_reports_content_and_rejects_escape_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("sample"), "one two\nthree\n")
            .await
            .unwrap();
        let report = inspect(dir.path(), "sample", 100).await.unwrap();
        assert_eq!(report["words"], 3);
        assert_eq!(report["lines"], 2);
        assert!(inspect(dir.path(), "sample", 3).await.is_err());
        assert!(inspect(dir.path(), "../outside", 100).await.is_err());
        tokio::fs::write(dir.path().join("binary"), [255])
            .await
            .unwrap();
        assert!(inspect(dir.path(), "binary", 100).await.is_err());
    }
}
