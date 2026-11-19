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
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};

use super::{Conn, Core, Host, hosts, requests, store};
use crate::{
    CoreError, DirectoryAgents, DirectorySnapshot, ids, protocol::response_error, storage::Replica,
};

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
    projects: Arc<Vec<Arc<Value>>>,
    workspaces: Arc<Vec<Arc<Value>>>,
    agents: DirectoryAgents,
    agent_ids: HashMap<Value, AgentSlots>,
    cached_at: Option<String>,
    /// Identity of the rows and `cached_at` above, renewed by every change of them; 0 is the
    /// empty directory. Published as `directoryRevision` and used to persist only changed
    /// replicas.
    revision: u64,
    snapshot: Option<Arc<DirectorySnapshot>>,
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

enum AgentSlots {
    One(usize),
    // Replicas retain duplicate IDs: updating uses the first match; removing clears all.
    Several(Vec<usize>),
}

impl AgentSlots {
    fn first(&self) -> usize {
        match self {
            Self::One(slot) => *slot,
            Self::Several(slots) => slots[0],
        }
    }

    fn push(&mut self, slot: usize) {
        match self {
            Self::One(first) => *self = Self::Several(vec![*first, slot]),
            Self::Several(slots) => slots.push(slot),
        }
    }
}

/// Process-wide, so two equal revisions are the same rows even across replaced hosts.
pub(super) fn next_revision() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Directory {
    pub fn apply_replica(&mut self, replica: Replica) {
        self.projects = Arc::new(replica.projects.into_iter().map(Arc::new).collect());
        self.workspaces = Arc::new(replica.workspaces.into_iter().map(Arc::new).collect());
        self.replace_agents(replica.agents.into_iter().map(Arc::new).collect());
        self.cached_at = replica.cached_at;
        self.changed();
    }

    pub fn projects(&self) -> &[Arc<Value>] {
        &self.projects
    }

    pub fn workspaces(&self) -> &[Arc<Value>] {
        &self.workspaces
    }

    pub fn agents(&self) -> impl Iterator<Item = &Arc<Value>> {
        self.agents.iter()
    }

    fn replace_agents(&mut self, agents: DirectoryAgents) {
        self.agent_ids.clear();
        self.agent_ids.reserve(agents.len());
        for (slot, row) in agents.iter().enumerate() {
            self.agent_ids
                .entry(row["id"].clone())
                .and_modify(|slots| slots.push(slot))
                .or_insert(AgentSlots::One(slot));
        }
        self.agents = agents;
    }

    fn agent(&self, id: &Value) -> Option<&Arc<Value>> {
        Some(self.agents.row_at(self.agent_ids.get(id)?.first()))
    }

    fn upsert_agent(&mut self, mut row: Value) -> bool {
        if !row.is_object() || row["id"].is_null() {
            return false;
        }
        if let Some(slots) = self.agent_ids.get(&row["id"]) {
            let slot = slots.first();
            let old = self.agents.row_at(slot);
            if row["project"].is_null() {
                row["project"] = old["project"].clone();
            }
            if **old == row {
                return false;
            }
            self.agents.replace_at(slot, Arc::new(row));
        } else {
            let id = row["id"].clone();
            let slot = self.agents.push(Arc::new(row));
            self.agent_ids.insert(id, AgentSlots::One(slot));
        }
        true
    }

    fn update_agent(&mut self, id: &Value, update: impl FnOnce(&mut Arc<Value>) -> bool) -> bool {
        let Some(slots) = self.agent_ids.get(id) else {
            return false;
        };
        let slot = slots.first();
        let mut row = self.agents.row_at(slot).clone();
        if !update(&mut row) {
            return false;
        }
        self.agents.replace_at(slot, row);
        true
    }

    fn remove_agent(&mut self, id: &Value) -> bool {
        match self.agent_ids.remove(id) {
            Some(AgentSlots::One(slot)) => self.agents.remove_at(slot),
            Some(AgentSlots::Several(slots)) => {
                for slot in slots {
                    self.agents.remove_at(slot);
                }
            }
            None => return false,
        }
        // Keep removals from shifting later agents, but reclaim holes after enough churn.
        if self.agents.slots() > 128.max(self.agents.len() * 2) {
            let compacted = self.agents.iter().cloned().collect();
            self.replace_agents(compacted);
        }
        true
    }

    pub fn cached_at(&self) -> Option<&str> {
        self.cached_at.as_deref()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn snapshot(&mut self) -> Arc<DirectorySnapshot> {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.revision != self.revision)
        {
            self.snapshot = Some(Arc::new(DirectorySnapshot {
                revision: self.revision,
                cached_at: self.cached_at.clone(),
                projects: self.projects.clone(),
                workspaces: self.workspaces.clone(),
                agents: self.agents.clone(),
            }));
        }
        self.snapshot
            .as_ref()
            .expect("snapshot initialized")
            .clone()
    }

    pub fn has_rows(&self) -> bool {
        !self.projects.is_empty() || !self.workspaces.is_empty() || !self.agents.is_empty()
    }

    fn changed(&mut self) {
        self.revision = next_revision();
    }

    /// Rows changed, or a baseline completed, now.
    fn mark_cached(&mut self) {
        self.cached_at = Some(ids::now_rfc3339());
        self.changed();
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
        && core.contains(host)
    {
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
    let expected = host.config.borrow()["expectedServerId"]
        .as_str()
        .map(str::to_owned);
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
            dir.projects = Arc::new(rows.into_iter().map(Arc::new).collect());
            dir.changed();
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
            let mut dir = host.dir.borrow_mut();
            dir.workspaces = Arc::new(rows.into_iter().map(Arc::new).collect());
            dir.changed();
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
            let mut dir = host.dir.borrow_mut();
            dir.replace_agents(rows.iter().map(|row| Arc::new(agent_row(row))).collect());
            dir.changed();
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
            Ok(()) => dir.mark_cached(),
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
            conn.request(
                message,
                response,
                Some(requests::REQUEST_TIMEOUT),
                None,
                None,
            )
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
            if payload["serverId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
            {
                *host.server_info.borrow_mut() = payload.clone();
            }
            sync_maintenance(core, host, payload);
            core.touch();
        }
        "agent_update"
        | "workspace_update"
        | "project.update"
        | "script_status_update"
        | "agent_status"
        | "agent_permission_request"
        | "agent_permission_resolved" => directory_event(core, host, message),
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
        .then(|| {
            update["operationId"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
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
            && generation != known.as_str()
        {
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
                    dir.remove_agent(&payload["agentId"])
                } else {
                    dir.upsert_agent(agent_row(payload))
                }
            }
            "agent_status" => {
                let info = &payload["info"];
                info.as_object().is_some_and(|source| {
                    dir.update_agent(&info["id"], |agent| {
                        if !agent.is_object()
                            || source
                                .iter()
                                .all(|(key, value)| agent.get(key) == Some(value))
                        {
                            return false;
                        }
                        let target = Arc::make_mut(agent)
                            .as_object_mut()
                            .expect("directory agent object");
                        for (key, value) in source {
                            if target.get(key) != Some(value) {
                                target.insert(key.clone(), value.clone());
                            }
                        }
                        true
                    })
                })
            }
            "workspace_update" => {
                if payload["kind"] == "remove" {
                    let mut changed = remove(&mut dir.workspaces, |w| w["id"] == payload["id"]);
                    if payload["emptyProject"].is_object() {
                        changed |= upsert(
                            &mut dir.projects,
                            payload["emptyProject"].clone(),
                            "projectId",
                        );
                    }
                    if !payload["removedProjectId"].is_null() {
                        changed |= remove(&mut dir.projects, |p| {
                            p["projectId"] == payload["removedProjectId"]
                        });
                    }
                    changed
                } else {
                    upsert(&mut dir.workspaces, payload["workspace"].clone(), "id")
                }
            }
            "project.update" => {
                if payload["kind"] == "remove" {
                    remove(&mut dir.projects, |p| {
                        p["projectId"] == payload["projectId"]
                    })
                } else {
                    upsert(&mut dir.projects, payload["project"].clone(), "projectId")
                }
            }
            // The list rows carry `pendingPermissions` (sidebar status, attention, badges); the
            // daemon announces requests and resolutions separately from agent snapshots.
            "agent_permission_request" => dir.update_agent(&payload["agentId"], |agent| {
                add_permission(agent, &payload["request"])
            }),
            "agent_permission_resolved" => dir.update_agent(&payload["agentId"], |agent| {
                remove_permission(agent, &payload["requestId"])
            }),
            "script_status_update" => {
                let id = &payload["workspaceId"];
                let scripts = &payload["scripts"];
                let differs = |workspace: &Arc<Value>| {
                    workspace["id"] == *id && workspace["scripts"] != *scripts
                };
                if dir.workspaces.iter().any(differs) {
                    for workspace in Arc::make_mut(&mut dir.workspaces)
                        .iter_mut()
                        .filter(|workspace| differs(workspace))
                    {
                        Arc::make_mut(workspace)["scripts"] = scripts.clone();
                    }
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    };
    // An update that leaves every row as it was saves nothing and rebuilds nothing; a change
    // renews only this host's revision, so only its replica is serialized again.
    if !changed {
        return;
    }
    host.dir.borrow_mut().mark_cached();
    store::schedule_save(core);
    core.touch();
}

fn remove(rows: &mut Arc<Vec<Arc<Value>>>, test: impl Fn(&Value) -> bool) -> bool {
    if !rows.iter().any(|row| test(row)) {
        return false;
    }
    Arc::make_mut(rows).retain(|row| !test(row));
    true
}

/// Adds a live permission request to an agent row; false when it is already pending.
fn add_permission(agent: &mut Arc<Value>, request: &Value) -> bool {
    if !request.is_object()
        || (!request["id"].is_null()
            && agent["pendingPermissions"]
                .as_array()
                .is_some_and(|pending| pending.iter().any(|entry| entry["id"] == request["id"])))
    {
        return false;
    }
    let agent = Arc::make_mut(agent);
    if !agent["pendingPermissions"].is_array() {
        agent["pendingPermissions"] = json!([]);
    }
    agent["pendingPermissions"]
        .as_array_mut()
        .expect("initialized permissions")
        .push(request.clone());
    true
}

/// Drops a resolved request from an agent row; false when it was not pending.
fn remove_permission(agent: &mut Arc<Value>, request_id: &Value) -> bool {
    if !agent["pendingPermissions"]
        .as_array()
        .is_some_and(|pending| pending.iter().any(|entry| entry["id"] == *request_id))
    {
        return false;
    }
    Arc::make_mut(agent)["pendingPermissions"]
        .as_array_mut()
        .expect("existing permissions")
        .retain(|entry| entry["id"] != *request_id);
    true
}

/// Inserts or replaces `row`; false when an equal row is already there.
fn upsert(rows: &mut Arc<Vec<Arc<Value>>>, row: Value, key: &str) -> bool {
    if !row.is_object() || row[key].is_null() {
        return false;
    }
    match rows.iter().position(|item| item[key] == row[key]) {
        None => Arc::make_mut(rows).push(Arc::new(row)),
        Some(index) if *rows[index] == row => return false,
        Some(index) => Arc::make_mut(rows)[index] = Arc::new(row),
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
        let agent_id = Value::String(agent_id);
        let needs_label = {
            let dir = host.dir.borrow();
            dir.agent(&agent_id).is_some_and(|agent| {
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
        let mut rows = Arc::new(vec![Arc::new(json!({"id":"a","v":1}))]);
        assert!(!upsert(&mut rows, json!({"id":"a","v":1}), "id"));
        assert!(upsert(&mut rows, json!({"id":"a","v":2}), "id"));
        assert!(upsert(&mut rows, json!({"id":"b"}), "id"));
        assert!(!upsert(&mut rows, json!({"v":3}), "id"));
        assert!(remove(&mut rows, |r| r["id"] == "b"));
        assert_eq!(*rows[0], json!({"id":"a","v":2}));
        let mut directory = Directory::default();
        directory.replace_agents(rows.iter().cloned().collect());
        let mut other = Directory::default();
        let before = directory.snapshot();
        let other_before = other.snapshot();
        assert!(Arc::ptr_eq(&before, &directory.snapshot()));
        assert!(directory.upsert_agent(json!({"id":"a","v":3})));
        directory.changed();
        let after = directory.snapshot();
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(before.agents.iter().next().unwrap()["v"], 2);
        assert_eq!(after.agents.iter().next().unwrap()["v"], 3);
        assert!(Arc::ptr_eq(&other_before, &other.snapshot()));
    }

    #[test]
    fn large_agent_update_shares_unmodified_pages_and_directory_lists() {
        let mut directory = Directory::default();
        directory.replace_agents((0..10_000).map(|id| Arc::new(json!({"id": id}))).collect());
        let before = directory.snapshot();
        let target = json!(9989);
        assert!(directory.update_agent(&target, |row| {
            add_permission(row, &json!({"id":"pending"}))
        }));
        directory.changed();
        let after = directory.snapshot();

        assert!(Arc::ptr_eq(&before.projects, &after.projects));
        assert!(Arc::ptr_eq(&before.workspaces, &after.workspaces));
        assert_eq!(Arc::strong_count(directory.agent(&json!(0)).unwrap()), 1);
        assert_eq!(Arc::strong_count(directory.agent(&json!(9990)).unwrap()), 2);
        let copied_row_refs = before
            .agents
            .iter()
            .filter(|row| Arc::strong_count(row) > 1);
        assert_eq!(copied_row_refs.count(), 15); // Only the updated row's unchanged page neighbors.
        let mut replacements = 0;
        assert!(
            after
                .agents
                .for_each_replacement(&before.agents, |old, new| {
                    replacements += 1;
                    assert_eq!(old["id"], target);
                    assert!(old["pendingPermissions"].is_null());
                    assert_eq!(new["pendingPermissions"][0]["id"], "pending");
                })
        );
        assert_eq!(replacements, 1);
        assert!(
            after
                .agents
                .iter()
                .enumerate()
                .all(|(id, row)| row["id"] == id)
        );
    }

    #[test]
    fn indexed_agent_removal_preserves_order_and_reinsertion_appends() {
        let mut directory = Directory::default();
        directory.replace_agents(
            [
                json!({"id":"b", "project":{"name":"existing"}}),
                json!({"id":"a"}),
                json!({"id":"b", "title":"duplicate"}),
                json!({"id":"c"}),
            ]
            .into_iter()
            .map(Arc::new)
            .collect(),
        );
        assert!(directory.upsert_agent(json!({"id":"b", "title":"changed"})));
        directory.changed();
        let before = directory.snapshot();
        assert_eq!(
            before.agents.iter().next().unwrap()["project"]["name"],
            "existing"
        );
        assert_eq!(before.agents.iter().nth(2).unwrap()["title"], "duplicate");

        assert!(directory.remove_agent(&json!("b")));
        assert!(!directory.remove_agent(&json!("b")));
        assert!(directory.upsert_agent(json!({"id":"b"})));
        directory.changed();
        let after = directory.snapshot();
        let ids: Vec<_> = after
            .agents
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["a", "c", "b"]);
        assert_eq!(before.agents.len(), 4);
    }

    #[test]
    fn agent_compaction_reindexes_live_rows_without_changing_order() {
        let mut directory = Directory::default();
        directory.replace_agents((0..129).map(|id| Arc::new(json!({"id": id}))).collect());
        for id in 0..65 {
            assert!(directory.remove_agent(&json!(id)));
        }
        assert_eq!(directory.agents.slots(), 64);
        assert!(directory.upsert_agent(json!({"id": 70, "title":"after compaction"})));
        assert_eq!(
            directory.agent(&json!(70)).unwrap()["title"],
            "after compaction"
        );
        assert!(
            directory
                .agents()
                .zip(65..129)
                .all(|(row, id)| row["id"] == id)
        );
    }

    #[test]
    fn agent_rows_carry_project_and_sync_sequence() {
        let row = agent_row(&json!({"agent":{"id":"a"},"project":{"p":1},"syncSeq":4}));
        assert_eq!(row, json!({"id":"a","project":{"p":1},"syncSeq":4}));
    }

    #[test]
    fn live_permission_events_keep_the_pending_list_current() {
        let mut agent = Arc::new(json!({"id":"a","status":"running"}));
        let request = json!({"id":"r1","kind":"question"});
        assert!(add_permission(&mut agent, &request));
        assert!(
            !add_permission(&mut agent, &request),
            "a repeated announcement is not a second request"
        );
        assert!(add_permission(&mut agent, &json!({"id":"r2"})));
        assert!(!add_permission(&mut agent, &Value::Null));
        assert!(remove_permission(&mut agent, &json!("r1")));
        assert!(!remove_permission(&mut agent, &json!("r1")));
        assert_eq!(agent["pendingPermissions"], json!([{"id":"r2"}]));
    }
}
