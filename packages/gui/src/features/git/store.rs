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
use super::json::{array, error_message, flag, object, text};
use super::policy::{ActionStatus, normalize_pr_status_payload};
use super::review::{ReviewDraft, storage_key};
use super::text::{t, tr};
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
    /// The in-flight step stays at the front until its response succeeds.
    steps: VecDeque<(String, Value)>,
    check: ActionCheck,
    failure: String,
    success_label: Option<String>,
    follow_up: ActionFollowUp,
}

impl ActionRun {
    fn accept_response(&mut self, result: &Result<Value, paseo_gui_core::CoreError>) -> Result<(), Failure> {
        let outcome = match self.check {
            ActionCheck::Git => wire::git_result(result).map(|_| ()),
            ActionCheck::RequireSuccess => match wire::checkout_result(result) {
                Ok(payload) if payload.get("success") == Some(&Value::Bool(true)) => Ok(()),
                Ok(payload) => Err(Failure::message(error_message(&payload).unwrap_or_else(|| self.failure.clone()))),
                Err(failure) => Err(failure),
            },
        };
        if outcome.is_ok() {
            self.steps.pop_front();
        }
        outcome
    }
}

/// A checkout's last failed action, kept until an action there succeeds or
/// the user dismisses it.
#[derive(Clone)]
pub struct ActionFailure {
    pub action_id: String,
    /// The action's failure copy ("Failed to push").
    pub title: String,
    /// The host's message (or the title when it gave none).
    pub message: String,
    /// A lost response is not evidence that a mutation did not run.
    pub retryable: bool,
    steps: Vec<(String, Value)>,
    check: ActionCheck,
    success_label: Option<String>,
    follow_up: ActionFollowUp,
}

/// Owner of the review prompts the store opens.
const REVIEW_OWNER: &str = "widget:git-review";

/// What a reviewed Commit or Create PR runs once confirmed.
#[derive(Clone, Debug)]
pub struct ReviewPlan {
    pub host: String,
    pub cwd: String,
    pub success_label: Option<String>,
}

/// A Commit / Create PR review in progress (one per window).
enum Review {
    /// Listing the files the commit includes.
    CommitFiles { plan: ReviewPlan, generation: u64 },
    /// The commit message prompt (empty lets the host write it).
    CommitMessage { plan: ReviewPlan, prompt: u64 },
    /// Title (first line) and description of the pull request.
    PrText { plan: ReviewPlan, base: String, prompt: u64 },
    /// The base branch, prefilled.
    PrBase { plan: ReviewPlan, title: String, body: String, prompt: u64 },
}

impl Review {
    fn prompt(&self) -> Option<u64> {
        match self {
            Review::CommitFiles { .. } => None,
            Review::CommitMessage { prompt, .. } | Review::PrText { prompt, .. } | Review::PrBase { prompt, .. } => Some(*prompt),
        }
    }
}

enum Pending {
    StatusRead { key: CheckoutKey, seq: u64 },
    PrStatus { key: CheckoutKey, generation: u64 },
    Action(ActionRun),
    Archive { key: CheckoutKey, workspace_id: String, failure: String },
    ReviewFiles { generation: u64 },
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
    failures: HashMap<CheckoutKey, ActionFailure>,
    review: Option<Review>,
    review_generation: u64,
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
        if self.review.as_ref().and_then(Review::prompt).is_some() {
            for (id, args) in crate::common::overlay::take_answers(REVIEW_OWNER) {
                self.on_review_answer(cx, ui, id, &args);
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
            Pending::ReviewFiles { generation } => self.on_review_files(cx, ui, generation, result),
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
        let steps = steps.into_iter().map(|(kind, fields)| (kind.to_owned(), fields)).collect();
        self.start_run(cx, host_id, cwd, action_id, steps, check, t(failure_key), success_label, follow_up);
    }

    #[allow(clippy::too_many_arguments)]
    fn start_run(
        &mut self,
        cx: &mut Cx,
        host_id: &str,
        cwd: &str,
        action_id: &str,
        steps: Vec<(String, Value)>,
        check: ActionCheck,
        failure: String,
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
            steps: steps.into(),
            check,
            failure,
            success_label,
            follow_up,
        };
        self.set_action_status(cx, &checkout, action_id, ActionStatus::Pending);
        self.send_next_step(run);
    }

    fn send_next_step(&mut self, run: ActionRun) {
        let Some(core) = self.core.as_ref() else { return };
        let Some((kind, fields)) = run.steps.front() else {
            return;
        };
        let mut fields = fields.clone();
        if fields.get("cwd").is_none() {
            fields["cwd"] = json!(run.cwd);
        }
        let request = wire::send(core, &run.host, kind, fields);
        self.pending.insert(request, Pending::Action(run));
    }

