//! Per-host project/workspace/agent directory (Flutter `AppStore._refreshUntilCurrent`,
//! `_refreshHost`, `_hydrateDirectory`, `_allPages`, `_event`, `_scheduleResync`,
//! `_syncMaintenance`, `_markChildTabOpen`).
//!
//! Partial hydration: the event feed is installed before any baseline, then the three lists
//! hydrate independently. A failed list keeps its last complete rows and reports in
//! `listErrors`; updates for a list still hydrating are queued in arrival order and applied
//! after its baseline. A generation mismatch, or a feed restored during its own refresh,
//! schedules one more pass on the host's current client (never debounced indefinitely).
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    rc::Rc,
    time::Duration,
};

use serde_json::{Value, json};

use super::{Conn, Core, Host, hosts, requests, store};
use crate::{CoreError, ids, protocol::response_error, storage::Replica};

const EVENTS: [&str; 13] = [
    "project.update",
    "providers_snapshot_update",
    "agent_attention_required",
    "terminal_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
    "agent.provider_subagents.update",
    "checkout_status_update",
    "script_status_update",
    "status.server_info",
    "workspace_setup_progress",
    "activity_log",
    "status.daemon_config_changed",
];
const MAINTENANCE_FEATURE: &str = "durableDaemonUpdates";
const MAINTENANCE_EVENT: &str = "daemon.update.maintenance";
const RESYNC_DELAY: Duration = Duration::from_millis(150);
const PAGE: u64 = 200;

#[derive(Default)]
pub(crate) struct Directory {
    pub projects: Vec<Value>,
    pub workspaces: Vec<Value>,
    pub agents: Vec<Value>,
    pub cached_at: Option<String>,
    pub loading: bool,
    pub hydrating: BTreeSet<&'static str>,
    pub list_errors: BTreeMap<&'static str, CoreError>,
    queued: Vec<Value>,
    generations: HashMap<&'static str, String>,
    sequences: HashMap<&'static str, i64>,
    pub refreshing: bool,
    refresh_client: Option<u64>,
    resync_for: Option<u64>,
    /// Refresh attempts started; directory observations remember theirs.
    pub epoch: u64,
    resync: Option<tokio::task::AbortHandle>,
    pub maintenance: Option<String>,
}

impl Directory {
    pub fn apply_replica(&mut self, replica: Replica) {
        self.projects = replica.projects;
        self.workspaces = replica.workspaces;
        self.agents = replica.agents;
        self.cached_at = replica.cached_at;
    }
}

fn kind_of(message_type: &str) -> &'static str {
    if message_type.starts_with("agent") {
        "agent"
    } else if message_type.starts_with("project") {
        "project"
    } else {
        "workspace"
    }
}

fn affects_project(message: &Value) -> bool {
    message["type"] == "workspace_update"
        && (message["payload"]["emptyProject"].is_object()
            || !message["payload"]["removedProjectId"].is_null())
}

fn require_success(payload: Value, request_type: &str) -> Result<Value, CoreError> {
    match response_error(&payload, request_type) {
        Some(error) => Err(error),
        None => Ok(payload),
    }
}

// ---------------------------------------------------------------------------------------------
// Refresh

/// Flutter `refreshHost`: a running refresh of an older client hydrates the current one again
/// once it settles instead of handing back a refresh of the old client.
pub(crate) fn refresh_host(core: &Rc<Core>, host: &Rc<Host>) {
    if core.closing.get() || !core.contains(host) {
        return;
    }
    {
        let mut dir = host.dir.borrow_mut();
        if dir.refreshing {
            if dir.refresh_client != Some(host.client_epoch.get()) {
                dir.resync_for = Some(host.client_epoch.get());
            }
            return;
        }
        dir.refreshing = true;
    }
    // Requests issued from now on wait for this attempt instead of failing as not connected.
    if host.conn().is_none() {
        host.enable();
        hosts::start_attempt(core, host);
    }
    let core = core.clone();
    let host = host.clone();
    tokio::task::spawn_local(async move {
        loop {
            {
                let mut dir = host.dir.borrow_mut();
                dir.resync_for = None;
                if let Some(timer) = dir.resync.take() {
                    timer.abort();
                }
                dir.epoch += 1;
                dir.refresh_client = Some(host.client_epoch.get());
            }
            refresh_once(&core, &host).await;
            let again = {
                let dir = host.dir.borrow();
                let client = host.client_epoch.get();
                !core.closing.get()
                    && core.contains(&host)
                    && dir.resync_for == Some(client)
                    && host.detached_epoch.get() != Some(client)
            };
            if !again {
                break;
            }
        }
        let mut dir = host.dir.borrow_mut();
        dir.refreshing = false;
        dir.refresh_client = None;
        dir.resync_for = None;
    });
}

