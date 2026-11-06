//! State shared by every git view of the window: per-checkout status and PR
//! status (original `CheckoutGitState`), the action runner
//! (`CheckoutGitActionsStore`), checkout invalidations
//! (`CheckoutInvalidations`), workspace comparisons
//! (`WorkingDiffComparisons`), review drafts (`ReviewDraft` registry) and the
//! unsupported-timeline registry.
//!
//! Every git root calls [`GitStore::begin_event`] first in `handle_event`; the
//! store applies a core batch once (keyed by `cx.event_id()`), so retained and
//! visible roots never double-apply a push. When the store changes it posts
//! [`GitStoreChanged`], so every root re-syncs in the follow-up dispatch.

use super::host;
use super::json::{error_message, flag, object, text};
use super::policy::{ActionStatus, normalize_pr_status_payload};
use super::review::{ReviewDraft, storage_key};
use super::text::t;
use super::wire::{self, Failure};
use crate::common::{UiContext, UiEffect};
use makepad_widgets::*;
use paseo_gui_core::{CoreEvent, CoreHandle, RequestId};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

thread_local! {
    static STORE: RefCell<GitStore> = RefCell::new(GitStore::default());
}

/// Runs `f` with the window's git store. Never nest calls.
pub fn with_store<R>(f: impl FnOnce(&mut GitStore) -> R) -> R {
    STORE.with(|store| f(&mut store.borrow_mut()))
}

/// Posted after the store changed so every git root re-syncs.
#[derive(Clone, Debug, Default)]
pub struct GitStoreChanged;

pub type CheckoutKey = (String, String);

pub fn key(host: &str, cwd: &str) -> CheckoutKey {
    (host.to_owned(), cwd.to_owned())
}

/// One status read shared by every reader that joined it.
struct ReadSlot {
    result: Option<Result<Value, Failure>>,
    waiters: usize,
}

/// Original `CheckoutGitState` for one host + cwd.
pub struct CheckoutState {
    pub refs: usize,
    pub status: Option<Value>,
    pub pr_payload: Option<Value>,
    pub status_error: Option<String>,
    pub status_loading: bool,
    pub pr_loading: bool,
    pub pr_fetching: bool,
    pub pr_error: Option<String>,
    /// Bumped whenever the PR status meaningfully changes (ignoring `requestId`).
    pub pr_revision: u64,
    status_generation: u64,
    pr_generation: u64,
    read_seq: u64,
    reads: HashMap<u64, ReadSlot>,
    /// The read new readers join: `(seq, invalidation revision when sent)`.
    current_read: Option<(u64, u64)>,
    /// The state's own reload awaiting `(seq, generation)`.
    reload_wait: Option<(u64, u64)>,
}

impl CheckoutState {
    fn new() -> Self {
        Self {
            refs: 0,
            status: None,
            pr_payload: None,
            status_error: None,
            status_loading: false,
            pr_loading: false,
            pr_fetching: false,
            pr_error: None,
            pr_revision: 0,
            status_generation: 0,
            pr_generation: 0,
            read_seq: 0,
            reads: HashMap::new(),
            current_read: None,
            reload_wait: None,
        }
    }

    pub fn is_git(&self) -> bool {
        self.status.as_ref().is_some_and(|s| flag(s, "isGit"))
    }

