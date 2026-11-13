//! One Changes / Diff / commit view's repository state (original
//! git_controller.dart `GitController`).
//!
//! The summary comes from an owned `subscribe_checkout_diff_request`
//! observation (`detail: summary`); patch bodies load lazily per file: a
//! working diff shares one `checkout.diff.get.request` (`detail: full`) per
//! patch revision, a commit asks `checkout.commits.file_diff.request` per
//! file. `generation` and `patch_revision` reject stale results.
//! A commit outside the workspace's commit list (opened from the Git Log)
//! reads its files from `checkout.commit.get.request` when the host has `gitLog`.
//! Successful checkout actions also invalidate working patches: a changed
//! body can keep the same summary counts and produce no subscription update.

use super::host;
use super::json::{array, error_message, flag, object, text};
use super::model::normalize_compare;
use super::store::GitStore;
use super::wire::{self, Failure};
use crate::common::{PanelTarget, UiContext};
use paseo_gui_core::{CoreError, CoreEvent, RequestId};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

const PATCH_UNAVAILABLE: &str = "This file changed since the summary. Refresh to load its current patch.";
const HOST_DISCONNECTED: &str = "Host disconnected. Repository state is cached until reconnection.";
const UPDATE_FOR_ALL: &str = "Update the host to view All changes";

#[derive(Clone, Copy, Debug)]
enum StatusPurpose {
    Refresh { generation: u64, status_revision: u64, resume: bool },
    Reload { connection: u64, revision: u64 },
}

#[derive(Clone, Debug)]
enum StatusSource {
    Shared(u64),
    Direct(RequestId),
}

#[derive(Clone, Debug)]
struct StatusWait {
    source: StatusSource,
    purpose: StatusPurpose,
}

#[derive(Clone, Copy, Debug)]
enum CommitsOrigin {
    /// The commit view's refresh continues with the commit's files.
    Refresh { generation: u64, resume: bool },
    /// `_reloadStatus` awaits the list inside its try.
    Reload { connection: u64 },
    /// The footer toggle reports its own loading/error.
    Footer,
    /// A push or shown-again reload whose failure is ignored.
    Quiet,
}

#[derive(Clone, Copy, Debug)]
enum PatchWaiter {
    /// Scrolled into view: failures stay per file.
    Ensure,
    /// Selected: failures are the selection's.
    Select(u64),
}

#[derive(Clone, Debug)]
struct FullWaiter {
    path: String,
    generation: u64,
    revision: u64,
    kind: PatchWaiter,
}

enum Pending {
    Commits { connection: u64, origin: CommitsOrigin },
    FileDiff { path: String, generation: u64, revision: u64, kind: PatchWaiter },
    FullDiff,
    Observe { generation: u64 },
    /// The commit's files when it is not in `checkout.commits.list`.
    CommitGet { generation: u64, resume: bool },
}

pub struct DiffController {
    pub host: String,
    pub workspace_id: String,
    pub cwd: String,
    pub is_commit: bool,
    pub sha: String,
    /// The commit's subject once its files loaded (commit views).
    pub subject: Option<String>,
    active: bool,
    stale: bool,
    patch_loads_deferred: bool,
    commits_stale: bool,
    commits_loaded: bool,
    seen_invalidation: u64,
    seen_comparisons: u64,
    connection_revision: u64,
    status_revision: u64,
    pub status: Option<Value>,
    pub files: Vec<Value>,
    pub commits: Vec<Value>,
    pub patches: HashMap<String, Value>,
    pub loading_patches: HashSet<String>,
    pub patch_errors: HashMap<String, String>,
    pub mode: String,
    pub base_ref: String,
    pub selected_path: String,
    pub ignore_whitespace: bool,
    pub loading: bool,
    pub patch_loading: bool,
    pub too_large: bool,
    pub error: Option<String>,
    /// The diff snapshot's own error (original `diffPayloadError`).
    pub diff_error: Option<String>,
    pub patch_error: Option<String>,
    pub commits_loading: bool,
    pub commits_error: Option<String>,
    generation: u64,
    /// Bumped whenever cached patches and in-flight loads are invalidated;
    /// placeholders keyed by it request again.
    pub patch_revision: u64,
    selection: u64,
    full_request: Option<RequestId>,
    full_waiters: Vec<FullWaiter>,
    observation: Option<RequestId>,
    status_waits: Vec<StatusWait>,
    pending: HashMap<RequestId, Pending>,
    /// Set whenever anything visible changed; the panel redraws and clears it.
    pub dirty: bool,
}

