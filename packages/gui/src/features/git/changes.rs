//! The Changes view (original changes_panel.dart `ChangesPanel`): the
//! explorer `tree`, the `combined` working diff pane (document plus an
//! optional tree rail) and the bare commit `diff`, with the repository and
//! comparison toolbars, the branch switcher and stash flows, discard, file
//! menus, review comments and feedback, and the commits footer.

use super::actions::{ArchiveConfirm, Checkout, ahead_count, menu_entries};
use super::chrome::{
    ButtonSize, ButtonVariant, TOOLBAR_HEIGHT, button, button_height, diff_stat, label_trigger,
    measure_button, measure_diff_stat, palette, toolbar_band, toolbar_button, toolbar_button_size, ui as ui_px,
    TREE_ROW_HEIGHT, file_tree_row,
};
use super::controller::DiffController;
use super::diff_doc::{DiffDoc, DocHit, DocInput, LineTarget};
use super::host;
use super::json::{array, error_message, flag, int, text};
use super::model::{
    ChangeKind, TreeRow, absolute_path, change_kind, changed_tree_rows, collapse_folder_recursive, commit_time_ago,
    format_base_ref_label, now_ms, parse_iso8601_ms,
};
use super::overlay::{MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult, SheetRequest};
use super::paint::{Font, GitPaint, Hits, rect, with_alpha};
use super::policy::{ActionStatus, forge_presentation, forge_setup_message, select_pr_hint};
use super::review::{
    build_attachment_snapshot, comment_subtitle, feedback_disabled_reason, review_draft_key, sent_comments,
};
use super::store::GitStore;
use super::text::{msg, t, tr};
use super::wire::{self, payload_error};
use crate::common::overlay::tooltip_at;
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::theme::DrawBox;
use makepad_widgets::*;
use makepad_widgets::Hit as Pointer;
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

/// Original `FLOATING_ACTION_BUTTON_CLEARANCE`: the 56px action plus its 16px inset.
const FAB_CLEARANCE: f64 = 56.0 + 16.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    Tree,
    Combined,
    Diff,
}

/// The widget parts a sheet draws with.
pub struct OverlayParts<'a> {
    pub list: &'a mut DrawList2d,
    pub bg: &'a mut DrawBox,
    pub input: &'a mut TextInput,
}

#[derive(Debug)]
enum Purpose {
    ModeMenu,
    OptionsMenu,
    GitActionsMenu,
    ArchiveConfirm(String),
    Branch,
    StashAndSwitch(String),
    RestoreStash(i64),
    Discard { path: String, old_path: Option<String> },
    FileMenu { path: String, old_path: Option<String> },
    FolderMenu(String),
    LineMenu(LineTarget),
    Comment { id: Option<String>, path: String, side: String, line: i64 },
    Recipient,
    Jump,
}

