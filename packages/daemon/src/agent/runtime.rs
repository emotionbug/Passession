use super::*;

impl AgentManager {
    fn launch_config(&self, mut config: Value) -> Result<Value> {
        object(&mut config).remove("daemonAppendSystemPrompt");
        if let Some(prompt) = self
            .app()?
            .config
            .snapshot()
            .get("appendSystemPrompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            config["daemonAppendSystemPrompt"] = json!(prompt);
        }
        Ok(config)
    }
    fn provider_environment(&self, config: &Value) -> Result<BTreeMap<String, String>> {
        let app = self.app()?;
        let mut env = app.config.environment();
        // LaunchContext carries a complete environment and adapters apply it
        // after their provider defaults. Preserve those defaults here before
        // workspace, per-session, daemon identity, and plugin overrides.
        env.extend(
            app.providers
                .entry(required(config, "provider")?)?
                .spec
                .env
                .clone(),
        );
        Ok(env)
    }
    async fn session_environment(
        &self,
        id: &str,
        config: &Value,
        workspace: Option<&Value>,
        reason: &str,
        purpose: &str,
        env: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>> {
        let app = self.app()?;
        let request = json!({"agentId":id,"workspaceId":workspace.cloned().unwrap_or(Value::Null),"provider":config["provider"],"cwd":config["cwd"],"reason":reason,"purpose":purpose,"env":env});
        let transformed = app.plugins.before("agent.session_open", request).await?;
        serde_json::from_value::<BTreeMap<String, String>>(transformed["env"].clone())
            .context("Invalid plugin session environment")
    }
    async fn open_context(
        &self,
        id: &str,
        config: &Value,
        workspace: Option<&Value>,
        reason: &str,
        purpose: &str,
        env: BTreeMap<String, String>,
    ) -> Result<crate::providers::api::LaunchContext> {
        let env = self
            .session_environment(id, config, workspace, reason, purpose, env)
            .await?;
        self.app()?.mcp.launch_context(id, config, env).await
    }
    /// A provider history read beside (or without) the agent's own session. It
    /// holds no MCP grant: issuing one would revoke the live session's grant.
    pub(super) async fn history_context(
        &self,
        id: &str,
        config: &Value,
        workspace: Option<&Value>,
    ) -> Result<crate::providers::api::LaunchContext> {
        let app = self.app()?;
        let mut env = self.provider_environment(config)?;
        if let Some(workspace) = workspace.and_then(Value::as_str) {
            env.extend(app.projects.teardown_environment(workspace).await?);
        }
        env.insert("PASEO_AGENT_ID".into(), id.into());
        env.insert("PASEO_AGENT_CWD".into(), required(config, "cwd")?.into());
        let env = self
            .session_environment(id, config, workspace, "resume", "history", env)
            .await?;
        Ok(crate::providers::api::LaunchContext {
            agent_id: id.into(),
            home: app.home.clone(),
            executable: app.executable.clone(),
            env,
            mcp_servers: json!({}),
            tool_catalog: json!([]),
            tool_manifest: json!([]),
            tool_executor: None,
            cancel: app.cancel.child_token(),
        })
    }
    /// The stored launch configuration: provider handle metadata, then the
    /// record's config and last mode, then caller overrides.
    pub(super) fn stored_launch_config(
        record: &Value,
        handle: Option<&Value>,
        provider: &str,
        overrides: &Value,
    ) -> Value {
        let mut config = handle
            .and_then(|h| h.get("metadata"))
            .cloned()
            .unwrap_or(json!({}));
        merge(&mut config, &record["config"]);
        if let Some(mode) = record.get("lastModeId").filter(|v| !v.is_null()) {
            config["modeId"] = mode.clone();
        }
        merge(&mut config, overrides);
        config["provider"] = json!(provider);
        if config.get("cwd").is_none() {
            config["cwd"] = record["cwd"].clone();
        }
        config
    }
    pub(super) async fn create_session(
        &self,
        mut config: Value,
        id: Option<String>,
        mut options: Value,
    ) -> Result<Value> {
        if self.closing.load(Ordering::Acquire) {
            bail!("Agent manager is shutting down");
        }
        let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        uuid::Uuid::parse_str(&id).context("createAgent: agentId must be a UUID")?;
        let lane = self.lane(&id).await;
        let _lane = lane.lock().await;
        if self.agent_snapshot(&id).await?.is_some() {
            bail!("Agent '{id}' already exists");
        }
        let app = self.app()?;
        if config["internal"] != true {
            let mut request = json!({"config":config});
            if let Some(env) = options.get("env") {
                request["env"] = env.clone();
            }
            let transformed = app.plugins.before("agent.create", request).await?;
            config = transformed["config"].clone();
            if let Some(env) = transformed.get("env") {
                options["env"] = env.clone();
            } else {
                object(&mut options).remove("env");
            }
        }
        let config = self.normalize_config(config, true).await?;
        let provider = required(&config, "provider")?;
        let factory = app.providers.get(provider)?;
        if !factory
            .is_available(json!({}), app.cancel.child_token())
            .await?
        {
            bail!("Provider '{provider}' is not available. Please ensure the CLI is installed.");
        }
        let timestamp = now();
        let mut snapshot = json!({"id":id,"provider":provider,"cwd":config["cwd"],"config":config,"createdAt":timestamp,"updatedAt":timestamp,"lastUserMessageAt":null,"title":options.get("initialTitle").or_else(||config.get("title")).cloned().unwrap_or(Value::Null),"labels":options.get("labels").cloned().unwrap_or(json!({})),"internal":config["internal"]==true,"persistence":null,"currentModeId":null,"requiresAttention":false,"attentionReason":null,"attentionTimestamp":null,"attention":{"requiresAttention":false},"archivedAt":null});
        for field in ["workspaceId", "owner"] {
            if let Some(value) = options.get(field) {
                snapshot[field] = value.clone();
            }
        }
        let (cell, receiver) = self.make_cell(&id, snapshot, true).await?;
        *cell.task.lock().await = Some(self.start_events(&id, &cell, receiver));
        let result = async {
            let mut env = self.provider_environment(&config)?;
            if let Some(workspace) = text(&options, "workspaceId") {
                env.extend(app.projects.teardown_environment(workspace).await?);
            }
            if let Some(overrides) = options.get("env").and_then(Value::as_object) {
                for (key, value) in overrides {
                    if let Some(value) = value.as_str() {
                        env.insert(key.clone(), value.to_owned());
                    }
                }
            }
            env.insert("PASEO_AGENT_ID".into(), id.clone());
            env.insert("PASEO_AGENT_CWD".into(), required(&config, "cwd")?.into());
            let context = self
                .open_context(
                    &id,
                    &config,
                    options.get("workspaceId"),
                    "create",
                    "interactive",
                    env,
                )
                .await?;
            let launch = self.launch_config(config.clone())?;
            let session = factory
                .create_session(launch, context, options.clone(), cell.sink.clone())
                .await?;
            let snapshot = self
                .register_session(&id, &cell, session, None, false)
                .await?;
            if snapshot["internal"] != true {
                let _ = app
                    .plugins
                    .emit("agent.created", json!({"agent":hook_agent(&snapshot)}))
                    .await;
            }
            Ok(snapshot)
        }
        .await;
        if result.is_err() {
            self.discard_failed_registration(&id, &cell).await;
        }
        result
    }
    pub(super) async fn discard_failed_registration(&self, id: &str, cell: &Arc<AgentCell>) {
        let _mutation = cell.mutation.lock().await;
        {
            let mut data = cell.data.lock().await;
            data.owner_ended = true;
            cell.owner_ended.cancel();
        }
        cell.cancel.cancel();
        let session = cell.data.lock().await.session.take();
        if let Some(session) = session
            && let Err(error) = session.close().await {
                tracing::warn!(agent_id=id,%error,"Failed to close unregistered provider session");
            }
        self.agents.write().await.remove(id);
        if let Ok(app) = self.app() {
            app.mcp.revoke_agent(id).await;
        }
        if let Some(task) = cell.task.lock().await.take() {
            let _ = task.await;
        }
    }
    async fn register_session(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        session: Arc<dyn AgentSession>,
        persistence: Option<Value>,
        restoring: bool,
    ) -> Result<Value> {
        let _mutation = cell.mutation.lock().await;
        {
            cell.data.lock().await.session = Some(session.clone());
        }
        if self.closing.load(Ordering::Acquire) || cell.cancel.is_cancelled() {
            bail!("Agent manager is shutting down");
        }
        self.drain_events(cell).await?;
        let capabilities = session.capabilities().clone();
        let config = cell.data.lock().await.snapshot["config"].clone();
        if config["mcpServers"]
            .as_object()
            .is_some_and(|servers| !servers.is_empty())
            && capabilities["supportsMcpServers"] != true
        {
            bail!(
                "Provider '{}' does not support MCP servers",
                session.provider()
            );
        }
        let modes = session.available_modes().await.unwrap_or_default();
        let mode = session.current_mode().await.unwrap_or(None);
        let info = session.runtime_info().await.ok();
        let prime =
            !session.initial_timeline().is_empty() && !cell.data.lock().await.history_primed;
        if prime {
            self.hydrate_history(id, cell, false).await?;
        }
        let _commit = cell.commit.lock().await;
        if cell.data.lock().await.owner_ended {
            bail!("Agent provider session ended during registration");
        }
        let mut snapshot = cell.data.lock().await.snapshot.clone();
        snapshot["capabilities"] = capabilities;
        snapshot["features"] = json!(session.features().as_ref());
        snapshot["availableModes"] = json!(modes);
        snapshot["currentModeId"] = json!(mode);
        if let Some(mode) = mode {
            snapshot["config"]["modeId"] = json!(mode);
        }
        if let Some(info) = info {
            snapshot["runtimeInfo"] = info;
        }
        if let Some(handle) = session.describe_persistence().or(persistence) {
            snapshot["persistence"] = with_cwd(handle, &snapshot["cwd"]);
        }
        let permissions = session.pending_permissions();
        if !permissions.is_empty() {
            snapshot["pendingPermissions"] = json!(permissions);
        }
        if snapshot["lifecycle"] == "initializing" {
            snapshot["lifecycle"] = json!("idle");
            snapshot["status"] = json!("idle");
        }
        // registerSession: restoring keeps the stored activity stamp so reopening a chat
        // does not rewrite the workspace's "last used" ordering.
        if !restoring {
            snapshot["updatedAt"] = json!(next_updated_at(&snapshot["updatedAt"]));
        }
        if !prime {
            for entry in session.initial_timeline() {
                for item in
                    notifications::project_agent_messages(entry.get("item").unwrap_or(entry))
                {
                    self.append_to_cell(id, cell, item, entry).await?;
                }
            }
        }
        if self.closing.load(Ordering::Acquire) || cell.cancel.is_cancelled() {
            bail!("Agent manager is shutting down");
        }
        self.persist_snapshot(&mut snapshot, !restoring).await?;
        {
            let mut data = cell.data.lock().await;
            data.ready = true;
            data.snapshot = snapshot.clone();
        }
        let _ = self
            .events
            .send(json!({"type":"agent_state","agent":snapshot}));
        Ok(snapshot)
    }
    pub(super) async fn resume(
        &self,
        id: &str,
        overrides: Value,
        force_history: bool,
    ) -> Result<Value> {
        let lane = self.lane(id).await;
        let _lane = lane.lock().await;
        if let Ok(cell) = self.cell(id).await {
            let data = cell.data.lock().await;
            if data.serves_runtime()
                && !force_history
                && overrides.as_object().is_none_or(|o| o.is_empty())
            {
                return Ok(data.snapshot.clone());
            }
        }
        self.resume_unlocked(id, overrides, force_history, "resume")
            .await
    }
    pub(super) async fn reload(
        &self,
        id: &str,
        overrides: Value,
        force_history: bool,
    ) -> Result<Value> {
        let lane = self.lane(id).await;
        let _lane = lane.lock().await;
        self.resume_unlocked(id, overrides, force_history, "refresh")
            .await
    }
    async fn resume_unlocked(
        &self,
        id: &str,
        overrides: Value,
        force_history: bool,
        reason: &str,
    ) -> Result<Value> {
        if self.closing.load(Ordering::Acquire) {
            bail!("Agent manager is shutting down");
        }
        // Child reads in flight finish (and apply) before a parent session can
        // start publishing live child rows; new child reads wait for this.
        let _starting = self.child_read_gate(id).await.write_owned().await;
        let mut record = self
            .agent_snapshot(id)
            .await?
            .ok_or_else(|| anyhow!("Agent not found: {id}"))?;
        let stored_provider = text(&record, "provider").unwrap_or("").to_owned();
        if self.app()?.providers.spec(&stored_provider).is_none() {
            bail!("Agent {id} references unavailable provider '{stored_provider}'");
        }
        let existing = { self.agents.read().await.get(id).cloned() };
        if let Some(cell) = existing {
            self.close_cell(id, &cell).await?;
        }
        let handle = record.get("persistence").filter(|v| v.is_object()).cloned();
        if handle.is_none() {
            // agent-loading.ts: without a provider handle this starts the agent's first
            // session, so stored attention is not carried over.
            record["requiresAttention"] = json!(false);
            record["attentionReason"] = Value::Null;
            record["attentionTimestamp"] = Value::Null;
            record["attention"] = json!({"requiresAttention":false});
        }
        let provider = handle
            .as_ref()
            .and_then(|h| text(h, "provider"))
            .or_else(|| text(&record, "provider"))
            .ok_or_else(|| anyhow!("Agent has no provider"))?
            .to_owned();
        let config = Self::stored_launch_config(&record, handle.as_ref(), &provider, &overrides);
        let history = record.get("archivedAt").is_some_and(|v| !v.is_null());
        let config = self.normalize_config_for(config, true, history).await?;
        record["config"] = config.clone();
        record["cwd"] = config["cwd"].clone();
        record["_historyOnly"] = json!(history);
        let app = self.app()?;
        let factory = app.providers.get(&provider)?;
        if !factory
            .is_available(json!({}), app.cancel.child_token())
            .await?
        {
            bail!("Provider '{provider}' is not available. Please ensure the CLI is installed.");
        }
        let mut env = self.provider_environment(&config)?;
        if let Some(workspace) = text(&record, "workspaceId") {
            env.extend(app.projects.teardown_environment(workspace).await?);
        }
        env.insert("PASEO_AGENT_ID".into(), id.into());
        env.insert("PASEO_AGENT_CWD".into(), required(&config, "cwd")?.into());
        let workspace = record.get("workspaceId").cloned();
        let record_labels = record.get("labels").cloned().unwrap_or(json!({}));
        let record_owner = record.get("owner").filter(|owner| !owner.is_null()).cloned();
        let config_internal = config["internal"] == true;
        let (cell, receiver) = self.make_cell(id, record, false).await?;
        *cell.task.lock().await = Some(self.start_events(id, &cell, receiver));
        let result = async {
            let context = self
                .open_context(
                    id,
                    &config,
                    workspace.as_ref(),
                    reason,
                    if history { "history" } else { "interactive" },
                    env,
                )
                .await?;
            let launch = self.launch_config(config)?;
            let session = if let Some(handle) = handle.as_ref() {
                let options = {
                    let timeline = cell
                        .timeline
                        .lock()
                        .map_err(|_| anyhow!("Timeline lock poisoned"))?;
                    resume_options(history, &timeline)
                };
                factory
                    .resume_session(handle.clone(), launch, context, options, cell.sink.clone())
                    .await?
            } else {
                let mut options = json!({"labels":record_labels,"workspaceId":workspace});
                if let Some(owner) = &record_owner {
                    options["owner"] = owner.clone();
                }
                if config_internal {
                    options["persistSession"] = json!(false);
                }
                factory
                    .create_session(launch, context, options, cell.sink.clone())
                    .await?
            };
            self.register_session(id, &cell, session, handle.clone(), handle.is_some())
                .await?;
            let needs_history = force_history || !cell.data.lock().await.history_primed;
            if needs_history {
                self.hydrate_history(id, &cell, force_history).await?;
            }
            Ok(cell.data.lock().await.snapshot.clone())
        }
        .await;
        match &result {
            Ok(_) => {
                // A live owner supersedes a recorded background failure.
                if self.preparations.clear_failure(id) {
                    self.publish_preparation(id).await;
                }
            }
            Err(_) => self.discard_failed_registration(id, &cell).await,
        }
        result
    }
    pub(super) async fn import_session(&self, input: Value) -> Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let app = self.app()?;
        let config = self
            .normalize_config(
                json!({"provider":input["provider"],"cwd":input["cwd"]}),
                true,
            )
            .await?;
        let factory = app.providers.get(required(&config, "provider")?)?;
        let timestamp = now();
        let snapshot = json!({"id":id,"provider":config["provider"],"cwd":config["cwd"],"config":config,"createdAt":timestamp,"updatedAt":timestamp,"lastUserMessageAt":null,"workspaceId":input["workspaceId"],"labels":input.get("labels").cloned().unwrap_or(json!({})),"internal":false,"title":null,"archivedAt":null,"requiresAttention":false,"attentionReason":null,"attentionTimestamp":null});
        let (cell, receiver) = self.make_cell(&id, snapshot, true).await?;
        *cell.task.lock().await = Some(self.start_events(&id, &cell, receiver));
        let result = async {
            let mut env = self.provider_environment(&config)?;
            if let Some(workspace) = text(&input, "workspaceId") {
                env.extend(app.projects.teardown_environment(workspace).await?);
            }
            env.insert("PASEO_AGENT_ID".into(), id.clone());
            env.insert("PASEO_AGENT_CWD".into(), required(&config, "cwd")?.into());
            let context = self
                .open_context(
                    &id,
                    &config,
                    input.get("workspaceId"),
                    "import",
                    "interactive",
                    env,
                )
                .await?;
            let imported = factory
                .import_session(
                    input.clone(),
                    self.launch_config(config.clone())?,
                    config,
                    context,
                    cell.sink.clone(),
                )
                .await?;
            {
                let mut config = imported.config;
                strip_config_internal_servers(&mut config);
                object(&mut config).remove("daemonAppendSystemPrompt");
                let mut data = cell.data.lock().await;
                // resolveImportedAgentTitle: the first non-empty imported user message
                // titles the chat, preferring an explicit config title.
                let first_prompt = imported.timeline.iter().find_map(|entry| {
                    notifications::project_agent_messages(entry.get("item").unwrap_or(entry))
                        .into_iter()
                        .find_map(|item| {
                            (item["type"] == "user_message")
                                .then(|| item["text"].as_str().map(str::trim).map(str::to_owned))
                                .flatten()
                                .filter(|text| !text.is_empty())
                        })
                });
                data.snapshot["title"] = json!(first_prompt.and_then(|prompt| {
                    super::creation::title(text(&config, "title"), &prompt)
                }));
                data.snapshot["config"] = config;
            }
            for entry in imported.timeline {
                for item in
                    notifications::project_agent_messages(entry.get("item").unwrap_or(&entry))
                {
                    self.append_to_cell(&id, &cell, item, &entry).await?;
                }
            }
            let snapshot = self
                .register_session(
                    &id,
                    &cell,
                    imported.session,
                    Some(imported.persistence),
                    false,
                )
                .await?;
            for event in imported.provider_subagent_events {
                self.apply_event(&id, &cell, event, true).await?;
            }
            Ok(snapshot)
        }
        .await;
        if result.is_err() {
            self.discard_failed_registration(&id, &cell).await;
        }
        result
    }
    pub(super) async fn ensure_loaded(&self, id: &str) -> Result<Arc<AgentCell>> {
        if let Ok(cell) = self.cell(id).await
            && cell.data.lock().await.serves_runtime() {
                return Ok(cell);
            }
        self.resume(id, json!({}), false).await?;
        self.cell(id).await
    }
    pub(super) async fn hydrate_history(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        force: bool,
    ) -> Result<()> {
        self.hydrate_history_for(id, cell, force, None, false).await
    }
    pub(super) async fn hydrate_history_for(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        force: bool,
        initiating_source: Option<&str>,
        allow_removal: bool,
    ) -> Result<()> {
        let session = {
            let data = cell.data.lock().await;
            if data.history_primed && !force {
                return Ok(());
            }
            data.session
                .clone()
                .ok_or_else(|| anyhow!("Agent has no session"))?
        };
        let (sink, mut receiver) = EventSink::channel(256, cell.cancel.child_token());
        let collect = async {
            let mut events = Vec::new();
            while let Some(event) = receiver.recv().await {
                events.push(event);
            }
            events
        };
        let (result, events) = tokio::join!(session.stream_history(sink), collect);
        let coverage = result?;
        self.drain_events(cell).await?;
        let _commit = cell.commit.lock().await;
        let timeline = cell.timeline.clone();
        let path = self.timeline_path(id)?;
        let id_owned = id.to_owned();
        let store = self.subagent_store.clone();
        let snapshot = cell.data.lock().await.snapshot.clone();
        let root = (snapshot["internal"] != true).then(|| self.app().map(|app| app.home.join("agent-subagents"))).transpose()?;
        let (epoch, subagent_outputs) = tokio::task::spawn_blocking(move || {
            let mut current = timeline
                .lock()
                .map_err(|_| anyhow!("Timeline lock poisoned"))?;
            let mut replacement = Timeline::new();
            let mut subagent_events = Vec::new();
            for event in events {
                if event["type"] == "timeline" {
                    for item in notifications::project_agent_messages(&event["item"]) {
                        replacement.append(item, &event);
                    }
                } else if event["type"] == "provider_subagent" {
                    subagent_events.push(event);
                }
            }
            let epoch = if replacement.reconcile_history(&current, coverage, allow_removal) {
                replacement.persist(&path, &id_owned)?;
                let epoch = replacement.epoch().to_owned();
                *current = replacement;
                Some(epoch)
            } else {
                tracing::warn!(agent_id=%id_owned, "Retaining canonical timeline: replay lacks complete identity coverage");
                None
            };
            drop(current);
            let outputs = if subagent_events.is_empty() { Vec::new() } else {
                store.lock().map_err(|_| anyhow!("Subagent lock poisoned"))?
                    .replay_history(root.as_deref(), &id_owned, subagent_events, false)?
            };
            Ok::<_, anyhow::Error>((epoch, outputs))
        })
        .await??;
        for output in subagent_outputs {
            self.publish_subagent_output(id, cell, &snapshot, output).await?;
        }
        let mut snapshot = cell.data.lock().await.snapshot.clone();
        snapshot["historySync"] = json!({
            "status": if epoch.is_none() { "retained" }
                else if coverage == crate::providers::api::HistoryCoverage::Complete { "complete" } else { "partial" },
            "canonicalPreserved": epoch.is_none(),
        });
        self.commit_snapshot_locked(cell, snapshot, false).await?;
        drop(_commit);
        self.release_event_stage(cell).await?;
        cell.data.lock().await.history_primed = true;
        if let Some(epoch) = epoch {
            let mut event = json!({"type":"timeline_replacement","agentId":id,"epoch":epoch});
            if let Some(source) = initiating_source {
                event["initiatingSourceId"] = json!(source);
            }
            let _ = self.events.send(event);
        } else if allow_removal {
            bail!("Provider rewind completed, but its history snapshot could not be safely reconciled; previous canonical history was preserved");
        }
        Ok(())
    }
    pub(super) fn timeline_path(&self, id: &str) -> Result<PathBuf> {
        Ok(self
            .app()?
            .home
            .join("agent-timelines")
            .join(format!("{id}.sqlite3")))
    }
    pub(super) async fn append_to_cell(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        item: Value,
        options: &Value,
    ) -> Result<Value> {
        let persistent = cell.data.lock().await.snapshot["internal"] != true;
        let timeline = cell.timeline.clone();
        let path = self.timeline_path(id)?;
        let id = id.to_owned();
        let options = options.clone();
        tokio::task::spawn_blocking(move || {
            let mut timeline = timeline
                .lock()
                .map_err(|_| anyhow!("Timeline lock poisoned"))?;
            if persistent {
                timeline.append_persisted(&path, &id, item, &options)
            } else {
                Ok(timeline.append(item, &options))
            }
        })
        .await?
    }
    pub(super) async fn apply_event(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        mut event: Value,
        from_history: bool,
    ) -> Result<()> {
        if event["type"] == "timeline" {
            event["item"] = timeline::limit_item_content(event["item"].take());
        }
        if !from_history {
            let mut stage = cell.stage.lock().await;
            if let Some(stage) = stage.as_mut() {
                stage.push(event).await?;
                return Ok(());
            }
        }
        let _commit = cell.commit.lock().await;
        if event["type"] == "provider_subagent" && event["event"]["type"] == "replay" {
            // Applied in the parent's event order; the outcome goes to the
            // provider and the reader. A failed replay never ends the parent's owner.
            self.apply_published_replay(id, &event).await;
            return Ok(());
        }
        if event["type"] == "provider_subagent" {
            self.apply_subagent_event(id, cell, &event, !from_history).await?;
            return Ok(());
        }
        let (mut snapshot, ready, owner_ended) = {
            let data = cell.data.lock().await;
            (data.snapshot.clone(), data.ready, data.owner_ended)
        };
        let previous_lifecycle = snapshot["lifecycle"].as_str().map(str::to_owned);
        let kind = required(&event, "type")?.to_owned();
        let session_ended = !from_history && kind == "turn_failed" && event["sessionEnded"] == true;
        if owner_ended && (matches!(kind.as_str(), "turn_started" | "turn_completed" | "turn_canceled")
            || (kind == "turn_failed" && snapshot["lifecycle"] == "closed")) {
            return Ok(());
        }
        if session_ended {
            // Process exit terminates the session, not just the provider turn that
            // happened to be current when the process watcher observed it.
            event["turnId"] = snapshot["activeTurnId"].clone();
        }
        let mut turn = text(&event, "turnId").map(str::to_owned);
        if turn.is_none()
            && !from_history
            && matches!(
                kind.as_str(),
                "turn_started"
                    | "turn_completed"
                    | "turn_failed"
                    | "turn_canceled"
                    | "timeline"
                    | "permission_requested"
                    | "permission_resolved"
            )
        {
            turn = text(&snapshot, "activeTurnId")
                .map(str::to_owned)
                .or_else(|| {
                    (kind == "turn_started").then(|| format!("autonomous-{}", uuid::Uuid::new_v4()))
                });
            if let Some(turn) = &turn {
                event["turnId"] = json!(turn);
            }
        }
        let stale = turn.as_ref().is_some_and(|turn| {
            text(&snapshot, "activeTurnId").is_some_and(|active| active != turn)
        }) || if let Some(turn) = &turn {
            cell.data.lock().await.finalized.contains(turn)
        } else {
            false
        };
        if stale && !session_ended
            && matches!(
                kind.as_str(),
                "turn_started" | "turn_completed" | "turn_failed" | "turn_canceled"
            )
        {
            return Ok(());
        }
        let mut streams = Vec::new();
        let mut resolutions = Vec::new();
        let mut dispatch = true;
        let mut attention = None;
        match kind.as_str() {
            "thread_started" => {
                let session = cell.data.lock().await.session.clone();
                if let Some(handle) = session.and_then(|session| session.describe_persistence()) {
                    snapshot["persistence"] = with_cwd(handle, &snapshot["cwd"]);
                } else if snapshot["persistence"].is_null() {
                    snapshot["persistence"] = with_cwd(
                        json!({"provider":snapshot["provider"],"sessionId":event["sessionId"]}),
                        &snapshot["cwd"],
                    );
                }
            }
            "turn_started" => {
                // agent-manager.ts onStreamTurnStarted: the manager already published the
                // accepted foreground start, so the provider's duplicate is suppressed.
                let foreground = turn.is_some()
                    && text(&snapshot, "activeForegroundTurnId") == turn.as_deref();
                if !from_history && !stale && !foreground {
                    snapshot["lifecycle"] = json!("running");
                    snapshot["activeTurnId"] = json!(turn);
                    snapshot["activeTurn"] = json!({"turnId":turn,"startedAt":now()});
                    snapshot["lastError"] = Value::Null;
                } else {
                    dispatch = false;
                }
            }
            "turn_completed" | "turn_canceled" | "turn_failed" => {
                if stale && !session_ended {
                    dispatch = false;
                } else if !from_history {
                    let replacing = {
                        let mut data = cell.data.lock().await;
                        if session_ended {
                            data.owner_ended = true;
                            data.session = None;
                            data.pending_replacement = false;
                            snapshot["_live"] = json!(false);
                            cell.owner_ended.cancel();
                        }
                        if let Some(turn) = &turn
                            && !data.finalized.contains(turn) {
                                data.finalized.push_back(turn.clone());
                                while data.finalized.len() > 4096 {
                                    data.finalized.pop_front();
                                }
                            }
                        data.pending_replacement
                    };
                    // finalizeForegroundTurn: a replacing prompt holds the agent busy unless the
                    // settled turn failed.
                    snapshot["lifecycle"] = json!(if kind == "turn_failed" {
                        "error"
                    } else if replacing {
                        "running"
                    } else {
                        "idle"
                    });
                    snapshot["activeTurnId"] = Value::Null;
                    snapshot["activeForegroundTurnId"] = Value::Null;
                    snapshot["activeTurn"] = Value::Null;
                    if kind != "turn_completed" {
                        if let Some(pending) = snapshot["pendingPermissions"].as_array() {
                            for request in pending {
                                let request_id = required(request, "id")?;
                                let resolution = json!({"type":"permission_resolved","provider":event["provider"],"requestId":request_id,"resolution":{"behavior":"deny","message":if kind=="turn_failed"{"Turn failed"}else{"Interrupted"}}});
                                cell.data
                                    .lock()
                                    .await
                                    .resolutions
                                    .insert(request_id.into(), resolution.clone());
                                resolutions.push(resolution);
                            }
                        }
                        snapshot["pendingPermissions"] = json!([]);
                    }
                    snapshot["lastTurnOutcome"] = json!(kind);
                    if let Some(usage) = event.get("usage") {
                        merge(&mut snapshot["lastUsage"], usage);
                    }
                    if kind == "turn_failed" {
                        snapshot["lastError"] = event["error"].clone();
                        // appendSystemErrorTimelineMessage: the failure is an assistant row,
                        // skipped when it repeats the latest item.
                        let item = json!({"type":"assistant_message","text":format!("{SYSTEM_ERROR_PREFIX} {}", turn_failed_message(&event))});
                        let repeated = cell
                            .timeline
                            .lock()
                            .map_err(|_| anyhow!("Timeline lock poisoned"))?
                            .last_item()
                            .is_some_and(|last| {
                                last["type"] == "assistant_message" && last["text"] == item["text"]
                            });
                        if !repeated {
                            let row = self
                                .append_to_cell(id, cell, item.clone(), &json!({}))
                                .await?;
                            streams.push((
                                json!({"type":"timeline","provider":event["provider"],"item":item}),
                                row,
                            ));
                        }
                    } else {
                        object(&mut snapshot).remove("lastError");
                    }
                }
            }
            "usage_updated" => snapshot["lastUsage"] = event["usage"].clone(),
            "mode_changed" => {
                snapshot["currentModeId"] = event["currentModeId"].clone();
                snapshot["availableModes"] = event["availableModes"].clone();
                if !event["currentModeId"].is_null() {
                    snapshot["config"]["modeId"] = event["currentModeId"].clone();
                }
                if snapshot["runtimeInfo"].is_object() {
                    snapshot["runtimeInfo"]["modeId"] = event["currentModeId"].clone();
                }
                dispatch = false;
            }
            "model_changed" => {
                snapshot["runtimeInfo"] = event["runtimeInfo"].clone();
                if let Some(mode) = event["runtimeInfo"].get("modeId") {
                    snapshot["currentModeId"] = mode.clone();
                    if !mode.is_null() {
                        snapshot["config"]["modeId"] = mode.clone();
                    }
                }
                dispatch = false;
            }
            "thinking_option_changed" => {
                if event["thinkingOptionId"].is_null() {
                    object(&mut snapshot["config"]).remove("thinkingOptionId");
                } else {
                    snapshot["config"]["thinkingOptionId"] = event["thinkingOptionId"].clone();
                }
                if snapshot["runtimeInfo"].is_object() {
                    snapshot["runtimeInfo"]["thinkingOptionId"] = event["thinkingOptionId"].clone();
                }
                dispatch = false;
            }
            "permission_requested" => {
                let request = &event["request"];
                let request_id = required(request, "id")?;
                let mut data = cell.data.lock().await;
                if data.resolutions.remove(request_id).is_none() {
                    let permissions = snapshot["pendingPermissions"]
                        .as_array_mut()
                        .ok_or_else(|| anyhow!("Invalid permissions state"))?;
                    // onStreamPermissionRequested: only the first outstanding request raises
                    // attention, and it is a broadcast, not the agent's unread state.
                    if permissions.is_empty() {
                        attention = Some("permission");
                    }
                    if let Some(previous) = permissions
                        .iter_mut()
                        .find(|value| text(value, "id") == Some(request_id))
                    {
                        *previous = request.clone();
                    } else {
                        permissions.push(request.clone());
                    }
                } else {
                    dispatch = false;
                }
            }
            "permission_resolved" => {
                let request_id = required(&event, "requestId")?;
                if let Some(permissions) = snapshot["pendingPermissions"].as_array_mut() {
                    let before = permissions.len();
                    permissions.retain(|value| text(value, "id") != Some(request_id));
                    if before == permissions.len() {
                        cell.data
                            .lock()
                            .await
                            .resolutions
                            .insert(request_id.into(), event.clone());
                        dispatch = false;
                    }
                }
                if snapshot["pendingPermissions"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
                    && snapshot["attentionReason"] == "permission"
                {
                    snapshot["requiresAttention"] = json!(false);
                    snapshot["attentionReason"] = Value::Null;
                    snapshot["attentionTimestamp"] = Value::Null;
                    snapshot["attention"] = json!({"requiresAttention":false});
                }
            }
            "attention_required" => {
                if event.get("timestamp").is_none_or(|value| !value.is_string()) {
                    event["timestamp"] = json!(now());
                }
            }
            "timeline" => {
                dispatch = false;
                for mut item in notifications::project_agent_messages(&event["item"]) {
                    let mut options = event.clone();
                    if item["type"] == "user_message"
                        && let Some(message_id) = text(&item, "clientMessageId").map(str::to_owned)
                        {
                            let exists = cell
                                .timeline
                                .lock()
                                .map_err(|_| anyhow!("Timeline lock poisoned"))?
                                .submitted_user_message(&message_id)
                                .is_some();
                            if exists {
                                if let Some(provider_id) =
                                    text(&item, "messageId").map(str::to_owned)
                                {
                                    let timeline = cell.timeline.clone();
                                    let path = self.timeline_path(id)?;
                                    let agent_id = id.to_owned();
                                    let internal = snapshot["internal"] == true;
                                    tokio::task::spawn_blocking(move || {
                                        let mut timeline = timeline
                                            .lock()
                                            .map_err(|_| anyhow!("Timeline lock poisoned"))?;
                                        if internal {
                                            timeline.enrich_submitted_user_message(
                                                &message_id,
                                                &provider_id,
                                            );
                                            Ok::<_, anyhow::Error>(())
                                        } else {
                                            timeline
                                                .enrich_submitted_persisted(
                                                    &path,
                                                    &agent_id,
                                                    &message_id,
                                                    &provider_id,
                                                )
                                                .map(|_| ())
                                        }
                                    })
                                    .await??;
                                }
                                continue;
                            }
                            if let Some(provider_id) = item.get("messageId") {
                                options["providerMessageId"] = provider_id.clone();
                            }
                            item["messageId"] = json!(message_id);
                        }
                    let row = self
                        .append_to_cell(id, cell, item.clone(), &options)
                        .await?;
                    if item["type"] == "user_message" && !from_history {
                        snapshot["lastUserMessageAt"] = json!(now());
                    }
                    let mut stream = event.clone();
                    stream["item"] = item;
                    streams.push((stream, row));
                }
            }
            _ => {}
        }
        if attention.is_none() && !from_history {
            attention = transition_attention(previous_lifecycle.as_deref(), &mut snapshot);
        }
        let attention_timestamp = if attention == Some("permission") {
            now()
        } else {
            text(&snapshot, "attentionTimestamp")
                .map(str::to_owned)
                .unwrap_or_else(now)
        };
        if !from_history {
            snapshot["updatedAt"] = json!(now());
        }
        snapshot["status"] = snapshot["lifecycle"].clone();
        if ready {
            self.persist_snapshot(&mut snapshot, !from_history).await?;
        }
        cell.data.lock().await.snapshot = snapshot.clone();
        if ready && !from_history {
            let _ = self
                .events
                .send(json!({"type":"agent_state","agent":snapshot}));
            for (stream, row) in streams {
                self.publish_stream(id, cell, stream, Some(row)).await?;
            }
            for resolution in resolutions {
                self.publish_stream(id, cell, resolution, None).await?;
            }
            if let Some(reason) = attention.filter(|_| snapshot["internal"] != true) {
                self.publish_stream(id,cell,json!({"type":"attention_required","provider":snapshot["provider"],"reason":reason,"timestamp":attention_timestamp}),None).await?;
            }
            if dispatch {
                self.publish_stream(id, cell, event, None).await?;
            }
        }
        Ok(())
    }
    /// `live`: reported by the running provider now, not imported history;
    /// only a live `running` upsert may bring a deleted child back.
    async fn apply_subagent_event(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        event: &Value,
        live: bool,
    ) -> Result<()> {
        let snapshot = cell.data.lock().await.snapshot.clone();
        let store = self.subagent_store.clone();
        let root = (snapshot["internal"] != true).then(|| self.app().map(|app| app.home.join("agent-subagents"))).transpose()?;
        let parent = id.to_owned();
        let event = event.clone();
        let output = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| anyhow!("Subagent lock poisoned"))?;
            let provider = required(&event, "provider")?;
            match root {
                Some(root) => store.apply_persisted_event(&root, &parent, provider, &event["event"], live),
                None => store.apply(&parent, provider, &event["event"]),
            }
        }).await??;
        self.publish_subagent_output(id, cell, &snapshot, output).await
    }
    async fn publish_subagent_output(
        &self, id: &str, cell: &Arc<AgentCell>, snapshot: &Value, output: Value,
    ) -> Result<()> {
        if cell.data.lock().await.ready {
            self.emit_subagent_output(id, snapshot["internal"] == true, output).await?;
        }
        Ok(())
    }
    /// Publishes one stored child change. Lazy child-body reads use this
    /// without a parent runtime; descriptors leave without their `source` and
    /// with the child's current native-refresh state.
    pub(super) async fn emit_subagent_output(&self, id: &str, internal: bool, mut output: Value) -> Result<()> {
        // A dropped event (it targeted a deleted child) publishes nothing.
        if output.is_null() {
            return Ok(());
        }
        if let Some(descriptor) = output.get_mut("subagent") {
            *descriptor = timeline::public_subagent(descriptor.take());
            self.overlay_subagent_refresh(descriptor);
        }
        let _ = self
            .events
            .send(json!({"type":"provider_subagent","event":output}));
        if !internal {
            let payload = match text(&output, "type") {
                Some("upsert") => json!({"kind":"upsert","subagent":output["subagent"]}),
                Some("timeline") => {
                    json!({"kind":"timeline","parentAgentId":id,"subagentId":output["subagentId"],"provider":output["provider"],"item":output["row"]["item"],"timestamp":output["row"]["timestamp"],"seq":output["row"]["seq"],"epoch":output["epoch"]})
                }
                _ => {
                    json!({"kind":"remove","parentAgentId":id,"subagentId":output["subagentId"]})
                }
            };
            self.app()?
                .transport
                .publish_event(
                    json!({"type":"agent.provider_subagents.update","payload":payload}),
                )
                .await?;
        }
        Ok(())
    }
    pub(super) async fn publish_stream(
        &self,
        id: &str,
        cell: &Arc<AgentCell>,
        mut event: Value,
        row: Option<Value>,
    ) -> Result<()> {
        if event["type"] == "timeline" {
            event["item"] = timeline::limit_item_content(event["item"].take());
        }
        let mut output = json!({"type":"agent_stream","agentId":id,"event":event});
        if let Some(row) = row {
            output["seq"] = row["seq"].clone();
            output["timestamp"] = row["timestamp"].clone();
            output["epoch"] = json!(
                cell.timeline
                    .lock()
                    .map_err(|_| anyhow!("Timeline lock poisoned"))?
                    .epoch()
            );
        }
        let _ = self.events.send(output.clone());
        let snapshot = cell.data.lock().await.snapshot.clone();
        if snapshot["internal"] == true {
            return Ok(());
        }
        let app = self.app()?;
        if let Some((name, payload)) = hook_stream(&snapshot, &event) {
            let _ = app.plugins.emit(name, payload).await;
        }
        if let Some(serialized) =
            serialize_stream_event(&event).filter(|_| event["type"] != "attention_required")
        {
            let mut payload = json!({"agentId":id,"event":serialized,"timestamp":output.get("timestamp").cloned().unwrap_or(json!(now()))});
            for key in ["seq", "epoch"] {
                if let Some(value) = output.get(key) {
                    payload[key] = value.clone();
                }
            }
            app.transport
                .publish_event(json!({"type":"agent_stream","payload":payload}))
                .await?;
        }
        match text(&event,"type"){
            Some("permission_requested")=>app.transport.publish_event(json!({"type":"agent_permission_request","payload":{"agentId":id,"request":event["request"]}})).await?,
            Some("permission_resolved")=>app.transport.publish_event(json!({"type":"agent_permission_resolved","payload":{"agentId":id,"requestId":event["requestId"],"resolution":event["resolution"]}})).await?,
            Some("attention_required")=>self.notifications.attention(&snapshot,required(&event,"reason")?,required(&event,"timestamp")?).await?,
            _=>{}
        }
        Ok(())
    }
    pub(super) async fn close_cell(&self, id: &str, cell: &Arc<AgentCell>) -> Result<()> {
        let _mutation = cell.mutation.lock().await;
        if cell.data.lock().await.event_error.is_some() {
            // Failed-owner cleanup may still be running or may retain a process
            // whose close failed. Join it before deciding who must be closed.
            if let Some(task) = cell.task.lock().await.take() {
                let _ = task.await;
            }
        }
        let session = cell.data.lock().await.session.clone();
        let mut event_error = if let Some(session) = session {
            session.close().await?;
            if cell.data.lock().await.event_error.is_some() {
                None
            } else {
                self.drain_events(cell).await.err()
            }
        } else if cell.data.lock().await.owner_ended && !cell.cancel.is_cancelled() {
            self.drain_events(cell).await.err()
        } else {
            None
        };
        if cell.data.lock().await.event_error.is_some() {
            // Join the failed owner before committing closed, so its terminal
            // update cannot race this close or a subsequent session reload.
            event_error = None;
            if let Some(task) = cell.task.lock().await.take() {
                let _ = task.await;
            }
        }
        let store = self.subagent_store.clone();
        let parent = id.to_owned();
        let internal = cell.data.lock().await.snapshot["internal"] == true;
        let root = (!internal).then(|| self.app().map(|app| app.home.join("agent-subagents"))).transpose()?;
        let listed = tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(|_| anyhow!("Subagent lock poisoned"))?;
            match root {
                Some(root) => store.list_persisted(&root, &parent),
                None => Ok((store.list(&parent), Vec::new())),
            }
        }).await?;
        let (children, unreadable) = match listed {
            Err(error) if error.downcast_ref::<timeline::SubagentStorageUnavailable>().is_some() => {
                tracing::warn!(agent_id = id, %error, "Running subagents not marked canceled: storage unavailable");
                (Vec::new(), Vec::new())
            }
            listed => listed?,
        };
        for error in unreadable {
            tracing::warn!(agent_id = id, %error, "Subagent not marked canceled: descriptor unreadable");
        }
        {
            let _commit = cell.commit.lock().await;
            for child in children
                .into_iter()
                .filter(|child| child["status"] == "running")
            {
                self.apply_subagent_event(id,cell,&json!({"provider":child["provider"],"event":{"type":"upsert","id":child["id"],"status":"canceled"}}),true).await?;
            }
            if !internal {
                let store = self.subagent_store.clone();
                let parent = id.to_owned();
                tokio::task::spawn_blocking(move || {
                    store.lock().map_err(|_| anyhow!("Subagent lock poisoned"))?.release_parent(&parent);
                    Ok::<_, anyhow::Error>(())
                }).await??;
            }
        }
        let mut snapshot = cell.data.lock().await.snapshot.clone();
        let interrupted = is_busy(&snapshot) || snapshot["activeForegroundTurnId"].is_string();
        if interrupted {
            snapshot["lastTurnOutcome"] = json!("turn_canceled");
        }
        snapshot["lifecycle"] = json!("closed");
        snapshot["status"] = json!("closed");
        snapshot["activeTurnId"] = Value::Null;
        snapshot["activeForegroundTurnId"] = Value::Null;
        snapshot["activeTurn"] = Value::Null;
        snapshot["pendingPermissions"] = json!([]);
        snapshot["_live"] = json!(false);
        // Releasing an idle/history-only view is not user or provider activity.
        self.commit_snapshot(cell, snapshot.clone(), interrupted).await?;
        if snapshot["internal"] != true {
            let _ = self
                .app()?
                .plugins
                .emit("agent.closed", json!({"agent":hook_agent(&snapshot)}))
                .await;
        }
        cell.cancel.cancel();
        if let Some(task) = cell.task.lock().await.take() {
            let _ = task.await;
        }
        self.agents.write().await.remove(id);
        self.app()?.mcp.revoke_agent(id).await;
        if let Some(error) = event_error {
            Err(error)
        } else {
            Ok(())
        }
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.notifications.shutdown().await?;
        self.message_delivery.shutdown().await?;
        self.closing.store(true, Ordering::Release);
        // Owned background reads end with the manager; their waiters see cancellation.
        self.preparations.abort_all();
        self.subagent_reads.abort_all();
        let agents = self
            .agents
            .read()
            .await
            .iter()
            .map(|(id, cell)| (id.clone(), cell.clone()))
            .collect::<Vec<_>>();
        let mut failure = None;
        for (id, cell) in agents {
            if let Err(error) = self.close_cell(&id, &cell).await {
                failure.get_or_insert(error);
            }
        }
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(())
        }
    }
}
/// `resume_session` options. `subagentCardLogs` carries each stored
/// sub-agent card's recorded log by call ID, and `subagentCardEntries` the
/// recorded identities of that log's entries, so a provider keeps the log
/// across replay and continues live child output exactly once without
/// reading child bodies.
pub(super) fn resume_options(history: bool, timeline: &Timeline) -> Value {
    let mut options = json!({"purpose":if history { "history" } else { "interactive" }});
    let (logs, entries) = timeline.subagent_card_logs();
    if !logs.is_empty() {
        options["subagentCardLogs"] = Value::Object(logs);
    }
    if !entries.is_empty() {
        options["subagentCardEntries"] = Value::Object(entries);
    }
    options
}
pub(super) fn hook_agent(snapshot: &Value) -> Value {
    json!({"id":snapshot["id"],"workspaceId":snapshot["workspaceId"],"parentAgentId":snapshot["labels"]["paseo.parent-agent-id"],"provider":snapshot["provider"],"cwd":snapshot["cwd"],"title":snapshot["title"]})
}
fn hook_stream(snapshot: &Value, event: &Value) -> Option<(&'static str, Value)> {
    let agent = hook_agent(snapshot);
    Some(match text(event, "type")? {
        "turn_started" => (
            "agent.turn_started",
            json!({"agent":agent,"turnId":event["turnId"]}),
        ),
        "turn_completed" => (
            "agent.turn_ended",
            json!({"agent":agent,"turnId":event["turnId"],"timeline":[],"outcome":{"kind":"completed"}}),
        ),
        "turn_failed" => {
            let mut outcome = json!({"kind":"failed","error":{"message":event["error"]}});
            if let Some(code) = event.get("code") {
                outcome["error"]["code"] = code.clone();
            }
            (
                "agent.turn_ended",
                json!({"agent":agent,"turnId":event["turnId"],"timeline":[],"outcome":outcome}),
            )
        }
        "turn_canceled" => {
            let mut outcome = json!({"kind":"canceled"});
            if let Some(reason) = event.get("reason") {
                outcome["reason"] = reason.clone();
            }
            (
                "agent.turn_ended",
                json!({"agent":agent,"turnId":event["turnId"],"timeline":[],"outcome":outcome}),
            )
        }
        "permission_requested" => (
            "agent.permission_requested",
            json!({"agent":agent,"request":event["request"]}),
        ),
        "permission_resolved" => (
            "agent.permission_resolved",
            json!({"agent":agent,"requestId":event["requestId"],"resolution":event["resolution"]}),
        ),
        _ => return None,
    })
}


