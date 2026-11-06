//! GUI-thread ownership and IO dispatch for the files feature.
//!
//! * File sessions are keyed by an immutable id and owned by a tab key (`owner`). A tab that is
//!   re-targeted to another host/cwd/path releases its old session — a modified buffer is still
//!   written to the file it was edited in — and opens a new one; no request result is ever
//!   applied to another identity (Flutter `FilePanel.didUpdateWidget` + `release`).
//! * Explorer trees are keyed by owner, so a hidden/resized/remounted explorer keeps its tree.
//! * Every daemon request is correlated by its core `RequestId`; results for released or replaced
//!   owners are dropped (stale generation guards live in the handle/explorer state machines).
//! * Transfers (save/share/open copies) and platform requests are routed here as well.
//!
//! The shell calls [`handle_event`] once per Makepad event before widget dispatch.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::CoreEvent;
use paseo_gui_platform::{PlatformAction, PlatformValue};
use serde_json::{Value, json};

use super::explorer::{Entry, ExplorerCmd, ExplorerPreferences, ExplorerState};
use super::handle::{FileVersion, HandleCmd};
use super::host;
use super::i18n::{l, lf, t};
use super::images;
use super::path::{parent_explorer_path, remote_absolute};
use super::resolve::editor_platform;
use super::service;
use super::session::{AutosaveRequest, FileSession};
use super::transfer::{self, Progress, Transfer, TransferMode, TransferStage};
use crate::common::{UiContext, UiEffect};

thread_local! { static STORE: RefCell<Store> = RefCell::new(Store::default()); }

pub fn with<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    STORE.with(|s| f(&mut s.borrow_mut()))
}

/// The answer of [`close_requested`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseCheck {
    Clean,
    Dirty,
}

/// One file session with its owner bookkeeping.
pub struct Owned {
    pub owner: String,
    pub session: FileSession,
    timer: Timer,
    /// Writes held while the shell asks to discard unsaved changes.
    hold: Option<u64>,
    /// Last published close guard state.
    guard: bool,
    observing: bool,
    /// An `Observe` waiting for the host connection (Flutter `_connecting`).
    deferred_observe: Option<u64>,
}

/// A daemon file entry action awaiting its response.
#[derive(Clone, Debug)]
pub enum EntryAction {
    Create { parent: String, kind: String },
    Rename { entry: Entry, parent: String },
    Delete { parent: String },
    Duplicate { parent: String },
}

enum Pending {
    Observe(u64, u64),
    Classify(u64, u64, u64),
    Read(u64, u64, u64),
    Confirm(u64, u64),
    Write(u64, u64),
    List { owner: String, path: String, generation: u64 },
    Entry { owner: String, cwd: String, action: EntryAction },
    Asset { owner: String, key: String, ticket: u64 },
    Transfer(u64),
}

/// A platform request awaiting its `PlatformAction::Completed`.
enum PlatformPending {
    SaveLocation(u64),
    /// Open/share of a finished download; the transfer card is already gone.
    TransferFinish,
    Clipboard { owner: String },
    DaemonStatus { owner: String, cwd: String, path: String, line: usize, column: usize },
    EditorTargets { owner: String, cwd: String, path: String, line: usize, column: usize },
    Other,
}

/// What an owner hears back from asynchronous work (consumed by its widget).
#[derive(Clone, Debug)]
pub enum OwnerEvent {
    /// Clipboard text for an editor paste.
    Clipboard(String),
    /// `Open with` targets for the local editor menu.
    EditorTargets { targets: Vec<(String, String)>, cwd: String, path: String, line: usize, column: usize },
}

/// A markdown/HTML asset read for display.
#[derive(Clone)]
pub enum Asset {
    Loading,
    Ready { bytes: Arc<Vec<u8>>, mime: String },
    Failed(String),
}

#[derive(Default)]
pub struct Store {
    next: u64,
    pub sessions: HashMap<u64, Owned>,
    owners: HashMap<String, u64>,
    pending: HashMap<String, Pending>,
    observations: HashMap<String, (u64, u64)>,
    stash: service::Stash,
    pub explorers: HashMap<String, ExplorerState>,
    /// Explorer owners with an entry action in flight (Flutter `_busy`).
    busy: HashSet<String>,
    /// Hosts whose connection was seen connected (re-list explorers on reconnect).
    connected: HashSet<String>,
    pub transfers: Vec<Transfer>,
    progress: ToUIReceiver<Progress>,
    platform_pending: HashMap<u64, PlatformPending>,
    owner_events: HashMap<String, Vec<OwnerEvent>>,
    assets: HashMap<String, Asset>,
    /// `expandedPathsByWorkspace` entries written by this client.
    expanded_written: HashMap<String, Value>,
    /// Bumps when anything a view shows changed (views redraw on change).
    pub epoch: u64,
}

/// The shell's close flow for a dirty file tab: holds the tab's writes while the confirmation
/// is open.
pub fn close_requested(owner: &str) -> CloseCheck {
    with(|s| {
        let Some(id) = s.owners.get(owner).copied() else { return CloseCheck::Clean };
        let Some(entry) = s.sessions.get_mut(&id) else { return CloseCheck::Clean };
        if !entry.session.modified() {
            return CloseCheck::Clean;
        }
        if entry.hold.is_none() {
            entry.hold = Some(entry.session.handle.hold_writes());
        }
        CloseCheck::Dirty
    })
}