#[derive(Clone, Debug, PartialEq)]
enum Hit {
    Branch,
    PrHint,
    OpenForge,
    GitActions,
    Mode,
    TreeToggle,
    Refresh,
    Options,
    Toggle(&'static str),
    Folder(String),
    File(String),
    EmptyAction(&'static str),
    CommitsHeader,
    Commit(String),
    Jump,
    Feedback,
    Doc(DocHit),
}

enum Flow {
    BranchSuggestions(Rect),
    Switch(String),
    StashSave(String),
    SwitchAfterStash,
    StashList(String),
    StashPop,
    Duplicate,
    Feedback { draft: String, sent: Vec<Value>, label: String },
}

pub struct ChangesView {
    pub target: PanelTarget,
    pub controller: DiffController,
    pub doc: DiffDoc,
    overlay: OverlayHost<Purpose>,
    collapsed_folders: HashSet<String>,
    collapsed_files: HashSet<String>,
    selected_tree_path: Option<String>,
    // Original `defaultChangesState`: tree rail hidden, commits collapsed.
    tree_visible: bool,
    commits_collapsed: bool,
    initial_revealed: bool,
    /// A refocus request from the shell (`focusRequestId` changed), applied at the next draw.
    pub pending_focus: Option<String>,
    draft_key: Option<String>,
    tree_scroll: f64,
    tree_area: Rect,
    tree_content: f64,
    commits_scroll: f64,
    commits_area: Rect,
    commits_content: f64,
    hits: Hits<Hit>,
    hovered: Option<Hit>,
    pressed: Option<Hit>,
    flows: HashMap<RequestId, Flow>,
    pub spinning: bool,
    /// A material file icon was still decoding at the last draw.
    pub icons_pending: bool,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    horizontal_scroll: makepad_widgets::scroll_bars::TouchScroll,
}

fn preferences(ui: &UiContext) -> Map<String, Value> {
    let mut merged: Map<String, Value> = [
        ("layout", json!("unified")),
        ("wrapLines", json!(false)),
        ("hideWhitespace", json!(false)),
        ("inlineDiff", json!(false)),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if let Value::Object(stored) = host::preference(ui, "changesPreferences") {
        for (key, value) in stored {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn pref_flag(prefs: &Map<String, Value>, key: &str) -> bool {
    prefs.get(key) == Some(&Value::Bool(true))
}

impl ChangesView {
    pub fn new(cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        let mut controller = DiffController::new(&target, store);
        if !controller.is_commit {
            // A working diff reads status through the checkout's shared state.
            store.acquire_checkout(cx, ui, &target.server_id, &target.cwd);
        }
        controller.ignore_whitespace = !controller.is_commit && pref_flag(&preferences(ui), "hideWhitespace");
        controller.refresh(ui, store, false);
        let mut view = Self {
            target,
            controller,
            doc: DiffDoc::default(),
            overlay: OverlayHost::default(),
            collapsed_folders: HashSet::new(),
            collapsed_files: HashSet::new(),
            selected_tree_path: None,
            tree_visible: false,
            commits_collapsed: true,
            initial_revealed: false,
            pending_focus: None,
            draft_key: None,
            tree_scroll: 0.0,
            tree_area: Rect::default(),
            tree_content: 0.0,
            commits_scroll: 0.0,
            commits_area: Rect::default(),
            commits_content: 0.0,
            hits: Hits::default(),
            hovered: None,
            pressed: None,
            flows: HashMap::new(),
            spinning: false,
            icons_pending: false,
            touch_scroll: Default::default(),
            horizontal_scroll: Default::default(),
        };
        view.bind_review(ui, store);
        view
    }

    fn host(&self) -> &str {
        &self.target.server_id
    }

    fn cwd(&self) -> &str {
        &self.target.cwd
    }

    fn checkout(&self) -> Checkout {
        Checkout {
            host: self.target.server_id.clone(),
            workspace_id: self.target.workspace_id.clone(),
            cwd: self.target.cwd.clone(),
        }
    }

    /// Releases everything this view holds in the store and the core.
    pub fn dispose(&mut self, store: &mut GitStore) {
        if let Some(core) = store.core().cloned() {
            self.controller.dispose(&core, store);
        }
        if !self.controller.is_commit {
            store.release_checkout(&self.target.server_id, &self.target.cwd);
        }
        if let Some(key) = self.draft_key.take() {
            store.release_draft(&key);
        }
    }

    pub fn close_overlay(&mut self, cx: &mut Cx) {
        self.overlay.close(cx);
    }

    fn presentation(&self, compact: bool, prefs: &Map<String, Value>) -> Presentation {
        match self.target.kind.as_str() {
            "changes_tree" if compact || host::is_mobile_platform() || pref_flag(prefs, "inlineDiff") => Presentation::Combined,
            "changes_tree" => Presentation::Tree,
            "commit_diff" => Presentation::Diff,
            _ => Presentation::Combined,
        }
    }

    /// Original `canUseSplitLayout`.
    fn can_split(compact: bool) -> bool {
        !compact && !host::is_mobile_platform()
    }

    fn split(compact: bool, prefs: &Map<String, Value>) -> bool {
        Self::can_split(compact) && prefs.get("layout").and_then(Value::as_str) == Some("split")
    }

    fn update_preferences(ui: &mut UiContext, updates: &[(&str, Value)]) {
        let mut prefs = preferences(ui);
        for (key, value) in updates {
            prefs.insert((*key).to_owned(), value.clone());
        }
        ui.push(UiEffect::SetPreference {
            key: "changesPreferences".into(),
            value: Value::Object(prefs),
        });
    }

    /// Original `_bindReview`: the draft follows comparison, base ref and whitespace.
    fn bind_review(&mut self, ui: &UiContext, store: &mut GitStore) {
        if self.controller.is_commit {
            return;
        }
        let key = review_draft_key(
            self.host(),
            &self.target.workspace_id,
            self.cwd(),
            &self.controller.mode,
            &self.controller.base_ref,
            self.controller.ignore_whitespace,
        );
        if self.draft_key.as_deref() == Some(key.as_str()) {
            return;
        }
        if let Some(previous) = self.draft_key.take() {
            store.release_draft(&previous);
        }
        store.acquire_draft(ui, &key);
        self.draft_key = Some(key);
    }

    fn comments(&self, store: &GitStore) -> Vec<Value> {
        self.draft_key
            .as_deref()
            .and_then(|key| store.draft(key))
            .map(|d| d.comments.clone())
            .unwrap_or_default()
    }

    /// Original `_ensureCommentPatches`: files that carry comments load theirs.
    fn ensure_comment_patches(&mut self, ui: &UiContext, store: &GitStore) {
        if self.controller.too_large {
            return;
        }
        for comment in self.comments(store) {
            let path = text(&comment, "filePath");
            let c = &self.controller;
            if c.patches.contains_key(path)
                || c.loading_patches.contains(path)
                || c.patch_errors.contains_key(path)
                || !c.files.iter().any(|f| text(f, "path") == path)
            {
                continue;
            }
            self.controller.ensure_patch(ui, path);
        }
    }

    // -----------------------------------------------------------------------
    // Event processing.

    /// Applies a core batch / store change; returns true when a redraw is due.
    pub fn process(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        store: &mut GitStore,
        visible: bool,
    ) -> bool {
        let mut dirty = false;
        if self.controller.active() != visible {
            self.controller.set_active(ui, store, visible);
            dirty = true;
        }
        let hide = pref_flag(&preferences(ui), "hideWhitespace");
        if !self.controller.is_commit && self.controller.ignore_whitespace != hide {
            self.controller.set_ignore_whitespace(ui, store, hide);
            dirty = true;
        }
        dirty |= self.controller.process(ui, store);
        self.bind_review(ui, store);
        self.ensure_comment_patches(ui, store);
        let responses: Vec<(RequestId, Result<Value, paseo_gui_core::CoreError>)> = ui
            .events
            .iter()
            .filter_map(|event| match event {
                CoreEvent::Response { request_id, result, .. } if self.flows.contains_key(request_id) => {
                    Some((request_id.clone(), result.clone()))
                }
                _ => None,
            })
            .collect();
        for (request_id, result) in responses {
            if let Some(flow) = self.flows.remove(&request_id) {
                self.on_flow(cx, ui, store, flow, result);
                dirty = true;
            }
        }
        dirty
    }

    fn send(&mut self, ui: &UiContext, kind: &str, fields: Value, flow: Flow) {
        let request = wire::send(&ui.core, self.host(), kind, fields);
        self.flows.insert(request, flow);
    }

    fn open_overlay(&mut self, purpose: Purpose, kind: OverlayKind) {
        self.overlay.open(Overlay { purpose, kind });
    }

    /// Sends a menu / confirm / select opened during this event to the shell.
    pub fn flush_overlay(&mut self, ui: &mut UiContext) {
        self.overlay.flush(ui);
    }

    fn on_flow(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        store: &mut GitStore,
        flow: Flow,
        result: Result<Value, paseo_gui_core::CoreError>,
    ) {
        let payload = wire::checkout_result(&result);
        match flow {
            Flow::BranchSuggestions(anchor) => {
                let payload = match payload {
                    Ok(payload) => payload,
                    Err(failure) => return ui.toast(failure.message, true),
                };
                if let Some(error) = payload_error(&payload) {
                    return ui.toast(error, true);
                }
                let options = array(&payload, "branches")
                    .iter()
                    .map(|b| {
                        let name = match b {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (name.clone(), name)
                    })
                    .collect();
                let selected = self.current_branch();
                self.open_overlay(Purpose::Branch, OverlayKind::Picker {
                    title: t("branchSwitcher.title"),
                    anchor,
                    min_width: 280.0,
                    icon: Some("git-branch"),
                    options,
                    selected,
                    placeholder: t("branchSwitcher.searchPlaceholder"),
                    empty: t("branchSwitcher.empty"),
                });
            }
            Flow::Switch(branch) => match payload {
                Err(failure) => ui.toast(wire::message_or(&failure, t("branchSwitcher.failedToSwitch")), true),
                Ok(payload) => match payload_error(&payload) {
                    Some(error) if error.to_lowercase().contains("uncommitted") => {
                        self.open_overlay(Purpose::StashAndSwitch(branch), OverlayKind::Confirm {
                            title: t("branchSwitcher.uncommittedTitle"),
                            message: t("branchSwitcher.uncommittedMessage"),
                            confirm: t("branchSwitcher.stashAndSwitch"),
                            destructive: false,
                        });
                    }
                    Some(error) => ui.toast(error, true),
                    None => {
                        store.invalidate(cx, self.host(), self.cwd());
                        // Original `maybeRestoreStashForBranch`: non-critical.
                        self.send(ui, "stash_list_request", json!({ "cwd": self.cwd(), "paseoOnly": true }), Flow::StashList(branch));
                    }
                },
            },
            Flow::StashSave(branch) => match payload {
                Err(failure) => ui.toast(wire::message_or(&failure, t("branchSwitcher.failedToStash")), true),
                Ok(payload) => match payload_error(&payload) {
                    Some(error) => ui.toast(error, true),
                    None => {
                        store.invalidate(cx, self.host(), self.cwd());
                        self.send(
                            ui,
                            "checkout_switch_branch_request",
                            json!({ "cwd": self.cwd(), "branch": branch }),
                            Flow::SwitchAfterStash,
                        );
                    }
                },
            },
            Flow::SwitchAfterStash => match payload {
                Err(failure) => ui.toast(wire::message_or(&failure, t("branchSwitcher.failedToStash")), true),
                Ok(payload) => match payload_error(&payload) {
                    Some(error) => ui.toast(error, true),
                    None => store.invalidate(cx, self.host(), self.cwd()),
                },
            },
            Flow::StashList(branch) => {
                let Ok(payload) = payload else { return };
                let entry = array(&payload, "entries")
                    .iter()
                    .find(|e| e.get("branch").and_then(Value::as_str) == Some(branch.as_str()));
                if let Some(entry) = entry {
                    self.open_overlay(Purpose::RestoreStash(int(entry, "index")), OverlayKind::Confirm {
                        title: t("branchSwitcher.restoreStashTitle"),
                        message: t("branchSwitcher.restoreStashMessage"),
                        confirm: t("branchSwitcher.restore"),
                        destructive: false,
                    });
                }
            }
            Flow::StashPop => {
                if let Ok(payload) = payload {
                    match payload_error(&payload) {
                        Some(error) => ui.toast(error, true),
                        None => ui.toast(t("branchSwitcher.stashRestored"), false),
                    }
                }
                store.invalidate(cx, self.host(), self.cwd());
            }
            Flow::Duplicate => match payload {
                Err(failure) => ui.toast(failure.message, true),
                Ok(payload) if payload.get("success") != Some(&Value::Bool(true)) => {
                    let message = match payload.get("error") {
                        Some(Value::String(error)) => error.clone(),
                        _ => t("workspace.fileExplorer.errors.duplicateFailed"),
                    };
                    ui.toast(message, true);
                }
                Ok(_) => {}
            },
            Flow::Feedback { draft, sent, label } => {
                let failed = t("review.feedback.failed");
                match wire::git_result(&result) {
                    Ok(_) => {
                        ui.toast(label, false);
                        store.edit_draft(cx, &draft, |d| d.clear_sent(&sent));
                        store.set_draft_phase(cx, &draft, None, None);
                    }
                    Err(failure) => {
                        let message = wire::message_or(&failure, failed);
                        store.set_draft_phase(cx, &draft, None, Some(message));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Interactions.

fn mode_label(mode: &str) -> &'static str {
    match mode {
        "all" => "workspace.git.diff.all",
        "base" => "workspace.git.diff.committed",
        _ => "workspace.git.diff.uncommitted",
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl ChangesView {
    /// The branch the switcher names: none for a detached HEAD.
    fn current_branch(&self) -> Option<String> {
        let branch = self.controller.status.as_ref().map(|s| text(s, "currentBranch")).unwrap_or("");
        (!branch.is_empty() && branch != "HEAD").then(|| branch.to_owned())
    }

    fn base_label(&self) -> String {
        format_base_ref_label(&self.controller.base_ref, &t("workspace.git.diff.base"))
    }

    /// Original `resolveBranchLabel` + `computeCommittedDiffDescription`.
    fn committed_description(&self) -> Option<String> {
        let branch = self.current_branch().unwrap_or_else(|| {
            let not_git = self.controller.status.as_ref().is_some_and(|s| !flag(s, "isGit"));
            t(if not_git { "workspace.git.diff.notRepository" } else { "workspace.git.diff.branchUnknown" })
        });
        let base = self.base_label();
        if branch.is_empty() || base.is_empty() || branch == base {
            return None;
        }
        Some(format!("{branch} -> {base}"))
    }

    fn all_files_collapsed(&self) -> bool {
        !self.controller.files.is_empty()
            && self.controller.files.iter().all(|f| self.collapsed_files.contains(text(f, "path")))
    }

    fn toggle_collapse_all(&mut self) {
        if self.all_files_collapsed() {
            self.collapsed_files.clear();
        } else {
            let paths: Vec<String> = self.controller.files.iter().map(|f| text(f, "path").to_owned()).collect();
            self.collapsed_files.extend(paths);
        }
    }

    fn refreshing(&self, store: &GitStore) -> bool {
        store.action_status(self.host(), self.cwd(), "refresh") == ActionStatus::Pending
    }

    /// Original `useCheckoutGitActionsStore.refresh`: the host re-reads the checkout.
    fn refresh_checkout(&mut self, cx: &mut Cx, store: &mut GitStore) {
        if self.refreshing(store) {
            return;
        }
        let (host, cwd) = (self.host().to_owned(), self.cwd().to_owned());
        store.run_action(
            cx,
            &host,
            &cwd,
            "refresh",
            vec![("checkout.refresh.request", json!({}))],
            "workspace.git.diff.failedRefresh",
            None,
            super::store::ActionFollowUp::None,
        );
    }

    fn open_target(ui: &mut UiContext, target: PanelTarget) {
        ui.push(UiEffect::OpenTab { target, placement: TabPlacement::Main });
    }

    fn open_file(&self, ui: &mut UiContext, path: &str, line: Option<i64>) {
        let mut data = json!({ "path": path });
        if let Some(line) = line {
            data["line"] = json!(line);
        }
        let target = PanelTarget::new("file", self.host(), &self.target.workspace_id, self.cwd())
            .with_id(path)
            .with_title(basename(path))
            .with_data(data);
        Self::open_target(ui, target);
    }

    /// Selects `path`; the explorer tree hands the file to the working diff
    /// pane (original `openTargetToSide({kind: "working_diff", focusPath,
    /// focusRequestId})`: one diff tab per workspace, refocused per request).
    fn select_file(&mut self, ui: &mut UiContext, compact: bool, path: &str) {
        let prefs = preferences(ui);
        if self.presentation(compact, &prefs) == Presentation::Tree {
            let target = PanelTarget::new("working_diff", self.host(), &self.target.workspace_id, self.cwd())
                .with_data(json!({ "focusPath": path, "focusRequestId": now_ms() }));
            Self::open_target(ui, target);
            return;
        }
        self.focus(ui, path);
    }

    /// Expands, selects and scrolls to `path`.
    pub fn focus(&mut self, ui: &UiContext, path: &str) {
        self.collapsed_files.remove(path);
        self.controller.select_file(ui, path);
        self.doc.reveal(path);
    }

    /// Original `useDiscardChangesAction`: only the uncommitted comparison.
    fn discard_supported(&self, ui: &UiContext) -> bool {
        !self.controller.is_commit
            && self.controller.mode == "uncommitted"
            && host::supports(ui, self.host(), "checkoutDiscardChanges")
    }

    /// Original `FileActionsContextMenu` for a changed file.
    fn file_menu(&mut self, ui: &UiContext, path: &str, anchor: Rect) {
        let file = self.controller.files.iter().find(|f| text(f, "path") == path);
        let old_path = file.map(|f| text(f, "oldPath")).filter(|p| !p.is_empty()).map(str::to_owned);
        let deleted = file.is_some_and(|f| change_kind(f) == ChangeKind::Deleted);
        let mut entries = Vec::new();
        if !deleted {
            entries.push(MenuEntry::Item(MenuItem::new("open", t("workspace.fileActions.openFile")).icon("file-text")));
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Item(MenuItem::new("copy", t("workspace.fileActions.copyPath")).icon("copy")));
        entries.push(MenuEntry::Item(MenuItem::new("copy-relative", t("workspace.fileActions.copyRelativePath")).icon("copy")));
        if host::supports(ui, self.host(), "fsEntryDuplicate") {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("duplicate", t("workspace.fileActions.duplicate")).icon("copy-plus")));
        }
        if self.discard_supported(ui) {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("revert", t("workspace.fileActions.revert")).icon("undo-2").destructive()));
        }
        self.open_overlay(Purpose::FileMenu { path: path.to_owned(), old_path }, OverlayKind::Menu { title: Some(basename(path).to_owned()), anchor, align_end: false, width: None, entries });
    }

    fn folder_menu(&mut self, ui: &UiContext, path: &str, anchor: Rect) {
        let mut entries = Vec::new();
        if !self.collapsed_folders.contains(path) {
            entries.push(MenuEntry::Item(MenuItem::new("collapse", t("workspace.fileActions.collapseFolder")).icon("folder-minus")));
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Item(MenuItem::new("copy", t("workspace.fileActions.copyPath")).icon("copy")));
        entries.push(MenuEntry::Item(MenuItem::new("copy-relative", t("workspace.fileActions.copyRelativePath")).icon("copy")));
        if host::supports(ui, self.host(), "fsEntryDuplicate") {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("duplicate", t("workspace.fileActions.duplicate")).icon("copy-plus")));
        }
        if self.discard_supported(ui) {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("revert", t("workspace.fileActions.revert")).icon("undo-2").destructive()));
        }
        self.open_overlay(Purpose::FolderMenu(path.to_owned()), OverlayKind::Menu { title: Some(basename(path).to_owned()), anchor, align_end: false, width: None, entries });
    }

    /// Original `discardPath`: confirm first.
    fn confirm_discard(&mut self, path: String, old_path: Option<String>) {
        let message = tr("workspace.fileActions.confirmRevert.message", &[("name", path.clone())]);
        self.open_overlay(Purpose::Discard { path, old_path }, OverlayKind::Confirm {
            title: t("workspace.fileActions.confirmRevert.title"),
            message,
            confirm: t("workspace.fileActions.confirmRevert.confirm"),
            destructive: true,
        });
    }

    fn duplicate(&mut self, ui: &UiContext, path: &str) {
        self.send(ui, "fs.entry.duplicate.request", json!({ "cwd": self.cwd(), "path": path }), Flow::Duplicate);
    }

    fn copy(ui: &mut UiContext, text: String) {
        ui.push(UiEffect::CopyText { text, toast: None });
    }

    fn line_menu(&mut self, target: LineTarget, anchor: Rect) {
        let line = &target.line;
        let mut entries = vec![
            MenuEntry::Item(MenuItem::new("copy", t("common.actions.copyLine")).icon("copy")),
            MenuEntry::Item(MenuItem::new("open", "Open file at line").icon("file-text")),
        ];
        if line.old_number.is_some() {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("old", "Comment on old line").icon("message-square-plus")));
        }
        if line.new_number.is_some() {
            if line.old_number.is_none() {
                entries.push(MenuEntry::Separator);
            }
            entries.push(MenuEntry::Item(MenuItem::new("new", "Comment on new line").icon("message-square-plus")));
        }
        let title = format!("{}:{}", target.path, line.new_number.or(line.old_number).unwrap_or_default());
        self.open_overlay(Purpose::LineMenu(target), OverlayKind::Menu { title: Some(title), anchor, align_end: true, width: None, entries });
    }

    /// Original `onComment`: a commit (or no draft) opens the file instead.
    fn comment(&mut self, cx: &mut Cx, ui: &mut UiContext, parts: &mut OverlayParts<'_>, target: &LineTarget, side: &str) {
        let number = if side == "old" { target.line.old_number } else { target.line.new_number };
        let Some(number) = number.filter(|_| !self.controller.is_commit && self.draft_key.is_some()) else {
            self.open_file(ui, &target.path, target.line.new_number.or(target.line.old_number));
            return;
        };
        self.prompt_comment(cx, parts, None, &target.path, side, number, "");
    }

    #[allow(clippy::too_many_arguments)]
    fn prompt_comment(&mut self, cx: &mut Cx, parts: &mut OverlayParts<'_>, id: Option<String>, path: &str, side: &str, line: i64, initial: &str) {
        self.overlay.open_sheet(
            cx,
            Purpose::Comment { id, path: path.to_owned(), side: side.to_owned(), line },
            SheetRequest::Comment {
                title: t("review.comment.label"),
                subtitle: comment_subtitle(path, side, Some(line)),
                placeholder: t("review.comment.placeholder"),
                initial: initial.to_owned(),
            },
            parts.input,
        );
    }

    /// Original `readRecipients`: the agent tabs open in this workspace.
    fn recipients(&self, ui: &UiContext) -> Vec<(String, String)> {
        host::tabs(ui)
            .iter()
            .filter(|tab| {
                tab.kind == "agent"
                    && tab.server_id == self.target.server_id
                    && tab.workspace_id == self.target.workspace_id
                    && tab.id.as_deref().is_some_and(|id| !id.is_empty())
            })
            .map(|tab| {
                let id = tab.id.clone().unwrap_or_default();
                let label = [host::agent_label(ui, self.host(), &id), tab.title.clone().unwrap_or_default(), t("sessionNavigation.untitled")]
                    .into_iter()
                    .find(|label| !label.trim().is_empty())
                    .unwrap_or_default();
                (id, label)
            })
            .collect()
    }

    /// Original `ReviewFeedbackAction._send`.
    fn send_feedback(&mut self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, agent_id: &str, recipient: &str) {
        let Some(key) = self.draft_key.clone() else { return };
        let comments = self.comments(store);
        let Some(snapshot) = build_attachment_snapshot(self.cwd(), &self.controller.mode, &self.controller.base_ref, &comments, &|path| {
            self.controller.patches.get(path)
        }) else {
            return;
        };
        if store.draft(&key).is_some_and(|d| d.phase.sending.is_some()) {
            return;
        }
        if !host::connected(ui, self.host()) {
            ui.toast(t("workspace.terminal.hostDisconnected"), true);
            return;
        }
        let sent = sent_comments(&comments, &snapshot);
        store.set_draft_phase(cx, &key, Some(snapshot.comment_count), None);
        let behavior = match host::preference(ui, "sendBehavior").as_str() {
            Some("interrupt") => "interrupt",
            Some("steer") | None => "steer",
            Some(_) => "interrupt",
        };
        let fields = json!({
            "agentId": agent_id,
            "text": t("review.feedback.prompt"),
            "messageId": uuid::Uuid::new_v4().to_string(),
            "activeTurnBehavior": behavior,
            "images": [],
            "attachments": [snapshot.attachment],
        });
        let label = tr("review.feedback.sent", &[("recipient", recipient.to_owned())]);
        self.send(ui, "send_agent_message_request", fields, Flow::Feedback { draft: key, sent, label });
    }

    /// Opens the overlay for a pressed region.
    #[allow(clippy::too_many_arguments)]
    fn activate(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        hit: Hit,
        anchor: Rect,
        secondary: bool,
        compact: bool,
    ) {
        let prefs = preferences(ui);
        match hit {
            Hit::Branch => {
                if host::connected(ui, self.host()) {
                    self.send(ui, "branch_suggestions_request", json!({ "cwd": self.cwd(), "limit": 200 }), Flow::BranchSuggestions(anchor));
                }
            }
            Hit::PrHint => {
                let target = PanelTarget::new("pull_request", self.host(), &self.target.workspace_id, self.cwd())
                    .with_title(t("panels.pullRequest.label"));
                Self::open_target(ui, target);
            }
            Hit::OpenForge => {
                let payload = store.checkout(self.host(), self.cwd()).and_then(|s| s.pr_payload.as_ref());
                if let Some(hint) = select_pr_hint(payload) {
                    ui.push(UiEffect::OpenExternal { url: hint.url });
                }
            }
            Hit::GitActions => {
                let checkout = self.checkout();
                let actions = checkout.build(ui, store);
                let mut list: Vec<_> = actions.primary.iter().cloned().collect();
                list.extend(actions.without_primary());
                if list.is_empty() {
                    return;
                }
                let entries = menu_entries(&checkout, store, &list);
                self.open_overlay(Purpose::GitActionsMenu, OverlayKind::Menu {
                    title: Some(t("workspace.header.actions.workspaceActions")),
                    anchor,
                    align_end: true,
                    width: None,
                    entries,
                });
            }
            Hit::Mode => {
                let mode = self.controller.mode.clone();
                let entries = vec![
                    MenuEntry::Item(MenuItem::new("all", t("workspace.git.diff.all")).selected(mode == "all")),
                    MenuEntry::Separator,
                    MenuEntry::Item(MenuItem::new("uncommitted", t("workspace.git.diff.uncommitted")).selected(mode == "uncommitted")),
                    MenuEntry::Separator,
                    MenuEntry::Item(
                        MenuItem::new("base", t("workspace.git.diff.committed"))
                            .hint(self.committed_description())
                            .selected(mode == "base"),
                    ),
                ];
                self.open_overlay(Purpose::ModeMenu, OverlayKind::Menu { title: Some(t("workspace.git.diff.diffMode")), anchor, align_end: false, width: Some(260.0), entries });
            }
            Hit::TreeToggle => self.tree_visible = !self.tree_visible,
            Hit::Refresh => self.refresh_checkout(cx, store),
            Hit::Options => self.options_menu(ui, anchor, compact),
            Hit::Toggle(which) => match which {
                "collapse" => self.toggle_collapse_all(),
                "split" => {
                    let split = Self::split(compact, &prefs);
                    Self::update_preferences(ui, &[("layout", json!(if split { "unified" } else { "split" }))]);
                }
                "whitespace" => Self::update_preferences(ui, &[("hideWhitespace", json!(!pref_flag(&prefs, "hideWhitespace")))]),
                "wrap" => Self::update_preferences(ui, &[("wrapLines", json!(!pref_flag(&prefs, "wrapLines")))]),
                _ => {}
            },
            Hit::Folder(path) => {
                self.selected_tree_path = Some(path.clone());
                if secondary {
                    self.folder_menu(ui, &path, anchor);
                } else if !self.collapsed_folders.remove(&path) {
                    self.collapsed_folders.insert(path);
                }
            }
            Hit::File(path) => {
                self.selected_tree_path = Some(path.clone());
                if secondary {
                    self.file_menu(ui, &path, anchor);
                } else {
                    self.select_file(ui, compact, &path);
                }
            }
            Hit::EmptyAction(mode) => store.select_comparison(cx, self.host(), &self.target.workspace_id, self.cwd(), mode),
            Hit::CommitsHeader => {
                self.commits_collapsed = !self.commits_collapsed;
                if !self.commits_collapsed {
                    self.controller.load_footer_commits(ui);
                }
            }
            Hit::Commit(sha) => {
                let subject = self
                    .controller
                    .commits
                    .iter()
                    .find(|c| text(c, "sha") == sha)
                    .map(|c| text(c, "subject").to_owned())
                    .unwrap_or_default();
                let target = PanelTarget::new("commit_diff", self.host(), &self.target.workspace_id, self.cwd())
                    .with_id(sha.clone())
                    .with_title(subject)
                    .with_data(json!({ "sha": sha }));
                Self::open_target(ui, target);
            }
            Hit::Jump => {
                // Original `_jumpToFile`: the fully expanded tree in an adaptive sheet.
                self.overlay.open_sheet(
                    cx,
                    Purpose::Jump,
                    SheetRequest::FileTree {
                        title: t("workspace.git.diff.jumpToFile.title"),
                        files: self.controller.files.clone(),
                        selected: self.selected_tree_path.clone(),
                    },
                    parts.input,
                );
            }
            Hit::Feedback => {
                let recipients = self.recipients(ui);
                if recipients.len() == 1 {
                    let (id, label) = recipients[0].clone();
                    self.send_feedback(cx, ui, store, &id, &label);
                } else if recipients.len() > 1 {
                    let entries = recipients
                        .iter()
                        .map(|(id, label)| MenuEntry::Item(MenuItem::new(id.clone(), label.clone()).icon("bot")))
                        .collect();
                    self.open_overlay(Purpose::Recipient, OverlayKind::Menu { title: Some(t("review.feedback.chooseAgent")), anchor, align_end: false, width: None, entries });
                }
            }
            Hit::Doc(doc_hit) => self.activate_doc(cx, ui, parts, store, doc_hit, anchor, secondary),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn activate_doc(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        hit: DocHit,
        anchor: Rect,
        secondary: bool,
    ) {
        match hit {
            DocHit::Header(path) => {
                if secondary {
                    self.file_menu(ui, &path, anchor);
                } else if !self.collapsed_files.remove(&path) {
                    self.collapsed_files.insert(path);
                }
            }
            DocHit::Expand { path, start } => self.doc.toggle_expand(&path, start),
            DocHit::Gutter { path, line, side } => {
                if let Some(target) = self.doc.line_target(&path, line) {
                    self.comment(cx, ui, parts, &target, side);
                }
            }
            DocHit::LineMenu { path, line } => {
                if let Some(target) = self.doc.line_target(&path, line) {
                    self.line_menu(target, anchor);
                }
            }
            DocHit::Retry(path) => self.controller.ensure_patch(ui, &path),
            DocHit::EditComment(id) => {
                let comment = self.comments(store).into_iter().find(|c| text(c, "id") == id);
                if let Some(comment) = comment {
                    self.prompt_comment(
                        cx,
                        parts,
                        Some(id),
                        text(&comment, "filePath"),
                        text(&comment, "side"),
                        int(&comment, "lineNumber"),
                        text(&comment, "body"),
                    );
                }
            }
            DocHit::DeleteComment(id) => {
                if let Some(key) = self.draft_key.clone() {
                    store.edit_draft(cx, &key, |draft| draft.delete(&id));
                }
            }
        }
    }

    /// Original `ChangesOptionsMenu`.
    fn options_menu(&mut self, ui: &UiContext, anchor: Rect, compact: bool) {
        let prefs = preferences(ui);
        let hide = pref_flag(&prefs, "hideWhitespace");
        let wrap = pref_flag(&prefs, "wrapLines");
        let inline = pref_flag(&prefs, "inlineDiff");
        let mut entries = Vec::new();
        if self.presentation(compact, &prefs) == Presentation::Tree {
            entries.push(MenuEntry::Item(MenuItem::new("open-diff", t("workspace.git.diff.openDiffTab")).icon("maximize")));
        } else {
            if !self.controller.files.is_empty() {
                let collapsed = self.all_files_collapsed();
                entries.push(MenuEntry::Item(
                    MenuItem::new(
                        "collapse",
                        t(if collapsed { "workspace.git.diff.expandAllFiles" } else { "workspace.git.diff.collapseAllFiles" }),
                    )
                    .icon(if collapsed { "list-chevrons-up-down" } else { "list-chevrons-down-up" }),
                ));
                entries.push(MenuEntry::Separator);
            }
            if Self::can_split(compact) {
                entries.push(MenuEntry::Item(
                    MenuItem::new("split", t("workspace.git.diff.split")).icon("columns-2").selected(Self::split(compact, &prefs)),
                ));
            }
            entries.push(MenuEntry::Item(
                MenuItem::new("whitespace", t(if hide { "workspace.git.diff.showWhitespace" } else { "workspace.git.diff.hideWhitespace" }))
                    .icon("pilcrow")
                    .selected(hide),
            ));
            entries.push(MenuEntry::Item(
                MenuItem::new("wrap", t(if wrap { "workspace.git.diff.scrollLongLines" } else { "workspace.git.diff.wrapLongLines" }))
                    .icon("wrap-text")
                    .selected(wrap),
            ));
        }
        if !compact {
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItem::new("inline-diff", t("workspace.git.diff.inlineDiff")).selected(inline)));
        }
        self.open_overlay(Purpose::OptionsMenu, OverlayKind::Menu { title: Some(t("workspace.git.diff.options")), anchor, align_end: true, width: Some(240.0), entries });
    }

    /// Routes an event to the open overlay; returns true while one is open.
    pub fn handle_overlay(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        compact: bool,
    ) -> bool {
        if !self.overlay.is_open() {
            return false;
        }
        if let Some(result) = self.overlay.handle(cx, event, parts.input) {
            self.on_overlay(cx, ui, parts, store, result, compact);
        }
        self.overlay.blocks_input()
    }

    fn on_overlay(
        &mut self,
        cx: &mut Cx,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        result: OverlayResult<Purpose>,
        compact: bool,
    ) {
        let (host, cwd) = (self.host().to_owned(), self.cwd().to_owned());
        match result {
            OverlayResult::Chose(Purpose::ModeMenu, mode) => {
                store.select_comparison(cx, &host, &self.target.workspace_id, &cwd, &mode);
            }
            OverlayResult::Chose(Purpose::OptionsMenu, value) => {
                let prefs = preferences(ui);
                match value.as_str() {
                    "open-diff" => {
                        let target = PanelTarget::new("working_diff", &host, &self.target.workspace_id, &cwd).with_title("Diff");
                        Self::open_target(ui, target);
                    }
                    "collapse" => self.toggle_collapse_all(),
                    "split" => {
                        let split = Self::split(compact, &prefs);
                        Self::update_preferences(ui, &[("layout", json!(if split { "unified" } else { "split" }))]);
                    }
                    "whitespace" => Self::update_preferences(ui, &[("hideWhitespace", json!(!pref_flag(&prefs, "hideWhitespace")))]),
                    "wrap" => Self::update_preferences(ui, &[("wrapLines", json!(!pref_flag(&prefs, "wrapLines")))]),
                    "inline-diff" => Self::update_preferences(ui, &[("inlineDiff", json!(!pref_flag(&prefs, "inlineDiff")))]),
                    _ => {}
                }
            }
            OverlayResult::Chose(Purpose::GitActionsMenu, id) => {
                let checkout = self.checkout();
                let actions = checkout.build(ui, store);
                let action = actions.primary.iter().chain(actions.secondary.iter()).find(|a| a.id == id).cloned();
                if let Some(action) = action
                    && let Some(ArchiveConfirm { title, message, confirm, workspace_id }) = checkout.run(cx, ui, store, &action)
                {
                    self.open_overlay(Purpose::ArchiveConfirm(workspace_id), OverlayKind::Confirm { title, message, confirm, destructive: true });
                }
            }
            OverlayResult::Confirmed(Purpose::ArchiveConfirm(workspace_id)) => {
                self.checkout().archive_confirmed(cx, ui, store, &workspace_id);
            }
            OverlayResult::Chose(Purpose::Branch, branch) => {
                if Some(&branch) != self.current_branch().as_ref() {
                    self.send(ui, "checkout_switch_branch_request", json!({ "cwd": cwd, "branch": branch }), Flow::Switch(branch.clone()));
                }
            }
            OverlayResult::Confirmed(Purpose::StashAndSwitch(branch)) => {
                let mut fields = json!({ "cwd": cwd });
                if let Some(current) = self.current_branch() {
                    fields["branch"] = json!(current);
                }
                self.send(ui, "stash_save_request", fields, Flow::StashSave(branch));
            }
            OverlayResult::Confirmed(Purpose::RestoreStash(index)) => {
                self.send(ui, "stash_pop_request", json!({ "cwd": cwd, "stashIndex": index }), Flow::StashPop);
            }
            OverlayResult::Confirmed(Purpose::Discard { path, old_path }) => {
                let mut paths = vec![json!(path)];
                if let Some(old) = old_path.filter(|p| !p.is_empty()) {
                    paths.push(json!(old));
                }
                store.run_success_action(
                    cx,
                    &host,
                    &cwd,
                    "discard-changes",
                    vec![("checkout.discard_changes.request", json!({ "paths": paths }))],
                    "workspace.fileActions.confirmRevert.failed",
                );
            }
            OverlayResult::Chose(Purpose::FileMenu { path, old_path, .. }, action) => match action.as_str() {
                "open" => self.open_file(ui, &path, None),
                "copy" => Self::copy(ui, absolute_path(&cwd, &path)),
                "copy-relative" => Self::copy(ui, path),
                "duplicate" => self.duplicate(ui, &path),
                "revert" => self.confirm_discard(path, old_path),
                _ => {}
            },
            OverlayResult::Chose(Purpose::FolderMenu(path), action) => match action.as_str() {
                // Original handleCollapseFolder: the folder and every folder under it.
                "collapse" => collapse_folder_recursive(&self.controller.files, &path, &mut self.collapsed_folders),
                "copy" => Self::copy(ui, absolute_path(&cwd, &path)),
                "copy-relative" => Self::copy(ui, path),
                "duplicate" => self.duplicate(ui, &path),
                // Original DiffFolderRow revert: the folder path itself.
                "revert" => self.confirm_discard(path, None),
                _ => {}
            },
            OverlayResult::Chose(Purpose::LineMenu(target), action) => match action.as_str() {
                "copy" => Self::copy(ui, target.line.content.clone()),
                "open" => self.open_file(ui, &target.path, target.line.new_number.or(target.line.old_number)),
                "old" => self.comment(cx, ui, parts, &target, "old"),
                "new" => self.comment(cx, ui, parts, &target, "new"),
                _ => {}
            },
            OverlayResult::Text(Purpose::Comment { id, path, side, line }, body) => {
                if let Some(key) = self.draft_key.clone() {
                    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
                    let new_id = uuid::Uuid::new_v4().to_string();
                    store.edit_draft(cx, &key, |draft| draft.save_comment(id.as_deref(), &path, &side, line, &body, new_id, now));
                }
            }
            OverlayResult::Chose(Purpose::Recipient, agent_id) => {
                let label = self.recipients(ui).into_iter().find(|(id, _)| *id == agent_id).map(|(_, l)| l).unwrap_or_default();
                self.send_feedback(cx, ui, store, &agent_id, &label);
            }
            OverlayResult::Chose(Purpose::Jump, path) => self.select_file(ui, compact, &path),
            _ => {}
        }
    }

    /// The original desktop `Tooltip` copy of a hovered region.
    fn tooltip_for(&self, hit: &Hit, ui: &UiContext, store: &GitStore, compact: bool) -> Option<String> {
        let prefs = preferences(ui);
        Some(match hit {
            Hit::Branch => t("branchSwitcher.triggerTooltip"),
            Hit::PrHint => t("panels.pullRequest.label"),
            Hit::OpenForge => {
                let payload = store.checkout(self.host(), self.cwd()).and_then(|s| s.pr_payload.as_ref());
                let hint = select_pr_hint(payload)?;
                tr("workspace.git.pr.actions.openOn", &[("brand", forge_presentation(Some(&hint.forge)).brand_label)])
            }
            Hit::TreeToggle => t(if self.tree_visible { "workspace.tree.hideFolderTree" } else { "workspace.tree.showFolderTree" }),
            Hit::Refresh => t(if self.refreshing(store) { "workspace.git.diff.refreshing" } else { "workspace.git.diff.refresh" }),
            Hit::Options => t("workspace.git.diff.options"),
            Hit::Toggle("collapse") => {
                t(if self.all_files_collapsed() { "workspace.git.diff.expandAllFiles" } else { "workspace.git.diff.collapseAllFiles" })
            }
            Hit::Toggle("split") => {
                t(if Self::split(compact, &prefs) { "workspace.git.diff.switchToUnified" } else { "workspace.git.diff.switchToSplit" })
            }
            Hit::Toggle("whitespace") => {
                t(if pref_flag(&prefs, "hideWhitespace") { "workspace.git.diff.showWhitespace" } else { "workspace.git.diff.hideWhitespace" })
            }
            Hit::Toggle("wrap") => {
                t(if pref_flag(&prefs, "wrapLines") { "workspace.git.diff.scrollLongLines" } else { "workspace.git.diff.wrapLongLines" })
            }
            Hit::Folder(path) | Hit::File(path) | Hit::Doc(DocHit::Header(path)) => path.clone(),
            _ => return None,
        })
    }

    /// Pointer input outside overlays; returns true when a redraw is due.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_pointer(
        &mut self,
        cx: &mut Cx,
        event: &Event,
        area: Area,
        ui: &mut UiContext,
        parts: &mut OverlayParts<'_>,
        store: &mut GitStore,
        compact: bool,
    ) -> bool {
        let wrap = pref_flag(&preferences(ui), "wrapLines");
        let hit = event.hits(cx, area);
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
            self.horizontal_scroll.reset();
        }
        let dy = self.touch_scroll.handle_event(cx, event, &hit);
        let dx = self.horizontal_scroll.handle_axis_event(cx, event, &hit, Vec2Index::X);
        let origin = self.touch_scroll.origin();
        if !self.doc.contains(origin) {
            self.horizontal_scroll.reset();
        }
        if dy.is_some() || dx.is_some() {
            self.pressed = None;
            if self.tree_area.contains(origin) {
                let max = (self.tree_content - self.tree_area.size.y).max(0.0);
                return self.touch_scroll.apply_delta(&mut self.tree_scroll, dy.unwrap_or(0.0), max);
            }
            if self.commits_area.contains(origin) {
                let max = (self.commits_content - self.commits_area.size.y).max(0.0);
                return self.touch_scroll.apply_delta(&mut self.commits_scroll, dy.unwrap_or(0.0), max);
            }
            if self.doc.contains(origin) {
                let mut changed = false;
                if let Some(delta) = dy {
                    let moved = self.doc.scroll_by(dvec2(0.0, delta), wrap);
                    if !moved { self.touch_scroll.stop_at_edge(); }
                    changed |= moved;
                }
                if let Some(delta) = dx {
                    let moved = self.doc.scroll_by(dvec2(delta, 0.0), wrap);
                    if !moved { self.horizontal_scroll.stop_at_edge(); }
                    changed |= moved;
                }
                return changed;
            }
            self.touch_scroll.reset();
            self.horizontal_scroll.reset();
        }
        match hit {
            Pointer::FingerHoverIn(e) | Pointer::FingerHoverOver(e) => {
                let (anchor, hovered) = self.hits.find(e.abs).map(|(anchor, hit)| (anchor, Some(hit))).unwrap_or_default();
                let doc_hovered = match &hovered {
                    Some(Hit::Doc(doc)) => Some(doc.clone()),
                    _ => None,
                };
                // Inside the document's `SelectionArea` text shows the text cursor.
                cx.set_cursor(if hovered.is_some() {
                    MouseCursor::Hand
                } else if self.doc.contains(e.abs) {
                    MouseCursor::Text
                } else {
                    MouseCursor::Default
                });
                if hovered != self.hovered {
                    let tip = hovered.as_ref().and_then(|hit| self.tooltip_for(hit, ui, store, compact));
                    tooltip_at(ui, tip.as_deref(), anchor, 0.3);
                    self.hovered = hovered;
                    self.doc.hovered = doc_hovered;
                    return true;
                }
            }
            Pointer::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                    self.doc.hovered = None;
                    return true;
                }
            }
            Pointer::FingerScroll(e) => {
                let delta = if e.modifiers.shift { dvec2(e.scroll.y, e.scroll.x) } else { e.scroll };
                if self.tree_area.contains(e.abs) {
                    let max = (self.tree_content - self.tree_area.size.y).max(0.0);
                    self.tree_scroll = (self.tree_scroll + delta.y).clamp(0.0, max);
                    return true;
                }
                if self.commits_area.contains(e.abs) {
                    let max = (self.commits_content - self.commits_area.size.y).max(0.0);
                    self.commits_scroll = (self.commits_scroll + delta.y).clamp(0.0, max);
                    return true;
                }
                if self.doc.contains(e.abs) {
                    return self.doc.scroll_by(delta, wrap);
                }
            }
            Pointer::FingerDown(e) => {
                // Original Tooltip hides when its control is pressed.
                tooltip_at(ui, None, Rect::default(), 0.3);
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
                // The document is one `SelectionArea`: a mouse press starts a
                // selection (double click a word, triple click the whole text).
                let had_selection = self.doc.selection.take().is_some();
                if !e.device.is_touch() && e.is_primary_hit() && self.doc.contains(e.abs) {
                    cx.set_key_focus(area);
                    if let Some(pos) = self.doc.pos_at(e.abs) {
                        self.doc.selection = Some(match e.tap_count {
                            1 => (pos, pos),
                            2 => self.doc.word_at(pos),
                            _ => self.doc.paragraph_at(pos),
                        });
                        self.doc.drag_pointer = Some(e.abs);
                    }
                    return true;
                }
                return had_selection;
            }
            Pointer::FingerLongPress(e) => {
                // Touch long-press opens the context menu.
                if let Some((rect, hit)) = self.hits.find(e.abs)
                    && matches!(hit, Hit::File(_) | Hit::Folder(_) | Hit::Doc(DocHit::Header(_)))
                {
                    self.pressed = None;
                    self.activate(cx, ui, parts, store, hit, rect, true, compact);
                    return true;
                }
                // Touch selection in the document: the word under the finger.
                if self.doc.contains(e.abs)
                    && let Some(pos) = self.doc.pos_at(e.abs)
                {
                    self.pressed = None;
                    self.doc.selection = Some(self.doc.word_at(pos));
                    cx.set_key_focus(area);
                    return true;
                }
            }
            Pointer::FingerMove(e) if self.doc.drag_pointer.is_some() => {
                // A drag extends the selection; leaving the viewport auto-scrolls.
                if e.move_distance() > 4.0 {
                    self.pressed = None;
                }
                self.doc.drag_pointer = Some(e.abs);
                let viewport = self.doc.viewport();
                let inside = dvec2(
                    e.abs.x.clamp(viewport.pos.x, viewport.pos.x + viewport.size.x),
                    e.abs.y.clamp(viewport.pos.y + 1.0, viewport.pos.y + viewport.size.y - 1.0),
                );
                if let (Some(pos), Some((anchor, _))) = (self.doc.pos_at(inside), self.doc.selection) {
                    self.doc.selection = Some((anchor, pos));
                }
                return true;
            }
            Pointer::FingerUp(e) => {
                self.doc.drag_pointer = None;
                let pressed = self.pressed.take();
                if !e.is_over || e.has_long_press_occurred || (e.device.is_touch() && !e.was_tap()) {
                    return false;
                }
                let secondary = e.mouse_button().is_some_and(|b| b.is_secondary());
                let Some((rect, hit)) = self.hits.find(e.abs) else { return false };
                if pressed.as_ref() != Some(&hit) {
                    return false;
                }
                self.activate(cx, ui, parts, store, hit, rect, secondary, compact);
                return true;
            }
            Pointer::TextCopy(ce) => {
                if let Some(text) = self.doc.selected_text() {
                    *ce.response.borrow_mut() = Some(text);
                }
            }
            Pointer::KeyDown(ke) if ke.key_code == KeyCode::KeyA && (ke.modifiers.control || ke.modifiers.logo) => {
                self.doc.select_all();
                return true;
            }
            Pointer::KeyFocusLost(_) => {
                return self.doc.selection.take().is_some();
            }
            _ => {}
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Drawing.

/// The width a label trigger needs (mirrors `label_trigger`).
fn label_trigger_width(cx: &mut Cx2d, paint: &mut GitPaint, label: &str, leading: bool, chevron: bool) -> f64 {
    8.0 + if leading { 18.0 } else { 0.0 } + paint.measure(cx, Font::Regular, ui_px(12.0), label).x + if chevron { 16.0 } else { 0.0 }
}

/// Original `PullRequestStateIcon`.
fn pr_state_icon(state: &str) -> (&'static str, Vec4) {
    let colors = palette();
    match state {
        "merged" => ("git-merge", colors.merged),
        "closed" => ("git-pull-request-closed", colors.danger),
        _ => ("git-pull-request", colors.success),
    }
}

impl ChangesView {
    fn push_hit(&mut self, area: Rect, hit: Hit) {
        self.hits.push(area, hit);
    }

    fn is_hovered(&self, hit: &Hit) -> bool {
        self.hovered.as_ref() == Some(hit)
    }

    /// A trailing toolbar button ending at `right`; returns its left edge.
    #[allow(clippy::too_many_arguments)]
    fn toolbar_icon(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, compact: bool, right: f64, center_y: f64, icon: Option<&str>, selected: bool, hit: Hit) -> f64 {
        let hovered = self.is_hovered(&hit);
        let button = toolbar_button(cx, paint, compact, right, center_y, icon, selected, true, hovered);
        self.push_hit(button, hit);
        button.pos.x - 1.0
    }

    #[allow(clippy::too_many_arguments)]
    fn refresh_button(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, store: &GitStore, compact: bool, right: f64, center_y: f64) -> f64 {
        if self.refreshing(store) {
            let size = toolbar_button_size(compact);
            let colors = palette();
            paint.spinner(cx, dvec2(right - size / 2.0, center_y), super::chrome::toolbar_icon_size(compact), colors.extra_muted, cx.seconds_since_app_start());
            self.spinning = true;
            return right - size - 1.0;
        }
        self.toolbar_icon(cx, paint, compact, right, center_y, Some("rotate-cw"), false, Hit::Refresh)
    }

    /// Original `ChangesToolbarRow` with the repository identity.
    #[allow(clippy::too_many_arguments)]
    fn repository_toolbar(&mut self, cx: &mut Cx2d, ui: &UiContext, store: &GitStore, paint: &mut GitPaint, band: Rect, sidebar: bool, compact: bool) {
        toolbar_band(cx, paint, band, sidebar);
        let center_y = band.pos.y + (band.size.y - 1.0) / 2.0;
        let colors = palette();
        let payload = store.checkout(self.host(), self.cwd()).and_then(|s| s.pr_payload.as_ref());
        let hint = select_pr_hint(payload);
        // Wide layouts carry the git actions in the workspace header.
        let show_actions = compact && {
            let actions = self.checkout().build(ui, store);
            actions.primary.is_some() || !actions.without_primary().is_empty()
        };
        let mut right = band.pos.x + band.size.x - super::chrome::toolbar_trailing_padding(compact, show_actions);
        if show_actions {
            let (w, h) = if compact { (48.0, 28.0) } else { (42.0, 26.0) };
            let trigger = rect(right - w, center_y - h / 2.0, w, h);
            let hovered = self.is_hovered(&Hit::GitActions) || matches!(self.overlay.purpose(), Some(Purpose::GitActionsMenu));
            paint.rounded(cx, trigger, if hovered { colors.surface2 } else { vec4(0.0, 0.0, 0.0, 0.0) }, 6.0, Some((colors.border_accent, 1.0)));
            paint.icon(cx, "git-branch", 16.0, colors.muted, dvec2(trigger.pos.x + w / 2.0 - 8.0, center_y));
            paint.icon(cx, "chevron-down", 12.0, colors.extra_muted, dvec2(trigger.pos.x + w / 2.0 + 8.0, center_y));
            self.push_hit(trigger, Hit::GitActions);
            right = trigger.pos.x - 4.0;
        }
        if let Some(hint) = &hint {
            right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("external-link"), false, Hit::OpenForge);
            let presentation = forge_presentation(Some(&hint.forge));
            let label = format!("{}{}", presentation.number_prefix, hint.number);
            let width = label_trigger_width(cx, paint, &label, true, false);
            let (icon, color) = pr_state_icon(hint.state);
            let hovered = self.is_hovered(&Hit::PrHint);
            let trigger = label_trigger(cx, paint, right - width, center_y, width, &label, Some((icon, color)), false, hovered);
            self.push_hit(trigger, Hit::PrHint);
            right = trigger.pos.x - 1.0;
        }
        if let Some(branch) = self.current_branch() {
            let left = band.pos.x + 8.0;
            let max = (right - 8.0 - left).max(0.0);
            let hovered = self.is_hovered(&Hit::Branch) || matches!(self.overlay.purpose(), Some(Purpose::Branch));
            let trigger = label_trigger(cx, paint, left, center_y, max, &branch, None, true, hovered);
            self.push_hit(trigger, Hit::Branch);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn comparison_toolbar(&mut self, cx: &mut Cx2d, ui: &UiContext, store: &GitStore, paint: &mut GitPaint, band: Rect, presentation: Presentation, compact: bool) {
        toolbar_band(cx, paint, band, presentation == Presentation::Tree);
        let center_y = band.pos.y + (band.size.y - 1.0) / 2.0;
        let has_changes = !self.controller.files.is_empty();
        let mut right = band.pos.x + band.size.x - super::chrome::toolbar_trailing_padding(compact, false);
        right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("ellipsis"), false, Hit::Options);
        if host::supports(ui, self.host(), "checkoutRefresh") {
            right = self.refresh_button(cx, paint, store, compact, right, center_y);
        }
        if presentation == Presentation::Combined && !compact && has_changes {
            right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("folder-tree"), self.tree_visible, Hit::TreeToggle);
        }
        let left = band.pos.x + 8.0;
        let label = t(mode_label(&self.controller.mode));
        let stat_width = if self.controller.loading {
            0.0
        } else {
            let (adds, dels) = self.totals();
            measure_diff_stat(cx, paint, adds, dels) + 4.0
        };
        let max = (right - 8.0 - left - stat_width).max(0.0);
        let hovered = self.is_hovered(&Hit::Mode);
        let trigger = label_trigger(cx, paint, left, center_y, max, &label, None, true, hovered);
        self.push_hit(trigger, Hit::Mode);
        if !self.controller.loading {
            let (adds, dels) = self.totals();
            diff_stat(cx, paint, trigger.pos.x + trigger.size.x + 4.0, center_y, adds, dels, false);
        }
    }

    fn totals(&self) -> (i64, i64) {
        self.controller
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + int(f, "additions"), d + int(f, "deletions")))
    }