async fn refresh_once(core: &Rc<Core>, host: &Rc<Host>) {
    host.dir.borrow_mut().loading = true;
    *host.error.borrow_mut() = None;
    core.touch();
    if let Err(error) = hydrate(core, host).await
        && core.contains(host) {
            *host.error.borrow_mut() = Some(error);
        }
    {
        let mut dir = host.dir.borrow_mut();
        dir.hydrating.clear();
        dir.loading = false;
    }
    drain(core, host);
    core.touch();
}

async fn hydrate(core: &Rc<Core>, host: &Rc<Host>) -> Result<(), CoreError> {
    let conn = match hosts::connect(core, host).await {
        Ok(conn) => conn,
        Err(error) => {
            let selector = host.selector.borrow().clone();
            if let Some(selector) = selector {
                selector.report_active_failure(&host.config_id(), &error);
            }
            return Err(error);
        }
    };
    let actual = conn.server_id();
    if actual.is_empty() {
        return Err(CoreError::new(
            "HOST_IDENTITY",
            "Connected host did not supply a server identity.",
            false,
        ));
    }
    let expected = host.config.borrow()["expectedServerId"].as_str().map(str::to_owned);
    match expected {
        Some(expected) if expected != actual => {
            return Err(CoreError::new(
                "DAEMON_IDENTITY_MISMATCH",
                "Host identity changed. Confirm the new host before replacing this connection.",
                false,
            ));
        }
        Some(_) => {}
        None => {
            let config = {
                let mut config = host.config.borrow_mut();
                config["expectedServerId"] = actual.clone().into();
                config.clone()
            };
            let mut connections = host.connections.borrow_mut();
            if let Some(entry) = connections.iter_mut().find(|c| c["id"] == config["id"]) {
                *entry = config;
            }
        }
    }
    let previous = host.server_id();
    if previous != actual {
        host.set_server_id(&actual);
        core.host_event(
            &actual,
            json!({"type":"host.identity_changed","payload":{"previousHostId":previous,"hostId":actual}}),
        );
    }
    let duplicate = core
        .hosts
        .borrow()
        .iter()
        .find(|other| !Rc::ptr_eq(other, host) && other.server_id() == actual)
        .cloned();
    if let Some(duplicate) = duplicate {
        hosts::merge_into(core, host, &duplicate, &previous).await;
        return Ok(());
    }
    requests::release_internal(core, host);
    {
        let mut dir = host.dir.borrow_mut();
        dir.list_errors.clear();
        dir.hydrating.extend(["project", "workspace", "agent"]);
        dir.queued.clear();
    }
    let client = host.client_epoch.get();
    let epoch = host.dir.borrow().epoch;
    let current = || core.contains(host) && host.client_epoch.get() == client;
    // Explicit-event clients receive these broadcasts only while an owned feed is installed.
    // Establish it before baselines so updates during pagination are queued, not lost.
    let mut events: Vec<Value> = EVENTS.iter().map(|e| Value::from(*e)).collect();
    if conn.supports(MAINTENANCE_FEATURE) {
        events.push(MAINTENANCE_EVENT.into());
    }
    if conn.supports("pluginManagement") {
        events.push("status.plugin_catalog_changed".into());
    }
    requests::observe_internal(
        core,
        host,
        json!({"type":"session.events.set_subscription.request","events":events,"notifications":true}),
        "session.events.set_subscription.response",
        epoch,
    )
    .await?;
    host.dir.borrow_mut().loading = false;
    core.touch();
    let background = conn.supports("backgroundWorkspaces");
    let projects = hydrate_list(core, host, "project", async {
        let snapshot = require_success(
            conn.request(
                json!({"type":"project.list.request"}),
                "project.list.response",
                Some(requests::REQUEST_TIMEOUT),
                None,
                None,
            )
            .await?,
            "project.list.request",
        )?;
        if current() {
            let rows = rows(&snapshot["projects"]);
            let mut dir = host.dir.borrow_mut();
            dir.projects = rows;
            cursor(&mut dir, "project", &snapshot);
        }
        Ok(())
    });
    let workspaces = hydrate_list(core, host, "workspace", async {
        let rows = observed_pages(
            core,
            host,
            &conn,
            "fetch_workspaces_request",
            "fetch_workspaces_response",
            background,
            epoch,
            client,
            "workspace",
        )
        .await?;
        if let Some(rows) = rows {
            host.dir.borrow_mut().workspaces = rows;
        }
        Ok(())
    });
    let agents = hydrate_list(core, host, "agent", async {
        let rows = observed_pages(
            core,
            host,
            &conn,
            "fetch_agents_request",
            "fetch_agents_response",
            background,
            epoch,
            client,
            "agent",
        )
        .await?;
        if let Some(rows) = rows {
            host.dir.borrow_mut().agents = rows.iter().map(agent_row).collect();
        }
        Ok(())
    });
    tokio::join!(projects, workspaces, agents);
    if !current() {
        return Ok(());
    }
    mark_child_tabs(core, host, &conn);
    if host.origin.is_none() && conn.supports("connectedHosts") {
        let (_, snapshot) = requests::observe_internal(
            core,
            host,
            json!({"type":"hosts.list.request","observe":true}),
            "hosts.list.response",
            epoch,
        )
        .await?;
        let core = core.clone();
        let origin = host.clone();
        tokio::task::spawn_local(async move {
            hosts::discover_routes(&core, &origin, snapshot["directory"].clone()).await;
        });
    }
    store::schedule_save(core);
    Ok(())
}