/// "Keep editing": the held writes run.
pub fn close_cancelled(owner: &str) {
    with(|s| {
        for entry in s.sessions.values_mut().filter(|e| e.owner == owner) {
            if let Some(hold) = entry.hold.take() {
                entry.session.handle.resume_writes(hold);
            }
        }
        s.epoch += 1;
    });
    SignalToUI::set_ui_signal();
}

/// "Close without saving": the session's held and queued writes are dropped.
pub fn close_confirmed(owner: &str) {
    with(|s| {
        if let Some(id) = s.owners.remove(owner)
            && let Some(entry) = s.sessions.get_mut(&id) {
                entry.hold = None;
                entry.session.dispose();
            }
        s.epoch += 1;
    });
    SignalToUI::set_ui_signal();
}

/// A files root went away (tab closed or retargeted without a confirmed discard): a modified
/// buffer is still written to its file, then the session ends.
pub fn release(owner: &str) {
    with(|s| {
        if let Some(id) = s.owners.remove(owner)
            && let Some(entry) = s.sessions.get_mut(&id) {
                if let Some(hold) = entry.hold.take() {
                    entry.session.handle.resume_writes(hold);
                }
                entry.session.release(&Value::Null);
            }
        s.explorers.remove(owner);
        s.busy.remove(owner);
        s.owner_events.remove(owner);
        let prefix = format!("{owner}\u{0}");
        s.assets.retain(|key, _| !key.starts_with(&prefix));
        images::forget_prefix(&prefix);
    });
    SignalToUI::set_ui_signal();
}

pub fn handle_event(cx: &mut Cx, event: &Event, ui: &mut UiContext) {
    with(|s| s.event(cx, event, ui));
}

fn associations(ui: &UiContext) -> Value {
    ui.preferences().get("filePanelAssociations").cloned().unwrap_or(Value::Null)
}

fn asset_key(owner: &str, path: &str) -> String {
    format!("{owner}\u{0}{path}")
}

impl Store {
    fn bump(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
    }

    // -----------------------------------------------------------------------------------------
    // File sessions

    /// The session of `owner` for exactly this identity, opening (and releasing the previous
    /// identity's) as needed.
    pub fn mount(&mut self, ui: &mut UiContext, owner: &str, host_id: &str, cwd: &str, path: &str) -> u64 {
        if let Some(id) = self.owners.get(owner).copied() {
            if let Some(old) = self.sessions.get(&id)
                && old.session.host_id == host_id && old.session.cwd == cwd && old.session.path == path && !old.session.released {
                    return id;
                }
            if let Some(old) = self.sessions.get_mut(&id) {
                if let Some(hold) = old.hold.take() {
                    old.session.handle.resume_writes(hold);
                }
                old.session.release(&associations(ui));
            }
        }
        self.next += 1;
        let id = self.next;
        self.sessions.insert(
            id,
            Owned {
                owner: owner.to_owned(),
                session: FileSession::new(host_id, cwd, path, editor_platform()),
                timer: Timer::default(),
                hold: None,
                guard: false,
                observing: false,
                deferred_observe: None,
            },
        );
        self.owners.insert(owner.to_owned(), id);
        self.bump();
        id
    }

    pub fn session_id(&self, owner: &str) -> Option<u64> {
        self.owners.get(owner).copied()
    }

    pub fn session(&self, owner: &str) -> Option<&FileSession> {
        self.session_id(owner).and_then(|id| self.sessions.get(&id)).map(|entry| &entry.session)
    }

    pub fn session_mut(&mut self, owner: &str) -> Option<&mut FileSession> {
        let id = self.session_id(owner)?;
        self.sessions.get_mut(&id).map(|entry| &mut entry.session)
    }

    /// Observation follows visibility and app foreground (Flutter `_syncObserving`).
    pub fn set_observing(&mut self, cx: &mut Cx, ui: &mut UiContext, owner: &str, observing: bool) {
        if let Some(id) = self.session_id(owner)
            && let Some(entry) = self.sessions.get_mut(&id) {
                entry.observing = observing;
                if !observing {
                    entry.deferred_observe = None;
                }
                entry.session.handle.set_observing(observing);
            }
        self.pump(cx, ui);
    }

    /// Observes again after a failure; before the connection is up, the observation starts
    /// when it connects (Flutter `retry`).
    pub fn retry(&mut self, cx: &mut Cx, ui: &mut UiContext, owner: &str) {
        if let Some(session) = self.session_mut(owner) {
            session.handle.retry();
        }
        let host = self.session(owner).map(|s| s.host_id.clone());
        if let Some(host) = host
            && !host::connected(&ui.state, &host) {
                ui.core.refresh_host(&host);
            }
        self.pump(cx, ui);
    }

    /// Runs `f` on the owner's model (edit, save, reload, overwrite) and dispatches its IO.
    pub fn with_model(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        owner: &str,
        f: impl FnOnce(&mut super::model::FileEditorModel, &mut dyn super::model::ModelIo),
    ) {
        let associations = associations(ui);
        if let Some(session) = self.session_mut(owner) {
            session.with_model(&associations, f);
        }
        self.pump(cx, ui);
    }

    pub fn retry_read(&mut self, cx: &mut Cx, ui: &mut UiContext, owner: &str) {
        let associations = associations(ui);
        if let Some(session) = self.session_mut(owner) {
            session.retry_read(&associations);
        }
        self.pump(cx, ui);
    }