    fn continue_action(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        mut run: ActionRun,
        result: &Result<Value, paseo_gui_core::CoreError>,
    ) {
        let checkout = key(&run.host, &run.cwd);
        let outcome = run.accept_response(result);
        match outcome {
            Ok(()) if !run.steps.is_empty() => self.send_next_step(run),
            Ok(()) => {
                // A success clears the checkout's last failure.
                self.failures.remove(&checkout);
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
                let message = wire::message_or(&failure, run.failure.clone());
                ui.toast(message.clone(), true);
                // Kept on the git actions button until a success or a dismiss.
                self.failures.insert(
                    checkout.clone(),
                    ActionFailure {
                        action_id: run.action_id.clone(),
                        title: run.failure,
                        message,
                        retryable: action_retryable(result),
                        steps: run.steps.into(),
                        check: run.check,
                        success_label: run.success_label,
                        follow_up: run.follow_up,
                    },
                );
                self.set_action_status(cx, &checkout, &run.action_id, ActionStatus::Idle);
                // Earlier steps or a lost response may already have changed Git.
                self.invalidate(cx, &run.host, &run.cwd);
                self.changed(cx);
            }
        }
    }

    /// The checkout's last failed action, until a success or a dismiss.
    pub fn last_failure(&self, host_id: &str, cwd: &str) -> Option<&ActionFailure> {
        self.failures.get(&key(host_id, cwd))
    }

    pub fn dismiss_failure(&mut self, cx: &mut Cx, host_id: &str, cwd: &str) {
        if self.failures.remove(&key(host_id, cwd)).is_some() {
            self.changed(cx);
        }
    }

    /// Retries only unfinished steps, and never an operation with an unknown outcome.
    pub fn retry_failure(&mut self, cx: &mut Cx, host_id: &str, cwd: &str) {
        let Some(failure) = self.last_failure(host_id, cwd) else { return };
        if !failure.retryable || self.action_status(host_id, cwd, &failure.action_id) == ActionStatus::Pending {
            return;
        }
        let ActionFailure { action_id, title, steps, check, success_label, follow_up, .. } = failure.clone();
        self.start_run(cx, host_id, cwd, &action_id, steps, check, title, success_label, follow_up);
    }

    // -----------------------------------------------------------------------
    // Commit / Create PR review.

    fn open_review_prompt(ui: &mut UiContext, title: &str, message: &str, initial: &str, placeholder: &str, confirm: &str, multiline: bool) -> u64 {
        use crate::common::overlay as shared;
        shared::with_owner(ui, REVIEW_OWNER, |ui| {
            if multiline {
                shared::prompt_multiline(ui, title, Some(message), initial, placeholder, confirm)
            } else {
                shared::prompt_text(ui, title, Some(message), initial, placeholder, confirm)
            }
        })
    }

    /// Commit through a review: lists the files it includes, then asks for
    /// the message (empty lets the host write one) before committing.
    pub fn review_commit(&mut self, cx: &mut Cx, plan: ReviewPlan) {
        if self.review.is_some() || self.action_status(&plan.host, &plan.cwd, "commit") == ActionStatus::Pending {
            return;
        }
        let Some(core) = self.core.clone() else { return };
        self.review_generation += 1;
        let generation = self.review_generation;
        let compare = super::model::normalize_compare("uncommitted", "", false, Some("summary"));
        let request = wire::send(&core, &plan.host, "checkout.diff.get.request", json!({ "cwd": plan.cwd, "compare": compare }));
        self.pending.insert(request, Pending::ReviewFiles { generation });
        self.review = Some(Review::CommitFiles { plan, generation });
        self.changed(cx);
    }

    fn take_review_files(&mut self, generation: u64) -> Option<ReviewPlan> {
        if !matches!(self.review.as_ref(), Some(Review::CommitFiles { generation: current, .. }) if *current == generation) {
            return None;
        }
        match self.review.take() {
            Some(Review::CommitFiles { plan, .. }) => Some(plan),
            _ => None,
        }
    }