fn rows(value: &Value) -> Vec<Value> {
    value
        .as_array()
        .map(|rows| rows.iter().filter(|row| row.is_object()).cloned().collect())
        .unwrap_or_default()
}

/// Flutter `_agentRow`: the agent object with its project and sync sequence.
fn agent_row(entry: &Value) -> Value {
    if !entry["agent"].is_object() {
        return entry.clone();
    }
    let mut row = entry["agent"].clone();
    row["project"] = entry["project"].clone();
    if let Some(seq) = entry.get("syncSeq") {
        row["syncSeq"] = seq.clone();
    }
    row
}

fn cursor(dir: &mut Directory, kind: &'static str, payload: &Value) {
    let sync = &payload["sync"];
    if let Some(generation) = sync["generation"].as_str() {
        dir.generations.insert(kind, generation.to_owned());
    }
    if let Some(seq) = sync["headSeq"].as_f64() {
        dir.sequences.insert(kind, seq as i64);
    }
}

/// One list's baseline (Flutter `_hydrateDirectory`).
async fn hydrate_list(
    core: &Rc<Core>,
    host: &Rc<Host>,
    kind: &'static str,
    load: impl Future<Output = Result<(), CoreError>>,
) {
    let result = load.await;
    {
        let mut dir = host.dir.borrow_mut();
        match result {
            Ok(()) => dir.cached_at = Some(ids::now_rfc3339()),
            // Retain the last complete list; other baselines publish independently.
            Err(error) => {
                dir.list_errors.insert(kind, error);
            }
        }
        dir.hydrating.remove(kind);
    }
    drain(core, host);
    if core.contains(host) {
        store::schedule_save(core);
        core.touch();
    }
}