impl DiffController {
    /// A view of `target` (`working_diff`, `changes_tree` or `commit_diff`).
    pub fn new(target: &PanelTarget, store: &GitStore) -> Self {
        let is_commit = target.kind == "commit_diff";
        let selected = match target.data.get("focusPath").or_else(|| target.data.get("path")).and_then(Value::as_str) {
            Some(path) => path.to_owned(),
            None => String::new(),
        };
        let sha = match target.data.get("sha").and_then(Value::as_str) {
            Some(sha) => sha.to_owned(),
            None => target.id.clone().unwrap_or_default(),
        };
        Self {
            host: target.server_id.clone(),
            workspace_id: target.workspace_id.clone(),
            cwd: target.cwd.clone(),
            is_commit,
            sha,
            subject: None,
            active: true,
            stale: false,
            patch_loads_deferred: false,
            commits_stale: false,
            commits_loaded: false,
            seen_invalidation: store.invalidation(&target.server_id, &target.cwd),
            seen_comparisons: store.comparisons_revision,
            connection_revision: 0,
            status_revision: 0,
            status: None,
            files: Vec::new(),
            commits: Vec::new(),
            patches: HashMap::new(),
            loading_patches: HashSet::new(),
            patch_errors: HashMap::new(),
            mode: if is_commit {
                "all".into()
            } else {
                store.resolve_comparison(&target.server_id, &target.workspace_id, &target.cwd)
            },
            base_ref: String::new(),
            selected_path: selected,
            ignore_whitespace: false,
            loading: false,
            patch_loading: false,
            too_large: false,
            error: None,
            diff_error: None,
            patch_error: None,
            commits_loading: false,
            commits_error: None,
            generation: 0,
            patch_revision: 0,
            selection: 0,
            full_request: None,
            full_waiters: Vec::new(),
            observation: None,
            status_waits: Vec::new(),
            pending: HashMap::new(),
            dirty: true,
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    fn compare(&self) -> Value {
        normalize_compare(&self.mode, &self.base_ref, self.ignore_whitespace, None)
    }

    fn compare_with(&self, detail: &str) -> Value {
        let mut compare = self.compare();
        compare["detail"] = json!(detail);
        compare
    }

    /// Releases the observation and every read this view waits on.
    pub fn dispose(&mut self, ui_core: &paseo_gui_core::CoreHandle, store: &mut GitStore) {
        self.generation += 1;
        if let Some(observation) = self.observation.take() {
            ui_core.cancel_request(&observation);
        }
        for (request, pending) in self.pending.drain() {
            if let Pending::Observe { .. } = pending {
                ui_core.cancel_request(&request);
            }
        }
        self.abandon_status_waits(store);
    }

    fn abandon_status_waits(&mut self, store: &mut GitStore) {
        for wait in self.status_waits.drain(..) {
            if let StatusSource::Shared(seq) = wait.source {
                store.abandon_read(&self.host, &self.cwd, seq);
            }
        }
    }

    fn release_observation(&mut self, ui: &UiContext) {
        if let Some(observation) = self.observation.take() {
            ui.core.cancel_request(&observation);
        }
    }

    fn abandon_loads(&mut self) {
        self.full_request = None;
        self.full_waiters.clear();
        self.loading_patches.clear();
        self.loading = false;
        self.patch_loading = false;
    }

    /// Original `_missed`: reload when shown, including a loaded commit list.
    fn missed(&mut self) {
        self.stale = true;
        if self.commits_loaded {
            self.commits_stale = true;
        }
    }

    /// Original `active` setter: a hidden view keeps its cached state but holds
    /// no subscription and issues no requests; what it missed reloads once.
    pub fn set_active(&mut self, ui: &UiContext, store: &mut GitStore, value: bool) {
        if value == self.active {
            return;
        }
        self.active = value;
        self.dirty = true;
        if !value {
            self.stale = self.stale || self.loading || !self.is_commit;
            if self.patch_loading || !self.loading_patches.is_empty() {
                self.patch_loads_deferred = true;
            }
            self.generation += 1;
            self.release_observation(ui);
            self.abandon_status_waits(store);
            self.abandon_loads();
            return;
        }
        let deferred = self.patch_loads_deferred;
        let commits = self.commits_stale;
        self.patch_loads_deferred = false;
        self.commits_stale = false;
        if self.stale {
            self.stale = false;
            self.refresh(ui, store, true);
        } else if deferred {
            self.patch_revision += 1;
        }
        if commits && !self.is_commit {
            self.load_commits(ui, CommitsOrigin::Quiet);
        }
    }

    /// The shared whitespace preference keys the diff request.
    pub fn set_ignore_whitespace(&mut self, ui: &UiContext, store: &mut GitStore, value: bool) {
        if self.is_commit || self.ignore_whitespace == value {
            return;
        }
        self.ignore_whitespace = value;
        self.refresh(ui, store, false);
    }

    /// Starts a status read: the checkout's shared read for working diffs, a
    /// direct request for commit views.
    fn read_status(&mut self, ui: &UiContext, store: &mut GitStore, purpose: StatusPurpose) {
        let source = if self.is_commit {
            StatusSource::Direct(wire::send(&ui.core, &self.host, "checkout_status_request", json!({ "cwd": self.cwd })))
        } else {
            match store.read_status(&self.host, &self.cwd) {
                Some(seq) => StatusSource::Shared(seq),
                None => StatusSource::Direct(wire::send(
                    &ui.core,
                    &self.host,
                    "checkout_status_request",
                    json!({ "cwd": self.cwd }),
                )),
            }
        };
        self.status_waits.push(StatusWait { source, purpose });
    }

    /// Original `refresh`: reloads status and resubscribes. A `resume` keeps
    /// the cached patches until the new snapshot replaces them.
    pub fn refresh(&mut self, ui: &UiContext, store: &mut GitStore, resume: bool) {
        if !self.active {
            self.stale = true;
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        self.patch_error = None;
        if !resume {
            self.patches.clear();
            self.patch_errors.clear();
            self.patch_revision += 1;
        }
        self.loading_patches.clear();
        self.full_request = None;
        self.full_waiters.clear();
        self.patch_loading = false;
        self.dirty = true;
        let status_revision = self.status_revision;
        self.read_status(ui, store, StatusPurpose::Refresh { generation, status_revision, resume });
    }

    /// Original `_reloadStatus`.
    fn reload_status(&mut self, ui: &UiContext, store: &mut GitStore) {
        self.error = None;
        self.loading = true;
        self.dirty = true;
        let purpose = StatusPurpose::Reload {
            connection: self.connection_revision,
            revision: self.status_revision,
        };
        self.read_status(ui, store, purpose);
    }

    fn on_status(&mut self, ui: &UiContext, purpose: StatusPurpose, result: Result<Value, Failure>) {
        // The status payload carries domain failures, reported as errors.
        let result = result.and_then(|payload| {
            if let Some(message) = error_message(&payload) {
                return Err(Failure::message(message));
            }
            if payload.get("success") == Some(&Value::Bool(false))
                || payload.get("accepted") == Some(&Value::Bool(false))
            {
                return Err(Failure::message("The host did not accept this operation."));
            }
            Ok(payload)
        });
        self.dirty = true;
        match purpose {
            StatusPurpose::Refresh { generation, status_revision, resume } => {
                if generation != self.generation {
                    return;
                }
                let next = match result {
                    Ok(next) => next,
                    Err(failure) => {
                        self.error = Some(failure.message);
                        self.loading = false;
                        return;
                    }
                };
                if status_revision == self.status_revision {
                    self.status = Some(next);
                }
                let current = self.status.clone().unwrap_or(Value::Null);
                if !self.is_commit {
                    self.base_ref = text(&current, "baseRef").to_owned();
                }
                if !flag(&current, "isGit") {
                    self.loading = false;
                    return;
                }
                if self.is_commit {
                    self.load_commits(ui, CommitsOrigin::Refresh { generation, resume });
                } else if self.mode == "all" && !host::supports(ui, &self.host, "checkoutDiffAll") {
                    // COMPAT(checkoutDiffAll): hosts without All changes report an update prompt.
                    self.release_observation(ui);
                    self.summary(&json!({
                        "files": [],
                        "error": { "code": "UNKNOWN", "message": UPDATE_FOR_ALL },
                    }));
                    self.finish_refresh(ui);
                } else {
                    self.release_observation(ui);
                    let request = ui.core.observe(
                        &self.host,
                        json!({
                            "type": "subscribe_checkout_diff_request",
                            "cwd": self.cwd,
                            "compare": self.compare_with("summary"),
                        }),
                        "subscribe_checkout_diff_response",
                    );
                    self.pending.insert(request, Pending::Observe { generation });
                }
            }
            StatusPurpose::Reload { connection, revision } => {
                if connection != self.connection_revision {
                    return;
                }
                match result {
                    Ok(next) => {
                        if revision == self.status_revision {
                            self.status = Some(next);
                        }
                        if self.commits_loaded {
                            if self.active {
                                self.load_commits(ui, CommitsOrigin::Reload { connection });
                                return;
                            }
                            self.missed();
                        }
                    }
                    Err(failure) => self.error = Some(failure.message),
                }
                self.loading = false;
            }
        }
    }

    /// The end of a successful refresh: the selected file reloads.
    fn finish_refresh(&mut self, ui: &UiContext) {
        if !self.selected_path.is_empty() {
            let path = self.selected_path.clone();
            self.select_file(ui, &path);
        }
        self.loading = false;
        self.dirty = true;
    }

    fn invalidate_patches(&mut self) {
        self.patches.clear();
        self.loading_patches.clear();
        self.patch_errors.clear();
        self.patch_revision += 1;
        self.full_request = None;
        self.full_waiters.clear();
        self.dirty = true;
    }

    /// Original `_summary`.
    fn summary(&mut self, payload: &Value) {
        self.diff_error = error_message(payload);
        self.files = array(payload, "files").to_vec();
        self.too_large = flag(payload, "diffTooLarge");
        self.invalidate_patches();
    }

    /// Original `loadCommits`.
    fn load_commits(&mut self, ui: &UiContext, origin: CommitsOrigin) {
        self.commits_loaded = true;
        let request = wire::send(&ui.core, &self.host, "checkout.commits.list.request", json!({ "cwd": self.cwd }));
        self.pending.insert(
            request,
            Pending::Commits {
                connection: self.connection_revision,
                origin,
            },
        );
    }

    /// The commits footer opened: load the list with its own state.
    pub fn load_footer_commits(&mut self, ui: &UiContext) {
        self.commits_loading = true;
        self.commits_error = None;
        self.dirty = true;
        self.load_commits(ui, CommitsOrigin::Footer);
    }

    fn on_commits(&mut self, ui: &UiContext, connection: u64, origin: CommitsOrigin, result: Result<Value, Failure>) {
        self.dirty = true;
        let current = connection == self.connection_revision;
        if current && let Ok(payload) = &result {
            self.commits = array(payload, "commits").to_vec();
        }
        match origin {
            CommitsOrigin::Footer => {
                if let Err(failure) = &result {
                    self.commits_error = Some(failure.message.clone());
                }
                self.commits_loading = false;
            }
            CommitsOrigin::Quiet => {}
            CommitsOrigin::Reload { connection } => {
                if connection != self.connection_revision {
                    return;
                }
                if let Err(failure) = result {
                    self.error = Some(failure.message);
                }
                self.loading = false;
            }
            CommitsOrigin::Refresh { generation, resume } => {
                if generation != self.generation {
                    return;
                }
                if let Err(failure) = result {
                    self.error = Some(failure.message);
                    self.loading = false;
                    return;
                }
                let Some(commit) = self.commits.iter().find(|c| text(c, "sha") == self.sha) else {
                    if host::supports(ui, &self.host, "gitLog") {
                        let request = wire::send(
                            &ui.core,
                            &self.host,
                            "checkout.commit.get.request",
                            json!({ "cwd": self.cwd, "sha": self.sha }),
                        );
                        self.pending.insert(request, Pending::CommitGet { generation, resume });
                    } else {
                        self.finish_commit(ui, None, Vec::new(), resume);
                    }
                    return;
                };
                let subject = Some(text(commit, "subject").to_owned());
                let files = array(commit, "files").to_vec();
                self.finish_commit(ui, subject, files, resume);
            }
        }
    }

    /// A commit view's files arrived (from the commit list or `checkout.commit.get`).
    fn finish_commit(&mut self, ui: &UiContext, subject: Option<String>, files: Vec<Value>, resume: bool) {
        self.subject = subject.filter(|s| !s.is_empty());
        self.files = files;
        if self.files.is_empty() && !self.selected_path.is_empty() {
            self.files = vec![json!({ "path": self.selected_path })];
        }
        // A commit's patches never change; placeholders left by
        // interrupted loads request again.
        if resume {
            self.patch_revision += 1;
        }
        self.finish_refresh(ui);
    }

    /// Original `selectFile`.
    pub fn select_file(&mut self, ui: &UiContext, path: &str) {
        self.selected_path = path.to_owned();
        self.patch_error = None;
        self.selection += 1;
        self.dirty = true;
        if !self.active {
            self.patch_loads_deferred = true;
            return;
        }
        if self.patches.contains_key(path) {
            self.patch_loading = false;
            return;
        }
        self.patch_loading = true;
        let waiter = PatchWaiter::Select(self.selection);
        self.fetch(ui, path, waiter);
    }

    /// Original `ensurePatch`: loads one file's patch without selecting it,
    /// for files scrolled near the viewport.
    pub fn ensure_patch(&mut self, ui: &UiContext, path: &str) {
        if self.patches.contains_key(path) || self.loading_patches.contains(path) {
            return;
        }
        if !self.active {
            self.patch_loads_deferred = true;
            return;
        }
        self.loading_patches.insert(path.to_owned());
        self.patch_errors.remove(path);
        self.dirty = true;
        self.fetch(ui, path, PatchWaiter::Ensure);
    }

    fn fetch(&mut self, ui: &UiContext, path: &str, kind: PatchWaiter) {
        let generation = self.generation;
        let revision = self.patch_revision;
        if self.is_commit {
            let request = wire::send(
                &ui.core,
                &self.host,
                "checkout.commits.file_diff.request",
                json!({ "cwd": self.cwd, "sha": self.sha, "path": path }),
            );
            self.pending.insert(
                request,
                Pending::FileDiff {
                    path: path.to_owned(),
                    generation,
                    revision,
                    kind,
                },
            );
            return;
        }
        // Working patches share one full-diff request, deferred until a file opens.
        if self.full_request.is_none() {
            let request = wire::send(
                &ui.core,
                &self.host,
                "checkout.diff.get.request",
                json!({ "cwd": self.cwd, "compare": self.compare_with("full") }),
            );
            self.pending.insert(request.clone(), Pending::FullDiff);
            self.full_request = Some(request);
        }
        self.full_waiters.push(FullWaiter {
            path: path.to_owned(),
            generation,
            revision,
            kind,
        });
    }

    fn finish_patch(&mut self, path: &str, kind: PatchWaiter, outcome: Result<(), String>) {
        match kind {
            PatchWaiter::Ensure => {
                match outcome {
                    Ok(()) => {
                        if !self.too_large && !self.patches.contains_key(path) {
                            // The host omitted this file: it changed after the summary.
                            self.patch_errors.insert(path.to_owned(), PATCH_UNAVAILABLE.to_owned());
                        }
                    }
                    Err(message) => {
                        self.patch_errors.insert(path.to_owned(), message);
                    }
                }
                self.loading_patches.remove(path);
            }
            PatchWaiter::Select(selection) => {
                if selection != self.selection {
                    return;
                }
                if let Err(message) = outcome {
                    self.patch_error = Some(message);
                }
                self.patch_loading = false;
            }
        }
        self.dirty = true;
    }

    fn on_full_diff(&mut self, result: Result<Value, Failure>) {
        let waiters = std::mem::take(&mut self.full_waiters);
        self.full_request = None;
        let current: Vec<FullWaiter> = waiters
            .into_iter()
            .filter(|w| w.generation == self.generation && w.revision == self.patch_revision)
            .collect();
        if current.is_empty() {
            return;
        }
        let outcome = match result {
            Err(failure) => Err(failure.message),
            // The host omits every file; the body shows the too-large state.
            Ok(payload) if flag(&payload, "diffTooLarge") => {
                self.too_large = true;
                Ok(())
            }
            Ok(payload) => match error_message(&payload) {
                Some(message) => Err(message),
                None => {
                    for file in array(&payload, "files") {
                        self.patches.insert(text(file, "path").to_owned(), file.clone());
                    }
                    Ok(())
                }
            },
        };
        for waiter in current {
            self.finish_patch(&waiter.path, waiter.kind, outcome.clone());
        }
    }

    /// Applies this view's share of a core batch and the store's revisions.
    /// Returns true when anything visible changed.
    pub fn process(&mut self, ui: &UiContext, store: &mut GitStore) -> bool {
        for event in &ui.events {
            match event {
                CoreEvent::Connected { host_id, .. } if *host_id == self.host => {
                    self.connection_revision += 1;
                    self.dirty = true;
                    if !self.active {
                        self.missed();
                    } else if self.observation.is_none() {
                        self.refresh(ui, store, false);
                    } else {
                        self.reload_status(ui, store);
                    }
                }
                CoreEvent::Disconnected { host_id, .. } if *host_id == self.host => {
                    self.connection_revision += 1;
                    self.patch_revision += 1;
                    self.full_request = None;
                    self.full_waiters.clear();
                    self.patches.clear();
                    self.loading = false;
                    self.patch_loading = false;
                    self.error = Some(HOST_DISCONNECTED.to_owned());
                    self.dirty = true;
                }
                CoreEvent::HostEvent { host_id, event } if *host_id == self.host => {
                    self.on_host_event(ui, store, event);
                }
                CoreEvent::Response { request_id, result, .. } => {
                    if let Some(pending) = self.pending.remove(request_id) {
                        self.on_response(ui, request_id, pending, result);
                    } else if let Some(index) = self
                        .status_waits
                        .iter()
                        .position(|w| matches!(&w.source, StatusSource::Direct(id) if id == request_id))
                    {
                        let wait = self.status_waits.remove(index);
                        self.on_status(ui, wait.purpose, wire::checkout_result(result));
                    }
                }
                _ => {}
            }
        }
        // Shared status reads that settled.
        let waits = std::mem::take(&mut self.status_waits);
        let mut ready = Vec::new();
        for wait in waits {
            let settled = match &wait.source {
                StatusSource::Shared(seq) => store.take_read(&self.host, &self.cwd, *seq),
                StatusSource::Direct(_) => None,
            };
            match settled {
                Some(result) => ready.push((wait.purpose, result)),
                None => self.status_waits.push(wait),
            }
        }
        for (purpose, result) in ready {
            self.on_status(ui, purpose, result);
        }
        // A finished action invalidated the checkout.
        let invalidation = store.invalidation(&self.host, &self.cwd);
        if invalidation != self.seen_invalidation {
            self.seen_invalidation = invalidation;
            if self.active {
                self.reload_status(ui, store);
                if !self.is_commit {
                    // The action response arrives after the host invalidates
                    // its diff cache; unchanged summaries need not be pushed.
                    self.invalidate_patches();
                    if !self.selected_path.is_empty() {
                        let path = self.selected_path.clone();
                        self.select_file(ui, &path);
                    }
                }
            } else {
                self.missed();
            }
        }
        // The workspace's comparison changed in another view.
        if !self.is_commit && store.comparisons_revision != self.seen_comparisons {
            self.seen_comparisons = store.comparisons_revision;
            let next = store.resolve_comparison(&self.host, &self.workspace_id, &self.cwd);
            if next != self.mode {
                self.mode = next;
                self.refresh(ui, store, false);
            }
        }
        std::mem::take(&mut self.dirty)
    }

    fn on_host_event(&mut self, ui: &UiContext, store: &mut GitStore, event: &Value) {
        let kind = text(event, "type");
        let payload = &event["payload"];
        if kind == "checkout_status_update" && text(payload, "cwd") == self.cwd {
            self.status_revision += 1;
            let previous_base = self.status.as_ref().map(|s| text(s, "baseRef").to_owned());
            self.status = Some(payload.clone());
            self.dirty = true;
            // Original applyCheckoutStatusUpdateFromEvent invalidates the commit list.
            if self.commits_loaded {
                if self.active {
                    self.load_commits(ui, CommitsOrigin::Quiet);
                } else {
                    self.missed();
                }
            }
            // The diff is keyed by the status base ref.
            let base = text(payload, "baseRef");
            if !self.is_commit && previous_base.is_some_and(|previous| previous != base) {
                self.base_ref = base.to_owned();
                self.refresh(ui, store, false);
            }
            return;
        }
        let Some(observation) = &self.observation else {
            return;
        };
        let observed = event
            .get("observations")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(observation.as_str())))
            || payload.get("observationId").and_then(Value::as_str) == Some(observation.as_str());
        if !observed {
            return;
        }
        if kind == "subscription.error" {
            self.error = Some(text(payload, "error").to_owned());
            self.dirty = true;
            return;
        }
        self.summary(payload);
        if kind == "subscription.restored" && !self.selected_path.is_empty() {
            let path = self.selected_path.clone();
            self.select_file(ui, &path);
        }
    }

    fn on_response(
        &mut self,
        ui: &UiContext,
        request_id: &RequestId,
        pending: Pending,
        result: &Result<Value, CoreError>,
    ) {
        match pending {
            Pending::Commits { connection, origin } => {
                self.on_commits(ui, connection, origin, wire::git_result(result));
            }
            Pending::FileDiff { path, generation, revision, kind } => {
                if generation != self.generation || revision != self.patch_revision {
                    return;
                }
                let outcome = match wire::git_result(result) {
                    Ok(payload) => {
                        let file = object(&payload, "file");
                        let patch = if file.is_null() {
                            json!({ "path": path, "hunks": [], "status": "binary" })
                        } else {
                            file.clone()
                        };
                        self.patches.insert(path.clone(), patch);
                        Ok(())
                    }
                    Err(failure) => Err(failure.message),
                };
                self.finish_patch(&path, kind, outcome);
            }
            Pending::CommitGet { generation, resume } => {
                if generation != self.generation {
                    return;
                }
                self.dirty = true;
                // A failed read keeps the original fallback (the focused path only).
                let (subject, files) = match wire::git_result(result) {
                    Ok(payload) => {
                        let commit = object(&payload, "commit");
                        (Some(text(commit, "subject").to_owned()), array(commit, "files").to_vec())
                    }
                    Err(_) => (None, Vec::new()),
                };
                self.finish_commit(ui, subject, files, resume);
            }
            Pending::FullDiff => {
                if self.full_request.as_ref() != Some(request_id) {
                    return;
                }
                self.on_full_diff(wire::checkout_result(result));
            }
            Pending::Observe { generation } => {
                if generation != self.generation {
                    // A refresh or hide replaced this subscription.
                    ui.core.cancel_request(request_id);
                    return;
                }
                match result {
                    Ok(snapshot) => {
                        self.observation = Some(request_id.clone());
                        self.summary(snapshot);
                        self.finish_refresh(ui);
                    }
                    Err(error) => {
                        self.error = Some(Failure::from(error).message);
                        self.loading = false;
                        self.dirty = true;
                    }
                }
            }
        }
    }
}
