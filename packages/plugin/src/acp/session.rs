use super::{
    AcpContext, AcpProvider, ConfigAccess, ConfigChange, Source,
    mapping::{self, Mapping},
    text,
    transport::{self, Rpc},
};
use crate::{Context, Result, anyhow, bail, ensure, provider::Events};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

struct Permission {
    id: Value,
    options: Vec<Value>,
}
pub(super) struct Session {
    pub rpc: Arc<Rpc>,
    pub mapping: StdMutex<Mapping>,
    pub agent: StdMutex<Value>,
    provider: AcpProvider,
    id: String,
    native: StdMutex<String>,
    events: Option<Events>,
    cancel: CancellationToken,
    closing: AtomicBool,
    failed: AtomicBool,
    interrupted: AtomicBool,
    permissions: StdMutex<BTreeMap<String, Permission>>,
    prompt: Mutex<Option<JoinHandle<()>>>,
    pump: Mutex<Option<JoinHandle<()>>>,
    commands: Notify,
}
impl Session {
    pub async fn start(
        provider: AcpProvider,
        launch: Option<&Value>,
        id: &str,
        events: Option<Events>,
        env: &Value,
        context: Context,
        parent: CancellationToken,
    ) -> Result<Arc<Self>> {
        let cancel = parent.child_token();
        let opening = async {
            match &provider.source {
                Source::Connector(connect) => connect(cancel.clone()).await,
                Source::Command(command) => {
                    let (program, args, mut environment) = if let Some(launch) = launch {
                        (
                            text(launch, "command")?.to_owned(),
                            serde_json::from_value::<Vec<String>>(launch["args"].clone())?,
                            Some(serde_json::from_value::<BTreeMap<String, String>>(
                                launch["env"].clone(),
                            )?),
                        )
                    } else {
                        (command[0].clone(), command[1..].to_vec(), None)
                    };
                    let overrides = if env.is_null() {
                        BTreeMap::new()
                    } else {
                        serde_json::from_value::<BTreeMap<String, String>>(env.clone())?
                    };
                    if !overrides.is_empty() {
                        let map = environment.get_or_insert_with(|| {
                            std::env::vars_os()
                                .filter_map(|(k, v)| {
                                    Some((k.into_string().ok()?, v.into_string().ok()?))
                                })
                                .collect()
                        });
                        map.extend(overrides);
                    }
                    transport::spawn(
                        &program,
                        &args,
                        environment.as_ref(),
                        &context,
                        cancel.clone(),
                    )
                    .await
                }
            }
        };
        let stream = tokio::select! {
            _=cancel.cancelled()=>bail!("ACP startup canceled"),
            result=tokio::time::timeout(provider.options.startup_timeout,opening)=>match result {
                Ok(Ok(stream))=>stream,
                Ok(Err(error))=>{cancel.cancel();return Err(error)},
                Err(_)=>{cancel.cancel();bail!("ACP connector startup timed out")},
            }
        };
        let (rpc, mut incoming) = Rpc::start(stream, cancel.clone()).await;
        let runtime = Arc::new(Self {
            rpc,
            mapping: StdMutex::new(Mapping::default()),
            agent: StdMutex::new(json!({})),
            provider,
            id: id.into(),
            native: StdMutex::new(String::new()),
            events,
            cancel,
            closing: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            interrupted: AtomicBool::new(false),
            permissions: StdMutex::new(BTreeMap::new()),
            prompt: Mutex::new(None),
            pump: Mutex::new(None),
            commands: Notify::new(),
        });
        let weak = Arc::downgrade(&runtime);
        let pump = tokio::spawn(async move {
            while let Some(message) = incoming.recv().await {
                let Some(runtime) = weak.upgrade() else { break };
                let result = match message {
                    Ok(message) => runtime.receive(message).await,
                    Err(error) => Err(error),
                };
                if let Err(error) = result {
                    runtime.transport_failed(error.to_string()).await;
                    runtime.cancel.cancel();
                    break;
                }
            }
            if let Some(runtime) = weak.upgrade() {
                runtime
                    .transport_failed("ACP transport closed unexpectedly".into())
                    .await;
            }
        });
        *runtime.pump.lock().await = Some(pump);
        let result=runtime.rpc.request("initialize",json!({"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"paseo","version":env!("CARGO_PKG_VERSION")}}),Some(runtime.provider.options.startup_timeout)).await;
        match result {
            Ok(value) if value["protocolVersion"] == 1 => {
                *runtime.agent.lock().unwrap() = value
                    .get("agentCapabilities")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(runtime)
            }
            Ok(value) => {
                let _ = runtime.close().await;
                bail!(
                    "ACP server negotiated unsupported protocol version: {}",
                    value["protocolVersion"]
                )
            }
            Err(error) => {
                let _ = runtime.close().await;
                Err(error)
            }
        }
    }
    fn context(self: &Arc<Self>) -> AcpContext {
        AcpContext {
            session_id: self.id.clone(),
            config: ConfigAccess {
                session: Arc::downgrade(self),
            },
        }
    }
    fn native(&self) -> Result<String> {
        let id = self.native.lock().unwrap().clone();
        ensure!(!id.is_empty(), "ACP session is not open");
        Ok(id)
    }
    async fn emit(&self, event: Value) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(events) = &self.events {
            events.emit(event).await?;
        }
        Ok(())
    }
    async fn emit_all(&self, events: Vec<Value>) -> Result<()> {
        for event in events {
            self.emit(event).await?;
        }
        Ok(())
    }
    async fn receive(&self, message: Value) -> Result<()> {
        ensure!(message["jsonrpc"] == "2.0", "Invalid ACP JSON-RPC version");
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return self.rpc.resolve(&message);
        };
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        if let Some(request_id) = message.get("id") {
            if method != "session/request_permission" {
                return self
                    .rpc
                    .reply(
                        request_id.clone(),
                        Err(anyhow!("Unsupported ACP client method: {method}")),
                    )
                    .await;
            }
            if params["sessionId"].as_str() != Some(self.native()?.as_str()) {
                return self
                    .rpc
                    .reply(
                        request_id.clone(),
                        Err(anyhow!("Unknown ACP permission session")),
                    )
                    .await;
            }
            let tool = &params["toolCall"];
            let permission_id = format!("permission:{}", text(tool, "toolCallId")?);
            let options = params["options"]
                .as_array()
                .ok_or_else(|| anyhow!("ACP permission options must be an array"))?
                .clone();
            let mut actions = Vec::with_capacity(options.len());
            for option in &options {
                actions.push(json!({"id":text(option,"optionId")?,"label":text(option,"name")?,"behavior":behavior(option)?}));
            }
            {
                let mut pending = self.permissions.lock().unwrap();
                ensure!(pending.len() < 256, "Too many pending ACP permissions");
                ensure!(
                    !pending.contains_key(&permission_id),
                    "Duplicate ACP permission: {permission_id}"
                );
                pending.insert(
                    permission_id.clone(),
                    Permission {
                        id: request_id.clone(),
                        options,
                    },
                );
            }
            let mut request = json!({"id":permission_id,"name":tool.get("name").or_else(||tool.get("title")).and_then(Value::as_str).unwrap_or("Tool"),"kind":"tool","input":tool.get("rawInput").filter(|v|v.is_object()).cloned().unwrap_or_else(||json!({})),"actions":actions});
            if let Some(title) = tool.get("title").filter(|v| v.is_string()) {
                request["title"] = title.clone();
            }
            return self
                .emit(json!({"type":"session.permission","sessionId":self.id,"request":request}))
                .await;
        }
        let events = if method == "session/update" {
            if params["sessionId"].as_str() != Some(self.native.lock().unwrap().as_str()) {
                return Ok(());
            }
            self.mapping.lock().unwrap().update(
                &params["update"],
                &self.id,
                &self.provider.transformers,
            )?
        } else {
            let mut events = Vec::new();
            for transformer in &self.provider.transformers {
                for update in transformer.notification(method, &params, &self.id)? {
                    events.extend(self.mapping.lock().unwrap().vendor(update, &self.id));
                }
            }
            events
        };
        if self.mapping.lock().unwrap().commands_received {
            self.commands.notify_waiters();
        }
        self.emit_all(events).await
    }
    async fn transport_failed(&self, message: String) {
        if self.closing.load(Ordering::Acquire) || self.failed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.permissions.lock().unwrap().clear();
        self.commands.notify_waiters();
        if !self.native.lock().unwrap().is_empty() {
            let _=self.emit(json!({"type":"session.runtime_failed","sessionId":self.id,"error":{"message":message}})).await;
        }
    }
    pub async fn discover(self: &Arc<Self>, cwd: Option<&str>) -> Result<Value> {
        let mut catalog = if cwd.is_none() && self.provider.options.workspace_only_catalog {
            json!({"models":[],"modes":[]})
        } else {
            let cwd = match cwd {
                Some(v) => v.to_owned(),
                None => std::env::current_dir()?.to_string_lossy().into_owned(),
            };
            let response = self
                .rpc
                .request(
                    "session/new",
                    json!({"cwd":cwd,"mcpServers":[]}),
                    Some(self.provider.options.startup_timeout),
                )
                .await?;
            *self.native.lock().unwrap() = text(&response, "sessionId")?.into();
            self.apply_snapshot(&response)?;
            self.mapping.lock().unwrap().catalog()
        };
        for transformer in &self.provider.transformers {
            catalog = transformer.discover(catalog, self.context()).await?;
        }
        Ok(catalog)
    }
    fn apply_snapshot(&self, response: &Value) -> Result<()> {
        let options = match response.get("configOptions") {
            None | Some(Value::Null) => Vec::new(),
            Some(v) => v
                .as_array()
                .ok_or_else(|| anyhow!("ACP configOptions must be an array"))?
                .clone(),
        };
        let mut state = self.mapping.lock().unwrap();
        state.modes = response.get("modes").cloned().unwrap_or(Value::Null);
        state.options = options;
        Ok(())
    }
    pub async fn open(self: &Arc<Self>, input: &Value, capabilities: &[String]) -> Result<()> {
        let config = &input["config"];
        let cwd = text(config, "cwd")?;
        let mut params =
            json!({"cwd":cwd,"mcpServers":mapping::mcp_servers(config)?,"_meta":{"_paseo":{}}});
        for key in ["systemPrompt", "providerOptions", "toolPolicy", "persist"] {
            if let Some(value) = config.get(key) {
                params["_meta"]["_paseo"][key] = value.clone();
            }
        }
        let native = input["persistence"]["data"]["sessionId"].as_str();
        if input
            .get("persistence")
            .is_some_and(|value| !value.is_null())
        {
            ensure!(
                input["persistence"]["version"] == 1 && native.is_some_and(|id| !id.is_empty()),
                "Unsupported ACP persistence record"
            );
        }
        let response = if let Some(id) = native {
            ensure!(
                input["persistence"]["version"] == 1 && !id.is_empty(),
                "Unsupported ACP persistence record"
            );
            *self.native.lock().unwrap() = id.into();
            self.mapping.lock().unwrap().replay = Some(input["history"] == "replay");
            params["sessionId"] = json!(id);
            let response = self
                .rpc
                .request(
                    "session/load",
                    params,
                    Some(self.provider.options.startup_timeout),
                )
                .await;
            self.mapping.lock().unwrap().replay = None;
            response?
        } else {
            let response = self
                .rpc
                .request(
                    "session/new",
                    params,
                    Some(self.provider.options.startup_timeout),
                )
                .await?;
            *self.native.lock().unwrap() = text(&response, "sessionId")?.into();
            response
        };
        self.apply_snapshot(&response)?;
        self.apply_changes(config).await?;
        let configurable = {
            let state = self.mapping.lock().unwrap();
            !state.modes.is_null()
                || !state.options.is_empty()
                || !self.provider.transformers.is_empty()
        };
        let caps: Vec<_> = capabilities
            .iter()
            .filter(|v| v.as_str() != "session.configure" || configurable)
            .cloned()
            .collect();
        let mut opened = json!({"type":"session.opened","requestId":input["requestId"],"sessionId":self.id,"capabilities":caps,"restoration":"core","persistence":{"version":1,"data":{"sessionId":self.native()?}},"cwd":cwd});
        if let Some(title) = config.get("title") {
            opened["title"] = title.clone();
        }
        self.emit(opened).await?;
        self.emit_config().await?;
        if self.provider.options.wait_for_initial_commands {
            let rpc_cancel = self.rpc.cancellation();
            let wait = async {
                loop {
                    let notified = self.commands.notified();
                    tokio::pin!(notified);
                    notified.as_mut().enable();
                    if self.mapping.lock().unwrap().commands_received {
                        break;
                    }
                    tokio::select! {_=self.cancel.cancelled()=>break,_=rpc_cancel.cancelled()=>break,_=notified=>{}}
                }
            };
            let _ =
                tokio::time::timeout(self.provider.options.initial_commands_timeout, wait).await;
        }
        ensure!(
            !self.rpc.cancellation().is_cancelled(),
            "ACP transport closed before session readiness"
        );
        self.emit(
            json!({"type":"session.ready","sessionId":self.id,"requestId":input["requestId"]}),
        )
        .await
    }
    async fn emit_config(&self) -> Result<()> {
        let config = {
            let state = self.mapping.lock().unwrap();
            if state.config_transaction {
                return Ok(());
            }
            state.config()
        };
        self.emit(json!({"type":"session.config","sessionId":self.id,"config":config}))
            .await
    }
    pub async fn set_option(&self, id: &str, value: Value) -> Result<()> {
        ensure!(
            value.is_string() || value.is_boolean(),
            "ACP configuration {id} accepts only string or boolean values"
        );
        let mut params = json!({"sessionId":self.native()?,"configId":id,"value":value});
        if value.is_boolean() {
            params["type"] = json!("boolean");
        }
        let response = self
            .rpc
            .request(
                "session/set_config_option",
                params,
                Some(self.provider.options.startup_timeout),
            )
            .await?;
        let options = response["configOptions"]
            .as_array()
            .ok_or_else(|| anyhow!("ACP configuration response omitted configOptions"))?
            .clone();
        self.mapping.lock().unwrap().options = options;
        Ok(())
    }
    async fn change(self: &Arc<Self>, change: ConfigChange) -> Result<()> {
        for transformer in &self.provider.transformers {
            if transformer.configure(&change, self.context()).await? {
                return Ok(());
            }
        }
        let category = match &change {
            ConfigChange::Model(_) => "model",
            _ => "thought_level",
        };
        if let ConfigChange::Mode(value) = change {
            ensure!(
                !self.mapping.lock().unwrap().modes.is_null(),
                "ACP session does not expose mode configuration"
            );
            let mode = value
                .as_str()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| anyhow!("ACP mode cannot be cleared"))?;
            self.rpc
                .request(
                    "session/set_mode",
                    json!({"sessionId":self.native()?,"modeId":mode}),
                    Some(self.provider.options.startup_timeout),
                )
                .await?;
            self.mapping.lock().unwrap().modes["currentModeId"] = value;
            return Ok(());
        }
        let (id, value) = match change {
            ConfigChange::Setting { id, value } => (id, value),
            ConfigChange::Model(value) | ConfigChange::Thinking(value) => {
                let id = self
                    .mapping
                    .lock()
                    .unwrap()
                    .options
                    .iter()
                    .find(|v| v["category"] == category)
                    .and_then(|v| v["id"].as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| {
                        anyhow!("ACP session does not expose {category} configuration")
                    })?;
                (id, value)
            }
            ConfigChange::Mode(_) => unreachable!(),
        };
        self.set_option(&id, value).await
    }
    async fn apply_changes(self: &Arc<Self>, changes: &Value) -> Result<()> {
        for (key, target) in [("model", 0), ("mode", 1), ("thinkingOption", 2)] {
            if let Some(value) = changes.get(key) {
                let change = match target {
                    0 => ConfigChange::Model(value.clone()),
                    1 => ConfigChange::Mode(value.clone()),
                    _ => ConfigChange::Thinking(value.clone()),
                };
                self.change(change).await?;
            }
        }
        if let Some(settings) = changes.get("settings") {
            for (id, value) in settings
                .as_object()
                .ok_or_else(|| anyhow!("ACP settings must be an object"))?
            {
                self.change(ConfigChange::Setting {
                    id: id.clone(),
                    value: value.clone(),
                })
                .await?;
            }
        }
        Ok(())
    }
    pub async fn configure(self: &Arc<Self>, changes: &Value) -> Result<()> {
        let (previous_modes, previous_options) = {
            let mut state = self.mapping.lock().unwrap();
            state.config_transaction = true;
            state.staged_config = None;
            (state.modes.clone(), state.options.clone())
        };
        let changed = self.apply_changes(changes).await;
        if let Err(error) = changed {
            let rollback = async {
                let current_mode = self.mapping.lock().unwrap().modes["currentModeId"].clone();
                if let Some(mode) = previous_modes
                    .get("currentModeId")
                    .filter(|v| !v.is_null() && **v != current_mode)
                {
                    self.rpc
                        .request(
                            "session/set_mode",
                            json!({"sessionId":self.native()?,"modeId":mode}),
                            Some(self.provider.options.startup_timeout),
                        )
                        .await?;
                }
                let current = self.mapping.lock().unwrap().values();
                for option in &previous_options {
                    let id = text(option, "id")?;
                    if current[id] != option["currentValue"] {
                        self.set_option(id, option["currentValue"].clone()).await?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            {
                let mut state = self.mapping.lock().unwrap();
                state.config_transaction = false;
                state.staged_config = None;
                state.modes = previous_modes;
                state.options = previous_options;
            }
            if let Err(rollback) = rollback {
                self.transport_failed(format!(
                    "ACP configuration failed ({error}); rollback failed ({rollback})"
                ))
                .await;
                let _ = self.close().await;
                bail!("ACP configuration rollback failed: {rollback}");
            }
            return Err(error);
        }
        let config = {
            let mut state = self.mapping.lock().unwrap();
            state.config_transaction = false;
            state.staged_config.take().unwrap_or_else(|| state.config())
        };
        self.emit(json!({"type":"session.config","sessionId":self.id,"config":config}))
            .await
    }
    pub async fn permission(&self, id: &str, response: &Value) -> Result<()> {
        let selected = {
            let pending = self.permissions.lock().unwrap();
            let permission = pending
                .get(id)
                .ok_or_else(|| anyhow!("Unknown ACP permission: {id}"))?;
            let requested = text(response, "behavior")?;
            ensure!(
                matches!(requested, "allow" | "deny"),
                "Invalid ACP permission behavior"
            );
            let explicit = response.get("selectedActionId").and_then(Value::as_str);
            let option = permission.options.iter().find(|option| {
                behavior(option).ok() == Some(requested)
                    && explicit.is_none_or(|id| option["optionId"] == id)
            });
            ensure!(
                explicit.is_none() || option.is_some(),
                "ACP permission action does not match requested behavior"
            );
            option.map(|option| option["optionId"].clone())
        };
        let pending = self
            .permissions
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| anyhow!("ACP permission already resolved"))?;
        let outcome = match selected {
            Some(id) => json!({"outcome":"selected","optionId":id}),
            None => json!({"outcome":"cancelled"}),
        };
        self.rpc
            .reply(pending.id, Ok(json!({"outcome":outcome})))
            .await?;
        self.emit(
            json!({"type":"session.permission_resolved","sessionId":self.id,"permissionId":id}),
        )
        .await
    }
    async fn cancel_permissions(&self) -> Result<()> {
        let permissions = std::mem::take(&mut *self.permissions.lock().unwrap());
        for (id, pending) in permissions {
            self.rpc
                .reply(pending.id, Ok(json!({"outcome":{"outcome":"cancelled"}})))
                .await?;
            self.emit(
                json!({"type":"session.permission_resolved","sessionId":self.id,"permissionId":id}),
            )
            .await?;
        }
        Ok(())
    }
    pub async fn prompt(self: &Arc<Self>, prompt: Value) -> Result<()> {
        let contents = mapping::prompt(&prompt)?;
        let client = text(&prompt, "clientMessageId")?;
        let mut active = self.prompt.lock().await;
        if let Some(mut previous) = active.take() {
            if !previous.is_finished() {
                if let Err(error) = self.interrupt_inner().await {
                    previous.abort();
                    let _ = previous.await;
                    let _ = self.rpc.close().await;
                    return Err(error);
                }
            }
            if tokio::time::timeout(self.provider.options.cancellation_timeout, &mut previous)
                .await
                .is_err()
            {
                previous.abort();
                let _ = previous.await;
                self.rpc.close().await?;
                bail!("ACP agent did not settle interrupted turn");
            }
        }
        self.interrupted.store(false, Ordering::Release);
        self.mapping.lock().unwrap().begin_turn();
        let turn = format!("acp:{client}");
        let text = contents
            .iter()
            .filter(|v| v["type"] == "text")
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.emit(json!({"type":"timeline.item","sessionId":self.id,"item":{"type":"user_message","id":client,"clientMessageId":client,"text":text}})).await?;
        self.emit(json!({"type":"session.prompt_result","sessionId":self.id,"clientMessageId":client,"result":{"type":"turn","turnId":turn}})).await?;
        self.emit(
            json!({"type":"session.turn","sessionId":self.id,"turnId":turn,"state":"started"}),
        )
        .await?;
        let native = self.native()?;
        let runtime = self.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        *active = Some(tokio::spawn(async move {
            let request = runtime.rpc.request(
                "session/prompt",
                json!({"sessionId":native,"prompt":contents}),
                None,
            );
            tokio::pin!(request);
            let mut started = Some(started);
            // Poll request before allowing an interrupt to queue its write.
            let response = std::future::poll_fn(|cx| {
                let result = request.as_mut().poll(cx);
                if let Some(started) = started.take() {
                    let _ = started.send(());
                }
                result
            })
            .await;
            let (state, error) = match response {
                Ok(value) => (
                    if value["stopReason"] == "cancelled"
                        || runtime.interrupted.load(Ordering::Acquire)
                    {
                        "canceled"
                    } else {
                        "completed"
                    },
                    None,
                ),
                Err(error) if runtime.interrupted.load(Ordering::Acquire) => {
                    ("canceled", Some(error.to_string()))
                }
                Err(error) => ("failed", Some(error.to_string())),
            };
            let terminal = runtime
                .mapping
                .lock()
                .unwrap()
                .finish_turn(state, &runtime.id);
            let _ = runtime.emit_all(terminal).await;
            let _ = runtime.cancel_permissions().await;
            let mut event =
                json!({"type":"session.turn","sessionId":runtime.id,"turnId":turn,"state":state});
            if state == "failed" {
                event["error"] =
                    json!({"message":error.unwrap_or_else(||"ACP prompt failed".into())});
            }
            let _ = runtime.emit(event).await;
        }));
        ready
            .await
            .map_err(|_| anyhow!("ACP prompt task exited before admission"))?;
        Ok(())
    }
    pub async fn interrupt(&self) -> Result<()> {
        let _active = self.prompt.lock().await;
        self.interrupt_inner().await
    }
    async fn interrupt_inner(&self) -> Result<()> {
        self.interrupted.store(true, Ordering::Release);
        self.rpc
            .notify("session/cancel", json!({"sessionId":self.native()?}))
            .await?;
        self.cancel_permissions().await
    }
    pub async fn close(&self) -> Result<()> {
        if self.closing.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let native = self.native.lock().unwrap().clone();
        self.interrupted.store(true, Ordering::Release);
        if !native.is_empty() && !self.rpc.cancellation().is_cancelled() {
            let _ = self
                .rpc
                .notify("session/cancel", json!({"sessionId":native}))
                .await;
            let _ = self.cancel_permissions().await;
            let _ = self
                .rpc
                .request(
                    "session/close",
                    json!({"sessionId":native}),
                    Some(Duration::from_secs(1)),
                )
                .await;
        }
        self.cancel.cancel();
        let result = self.rpc.close().await;
        self.permissions.lock().unwrap().clear();
        self.commands.notify_waiters();
        if let Some(mut prompt) = self.prompt.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(1), &mut prompt)
                .await
                .is_err()
            {
                prompt.abort();
                let _ = prompt.await;
            }
        }
        if let Some(mut pump) = self.pump.lock().await.take() {
            if tokio::time::timeout(Duration::from_secs(1), &mut pump)
                .await
                .is_err()
            {
                pump.abort();
                let _ = pump.await;
            }
        }
        result
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
fn behavior(option: &Value) -> Result<&'static str> {
    match text(option, "kind")? {
        "allow_once" | "allow_always" => Ok("allow"),
        "reject_once" | "reject_always" => Ok("deny"),
        other => bail!("Unsupported ACP permission option kind: {other}"),
    }
}
