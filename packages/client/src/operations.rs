use crate::{ClientError, DaemonClient};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};
use tokio_util::sync::CancellationToken;

/// Explicit per-operation lifetime. Cancellation does not roll back a submitted mutation.
#[derive(Clone)]
pub struct RequestOptions {
    pub timeout: Duration,
    pub cancel: CancellationToken,
}
impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            cancel: CancellationToken::new(),
        }
    }
}

macro_rules! operations {
    ($($name:ident => ($request:literal, $response:literal),)*) => {
        impl DaemonClient { $(
            #[doc = concat!("Invoke `", $request, "` with wire-shaped fields and return its response payload. Use `rpc` for a custom deadline or cancellation.")]
            pub async fn $name(&self, fields: Value) -> Result<Value> {
                self.rpc($request, $response, fields, RequestOptions::default()).await
            }
        )* }
    }
}
operations! {
    list_hosts => ("hosts.list.request", "hosts.list.response"),
    add_host => ("hosts.add.request", "hosts.add.response"),
    update_host => ("hosts.update.request", "hosts.update.response"),
    remove_host => ("hosts.remove.request", "hosts.remove.response"),
    open_host_route => ("hosts.route.open.request", "hosts.route.open.response"),
    open_host_link => ("hosts.link.open.request", "hosts.link.open.response"),
    mark_workspace_unread => ("workspace.mark_unread.request", "workspace.mark_unread.response"),
    unregister_push_token => ("push.unregister.request", "push.unregister.response"),
    update_workspace_label => ("workspace.label.update.request", "workspace.label.update.response"),
    delete_workspace_label => ("workspace.label.delete.request", "workspace.label.delete.response"),
    inspect_workspace_label_delete => ("workspace.label.delete.inspect.request", "workspace.label.delete.inspect.response"),
    open_project => ("open_project_request", "open_project_response"),
    add_project => ("project.add.request", "project.add.response"),
    create_project_directory => ("project.create_directory.request", "project.create_directory.response"),
    search_github_repositories => ("workspace.github.search_repositories.request", "workspace.github.search_repositories.response"),
    clone_github_project => ("project.github.clone.request", "project.github.clone.response"),
    start_workspace_script => ("start_workspace_script_request", "start_workspace_script_response"),
    list_workspace_scripts => ("workspace.script.list.request", "workspace.script.list.response"),
    start_workspace_script_with_status => ("workspace.script.start.request", "workspace.script.start.response"),
    stop_workspace_script => ("workspace.script.stop.request", "workspace.script.stop.response"),
    archive_workspace => ("archive_workspace_request", "archive_workspace_response"),
    fetch_workspace_setup_status => ("workspace_setup_status_request", "workspace_setup_status_response"),
    run_workspace_setup => ("workspace.setup.run.request", "workspace.setup.run.response"),
    fetch_agent => ("fetch_agent_request", "fetch_agent_response"),
    detach_agent => ("agent.detach.request", "agent.detach.response"),
    rename_project => ("project.rename.request", "project.rename.response"),
    set_project_icon => ("project.icon.set.request", "project.icon.set.response"),
    remove_project => ("project.remove.request", "project.remove.response"),
    set_workspace_title => ("workspace.title.set.request", "workspace.title.set.response"),
    set_workspace_pinned => ("workspace.pin.set.request", "workspace.pin.set.response"),
    inspect_workspace_recovery => ("workspace.recovery.inspect.request", "workspace.recovery.inspect.response"),
    restore_workspace => ("workspace.recovery.restore.request", "workspace.recovery.restore.response"),
    append_agent_timeline_item => ("agent.timeline.append.request", "agent.timeline.append.response"),
    search_agent_timeline => ("agent.timeline.search.request", "agent.timeline.search.response"),
    get_checkout_diff => ("checkout.diff.get.request", "checkout.diff.get.response"),
    checkout_commit => ("checkout_commit_request", "checkout_commit_response"),
    checkout_merge => ("checkout_merge_request", "checkout_merge_response"),
    checkout_merge_from_base => ("checkout_merge_from_base_request", "checkout_merge_from_base_response"),
    checkout_pull => ("checkout_pull_request", "checkout_pull_response"),
    checkout_push => ("checkout_push_request", "checkout_push_response"),
    checkout_refresh => ("checkout.refresh.request", "checkout.refresh.response"),
    list_checkout_commits => ("checkout.commits.list.request", "checkout.commits.list.response"),
    get_commit_file_diff => ("checkout.commits.file_diff.request", "checkout.commits.file_diff.response"),
    checkout_pr_create => ("checkout_pr_create_request", "checkout_pr_create_response"),
    checkout_pr_merge => ("checkout_pr_merge_request", "checkout_pr_merge_response"),
    checkout_forge_set_auto_merge => ("checkout.forge.set_auto_merge.request", "checkout.forge.set_auto_merge.response"),
    checkout_github_set_auto_merge => ("checkout.github.set_auto_merge.request", "checkout.github.set_auto_merge.response"),
    checkout_forge_get_check_details => ("checkout.forge.get_check_details.request", "checkout.forge.get_check_details.response"),
    checkout_github_get_check_details => ("checkout.github.get_check_details.request", "checkout.github.get_check_details.response"),
    checkout_pr_status => ("checkout_pr_status_request", "checkout_pr_status_response"),
    pull_request_timeline => ("pull_request_timeline_request", "pull_request_timeline_response"),
    checkout_switch_branch => ("checkout_switch_branch_request", "checkout_switch_branch_response"),
    rename_branch => ("checkout.rename_branch.request", "checkout.rename_branch.response"),
    stash_save => ("stash_save_request", "stash_save_response"),
    stash_pop => ("stash_pop_request", "stash_pop_response"),
    stash_list => ("stash_list_request", "stash_list_response"),
    get_paseo_worktree_list => ("paseo_worktree_list_request", "paseo_worktree_list_response"),
    archive_paseo_worktree => ("paseo_worktree_archive_request", "paseo_worktree_archive_response"),
    create_paseo_worktree => ("create_paseo_worktree_request", "create_paseo_worktree_response"),
    validate_branch => ("validate_branch_request", "validate_branch_response"),
    get_branch_suggestions => ("branch_suggestions_request", "branch_suggestions_response"),
    search_forge => ("forge.search.request", "forge.search.response"),
    search_git_hub => ("github_search_request", "github_search_response"),
    get_directory_suggestions => ("directory_suggestions_request", "directory_suggestions_response"),
    write_file => ("fs.file.write.request", "fs.file.write.response"),
    create_file_entry => ("fs.entry.create.request", "fs.entry.create.response"),
    rename_file_entry => ("fs.entry.rename.request", "fs.entry.rename.response"),
    duplicate_file_entry => ("fs.entry.duplicate.request", "fs.entry.duplicate.response"),
    delete_file_entry => ("fs.entry.delete.request", "fs.entry.delete.response"),
    checkout_discard_changes => ("checkout.discard_changes.request", "checkout.discard_changes.response"),
    request_download_token => ("file_download_token_request", "file_download_token_response"),
    request_project_icon => ("project_icon_request", "project_icon_response"),
    get_project_icon => ("project.icon.get.request", "project.icon.get.response"),
    list_provider_models => ("list_provider_models_request", "list_provider_models_response"),
    list_provider_modes => ("list_provider_modes_request", "list_provider_modes_response"),
    list_provider_features => ("list_provider_features_request", "list_provider_features_response"),
    list_available_providers => ("list_available_providers_request", "list_available_providers_response"),
    get_daemon_config => ("get_daemon_config_request", "get_daemon_config_response"),
    get_daemon_status => ("daemon.get_status.request", "daemon.get_status.response"),
    connect_hub => ("hub.management.daemon.connect.request", "hub.management.daemon.connect.response"),
    update_hub_permissions => ("hub.management.daemon.permissions.update.request", "hub.management.daemon.permissions.update.response"),
    get_hub_status => ("hub.management.daemon.get_status.request", "hub.management.daemon.get_status.response"),
    disconnect_hub => ("hub.management.daemon.disconnect.request", "hub.management.daemon.disconnect.response"),
    get_daemon_pairing_offer => ("daemon.get_pairing_offer.request", "daemon.get_pairing_offer.response"),
    patch_daemon_config => ("set_daemon_config_request", "set_daemon_config_response"),
    read_project_config => ("read_project_config_request", "read_project_config_response"),
    write_project_config => ("write_project_config_request", "write_project_config_response"),
    refresh_providers_snapshot => ("refresh_providers_snapshot_request", "refresh_providers_snapshot_response"),
    get_provider_diagnostic => ("provider_diagnostic_request", "provider_diagnostic_response"),
    list_commands => ("list_commands_request", "list_commands_response"),
    get_plugin_catalog => ("plugin.catalog.get.request", "plugin.catalog.get.response"),
    list_plugins => ("plugin.list.request", "plugin.list.response"),
    get_plugin_logs => ("plugin.logs.get.request", "plugin.logs.get.response"),
    get_agent_skills_status => ("agent.skills.get_status.request", "agent.skills.get_status.response"),
    reconcile_agent_skills => ("agent.skills.reconcile.request", "agent.skills.reconcile.response"),
    uninstall_agent_skills => ("agent.skills.uninstall.request", "agent.skills.uninstall.response"),
    save_agent_skills_selection => ("agent.skills.save_selection.request", "agent.skills.save_selection.response"),
    import_legacy_agent_skills_selection => ("agent.skills.import_legacy_selection.request", "agent.skills.import_legacy_selection.response"),
    install_directory_plugin => ("plugin.directory.install.request", "plugin.directory.install.response"),
    install_plugin_source => ("plugin.source.install.request", "plugin.source.install.response"),
    get_plugin_source_status => ("plugin.source.status.request", "plugin.source.status.response"),
    preview_plugin_updates => ("plugin.source.update.preview.request", "plugin.source.update.preview.response"),
    apply_plugin_updates => ("plugin.source.update.apply.request", "plugin.source.update.apply.response"),
    update_plugin_sources => ("plugin.source.update.request", "plugin.source.update.response"),
    inspect_directory_plugin => ("plugin.directory.inspect.request", "plugin.directory.inspect.response"),
    remove_plugin => ("plugin.remove.request", "plugin.remove.response"),
    invoke_plugin_rpc => ("plugin.rpc.invoke.request", "plugin.rpc.invoke.response"),
    list_terminals => ("list_terminals_request", "list_terminals_response"),
    create_terminal => ("create_terminal_request", "create_terminal_response"),
    rename_terminal => ("terminal.rename.request", "terminal.rename.response"),
    kill_terminal => ("kill_terminal_request", "kill_terminal_response"),
    close_items => ("close_items_request", "close_items_response"),
    capture_terminal => ("capture_terminal_request", "capture_terminal_response"),
    schedule_create => ("schedule/create", "schedule/create/response"),
    schedule_list => ("schedule/list", "schedule/list/response"),
    schedule_inspect => ("schedule/inspect", "schedule/inspect/response"),
    schedule_logs => ("schedule/logs", "schedule/logs/response"),
    schedule_pause => ("schedule/pause", "schedule/pause/response"),
    schedule_resume => ("schedule/resume", "schedule/resume/response"),
    schedule_delete => ("schedule/delete", "schedule/delete/response"),
    schedule_run_once => ("schedule/run-once", "schedule/run-once/response"),
    clear_agent_attention => ("clear_agent_attention", "clear_agent_attention_response"),
    clear_workspace_attention => ("workspace.clear_attention.request", "workspace.clear_attention.response"),
    register_browser_host => ("browser.host.register.request", "browser.host.register.response"),
    fetch_agent_history => ("fetch_agent_history_request", "fetch_agent_history_response"),
    fetch_recent_provider_sessions => ("fetch_recent_provider_sessions_request", "fetch_recent_provider_sessions_response"),
    fetch_workspaces => ("fetch_workspaces_request", "fetch_workspaces_response"),
    list_projects => ("project.list.request", "project.list.response"),
    delete_agent => ("delete_agent_request", "agent_deleted"),
    archive_agent => ("archive_agent_request", "agent_archived"),
    resume_agent => ("resume_agent_request", "status"),
    import_agent => ("import_agent_request", "status"),
    refresh_agent => ("refresh_agent_request", "status"),
    fetch_agent_timeline => ("fetch_agent_timeline_request", "fetch_agent_timeline_response"),
    list_agent_timeline_prompts => ("agent.timeline.list_prompts.request", "agent.timeline.list_prompts.response"),
    list_provider_subagents => ("agent.provider_subagents.list.request", "agent.provider_subagents.list.response"),
    delete_provider_subagents => ("agent.provider_subagents.delete.request", "agent.provider_subagents.delete.response"),
    fetch_provider_subagent_timeline => ("agent.provider_subagents.timeline.get.request", "agent.provider_subagents.timeline.get.response"),
    build_agent_fork_context => ("agent.fork_context.request", "agent.fork_context.response"),
    send_agent_message => ("send_agent_message_request", "send_agent_message_response"),
    get_agent_message_receipt => ("agent.message_receipt.get.request", "agent.message_receipt.get.response"),
    dismiss_agent_message_receipt => ("agent.message_receipt.dismiss.request", "agent.message_receipt.dismiss.response"),
    rewind_agent => ("agent.rewind.request", "agent.rewind.response"),
    cancel_agent => ("cancel_agent_request", "cancel_agent_response"),
    set_agent_mode => ("set_agent_mode_request", "set_agent_mode_response"),
    set_agent_model => ("set_agent_model_request", "set_agent_model_response"),
    set_agent_feature => ("set_agent_feature_request", "set_agent_feature_response"),
    set_agent_thinking_option => ("set_agent_thinking_request", "set_agent_thinking_response"),
    apply_agent_config => ("agent.config.apply.request", "agent.config.apply.response"),
    get_checkout_status => ("checkout_status_request", "checkout_status_response"),
    fetch_agents => ("fetch_agents_request", "fetch_agents_response"),
    list_workspace_labels => ("workspace.label.list.request", "workspace.label.list.response"),
    set_workspace_label => ("workspace.label.assignment.set.request", "workspace.label.assignment.set.response"),
    schedule_update => ("schedule/update", "schedule/update/response"),
    update_daemon => ("daemon.update.request", "daemon.update.response"),
    submit_daemon_update => ("daemon.update.submit.request", "daemon.update.submit.response"),
    get_daemon_update_status => ("daemon.update.status.request", "daemon.update.status.response"),
    reload_daemon_config => ("daemon.config.reload.request", "daemon.config.reload.response"),
    collect_diagnostics => ("diagnostics.request", "diagnostics.response"),
    list_provider_usage => ("provider.usage.list.request", "provider.usage.list.response"),
    list_usage_reports => ("usage.list_reports.request", "usage.list_reports.response"),
    get_providers_snapshot => ("get_providers_snapshot_request", "get_providers_snapshot_response"),
}