    fn classify(&mut self, ui: &UiContext, id: u64, generation: u64, value: Value) {
        let version = FileVersion::from_json(value.get("initial").unwrap_or(&value));
        let Some(entry) = self.sessions.get_mut(&id) else { return };
        if version.is_ready() && version.kind().is_none() {
            // COMPAT(fileVersionKind): the first read's kind stands for later versions.
            let f = &entry.session;
            let (request, ticket) = service::read(&ui.core, &self.stash, &f.host_id, &f.cwd, &f.path, service::unbounded_read());
            self.pending.insert(request, Pending::Classify(id, generation, ticket));
        } else {
            let version = entry.session.handle.classify(version);
            entry.session.handle.on_observed(generation, Ok(version));
        }
    }

    fn issue_observe(&mut self, ui: &UiContext, id: u64, generation: u64) {
        let Some(entry) = self.sessions.get(&id) else { return };
        let f = &entry.session;
        if host::feature(ui, &f.host_id, "workspaceFileEditing") {
            let request = service::observe_file(&ui.core, &f.host_id, &f.cwd, &f.path);
            self.observations.insert(request.clone(), (id, generation));
            self.pending.insert(request, Pending::Observe(id, generation));
        } else {
            // COMPAT(workspaceFileEditing): the host cannot observe; its read is the stat.
            let (request, ticket) = service::read(&ui.core, &self.stash, &f.host_id, &f.cwd, &f.path, service::unbounded_read());
            self.pending.insert(request, Pending::Classify(id, generation, ticket));
        }
    }

    /// Routes session state and executes every queued handle/explorer command.
    pub fn pump(&mut self, cx: &mut Cx, ui: &mut UiContext) {
        let associations = associations(ui);
        loop {
            let mut issued = false;
            let ids: Vec<u64> = self.sessions.keys().copied().collect();
            for id in ids {
                let Some(entry) = self.sessions.get_mut(&id) else { continue };
                entry.session.pump(&associations);
                match std::mem::take(&mut entry.session.autosave) {
                    AutosaveRequest::Schedule => {
                        cx.stop_timer(entry.timer);
                        entry.timer = cx.start_timeout(super::model::AUTOSAVE_DELAY_SECONDS);
                    }
                    AutosaveRequest::Cancel => {
                        cx.stop_timer(entry.timer);
                        entry.timer = Timer::default();
                    }
                    AutosaveRequest::None => {}
                }
                let owned = self.owners.get(&entry.owner) == Some(&id);
                let dirty = owned && entry.session.modified();
                if dirty != entry.guard {
                    entry.guard = dirty;
                    ui.effects.push(UiEffect::SetCloseGuard { owner: entry.owner.clone(), dirty });
                }
                let commands = std::mem::take(&mut entry.session.handle.cmds);
                let (host_id, cwd, path) = (entry.session.host_id.clone(), entry.session.cwd.clone(), entry.session.path.clone());
                for command in commands {
                    issued = true;
                    match command {
                        HandleCmd::Observe { generation } => {
                            if host::connected(&ui.state, &host_id) {
                                self.issue_observe(ui, id, generation);
                            } else if let Some(entry) = self.sessions.get_mut(&id) {
                                entry.deferred_observe = Some(generation);
                            }
                        }
                        HandleCmd::StopObserve { generation } => {
                            if let Some(entry) = self.sessions.get_mut(&id) {
                                entry.deferred_observe = None;
                            }
                            let requests: Vec<String> = self
                                .observations
                                .iter()
                                .filter(|(_, pair)| **pair == (id, generation))
                                .map(|(r, _)| r.clone())
                                .collect();
                            for request in requests {
                                ui.core.cancel_request(&request);
                                self.observations.remove(&request);
                                self.pending.remove(&request);
                            }
                        }
                        HandleCmd::Read { job } => {
                            let (r, ticket) = service::read(&ui.core, &self.stash, &host_id, &cwd, &path, service::unbounded_read());
                            self.pending.insert(r, Pending::Read(id, job, ticket));
                        }
                        HandleCmd::ConfirmRead => {
                            let (r, ticket) = service::read(&ui.core, &self.stash, &host_id, &cwd, &path, service::unbounded_read());
                            self.pending.insert(r, Pending::Confirm(id, ticket));
                        }
                        HandleCmd::Write { job, content, expected } => {
                            if !host::feature(ui, &host_id, "workspaceFileEditing") {
                                // COMPAT(workspaceFileEditing): the host cannot write files.
                                if let Some(entry) = self.sessions.get_mut(&id) {
                                    entry.session.handle.on_write(job, Err(l("Update the host to edit files.").to_owned()));
                                }
                                continue;
                            }
                            let r = service::write(&ui.core, &host_id, &cwd, &path, content, &expected.modified_at, expected.revision.as_deref());
                            self.pending.insert(r, Pending::Write(id, job));
                        }
                    }
                }
            }
            // Released sessions whose writes settled go away (their guard clears).
            let finished: Vec<u64> = self.sessions.iter().filter(|(_, e)| e.session.finished()).map(|(id, _)| *id).collect();
            for id in finished {
                if let Some(entry) = self.sessions.remove(&id) {
                    cx.stop_timer(entry.timer);
                    if entry.guard {
                        ui.effects.push(UiEffect::SetCloseGuard { owner: entry.owner.clone(), dirty: false });
                    }
                    let requests: Vec<String> =
                        self.observations.iter().filter(|(_, pair)| pair.0 == id).map(|(r, _)| r.clone()).collect();
                    for request in requests {
                        ui.core.cancel_request(&request);
                        self.observations.remove(&request);
                        self.pending.remove(&request);
                    }
                }
            }
            issued |= self.pump_explorers(ui);
            if !issued {
                break;
            }
        }
        self.bump();
    }