    /// Original `ChangesDiffOnlyToolbar` with the bare toggles.
    #[allow(clippy::too_many_arguments)]
    fn diff_only_toolbar(&mut self, cx: &mut Cx2d, ui: &UiContext, store: &GitStore, paint: &mut GitPaint, band: Rect, compact: bool, prefs: &Map<String, Value>) {
        toolbar_band(cx, paint, band, false);
        let colors = palette();
        let center_y = band.pos.y + (band.size.y - 1.0) / 2.0;
        let mut right = band.pos.x + band.size.x - super::chrome::toolbar_trailing_padding(compact, false);
        let wrap = pref_flag(prefs, "wrapLines");
        let hide = pref_flag(prefs, "hideWhitespace");
        right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("wrap-text"), wrap, Hit::Toggle("wrap"));
        right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("pilcrow"), hide, Hit::Toggle("whitespace"));
        if Self::can_split(compact) {
            right = self.toolbar_icon(cx, paint, compact, right, center_y, Some("columns-2"), Self::split(compact, prefs), Hit::Toggle("split"));
        }
        if !self.controller.files.is_empty() {
            let icon = if self.all_files_collapsed() { "list-chevrons-up-down" } else { "list-chevrons-down-up" };
            right = self.toolbar_icon(cx, paint, compact, right, center_y, Some(icon), false, Hit::Toggle("collapse"));
        }
        if host::supports(ui, self.host(), "checkoutRefresh") {
            right = self.refresh_button(cx, paint, store, compact, right, center_y);
        }
        let title = self.target.title.clone().unwrap_or_else(|| t("panels.diff.commitSubtitle"));
        let left = band.pos.x + 8.0 + 4.0;
        paint.text_centered(cx, Font::Regular, ui_px(14.0), colors.foreground, left, band.pos.y, band.size.y - 1.0, &title, (right - 8.0 - left).max(0.0));
    }

    /// Original `DiffBodyContent` states: 64px from the top, centred copy
    /// (destructive for errors) under an optional 32px spinner.
    #[allow(clippy::too_many_arguments)]
    fn state(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, body: Rect, message: Option<&str>, spinner: bool, error: bool, action: Option<(&str, &'static str)>) {
        let colors = palette();
        let mut y = body.pos.y + 64.0;
        let inner = body.size.x - 48.0;
        if spinner {
            paint.spinner(cx, dvec2(body.pos.x + body.size.x / 2.0, y + 16.0), 32.0, colors.muted, cx.seconds_since_app_start());
            self.spinning = true;
            y += 32.0 + 16.0;
        }
        if let Some(message) = message {
            let size = ui_px(14.0);
            let width = paint.measure(cx, Font::Regular, size, message).x.min(inner);
            let height = paint.text_wrapped(cx, Font::Regular, size, if error { colors.destructive } else { colors.muted }, dvec2(body.pos.x + (body.size.x - width) / 2.0, y), message, inner);
            y += height + 8.0;
        }
        if let Some((label, mode)) = action {
            let compact = crate::common::is_compact_layout(cx);
            let width = measure_button(cx, paint, label, false, ButtonSize::Xs, compact);
            let height = button_height(ButtonSize::Xs, compact);
            let area = rect(body.pos.x + (body.size.x - width) / 2.0, y, width, height);
            let hovered = self.is_hovered(&Hit::EmptyAction(mode));
            button(cx, paint, area, label, None, ButtonVariant::Ghost, ButtonSize::Xs, compact, true, hovered);
            self.push_hit(area, Hit::EmptyAction(mode));
        }
    }

    /// Original `DiffTooLargeState`.
    fn too_large_state(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, body: Rect) {
        let colors = palette();
        let center_x = body.pos.x + body.size.x / 2.0;
        let mut y = body.pos.y + 64.0;
        paint.icon(cx, "file-warning", 24.0, colors.muted, dvec2(center_x, y + 12.0));
        y += 24.0 + 12.0;
        let inner = body.size.x - 48.0;
        for (key, font, size, color) in [
            ("workspace.git.diff.previewTooLargeTitle", Font::Medium, ui_px(14.0), colors.foreground),
            ("workspace.git.diff.previewTooLargeDescription", Font::Regular, ui_px(12.0), colors.muted),
        ] {
            let copy = t(key);
            let width = paint.measure(cx, font, size, &copy).x.min(inner);
            y += paint.text_wrapped(cx, font, size, color, dvec2(center_x - width / 2.0, y), &copy, inner) + 4.0;
        }
    }

    /// Original `computeChangesEmptyAction`.
    fn empty_action(&self, hide_whitespace: bool) -> Option<(&'static str, &'static str)> {
        let status = self.controller.status.as_ref()?;
        if hide_whitespace || self.controller.is_commit {
            return None;
        }
        if self.controller.mode == "base" && flag(status, "isDirty") {
            Some(("workspace.git.diff.seeUncommittedChanges", "uncommitted"))
        } else if self.controller.mode == "uncommitted" && ahead_count(status) > 0 {
            Some(("workspace.git.diff.seeCommittedChanges", "base"))
        } else {
            None
        }
    }

    /// Original `ChangedFilesTree`.
    fn draw_tree(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, bottom_padding: f64) {
        let rows = changed_tree_rows(&self.controller.files, &self.collapsed_folders);
        let row_height = TREE_ROW_HEIGHT.max(paint.measure(cx, Font::Regular, ui_px(14.0), "Ag").y + 8.0);
        self.tree_area = area;
        self.tree_content = rows.len() as f64 * row_height + bottom_padding;
        self.tree_scroll = self.tree_scroll.clamp(0.0, (self.tree_content - area.size.y).max(0.0));
        paint.begin_clip(cx, area);
        let first = (self.tree_scroll / row_height).floor() as usize;
        for (index, row) in rows.iter().enumerate().skip(first) {
            let top = area.pos.y + index as f64 * row_height - self.tree_scroll;
            if top > area.pos.y + area.size.y {
                break;
            }
            let band = rect(area.pos.x, top, area.size.x, row_height);
            let (hit, path) = match row {
                TreeRow::Folder { path, .. } => (Hit::Folder(path.clone()), path.as_str()),
                TreeRow::File { index, .. } => {
                    let path = text(&self.controller.files[*index], "path");
                    (Hit::File(path.to_owned()), path)
                }
            };
            let selected = self.selected_tree_path.as_deref() == Some(path);
            let hovered = self.is_hovered(&hit);
            if !file_tree_row(cx, paint, band, row, &self.controller.files, &self.collapsed_folders, hovered, selected) {
                self.icons_pending = true;
            }
            self.hits.push_clipped(band, area, hit);
        }
        paint.end_clip(cx);
    }

    /// Original `CommitsSection`; returns its height.
    fn commits_height(&self, cx: &mut Cx2d, paint: &mut GitPaint, window_height: f64) -> f64 {
        let header = 8.0 + paint.measure(cx, Font::Regular, ui_px(14.0), "Ag").y + 8.0;
        if self.commits_collapsed {
            return header + 1.0;
        }
        let commits = self.workspace_commits();
        let row_h = 28.0_f64.max(ui_px(14.0) * 1.4 + 8.0);
        // Error row (8 + 16 + 8) and loading spinner row (4 + 20 + 8) are both 32px.
        let content = if self.controller.commits_error.is_some() || (self.controller.commits_loading && commits.is_empty()) {
            32.0 + (ui_px(12.0) - 12.0).max(0.0) * 1.4
        } else if commits.is_empty() {
            28.0 + (ui_px(12.0) - 12.0).max(0.0) * 1.4
        } else {
            (commits.len() as f64 * row_h + 4.0).min(window_height * 0.3)
        };
        header + content + 1.0
    }

    fn workspace_commits(&self) -> Vec<&Value> {
        self.controller.commits.iter().filter(|c| !flag(c, "isOnBase")).collect()
    }

    fn draw_commits(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect) {
        let colors = palette();
        paint.fill(cx, rect(area.pos.x, area.pos.y, area.size.x, 1.0), colors.border);
        let header_height = 8.0 + paint.measure(cx, Font::Regular, ui_px(14.0), "Ag").y + 8.0;
        let header = rect(area.pos.x, area.pos.y + 1.0, area.size.x, header_height);
        let center_y = header.pos.y + header_height / 2.0;
        let x = area.pos.x + 12.0;
        paint.icon(cx, if self.commits_collapsed { "chevron-right" } else { "chevron-down" }, 14.0, colors.muted, dvec2(x + 8.0, center_y));
        let title = t("workspace.git.diff.commits.title");
        let title_width = paint.text_centered(cx, Font::Regular, ui_px(14.0), colors.foreground, x + 16.0 + 8.0, header.pos.y, header_height, &title, area.size.x - 48.0);
        let commits: Vec<Value> = self.workspace_commits().into_iter().cloned().collect();
        let loaded = !self.controller.commits_loading && self.controller.commits_error.is_none() && (!commits.is_empty() || !self.commits_collapsed);
        if loaded && !commits.is_empty() {
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, x + 24.0 + title_width + 8.0, header.pos.y, header_height, &commits.len().to_string(), 60.0);
        }
        self.push_hit(header, Hit::CommitsHeader);
        if self.commits_collapsed {
            self.commits_area = Rect::default();
            return;
        }
        let content = rect(area.pos.x, header.pos.y + header_height, area.size.x, area.pos.y + area.size.y - header.pos.y - header_height);
        let small = ui_px(12.0);
        if self.controller.commits_error.is_some() {
            paint.text_fit(cx, Font::Regular, small, colors.danger, dvec2(content.pos.x + 8.0, content.pos.y + 8.0), &t("workspace.git.diff.commits.loadError"), content.size.x - 20.0);
            return;
        }
        if self.controller.commits_loading && commits.is_empty() {
            // Original `_CommitsSkeleton`.
            let y = content.pos.y + 4.0;
            let fill = colors.surface2;
            let mut cursor = content.pos.x + 8.0;
            paint.rounded(cx, rect(cursor, y + 6.0, 8.0, 8.0), fill, 4.0, None);
            cursor += 16.0;
            paint.rounded(cx, rect(cursor, y + 5.0, 48.0, 10.0), fill, 4.0, None);
            cursor += 56.0;
            let end = content.pos.x + content.size.x - 8.0 - 16.0 - 8.0 - 40.0 - 8.0;
            paint.rounded(cx, rect(cursor, y + 4.0, (end - cursor).max(0.0), 12.0), fill, 4.0, None);
            paint.rounded(cx, rect(end + 8.0, y + 5.0, 40.0, 10.0), fill, 4.0, None);
            return;
        }
        if commits.is_empty() {
            let copy = tr("workspace.git.diff.commits.noneAhead", &[("baseRef", self.base_label())]);
            paint.text_fit(cx, Font::Regular, small, colors.muted, dvec2(content.pos.x + 8.0, content.pos.y + 4.0), &copy, content.size.x - 20.0);
            return;
        }
        self.commits_area = content;
        let row_h = 28.0_f64.max(ui_px(14.0) * 1.4 + 8.0);
        self.commits_content = commits.len() as f64 * row_h + 4.0;
        self.commits_scroll = self.commits_scroll.clamp(0.0, (self.commits_content - content.size.y).max(0.0));
        paint.begin_clip(cx, content);
        let now = now_ms();
        let count = commits.len();
        for (index, commit) in commits.iter().enumerate() {
            let top = content.pos.y + index as f64 * row_h - self.commits_scroll;
            if top + row_h < content.pos.y || top > content.pos.y + content.size.y {
                continue;
            }
            let band = rect(content.pos.x, top, content.size.x, row_h);
            let sha = text(commit, "sha").to_owned();
            let hit = Hit::Commit(sha);
            if self.is_hovered(&hit) {
                paint.fill(cx, band, colors.sidebar_hover);
            }
            let center_y = top + row_h / 2.0;
            // Original `CommitGraphNode`: rail, ring, filled once pushed.
            let rail = if flag(commit, "isOnBase") { colors.muted } else { colors.accent };
            let node_x = band.pos.x + 8.0 + 4.0;
            if count > 1 {
                if index > 0 {
                    paint.fill(cx, rect(node_x - 1.0, top, 2.0, row_h / 2.0), rail);
                }
                if index + 1 < count {
                    paint.fill(cx, rect(node_x - 1.0, center_y, 2.0, row_h / 2.0), rail);
                }
            }
            let fill = if flag(commit, "isOnRemote") { rail } else { colors.surface0 };
            paint.rounded(cx, rect(node_x - 4.0, center_y - 4.0, 8.0, 8.0), fill, 4.0, Some((rail, 2.0)));
            let mut x = band.pos.x + 8.0 + 8.0 + 8.0;
            paint.text_centered(cx, Font::Mono, small, colors.muted, x, top, row_h, text(commit, "shortSha"), 70.0);
            x += 70.0 + 4.0;
            let age = parse_iso8601_ms(text(commit, "authorDate")).map(|date| commit_time_ago(date, now)).unwrap_or_default();
            let caret_x = band.pos.x + band.size.x - 8.0 - 16.0;
            let age_width = paint.measure(cx, Font::Regular, small, &age).x;
            paint.text_centered(cx, Font::Regular, small, colors.muted, caret_x - 8.0 - age_width, top, row_h, &age, age_width + 1.0);
            paint.icon(cx, "chevron-right", 14.0, colors.muted, dvec2(caret_x + 8.0, center_y));
            paint.text_centered(cx, Font::Regular, ui_px(14.0), colors.foreground, x, top, row_h, text(commit, "subject"), (caret_x - 8.0 - age_width - 8.0 - x).max(0.0));
            self.hits.push_clipped(band, content, hit);
        }
        paint.end_clip(cx);
    }

    /// Original `GitFloatingActionButton`: a 56px surface3 circle (or a pill with a label).
    #[allow(clippy::too_many_arguments)]
    fn fab(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, body: Rect, bottom: f64, icon: &str, label: Option<&str>, enabled: bool, hit: Hit) {
        let colors = palette();
        let size = ui_px(14.0);
        let width = match label {
            Some(label) => (32.0 + 20.0 + 8.0 + paint.measure(cx, Font::Regular, size, label).x).max(56.0),
            None => 56.0,
        };
        let area = rect(body.pos.x + body.size.x - 16.0 - width, body.pos.y + body.size.y - bottom - 16.0 - 56.0, width, 56.0);
        let alpha = if enabled { 1.0 } else { 0.5 };
        let pressed = self.pressed.as_ref() == Some(&hit);
        paint.rounded(cx, area, with_alpha(colors.surface3, alpha), 28.0, None);
        if pressed && enabled {
            paint.rounded(cx, area, colors.interaction_highlight, 28.0, None);
        }
        match label {
            Some(label) => {
                let x = area.pos.x + 16.0;
                paint.icon(cx, icon, 20.0, with_alpha(colors.foreground, alpha), dvec2(x + 10.0, area.pos.y + 28.0));
                paint.text_centered(cx, Font::Regular, size, with_alpha(colors.foreground, alpha), x + 28.0, area.pos.y, 56.0, label, width - 44.0);
            }
            None => paint.icon(cx, icon, 20.0, with_alpha(colors.foreground, alpha), area.center()),
        }
        if enabled {
            self.push_hit(area, hit);
        }
    }

    /// Original `ReviewFeedbackAction`.
    #[allow(clippy::too_many_arguments)]
    fn draw_feedback(&mut self, cx: &mut Cx2d, ui: &UiContext, store: &GitStore, paint: &mut GitPaint, body: Rect, compact: bool, bottom_offset: f64) {
        let Some(key) = self.draft_key.clone() else { return };
        let Some(draft) = store.draft(&key) else { return };
        let count = draft.comments.len();
        if count == 0 && draft.phase.failed.is_none() {
            return;
        }
        let colors = palette();
        let snapshot = build_attachment_snapshot(self.cwd(), &self.controller.mode, &self.controller.base_ref, &draft.comments, &|path| {
            self.controller.patches.get(path)
        });
        let recipients = self.recipients(ui);
        let reason = feedback_disabled_reason(recipients.len(), host::connected(ui, self.host()), snapshot.is_some());
        let sending = draft.phase.sending;
        let enabled = count > 0 && reason.is_none() && sending.is_none();
        let shown = sending.or(snapshot.as_ref().map(|s| s.comment_count)).unwrap_or(0);
        let label = tr(if sending.is_some() { "review.feedback.sending" } else { "review.feedback.send" }, &[("count", shown.to_string())]);
        let notice = draft
            .phase
            .failed
            .clone()
            .or_else(|| reason.filter(|_| count > 0).map(|reason| t(&format!("review.feedback.{reason}"))));
        let failed = draft.phase.failed.is_some();
        if count > 0 {
            if compact {
                self.fab(cx, paint, body, bottom_offset, "send", Some(&label), enabled, Hit::Feedback);
            } else {
                let width = measure_button(cx, paint, &label, true, ButtonSize::Sm, false);
                let height = button_height(ButtonSize::Sm, false);
                let area = rect(body.pos.x + (body.size.x - width) / 2.0, body.pos.y + body.size.y - bottom_offset - 16.0 - height, width, height);
                let hovered = self.is_hovered(&Hit::Feedback);
                button(cx, paint, area, &label, Some("send"), ButtonVariant::Primary, ButtonSize::Sm, false, enabled, hovered);
                if enabled {
                    self.push_hit(area, Hit::Feedback);
                }
            }
        }
        if let Some(notice) = notice {
            let size = ui_px(12.0);
            let width = paint.measure(cx, Font::Regular, size, &notice).x.min(body.size.x - 48.0) + 16.0;
            // +1: a wrap width equal to the measured line rounds the last glyph onto a new line.
            let height = paint.measure_wrapped(cx, Font::Regular, size, &notice, width - 15.0) + 16.0;
            let bottom = bottom_offset + if compact { 88.0 } else { 32.0 + 16.0 + 8.0 };
            let x = if compact { body.pos.x + body.size.x - 16.0 - width } else { body.pos.x + (body.size.x - width) / 2.0 };
            let card = rect(x, body.pos.y + body.size.y - bottom - height, width, height);
            paint.rounded(cx, card, colors.surface1, 6.0, None);
            paint.text_wrapped(cx, Font::Regular, size, if failed { colors.danger } else { colors.foreground }, dvec2(card.pos.x + 8.0, card.pos.y + 8.0), &notice, width - 15.0);
        }
    }

    /// Draws the whole view into `area`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cx: &mut Cx2d, ui: &mut UiContext, store: &GitStore, paint: &mut GitPaint, parts: &mut OverlayParts<'_>, area: Rect, compact: bool) {
        if let Some(path) = self.pending_focus.take() {
            self.focus(ui, &path);
        }
        let colors = palette();
        let prefs = preferences(ui);
        let presentation = self.presentation(compact, &prefs);
        let split = Self::split(compact, &prefs);
        let wrap = pref_flag(&prefs, "wrapLines");
        let hide_whitespace = pref_flag(&prefs, "hideWhitespace");
        self.hits.clear();
        self.spinning = false;
        self.icons_pending = false;
        self.tree_area = Rect::default();
        self.commits_area = Rect::default();
        paint.fill(cx, area, if presentation == Presentation::Tree { colors.sidebar } else { colors.surface0 });
        let status = self.controller.status.clone();
        let is_git = status.as_ref().is_some_and(|s| flag(s, "isGit"));
        let mut y = area.pos.y;
        let toolbar_h = TOOLBAR_HEIGHT.max(ui_px(14.0) * 1.4 + 12.0);
        if is_git || self.controller.is_commit {
            if presentation == Presentation::Diff {
                self.diff_only_toolbar(cx, ui, store, paint, rect(area.pos.x, y, area.size.x, toolbar_h), compact, &prefs);
                y += toolbar_h;
            } else {
                self.repository_toolbar(cx, ui, store, paint, rect(area.pos.x, y, area.size.x, toolbar_h), presentation == Presentation::Tree, compact);
                y += toolbar_h;
                self.comparison_toolbar(cx, ui, store, paint, rect(area.pos.x, y, area.size.x, toolbar_h), presentation, compact);
                y += toolbar_h;
            }
        }
        let payload = store.checkout(self.host(), self.cwd()).and_then(|s| s.pr_payload.clone());
        // Original forge setup callout (`computeForgeSetupAction`).
        if is_git
            && let Some(payload) = &payload
            && let Some(message) = forge_setup_message(
                Some(text(payload, "forge")).filter(|f| !f.is_empty()),
                Some(text(payload, "authState")),
                host::supports(ui, self.host(), "forgeProviders"),
                status.as_ref().map(|s| text(s, "remoteUrl")),
            )
        {
            let copy = msg(&message);
            let size = ui_px(12.0);
            let width = area.size.x - 24.0;
            let height = paint.measure_wrapped(cx, Font::Regular, size, &copy, width - 24.0) + 16.0;
            let card = rect(area.pos.x + 12.0, y, width, height);
            paint.rounded(cx, card, colors.surface1, 6.0, Some((colors.border, 1.0)));
            paint.text_wrapped(cx, Font::Regular, size, colors.muted, dvec2(card.pos.x + 12.0, card.pos.y + 8.0), &copy, width - 24.0);
            y += height + 8.0;
        }
        // Original `computePrErrorMessage` and the view's own error.
        let mut errors = Vec::new();
        if let Some(payload) = &payload
            && super::json::flag_or(payload, "githubFeaturesEnabled", true)
            && let Some(message) = error_message(payload).filter(|m| !m.is_empty())
        {
            errors.push(message);
        }
        if status.is_some()
            && let Some(error) = &self.controller.error
        {
            errors.push(error.clone());
        }
        for message in errors {
            let size = ui_px(12.0);
            let height = paint.text_wrapped(cx, Font::Regular, size, colors.destructive, dvec2(area.pos.x + 12.0, y), &message, area.size.x - 24.0);
            y += height + 4.0;
        }
        let window_height = cx.current_pass_size().y;
        let footer = if is_git && presentation != Presentation::Diff { self.commits_height(cx, paint, window_height) } else { 0.0 };
        let body = rect(area.pos.x, y, area.size.x, (area.pos.y + area.size.y - footer - y).max(0.0));
        self.draw_body(cx, ui, store, paint, body, presentation, split, wrap, compact, hide_whitespace);
        if footer > 0.0 {
            self.draw_commits(cx, paint, rect(area.pos.x, body.pos.y + body.size.y, area.size.x, footer));
        }
        if self.overlay.draw(cx, parts.list, parts.bg, paint, parts.input, compact) {
            self.icons_pending = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_body(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut UiContext,
        store: &GitStore,
        paint: &mut GitPaint,
        body: Rect,
        presentation: Presentation,
        split: bool,
        wrap: bool,
        compact: bool,
        hide_whitespace: bool,
    ) {
        let Some(status) = self.controller.status.clone() else {
            match self.controller.error.clone().filter(|_| !self.controller.loading) {
                Some(error) => self.state(cx, paint, body, Some(&error), false, true, None),
                None => self.state(cx, paint, body, Some(&t("workspace.git.diff.checkingRepository")), true, false, None),
            }
            return;
        };
        if !flag(&status, "isGit") {
            self.state(cx, paint, body, Some(&t("workspace.git.diff.notRepository")), false, false, None);
            return;
        }
        let has_changes = !self.controller.files.is_empty();
        if !has_changes && self.controller.loading {
            self.state(cx, paint, body, None, true, false, None);
            return;
        }
        // The whole-diff limit, then the snapshot's error.
        if self.controller.too_large && !self.controller.is_commit {
            self.too_large_state(cx, paint, body);
            return;
        }
        if let Some(error) = self.controller.diff_error.clone() {
            self.state(cx, paint, body, Some(&error), false, true, None);
            return;
        }
        if !has_changes {
            let action = self.empty_action(hide_whitespace).map(|(key, mode)| (t(key), mode));
            let empty = t("diffViewer.empty");
            self.state(cx, paint, body, Some(&empty), false, false, action.as_ref().map(|(label, mode)| (label.as_str(), *mode)));
            return;
        }
        match presentation {
            Presentation::Tree => {
                paint.fill(cx, body, palette().sidebar);
                self.draw_tree(cx, paint, body, 32.0);
            }
            Presentation::Diff => self.draw_doc(cx, ui, store, paint, body, split, wrap, 0.0),
            Presentation::Combined => {
                let jump = if compact { FAB_CLEARANCE } else { 0.0 };
                let has_feedback = self.draft_key.as_deref().and_then(|k| store.draft(k)).is_some_and(|d| !d.comments.is_empty());
                let feedback = if has_feedback { if compact { FAB_CLEARANCE } else { 32.0 + 16.0 } } else { 0.0 };
                let (doc_area, rail) = if !compact && self.tree_visible {
                    // Original TreeRail: content left, a 220px tree right of a 1px border.
                    (
                        rect(body.pos.x, body.pos.y, body.size.x - 221.0, body.size.y),
                        Some(rect(body.pos.x + body.size.x - 220.0, body.pos.y, 220.0, body.size.y)),
                    )
                } else {
                    (body, None)
                };
                self.draw_doc(cx, ui, store, paint, doc_area, split, wrap, jump + feedback);
                if compact {
                    self.fab(cx, paint, doc_area, 0.0, "folder-tree", None, true, Hit::Jump);
                }
                self.draw_feedback(cx, ui, store, paint, doc_area, compact, jump);
                if let Some(rail) = rail {
                    paint.fill(cx, rect(rail.pos.x - 1.0, rail.pos.y, 1.0, rail.size.y), palette().border);
                    self.draw_tree(cx, paint, rail, 32.0);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_doc(&mut self, cx: &mut Cx2d, ui: &mut UiContext, store: &GitStore, paint: &mut GitPaint, area: Rect, split: bool, wrap: bool, bottom_inset: f64) {
        if !self.initial_revealed && !self.controller.selected_path.is_empty() {
            self.initial_revealed = true;
            let path = self.controller.selected_path.clone();
            self.doc.reveal(&path);
        }
        let mut comments: HashMap<String, HashMap<String, Vec<Value>>> = HashMap::new();
        let drafted = self.comments(store);
        for comment in &drafted {
            comments
                .entry(text(comment, "filePath").to_owned())
                .or_default()
                .entry(format!("{}:{}", text(comment, "side"), int(comment, "lineNumber")))
                .or_default()
                .push(comment.clone());
        }
        let comments_revision = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            Value::Array(drafted).to_string().hash(&mut hasher);
            hasher.finish()
        };
        let syntax = host::preference(ui, "syntaxTheme").as_str().unwrap_or("one").to_owned();
        let input = DocInput {
            files: &self.controller.files,
            patches: &self.controller.patches,
            patch_errors: &self.controller.patch_errors,
            patch_revision: self.controller.patch_revision,
            collapsed: &self.collapsed_files,
            comments: &comments,
            comments_revision,
            split,
            wrap,
            syntax_theme: &syntax,
            bottom_inset,
        };
        self.doc.draw(cx, paint, area, &input);
        for (rect_area, clip, hit) in self.doc.hits.entries().to_vec() {
            self.hits.push_clipped(rect_area, clip.unwrap_or(area), Hit::Doc(hit));
        }
        self.spinning |= self.doc.spinning;
        // Placeholders near the viewport request their patch once.
        let wanted = std::mem::take(&mut self.doc.wanted);
        for path in wanted {
            self.controller.ensure_patch(ui, &path);
        }
    }
}