/// Subscribes a directory list, then walks every page (Flutter observe + `_allPages`).
#[allow(clippy::too_many_arguments)]
async fn observed_pages(
    core: &Rc<Core>,
    host: &Rc<Host>,
    conn: &Rc<Conn>,
    request: &str,
    response: &str,
    background: bool,
    epoch: u64,
    client: u64,
    kind: &'static str,
) -> Result<Option<Vec<Value>>, CoreError> {
    let mut message = json!({"type":request,"subscribe":{},"page":{"limit":PAGE}});
    if background {
        message["includeBackground"] = true.into();
    }
    let (id, snapshot) = requests::observe_internal(core, host, message, response, epoch).await?;
    let current = || core.contains(host) && host.client_epoch.get() == client;
    if !current() {
        requests::release_observation(core, &id);
        return Ok(None);
    }
    let snapshot = require_success(snapshot, request)?;
    let mut rows = rows(&snapshot["entries"]);
    let mut page = snapshot["pageInfo"].clone();
    let mut seen = BTreeSet::new();
    while page["hasMore"] == true {
        let next_cursor = page["nextCursor"].as_str().unwrap_or_default().to_owned();
        if next_cursor.is_empty() || !seen.insert(next_cursor.clone()) {
            return Err(CoreError::new(
                "DIRECTORY_CURSOR",
                "Host returned an invalid directory cursor.",
                false,
            ));
        }
        let mut message = json!({"type":request,"page":{"limit":PAGE,"cursor":next_cursor}});
        if background {
            message["includeBackground"] = true.into();
        }
        let next = require_success(
            conn.request(message, response, Some(requests::REQUEST_TIMEOUT), None, None)
                .await?,
            request,
        )?;
        if host.client_epoch.get() != client {
            return Err(CoreError::new(
                "DIRECTORY_CLIENT_CHANGED",
                "Host connection changed during directory pagination.",
                false,
            ));
        }
        rows.extend(self::rows(&next["entries"]));
        page = next["pageInfo"].clone();
    }
    if !current() {
        return Ok(None);
    }
    cursor(&mut host.dir.borrow_mut(), kind, &snapshot);
    Ok(Some(rows))
}

// ---------------------------------------------------------------------------------------------
// Live updates

fn drain(core: &Rc<Core>, host: &Rc<Host>) {
    let queued = std::mem::take(&mut host.dir.borrow_mut().queued);
    for message in queued {
        directory_event(core, host, &message);
    }
}

/// Every non-correlated message of the host's connection (Flutter `_event`).
pub(crate) fn on_message(core: &Rc<Core>, host: &Rc<Host>, _conn: &Rc<Conn>, message: &Value) {
    if !core.contains(host) {
        return;
    }
    let kind = message["type"].as_str().unwrap_or_default();
    let payload = &message["payload"];
    match kind {
        "hosts.changed" => {
            let core = core.clone();
            let origin = host.clone();
            let directory = payload["directory"].clone();
            tokio::task::spawn_local(async move {
                hosts::discover_routes(&core, &origin, directory).await;
            });
        }
        MAINTENANCE_EVENT => {
            let id = payload["operationId"].as_str().unwrap_or_default();
            let Some(active) = payload["active"].as_bool() else {
                return;
            };
            if id.is_empty() {
                return;
            }
            let changed = {
                let mut dir = host.dir.borrow_mut();
                let next = if active {
                    Some(id.to_owned())
                } else if dir.maintenance.as_deref() == Some(id) {
                    None
                } else {
                    dir.maintenance.clone()
                };
                let changed = next != dir.maintenance;
                dir.maintenance = next;
                changed
            };
            if changed {
                core.touch();
            }
        }
        "status" if payload["status"] == "server_info" => {
            if payload["serverId"].as_str().is_some_and(|id| !id.is_empty()) {
                *host.server_info.borrow_mut() = payload.clone();
            }
            sync_maintenance(core, host, payload);
            core.touch();
        }
        "agent_update" | "workspace_update" | "project.update" | "script_status_update"
        | "agent_status" | "agent_permission_request" | "agent_permission_resolved" => directory_event(core, host, message),
        _ => {}
    }
}