    fn on_review_files(&mut self, cx: &mut Cx, ui: &mut UiContext, generation: u64, result: &Result<Value, paseo_gui_core::CoreError>) {
        let Some(plan) = self.take_review_files(generation) else { return };
        let listed = wire::checkout_result(result).and_then(|payload| match error_message(&payload) {
            Some(message) => Err(Failure::message(message)),
            None => Ok(array(&payload, "files").iter().map(|f| text(f, "path").to_owned()).collect::<Vec<_>>()),
        });
        let message = match listed {
            Ok(paths) => commit_files_message(&paths),
            Err(failure) => tr("workspace.git.actions.review.filesUnavailable", &[("error", failure.message)]),
        };
        let prompt = Self::open_review_prompt(
            ui,
            &t("workspace.git.actions.review.commitTitle"),
            &message,
            "",
            &t("workspace.git.actions.review.commitPlaceholder"),
            &t("workspace.git.actions.review.commit"),
            true,
        );
        self.review = Some(Review::CommitMessage { plan, prompt });
        self.changed(cx);
    }

    /// Create PR through a review: title and description (empty lets the
    /// host write them), then the base branch, prefilled.
    pub fn review_pr(&mut self, cx: &mut Cx, ui: &mut UiContext, plan: ReviewPlan, base: String, branch: String) {
        if self.review.is_some() || self.action_status(&plan.host, &plan.cwd, "create-pr") == ActionStatus::Pending {
            return;
        }
        let message = tr("workspace.git.actions.review.prMessage", &[("base", base.clone()), ("branch", branch)]);
        let prompt = Self::open_review_prompt(
            ui,
            &t("workspace.git.actions.review.prTitle"),
            &message,
            "",
            &t("workspace.git.actions.review.prPlaceholder"),
            &t("workspace.git.actions.review.next"),
            true,
        );
        self.review = Some(Review::PrText { plan, base, prompt });
        self.changed(cx);
    }

    fn take_review_answer(&mut self, id: u64, args: &Value) -> Option<(Review, Option<String>)> {
        if self.review.as_ref().and_then(Review::prompt) != Some(id) {
            return None;
        }
        let review = self.review.take()?;
        let answer = if flag(args, "cancelled") { None } else { args.get("text").and_then(Value::as_str).map(str::to_owned) };
        Some((review, answer))
    }