    // -----------------------------------------------------------------------------------------
    // Explorers

    /// The explorer of `owner`, created on first use. `workspace_key` is `None` for a picker.
    pub fn explorer(
        &mut self,
        ui: &mut UiContext,
        owner: &str,
        cwd: &str,
        initial: &str,
        directories_only: bool,
        workspace_key: Option<String>,
    ) -> &mut ExplorerState {
        // Existing explorers are the hot path (including pointer moves and caret
        // timers): neither allocate an owner key nor copy the preference tree.
        if !self.explorers.contains_key(owner) {
            let state = ExplorerState::new(
                cwd,
                initial,
                directories_only,
                workspace_key.map(|workspace_key| ExplorerPreferences { workspace_key }),
                host::preferences(&ui.state),
            );
            self.explorers.insert(owner.to_owned(), state);
        }
        let state = self.explorers.get_mut(owner).unwrap();
        if state.cwd() != cwd {
            state.reset_cwd(cwd, host::preferences(&ui.state));
        } else if !state.started() {
            state.start(host::preferences(&ui.state));
        }
        state
    }

    pub fn explorer_mut(&mut self, owner: &str) -> Option<&mut ExplorerState> {
        self.explorers.get_mut(owner)
    }

    pub fn is_busy(&self, owner: &str) -> bool {
        self.busy.contains(owner)
    }

    fn pump_explorers(&mut self, ui: &mut UiContext) -> bool {
        let mut issued = false;
        let owners: Vec<String> = self.explorers.keys().cloned().collect();
        for owner in owners {
            let Some(state) = self.explorers.get_mut(&owner) else { continue };
            let cmds = std::mem::take(&mut state.cmds);
            let cwd = state.cwd().to_owned();
            for cmd in cmds {
                issued = true;
                match cmd {
                    ExplorerCmd::List { path, generation } => {
                        let Some(host_id) = explorer_host(&owner, ui) else { continue };
                        let request = service::list_directory(&ui.core, &host_id, &cwd, &path);
                        self.pending.insert(request, Pending::List { owner: owner.clone(), path, generation });
                    }
                    ExplorerCmd::SavePreference { key, value } => host::set_preference(ui, key, value),
                    ExplorerCmd::SaveExpanded { workspace_key, paths } => {
                        // The shell applies preference writes after this dispatch; the keys this
                        // client wrote stay authoritative over a not-yet-updated state copy.
                        self.expanded_written.insert(workspace_key, json!(paths));
                        let mut all = ui.preference("expandedPathsByWorkspace").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
                        for (key, value) in &self.expanded_written {
                            all[key.as_str()] = value.clone();
                        }
                        host::set_preference(ui, "expandedPathsByWorkspace", all);
                    }
                }
            }
        }
        issued
    }

    /// One `fs.entry.<action>.request`; the explorer is busy until it answers.
    pub fn entry_request(&mut self, ui: &mut UiContext, owner: &str, host_id: &str, cwd: &str, action: EntryAction, fields: Value) {
        let name = match &action {
            EntryAction::Create { .. } => "create",
            EntryAction::Rename { .. } => "rename",
            EntryAction::Delete { .. } => "delete",
            EntryAction::Duplicate { .. } => "duplicate",
        };
        let request = service::entry_request(&ui.core, host_id, cwd, name, fields);
        self.pending.insert(request, Pending::Entry { owner: owner.to_owned(), cwd: cwd.to_owned(), action });
        self.busy.insert(owner.to_owned());
        self.bump();
    }

