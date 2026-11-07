//! One viewed conversation (port of `chat_controller.dart` ChatController):
//! subscription membership, authoritative catch-up (viewed-timeline-sync.ts),
//! response reconciliation (session-stream-reducers.ts processTimelineResponse)
//! and live stream sequencing (processAgentStreamEvent).
//!
//! The Dart controller is async; here every await point is an explicit state
//! (`Pending` request routes, `LoadOp` queue, retry `Timer`s). Request
//! responses arrive through `TimelineStore`, which owns the routing table.

use super::fold::TimelineFold;
use super::json::*;
use crate::common::UiEffect;
use makepad_widgets::{Cx, Timer};
use paseo_gui_core::{CoreError, CoreHandle, RequestId};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// timeline-fetch-policy.ts TIMELINE_FETCH_PAGE_SIZE: projected items per page.
pub const TIMELINE_FETCH_PAGE_SIZE: i64 = 40;

/// viewed-timeline-sync.ts getNextRetryDelayMs: 1s doubling to a 30s ceiling.
pub fn next_retry_delay(previous: Option<f64>) -> f64 {
    match previous {
        None => 1.0,
        Some(previous) => (previous * 2.0).min(30.0),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub host_id: String,
    /// The real agent (the parent for a provider subagent pane).
    pub agent_id: String,
    pub subagent_id: Option<String>,
}

impl SessionKey {
    pub fn is_subagent(&self) -> bool {
        self.subagent_id.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Ready,
    Pending,
    Error,
    Retrying,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    Ready,
    Preparing,
    Failed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cursor {
    pub epoch: String,
    pub seq: i64,
}

impl Cursor {
    fn to_json(&self) -> Value {
        json!({"epoch": self.epoch, "seq": self.seq})
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatchUp {
    Running,
    Complete,
    Error,
}

/// agent-snapshots.ts derivePendingPermissionKey, scoped to one agent.
pub fn pending_permission_key(request: &Value) -> String {
    let metadata = get(request, "metadata");
    for candidate in [
        get(request, "id"),
        get(metadata, "id"),
        get(request, "name"),
        get(request, "title"),
    ] {
        if let Some(s) = non_empty_str(candidate) {
            return s.to_owned();
        }
    }
    let input = if !get(request, "input").is_null() {
        get(request, "input").clone()
    } else if !get(request, "metadata").is_null() {
        get(request, "metadata").clone()
    } else {
        json!({})
    };
    let kind = match get(request, "kind") {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    };
    format!("{kind}:{}", serde_json::to_string(&input).unwrap_or_default())
}

/// What a session asked the host for; the store routes the response back.
#[derive(Clone, Debug)]
pub enum Pending {
    Membership { generation: u64 },
    Fetch { direction: &'static str, force_replace: bool, generation: u64 },
    Subagents,
    /// `ChatController.action` callers; the outcome lands in `completions`.
    Action(ActionKind),
}

/// Mutations whose result a view reacts to.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionKind {
    Permission { key: String },
    Rename,
    Refresh,
    Unarchive,
    Archive,
    Delete,
    Detach,
    Cancel,
    Fork { target: ForkTarget },
    Rewind { mode: String, text: String },
    ListPrompts { request: u64 },
    OutlineMenu,
    Search { token: u64 },
    ClearAttention,
    ArchiveSubagent { key: String },
    DetachSubagent { host_id: String, agent_id: String },
    ArchiveFinished { key: Option<String> },
    LookupFile { token: String, target: Value },
    ProbeFolder { generation: u64, target: Value },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ForkTarget {
    Tab,
    Workspace,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub kind: ActionKind,
    pub result: Result<Value, CoreError>,
}

#[derive(Clone, Debug)]
enum LoadOp {
    CatchUp { generation: u64, gap: bool },
    Tail { force_replace: bool },
    Older { reveal: Option<(i64, u64)> },
}

#[derive(Clone, Debug)]
struct ActiveLoad {
    op: LoadOp,
    request: Option<RequestId>,
    /// CatchUp: the first resume is bounded (install the latest tail instead
    /// of replaying every page).
    fallback_to_latest_tail: bool,
    forced_tail: bool,
    /// Older/reveal: the start cursor before the page, to detect no progress.
    previous_start: Option<i64>,
}

/// Outcome of `reveal` (chat find, outline jumps): token → ok/failure.
#[derive(Clone, Debug)]
pub struct RevealDone {
    pub token: u64,
    pub ok: bool,
}

/// Where a host response goes back to.
#[derive(Clone, Debug)]
pub struct RequestRoute {
    pub key: SessionKey,
    pub pending: Pending,
    pub request_type: String,
}

/// Everything a session needs to talk to the host for one call.
pub struct Io<'a> {
    pub cx: &'a mut Cx,
    pub core: &'a CoreHandle,
    pub server_info: &'a Value,
    pub requests: &'a mut HashMap<RequestId, RequestRoute>,
    pub effects: &'a mut Vec<UiEffect>,
    pub create_handoff: bool,
}

impl Io<'_> {
    pub fn supports(&self, feature_name: &str) -> bool {
        feature(self.server_info, feature_name)
    }
}

pub struct ChatSession {
    pub key: SessionKey,
    rows: Vec<Value>,
    fold: TimelineFold,
    projection_pending: bool,
    /// The projection (fold entries); a new Rc only when an entry changed.
    pub timeline: Rc<Vec<Rc<Value>>>,
    pub agent: Value,
    /// Pending permission requests by [pending_permission_key], in arrival order.
    pub permissions: Vec<(String, Value)>,
    pub subagents: Vec<Value>,
    pub turns: HashMap<String, Value>,
    pub epoch: Option<String>,
    pub start_cursor: Option<Cursor>,
    pub end_cursor: Option<Cursor>,
    pub has_older: bool,
    pub loading: bool,
    pub connected: bool,
    /// Authoritative history has been applied (session-store status "synced").
    pub synced: bool,
    /// The catch-up or subscription failure the sync-error callout reports.
    pub error: Option<String>,
    pub notice: Option<String>,
    /// The latest parent page came from the host's saved checkpoint.
    pub history_saved: bool,
    preparation_retry: bool,
    live_refresh_done: bool,
    live_observed: bool,
    refresh_pending: bool,
    child_resync_epoch: Option<String>,
    child_refresh: Value,
    generation: u64,
    buffer: Vec<Value>,
    queue: VecDeque<LoadOp>,
    active: Option<ActiveLoad>,

    // Membership (viewed-timeline-sync.ts reconcileMembership).
    subscribed: bool,
    membership_needs_retry: bool,
    membership_generation: u64,
    /// The core observation that owns this chat's timeline membership.
    observation: Option<RequestId>,
    membership_retry: Timer,
    membership_delay: Option<f64>,

    // Catch-up.
    catch_up: Option<CatchUp>,
    catch_up_generation: u64,
    gap_pending: bool,
    catch_up_retry: Timer,
    catch_up_delay: Option<f64>,

    pending: bool,
    manual_retry: bool,
    app_visible: bool,
    disposed: bool,

    pub completions: Vec<Completion>,
    pub reveals: Vec<RevealDone>,
    /// Failed older-history pages (each owns a transient toast).
    pub older_failures: u32,
    /// Bumps on every observable change; views redraw when it moves.
    pub version: u64,
    /// The first catch-up failed before any history landed.
    pub initial_sync_failed: bool,
    pub hydrated: bool,
    pub ended_create_handoff: bool,
}

impl ChatSession {
    pub fn new(key: SessionKey, connected: bool) -> Self {
        Self {
            key,
            rows: Vec::new(),
            fold: TimelineFold::default(),
            projection_pending: false,
            timeline: Rc::new(Vec::new()),
            agent: Value::Object(Default::default()),
            permissions: Vec::new(),
            subagents: Vec::new(),
            turns: HashMap::new(),
            epoch: None,
            start_cursor: None,
            end_cursor: None,
            has_older: false,
            loading: false,
            connected,
            synced: false,
            error: None,
            notice: None,
            history_saved: false,
            preparation_retry: false,
            live_refresh_done: false,
            live_observed: false,
            refresh_pending: false,
            child_resync_epoch: None,
            child_refresh: Value::Null,
            generation: 0,
            buffer: Vec::new(),
            queue: VecDeque::new(),
            active: None,
            subscribed: false,
            membership_needs_retry: false,
            membership_generation: 0,
            observation: None,
            membership_retry: Timer::empty(),
            membership_delay: None,
            catch_up: None,
            catch_up_generation: 0,
            gap_pending: false,
            catch_up_retry: Timer::empty(),
            catch_up_delay: None,
            pending: true,
            manual_retry: false,
            app_visible: true,
            disposed: false,
            completions: Vec::new(),
            reveals: Vec::new(),
            older_failures: 0,
            version: 1,
            initial_sync_failed: false,
            hydrated: false,
            ended_create_handoff: false,
        }
    }

    fn notify(&mut self) {
        self.version = self.version.wrapping_add(1);
        // use-agent-screen-state-machine.ts updateInitialSyncFailureMemory.
        if self.synced {
            self.hydrated = true;
            self.initial_sync_failed = false;
        } else if self.sync_status() == SyncStatus::Error && self.error.is_some() {
            self.initial_sync_failed = true;
        }
    }

    pub fn is_subagent(&self) -> bool {
        self.key.is_subagent()
    }

    pub fn agent_id(&self) -> &str {
        &self.key.agent_id
    }

    /// Execution readiness from the agent snapshot's `preparation`.
    pub fn execution(&self) -> ExecutionStatus {
        let preparation = get(&self.agent, "preparation");
        if !preparation.is_object() {
            return ExecutionStatus::Ready;
        }
        if get(preparation, "status") == "failed" {
            ExecutionStatus::Failed
        } else {
            ExecutionStatus::Preparing
        }
    }

    /// The provider's own failure message, shown verbatim.
    pub fn preparation_error(&self) -> Option<String> {
        (self.execution() == ExecutionStatus::Failed)
            .then(|| str_of(get(get(&self.agent, "preparation"), "error")).to_owned())
    }

    pub fn preparation_retrying(&self) -> bool {
        self.preparation_retry
    }

    pub fn running(&self, create_handoff: bool) -> bool {
        self.execution() == ExecutionStatus::Ready
            && (get(&self.agent, "status") == "running"
                || !get(&self.agent, "activeTurn").is_null()
                || (create_handoff
                    && (map(&self.agent).is_empty() || get(&self.agent, "status") == "idle")))
    }

    /// Sends, send-now and agent controls need a live snapshot.
    pub fn execution_authorized(&self, create_handoff: bool) -> bool {
        self.execution() == ExecutionStatus::Ready && (self.live_observed || create_handoff)
    }

    pub fn child_refreshing(&self) -> bool {
        self.is_subagent() && get(&self.child_refresh, "status") == "refreshing"
    }

    pub fn child_refresh_error(&self) -> Option<String> {
        (self.is_subagent() && get(&self.child_refresh, "status") == "failed")
            .then(|| str_of(get(&self.child_refresh, "error")).to_owned())
    }

    /// viewed-timeline-sync.ts getAgentTimelineStatus.
    pub fn sync_status(&self) -> SyncStatus {
        if self.manual_retry {
            SyncStatus::Retrying
        } else if self.error.is_some() {
            SyncStatus::Error
        } else if self.pending {
            SyncStatus::Pending
        } else {
            SyncStatus::Ready
        }
    }

    pub fn archived(&self) -> bool {
        !get(&self.agent, "archivedAt").is_null()
    }

    // ── host plumbing ───────────────────────────────────────────────────

    fn send(&mut self, io: &mut Io, pending: Pending, ty: &str, response: &str, mut fields: Value) -> RequestId {
        if let Some(obj) = fields.as_object_mut() {
            obj.insert("type".into(), Value::String(ty.to_owned()));
        }
        let id = io.core.request(&self.key.host_id, fields, response);
        io.requests.insert(id.clone(), RequestRoute { key: self.key.clone(), pending, request_type: ty.to_owned() });
        id
    }

    /// `ChatController.action`: the agent-scoped mutation; its result lands
    /// in [completions] (and its `notice` becomes a toast).
    pub fn action(&mut self, io: &mut Io, kind: ActionKind, ty: &str, response: &str, fields: Value) -> RequestId {
        let mut body = json!({"agentId": self.key.agent_id});
        if let (Some(target), Some(source)) = (body.as_object_mut(), fields.as_object()) {
            for (k, v) in source {
                target.insert(k.clone(), v.clone());
            }
        }
        self.send(io, Pending::Action(kind), ty, response, body)
    }

    /// Requests that are not scoped to this agent (subagent archive/detach,
    /// file lookups); the outcome still lands in [completions].
    pub fn host_request(&mut self, io: &mut Io, kind: ActionKind, ty: &str, response: &str, fields: Value) -> RequestId {
        self.send(io, Pending::Action(kind), ty, response, fields)
    }

    /// [host_request] sent to another host (a managed child that lives on a
    /// different host); the outcome still lands in [completions] here.
    pub fn request_on_host(&mut self, io: &mut Io, host_id: &str, kind: ActionKind, ty: &str, response: &str, mut fields: Value) -> RequestId {
        if let Some(obj) = fields.as_object_mut() {
            obj.insert("type".into(), Value::String(ty.to_owned()));
        }
        let id = io.core.request(host_id, fields, response);
        io.requests.insert(id.clone(), RequestRoute { key: self.key.clone(), pending: Pending::Action(kind), request_type: ty.to_owned() });
        id
    }

    /// [action] with an explicit deadline (permission responses: 15s,
    /// agent-panel.tsx respondToPermission timeoutMs).
    pub fn action_with_timeout(&mut self, io: &mut Io, kind: ActionKind, ty: &str, response: &str, fields: Value, timeout: std::time::Duration) -> RequestId {
        let mut body = json!({"agentId": self.key.agent_id, "type": ty});
        if let (Some(target), Some(source)) = (body.as_object_mut(), fields.as_object()) {
            for (k, v) in source {
                target.insert(k.clone(), v.clone());
            }
        }
        let id = io.core.request_with_timeout(&self.key.host_id, body, response, Some(timeout));
        io.requests.insert(id.clone(), RequestRoute { key: self.key.clone(), pending: Pending::Action(kind), request_type: ty.to_owned() });
        id
    }

    pub fn start(&mut self, io: &mut Io) {
        self.reconcile_membership(io);
    }

    // ── membership ──────────────────────────────────────────────────────

    /// viewed-timeline-sync.ts reconcileMembership through the core's owned
    /// observation (`CoreHandle::observe`, Flutter `PaseoClient.observe`):
    /// owned hosts get this chat's own subscription, legacy selective hosts
    /// the connection-wide member set, legacy broadcast hosts no RPC. The
    /// core restores it after every reconnect (`subscription.restored`).
    pub fn reconcile_membership(&mut self, io: &mut Io) {
        if self.disposed || !self.connected {
            return;
        }
        if self.observation.is_some() && !self.membership_needs_retry {
            return;
        }
        self.membership_generation += 1;
        let generation = self.membership_generation;
        self.membership_needs_retry = false;
        if let Some(previous) = self.observation.take() {
            io.core.cancel_request(&previous);
        }
        let id = io.core.observe(
            &self.key.host_id,
            json!({
                "type": "agent.timeline.set_subscription.request",
                "agentIds": [self.key.agent_id],
            }),
            "agent.timeline.set_subscription.response",
        );
        io.requests.insert(
            id.clone(),
            RequestRoute {
                key: self.key.clone(),
                pending: Pending::Membership { generation },
                request_type: "agent.timeline.set_subscription.request".to_owned(),
            },
        );
        self.observation = Some(id);
    }

    /// The observation's first snapshot (or its failure).
    pub fn on_membership_response(&mut self, io: &mut Io, request: &RequestId, generation: u64, result: Result<Value, CoreError>) {
        if self.disposed || generation != self.membership_generation || self.observation.as_ref() != Some(request) {
            if result.is_ok() {
                io.core.cancel_request(request);
            }
            return;
        }
        match result {
            Ok(_) => self.membership_acknowledged(io),
            Err(error) => {
                self.observation = None;
                if !self.connected {
                    return;
                }
                self.membership_failed(io, error.message);
            }
        }
    }

    /// `subscription.restored` / `subscription.error` for this chat's observation.
    pub fn on_subscription_event(&mut self, io: &mut Io, message: &Value) -> bool {
        let payload = get(message, "payload");
        let Some(observation) = self.observation.clone() else { return false };
        if get(payload, "observationId") != observation.as_str() || self.disposed {
            return false;
        }
        match str_of(get(message, "type")) {
            "subscription.restored" => {
                // The core re-established membership after a reconnect.
                self.membership_acknowledged(io);
                true
            }
            "subscription.error" if self.connected => {
                self.observation = None;
                io.core.cancel_request(&observation);
                let error = get(payload, "error");
                let message = non_empty_str(error)
                    .map(str::to_owned)
                    .unwrap_or_else(|| super::json::pretty(error));
                self.membership_failed(io, message);
                true
            }
            _ => false,
        }
    }

    fn membership_acknowledged(&mut self, io: &mut Io) {
        self.subscribed = true;
        io.cx.stop_timer(self.membership_retry);
        self.membership_retry = Timer::empty();
        self.membership_delay = None;
        self.start_catch_up(io, false, false);
        self.refresh_subagents(io);
    }

    fn membership_failed(&mut self, io: &mut Io, failure: String) {
        self.subscribed = false;
        self.membership_needs_retry = true;
        let delay = next_retry_delay(self.membership_delay);
        self.membership_delay = Some(delay);
        io.cx.stop_timer(self.membership_retry);
        self.membership_retry = io.cx.start_timeout(delay);
        self.set_error(failure);
    }

    /// Filter before constructing Io: unrelated widget timers need no host snapshot.
    pub fn owns_timer(&self, event: &makepad_widgets::Event) -> bool {
        (!self.membership_retry.is_empty() && self.membership_retry.is_event(event).is_some())
            || (!self.catch_up_retry.is_empty() && self.catch_up_retry.is_event(event).is_some())
    }

    pub fn on_timer(&mut self, io: &mut Io, event: &makepad_widgets::Event) -> bool {
        let mut hit = false;
        if self.membership_retry.is_event(event).is_some() {
            self.membership_retry = Timer::empty();
            hit = true;
            if !self.disposed && self.connected {
                self.reconcile_membership(io);
            }
        }
        if self.catch_up_retry.is_event(event).is_some() {
            self.catch_up_retry = Timer::empty();
            hit = true;
            if self.catch_up == Some(CatchUp::Error) {
                self.start_catch_up(io, false, false);
            }
        }
        hit
    }

    /// viewed-timeline-sync.ts setConnected. Disconnect keeps the obligation:
    /// the chat returns to pending and catches up once membership is restored.
    pub fn connection_changed(&mut self, io: &mut Io, ready: bool) {
        if ready == self.connected {
            return;
        }
        self.connected = ready;
        self.membership_generation += 1;
        io.cx.stop_timer(self.membership_retry);
        self.membership_retry = Timer::empty();
        self.membership_delay = None;
        if !ready {
            self.subscribed = false;
            self.cancel_catch_up(io);
            self.pending = true;
            self.error = None;
            self.manual_retry = false;
            self.preparation_retry = false;
            // Execution authority belongs to this connection: after a
            // reconnect only a fresh live snapshot restores it.
            self.live_observed = false;
            // In-flight loads died with the connection; their callers learn it.
            let mut ops: Vec<LoadOp> = self.queue.drain(..).collect();
            if let Some(active) = self.active.take() {
                ops.insert(0, active.op);
            }
            for op in ops {
                if let LoadOp::Older { reveal: Some((_, token)) } = op {
                    self.reveals.push(RevealDone { token, ok: false });
                }
            }
            self.loading = false;
            self.flush_buffer();
            self.project();
        } else if self.observation.is_none() {
            // Never acknowledged (or released after a failure): subscribe again.
            self.membership_needs_retry = false;
            self.reconcile_membership(io);
        }
        // Otherwise the core restores the observation and reports
        // subscription.restored, which starts the catch-up.
        self.notify();
    }

    /// viewed-timeline-sync.ts setActive: returning to the foreground catches
    /// the chat up again, showing it as pending until that settles.
    pub fn set_app_visible(&mut self, io: &mut Io, visible: bool) {
        if self.app_visible == visible {
            return;
        }
        self.app_visible = visible;
        if !visible || self.disposed {
            return;
        }
        self.pending = true;
        self.error = None;
        self.start_catch_up(io, false, true);
        self.notify();
    }

    /// viewed-timeline-sync.ts retryVisibleAgentTimeline: the sync-error
    /// callout's retry owns a `retrying` state until the attempt settles.
    pub fn retry_sync(&mut self, io: &mut Io) {
        if self.disposed || self.manual_retry {
            return;
        }
        let membership_retryable = self.membership_needs_retry && self.connected;
        if self.catch_up != Some(CatchUp::Error) && !membership_retryable {
            return;
        }
        self.manual_retry = true;
        self.notify();
        if self.catch_up == Some(CatchUp::Error) {
            io.cx.stop_timer(self.catch_up_retry);
            self.catch_up_retry = Timer::empty();
            self.start_catch_up(io, false, true);
            return;
        }
        io.cx.stop_timer(self.membership_retry);
        self.membership_retry = Timer::empty();
        self.membership_delay = None;
        self.reconcile_membership(io);
    }

    /// Retry a failed provider preparation: any parent timeline fetch
    /// restarts it on the host; saved history stays on screen.
    pub fn retry_preparation(&mut self, io: &mut Io) {
        if self.disposed
            || self.is_subagent()
            || self.preparation_retry
            || self.execution() != ExecutionStatus::Failed
        {
            return;
        }
        self.preparation_retry = true;
        self.notify();
        self.request_refresh(io);
    }

    pub fn retry_child_refresh(&mut self, io: &mut Io) {
        if self.disposed || self.preparation_retry || self.child_refresh_error().is_none() {
            return;
        }
        self.preparation_retry = true;
        self.notify();
        self.request_refresh(io);
    }

    fn refresh_subagents(&mut self, io: &mut Io) {
        if self.is_subagent() || self.disposed || !io.supports("providerSubagents") {
            return;
        }
        self.load_subagents(io);
    }

    /// subagents/select.ts: the provider subagents of this agent.
    pub fn load_subagents(&mut self, io: &mut Io) {
        let agent_id = self.key.agent_id.clone();
        self.send(
            io,
            Pending::Subagents,
            "agent.provider_subagents.list.request",
            "agent.provider_subagents.list.response",
            json!({"parentAgentId": agent_id}),
        );
    }

    pub fn on_subagents(&mut self, result: Result<Value, CoreError>) {
        let Ok(body) = result else { return };
        if self.disposed {
            return;
        }
        self.subagents = list(get(&body, "subagents")).to_vec();
        if let Some(id) = self.key.subagent_id.clone() {
            for child in &self.subagents {
                if get(child, "id") == id.as_str() {
                    self.child_refresh = get(child, "nativeRefresh").clone();
                }
            }
        }
        self.notify();
    }

    // ── catch-up ────────────────────────────────────────────────────────

    fn can_catch_up(&self) -> bool {
        !self.disposed && self.connected && self.subscribed
    }

    fn cancel_catch_up(&mut self, io: &mut Io) {
        self.catch_up_generation += 1;
        io.cx.stop_timer(self.catch_up_retry);
        self.catch_up_retry = Timer::empty();
        self.catch_up = None;
        self.gap_pending = false;
        self.refresh_pending = false;
    }

    /// One more resume after the running catch-up (or now).
    fn request_refresh(&mut self, io: &mut Io) {
        if self.catch_up == Some(CatchUp::Running) {
            self.refresh_pending = true;
            return;
        }
        self.start_catch_up(io, false, true);
    }

    /// viewed-timeline-sync.ts startCatchUp/decideCatchUp.
    pub fn start_catch_up(&mut self, io: &mut Io, gap: bool, supersede: bool) {
        if !self.can_catch_up() {
            if gap {
                self.gap_pending = true;
            }
            return;
        }
        let supersede = supersede || gap;
        match self.catch_up {
            Some(CatchUp::Running) => {
                // keep-and-park: the running fetch owns the request; the gap runs next.
                if gap {
                    self.gap_pending = true;
                }
                return;
            }
            Some(CatchUp::Complete) if !supersede => return,
            _ => {}
        }
        io.cx.stop_timer(self.catch_up_retry);
        self.catch_up_retry = Timer::empty();
        if supersede || self.catch_up != Some(CatchUp::Error) {
            self.catch_up_delay = None;
        }
        self.catch_up_generation += 1;
        let generation = self.catch_up_generation;
        self.catch_up = Some(CatchUp::Running);
        self.gap_pending = false;
        self.enqueue(io, LoadOp::CatchUp { generation, gap });
    }

    fn owns_catch_up(&self, generation: u64) -> bool {
        self.can_catch_up() && self.catch_up_generation == generation
    }

    /// Bring the conversation current after a local action.
    pub fn reload(&mut self, io: &mut Io) {
        self.start_catch_up(io, false, true);
    }

    /// timeline-replacement.ts requestTimelineReplacement: one latest tail.
    pub fn refresh_tail(&mut self, io: &mut Io) {
        self.enqueue(io, LoadOp::Tail { force_replace: false });
    }

    /// use-load-older-agent-history.ts: one `before` page.
    pub fn older(&mut self, io: &mut Io) {
        self.enqueue(io, LoadOp::Older { reveal: None });
    }

    /// Page older history until [seq] is loaded (chat find, outline jumps).
    pub fn reveal(&mut self, io: &mut Io, seq: i64, token: u64) {
        self.enqueue(io, LoadOp::Older { reveal: Some((seq, token)) });
    }

    fn enqueue(&mut self, io: &mut Io, op: LoadOp) {
        self.queue.push_back(op);
        self.pump(io);
    }

    /// `_loads` serialization: one load at a time; live events buffer while
    /// it runs and replay when it settles.
    fn pump(&mut self, io: &mut Io) {
        while self.active.is_none() {
            let Some(op) = self.queue.pop_front() else { return };
            if self.disposed {
                self.queue.clear();
                return;
            }
            if !self.loading {
                self.loading = true;
                self.notify();
            }
            let mut active = ActiveLoad {
                op: op.clone(),
                request: None,
                fallback_to_latest_tail: false,
                forced_tail: false,
                previous_start: None,
            };
            match op {
                LoadOp::CatchUp { generation, gap } => {
                    if !self.owns_catch_up(generation) {
                        self.finish_load(io);
                        continue;
                    }
                    active.fallback_to_latest_tail = !gap && self.end_cursor.is_some();
                    let direction = if self.end_cursor.is_none() { "tail" } else { "after" };
                    active.request = Some(self.fetch(io, direction, false));
                }
                LoadOp::Tail { force_replace } => {
                    active.request = Some(self.fetch(io, "tail", force_replace));
                }
                LoadOp::Older { reveal } => {
                    let wanted = match reveal {
                        None => self.start_cursor.is_some() && self.has_older,
                        Some((seq, _)) => {
                            self.has_older
                                && self.start_cursor.as_ref().is_none_or(|c| c.seq > seq)
                        }
                    };
                    if !wanted {
                        if let Some((_, token)) = reveal {
                            self.reveals.push(RevealDone { token, ok: true });
                        }
                        self.finish_load(io);
                        continue;
                    }
                    active.previous_start = self.start_cursor.as_ref().map(|c| c.seq);
                    active.request = Some(self.fetch(io, "before", false));
                }
            }
            self.active = Some(active);
        }
    }

    fn fetch(&mut self, io: &mut Io, direction: &'static str, force_replace: bool) -> RequestId {
        let generation = self.generation;
        let cursor = if direction == "before" { self.start_cursor.clone() } else { self.end_cursor.clone() };
        let mut fields = if let Some(subagent) = self.key.subagent_id.clone() {
            json!({"parentAgentId": self.key.agent_id, "subagentId": subagent})
        } else {
            json!({"agentId": self.key.agent_id, "projection": "projected"})
        };
        let obj = fields.as_object_mut().expect("object");
        obj.insert("direction".into(), json!(direction));
        obj.insert("limit".into(), json!(TIMELINE_FETCH_PAGE_SIZE));
        if direction != "tail"
            && let Some(cursor) = cursor
        {
            obj.insert("cursor".into(), cursor.to_json());
        }
        let (ty, response) = if self.is_subagent() {
            ("agent.provider_subagents.timeline.get.request", "agent.provider_subagents.timeline.get.response")
        } else {
            ("fetch_agent_timeline_request", "fetch_agent_timeline_response")
        };
        self.send(io, Pending::Fetch { direction, force_replace, generation }, ty, response, fields)
    }

    /// A fetch response for the active load.
    pub fn on_fetch(&mut self, io: &mut Io, request: &RequestId, direction: &'static str, force_replace: bool, generation: u64, result: Result<Value, CoreError>) {
        let Some(mut active) = self.active.take() else { return };
        if active.request.as_ref() != Some(request) {
            self.active = Some(active);
            return;
        }
        active.request = None;
        let page = match result {
            Ok(page) => {
                if !self.disposed && generation == self.generation {
                    self.apply_page(io, direction, &page, force_replace);
                }
                page
            }
            Err(error) => {
                self.load_failed(io, active, error.message);
                return;
            }
        };
        match active.op.clone() {
            LoadOp::CatchUp { generation: owner, .. } => {
                if !self.owns_catch_up(owner) {
                    self.finish_load(io);
                    return;
                }
                if active.forced_tail {
                    self.catch_up_complete(io);
                    self.finish_load(io);
                    return;
                }
                let has_newer = is_true(get(&page, "hasNewer"));
                if has_newer && get(&page, "endCursor").is_object() {
                    if active.fallback_to_latest_tail {
                        active.forced_tail = true;
                        active.request = Some(self.fetch(io, "tail", true));
                    } else {
                        active.fallback_to_latest_tail = false;
                        active.request = Some(self.fetch(io, "after", false));
                    }
                    self.active = Some(active);
                    return;
                }
                if has_newer {
                    let message = format!("Timeline page for {} hasNewer without an end cursor", self.key.agent_id);
                    self.load_failed(io, active, message);
                    return;
                }
                self.catch_up_complete(io);
                self.finish_load(io);
            }
            LoadOp::Tail { .. } => self.finish_load(io),
            LoadOp::Older { reveal } => {
                if let Some((seq, token)) = reveal {
                    let start = self.start_cursor.as_ref().map(|c| c.seq);
                    let more = !self.disposed
                        && self.has_older
                        && start.is_none_or(|s| s > seq)
                        && start != active.previous_start;
                    if more {
                        active.previous_start = start;
                        active.request = Some(self.fetch(io, "before", false));
                        self.active = Some(active);
                        return;
                    }
                    self.reveals.push(RevealDone { token, ok: true });
                }
                self.finish_load(io);
            }
        }
    }

    fn load_failed(&mut self, io: &mut Io, active: ActiveLoad, message: String) {
        match active.op {
            LoadOp::CatchUp { generation, .. } => {
                if !self.disposed && generation == self.catch_up_generation {
                    let delay = next_retry_delay(self.catch_up_delay);
                    self.catch_up_delay = Some(delay);
                    self.catch_up = Some(CatchUp::Error);
                    io.cx.stop_timer(self.catch_up_retry);
                    self.catch_up_retry = io.cx.start_timeout(delay);
                    log_sync_failure(&self.key, &message);
                    self.set_error(message);
                }
            }
            LoadOp::Tail { .. } => log_sync_failure(&self.key, &message),
            LoadOp::Older { reveal } => match reveal {
                // A reveal's failure is its caller's (find / outline).
                Some((_, token)) => self.reveals.push(RevealDone { token, ok: false }),
                // A failed older page is a toast, never a timeline sync error.
                None => self.older_failures += 1,
            },
        }
        self.finish_load(io);
    }

    fn catch_up_complete(&mut self, io: &mut Io) {
        self.catch_up = Some(CatchUp::Complete);
        self.catch_up_delay = None;
        if self.gap_pending {
            self.gap_pending = false;
            self.catch_up = None;
            // Runs after this load settles (scheduleMicrotask).
            let generation = self.catch_up_generation + 1;
            self.catch_up_generation = generation;
            self.catch_up = Some(CatchUp::Running);
            self.queue.push_back(LoadOp::CatchUp { generation, gap: true });
            return;
        }
        self.pending = false;
        // viewed-timeline-sync.ts onCatchUpEnded → create-flow-store clearByAgent.
        self.ended_create_handoff = true;
        self.error = None;
        self.manual_retry = false;
        self.notify();
        if self.refresh_pending {
            self.refresh_pending = false;
            self.catch_up_generation += 1;
            let generation = self.catch_up_generation;
            self.catch_up = Some(CatchUp::Running);
            self.catch_up_delay = None;
            self.queue.push_back(LoadOp::CatchUp { generation, gap: false });
        }
        let _ = io;
    }

    fn finish_load(&mut self, io: &mut Io) {
        self.active = None;
        self.loading = false;
        self.flush_buffer();
        self.project();
        self.pump(io);
    }

    fn flush_buffer(&mut self) {
        let pending = std::mem::take(&mut self.buffer);
        for event in pending {
            self.apply_event_inner(None, &event);
        }
    }

    fn set_error(&mut self, failure: String) {
        self.manual_retry = false;
        self.preparation_retry = false;
        self.pending = false;
        self.error = Some(if failure.is_empty() {
            "Conversation synchronization failed".to_owned()
        } else {
            failure
        });
        self.notify();
    }

    // ── authoritative pages ─────────────────────────────────────────────

    /// session-stream-reducers.ts processTimelineResponse.
    fn apply_page(&mut self, io: &mut Io, direction: &str, result: &Value, force_replace: bool) {
        let next_epoch = str_of(get(result, "epoch")).to_owned();
        if get(result, "agent").is_object() {
            self.set_agent(get(result, "agent").clone());
        }
        let source = if self.is_subagent() { get(result, "rows") } else { get(result, "entries") };
        let incoming: Vec<Value> = list(source)
            .iter()
            .filter(|row| row.is_object())
            .map(|row| {
                let start = int_or(get(row, "seqStart"), int_of(get(row, "seq")));
                let end = int_or(get(row, "seqEnd"), start);
                let mut next = map(row).clone();
                next.insert("seqStart".into(), json!(start));
                next.insert("seqEnd".into(), json!(end));
                Value::Object(next)
            })
            .collect();
        let page_start = if get(result, "startCursor").is_object() {
            Some(int_of(get(get(result, "startCursor"), "seq")))
        } else {
            incoming.iter().map(|row| int_of(get(row, "seqStart"))).min()
        };
        let page_end = if get(result, "endCursor").is_object() {
            Some(int_of(get(get(result, "endCursor"), "seq")))
        } else {
            incoming.iter().map(|row| int_of(get(row, "seqEnd"))).max()
        };
        let local_end = self.end_cursor.as_ref().map(|c| c.seq);
        let local_start = self.start_cursor.as_ref().map(|c| c.seq);

        enum Mode {
            Replace,
            Append,
            Prepend,
            Discard,
        }
        let mut units = incoming.clone();
        let mode = if force_replace || is_true(get(result, "reset")) {
            Mode::Replace
        } else if direction == "tail" {
            // deriveBootstrapTailTimelinePolicy / deriveResumeTailPolicy.
            let window_max = int_of(get(get(result, "window"), "maxSeq"));
            match local_end {
                _ if !self.synced || self.epoch.is_none() || self.epoch.as_deref() != Some(next_epoch.as_str()) => Mode::Replace,
                None => Mode::Replace,
                Some(end) if window_max == end => Mode::Discard,
                Some(end) if window_max < end => Mode::Replace,
                Some(end) if page_start.is_some_and(|s| s <= end + 1) => {
                    units.retain(|row| int_of(get(row, "seqEnd")) > end);
                    Mode::Append
                }
                Some(_) => Mode::Replace,
            }
        } else if direction == "after" {
            // acceptIncrementalTimelineUnits.
            match (page_start, page_end, local_end) {
                (None, _, _) | (_, None, _) => Mode::Discard,
                (_, _, None) => {
                    if self.synced { Mode::Append } else { Mode::Replace }
                }
                (Some(start), Some(end), Some(local)) => {
                    if self.epoch.as_deref() != Some(next_epoch.as_str()) || end <= local {
                        Mode::Discard
                    } else if start > local + 1 {
                        self.gap_pending = true;
                        Mode::Discard
                    } else {
                        Mode::Append
                    }
                }
            }
        } else {
            // acceptOlderTimelineUnits: only a page adjacent to the history start.
            match (local_start, page_end) {
                (Some(start), Some(end)) if self.epoch.as_deref() == Some(next_epoch.as_str()) && end == start - 1 => Mode::Prepend,
                _ => Mode::Discard,
            }
        };

        match mode {
            Mode::Replace => {
                self.rows = incoming;
                self.fold.reset();
                if self.epoch.as_deref() != Some(next_epoch.as_str()) {
                    self.turns.clear();
                }
                self.epoch = Some(next_epoch.clone());
                self.start_cursor = page_start.map(|seq| Cursor { epoch: next_epoch.clone(), seq });
                self.end_cursor = page_end.map(|seq| Cursor { epoch: next_epoch.clone(), seq });
                self.has_older = is_true(get(result, "hasOlder"));
            }
            Mode::Append => {
                self.merge(units);
                self.epoch = Some(next_epoch.clone());
                if let Some(end) = page_end
                    && local_end.is_none_or(|local| end > local)
                {
                    self.end_cursor = Some(Cursor { epoch: next_epoch.clone(), seq: end });
                }
                if self.start_cursor.is_none()
                    && let Some(start) = page_start
                {
                    self.start_cursor = Some(Cursor { epoch: next_epoch.clone(), seq: start });
                    self.has_older = is_true(get(result, "hasOlder"));
                }
            }
            Mode::Prepend => {
                self.merge(units);
                let seq = page_start.or(local_start).unwrap_or_default();
                self.start_cursor = Some(Cursor { epoch: next_epoch.clone(), seq });
                self.has_older = is_true(get(result, "hasOlder"));
            }
            Mode::Discard => {}
        }
        if direction != "before" {
            self.synced = true;
        }
        if !self.is_subagent() && direction != "before" {
            // A retried preparation settles with the fetch that restarted it.
            self.preparation_retry = false;
            self.history_saved = get(result, "historySource") == "saved";
            self.refresh_saved_when_ready(io);
        }
        if self.is_subagent() && direction != "before" {
            self.preparation_retry = false;
            self.child_refresh = get(result, "nativeRefresh").clone();
        }
        self.notify();
    }

    /// Saved history is replaced through one ordinary `after` page once the
    /// provider is ready. Nothing is cleared before that page.
    fn refresh_saved_when_ready(&mut self, io: &mut Io) {
        if self.is_subagent()
            || !self.history_saved
            || self.live_refresh_done
            || map(&self.agent).is_empty()
            || self.execution() != ExecutionStatus::Ready
            || get(&self.agent, "snapshotAvailability") == "last_persisted"
        {
            return;
        }
        self.live_refresh_done = true;
        self.request_refresh(io);
    }

    fn ranges(row: &Value) -> Vec<(i64, i64)> {
        let ranges = list(get(row, "sourceSeqRanges"));
        if ranges.is_empty() {
            return vec![(int_of(get(row, "seqStart")), int_of(get(row, "seqEnd")))];
        }
        ranges
            .iter()
            .map(|r| (int_of(get(r, "startSeq")), int_of(get(r, "endSeq"))))
            .collect()
    }

    /// A projected row can grow across a cursor. Replace its covered live
    /// chunks, not merely the row with the same start.
    fn merge(&mut self, incoming: Vec<Value>) {
        let mut coverage: Vec<(i64, i64)> = incoming.iter().flat_map(Self::ranges).collect();
        coverage.sort_by_key(|r| r.0);
        let mut merged: Vec<(i64, i64)> = Vec::new();
        for range in coverage {
            match merged.last_mut() {
                Some(last) if range.0 <= last.1 + 1 => last.1 = last.1.max(range.1),
                _ => merged.push(range),
            }
        }
        self.rows.retain(|row| {
            !Self::ranges(row).iter().any(|range| {
                let (mut lo, mut hi) = (0usize, merged.len());
                while lo < hi {
                    let mid = (lo + hi) / 2;
                    if merged[mid].1 < range.0 {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                lo < merged.len() && merged[lo].0 <= range.1
            })
        });
        self.rows.extend(incoming);
        // Stable sort (mergeSort).
        self.rows.sort_by_key(|row| int_of(get(row, "seqStart")));
        self.fold.reset();
    }

    // ── live events ─────────────────────────────────────────────────────

    /// Whether a host event concerns this conversation (`ChatController._event`).
    pub fn wants(&self, message: &Value) -> bool {
        let payload = get(message, "payload");
        let id = self.key.agent_id.as_str();
        get(payload, "agentId") == id
            || get(get(payload, "agent"), "id") == id
            || get(payload, "parentAgentId") == id
            || get(get(payload, "subagent"), "parentAgentId") == id
    }

    /// Applies ordered live state now; the store publishes projection once per GUI batch.
    pub fn on_event(&mut self, io: &mut Io, message: &Value) {
        if self.disposed || !self.wants(message) {
            return;
        }
        if self.loading {
            self.buffer.push(message.clone());
            return;
        }
        self.apply_event_inner(Some(io), message);
        self.projection_pending = true;
    }

    fn apply_event_inner(&mut self, mut io: Option<&mut Io>, message: &Value) {
        let p = get(message, "payload");
        let ty = str_of(get(message, "type"));
        let id = self.key.agent_id.clone();
        if ty == "agent.provider_subagents.update" {
            let kind = get(p, "kind");
            if kind == "upsert" {
                let child = get(p, "subagent");
                if get(child, "parentAgentId") == id.as_str() {
                    self.subagents.retain(|s| get(s, "id") != get(child, "id"));
                    self.subagents.push(child.clone());
                    if let Some(sub) = &self.key.subagent_id
                        && get(child, "id") == sub.as_str()
                    {
                        self.child_refresh = get(child, "nativeRefresh").clone();
                    }
                }
            } else if get(p, "parentAgentId") == id.as_str() && kind == "remove" {
                self.subagents.retain(|s| get(s, "id") != get(p, "subagentId"));
            }
            if let Some(sub) = self.key.subagent_id.clone()
                && get(p, "parentAgentId") == id.as_str()
                && get(p, "subagentId") == sub.as_str()
                && kind == "timeline"
            {
                let event = json!({"type": "timeline", "item": get(p, "item"), "provider": get(p, "provider")});
                self.live(io.as_deref_mut(), p, &event);
            }
            self.notify();
            return;
        }
        if get(p, "agentId") != id.as_str() && get(get(p, "agent"), "id") != id.as_str() {
            return;
        }
        if ty == "agent.timeline.replacement" {
            if !self.is_subagent() {
                if let Some(io) = io.as_deref_mut() {
                    self.refresh_tail(io);
                } else {
                    self.queue.push_back(LoadOp::Tail { force_replace: false });
                }
            }
            return;
        }
        if ty == "agent_stream" && !self.is_subagent() {
            let event = get(p, "event").clone();
            self.live(io.as_deref_mut(), p, &event);
        }
        if ty == "agent_permission_request" {
            let request = get(p, "request").clone();
            let key = pending_permission_key(&request);
            if let Some(slot) = self.permissions.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = request;
            } else {
                self.permissions.push((key, request));
            }
        }
        if ty == "agent_permission_resolved" {
            // session-context.tsx: the resolution removes the pending request.
            let request_id = get(p, "requestId");
            let before = self.permissions.len();
            let as_key = value_key(request_id);
            self.permissions.retain(|(k, _)| *k != as_key);
            if self.permissions.len() == before
                && let Some(index) = self.permissions.iter().position(|(_, v)| get(v, "id") == request_id)
            {
                self.permissions.remove(index);
            }
        }
        if get(p, "agent").is_object() {
            self.set_agent(get(p, "agent").clone());
        }
        if ty == "agent_status" && get(p, "info").is_object() {
            self.set_agent(get(p, "info").clone());
        }
        if let Some(io) = io {
            self.refresh_saved_when_ready(io);
        }
        self.notify();
    }

    /// A live agent snapshot is authoritative for its activity and pending
    /// permissions. A preparing/failed provider has neither, and a
    /// last-persisted observation cannot revoke or grant what live events reported.
    fn set_agent(&mut self, value: Value) {
        self.agent = value;
        if get(&self.agent, "preparation").is_object() {
            self.permissions.clear();
            self.live_refresh_done = false;
            self.live_observed = false;
            return;
        }
        if get(&self.agent, "snapshotAvailability") == "last_persisted" {
            return;
        }
        self.live_observed = true;
        let active = get(&self.agent, "activeTurn").clone();
        if let Some(turn_id) = active.get("turnId").and_then(Value::as_str) {
            let known = self.turns.get(turn_id).map(|t| get(t, "startedAt").clone());
            let mut record = json!({"type": "turn_started"});
            for (k, v) in map(&active) {
                record[k] = v.clone();
            }
            // turn-liveness.ts openTurn: the same turn keeps its first start.
            if let Some(known) = known.filter(|k| !k.is_null()) {
                record["startedAt"] = known;
            }
            self.turns.insert(turn_id.to_owned(), record);
        }
        self.permissions = list(get(&self.agent, "pendingPermissions"))
            .iter()
            .map(|p| (pending_permission_key(p), p.clone()))
            .collect();
    }

    /// A background native refresh can move an open child to a new epoch
    /// without a replacement notification: install its latest tail once.
    fn resync_child(&mut self, io: Option<&mut Io>, announced: &str) {
        if self.child_resync_epoch.as_deref() == Some(announced) {
            return;
        }
        self.child_resync_epoch = Some(announced.to_owned());
        match io {
            Some(io) => self.refresh_tail(io),
            None => self.queue.push_back(LoadOp::Tail { force_replace: false }),
        }
    }

    /// processTimelineSequencingGate + turn-liveness.ts for lifecycle events.
    fn live(&mut self, io: Option<&mut Io>, payload: &Value, event: &Value) {
        let ty = str_of(get(event, "type"));
        if ty == "timeline" {
            let seq = get(payload, "seq");
            let event_epoch = get(payload, "epoch");
            let row = |position: i64| {
                json!({
                    "item": get(event, "item"),
                    "provider": get(event, "provider"),
                    "turnId": get(event, "turnId"),
                    "timestamp": get(payload, "timestamp"),
                    "seqStart": position,
                    "seqEnd": position,
                    "sourceSeqRanges": [{"startSeq": position, "endSeq": position}],
                })
            };
            let (Some(seq), Some(event_epoch)) = (seq.as_i64(), event_epoch.as_str()) else {
                // Unsequenced (legacy) delivery applies without moving cursors.
                let mut r = row(self.end_cursor.as_ref().map(|c| c.seq).unwrap_or(0));
                r["sourceSeqRanges"] = json!([]);
                self.rows.push(r);
                return;
            };
            if !self.synced {
                // No authoritative baseline yet: a live overlay the first page replaces.
                self.rows.push(row(seq));
                return;
            }
            match &self.end_cursor {
                None => {
                    self.epoch = Some(event_epoch.to_owned());
                    if self.start_cursor.is_none() {
                        self.start_cursor = Some(Cursor { epoch: event_epoch.to_owned(), seq });
                    }
                }
                Some(_) if self.epoch.as_deref() != Some(event_epoch) => {
                    // drop_epoch, except a fresh epoch's first event resets the timeline.
                    if seq != 1 {
                        if self.is_subagent() {
                            self.resync_child(io, event_epoch);
                        }
                        return;
                    }
                    self.generation += 1;
                    self.rows.clear();
                    self.fold.reset();
                    self.turns.clear();
                    self.epoch = Some(event_epoch.to_owned());
                    self.start_cursor = Some(Cursor { epoch: event_epoch.to_owned(), seq });
                    self.has_older = false;
                }
                Some(end) => {
                    let end = end.seq;
                    if seq <= end {
                        return;
                    }
                    if seq > end + 1 {
                        // Recover missed canonical rows before accepting a later live row.
                        match io {
                            Some(io) => self.start_catch_up(io, true, true),
                            None => self.gap_pending = true,
                        }
                        return;
                    }
                }
            }
            self.rows.push(row(seq));
            self.end_cursor = Some(Cursor { epoch: event_epoch.to_owned(), seq });
        } else if ty == "turn_started" {
            let turn_id = get(event, "turnId").as_str().map(str::to_owned);
            let active = get(&self.agent, "activeTurn").clone();
            let running = self.running(false);
            let same_turn = running
                && match &turn_id {
                    Some(id) => get(&active, "turnId") == id.as_str(),
                    None => get(&active, "turnId").is_null(),
                };
            let started_at = if same_turn && !get(&active, "startedAt").is_null() {
                get(&active, "startedAt").clone()
            } else {
                get(payload, "timestamp").clone()
            };
            if let Some(obj) = self.agent.as_object_mut() {
                obj.insert("status".into(), json!("running"));
                obj.insert("activeTurn".into(), json!({"turnId": turn_id, "startedAt": started_at}));
            }
            if let Some(turn_id) = turn_id {
                let first = self
                    .turns
                    .get(&turn_id)
                    .map(|t| get(t, "startedAt").clone())
                    .filter(|v| !v.is_null())
                    .unwrap_or(started_at);
                let mut record = event.clone();
                record["startedAt"] = first;
                self.turns.insert(turn_id, record);
            }
        } else if matches!(ty, "turn_completed" | "turn_canceled" | "turn_failed") {
            // An identified terminal never closes a different identified turn.
            let turn_id = get(event, "turnId");
            let active_id = get(get(&self.agent, "activeTurn"), "turnId");
            if self.running(false) && turn_id.is_string() && active_id.is_string() && turn_id != active_id {
                return;
            }
            if let Some(obj) = self.agent.as_object_mut() {
                obj.insert("status".into(), json!(if ty == "turn_failed" { "error" } else { "idle" }));
                obj.insert("activeTurn".into(), Value::Null);
                if !get(event, "usage").is_null() {
                    obj.insert("lastUsage".into(), get(event, "usage").clone());
                }
            }
        }
    }

    // ── projection ──────────────────────────────────────────────────────

    pub fn flush_projection(&mut self) {
        if self.projection_pending {
            self.project();
        }
    }

    /// Folds the stored rows not yet projected (all of them after any change
    /// other than an append) and keeps unchanged entries identical.
    pub fn project(&mut self) {
        self.projection_pending = false;
        if self.fold.consumed > self.rows.len() {
            self.fold.reset();
        }
        if self.fold.consumed < self.rows.len() || (self.rows.is_empty() && !self.timeline.is_empty()) {
            let refold = self.fold.consumed == 0;
            let agent_id = self.key.agent_id.clone();
            for row in &self.rows[self.fold.consumed..] {
                self.fold.add(&agent_id, row);
            }
            self.fold.consumed = self.rows.len();
            if refold && !self.timeline.is_empty() {
                self.keep_unchanged();
            }
            self.timeline = Rc::new(self.fold.entries.clone());
        }
        self.notify();
    }

    fn keep_unchanged(&mut self) {
        let previous: HashMap<i64, Rc<Value>> = self
            .timeline
            .iter()
            .filter(|entry| is_int(get(entry, "seqStart")))
            .map(|entry| (int_of(get(entry, "seqStart")), entry.clone()))
            .collect();
        for entry in self.fold.entries.iter_mut() {
            if let Some(old) = previous.get(&int_of(get(entry, "seqStart")))
                && get(old, "seqEnd") == get(entry, "seqEnd")
                && **old == **entry
            {
                *entry = old.clone();
            }
        }
    }

    // ── responses for agent-scoped actions ──────────────────────────────

    pub fn on_action(&mut self, kind: ActionKind, result: Result<Value, CoreError>) {
        if let Ok(body) = &result
            && let Some(message) = non_empty_str(get(get(body, "notice"), "message"))
        {
            self.notice = Some(message.to_owned());
        }
        self.completions.push(Completion { kind, result });
        self.notify();
    }

    /// Closing the chat ends the obligation; the store releases membership.
    pub fn dispose(&mut self, io: &mut Io) {
        self.disposed = true;
        self.generation += 1;
        self.membership_generation += 1;
        self.catch_up_generation += 1;
        io.cx.stop_timer(self.membership_retry);
        io.cx.stop_timer(self.catch_up_retry);
        // Releasing the observation ends the selective-delivery demand.
        if let Some(id) = self.observation.take() {
            io.core.cancel_request(&id);
        }
    }

    pub fn latest_tasks(&self) -> Vec<Value> {
        self.timeline
            .iter()
            .rev()
            .map(|entry| get(entry, "item"))
            .find(|item| get(item, "type") == "todo")
            .map(|item| list(get(item, "items")).to_vec())
            .unwrap_or_default()
    }

    pub fn dismiss_notice(&mut self) {
        self.notice = None;
        self.notify();
    }

    pub fn touch(&mut self) {
        self.notify();
    }
}

fn value_key(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn log_sync_failure(key: &SessionKey, message: &str) {
    makepad_widgets::log!("[ChatSession {}] timeline synchronization failed: {}", key.agent_id, message);
}

#[cfg(test)]
mod timer_ownership_tests {
    use super::*;
    use makepad_widgets::{Event, TimerEvent};

    #[test]
    fn only_live_session_retry_timers_are_owned() {
        let key = SessionKey {
            host_id: "host".into(),
            agent_id: "agent".into(),
            subagent_id: None,
        };
        let mut session = ChatSession::new(key, true);
        let tick = |timer_id| Event::Timer(TimerEvent { time: None, timer_id });
        assert!(!session.owns_timer(&tick(0)));
        assert!(!session.owns_timer(&Event::Signal));
        session.membership_retry = Timer(41);
        session.catch_up_retry = Timer(42);
        assert!(session.owns_timer(&tick(41)));
        assert!(session.owns_timer(&tick(42)));
        assert!(!session.owns_timer(&tick(43)));
        session.membership_retry = Timer::empty();
        assert!(!session.owns_timer(&tick(41)));
        assert!(session.owns_timer(&tick(42)));
    }
}