    fn on_review_answer(&mut self, cx: &mut Cx, ui: &mut UiContext, id: u64, args: &Value) {
        let Some((review, answer)) = self.take_review_answer(id, args) else { return };
        self.changed(cx);
        // Cancel (or a dismissed prompt) ends the review without running anything.
        let Some(answer) = answer else { return };
        match review {
            Review::CommitFiles { .. } => {}
            Review::CommitMessage { plan, .. } => {
                let mut fields = json!({ "addAll": true });
                let message = answer.trim();
                if !message.is_empty() {
                    fields["message"] = json!(message);
                }
                let steps = vec![("checkout_commit_request".to_owned(), fields)];
                let failure = t("workspace.git.actions.toasts.failedCommit");
                self.start_run(cx, &plan.host, &plan.cwd, "commit", steps, ActionCheck::Git, failure, plan.success_label, ActionFollowUp::None);
            }
            Review::PrText { plan, base, .. } => {
                let (title, body) = split_title_body(&answer);
                let prompt = Self::open_review_prompt(
                    ui,
                    &t("workspace.git.actions.review.baseTitle"),
                    &t("workspace.git.actions.review.baseMessage"),
                    &base,
                    &t("workspace.git.actions.review.basePlaceholder"),
                    &t("workspace.git.actions.review.createPr"),
                    false,
                );
                self.review = Some(Review::PrBase { plan, title, body, prompt });
            }
            Review::PrBase { plan, title, body, .. } => {
                let mut fields = json!({});
                if !title.is_empty() {
                    fields["title"] = json!(title);
                }
                if !body.is_empty() {
                    fields["body"] = json!(body);
                }
                let base = answer.trim();
                if !base.is_empty() {
                    fields["baseRef"] = json!(base);
                }
                let steps = vec![("checkout_pr_create_request".to_owned(), fields)];
                let failure = t("workspace.git.actions.toasts.failedCreatePr");
                self.start_run(cx, &plan.host, &plan.cwd, "create-pr", steps, ActionCheck::Git, failure, plan.success_label, ActionFollowUp::None);
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

fn action_retryable(result: &Result<Value, paseo_gui_core::CoreError>) -> bool {
    match result {
        Err(error) => !error.outcome_unknown,
        Ok(payload) => paseo_gui_core::response_error(payload, "git action").is_none_or(|error| !error.outcome_unknown),
    }
}

/// Files listed in the commit review (the rest counted).
const REVIEW_FILES_SHOWN: usize = 12;

/// The commit review's body: how many files the commit includes and their paths.
fn commit_files_message(paths: &[String]) -> String {
    if paths.is_empty() {
        return t("workspace.git.actions.review.noFiles");
    }
    let mut lines = vec![tr("workspace.git.actions.review.files", &[("count", paths.len().to_string())])];
    lines.extend(paths.iter().take(REVIEW_FILES_SHOWN).map(|path| format!("• {path}")));
    if paths.len() > REVIEW_FILES_SHOWN {
        lines.push(tr("workspace.git.actions.review.moreFiles", &[("count", (paths.len() - REVIEW_FILES_SHOWN).to_string())]));
    }
    lines.join("\n")
}

/// A pull request's title (the first non-empty line) and description (the rest).
fn split_title_body(text: &str) -> (String, String) {
    let text = text.trim();
    match text.split_once('\n') {
        Some((title, body)) => (title.trim().to_owned(), body.trim().to_owned()),
        None => (text.to_owned(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_plan() -> ReviewPlan {
        ReviewPlan { host: "host".into(), cwd: "/repo".into(), success_label: None }
    }

    #[test]
    fn cancelled_review_never_submits_and_empty_confirm_is_distinct() {
        let mut store = GitStore::default();
        for review in [
            Review::CommitMessage { plan: review_plan(), prompt: 7 },
            Review::PrText { plan: review_plan(), base: "main".into(), prompt: 7 },
            Review::PrBase { plan: review_plan(), title: "Title".into(), body: String::new(), prompt: 7 },
        ] {
            store.review = Some(review);
            let (_, answer) = store.take_review_answer(7, &json!({ "cancelled": true, "text": "" })).unwrap();
            assert!(answer.is_none());
            assert!(store.review.is_none());
            assert!(store.take_review_answer(7, &json!({ "text": "duplicate" })).is_none());
        }
        store.review = Some(Review::CommitMessage { plan: review_plan(), prompt: 8 });
        assert!(store.take_review_answer(7, &json!({ "text": "stale" })).is_none());
        let (_, answer) = store.take_review_answer(8, &json!({ "text": "" })).unwrap();
        assert_eq!(answer.as_deref(), Some(""));
    }

    #[test]
    fn stale_file_list_keeps_the_current_review() {
        let mut store = GitStore::default();
        store.review = Some(Review::PrText { plan: review_plan(), base: "main".into(), prompt: 7 });
        assert!(store.take_review_files(1).is_none());
        assert_eq!(store.review.as_ref().and_then(Review::prompt), Some(7));
        store.review = Some(Review::CommitFiles { plan: review_plan(), generation: 2 });
        assert!(store.take_review_files(1).is_none());
        assert!(store.take_review_files(2).is_some());
        assert!(store.review.is_none());
    }

    #[test]
    fn failed_action_retains_only_unfinished_steps() {
        let mut run = ActionRun {
            host: "host".into(),
            cwd: "/repo".into(),
            action_id: "pull-and-push".into(),
            steps: VecDeque::from([
                ("checkout_pull_request".into(), json!({})),
                ("checkout_push_request".into(), json!({})),
            ]),
            check: ActionCheck::Git,
            failure: "Failed".into(),
            success_label: None,
            follow_up: ActionFollowUp::None,
        };
        assert!(run.accept_response(&Ok(json!({ "success": true }))).is_ok());
        assert!(run.accept_response(&Ok(json!({ "error": { "message": "Push rejected" } }))).is_err());
        assert_eq!(run.steps.len(), 1);
        assert_eq!(run.steps.front().unwrap().0, "checkout_push_request");
        assert!(run.accept_response(&Ok(json!({ "success": true }))).is_ok());
        assert!(run.steps.is_empty());
    }

    #[test]
    fn retry_refuses_unknown_transport_and_server_outcomes() {
        let error = paseo_gui_core::CoreError { code: "DAEMON_REQUEST_TIMEOUT".into(), message: "Timed out".into(), outcome_unknown: true };
        assert!(!action_retryable(&Err(error.clone())));
        assert!(action_retryable(&Err(paseo_gui_core::CoreError { outcome_unknown: false, ..error })));
        assert!(!action_retryable(&Ok(json!({ "success": false, "outcomeUnknown": true }))));
        assert!(action_retryable(&Ok(json!({ "success": false }))));
    }

    #[test]
    fn pull_request_text_splits_title_and_body() {
        assert_eq!(split_title_body("  Fix login\n\nThe form now validates.\nTwo lines. "), ("Fix login".to_owned(), "The form now validates.\nTwo lines.".to_owned()));
        assert_eq!(split_title_body("Only a title"), ("Only a title".to_owned(), String::new()));
        assert_eq!(split_title_body("  \n "), (String::new(), String::new()));
    }
}