    fn on_entry(&mut self, ui: &mut UiContext, owner: &str, cwd: &str, action: EntryAction, result: Result<Value, String>) {
        self.busy.remove(owner);
        let (failed_key, action_name) = match &action {
            EntryAction::Create { .. } => ("workspace.fileExplorer.errors.createFailed", "fs.entry.create.request"),
            EntryAction::Rename { .. } => ("workspace.fileExplorer.errors.renameFailed", "fs.entry.rename.request"),
            EntryAction::Delete { .. } => ("workspace.fileExplorer.errors.deleteFailed", "fs.entry.delete.request"),
            EntryAction::Duplicate { .. } => ("workspace.fileExplorer.errors.duplicateFailed", "fs.entry.duplicate.request"),
        };
        let payload = match result {
            Ok(payload) => payload,
            Err(error) => {
                host::toast(ui, error, true);
                return;
            }
        };
        if payload.get("success") != Some(&Value::Bool(true)) {
            let message = payload
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| service::payload_error(&payload, action_name))
                .unwrap_or_else(|| t(failed_key));
            host::toast(ui, message, true);
            return;
        }
        // The action ran in `cwd`; follow-ups only while the explorer still shows it.
        let host_id = explorer_host(owner, ui);
        let workspace_id = explorer_workspace(owner, ui);
        let Some(state) = self.explorers.get_mut(owner).filter(|state| state.cwd() == cwd) else { return };
        match action {
            EntryAction::Create { parent, kind } => {
                state.reload(&parent);
                let path = payload.get("path").and_then(Value::as_str).unwrap_or("");
                if kind == "file" && !path.is_empty()
                    && let (Some(host_id), Some(workspace_id)) = (host_id, workspace_id) {
                        let beside = open_beside_preference(ui);
                        host::open_file(ui, &host_id, &workspace_id, path, beside);
                    }
            }
            EntryAction::Rename { entry, parent } => {
                let renamed = payload.get("renamedPath").and_then(Value::as_str).unwrap_or("").to_owned();
                if renamed.is_empty() {
                    host::toast(ui, t(failed_key), true);
                    return;
                }
                state.reload(&parent);
                if entry.is_directory() {
                    state.renamed(&entry.path, &renamed);
                } else if let (Some(host_id), Some(workspace_id)) = (host_id, workspace_id) {
                    // Original `handleRenameCommit`: open tabs keep their old path.
                    let beside = open_beside_preference(ui);
                    host::open_file(ui, &host_id, &workspace_id, &renamed, beside);
                }
            }
            EntryAction::Delete { parent } => state.reload(&parent),
            EntryAction::Duplicate { parent } => {
                if !payload.get("duplicatedPath").is_some_and(Value::is_string) {
                    host::toast(ui, t(failed_key), true);
                }
                state.reload(&parent);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Assets (Markdown images, HTML preview assets are read by the preview server itself)

    /// A remote asset for display, read once per owner and path through the host connection.
    pub fn asset(&mut self, ui: &UiContext, owner: &str, host_id: &str, cwd: &str, path: &str) -> Asset {
        let key = asset_key(owner, path);
        if let Some(asset) = self.assets.get(&key) {
            return asset.clone();
        }
        if !host::connected(&ui.state, host_id) {
            return Asset::Loading;
        }
        let (request, ticket) = service::read(&ui.core, &self.stash, host_id, cwd, path, service::PREVIEW_MAX_BYTES);
        self.pending.insert(request, Pending::Asset { owner: owner.to_owned(), key: key.clone(), ticket });
        self.assets.insert(key, Asset::Loading);
        Asset::Loading
    }

    /// The asset state without starting a read.
    pub fn asset_peek(&self, owner: &str, path: &str) -> Option<Asset> {
        self.assets.get(&asset_key(owner, path)).cloned()
    }

    /// The connection changed: failed reads of an owner's assets are read again.
    fn forget_failed_assets(&mut self) {
        self.assets.retain(|_, asset| !matches!(asset, Asset::Failed(_)));
    }

    // -----------------------------------------------------------------------------------------
    // Transfers

    /// Flutter `saveRemoteFile`: save (desktop dialog first), share or open a downloaded copy.
    #[allow(clippy::too_many_arguments)]
    pub fn start_transfer(
        &mut self,
        ui: &mut UiContext,
        owner: &str,
        host_id: &str,
        cwd: &str,
        path: &str,
        mode: TransferMode,
        mime: Option<String>,
        current_bytes: Option<Arc<Vec<u8>>>,
    ) {
        self.next += 1;
        let mut transfer = Transfer::new(self.next, owner, host_id, cwd, path, mode, mime, current_bytes);
        let Some(platform) = host::platform() else {
            host::toast(ui, l("Platform services are not available"), true);
            return;
        };
        if mode == TransferMode::Save && host::has_save_dialog() {
            transfer.stage = TransferStage::Choosing;
            let request = platform.save_location(&transfer.name);
            transfer.platform_request = Some(request);
            self.platform_pending.insert(request, PlatformPending::SaveLocation(transfer.id));
            self.transfers.push(transfer);
        } else {
            transfer.run(&ui.core, self.progress.sender());
            if let Some(request) = transfer.request.clone() {
                self.pending.insert(request, Pending::Transfer(transfer.id));
            }
            self.transfers.push(transfer);
        }
        self.bump();
    }

    pub fn cancel_transfer(&mut self, ui: &mut UiContext, id: u64) {
        if let Some(index) = self.transfers.iter().position(|t| t.id == id) {
            let transfer = self.transfers.remove(index);
            if let Some(request) = &transfer.request {
                ui.core.cancel_request(request);
                self.pending.remove(request);
            }
            transfer::remove_staging_later(transfer.staging.clone());
        }
        self.bump();
    }

    pub fn transfers_of(&self, owner: &str) -> Vec<&Transfer> {
        self.transfers.iter().filter(|t| t.owner == owner && matches!(t.stage, TransferStage::Running)).collect()
    }

    fn on_transfer(&mut self, ui: &mut UiContext, id: u64, result: Result<Value, String>) {
        let Some(index) = self.transfers.iter().position(|t| t.id == id) else { return };
        let transfer = self.transfers.remove(index);
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                transfer::remove_staging_later(transfer.staging.clone());
                if !transfer.cancelled {
                    host::toast(ui, error, true);
                }
                return;
            }
        };
        let local = value.get("path").and_then(Value::as_str).unwrap_or("").to_owned();
        let mime = value.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream").to_owned();
        if value.get("saved") == Some(&Value::Bool(true)) {
            host::toast(ui, lf("Saved to {path}", &[("path", &local)]), false);
            return;
        }
        let Some(platform) = host::platform() else { return };
        let path = std::path::PathBuf::from(&local);
        let request = match transfer.mode {
            TransferMode::Open => platform.open_path(&path),
            // Mobile "save" shares (no save dialog), like the original.
            TransferMode::Share | TransferMode::Save => platform.share_file(&path, &mime, &transfer.name),
        };
        self.platform_pending.insert(request, PlatformPending::TransferFinish);
        // The staging copy is kept: it must outlive the external app/share sheet and lives in
        // the temp directory, as in the original.
    }