/// agent-manager.ts SYSTEM_ERROR_PREFIX.
const SYSTEM_ERROR_PREFIX: &str = "[System Error]";

/// agent-manager.ts formatTurnFailedMessage.
pub(super) fn turn_failed_message(event: &Value) -> String {
    let base = event["error"].as_str().unwrap_or("").trim();
    let mut parts = vec![if base.is_empty() {
        "Provider run failed"
    } else {
        base
    }];
    let code = event["code"].as_str().map(str::trim).unwrap_or("");
    let code = (!code.is_empty()).then(|| format!("code: {code}"));
    if let Some(code) = code.as_deref() {
        parts.push(code);
    }
    let diagnostic = event["diagnostic"].as_str().map(str::trim).unwrap_or("");
    if !diagnostic.is_empty() && diagnostic != base {
        parts.push(diagnostic);
    }
    parts.join("\n\n")
}

/// agent-manager.ts checkAndSetAttention: attention is an edge-triggered unread signal,
/// raised once on running -> idle (finished) or on entering error, and never replacing
/// an attention the user has not cleared yet.
pub(super) fn transition_attention(
    previous: Option<&str>,
    snapshot: &mut Value,
) -> Option<&'static str> {
    if snapshot["internal"] == true || snapshot["requiresAttention"] == true {
        return None;
    }
    let current = text(snapshot, "lifecycle");
    let reason = if previous == Some("running") && current == Some("idle") {
        "finished"
    } else if previous != Some("error") && current == Some("error") {
        "error"
    } else {
        return None;
    };
    let timestamp = now();
    snapshot["requiresAttention"] = json!(true);
    snapshot["attentionReason"] = json!(reason);
    snapshot["attentionTimestamp"] = json!(timestamp);
    snapshot["attention"] =
        json!({"requiresAttention":true,"attentionReason":reason,"attentionTimestamp":timestamp});
    Some(reason)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::providers::api::{AgentProvider, LaunchContext, ProviderSpec};
    use crate::providers::pi_omp::JsonlProvider;
    use crate::transport::{OutboundFrame, RequestContext};

    #[tokio::test]
    async fn provider_environment_selects_private_home_before_session_overrides() {
        let root = tempfile::tempdir().unwrap();
        let private_home = root.path().join("provider-home");
        std::fs::create_dir(&private_home).unwrap();
        std::fs::write(private_home.join("selection"), "provider configuration").unwrap();
        let app = AppState::new(
            root.path().join("daemon-home"),
            root.path().into(),
            std::env::current_exe().unwrap(),
        );
        // Register the real provider configuration without launching discovery
        // or a model. The filesystem consumer distinguishes the selected HOME
        // from the daemon's inherited HOME without reading any user files.
        app.providers.catalogs.shutdown().await;
        app.providers.apply_config(
            &json!({"providerOverrides":{"pi":{"env":{
                "HOME":private_home.to_str().unwrap(),
                "PASEO_AGENT_ID":"configured-id"
            }}}}),
            &json!({}),
        ).await.unwrap();
        for internal in [false, true] {
            let id = "00000000-0000-4000-8000-000000000045";
            let context = app.agents.history_context(
                id,
                &json!({"provider":"pi","internal":internal,"cwd":root.path()}),
                None,
            ).await.unwrap();
            let env = context.env;
            assert_eq!(Path::new(&env["HOME"]), private_home.as_path());
            assert_eq!(
                std::fs::read_to_string(Path::new(&env["HOME"]).join("selection")).unwrap(),
                "provider configuration"
            );
            assert_eq!(env["PASEO_AGENT_ID"], id);
            assert_eq!(Path::new(&env["PASEO_AGENT_CWD"]), root.path());
        }
    }

    pub(in crate::agent) async fn staged_cell(app: &Arc<AppState>, id: &str) -> Arc<AgentCell> {
        staged_cell_with_config(app, id, json!({})).await
    }

    pub(in crate::agent) async fn staged_cell_with_config(
        app: &Arc<AppState>,
        id: &str,
        config: Value,
    ) -> Arc<AgentCell> {
        let manager = &app.agents;
        let snapshot = json!({
            "id":id,"provider":"mock","internal":true,
            "config":{},"updatedAt":now(),"requiresAttention":false
        });
        let (cell, receiver) = manager.make_cell(id, snapshot, true).await.unwrap();
        let session = crate::providers::development::DevelopmentProvider::new("mock").unwrap()
            .create_session(
                config,
                LaunchContext {
                    agent_id: id.into(),
                    home: app.home.clone(),
                    executable: app.executable.clone(),
                    env: BTreeMap::new(),
                    mcp_servers: json!({}),
                    tool_catalog: json!([]),
                    tool_manifest: json!([]),
                    tool_executor: None,
                    cancel: cell.cancel.child_token(),
                },
                json!({}),
                cell.sink.clone(),
            ).await.unwrap();
        {
            let mut data = cell.data.lock().await;
            data.session = Some(session);
            data.ready = true;
            data.snapshot["lifecycle"] = json!("running");
            data.snapshot["status"] = json!("running");
            data.snapshot["activeTurnId"] = json!("live-turn");
            data.snapshot["activeForegroundTurnId"] = json!("live-turn");
            data.snapshot["activeTurn"] = json!({"turnId":"live-turn"});
        }
        *cell.task.lock().await = Some(manager.start_events(id, &cell, receiver));
        manager.begin_event_stage(&cell).await.unwrap();
        cell
    }

    #[tokio::test]
    async fn pending_stage_spills_without_overtaking_the_accepted_prompt() {
        let root = tempfile::tempdir().unwrap();
        let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        let id = "00000000-0000-4000-8000-000000000002";
        let cell = staged_cell(&app, id).await;
        // A delayed acknowledgement must neither drop output nor expose it before the prompt.
        for tokens in 0..4100 {
            cell.sink.emit(json!({
                "type":"usage_updated","provider":"mock","turnId":"live-turn",
                "usage":{"outputTokens":tokens}
            })).await.unwrap();
        }
        cell.sink.emit(json!({
            "type":"timeline","provider":"mock","turnId":"next-turn",
            "item":{"type":"assistant_message","text":"pending response","messageId":"pending"}
        })).await.unwrap();
        cell.sink.emit(json!({
            "type":"timeline","provider":"mock","turnId":"live-turn",
            "item":{"type":"assistant_message","text":"completed response","messageId":"live"}
        })).await.unwrap();
        cell.sink.emit(json!({"type":"turn_completed","provider":"mock","turnId":"live-turn"})).await.unwrap();
        manager.drain_events(&cell).await.unwrap();
        assert_eq!(cell.data.lock().await.snapshot["lifecycle"], "running");
        assert!(cell.timeline.lock().unwrap().rows().is_empty());
        manager.record_submitted(id, &cell, &json!("steered prompt"), Some("client"), Some("live-turn"), false).await.unwrap();
        manager.release_event_stage(&cell).await.unwrap();
        let snapshot = cell.data.lock().await.snapshot.clone();
        assert_eq!(snapshot["lastUsage"]["outputTokens"], 4099);
        assert_eq!(snapshot["lifecycle"], "idle");
        assert_eq!(snapshot["activeTurnId"], Value::Null);
        {
            let timeline = cell.timeline.lock().unwrap();
            let rows = timeline.rows();
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[0]["item"]["text"], "steered prompt");
            assert_eq!(rows[1]["item"]["messageId"], "pending");
            assert_eq!(rows[2]["item"]["messageId"], "live");
        }
        cell.cancel.cancel();
        cell.task.lock().await.take().unwrap().await.unwrap();
    }

    #[tokio::test]
    async fn pending_stage_failure_ends_running_state_and_allows_close() {
        let root = tempfile::tempdir().unwrap();
        let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        let id = "00000000-0000-4000-8000-000000000003";
        let cell = staged_cell(&app, id).await;
        cell.sink.emit(json!({})).await.unwrap();
        assert!(manager.release_event_stage(&cell).await.is_err());
        cell.task.lock().await.take().unwrap().await.unwrap();
        let snapshot = cell.data.lock().await.snapshot.clone();
        assert_eq!(snapshot["lifecycle"], "error");
        assert_eq!(snapshot["lastTurnOutcome"], "turn_failed");
        assert_eq!(snapshot["activeTurnId"], Value::Null);
        assert_eq!(snapshot["_live"], false);
        assert!(cell.data.lock().await.session.is_none());
        assert!(cell.owner_ended.is_cancelled());
        assert!(cell.barriers.lock().await.is_empty());
        assert!(manager.update_snapshot(&cell, |snapshot| {
            snapshot["lifecycle"] = json!("running");
            snapshot["activeTurnId"] = json!("late-acceptance");
            Ok(())
        }).await.is_err());
        assert_eq!(cell.data.lock().await.snapshot["lifecycle"], "error");
        assert_eq!(cell.data.lock().await.snapshot["activeTurnId"], Value::Null);
        manager.close_cell(id, &cell).await.unwrap();
        assert!(manager.cell(id).await.is_err());
    }

    #[tokio::test]
    async fn provider_exit_clears_ownership_preserves_fifo_and_rejects_late_ack() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home.clone()) }).await;
        let id = "00000000-0000-4000-8000-000000000004";
        let cell = staged_cell(&app, id).await;
        {
            let mut data = cell.data.lock().await;
            data.snapshot["internal"] = json!(false);
            data.snapshot["cwd"] = json!(root.path());
            data.snapshot["createdAt"] = json!(now());
            data.snapshot["pendingPermissions"] = json!([{"id":"permission"}]);
            data.pending_replacement = true;
        }
        cell.sink.emit(json!({
            "type":"timeline","provider":"mock","turnId":"live-turn",
            "item":{"type":"assistant_message","text":"last output","messageId":"last"}
        })).await.unwrap();
        cell.sink.emit(json!({
            "type":"turn_failed","provider":"mock","turnId":"live-turn",
            "sessionEnded":true,"error":"Provider process exited"
        })).await.unwrap();
        manager.drain_events(&cell).await.unwrap();
        assert!(manager.session(id).await.unwrap().is_some());
        manager.record_submitted(id, &cell, &json!("submitted prompt"), Some("client"), Some("live-turn"), false).await.unwrap();
        manager.release_event_stage(&cell).await.unwrap();
        let snapshot = manager.agent_snapshot(id).await.unwrap().unwrap();
        assert_eq!(snapshot["lifecycle"], "error");
        assert_eq!(snapshot["lastTurnOutcome"], "turn_failed");
        assert_eq!(snapshot["_live"], false);
        assert!(snapshot["activeTurn"].is_null());
        assert!(snapshot["activeTurnId"].is_null());
        assert!(snapshot["activeForegroundTurnId"].is_null());
        assert_eq!(snapshot["pendingPermissions"], json!([]));
        assert!(manager.session(id).await.unwrap().is_none());
        assert!(!manager.has_in_flight_run(id).await.unwrap());
        assert!(cell.owner_ended.is_cancelled());
        assert!(!cell.data.lock().await.pending_replacement);
        assert!(cell.barriers.lock().await.is_empty());
        {
            let timeline = cell.timeline.lock().unwrap();
            let rows = timeline.rows();
            assert_eq!(rows[0]["item"]["text"], "submitted prompt");
            assert_eq!(rows[1]["item"]["text"], "last output");
            assert_eq!(rows[2]["item"]["text"], "[System Error] Provider process exited");
        }
        assert!(manager.update_snapshot(&cell, |snapshot| {
            snapshot["lifecycle"] = json!("running");
            snapshot["activeTurnId"] = json!("late-ack");
            Ok(())
        }).await.is_err());
        cell.sink.emit(json!({"type":"turn_completed","provider":"mock","turnId":"live-turn"})).await.unwrap();
        cell.sink.emit(json!({"type":"turn_started","provider":"mock","turnId":"late-start"})).await.unwrap();
        manager.drain_events(&cell).await.unwrap();
        let stored = AgentStorage::new(home).get(id).await.unwrap().unwrap();
        assert_eq!(stored["lastStatus"], "error");
        assert_eq!(stored["lastTurnOutcome"], "turn_failed");
        assert_eq!(manager.agent_snapshot(id).await.unwrap().unwrap()["lifecycle"], "error");
        manager.close_cell(id, &cell).await.unwrap();
        assert!(manager.cell(id).await.is_err());
    }

    #[tokio::test]
    async fn unexpected_event_consumer_end_settles_active_owner() {
        let root = tempfile::tempdir().unwrap();
        let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        let id = "00000000-0000-4000-8000-000000000005";
        let cell = staged_cell(&app, id).await;
        manager.release_event_stage(&cell).await.unwrap();
        cell.cancel.cancel();
        cell.task.lock().await.take().unwrap().await.unwrap();
        let snapshot = manager.agent_snapshot(id).await.unwrap().unwrap();
        assert_eq!(snapshot["lifecycle"], "error");
        assert_eq!(snapshot["_live"], false);
        assert!(snapshot["activeTurnId"].is_null());
        assert!(manager.session(id).await.unwrap().is_none());
        assert!(cell.owner_ended.is_cancelled());
        manager.close_cell(id, &cell).await.unwrap();
    }

    #[tokio::test]
    async fn close_without_terminal_event_records_cancellation() {
        let root = tempfile::tempdir().unwrap();
        let app = AppState::new(root.path().join("home"), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        let id = "00000000-0000-4000-8000-000000000006";
        let cell = staged_cell(&app, id).await;
        manager.release_event_stage(&cell).await.unwrap();
        manager.close_cell(id, &cell).await.unwrap();
        let data = cell.data.lock().await;
        assert_eq!(data.snapshot["lifecycle"], "closed");
        assert_eq!(data.snapshot["lastTurnOutcome"], "turn_canceled");
        assert_eq!(data.snapshot["_live"], false);
        assert!(data.snapshot["activeTurnId"].is_null());
        assert!(data.session.is_none());
        assert!(cell.owner_ended.is_cancelled());
    }

    async fn timeline_request(manager: &AgentManager, id: &str, options: Value) -> Value {
        let (source, mut output) = RequestContext::attach(crate::transport::SourceConfig {
            id: "history-test-source".into(),
            session_id: "history-test-session".into(),
            client_id: "history-test-client".into(),
            capabilities: json!({"owned_subscriptions":true,"all_providers":true,"timeline_notifications":true}),
            principal: json!({"id":"owner","kind":"owner"}),
            permissions: Arc::new(std::sync::RwLock::new(
                ["workspace.read".to_owned()].into_iter().collect(),
            )),
            routed: false,
            app_version: None,
            cancel: CancellationToken::new(),
        });
        let mut message = json!({
            "type":"fetch_agent_timeline_request","requestId":"history",
            "agentId":id,"projection":"projected"
        });
        merge(&mut message, &options);
        let request = source.for_request(message.clone());
        assert!(manager.handle(&request, &message).await.unwrap());
        let frame = output.recv().await.unwrap();
        let OutboundFrame::Text(text) = &frame.frame else {
            panic!("expected timeline response");
        };
        let response: Value = serde_json::from_str(text).unwrap();
        assert_eq!(response["message"]["type"], "fetch_agent_timeline_response");
        let payload = response["message"]["payload"].clone();
        assert!(payload["error"].is_null(), "{payload}");
        source.detach();
        payload
    }

    #[tokio::test]
    async fn omp_history_recovers_partial_checkpoint_before_public_timeline_and_live_events() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("paseo");
        let original = root.path().join("original.jsonl");
        let records = [
            json!({"type":"title","title":"History fixture","v":1}),
            json!({"type":"session","version":3,"id":"native-session","cwd":root.path(),"timestamp":"2026-10-24T00:00:00.000Z"}),
            json!({"type":"model_change","id":"model","parentId":null,"model":"test/model"}),
            json!({"type":"message","id":"user-old","parentId":"model","timestamp":"2026-10-24T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"old question"}]}}),
            json!({"type":"message","id":"abandoned","parentId":"user-old","message":{"role":"assistant","content":[{"type":"text","text":"inactive branch"}]}}),
            json!({"type":"message","id":"answer-old","parentId":"user-old","timestamp":"2026-10-24T00:00:02.000Z","message":{"role":"assistant","responseId":"answer-old","content":[{"type":"text","text":"old answer"}]}}),
            json!({"type":"message","id":"user-recent","parentId":"answer-old","timestamp":"2026-10-31T00:00:01.000Z","message":{"role":"user","content":"recent question"}}),
            json!({"type":"message","id":"answer-recent","parentId":"user-recent","timestamp":"2026-10-31T00:00:02.000Z","message":{"role":"assistant","responseId":"answer-recent","content":[{"type":"text","text":"recent answer"}]}}),
        ];
        let original_bytes = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&original, &original_bytes).await.unwrap();
        let app = AppState::new(
            home.clone(),
            root.path().to_owned(),
            std::env::current_exe().unwrap(),
        );
        let manager = &app.agents;
        manager
            .storage
            .get_or_init(|| async { AgentStorage::new(home.clone()) })
            .await;
        let id = "00000000-0000-4000-8000-000000000001";
        let mut partial = Timeline::new();
        partial.append(json!({"type":"notification","level":"info","message":"previous startup"}), &json!({}));
        partial.append(json!({"type":"user_message","text":"recent question","messageId":"user-recent"}), &json!({}));
        partial.persist(&manager.timeline_path(id).unwrap(), id).unwrap();
        let old_epoch = partial.epoch().to_owned();
        let persistence = json!({
            "provider":"omp","sessionId":"native-session","nativeHandle":original,
            "metadata":{"cwd":root.path(),"model":"test/model"}
        });
        let snapshot = json!({
            "id":id,"provider":"omp","cwd":root.path(),"config":{"model":"test/model"},
            "persistence":persistence,"internal":false,"createdAt":"2026-10-24T00:00:00.000Z",
            "updatedAt":"2026-10-31T00:00:02.000Z","requiresAttention":false
        });
        let (cell, receiver) = manager.make_cell(id, snapshot, false).await.unwrap();
        assert!(!cell.data.lock().await.history_primed);
        *cell.task.lock().await = Some(manager.start_events(id, &cell, receiver));
        let provider = JsonlProvider::new(ProviderSpec {
            id: "omp".into(),
            family: "omp".into(),
            command: Vec::new(),
            env: BTreeMap::new(),
            options: json!({}),
            defaults: json!({}),
            runtime_settings: json!({}),
            configured_models: Vec::new(),
        });
        // The real OMP parser reads a private fixture without launching or resuming a CLI.
        let session = provider
            .resume_session(
                persistence,
                json!({"cwd":root.path(),"model":"test/model"}),
                LaunchContext {
                    agent_id: id.into(),
                    home,
                    executable: app.executable.clone(),
                    env: BTreeMap::new(),
                    mcp_servers: json!({}),
                    tool_catalog: json!([]),
                    tool_manifest: json!([]),
                    tool_executor: None,
                    cancel: cell.cancel.child_token(),
                },
                json!({"purpose":"history"}),
                cell.sink.clone(),
            )
            .await
            .unwrap();
        {
            let mut data = cell.data.lock().await;
            data.session = Some(session.clone());
            data.ready = true;
            data.snapshot["lifecycle"] = json!("idle");
        }
        // Reproduce a fetch racing the owning resume, plus startup/live output
        // arriving before that resume finishes its history read.
        let lane = manager.lane(id).await;
        let lane_guard = lane.lock().await;
        let pending = timeline_request(manager, id, json!({"direction":"tail","limit":2}));
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        for item in [
            json!({"type":"notification","level":"info","message":"current startup"}),
            json!({"type":"user_message","text":"live question","messageId":"user-live"}),
        ] {
            cell.sink
                .emit(json!({"type":"timeline","provider":"omp","item":item}))
                .await
                .unwrap();
        }
        manager.drain_events(&cell).await.unwrap();
        let hydration = manager.hydrate_history(id, &cell, false);
        tokio::pin!(hydration);
        assert!(futures_util::poll!(&mut hydration).is_pending());
        cell.sink
            .emit(json!({"type":"timeline","provider":"omp","item":{
                "type":"assistant_message","text":"live answer","messageId":"answer-live"
            }}))
            .await
            .unwrap();
        hydration.await.unwrap();
        drop(lane_guard);
        let tail = pending.await;
        assert_ne!(tail["epoch"], old_epoch);
        assert_eq!(tail["hasOlder"], true);
        assert_eq!(tail["entries"][0]["item"]["messageId"], "user-live");
        assert_eq!(tail["entries"][1]["item"]["messageId"], "answer-live");

        let stale = timeline_request(manager, id, json!({
            "direction":"after","limit":2,"cursor":{"epoch":old_epoch,"seq":2}
        })).await;
        assert_eq!(stale["reset"], true);
        assert_eq!(stale["entries"], tail["entries"]);
        let before = timeline_request(manager, id, json!({
            "direction":"before","limit":2,"cursor":tail["startCursor"]
        })).await;
        assert_eq!(before["entries"][0]["item"]["messageId"], "answer-recent");
        assert_eq!(before["entries"][1]["item"]["type"], "notification");

        let mut cursor = json!({"epoch":tail["epoch"],"seq":0});
        let mut entries = Vec::new();
        loop {
            let page = timeline_request(manager, id, json!({
                "direction":"after","limit":2,"cursor":cursor
            })).await;
            entries.extend(page["entries"].as_array().unwrap().iter().cloned());
            if page["hasNewer"] != true {
                break;
            }
            let next = page["endCursor"].clone();
            assert_ne!(next, cursor);
            cursor = next;
        }
        assert_eq!(entries.iter().filter(|row| row["item"]["type"] == "notification"
            && row["item"]["message"] == "previous startup").count(), 1);
        assert_eq!(entries[0]["item"]["messageId"], "user-old");
        assert_eq!(entries[0]["timestamp"], "2026-10-24T00:00:01.000Z");
        assert_eq!(entries[1]["item"]["messageId"], "answer-old");
        let message_ids: Vec<_> = entries.iter()
            .filter_map(|row| row["item"]["messageId"].as_str()).collect();
        assert_eq!(message_ids, ["user-old", "answer-old", "user-recent", "answer-recent", "user-live", "answer-live"]);
        assert!(entries.windows(2).all(|rows| rows[0]["seqEnd"].as_i64() < rows[1]["seqStart"].as_i64()));
        manager.hydrate_history(id, &cell, false).await.unwrap();
        let unchanged = timeline_request(manager, id, json!({"direction":"tail","limit":2})).await;
        assert_eq!(unchanged["epoch"], tail["epoch"]);
        assert_eq!(unchanged["window"], tail["window"]);
        let recovered = Timeline::load(&manager.timeline_path(id).unwrap(), id).unwrap();
        assert_eq!(recovered.items(), entries.iter().map(|row| row["item"].clone()).collect::<Vec<_>>());
        assert_eq!(recovered.epoch(), tail["epoch"].as_str().unwrap());
        assert_eq!(recovered.fetch(&json!({"limit":0}))["window"], tail["window"]);
        assert_eq!(tokio::fs::read(&original).await.unwrap(), original_bytes.as_bytes());
        session.close().await.unwrap();
        cell.cancel.cancel();
        cell.task.lock().await.take().unwrap().await.unwrap();
    }

    #[tokio::test]
    async fn archived_history_reopen_refresh_and_close_preserve_activity_stamps() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let transcript = root.path().join("history.jsonl");
        let lines = [
            json!({"type":"session","version":3,"id":"native-history","cwd":root.path(),"timestamp":"2026-01-01T00:00:00.000Z"}),
            json!({"type":"message","id":"question","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"old question"}}),
            json!({"type":"message","id":"answer","parentId":"question","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"old answer"}]}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        tokio::fs::write(&transcript, &lines).await.unwrap();
        let app = AppState::new(home.clone(), root.path().into(), std::env::current_exe().unwrap());
        let manager = &app.agents;
        manager.storage.get_or_init(|| async { AgentStorage::new(home.clone()) }).await;
        let id = "00000000-0000-4000-8000-000000000044";
        let persistence = json!({"provider":"omp","sessionId":"native-history","nativeHandle":transcript,"metadata":{"cwd":root.path()}});
        let original = json!({
            "id":id,"provider":"omp","cwd":root.path(),"config":{},"internal":false,
            "persistence":persistence,"lastStatus":"closed","requiresAttention":false,
            "createdAt":"2026-01-01T00:00:00.000Z","updatedAt":"2026-01-03T00:00:00.000Z",
            "lastActivityAt":"2026-01-01T00:00:02.000Z","archivedAt":"2026-01-03T00:00:00.000Z",
            "_historyOnly":true
        });
        manager.storage().unwrap().upsert(original.clone()).await.unwrap();
        let provider = JsonlProvider::new(ProviderSpec {
            id:"omp".into(),family:"omp".into(),command:Vec::new(),env:BTreeMap::new(),
            options:json!({}),defaults:json!({}),runtime_settings:json!({}),configured_models:Vec::new(),
        });
        let (cell, receiver) = manager.make_cell(id, original.clone(), false).await.unwrap();
        *cell.task.lock().await = Some(manager.start_events(id, &cell, receiver));
        let session = provider.resume_session(
            persistence.clone(),json!({"cwd":root.path()}),
            LaunchContext {
                agent_id:id.into(),home:home.clone(),executable:app.executable.clone(),
                env:BTreeMap::new(),mcp_servers:json!({}),tool_catalog:json!([]),
                tool_manifest:json!([]),tool_executor:None,cancel:cell.cancel.child_token(),
            },
            json!({"purpose":"history"}),cell.sink.clone(),
        ).await.unwrap();
        let restored = manager.register_session(id, &cell, session, Some(persistence), true).await.unwrap();
        assert_eq!(restored["updatedAt"], original["updatedAt"]);
        assert_eq!(restored["lastActivityAt"], original["lastActivityAt"]);
        manager.hydrate_history(id, &cell, true).await.unwrap();
        assert_eq!(cell.timeline.lock().unwrap().rows()[1]["item"]["text"], "old answer");
        let refreshed = manager.agent_snapshot(id).await.unwrap().unwrap();
        assert_eq!(refreshed["updatedAt"], original["updatedAt"]);
        assert_eq!(refreshed["lastActivityAt"], original["lastActivityAt"]);
        assert_eq!(refreshed["historySync"]["status"], "complete");
        manager.close_cell(id, &cell).await.unwrap();
        let closed = AgentStorage::new(home).get(id).await.unwrap().unwrap();
        assert_eq!(closed["updatedAt"], original["updatedAt"]);
        assert_eq!(closed["lastActivityAt"], original["lastActivityAt"]);
        assert_eq!(closed["archivedAt"], original["archivedAt"]);
        assert_eq!(tokio::fs::read_to_string(transcript).await.unwrap(), lines);
    }

    #[test]
    fn turn_failure_message_includes_code_and_distinct_diagnostic() {
        assert_eq!(
            turn_failed_message(&json!({"error":"  boom  ","code":" E42 ","diagnostic":"boom"})),
            "boom\n\ncode: E42"
        );
        assert_eq!(
            turn_failed_message(&json!({"error":"","diagnostic":"stderr tail"})),
            "Provider run failed\n\nstderr tail"
        );
    }

    #[test]
    fn attention_is_edge_triggered_and_keeps_unread_state() {
        let mut finished = json!({"lifecycle":"idle","requiresAttention":false});
        assert_eq!(
            transition_attention(Some("running"), &mut finished),
            Some("finished")
        );
        assert_eq!(finished["attention"]["attentionReason"], "finished");

        // An unread finish is not replaced by a later failure.
        let mut failed = finished.clone();
        failed["lifecycle"] = json!("error");
        assert_eq!(transition_attention(Some("running"), &mut failed), None);
        assert_eq!(failed["attentionReason"], "finished");

        let mut errored = json!({"lifecycle":"error","requiresAttention":false});
        assert_eq!(
            transition_attention(Some("running"), &mut errored),
            Some("error")
        );
        let mut still_error = json!({"lifecycle":"error","requiresAttention":false});
        assert_eq!(transition_attention(Some("error"), &mut still_error), None);

        let mut started = json!({"lifecycle":"running","requiresAttention":false});
        assert_eq!(transition_attention(Some("idle"), &mut started), None);

        let mut internal = json!({"lifecycle":"idle","internal":true});
        assert_eq!(transition_attention(Some("running"), &mut internal), None);
    }
}