impl DaemonClient {
    /// All protocol operations are available without dropping unknown provider/plugin fields.
    pub async fn rpc(
        &self,
        request: &str,
        response: &str,
        mut fields: Value,
        options: RequestOptions,
    ) -> Result<Value> {
        if fields.is_null() {
            fields = json!({});
        }
        ensure!(fields.is_object(), "RPC fields must be an object");
        fields["type"] = request.into();
        self.request_cancellable(fields, response, options.timeout, options.cancel)
            .await
    }

    pub async fn ping(&self, timeout: Duration) -> Result<Duration> {
        let start = tokio::time::Instant::now();
        let sent = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        self.request(json!({"type":"ping","clientSentAt":sent}), "pong", timeout)
            .await?;
        Ok(start.elapsed())
    }

    pub async fn update_agent(&self, agent_id: &str, mut patch: Value) -> Result<Value> {
        ensure!(patch.is_object(), "Agent patch must be an object");
        patch["agentId"] = agent_id.into();
        self.rpc(
            "update_agent_request",
            "update_agent_response",
            patch,
            RequestOptions::default(),
        )
        .await
    }

    pub async fn list_directory(&self, mut fields: Value) -> Result<Value> {
        if fields.is_null() {
            fields = json!({});
        }
        ensure!(fields.is_object(), "Directory query must be an object");
        fields["mode"] = "list".into();
        self.rpc(
            "file_explorer_request",
            "file_explorer_response",
            fields,
            RequestOptions::default(),
        )
        .await
    }