    // -----------------------------------------------------------------------------------------
    // Platform requests

    /// Pastes the clipboard text into the owner's editor (answer via [`take_owner_events`]).
    pub fn read_clipboard(&mut self, owner: &str) {
        if let Some(platform) = host::platform() {
            let request = platform.read_clipboard(false);
            self.platform_pending.insert(request, PlatformPending::Clipboard { owner: owner.to_owned() });
        }
    }

    /// Flutter `_openLocalEditor`: checks the file's host is the desktop's own daemon, then
    /// lists editor targets for the owner's `Open with` menu.
    pub fn open_local_editor(&mut self, ui: &mut UiContext, owner: &str, cwd: &str, path: &str, line: usize, column: usize) {
        let Some(platform) = host::platform().filter(|p| p.capabilities().editors) else {
            host::toast(ui, l("Local desktop host is not available"), true);
            return;
        };
        let request = platform.desktop_invoke("desktop_daemon_status", json!({}));
        self.platform_pending.insert(
            request,
            PlatformPending::DaemonStatus { owner: owner.to_owned(), cwd: cwd.to_owned(), path: path.to_owned(), line, column },
        );
    }

    pub fn open_editor(&mut self, editor_id: &str, cwd: &str, path: &str, line: usize, column: usize) {
        if let Some(platform) = host::platform() {
            let request = platform.open_editor(paseo_gui_platform::OpenEditorRequest {
                editor_id: editor_id.to_owned(),
                workspace_path: cwd.into(),
                file_path: Some(remote_absolute(cwd, path).into()),
                line: Some(line as u32),
                column: Some(column as u32),
            });
            self.platform_pending.insert(request, PlatformPending::Other);
        }
    }

    pub fn take_owner_events(&mut self, owner: &str) -> Vec<OwnerEvent> {
        self.owner_events.remove(owner).unwrap_or_default()
    }

    fn on_platform(&mut self, ui: &mut UiContext, request: u64, result: Result<PlatformValue, paseo_gui_platform::PlatformError>) {
        let Some(pending) = self.platform_pending.remove(&request) else { return };
        match pending {
            PlatformPending::SaveLocation(id) => {
                let Some(index) = self.transfers.iter().position(|t| t.id == id) else { return };
                match result {
                    Ok(PlatformValue::SaveLocation(Some(destination))) => {
                        let transfer = &mut self.transfers[index];
                        transfer.destination = Some(destination);
                        transfer.run(&ui.core, self.progress.sender());
                        if let Some(request) = transfer.request.clone() {
                            self.pending.insert(request, Pending::Transfer(id));
                        }
                    }
                    Ok(_) => {
                        self.transfers.remove(index);
                    }
                    Err(error) => {
                        self.transfers.remove(index);
                        if error.code != "cancelled" {
                            host::toast(ui, error.message, true);
                        }
                    }
                }
            }
            PlatformPending::TransferFinish | PlatformPending::Other => {
                if let Err(error) = result
                    && error.code != "cancelled" {
                        host::toast(ui, error.message, true);
                    }
            }
            PlatformPending::Clipboard { owner } => match result {
                Ok(PlatformValue::Clipboard(content)) => {
                    if let Some(text) = content.text {
                        self.owner_events.entry(owner).or_default().push(OwnerEvent::Clipboard(text));
                    }
                }
                Ok(PlatformValue::Text(Some(text))) => {
                    self.owner_events.entry(owner).or_default().push(OwnerEvent::Clipboard(text));
                }
                Ok(_) => {}
                Err(error) => host::toast(ui, error.message, true),
            },
            PlatformPending::DaemonStatus { owner, cwd, path, line, column } => {
                let status = match result {
                    Ok(PlatformValue::Json(value)) => value,
                    Ok(_) => Value::Null,
                    Err(error) => {
                        host::toast(ui, error.message, true);
                        return;
                    }
                };
                let host_id = self.session(&owner).map(|s| s.host_id.clone()).unwrap_or_default();
                let local_server = status.get("serverId").and_then(Value::as_str).unwrap_or("");
                let record_server = host::host(&ui.state, &host_id)
                    .and_then(|h| h.get("serverId"))
                    .and_then(Value::as_str)
                    .unwrap_or(&host_id)
                    .to_owned();
                if local_server != record_server && local_server != host_id {
                    host::toast(ui, l("This file belongs to a different host. Open a downloaded copy instead."), true);
                    return;
                }
                let Some(platform) = host::platform() else { return };
                let request = platform.list_editor_targets();
                self.platform_pending.insert(request, PlatformPending::EditorTargets { owner, cwd, path, line, column });
            }
            PlatformPending::EditorTargets { owner, cwd, path, line, column } => match result {
                Ok(PlatformValue::EditorTargets(targets)) => {
                    let targets = targets.into_iter().map(|t| (t.id, t.label)).collect();
                    self.owner_events.entry(owner).or_default().push(OwnerEvent::EditorTargets { targets, cwd, path, line, column });
                }
                Ok(_) => {}
                Err(error) => host::toast(ui, error.message, true),
            },
        }
        self.bump();
    }

    // -----------------------------------------------------------------------------------------
    // Event routing

