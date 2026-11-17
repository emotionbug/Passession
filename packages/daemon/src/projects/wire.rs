use super::*;
use std::collections::BTreeSet;
use tokio::sync::broadcast;

impl ProjectService {
    pub(super) async fn handle_wire(&self, ctx: &RequestContext, m: &Value) -> Result<bool> {
        let kind = m["type"].as_str().unwrap_or("");
        let response = match kind {
            "project.list.request" => "project.list.response",
            "fetch_workspaces_request" => "fetch_workspaces_response",
            "open_project_request" => "open_project_response",
            "project.add.request" => "project.add.response",
            "project.create_directory.request" => "project.create_directory.response",
            "project.rename.request" => "project.rename.response",
            "project.remove.request" => "project.remove.response",
            "workspace.title.set.request" => "workspace.title.set.response",
            "workspace.pin.set.request" => "workspace.pin.set.response",
            "archive_workspace_request" => "archive_workspace_response",
            "paseo_worktree_list_request" => "paseo_worktree_list_response",
            "paseo_worktree_archive_request" => "paseo_worktree_archive_response",
            "create_paseo_worktree_request" => "create_paseo_worktree_response",
            "workspace.create.request" => "workspace.create.response",
            "workspace.recovery.inspect.request" => "workspace.recovery.inspect.response",
            "workspace.recovery.restore.request" => "workspace.recovery.restore.response",
            "workspace.label.list.request" => "workspace.label.list.response",
            "workspace.label.assignment.set.request" => "workspace.label.assignment.set.response",
            "workspace.label.update.request" => "workspace.label.update.response",
            "workspace.label.delete.request" => "workspace.label.delete.response",
            "workspace.label.delete.inspect.request" => "workspace.label.delete.inspect.response",
            "workspace.clear_attention.request" => "workspace.clear_attention.response",
            "workspace.mark_unread.request" => "workspace.mark_unread.response",
            "project.icon.set.request" => "project.icon.set.response",
            "project.icon.get.request" => "project.icon.get.response",
            "project_icon_request" => "project_icon_response",
            "workspace.github.search_repositories.request" => {
                "workspace.github.search_repositories.response"
            }
            "project.github.clone.request" => "project.github.clone.response",
            _ => return Ok(false),
        };
        if kind == "workspace.label.list.request"
            && m.get("subscribe").is_some_and(|s| !s.is_null())
        {
            if let Err(error) = self.subscribe_labels(ctx, m).await {
                send_rpc_error(ctx, m, &error).await?;
            }
            return Ok(true);
        }
        if kind == "fetch_workspaces_request" && m.get("subscribe").is_some_and(|s| !s.is_null()) {
            if let Err(error) = self.subscribe_workspaces(ctx, m).await {
                send_rpc_error(ctx, m, &error).await?;
            }
            return Ok(true);
        }
        if kind == "workspace.create.request" {
            if let Err(error) = self.create_wire(ctx, m).await {
                let mut payload = failure(kind, m, &error);
                payload["requestId"] = m["requestId"].clone();
                ctx.send(json!({"type":response,"payload":payload})).await?;
            }
            return Ok(true);
        }
        let result = self.execute_wire(ctx, m).await;
        if let Err(error) = &result
            && (kind.starts_with("workspace.label.")
                || matches!(kind, "project.list.request" | "fetch_workspaces_request"))
        {
            send_rpc_error(ctx, m, error).await?;
            return Ok(true);
        }
        let mut payload = match result {
            Ok(payload) => payload,
            Err(error) => failure(kind, m, &error),
        };
        payload["requestId"] = m["requestId"].clone();
        ctx.send(json!({"type":response,"payload":payload})).await?;
        Ok(true)
    }
    async fn execute_wire(&self, ctx: &RequestContext, m: &Value) -> Result<Value> {
        let app = self.app()?;
        match text(m, "type")? {
            "project.list.request" => {
                let mut projects = self.list_projects().await?;
                for p in &mut projects {
                    let snapshot = self.icon_snapshot(p).await?;
                    p["projectIconRevision"] = snapshot["revision"].clone();
                }
                directory::fetch_projects(&app, m, projects)
            }
            "fetch_workspaces_request" => {
                directory::fetch(
                    &app,
                    m,
                    self.list_workspaces().await?,
                    self.list_projects().await?,
                )
                .await
            }
            "open_project_request" => {
                let workspace = self.resolve_workspace(Path::new(text(m, "cwd")?)).await?;
                Ok(
                    json!({"workspace":self.descriptor(text(&workspace,"workspaceId")?).await?,"error":null}),
                )
            }
            "project.add.request" => {
                let project = self.register_project(Path::new(text(m, "cwd")?)).await?;
                Ok(json!({"project":directory::project_descriptor(&project),"error":null}))
            }
            "project.create_directory.request" => self.create_project_directory(m).await,
            "project.rename.request" => {
                let id = text(m, "projectId")?;
                let name = m["customName"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let p = self
                    .storage()
                    .await?
                    .update_project(id, |old| {
                        let mut p = old.clone();
                        p["customName"] = json!(name);
                        p["updatedAt"] = json!(now());
                        Ok(p)
                    })
                    .await?
                    .ok_or_else(|| anyhow!("Project not found"))?;
                self.publish_project(&p).await?;
                Ok(json!({"projectId":id,"accepted":true,"customName":name,"error":null}))
            }
            "project.remove.request" => {
                let _guard = self.lifecycle.lock().await;
                let id = text(m, "projectId")?;
                // `handleProjectRemoveRequest`: every active workspace is marked
                // archiving (and published) before any of them is archived.
                let active: Vec<String> = self
                    .list_workspaces()
                    .await?
                    .into_iter()
                    .filter(|w| w["projectId"] == id && w["archivedAt"].is_null())
                    .filter_map(|w| w["workspaceId"].as_str().map(str::to_owned))
                    .collect();
                if !active.is_empty() {
                    self.directory_state.mark_archiving(&active, &now());
                    for wid in &active {
                        self.publish_workspace(wid).await?;
                    }
                }
                let mut removed = Vec::new();
                let result = async {
                    for wid in &active {
                        self.archive_locked(ctx.principal(), wid, None).await?;
                        removed.push(wid.clone());
                    }
                    self.storage().await?.remove_project(id).await
                }
                .await;
                self.directory_state.clear_archiving(&active);
                result?;
                if let Err(error) = self.remove_icon(id).await {
                    tracing::warn!(%error,"Failed to clean up removed project icon");
                }
                self.icon_snapshots
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(id);
                self.sync_project_watches().await?;
                let payload = app
                    .agents
                    .directory_sync
                    .sequence_project_update(json!({"kind":"remove","projectId":id}), true)?;
                app.transport
                    .publish_event(json!({"type":"project.update","payload":payload}))
                    .await?;
                Ok(
                    json!({"projectId":id,"accepted":true,"removedWorkspaceIds":removed,"error":null}),
                )
            }
            "workspace.title.set.request" => {
                let id = text(m, "workspaceId")?;
                if !self.rename_workspace(id, m["title"].as_str()).await? {
                    bail!("Workspace not found")
                };
                Ok(
                    json!({"workspaceId":id,"accepted":true,"title":self.workspace(id).await?.unwrap()["title"],"error":null}),
                )
            }
            "workspace.pin.set.request" => {
                let id = text(m, "workspaceId")?;
                let pinned = if m["pinned"] == true {
                    json!(now())
                } else {
                    Value::Null
                };
                self.storage()
                    .await?
                    .update_workspace(id, |old| {
                        let mut w = old.clone();
                        w["pinnedAt"] = pinned.clone();
                        w["updatedAt"] = json!(now());
                        Ok(w)
                    })
                    .await?
                    .ok_or_else(|| anyhow!("Workspace not found"))?;
                self.publish_workspace(id).await?;
                Ok(json!({"workspaceId":id,"accepted":true,"pinnedAt":pinned,"error":null}))
            }
            "archive_workspace_request" => {
                let result = self
                    .archive_workspace(ctx.principal(), text(m, "workspaceId")?)
                    .await?;
                Ok(
                    json!({"workspaceId":m["workspaceId"],"archivedAt":result["archivedAt"],"error":null}),
                )
            }
            // COMPAT(managedWorktrees): retained wire requests are rejected, not reinterpreted.
            "paseo_worktree_list_request"
            | "paseo_worktree_archive_request"
            | "create_paseo_worktree_request" => Err(unsupported_worktrees()),
            "workspace.recovery.inspect.request" => {
                let id = text(m, "workspaceId")?;
                let workspace = self.workspace(id).await?;
                let project = match workspace.as_ref() {
                    Some(w) => self.project(text(w, "projectId")?).await?,
                    None => None,
                };
                Ok(
                    json!({"state":worktrees::inspect_recovery(id,workspace.as_ref(),project.as_ref()).await?}),
                )
            }
            "workspace.recovery.restore.request" => {
                let _guard = self.lifecycle.lock().await;
                let id = text(m, "workspaceId")?;
                let w = self
                    .workspace(id)
                    .await?
                    .ok_or_else(|| anyhow!("Workspace not found: {id}"))?;
                require_directory(Path::new(text(&w, "cwd")?)).await?;
                self.unarchive(w).await?;
                Ok(json!({"workspaceId":id,"accepted":true,"error":null}))
            }
            "workspace.label.list.request" => {
                Ok(self.storage().await?.list_labels(m.get("sync")).await)
            }
            "workspace.label.assignment.set.request" => {
                let result = self
                    .storage()
                    .await?
                    .set_label_assignment(
                        text(m, "workspaceId")?,
                        &m["label"],
                        m["assigned"] == true,
                    )
                    .await?;
                self.publish_workspace(text(m, "workspaceId")?).await?;
                Ok(result)
            }
            "workspace.label.update.request" => {
                let result = self
                    .storage()
                    .await?
                    .update_label(text(m, "name")?, m["newName"].as_str(), m["color"].as_str())
                    .await?;
                self.publish_all_workspaces().await?;
                Ok(result)
            }
            "workspace.label.delete.request" => {
                let result = self.storage().await?.delete_label(text(m, "name")?).await?;
                self.publish_all_workspaces().await?;
                Ok(result)
            }
            "workspace.label.delete.inspect.request" => Ok(
                json!({"affectedWorkspaceCount":self.storage().await?.count_label_workspaces(text(m,"name")?).await?}),
            ),
            "workspace.clear_attention.request" => self.clear_attention(ctx.principal(), m).await,
            "workspace.mark_unread.request" => self.mark_unread(ctx.principal(), m).await,
            "project.icon.set.request" => self.set_icon(m).await,
            "project.icon.get.request" => {
                let id = text(m, "projectId")?;
                let p = self
                    .project(id)
                    .await?
                    .ok_or_else(|| anyhow!("Project not found"))?;
                Ok(json!({"projectId":id,"icon":self.cached_icon(&p).await?,"error":null}))
            }
            "project_icon_request" => Ok(
                json!({"cwd":m["cwd"],"icon":crate::files::project_icon(Path::new(text(m,"cwd")?)).await?,"error":null}),
            ),
            "workspace.github.search_repositories.request" => {
                self.search_repositories(m, ctx.cancellation()).await
            }
            "project.github.clone.request" => self.clone_project(m, ctx.cancellation()).await,
            _ => bail!("Unknown project operation"),
        }
    }
    async fn publish_all_workspaces(&self) -> Result<()> {
        for w in self.list_workspaces().await? {
            self.publish_workspace(text(&w, "workspaceId")?).await?;
        }
        Ok(())
    }
    async fn create_wire(&self, ctx: &RequestContext, m: &Value) -> Result<()> {
        if m.get("agent").is_some() && !ctx.allows("workspace.write") && !ctx.allows("hub.execute")
        {
            bail!("Session is not authorized to create an agent")
        }
        let app = self.app()?;
        let operation = if m["subscribe"] == true {
            Some(ctx.retain_operation(&["workspace.create.update", "agent.create.update"])?)
        } else {
            None
        };
        let mut events = app.agents.subscribe_creation_events();
        let key = m["idempotencyKey"]
            .as_str()
            .or_else(|| m["requestId"].as_str())
            .unwrap_or("")
            .to_owned();
        let future = app
            .agents
            .create_workspace_with_receipt(ctx.principal(), m.clone());
        tokio::pin!(future);
        let mut progress_open = operation.is_some();
        let result = loop {
            tokio::select! {
                result=&mut future=>break result,
                event=events.recv(),if progress_open=>match event{
                    Ok(snapshot) if snapshot["kind"]=="workspace"&&snapshot["idempotencyKey"]==key=>{if let Some(op)=operation.as_ref()&& op.emit(json!({"type":"workspace.create.update","payload":snapshot})).await.is_err(){progress_open=false;}},
                    Ok(_)=>{},
                    Err(broadcast::error::RecvError::Lagged(_))=>{
                        if let Some(snapshot)=app.agents.creation_snapshot("workspace",&key).await?&& let Some(op)=operation.as_ref()&& op.emit(json!({"type":"workspace.create.update","payload":snapshot})).await.is_err(){progress_open=false;}
                    },
                    Err(broadcast::error::RecvError::Closed)=>{progress_open=false;},
                }
            }
        };
        let mut payload = match result {
            Ok(creation) => {
                let mut p = json!({"workspace":creation.get("workspace").cloned().unwrap_or(Value::Null),"error":creation["error"],"setupTerminalId":null});
                for key in ["agent", "errorCode", "setupSkippedReason"] {
                    if let Some(v) = creation.get(key) {
                        p[key] = v.clone();
                    }
                }
                p["creation"] = creation;
                p
            }
            Err(error) => failure("workspace.create.request", m, &error),
        };
        payload["requestId"] = m["requestId"].clone();
        let send = ctx
            .send(json!({"type":"workspace.create.response","payload":payload}))
            .await;
        if let Some(op) = operation {
            op.release().await?;
        }
        send
    }
    async fn subscribe_labels(&self, ctx: &RequestContext, m: &Value) -> Result<()> {
        let owner =
            ctx.begin_subscription("labels", m["subscribe"]["subscriptionId"].as_str(), None)?;
        let bootstrap = async {
            let (mut initial, events) = self.storage().await?.subscribe_labels(m.get("sync")).await;
            initial["requestId"] = m["requestId"].clone();
            initial["subscriptionId"] = json!(owner.response_id());
            ctx.send(json!({"type":"workspace.label.list.response","payload":initial}))
                .await?;
            owner.activate().await?;
            Ok::<_, anyhow::Error>(events)
        }
        .await;
        let mut events = match bootstrap {
            Ok(events) => events,
            Err(error) => {
                owner.release().await?;
                return Err(error);
            }
        };
        let request_id = m["requestId"].clone();
        tokio::spawn(async move {
            let cancel = owner.cancellation();
            loop {
                tokio::select! {_ =cancel.cancelled()=>break,event=events.recv()=>match event{
                    Ok(event)=>{if owner.emit(json!({"type":"workspace.label.update","payload":event})).await.is_err(){break}},
                    Err(broadcast::error::RecvError::Lagged(_))=>{
                        // Dropped catalog events must terminate the source-owned stream, not claim a complete delta.
                        let _=owner.emit(json!({"type":"rpc_error","payload":{"requestId":request_id,"requestType":"workspace.label.list.request","code":"workspace_label_operation_failed","error":"Workspace label subscription fell behind; fetch a fresh snapshot"}})).await;
                        break
                    },Err(broadcast::error::RecvError::Closed)=>break,
                }}
            }
            let _ = owner.release().await;
        });
        Ok(())
    }
    async fn subscribe_workspaces(&self, ctx: &RequestContext, m: &Value) -> Result<()> {
        let owner = ctx.begin_subscription(
            "workspaces",
            m["subscribe"]["subscriptionId"].as_str(),
            None,
        )?;
        let bootstrap = async {
            let app = self.app()?;
            directory::validate_fetch(m)?;
            // The response and the first delta baseline share one rebuild.
            let baseline = directory::Observation::build(&app).await?;
            let mut initial = directory::fetch_entries(
                &app,
                m,
                baseline.entries().to_vec(),
                baseline.projects(),
            )?;
            initial["requestId"] = m["requestId"].clone();
            initial["subscriptionId"] = json!(owner.response_id());
            ctx.send(json!({"type":"fetch_workspaces_response","payload":initial}))
                .await?;
            owner.activate().await?;
            Ok::<_, anyhow::Error>((initial, baseline, directory::observe_workspaces(&app)))
        }
        .await;
        let (initial, baseline, mut observations) = match bootstrap {
            Ok(value) => value,
            Err(error) => {
                owner.release().await?;
                return Err(error);
            }
        };
        let weak = self.app.clone();
        let request = m.clone();
        tokio::spawn(async move {
            let include_background = request["includeBackground"] == true
                || request["filter"]["includeBackground"] == true;
            let sequenced = request.get("sync").is_some_and(|v| !v.is_null());
            let mut visible: BTreeSet<String> = initial["entries"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v["id"].as_str().map(str::to_owned))
                .collect();
            let mut previous = Arc::new(baseline);
            let cancel = owner.cancellation();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    changed = observations.changed() => if changed.is_err() { break },
                }
                let Some(observation) = observations.borrow_and_update().clone() else {
                    continue;
                };
                // A rebuild that began before this subscription's baseline is older than it.
                if observation.started() < previous.started() {
                    continue;
                }
                let Some(app) = weak.upgrade() else { break };
                let updates = directory::workspace_deltas(
                    &previous,
                    &observation,
                    include_background,
                    request.get("filter"),
                    &mut visible,
                );
                let records = observation.records();
                let projects = observation.projects();
                for (id, value, mut payload) in updates {
                    if payload["kind"] == "remove" {
                        let project_id = records
                            .iter()
                            .find(|w| w["workspaceId"] == id)
                            .or_else(|| previous.entry(&id))
                            .and_then(|w| w["projectId"].as_str());
                        if let Some(project_id) = project_id {
                            if let Some(project) = projects
                                .iter()
                                .find(|p| p["projectId"] == project_id && p["archivedAt"].is_null())
                            {
                                if !records.iter().any(|w| {
                                    w["projectId"] == project_id && w["archivedAt"].is_null()
                                }) {
                                    let mut enriched = project.clone();
                                    if let Ok(icon) = app.projects.icon_snapshot(project).await {
                                        enriched["projectIconRevision"] = icon["revision"].clone();
                                    }
                                    payload["emptyProject"] =
                                        directory::project_descriptor(&enriched);
                                }
                            } else if !projects.iter().any(|p| p["projectId"] == project_id) {
                                payload["removedProjectId"] = json!(project_id);
                            }
                        }
                    }
                    let payload = match app.agents.directory_sync.sequence_workspace_update(
                        payload,
                        value,
                        &id,
                        sequenced,
                        include_background,
                    ) {
                        Ok(p) => p,
                        Err(error) => {
                            tracing::warn!(%error,"Workspace sequencing failed");
                            let _ = owner.release().await;
                            return;
                        }
                    };
                    if owner
                        .emit(json!({"type":"workspace_update","payload":payload}))
                        .await
                        .is_err()
                    {
                        let _ = owner.release().await;
                        return;
                    }
                }
                previous = observation;
            }
            let _ = owner.release().await;
        });
        Ok(())
    }
    async fn clear_attention(&self, actor: &Value, m: &Value) -> Result<Value> {
        let requested = m["workspaceId"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![m["workspaceId"].clone()]);
        let app = self.app()?;
        let agents = app.agents.list_snapshots().await?;
        let mut results = Vec::new();
        let mut all = Vec::new();
        let mut errors = Vec::new();
        for id in requested {
            let id = id.as_str().context("Invalid workspace ID")?;
            let mut cleared = Vec::new();
            let result = async {
                self.workspace(id)
                    .await?
                    .filter(|w| w["archivedAt"].is_null())
                    .ok_or_else(|| anyhow!("Workspace not found: {id}"))?;
                for agent in &agents {
                    if agent["workspaceId"] == id
                        && agent["archivedAt"].is_null()
                        && agent["internal"] != true
                        && agent["requiresAttention"] == true
                        && agent["attentionReason"] != "permission"
                        && agent["pendingPermissions"]
                            .as_array()
                            .is_none_or(Vec::is_empty)
                    {
                        let aid = text(agent, "id")?;
                        app.agents
                            .control(actor, aid, "clear_attention", json!({}))
                            .await?;
                        cleared.push(aid.to_owned());
                    }
                }
                self.publish_workspace(id).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let error = result.err().map(|e| e.to_string());
            if let Some(error) = &error {
                errors.push(error.clone());
            }
            all.extend(cleared.clone());
            results.push(json!({"workspaceId":id,"clearedAgentIds":cleared,"success":error.is_none(),"error":error}));
        }
        Ok(
            json!({"workspaceId":m["workspaceId"],"clearedAgentIds":all,"results":results,"success":errors.is_empty(),"error":if errors.is_empty(){Value::Null}else{json!(errors.join("; "))}}),
        )
    }
    async fn mark_unread(&self, actor: &Value, m: &Value) -> Result<Value> {
        let id = text(m, "workspaceId")?;
        self.workspace(id)
            .await?
            .filter(|w| w["archivedAt"].is_null())
            .ok_or_else(|| anyhow!("Workspace not found: {id}"))?;
        let app = self.app()?;
        let agents = app.agents.list_snapshots().await?;
        let mut candidates: Vec<_> = agents
            .iter()
            .filter(|a| {
                a["workspaceId"] == id
                    && a["archivedAt"].is_null()
                    && a["internal"] != true
                    && matches!(a["status"].as_str(), Some("idle" | "closed"))
                    && a["requiresAttention"] != true
                    && a["pendingPermissions"].as_array().is_none_or(Vec::is_empty)
                    && match a["labels"]["paseo.parent-agent-id"]
                        .as_str()
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                    {
                        None => true,
                        Some(parent) => agents
                            .iter()
                            .find(|other| other["id"] == parent)
                            .is_some_and(|other| other["workspaceId"] != a["workspaceId"]),
                    }
            })
            .collect();
        candidates.sort_by(|a, b| {
            b["updatedAt"]
                .as_str()
                .cmp(&a["updatedAt"].as_str())
                .then(a["id"].as_str().cmp(&b["id"].as_str()))
        });
        let agent = candidates
            .first()
            .ok_or_else(|| anyhow!("Workspace has no finished agent to mark unread: {id}"))?;
        let aid = text(agent, "id")?;
        app.agents
            .control(actor, aid, "mark_unread", json!({}))
            .await?;
        Ok(json!({"workspaceId":id,"markedAgentId":aid,"success":true,"error":null}))
    }
}
fn failure(kind: &str, m: &Value, error: &anyhow::Error) -> Value {
    let code = error
        .downcast_ref::<RequestError>()
        .map(|e| e.code)
        .or_else(|| error.downcast_ref::<store::StoreError>().map(|e| e.code))
        .unwrap_or("unknown");
    let mut p = json!({"error":error.to_string()});
    match kind {
        "open_project_request" | "workspace.create.request" | "create_paseo_worktree_request" => {
            p["workspace"] = Value::Null;
            p["errorCode"] = json!(code);
            if kind != "open_project_request" {
                p["setupTerminalId"] = Value::Null;
            }
        }
        "project.add.request" => {
            p["project"] = Value::Null;
            p["errorCode"] = json!(code);
        }
        "project.create_directory.request" => {
            p["project"] = Value::Null;
            p["directoryPath"] = Value::Null;
            p["errorCode"] = json!(code);
        }
        "archive_workspace_request" => {
            p["workspaceId"] = m["workspaceId"].clone();
            p["archivedAt"] = Value::Null;
        }
        "project.github.clone.request" => {
            p["repo"] = m["repo"].clone();
            p["checkoutPath"] = Value::Null;
            p["project"] = Value::Null;
        }
        "project.icon.get.request" => {
            p["projectId"] = m["projectId"].clone();
            p["icon"] = Value::Null;
        }
        "project_icon_request" => {
            p["cwd"] = m["cwd"].clone();
            p["icon"] = Value::Null;
        }
        "paseo_worktree_list_request" => {
            p["worktrees"] = json!([]);
            p["error"] = json!({"code":"UNKNOWN","message":error.to_string()});
        }
        "paseo_worktree_archive_request" => {
            p["success"] = json!(false);
            p["removedAgents"] = json!([]);
            p["error"] = json!({"code":"UNKNOWN","message":error.to_string()});
        }
        "workspace.mark_unread.request" => {
            p["workspaceId"] = m["workspaceId"].clone();
            p["markedAgentId"] = Value::Null;
            p["success"] = json!(false);
        }
        "workspace.clear_attention.request" => {
            p["workspaceId"] = m["workspaceId"].clone();
            p["clearedAgentIds"] = json!([]);
            let ids = m["workspaceId"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![m["workspaceId"].clone()]);
            p["results"]=json!(ids.into_iter().map(|id|json!({"workspaceId":id,"clearedAgentIds":[],"success":false,"error":error.to_string()})).collect::<Vec<_>>());
            p["success"] = json!(false);
        }
        kind if kind.starts_with("workspace.label.") => {
            p["errorCode"] = json!(code);
        }
        kind if kind.ends_with(".set.request")
            || kind.ends_with(".rename.request")
            || kind.ends_with(".remove.request")
            || kind.ends_with(".restore.request") =>
        {
            p["accepted"] = json!(false);
            for key in ["projectId", "workspaceId"] {
                if let Some(v) = m.get(key) {
                    p[key] = v.clone();
                }
            }
            if kind == "workspace.title.set.request" {
                p["title"] = Value::Null;
            }
            if kind == "workspace.pin.set.request" {
                p["pinnedAt"] = Value::Null;
            }
            if kind == "project.rename.request" {
                p["customName"] = Value::Null;
            }
            if kind == "project.remove.request" {
                p["removedWorkspaceIds"] = json!([]);
            }
        }
        _ => {}
    }
    p
}
async fn send_rpc_error(ctx: &RequestContext, m: &Value, error: &anyhow::Error) -> Result<()> {
    let code = error
        .downcast_ref::<RequestError>()
        .map(|e| e.code)
        .or_else(|| error.downcast_ref::<store::StoreError>().map(|e| e.code))
        .or_else(|| {
            error
                .downcast_ref::<directory::DirectoryRequestError>()
                .map(|e| e.code)
        })
        .unwrap_or(match m["type"].as_str() {
            Some("project.list.request") => "project_list_failed",
            Some("fetch_workspaces_request") => "fetch_workspaces_failed",
            _ => "workspace_label_operation_failed",
        });
    ctx.send(json!({"type":"rpc_error","payload":{"requestId":m["requestId"],"requestType":m["type"],"code":code,"error":error.to_string()}})).await
}