    pub async fn create_agent(&self, fields: Value, options: RequestOptions) -> Result<Value> {
        self.create("agent", fields, options).await
    }

    pub async fn create_workspace(&self, fields: Value, options: RequestOptions) -> Result<Value> {
        self.create("workspace", fields, options).await
    }

    async fn create(
        &self,
        kind: &str,
        mut fields: Value,
        options: RequestOptions,
    ) -> Result<Value> {
        ensure!(fields.is_object(), "Creation fields must be an object");
        if fields["idempotencyKey"].is_null() {
            fields["idempotencyKey"] = uuid::Uuid::new_v4().to_string().into();
        }
        self.require_feature(
            "creationLifecycle",
            "Update the host to use keyed creation lifecycle",
        )?;
        // The key remains in error details so callers can inspect or recover an uncertain result.
        let key = fields["idempotencyKey"].clone();
        let result = self
            .rpc(
                &format!("{kind}.create.request"),
                &format!("{kind}.create.response"),
                fields,
                options,
            )
            .await;
        result.map_err(|error| match error.downcast::<ClientError>() {
            Ok(mut error) => {
                let mut detail = error.details.take().unwrap_or_else(|| json!({}));
                if !detail.is_object() {
                    detail = json!({"response":detail});
                }
                detail["idempotencyKey"] = key;
                detail["kind"] = kind.into();
                error.details = Some(detail);
                error.into()
            }
            Err(error) => error,
        })
    }