    fn event(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext) {
        let mut changed = false;
        let associations = associations(ui);
        if let Event::Timer(_) = event {
            let ids: Vec<u64> = self.sessions.iter().filter(|(_, e)| e.timer.is_event(event).is_some()).map(|(id, _)| *id).collect();
            for id in ids {
                if let Some(entry) = self.sessions.get_mut(&id) {
                    entry.timer = Timer::default();
                    entry.session.with_model(&associations, |m, io| m.autosave(io));
                    changed = true;
                }
            }
        }
        if let Event::Actions(actions) = event {
            for action in actions.iter() {
                if let Some(PlatformAction::Completed { request, result }) = action.downcast_ref::<PlatformAction>()
                    && self.platform_pending.contains_key(request) {
                        self.on_platform(ui, *request, result.clone());
                        changed = true;
                    }
            }
        }
        if let Event::Signal = event {
            while let Ok(progress) = self.progress.try_recv() {
                if let Some(transfer) = self.transfers.iter_mut().find(|t| t.id == progress.id) {
                    transfer.received = progress.received;
                    transfer.total = progress.total;
                    changed = true;
                }
            }
            changed |= images::poll(cx);
            changed |= super::highlight::poll();
        }
        let events = std::mem::take(&mut ui.events);
        for core_event in &events {
            match core_event {
                CoreEvent::Response { request_id, result, .. } => {
                    let Some(pending) = self.pending.remove(request_id) else { continue };
                    changed = true;
                    let result = result.clone().map_err(|e| e.message.clone());
                    self.on_response(ui, request_id, pending, result);
                }
                CoreEvent::HostEvent { host_id, event } => {
                    let ids: Vec<(u64, u64)> = event
                        .get("observations")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter_map(|r| self.observations.get(r).copied())
                        .collect();
                    for (id, generation) in ids {
                        let Some(entry) = self.sessions.get_mut(&id) else { continue };
                        if entry.session.host_id != *host_id {
                            continue;
                        }
                        changed = true;
                        let payload = event.get("payload").unwrap_or(&Value::Null);
                        match event.get("type").and_then(Value::as_str) {
                            // `fs.file.update` carries a version; a restored subscription's
                            // snapshot repairs what a reconnect missed.
                            Some("fs.file.update") => {
                                let version = FileVersion::from_json(payload.get("version").unwrap_or(&Value::Null));
                                entry.session.handle.on_observation(generation, version);
                            }
                            Some("subscription.restored") => {
                                let version = FileVersion::from_json(payload.get("initial").unwrap_or(&Value::Null));
                                entry.session.handle.on_observation(generation, version);
                            }
                            Some("subscription.error") => {
                                let error = payload.get("error").and_then(Value::as_str).unwrap_or("File observation failed");
                                entry.session.handle.on_observation(generation, FileVersion::Error(error.to_owned()));
                            }
                            _ => {}
                        }
                    }
                }
                CoreEvent::Connected { host_id, .. } => {
                    changed = true;
                    self.on_connected(ui, host_id);
                }
                CoreEvent::Disconnected { host_id, .. } => {
                    self.connected.remove(host_id);
                }
                _ => {}
            }
        }
        ui.events = events;
        if changed || !self.pending_cmds_empty() {
            self.pump(cx, ui);
        }
    }

    pub(super) fn pending_cmds_empty(&self) -> bool {
        self.sessions.values().all(|e| e.session.handle.cmds.is_empty() && e.session.handle.events.is_empty())
            && self.explorers.values().all(|e| e.cmds.is_empty())
    }

    fn on_connected(&mut self, ui: &mut UiContext, host_id: &str) {
        let reconnect = !self.connected.insert(host_id.to_owned());
        // Deferred observations start once the connection is up (exactly once).
        let deferred: Vec<(u64, u64)> = self
            .sessions
            .iter_mut()
            .filter(|(_, e)| e.session.host_id == host_id)
            .filter_map(|(id, e)| e.deferred_observe.take().map(|generation| (*id, generation)))
            .collect();
        for (id, generation) in deferred {
            self.issue_observe(ui, id, generation);
        }
        // COMPAT(workspaceFileEditing): a read-as-stat observation does not restore itself.
        if !host::feature(ui, host_id, "workspaceFileEditing") {
            for entry in self.sessions.values_mut().filter(|e| e.session.host_id == host_id && e.observing) {
                entry.session.handle.retry();
            }
        }
        // The tree and its expansion stay; it lists again on the new connection.
        let owners: Vec<String> = self.explorers.keys().cloned().collect();
        for owner in owners {
            if explorer_host(&owner, ui).as_deref() == Some(host_id)
                && let Some(state) = self.explorers.get_mut(&owner)
                    && (reconnect || state.root_error().is_some()) {
                        state.connection_restored();
                    }
        }
        self.forget_failed_assets();
    }