/// `server_info` is a full snapshot: an absent `daemonUpdate` ends the pause.
pub(crate) fn sync_maintenance(core: &Rc<Core>, host: &Rc<Host>, info: &Value) {
    if info.as_object().is_none_or(|info| info.is_empty()) {
        return;
    }
    let update = &info["daemonUpdate"];
    let next = (update["active"] == true)
        .then(|| update["operationId"].as_str().unwrap_or_default().to_owned())
        .filter(|id| !id.is_empty());
    let changed = {
        let mut dir = host.dir.borrow_mut();
        let changed = dir.maintenance != next;
        dir.maintenance = next;
        changed
    };
    if changed {
        core.touch();
    }
}

fn directory_event(core: &Rc<Core>, host: &Rc<Host>, message: &Value) {
    let message_type = message["type"].as_str().unwrap_or_default();
    let kind = kind_of(message_type);
    let project_joint = affects_project(message);
    {
        let mut dir = host.dir.borrow_mut();
        // Workspace removals can also update project placement: keep that joint update behind
        // both baselines without blocking unrelated agent events.
        let has_earlier = dir.queued.iter().any(|queued| {
            let queued_kind = kind_of(queued["type"].as_str().unwrap_or_default());
            queued_kind == kind
                || (kind == "project" && affects_project(queued))
                || (project_joint && queued_kind == "project")
        });
        if has_earlier
            || dir.hydrating.contains(kind)
            || (project_joint && dir.hydrating.contains("project"))
        {
            dir.queued.push(message.clone());
            return;
        }
    }
    let payload = &message["payload"];
    {
        let mut dir = host.dir.borrow_mut();
        let generation = payload["generation"].as_str();
        let seq = payload["seq"].as_f64().map(|s| s as i64).unwrap_or(0);
        if let (Some(generation), Some(known)) = (generation, dir.generations.get(kind))
            && generation != known.as_str() {
                drop(dir);
                schedule_resync(core, host);
                return;
            }
        if seq > 0 && seq <= dir.sequences.get(kind).copied().unwrap_or(0) {
            return;
        }
        if let Some(generation) = generation {
            dir.generations.insert(kind, generation.to_owned());
        }
        if seq > 0 {
            dir.sequences.insert(kind, seq);
        }
    }
    let changed = {
        let mut dir = host.dir.borrow_mut();
        let dir = &mut *dir;
        match message_type {
            "agent_update" => {
                if payload["kind"] == "remove" {
                    remove(&mut dir.agents, |a| a["id"] == payload["agentId"])
                } else {
                    let mut row = agent_row(payload);
                    if row["project"].is_null()
                        && let Some(old) = dir.agents.iter().find(|a| a["id"] == row["id"]) {
                            row["project"] = old["project"].clone();
                        }
                    upsert(&mut dir.agents, row, "id")
                }
            }
            "agent_status" => {
                let info = &payload["info"];
                let existing = dir.agents.iter().find(|a| a["id"] == info["id"]).cloned();
                match existing {
                    Some(mut merged) => {
                        if let (Some(target), Some(source)) = (merged.as_object_mut(), info.as_object()) {
                            for (key, value) in source {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                        upsert(&mut dir.agents, merged, "id")
                    }
                    None => false,
                }
            }
            "workspace_update" => {
                if payload["kind"] == "remove" {
                    let mut changed = remove(&mut dir.workspaces, |w| w["id"] == payload["id"]);
                    if payload["emptyProject"].is_object() {
                        changed |= upsert(&mut dir.projects, payload["emptyProject"].clone(), "projectId");
                    }
                    if !payload["removedProjectId"].is_null() {
                        changed |= remove(&mut dir.projects, |p| p["projectId"] == payload["removedProjectId"]);
                    }
                    changed
                } else {
                    upsert(&mut dir.workspaces, payload["workspace"].clone(), "id")
                }
            }
            "project.update" => {
                if payload["kind"] == "remove" {
                    remove(&mut dir.projects, |p| p["projectId"] == payload["projectId"])
                } else {
                    upsert(&mut dir.projects, payload["project"].clone(), "projectId")
                }
            }
            // The list rows carry `pendingPermissions` (sidebar status, attention, badges); the
            // daemon announces requests and resolutions separately from agent snapshots.
            "agent_permission_request" => match dir.agents.iter_mut().find(|a| a["id"] == payload["agentId"]) {
                Some(agent) => add_permission(agent, &payload["request"]),
                None => false,
            },
            "agent_permission_resolved" => match dir.agents.iter_mut().find(|a| a["id"] == payload["agentId"]) {
                Some(agent) => remove_permission(agent, &payload["requestId"]),
                None => false,
            },
            "script_status_update" => {
                let mut changed = false;
                for workspace in dir.workspaces.iter_mut() {
                    if workspace["id"] == payload["workspaceId"] && workspace["scripts"] != payload["scripts"] {
                        workspace["scripts"] = payload["scripts"].clone();
                        changed = true;
                    }
                }
                changed
            }
            _ => false,
        }
    };
    // An update that leaves every row as it was saves nothing and rebuilds nothing.
    if !changed {
        return;
    }
    host.dir.borrow_mut().cached_at = Some(ids::now_rfc3339());
    store::schedule_save(core);
    core.touch();
}

fn remove(rows: &mut Vec<Value>, test: impl Fn(&Value) -> bool) -> bool {
    let before = rows.len();
    rows.retain(|row| !test(row));
    rows.len() != before
}

/// Adds a live permission request to an agent row; false when it is already pending.
fn add_permission(agent: &mut Value, request: &Value) -> bool {
    if !request.is_object() {
        return false;
    }
    if !agent["pendingPermissions"].is_array() {
        agent["pendingPermissions"] = json!([]);
    }
    let Some(pending) = agent["pendingPermissions"].as_array_mut() else { return false };
    if !request["id"].is_null() && pending.iter().any(|p| p["id"] == request["id"]) {
        return false;
    }
    pending.push(request.clone());
    true
}

/// Drops a resolved request from an agent row; false when it was not pending.
fn remove_permission(agent: &mut Value, request_id: &Value) -> bool {
    let Some(pending) = agent["pendingPermissions"].as_array_mut() else { return false };
    let before = pending.len();
    pending.retain(|p| p["id"] != *request_id);
    pending.len() != before
}

/// Inserts or replaces `row`; false when an equal row is already there.
fn upsert(rows: &mut Vec<Value>, row: Value, key: &str) -> bool {
    if !row.is_object() || row[key].is_null() {
        return false;
    }
    match rows.iter().position(|item| item[key] == row[key]) {
        None => rows.push(row),
        Some(index) if rows[index] == row => return false,
        Some(index) => rows[index] = row,
    }
    true
}

/// Flutter `_scheduleResync`: a running refresh hydrates again after its current attempt;
/// otherwise one refresh starts 150 ms after the first request, and later requests join it.
pub(crate) fn schedule_resync(core: &Rc<Core>, host: &Rc<Host>) {
    let mut dir = host.dir.borrow_mut();
    if dir.refreshing {
        dir.resync_for = Some(host.client_epoch.get());
        return;
    }
    if dir.resync.is_some() {
        return;
    }
    let core = core.clone();
    let task_host = host.clone();
    let handle = tokio::task::spawn_local(async move {
        tokio::time::sleep(RESYNC_DELAY).await;
        task_host.dir.borrow_mut().resync = None;
        refresh_host(&core, &task_host);
    });
    dir.resync = Some(handle.abort_handle());
}

pub(crate) fn cancel_resync(host: &Rc<Host>) {
    if let Some(timer) = host.dir.borrow_mut().resync.take() {
        timer.abort();
    }
}

/// A directory feed restored after reconnecting has no replay history: a feed restored while
/// its own refresh still runs needs another pass (Flutter `_observationLifecycle`).
pub(crate) fn observation_restored(core: &Rc<Core>, host: &Rc<Host>, epoch: u64) {
    if core.contains(host) && host.dir.borrow().epoch == epoch {
        schedule_resync(core, host);
    }
}

pub(crate) fn observation_failed(core: &Rc<Core>, host: &Rc<Host>, error: CoreError) {
    if core.contains(host) {
        *host.error.borrow_mut() = Some(error);
        core.touch();
    }
}

/// Flutter `_markChildTabOpen` for saved tabs: an open child session tab carries this client's
/// `paseo.open-agent-tab.<clientId>` label. Opening never clears attention.
fn mark_child_tabs(core: &Rc<Core>, host: &Rc<Host>, conn: &Rc<Conn>) {
    let server_id = host.server_id();
    let label = format!("paseo.open-agent-tab.{}", core.client_id.borrow());
    let targets: BTreeSet<String> = {
        let store = core.store.borrow();
        store
            .layouts
            .values()
            .filter_map(|layout| layout["tabs"].as_array())
            .flatten()
            .filter(|tab| tab["kind"] == "agent" && tab["serverId"] == server_id.as_str())
            .filter_map(|tab| tab["id"].as_str().map(str::to_owned))
            .collect()
    };
    for agent_id in targets {
        let needs_label = {
            let dir = host.dir.borrow();
            dir.agents.iter().find(|a| a["id"] == agent_id.as_str()).is_some_and(|agent| {
                let labels = &agent["labels"];
                (!agent["parentAgentId"].is_null() || !labels["paseo.parent-agent-id"].is_null())
                    && labels[&label] != "true"
            })
        };
        if !needs_label {
            continue;
        }
        let core = core.clone();
        let host = host.clone();
        let conn = conn.clone();
        let label = label.clone();
        tokio::task::spawn_local(async move {
            let result = conn
                .request(
                    json!({"type":"update_agent_request","agentId":agent_id,"labels":{label:"true"}}),
                    "update_agent_response",
                    Some(requests::REQUEST_TIMEOUT),
                    None,
                    None,
                )
                .await
                .and_then(|payload| require_success(payload, "update_agent_request"));
            if let Err(error) = result {
                *host.error.borrow_mut() = Some(error);
                core.touch();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upserts_report_only_real_changes() {
        let mut rows = vec![json!({"id":"a","v":1})];
        assert!(!upsert(&mut rows, json!({"id":"a","v":1}), "id"));
        assert!(upsert(&mut rows, json!({"id":"a","v":2}), "id"));
        assert!(upsert(&mut rows, json!({"id":"b"}), "id"));
        assert!(!upsert(&mut rows, json!({"v":3}), "id"));
        assert!(remove(&mut rows, |r| r["id"] == "b"));
        assert_eq!(rows, vec![json!({"id":"a","v":2})]);
    }

    #[test]
    fn agent_rows_carry_project_and_sync_sequence() {
        let row = agent_row(&json!({"agent":{"id":"a"},"project":{"p":1},"syncSeq":4}));
        assert_eq!(row, json!({"id":"a","project":{"p":1},"syncSeq":4}));
    }

    #[test]
    fn live_permission_events_keep_the_pending_list_current() {
        let mut agent = json!({"id":"a","status":"running"});
        let request = json!({"id":"r1","kind":"question"});
        assert!(add_permission(&mut agent, &request));
        assert!(!add_permission(&mut agent, &request), "a repeated announcement is not a second request");
        assert!(add_permission(&mut agent, &json!({"id":"r2"})));
        assert!(!add_permission(&mut agent, &Value::Null));
        assert!(remove_permission(&mut agent, &json!("r1")));
        assert!(!remove_permission(&mut agent, &json!("r1")));
        assert_eq!(agent["pendingPermissions"], json!([{"id":"r2"}]));
    }
}