    fn set_pr_payload(&mut self, next: &Value) {
        let normalized = normalize_pr_status_payload(next);
        let identity = |payload: Option<&Value>| match payload {
            None => String::new(),
            Some(Value::Object(map)) => {
                let mut map = map.clone();
                map.remove("requestId");
                Value::Object(map).to_string()
            }
            Some(other) => other.to_string(),
        };
        if identity(self.pr_payload.as_ref()) != identity(Some(&normalized)) {
            self.pr_revision += 1;
        }
        self.pr_payload = Some(normalized);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionCheck {
    /// `gitRequest`: payload error or rejection fails.
    Git,
    /// `payload.success != true` fails (discard, archive).
    RequireSuccess,
}

/// What a finished action does besides invalidating its checkout.
#[derive(Clone, Debug)]
pub enum ActionFollowUp {
    None,
    SuggestPostShipArchive,
}

struct ActionRun {
    host: String,
    cwd: String,
    action_id: String,
    steps: VecDeque<(String, Value)>,
    check: ActionCheck,
    failure: String,
    success_label: Option<String>,
    follow_up: ActionFollowUp,
}

enum Pending {
    StatusRead { key: CheckoutKey, seq: u64 },
    PrStatus { key: CheckoutKey, generation: u64 },
    Action(ActionRun),
    Archive { key: CheckoutKey, workspace_id: String, failure: String },
}

enum StoreTimer {
    ActionIdle { key: CheckoutKey, action_id: String },
}

/// The window-wide git store.
#[derive(Default)]
pub struct GitStore {
    core: Option<CoreHandle>,
    processed_event: u64,
    notified_event: u64,
    pub revision: u64,
    connected: HashMap<String, bool>,
    checkouts: HashMap<CheckoutKey, CheckoutState>,
    invalidations: HashMap<CheckoutKey, u64>,
    action_status: HashMap<(CheckoutKey, String), ActionStatus>,
    post_ship_archive: HashSet<CheckoutKey>,
    archiving: HashSet<CheckoutKey>,
    comparisons: HashMap<String, String>,
    pub comparisons_revision: u64,
    drafts: HashMap<String, ReviewDraft>,
    unsupported_timelines: HashSet<String>,
    pending: HashMap<RequestId, Pending>,
    timers: Vec<(Timer, StoreTimer)>,
}

impl GitStore {
    pub fn core(&self) -> Option<&CoreHandle> {
        self.core.as_ref()
    }

    /// Marks a visible change: bumps the revision and, once per event, posts
    /// [`GitStoreChanged`] so roots dispatched earlier in this event re-sync.
    pub fn changed(&mut self, cx: &mut Cx) {
        self.revision += 1;
        let event = cx.event_id();
        if self.notified_event != event {
            self.notified_event = event;
            cx.action(GitStoreChanged);
        }
    }

    /// Applies the current core batch once per Makepad event, then timers.
    pub fn begin_event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext) {
        if self.core.is_none() {
            self.core = Some(ui.core.clone());
        }
        if let Some(index) = self
            .timers
            .iter()
            .position(|(timer, _)| timer.is_event(event).is_some())
        {
            let (_, job) = self.timers.remove(index);
            match job {
                StoreTimer::ActionIdle { key, action_id } => {
                    if self.action_status.remove(&(key, action_id)).is_some() {
                        self.changed(cx);
                    }
                }
            }
        }
        if ui.events.is_empty() || self.processed_event == cx.event_id() {
            return;
        }
        self.processed_event = cx.event_id();
        let events = std::mem::take(&mut ui.events);
        for core_event in &events {
            match core_event {
                CoreEvent::Connected { host_id, .. } => {
                    self.connected.insert(host_id.clone(), true);
                    let keys: Vec<CheckoutKey> =
                        self.checkouts.keys().filter(|k| &k.0 == host_id).cloned().collect();
                    for key in keys {
                        self.reload(cx, &key);
                    }
                }
                CoreEvent::Disconnected { host_id, .. } => {
                    self.connected.insert(host_id.clone(), false);
                    // A read sent on the lost connection never serves the next one.
                    for (key, state) in self.checkouts.iter_mut() {
                        if &key.0 == host_id {
                            state.current_read = None;
                        }
                    }
                }
                CoreEvent::HostEvent { host_id, event } => {
                    if event["type"] == "checkout_status_update" {
                        self.apply_status_push(cx, host_id, &event["payload"]);
                    }
                }
                CoreEvent::Response { request_id, result, .. } => {
                    if let Some(pending) = self.pending.remove(request_id) {
                        self.on_response(cx, ui, pending, result);
                    }
                }
                _ => {}
            }
        }
        ui.events = events;
    }

    fn apply_status_push(&mut self, cx: &mut Cx, host: &str, payload: &Value) {
        let key = key(host, text(payload, "cwd"));
        let Some(state) = self.checkouts.get_mut(&key) else {
            return;
        };
        state.status_generation += 1;
        // A read sent before this push would overwrite it for a later joiner.
        state.current_read = None;
        let mut status = payload.clone();
        if let Value::Object(map) = &mut status {
            map.remove("prStatus");
        }
        state.status = Some(status);
        state.status_error = None;
        state.status_loading = false;
        if payload["prStatus"].is_object() {
            state.pr_generation += 1;
            state.set_pr_payload(&payload["prStatus"]);
            state.pr_error = None;
            state.pr_loading = false;
        }
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Checkout state lifecycle.

    /// Original `CheckoutGitState.acquire`: shared per host + cwd.
    pub fn acquire_checkout(&mut self, cx: &mut Cx, ui: &UiContext, host_id: &str, cwd: &str) {
        // A widget may acquire while drawing, before its first `begin_event`;
        // without the core handle the initial read would never be sent.
        if self.core.is_none() {
            self.core = Some(ui.core.clone());
        }
        let checkout = key(host_id, cwd);
        let created = !self.checkouts.contains_key(&checkout);
        self.checkouts.entry(checkout.clone()).or_insert_with(CheckoutState::new).refs += 1;
        self.connected
            .entry(host_id.to_owned())
            .or_insert_with(|| host::connected(ui, host_id));
        if created && host::connected(ui, host_id) {
            self.reload(cx, &checkout);
        }
    }

    pub fn release_checkout(&mut self, host_id: &str, cwd: &str) {
        let checkout = key(host_id, cwd);
        if let Some(state) = self.checkouts.get_mut(&checkout) {
            state.refs = state.refs.saturating_sub(1);
            if state.refs == 0 {
                self.checkouts.remove(&checkout);
                self.pending.retain(|_, pending| match pending {
                    Pending::StatusRead { key, .. } | Pending::PrStatus { key, .. } => *key != checkout,
                    _ => true,
                });
            }
        }
    }

    pub fn checkout(&self, host_id: &str, cwd: &str) -> Option<&CheckoutState> {
        self.checkouts.get(&key(host_id, cwd))
    }

    pub fn is_connected(&self, host_id: &str) -> bool {
        self.connected.get(host_id).copied().unwrap_or(false)
    }

    /// Original `CheckoutGitState.readStatus`: joins the read in flight unless
    /// the checkout was invalidated, pushed or disconnected since it was sent.
    /// Returns the read's sequence for [`take_read`](Self::take_read).
    pub fn read_status(&mut self, host_id: &str, cwd: &str) -> Option<u64> {
        let checkout = key(host_id, cwd);
        let invalidation = self.invalidation(host_id, cwd);
        let core = self.core.clone()?;
        let state = self.checkouts.get_mut(&checkout)?;
        if let Some((seq, sent_at)) = state.current_read
            && sent_at == invalidation
            && let Some(slot) = state.reads.get_mut(&seq)
        {
            slot.waiters += 1;
            return Some(seq);
        }
        state.read_seq += 1;
        let seq = state.read_seq;
        let request = wire::send(&core, host_id, "checkout_status_request", json!({ "cwd": cwd }));
        state.reads.insert(seq, ReadSlot { result: None, waiters: 1 });
        state.current_read = Some((seq, invalidation));
        self.pending.insert(request, Pending::StatusRead { key: checkout, seq });
        Some(seq)
    }

    /// The settled result of read `seq`, consumed once per joined reader.
    pub fn take_read(&mut self, host_id: &str, cwd: &str, seq: u64) -> Option<Result<Value, Failure>> {
        let state = self.checkouts.get_mut(&key(host_id, cwd))?;
        let slot = state.reads.get_mut(&seq)?;
        let result = slot.result.clone()?;
        slot.waiters = slot.waiters.saturating_sub(1);
        if slot.waiters == 0 {
            state.reads.remove(&seq);
        }
        Some(result)
    }

    /// A reader stopped waiting for read `seq` (hidden, rebound or dropped).
    pub fn abandon_read(&mut self, host_id: &str, cwd: &str, seq: u64) {
        if let Some(state) = self.checkouts.get_mut(&key(host_id, cwd))
            && let Some(slot) = state.reads.get_mut(&seq)
        {
            slot.waiters = slot.waiters.saturating_sub(1);
            if slot.waiters == 0 && slot.result.is_some() {
                state.reads.remove(&seq);
            }
        }
    }

    /// Original `CheckoutGitState.reload`.
    pub fn reload(&mut self, cx: &mut Cx, checkout: &CheckoutKey) {
        let Some(state) = self.checkouts.get_mut(checkout) else {
            return;
        };
        state.status_generation += 1;
        let generation = state.status_generation;
        state.status_loading = state.status.is_none();
        if let Some((seq, _)) = state.reload_wait.take() {
            self.abandon_read(&checkout.0, &checkout.1, seq);
        }
        if let Some(seq) = self.read_status(&checkout.0, &checkout.1)
            && let Some(state) = self.checkouts.get_mut(checkout)
        {
            state.reload_wait = Some((seq, generation));
        }
        self.changed(cx);
    }

    /// Original `CheckoutGitState.reloadPullRequest`.
    pub fn reload_pull_request(&mut self, cx: &mut Cx, checkout: &CheckoutKey) {
        let Some(core) = self.core.clone() else { return };
        let Some(state) = self.checkouts.get_mut(checkout) else {
            return;
        };
        state.pr_generation += 1;
        state.pr_loading = state.pr_payload.is_none();
        state.pr_fetching = true;
        let request = wire::send(&core, &checkout.0, "checkout_pr_status_request", json!({ "cwd": checkout.1 }));
        self.pending.insert(
            request,
            Pending::PrStatus {
                key: checkout.clone(),
                generation: state.pr_generation,
            },
        );
        self.changed(cx);
    }

    fn on_response(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        pending: Pending,
        result: &Result<Value, paseo_gui_core::CoreError>,
    ) {
        match pending {
            Pending::StatusRead { key: checkout, seq } => {
                let Some(state) = self.checkouts.get_mut(&checkout) else {
                    return;
                };
                if let Some(slot) = state.reads.get_mut(&seq) {
                    slot.result = Some(wire::checkout_result(result));
                }
                if state.current_read.is_some_and(|(current, _)| current == seq) {
                    state.current_read = None;
                }
                let Some((wait_seq, generation)) = state.reload_wait else {
                    self.changed(cx);
                    return;
                };
                if wait_seq != seq {
                    self.changed(cx);
                    return;
                }
                state.reload_wait = None;
                let read = self.take_read(&checkout.0, &checkout.1, seq);
                let Some(state) = self.checkouts.get_mut(&checkout) else {
                    return;
                };
                if generation != state.status_generation {
                    self.changed(cx);
                    return;
                }
                match read {
                    Some(Ok(payload)) => {
                        state.status = Some(payload);
                        state.status_error = None;
                    }
                    Some(Err(failure)) => state.status_error = Some(failure.message),
                    None => {}
                }
                state.status_loading = false;
                let is_git = state.is_git();
                if is_git {
                    self.reload_pull_request(cx, &checkout);
                }
                self.changed(cx);
            }
            Pending::PrStatus { key: checkout, generation } => {
                let Some(state) = self.checkouts.get_mut(&checkout) else {
                    return;
                };
                if generation != state.pr_generation {
                    return;
                }
                match wire::checkout_result(result) {
                    Ok(payload) => {
                        state.set_pr_payload(&payload);
                        state.pr_error = None;
                    }
                    // The original query keeps its last data on a failed refetch.
                    Err(failure) => state.pr_error = Some(failure.message),
                }
                state.pr_loading = false;
                state.pr_fetching = false;
                self.changed(cx);
            }
            Pending::Action(run) => self.continue_action(cx, ui, run, result),
            Pending::Archive {
                key: checkout,
                workspace_id,
                failure,
            } => {
                self.archiving.remove(&checkout);
                let outcome = match wire::checkout_result(result) {
                    Ok(payload) => {
                        let rejected = payload.get("error").is_some_and(|e| !e.is_null())
                            || payload.get("success") == Some(&Value::Bool(false))
                            || payload.get("accepted") == Some(&Value::Bool(false));
                        if rejected {
                            Err(error_message(&payload).unwrap_or_else(|| failure.clone()))
                        } else {
                            Ok(())
                        }
                    }
                    Err(error) => Err(wire::message_or(&error, failure.clone())),
                };
                match outcome {
                    Ok(()) => {
                        let closing: Vec<String> = host::tabs(ui)
                            .iter()
                            .filter(|tab| tab.server_id == checkout.0 && tab.workspace_id == workspace_id)
                            .map(|tab| tab.key())
                            .collect();
                        for tab_key in closing {
                            ui.push(UiEffect::CloseTab { tab_key });
                        }
                        ui.core.refresh_host(&checkout.0);
                    }
                    Err(message) => ui.toast(message, true),
                }
                self.changed(cx);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Invalidations.

    /// Original `CheckoutInvalidations.revision`.
    pub fn invalidation(&self, host_id: &str, cwd: &str) -> u64 {
        self.invalidations.get(&key(host_id, cwd)).copied().unwrap_or(0)
    }

    /// Original `invalidateCheckoutGitQueries`: status, PR status, commits,
    /// activity and pipeline of the checkout reload.
    pub fn invalidate(&mut self, cx: &mut Cx, host_id: &str, cwd: &str) {
        let checkout = key(host_id, cwd);
        *self.invalidations.entry(checkout.clone()).or_insert(0) += 1;
        self.reload(cx, &checkout);
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Actions.

    pub fn action_status(&self, host_id: &str, cwd: &str, action_id: &str) -> ActionStatus {
        self.action_status
            .get(&(key(host_id, cwd), action_id.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    pub fn post_ship_archive_suggested(&self, host_id: &str, cwd: &str) -> bool {
        self.post_ship_archive.contains(&key(host_id, cwd))
    }

    pub fn archiving(&self, host_id: &str, cwd: &str) -> bool {
        self.archiving.contains(&key(host_id, cwd))
    }

    fn set_action_status(&mut self, cx: &mut Cx, checkout: &CheckoutKey, action_id: &str, status: ActionStatus) {
        let entry = (checkout.clone(), action_id.to_owned());
        let current = self.action_status.get(&entry).copied().unwrap_or_default();
        if current == status {
            return;
        }
        if status == ActionStatus::Idle {
            self.action_status.remove(&entry);
        } else {
            self.action_status.insert(entry, status);
        }
        self.changed(cx);
    }

    /// Original `CheckoutGitActionsStore.run`: one run per checkout action
    /// (later callers join it), pending → success for one second, and a
    /// finished run invalidates the checkout. `steps` run in order.
    #[allow(clippy::too_many_arguments)]
    pub fn run_action(
        &mut self,
        cx: &mut Cx,
        host_id: &str,
        cwd: &str,
        action_id: &str,
        steps: Vec<(&str, Value)>,
        failure_key: &str,
        success_label: Option<String>,
        follow_up: ActionFollowUp,
    ) {
        self.run_checked(cx, host_id, cwd, action_id, steps, ActionCheck::Git, failure_key, success_label, follow_up);
    }

    /// [`run_action`](Self::run_action) whose steps must answer `success: true`.
    pub fn run_success_action(
        &mut self,
        cx: &mut Cx,
        host_id: &str,
        cwd: &str,
        action_id: &str,
        steps: Vec<(&str, Value)>,
        failure_key: &str,
    ) {
        self.run_checked(
            cx,
            host_id,
            cwd,
            action_id,
            steps,
            ActionCheck::RequireSuccess,
            failure_key,
            None,
            ActionFollowUp::None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn run_checked(
        &mut self,
        cx: &mut Cx,
        host_id: &str,
        cwd: &str,
        action_id: &str,
        steps: Vec<(&str, Value)>,
        check: ActionCheck,
        failure_key: &str,
        success_label: Option<String>,
        follow_up: ActionFollowUp,
    ) {
        let checkout = key(host_id, cwd);
        if self.action_status(host_id, cwd, action_id) == ActionStatus::Pending {
            return;
        }
        self.timers.retain(|(_, job)| match job {
            StoreTimer::ActionIdle { key, action_id: id } => !(key == &checkout && id == action_id),
        });
        let run = ActionRun {
            host: host_id.to_owned(),
            cwd: cwd.to_owned(),
            action_id: action_id.to_owned(),
            steps: steps
                .into_iter()
                .map(|(kind, fields)| (kind.to_owned(), fields))
                .collect(),
            check,
            failure: t(failure_key),
            success_label,
            follow_up,
        };
        self.set_action_status(cx, &checkout, action_id, ActionStatus::Pending);
        self.send_next_step(run);
    }

    fn send_next_step(&mut self, mut run: ActionRun) {
        let Some(core) = self.core.clone() else { return };
        let Some((kind, mut fields)) = run.steps.pop_front() else {
            return;
        };
        if fields.get("cwd").is_none() {
            fields["cwd"] = json!(run.cwd);
        }
        let request = wire::send(&core, &run.host, &kind, fields);
        self.pending.insert(request, Pending::Action(run));
    }

    fn continue_action(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        run: ActionRun,
        result: &Result<Value, paseo_gui_core::CoreError>,
    ) {
        let checkout = key(&run.host, &run.cwd);
        let outcome = match run.check {
            ActionCheck::Git => wire::git_result(result).map(|_| ()),
            ActionCheck::RequireSuccess => match wire::checkout_result(result) {
                Ok(payload) if payload.get("success") == Some(&Value::Bool(true)) => Ok(()),
                Ok(payload) => Err(Failure::message(
                    error_message(&payload).unwrap_or_else(|| run.failure.clone()),
                )),
                Err(failure) => Err(failure),
            },
        };
        match outcome {
            Ok(()) if !run.steps.is_empty() => self.send_next_step(run),
            Ok(()) => {
                self.invalidate(cx, &run.host, &run.cwd);
                self.set_action_status(cx, &checkout, &run.action_id, ActionStatus::Success);
                let timer = cx.start_timeout(1.0);
                self.timers.push((
                    timer,
                    StoreTimer::ActionIdle {
                        key: checkout.clone(),
                        action_id: run.action_id.clone(),
                    },
                ));
                if let ActionFollowUp::SuggestPostShipArchive = run.follow_up
                    && self.post_ship_archive.insert(checkout.clone())
                {
                    self.changed(cx);
                }
                if let Some(label) = run.success_label {
                    ui.toast(label, false);
                }
            }
            Err(failure) => {
                self.set_action_status(cx, &checkout, &run.action_id, ActionStatus::Idle);
                ui.toast(wire::message_or(&failure, run.failure.clone()), true);
            }
        }
    }

    /// Original `useWorkspaceArchive` after confirmation: archive, close the
    /// workspace's tabs and refresh the host list.
    pub fn archive_workspace(&mut self, cx: &mut Cx, ui: &mut UiContext, host_id: &str, cwd: &str, workspace_id: &str) {
        if !host::connected(ui, host_id) {
            ui.toast(t("sidebar.workspace.toasts.hostDisconnected"), true);
            return;
        }
        let checkout = key(host_id, cwd);
        if !self.archiving.insert(checkout.clone()) {
            return;
        }
        let request = wire::send(
            &ui.core,
            host_id,
            "archive_workspace_request",
            json!({ "workspaceId": workspace_id }),
        );
        self.pending.insert(
            request,
            Pending::Archive {
                key: checkout,
                workspace_id: workspace_id.to_owned(),
                failure: t("sidebar.workspace.toasts.archiveFailed"),
            },
        );
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Comparisons (original `WorkingDiffComparisons`).

    pub fn comparison_key(host_id: &str, workspace_id: &str, cwd: &str) -> String {
        let encode = |value: &str| {
            let mut out = String::new();
            for byte in value.bytes() {
                if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            out
        };
        let workspace = workspace_id.trim();
        let trimmed = cwd.trim();
        let normalized = if trimmed == "/" { trimmed } else { trimmed.trim_end_matches('/') };
        let checkout = if workspace.is_empty() {
            format!("cwd={}", encode(normalized))
        } else {
            format!("workspace={}", encode(workspace))
        };
        format!("working-diff:server={}:{checkout}", encode(host_id.trim()))
    }

    pub fn resolve_comparison(&self, host_id: &str, workspace_id: &str, cwd: &str) -> String {
        self.comparisons
            .get(&Self::comparison_key(host_id, workspace_id, cwd))
            .cloned()
            .unwrap_or_else(|| "all".to_owned())
    }

    pub fn select_comparison(&mut self, cx: &mut Cx, host_id: &str, workspace_id: &str, cwd: &str, comparison: &str) {
        let id = Self::comparison_key(host_id, workspace_id, cwd);
        if self.comparisons.get(&id).map(String::as_str) == Some(comparison) {
            return;
        }
        self.comparisons.insert(id, comparison.to_owned());
        self.comparisons_revision += 1;
        self.changed(cx);
    }

    // -----------------------------------------------------------------------
    // Review drafts (original `ReviewDraft` registry).

    pub fn acquire_draft(&mut self, ui: &UiContext, draft_key: &str) {
        let draft = self
            .drafts
            .entry(draft_key.to_owned())
            .or_insert_with(|| ReviewDraft::new(draft_key));
        draft.refs += 1;
        if !draft.loaded {
            draft.load(host::prefs_value(ui, &storage_key(draft_key)));
        }
    }

    pub fn release_draft(&mut self, draft_key: &str) {
        if let Some(draft) = self.drafts.get_mut(draft_key) {
            draft.refs = draft.refs.saturating_sub(1);
            // A send in flight keeps the draft until it finishes.
            if draft.refs == 0 && draft.phase.sending.is_none() {
                self.drafts.remove(draft_key);
            }
        }
    }

    pub fn draft(&self, draft_key: &str) -> Option<&ReviewDraft> {
        self.drafts.get(draft_key)
    }

    /// Mutates a draft and persists it when `edit` reports a change.
    pub fn edit_draft(&mut self, cx: &mut Cx, draft_key: &str, edit: impl FnOnce(&mut ReviewDraft) -> bool) {
        let Some(draft) = self.drafts.get_mut(draft_key) else {
            return;
        };
        if !edit(draft) {
            return;
        }
        if let Some(core) = &self.core {
            core.save_state(&format!("prefs:{}", storage_key(draft_key)), draft.persisted());
        }
        self.changed(cx);
    }

    /// Updates only a draft's feedback phase (never persisted).
    pub fn set_draft_phase(&mut self, cx: &mut Cx, draft_key: &str, sending: Option<usize>, failed: Option<String>) {
        if let Some(draft) = self.drafts.get_mut(draft_key) {
            draft.phase.sending = sending;
            draft.phase.failed = failed;
            if draft.refs == 0 && sending.is_none() {
                self.drafts.remove(draft_key);
            }
            self.changed(cx);
        }
    }

    // -----------------------------------------------------------------------
    // Unsupported timelines (original `defaultUnsupportedTimelineRegistry`).

    pub fn timeline_unsupported(&self, entry: &str) -> bool {
        self.unsupported_timelines.contains(entry)
    }

    pub fn mark_timeline_unsupported(&mut self, entry: String) {
        self.unsupported_timelines.insert(entry);
    }
}

/// The PR status payload's `status` object (or `Null`).
pub fn pr_status(state: Option<&CheckoutState>) -> &Value {
    static NULL: Value = Value::Null;
    match state.and_then(|s| s.pr_payload.as_ref()) {
        Some(payload) => object(payload, "status"),
        None => &NULL,
    }
}