    fn on_response(&mut self, ui: &mut UiContext, request_id: &str, pending: Pending, result: Result<Value, String>) {
        match pending {
            Pending::Observe(id, generation) => {
                match result {
                    Ok(value) => self.classify(ui, id, generation, value),
                    Err(error) => {
                        self.observations.remove(request_id);
                        if let Some(e) = self.sessions.get_mut(&id) {
                            e.session.handle.on_observed(generation, Err(error));
                        }
                    }
                }
            }
            Pending::Classify(id, generation, ticket) => {
                let contents = result.and_then(|_| self.stash.take(ticket).ok_or_else(|| "File payload unavailable".to_owned()));
                self.stash.discard(ticket);
                if let Some(e) = self.sessions.get_mut(&id) {
                    let version = contents.map(|c| {
                        e.session.handle.set_observed_kind(&c.kind, &c.mime_type);
                        c.as_version()
                    });
                    match version {
                        Ok(version) => {
                            // COMPAT(fileVersionKind): classify the observed initial version.
                            let observed = self.observations.iter().any(|(_, pair)| *pair == (id, generation));
                            let version = if observed { e.session.handle.classify(version) } else { version };
                            e.session.handle.on_observed(generation, Ok(version));
                        }
                        Err(error) => {
                            // An observation that failed to classify releases itself.
                            let requests: Vec<String> = self.observations.iter().filter(|(_, pair)| **pair == (id, generation)).map(|(r, _)| r.clone()).collect();
                            for request in requests {
                                ui.core.cancel_request(&request);
                                self.observations.remove(&request);
                            }
                            e.session.handle.on_observed(generation, Err(error));
                        }
                    }
                }
            }
            Pending::Read(id, job, ticket) => {
                let contents = result.and_then(|_| self.stash.take(ticket).ok_or_else(|| "File payload unavailable".to_owned()));
                self.stash.discard(ticket);
                if let Some(e) = self.sessions.get_mut(&id) {
                    e.session.handle.on_read(job, contents);
                }
            }
            Pending::Confirm(id, ticket) => {
                let contents = result.ok().and_then(|_| self.stash.take(ticket));
                self.stash.discard(ticket);
                if let Some(e) = self.sessions.get_mut(&id) {
                    e.session.handle.on_confirm_read(contents);
                }
            }
            Pending::Write(id, job) => {
                if let Some(e) = self.sessions.get_mut(&id) {
                    let outcome = result.and_then(|payload| {
                        service::write_result(&payload).ok_or_else(|| {
                            service::payload_error(&payload, "fs.file.write.request").unwrap_or_else(|| "Save was not acknowledged".to_owned())
                        })
                    });
                    e.session.handle.on_write(job, outcome);
                }
            }
            Pending::List { owner, path, generation } => {
                let Some(state) = self.explorers.get_mut(&owner) else { return };
                let listed = result.and_then(|payload| {
                    if let Some(error) = payload.get("error").and_then(Value::as_str) {
                        return Err(error.to_owned());
                    }
                    let directory = payload.get("directory").filter(|d| d.is_object()).ok_or_else(|| l("Directory listing unavailable").to_owned())?;
                    Ok(directory.get("entries").and_then(Value::as_array).cloned().unwrap_or_default())
                });
                state.on_listed(&path, generation, listed);
            }
            Pending::Entry { owner, cwd, action } => self.on_entry(ui, &owner, &cwd, action, result),
            Pending::Asset { owner, key, ticket } => {
                let asset = match result.and_then(|_| self.stash.take(ticket).ok_or_else(|| "File payload unavailable".to_owned())) {
                    Ok(contents) => Asset::Ready { bytes: contents.bytes, mime: contents.mime_type },
                    Err(error) => Asset::Failed(error),
                };
                self.stash.discard(ticket);
                // A released owner's late asset is dropped.
                if self.owners.contains_key(&owner) || self.explorers.contains_key(&owner) || self.assets.contains_key(&key) {
                    self.assets.insert(key, asset);
                }
            }
            Pending::Transfer(id) => self.on_transfer(ui, id, result),
        }
    }
}

/// The host of an explorer owner (its tab target, or a picker's registered host).
fn explorer_host(owner: &str, ui: &UiContext) -> Option<String> {
    PICKER_HOSTS.with(|hosts| hosts.borrow().get(owner).cloned()).or_else(|| {
        ui.layout.target(owner).map(|t| t.server_id.clone()).or_else(|| {
            OWNER_TARGETS.with(|targets| targets.borrow().get(owner).map(|(host, _)| host.clone()))
        })
    })
}

fn explorer_workspace(owner: &str, ui: &UiContext) -> Option<String> {
    ui.layout
        .target(owner)
        .map(|t| t.workspace_id.clone())
        .or_else(|| OWNER_TARGETS.with(|targets| targets.borrow().get(owner).map(|(_, ws)| ws.clone())))
}

/// `preferences.openInSidePane.explorerFiles` (Flutter `_open` default).
pub fn open_beside_preference(ui: &UiContext) -> bool {
    ui.preference("openInSidePane")
        .and_then(|value| value.get("explorerFiles"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

thread_local! {
    /// Owner → (host, workspace) of mounted files roots (set by the panel on `set_target`).
    static OWNER_TARGETS: RefCell<HashMap<String, (String, String)>> = RefCell::new(HashMap::new());
    /// Picker owner → host.
    static PICKER_HOSTS: RefCell<HashMap<String, String>> = RefCell::new(HashMap::new());
}

pub fn register_owner(owner: &str, host_id: &str, workspace_id: &str) {
    OWNER_TARGETS.with(|targets| targets.borrow_mut().insert(owner.to_owned(), (host_id.to_owned(), workspace_id.to_owned())));
}

pub fn unregister_owner(owner: &str) {
    OWNER_TARGETS.with(|targets| targets.borrow_mut().remove(owner));
    PICKER_HOSTS.with(|hosts| hosts.borrow_mut().remove(owner));
}

pub fn register_picker(owner: &str, host_id: &str) {
    PICKER_HOSTS.with(|hosts| hosts.borrow_mut().insert(owner.to_owned(), host_id.to_owned()));
}

/// Explorer entry parent (re-exported for the view).
pub fn parent_of(path: &str) -> String {
    parent_explorer_path(path)
}