    /// Wait for a turn, independently cancellable from cancellation of the agent itself.
    pub async fn wait_for_finish(&self, agent_id: &str, options: RequestOptions) -> Result<Value> {
        self.rpc(
            "wait_for_finish_request",
            "wait_for_finish_response",
            json!({"agentId":agent_id}),
            options,
        )
        .await
    }

    pub async fn send_heartbeat(&self, fields: Value) -> Result<()> {
        self.notify("client_heartbeat", fields).await
    }
    pub async fn register_push_token(&self, token: &str) -> Result<()> {
        self.notify("register_push_token", json!({"token":token}))
            .await
    }
    pub async fn respond_to_permission(&self, fields: Value) -> Result<()> {
        self.notify("agent_permission_response", fields).await
    }
    pub async fn send_terminal_input(&self, fields: Value) -> Result<()> {
        self.notify("terminal_input", fields).await
    }

    pub async fn notify(&self, kind: &str, mut fields: Value) -> Result<()> {
        ensure!(fields.is_object(), "Notification fields must be an object");
        fields["type"] = kind.into();
        self.send(fields).await
    }

    pub async fn fetch_all_agents(&self, fields: Value) -> Result<Vec<Value>> {
        let mut pages = CursorPages::agents(fields);
        let mut agents = Vec::new();
        while let Some(page) = pages.next(self, RequestOptions::default()).await? {
            let entries = page["entries"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Agent page omitted entries"))?;
            agents.extend(
                entries
                    .iter()
                    .map(|entry| entry.get("agent").unwrap_or(entry).clone()),
            );
        }
        Ok(agents)
    }
}

/// A bounded-memory cursor walker. Cursors are opaque and never reconstructed from IDs.
/// A page is returned only once, and a repeated cursor is a protocol error, not an infinite loop.
pub struct CursorPages {
    request: String,
    response: String,
    fields: Value,
    request_cursor: String,
    response_cursor: String,
    page_pointer: String,
    seen: HashSet<String>,
    done: bool,
}
impl CursorPages {
    pub fn new(
        request: &str,
        response: &str,
        fields: Value,
        request_cursor: &str,
        response_cursor: &str,
        page_pointer: &str,
    ) -> Self {
        Self {
            request: request.into(),
            response: response.into(),
            fields,
            request_cursor: request_cursor.into(),
            response_cursor: response_cursor.into(),
            page_pointer: page_pointer.into(),
            seen: HashSet::new(),
            done: false,
        }
    }
    pub fn agents(fields: Value) -> Self {
        Self::new(
            "fetch_agents_request",
            "fetch_agents_response",
            fields,
            "/page/cursor",
            "/pageInfo/nextCursor",
            "",
        )
    }
    pub fn workspaces(fields: Value) -> Self {
        Self::new(
            "fetch_workspaces_request",
            "fetch_workspaces_response",
            fields,
            "/page/cursor",
            "/pageInfo/nextCursor",
            "",
        )
    }
    pub fn history(fields: Value) -> Self {
        Self::new(
            "fetch_agent_history_request",
            "fetch_agent_history_response",
            fields,
            "/page/cursor",
            "/pageInfo/nextCursor",
            "",
        )
    }
    pub fn timeline(agent_id: &str, direction: &str, limit: usize) -> Result<Self> {
        ensure!(
            matches!(direction, "before" | "after"),
            "Timeline direction must be before or after"
        );
        Ok(Self::new(
            "fetch_agent_timeline_request",
            "fetch_agent_timeline_response",
            json!({"agentId":agent_id,"direction":direction,"limit":limit}),
            "/cursor",
            if direction == "before" {
                "/startCursor"
            } else {
                "/endCursor"
            },
            "",
        ))
    }
    pub async fn next(
        &mut self,
        client: &DaemonClient,
        options: RequestOptions,
    ) -> Result<Option<Value>> {
        if self.done {
            return Ok(None);
        }
        if self.fields.is_null() {
            self.fields = json!({});
        }
        ensure!(
            self.fields.is_object(),
            "Pagination fields must be an object"
        );
        if self.request_cursor == "/page/cursor" && self.fields["page"].is_null() {
            self.fields["page"] = json!({"limit":100});
        }
        let payload = client
            .rpc(&self.request, &self.response, self.fields.clone(), options)
            .await?;
        let page = payload
            .pointer(&self.page_pointer)
            .ok_or_else(|| anyhow::anyhow!("Response omitted page"))?;
        let exhausted = if self.request == "fetch_agent_timeline_request" {
            page[if self.fields["direction"] == "before" {
                "hasOlder"
            } else {
                "hasNewer"
            }] == false
        } else {
            page.pointer("/pageInfo/hasMore") == Some(&Value::Bool(false))
        };
        if exhausted {
            self.done = true;
            return Ok(Some(page.clone()));
        }
        let cursor = page
            .pointer(&self.response_cursor)
            .filter(|v| !v.is_null())
            .cloned();
        match cursor {
            None => self.done = true,
            Some(cursor) => {
                ensure!(
                    self.seen.insert(serde_json::to_string(&cursor)?),
                    "Daemon repeated a pagination cursor"
                );
                let segments: Vec<_> = self
                    .request_cursor
                    .trim_start_matches('/')
                    .split('/')
                    .collect();
                let mut field = &mut self.fields;
                for segment in segments {
                    if field.is_null() {
                        *field = json!({});
                    }
                    ensure!(
                        field.is_object(),
                        "Pagination cursor path must contain objects"
                    );
                    field = &mut field[segment];
                }
                *field = cursor;
            }
        }
        Ok(Some(page.clone()))
    }
}
