//! The Git Log tab (`git_log`, IntelliJ's Git Log tool window): a branches
//! panel, the commit table with its lane graph and the selected commit's
//! changed files and message on desktop; on phones a log screen and a commit
//! screen, one at a time. State and requests live in [`LogState`]; this file
//! routes input and actions, `log_draw.rs` paints.

use super::log::{Availability, Filters, LogState, RefFilter, RefKind};
use super::model::now_ms;
use super::overlay::{MenuEntry, MenuItem, Overlay, OverlayHost, OverlayKind, OverlayResult};
use super::paint::Hits;
use super::store::GitStore;
use super::text::{t, tr};
use super::wire;
use crate::common::overlay::{SelectOption, tooltip_at};
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use makepad_widgets::Hit as Pointer;
use makepad_widgets::*;
use paseo_gui_core::{CoreEvent, RequestId};
use serde_json::json;
use std::collections::{HashMap, HashSet};

/// Typing in the search field filters after this pause.
const SEARCH_DEBOUNCE: f64 = 0.3;

/// The panel's text fields the Git Log draws and routes.
pub struct LogInputs<'a> {
    pub search: &'a mut TextInput,
    pub branch: &'a mut TextInput,
}

/// One vertically scrolled region.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Scroller {
    pub offset: f64,
    pub area: Rect,
    pub content: f64,
}

impl Scroller {
    pub fn max(&self) -> f64 {
        (self.content - self.area.size.y).max(0.0)
    }

    /// The region drawn this frame: `content` tall inside `area`.
    pub fn set(&mut self, area: Rect, content: f64) {
        self.area = area;
        self.content = content;
        self.offset = self.offset.clamp(0.0, self.max());
    }

    fn scroll_by(&mut self, delta: f64) -> bool {
        let before = self.offset;
        self.offset = (self.offset + delta).clamp(0.0, self.max());
        before != self.offset
    }

    /// Scrolls the least so `top..top + height` (content coordinates) is visible.
    pub fn reveal(&mut self, top: f64, height: f64) {
        let view = self.area.size.y;
        if view <= 0.0 {
            return;
        }
        if top < self.offset {
            self.offset = top;
        } else if top + height > self.offset + view {
            self.offset = top + height - view;
        }
        self.offset = self.offset.clamp(0.0, self.max());
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Region {
    List,
    Branches,
    Files,
    Info,
    Sheet,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Screen {
    Log,
    /// The phone commit screen of a SHA.
    Commit(String),
}

/// The list the keyboard moves through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    List,
    Files,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum LogHit {
    BranchesToggle,
    Refresh,
    BranchChip,
    AuthorChip,
    SearchToggle,
    SearchClear,
    BranchSearchClear,
    Section(String),
    Ref { kind: RefKind, name: String },
    RefMenu { kind: RefKind, name: String },
    Row(usize),
    PageRetry,
    DetailRetry(String),
    File(String),
    CopyHash(String),
    Parent(String),
    Back,
}

#[derive(Debug)]
pub(super) enum Purpose {
    Branch,
    Author,
    RefMenu { kind: RefKind, name: String },
    RowMenu(String),
    /// The checkout of this branch failed on uncommitted changes.
    StashAndSwitch(String),
}

/// The Checkout action's requests (the Changes branch switcher's stash flow).
enum Flow {
    Switch(String),
    StashSave(String),
    SwitchAfterStash(String),
}

pub struct LogView {
    pub target: PanelTarget,
    pub state: LogState,
    pub(super) overlay: OverlayHost<Purpose>,
    flows: HashMap<RequestId, Flow>,
    pub(super) hits: Hits<LogHit>,
    pub(super) hovered: Option<LogHit>,
    pub(super) pressed: Option<LogHit>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub(super) list: Scroller,
    pub(super) branches: Scroller,
    pub(super) files: Scroller,
    pub(super) info: Scroller,
    pub(super) sheet: Scroller,
    /// Commit row height of the last draw (keyboard paging).
    pub(super) row_height: f64,
    /// File row height and the files listed at the last draw (keyboard moves).
    pub(super) file_row_height: f64,
    pub(super) listed_files: Vec<String>,
    pub(super) branches_open: bool,
    pub(super) collapsed: HashSet<String>,
    pub(super) branch_query: String,
    pub(super) search_text: String,
    search_timer: Timer,
    /// The phone top bar shows its search field.
    pub(super) search_open: bool,
    pub(super) focus_search: bool,
    pub(super) screen: Screen,
    pub(super) focus: Focus,
    pub(super) selected_file: Option<String>,
    /// The last draw used the stacked phone layout.
    pub(super) phone: bool,
    /// Text field style last applied (theme colours, type size, locale).
    pub(super) input_style: String,
    pub spinning: bool,
    /// A material file icon was still decoding at the last draw.
    pub icons_pending: bool,
}

pub(super) fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(super) fn dirname(path: &str) -> &str {
    path.rfind('/').map(|index| &path[..index]).unwrap_or("")
}

/// Select values of the Branch filter: `all` or `<kind>:<name>`.
fn ref_value(kind: RefKind, name: &str) -> String {
    let prefix = match kind {
        RefKind::Head => "head",
        RefKind::Local => "local",
        RefKind::Remote => "remote",
        RefKind::Tag => "tag",
    };
    format!("{prefix}:{name}")
}

fn parse_ref_value(value: &str) -> Option<RefFilter> {
    let (prefix, name) = value.split_once(':')?;
    let kind = match prefix {
        "head" => RefKind::Head,
        "local" => RefKind::Local,
        "remote" => RefKind::Remote,
        "tag" => RefKind::Tag,
        _ => return None,
    };
    Some(RefFilter { name: name.to_owned(), kind: Some(kind) })
}

pub(super) fn ahead_behind(ahead: Option<i64>, behind: Option<i64>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ahead) = ahead.filter(|n| *n > 0) {
        parts.push(format!("↑{ahead}"));
    }
    if let Some(behind) = behind.filter(|n| *n > 0) {
        parts.push(format!("↓{behind}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

impl LogView {
    pub fn new(cx: &mut Cx, ui: &UiContext, store: &mut GitStore, target: PanelTarget) -> Self {
        // Status pushes for this checkout tell the log when HEAD moved.
        store.acquire_checkout(cx, ui, &target.server_id, &target.cwd);
        let state = LogState::new(&target, store);
        Self {
            target,
            state,
            overlay: OverlayHost::default(),
            flows: HashMap::new(),
            hits: Hits::default(),
            hovered: None,
            pressed: None,
            touch_scroll: Default::default(),
            list: Scroller::default(),
            branches: Scroller::default(),
            files: Scroller::default(),
            info: Scroller::default(),
            sheet: Scroller::default(),
            row_height: 26.0,
            file_row_height: 24.0,
            listed_files: Vec::new(),
            branches_open: true,
            collapsed: HashSet::new(),
            branch_query: String::new(),
            search_text: String::new(),
            search_timer: Timer::empty(),
            search_open: false,
            focus_search: false,
            screen: Screen::Log,
            focus: Focus::List,
            selected_file: None,
            phone: false,
            input_style: String::new(),
            spinning: false,
            icons_pending: false,
        }
    }

    pub fn dispose(&mut self, store: &mut GitStore) {
        self.state.dispose();
        store.release_checkout(&self.target.server_id, &self.target.cwd);
    }

    pub fn close_overlay(&mut self, cx: &mut Cx) {
        self.overlay.close(cx);
    }

    /// Sends a menu or select opened during this event to the shell.
    pub fn flush_overlay(&mut self, ui: &mut UiContext) {
        self.overlay.flush(ui);
    }

    pub(super) fn host(&self) -> &str {
        &self.target.server_id
    }

    /// Applies a core batch, store change or timer; returns true when a redraw is due.
    pub fn process(&mut self, cx: &mut Cx, event: &Event, ui: &mut UiContext, store: &mut GitStore, visible: bool) -> bool {
        self.state.set_active(ui, visible);
        let mut dirty = self.state.process(ui, store);
        let responses: Vec<(Flow, Result<serde_json::Value, wire::Failure>)> = ui
            .events
            .iter()
            .filter_map(|event| match event {
                CoreEvent::Response { request_id, result, .. } => {
                    self.flows.remove(request_id).map(|flow| (flow, wire::git_result(result)))
                }
                _ => None,
            })
            .collect();
        for (flow, result) in responses {
            self.on_flow(cx, ui, store, flow, result);
            dirty = true;
        }
        if self.search_timer.is_event(event).is_some() {
            self.search_timer = Timer::empty();
            self.apply_search(ui);
            dirty = true;
        }
        if dirty {
            self.sync_selection();
        }
        dirty
    }

    /// Drops a file selection whose commit is gone.
    fn sync_selection(&mut self) {
        if self.state.selected.is_none() {
            self.selected_file = None;
            if self.focus == Focus::Files {
                self.focus = Focus::List;
            }
        }
    }

    fn apply_search(&mut self, ui: &UiContext) {
        let filters = Filters { text: self.search_text.trim().to_owned(), ..self.state.filters.clone() };
        self.apply_filters(ui, filters);
    }

    fn apply_filters(&mut self, ui: &UiContext, filters: Filters) {
        if filters != self.state.filters {
            self.list.offset = 0.0;
            self.state.set_filters(ui, filters);
        }
    }

    pub(super) fn refresh(&mut self, ui: &UiContext) {
        if self.state.availability(ui) == Availability::Ready && !self.state.page_loading {
            let keep = self.state.loaded;
            self.state.reload(ui, keep);
        }
    }

    // -----------------------------------------------------------------------
    // Text fields.

    /// Routes an event to the search fields; returns true when a redraw is due.
    pub fn handle_inputs(&mut self, cx: &mut Cx, event: &Event, ui: &UiContext, inputs: &mut LogInputs<'_>) -> bool {
        let search_uid = inputs.search.widget_uid();
        let branch_uid = inputs.branch.widget_uid();
        let actions = cx.capture_actions(|cx| {
            inputs.search.handle_event(cx, event, &mut Scope::empty());
            inputs.branch.handle_event(cx, event, &mut Scope::empty());
        });
        let mut redraw = false;
        for action in &actions {
            let Some(action) = action.as_widget_action() else { continue };
            let search = action.widget_uid == search_uid;
            if !search && action.widget_uid != branch_uid {
                continue;
            }
            match action.action.downcast_ref::<TextInputAction>() {
                Some(TextInputAction::Changed(text)) => {
                    let text = text.replace(['\n', '\r'], " ");
                    if search {
                        self.search_text = text;
                        cx.stop_timer(self.search_timer);
                        self.search_timer = cx.start_timeout(SEARCH_DEBOUNCE);
                    } else {
                        self.branch_query = text;
                        self.branches.offset = 0.0;
                    }
                    redraw = true;
                }
                Some(TextInputAction::Returned(..)) if search => {
                    cx.stop_timer(self.search_timer);
                    self.search_timer = Timer::empty();
                    self.apply_search(ui);
                    redraw = true;
                }
                Some(TextInputAction::Escaped) => {
                    if search {
                        self.clear_search(cx, ui, inputs.search);
                        if self.phone {
                            self.search_open = false;
                        }
                    } else {
                        inputs.branch.set_text(cx, "");
                        self.branch_query.clear();
                    }
                    redraw = true;
                }
                Some(TextInputAction::KeyFocus | TextInputAction::KeyFocusLost) => redraw = true,
                _ => {}
            }
        }
        redraw
    }

    fn clear_search(&mut self, cx: &mut Cx, ui: &UiContext, input: &mut TextInput) {
        input.set_text(cx, "");
        self.search_text.clear();
        cx.stop_timer(self.search_timer);
        self.search_timer = Timer::empty();
        self.apply_search(ui);
    }

    // -----------------------------------------------------------------------
    // Actions.

    pub(super) fn select(&mut self, ui: &UiContext, index: usize) {
        let Some(commit) = self.state.commits.get(index) else { return };
        let sha = commit.sha.clone();
        if self.state.selected.as_deref() != Some(sha.as_str()) {
            self.state.selected = Some(sha);
            self.selected_file = None;
            self.files.offset = 0.0;
            self.info.offset = 0.0;
        }
        self.list.reveal(index as f64 * self.row_height, self.row_height);
        if !self.phone {
            let sha = self.state.selected.clone().unwrap_or_default();
            self.state.ensure_detail(ui, &sha);
        }
    }

    fn open_commit_screen(&mut self, ui: &UiContext, index: usize) {
        self.select(ui, index);
        if let Some(sha) = self.state.selected.clone() {
            self.state.ensure_detail(ui, &sha);
            self.sheet.offset = 0.0;
            self.screen = Screen::Commit(sha);
        }
    }

    /// The phone commit screen returns to the log (back arrow, Back, Escape).
    pub fn back(&mut self) -> bool {
        if self.phone && matches!(self.screen, Screen::Commit(_)) {
            self.screen = Screen::Log;
            return true;
        }
        false
    }

    pub(super) fn subject_of(&self, sha: &str) -> String {
        if let Some(commit) = self.state.commit(sha) {
            return commit.subject.clone();
        }
        match self.state.details.get(sha) {
            Some(super::log::Detail::Ready(detail)) => detail.subject.clone(),
            _ => String::new(),
        }
    }

    /// Opens the commit's `commit_diff` tab, focused on `path` when given.
    pub(super) fn open_diff(&self, ui: &mut UiContext, sha: &str, path: Option<&str>) {
        let mut data = json!({ "sha": sha });
        if let Some(path) = path {
            data["focusPath"] = json!(path);
            data["focusRequestId"] = json!(now_ms());
        }
        let target = PanelTarget::new("commit_diff", self.host(), &self.target.workspace_id, &self.state.cwd)
            .with_id(sha)
            .with_title(self.subject_of(sha))
            .with_data(data);
        ui.push(UiEffect::OpenTab { target, placement: TabPlacement::Main });
    }

    fn copy(ui: &mut UiContext, text: String, toast: &str) {
        ui.push(UiEffect::CopyText { text, toast: Some(t(toast)) });
    }

    fn filter_ref(&mut self, ui: &UiContext, filter: Option<RefFilter>) {
        let filters = Filters { revision: filter, ..self.state.filters.clone() };
        self.apply_filters(ui, filters);
    }

    fn checkout(&mut self, ui: &UiContext, kind: RefKind, name: &str) {
        // A remote branch checks out (and tracks) its local namesake.
        let branch = match kind {
            RefKind::Remote => self
                .state
                .refs
                .as_ref()
                .and_then(|refs| refs.remote.iter().find(|b| b.name == name))
                .map(|b| b.branch.clone())
                .unwrap_or_else(|| name.to_owned()),
            _ => name.to_owned(),
        };
        self.send_switch(ui, branch.clone(), Flow::Switch(branch));
    }

    fn send(&mut self, ui: &UiContext, kind: &str, fields: serde_json::Value, flow: Flow) {
        let request = wire::send(&ui.core, &self.target.server_id, kind, fields);
        self.flows.insert(request, flow);
    }

    fn send_switch(&mut self, ui: &UiContext, branch: String, flow: Flow) {
        let fields = json!({ "cwd": self.state.cwd, "branch": branch });
        self.send(ui, "checkout_switch_branch_request", fields, flow);
    }

    /// Original branch switcher: uncommitted changes offer stash-and-switch.
    fn on_flow(&mut self, cx: &mut Cx, ui: &mut UiContext, store: &mut GitStore, flow: Flow, result: Result<serde_json::Value, wire::Failure>) {
        let (host, cwd) = (self.target.server_id.clone(), self.state.cwd.clone());
        match flow {
            Flow::Switch(branch) | Flow::SwitchAfterStash(branch) if result.is_ok() => {
                store.invalidate(cx, &host, &cwd);
                ui.toast(tr("workspace.git.log.checkedOut", &[("name", branch)]), false);
            }
            Flow::Switch(branch) => {
                let failure = result.err().unwrap_or_else(|| wire::Failure::message(""));
                if failure.message.to_lowercase().contains("uncommitted") {
                    self.overlay.open(Overlay {
                        purpose: Purpose::StashAndSwitch(branch),
                        kind: OverlayKind::Confirm {
                            title: t("branchSwitcher.uncommittedTitle"),
                            message: t("branchSwitcher.uncommittedMessage"),
                            confirm: t("branchSwitcher.stashAndSwitch"),
                            destructive: false,
                        },
                    });
                } else {
                    ui.toast(wire::message_or(&failure, t("workspace.git.log.checkoutFailed")), true);
                }
            }
            Flow::SwitchAfterStash(_) => {
                if let Err(failure) = result {
                    ui.toast(wire::message_or(&failure, t("workspace.git.log.checkoutFailed")), true);
                }
            }
            Flow::StashSave(branch) => match result {
                Err(failure) => ui.toast(wire::message_or(&failure, t("branchSwitcher.failedToStash")), true),
                Ok(_) => {
                    store.invalidate(cx, &host, &cwd);
                    self.send_switch(ui, branch.clone(), Flow::SwitchAfterStash(branch));
                }
            },
        }
    }

    fn current_branch(&self) -> Option<&str> {
        self.state.refs.as_ref().and_then(|refs| refs.local.iter().find(|b| b.current)).map(|b| b.name.as_str())
    }

    fn ref_menu(&mut self, kind: RefKind, name: &str, anchor: Rect) {
        let mut entries = Vec::new();
        let current = kind == RefKind::Local && self.current_branch() == Some(name);
        if matches!(kind, RefKind::Local | RefKind::Remote) && !current {
            entries.push(MenuEntry::Item(MenuItem::new("checkout", t("workspace.git.log.menu.checkout")).icon("git-branch")));
        }
        entries.push(MenuEntry::Item(MenuItem::new("show", t("workspace.git.log.menu.showOnly")).icon("list-filter")));
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Item(MenuItem::new("copy", t("workspace.git.log.menu.copyName")).icon("copy")));
        self.overlay.open(Overlay {
            purpose: Purpose::RefMenu { kind, name: name.to_owned() },
            kind: OverlayKind::Menu { title: Some(name.to_owned()), anchor, align_end: false, width: None, entries },
        });
    }

    fn row_menu(&mut self, index: usize, anchor: Rect) {
        let Some(commit) = self.state.commits.get(index) else { return };
        let entries = vec![
            MenuEntry::Item(MenuItem::new("copy-hash", t("workspace.git.log.menu.copyRevision")).icon("copy")),
            MenuEntry::Item(MenuItem::new("copy-subject", t("workspace.git.log.menu.copySubject")).icon("copy")),
            MenuEntry::Separator,
            MenuEntry::Item(MenuItem::new("open-diff", t("workspace.git.log.menu.openDiff")).icon("file-diff")),
        ];
        let title = commit.short_sha.clone();
        self.overlay.open(Overlay {
            purpose: Purpose::RowMenu(commit.sha.clone()),
            kind: OverlayKind::Menu { title: Some(title), anchor, align_end: false, width: None, entries },
        });
    }

    fn branch_select(&mut self, anchor: Rect) {
        let mut options = vec![SelectOption::new("all", t("workspace.git.log.filters.allBranches")).icon("git-graph")];
        if let Some(refs) = &self.state.refs {
            let head = match (&refs.head_branch, &refs.head_sha) {
                (Some(branch), _) if !refs.detached => format!("HEAD ({branch})"),
                (_, Some(sha)) => format!("HEAD ({})", sha.chars().take(7).collect::<String>()),
                _ => "HEAD".to_owned(),
            };
            options.push(SelectOption::new(ref_value(RefKind::Head, "HEAD"), head).icon("crosshair").separator_before(true));
            for (index, branch) in refs.local.iter().enumerate() {
                let mut option = SelectOption::new(ref_value(RefKind::Local, &branch.name), branch.name.clone())
                    .icon(if branch.current { "circle-dot" } else { "git-branch" })
                    .separator_before(index == 0);
                let mut trailing = ahead_behind(branch.ahead, branch.behind);
                if branch.gone {
                    trailing = Some(t("workspace.git.log.branches.gone"));
                }
                if let Some(trailing) = trailing {
                    option = option.trailing(trailing);
                }
                options.push(option);
            }
            for (index, branch) in refs.remote.iter().enumerate() {
                options.push(
                    SelectOption::new(ref_value(RefKind::Remote, &branch.name), branch.name.clone())
                        .icon("cloud")
                        .separator_before(index == 0),
                );
            }
            for (index, tag) in refs.tags.iter().enumerate() {
                options.push(SelectOption::new(ref_value(RefKind::Tag, tag), tag.clone()).icon("tag").separator_before(index == 0));
            }
        }
        let selected = match &self.state.filters.revision {
            None => Some("all".to_owned()),
            Some(RefFilter { name, kind: Some(kind) }) => Some(ref_value(*kind, name)),
            Some(_) => None,
        };
        self.overlay.open(Overlay {
            purpose: Purpose::Branch,
            kind: OverlayKind::Select {
                title: t("workspace.git.log.filters.branchTitle"),
                anchor,
                min_width: 280.0,
                options,
                selected,
                placeholder: t("workspace.git.log.branches.search"),
                empty: t("workspace.git.log.branches.empty"),
            },
        });
    }

    fn author_select(&mut self, anchor: Rect) {
        let mut options = vec![SelectOption::new("", t("workspace.git.log.filters.allUsers")).icon("users")];
        for (index, author) in self.state.authors.iter().enumerate() {
            options.push(SelectOption::new(format!("author:{author}"), author.clone()).icon("user").separator_before(index == 0));
        }
        let selected = Some(self.state.filters.author.as_ref().map(|a| format!("author:{a}")).unwrap_or_default());
        self.overlay.open(Overlay {
            purpose: Purpose::Author,
            kind: OverlayKind::Select {
                title: t("workspace.git.log.filters.userTitle"),
                anchor,
                min_width: 240.0,
                options,
                selected,
                placeholder: t("workspace.git.log.filters.userSearch"),
                empty: t("workspace.git.log.filters.userEmpty"),
            },
        });
    }

    /// Applies a menu / select answer; returns true when one arrived.
    pub fn handle_overlay(&mut self, ui: &mut UiContext) -> bool {
        let Some(result) = self.overlay.poll() else { return false };
        match result {
            OverlayResult::Chose(Purpose::Branch, value) => {
                let filter = if value == "all" { None } else { parse_ref_value(&value) };
                self.filter_ref(ui, filter);
            }
            OverlayResult::Chose(Purpose::Author, value) => {
                let author = value.strip_prefix("author:").map(str::to_owned);
                let filters = Filters { author, ..self.state.filters.clone() };
                self.apply_filters(ui, filters);
            }
            OverlayResult::Chose(Purpose::RefMenu { kind, name }, action) => match action.as_str() {
                "checkout" => self.checkout(ui, kind, &name),
                "show" => self.filter_ref(ui, Some(RefFilter { name, kind: Some(kind) })),
                "copy" => Self::copy(ui, name, "workspace.git.log.toasts.nameCopied"),
                _ => {}
            },
            OverlayResult::Chose(Purpose::RowMenu(sha), action) => match action.as_str() {
                "copy-hash" => Self::copy(ui, sha, "workspace.git.log.toasts.revisionCopied"),
                "copy-subject" => {
                    let subject = self.subject_of(&sha);
                    Self::copy(ui, subject, "workspace.git.log.toasts.subjectCopied");
                }
                "open-diff" => self.open_diff(ui, &sha, None),
                _ => {}
            },
            OverlayResult::Confirmed(Purpose::StashAndSwitch(branch)) => {
                let mut fields = json!({ "cwd": self.state.cwd });
                if let Some(current) = self.current_branch() {
                    fields["branch"] = json!(current);
                }
                self.send(ui, "stash_save_request", fields, Flow::StashSave(branch));
            }
            _ => {}
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn activate(&mut self, cx: &mut Cx, ui: &mut UiContext, hit: LogHit, anchor: Rect, secondary: bool, inputs: &mut LogInputs<'_>) {
        match hit {
            LogHit::BranchesToggle => self.branches_open = !self.branches_open,
            LogHit::Refresh => self.refresh(ui),
            LogHit::BranchChip => self.branch_select(anchor),
            LogHit::AuthorChip => self.author_select(anchor),
            LogHit::SearchToggle => {
                if self.search_open {
                    self.search_open = false;
                    if !self.search_text.is_empty() {
                        self.clear_search(cx, ui, inputs.search);
                    }
                } else {
                    self.search_open = true;
                    self.focus_search = true;
                }
            }
            LogHit::SearchClear => self.clear_search(cx, ui, inputs.search),
            LogHit::BranchSearchClear => {
                inputs.branch.set_text(cx, "");
                self.branch_query.clear();
                self.branches.offset = 0.0;
            }
            LogHit::Section(key) => {
                if !self.collapsed.remove(&key) {
                    self.collapsed.insert(key);
                }
            }
            LogHit::Ref { kind, name } => {
                if secondary {
                    if kind != RefKind::Head {
                        self.ref_menu(kind, &name, anchor);
                    }
                } else {
                    self.filter_ref(ui, Some(RefFilter { name, kind: Some(kind) }));
                }
            }
            LogHit::RefMenu { kind, name } => self.ref_menu(kind, &name, anchor),
            LogHit::Row(index) => {
                self.focus = Focus::List;
                if secondary {
                    self.select(ui, index);
                    self.row_menu(index, anchor);
                } else if self.phone {
                    self.open_commit_screen(ui, index);
                } else {
                    self.select(ui, index);
                }
            }
            LogHit::PageRetry => self.state.retry(ui),
            LogHit::DetailRetry(sha) => self.state.retry_detail(ui, &sha),
            LogHit::File(path) => {
                self.focus = Focus::Files;
                self.selected_file = Some(path.clone());
                if !secondary && let Some(sha) = self.shown_commit() {
                    self.open_diff(ui, &sha, Some(&path));
                }
            }
            LogHit::CopyHash(sha) => Self::copy(ui, sha, "workspace.git.log.toasts.revisionCopied"),
            LogHit::Parent(sha) => {
                if let Some(index) = self.state.index_of(&sha) {
                    if self.phone {
                        self.open_commit_screen(ui, index);
                    } else {
                        self.select(ui, index);
                    }
                }
            }
            LogHit::Back => {
                self.back();
            }
        }
    }

    /// The commit whose details are shown (the phone screen's, else the selection).
    pub(super) fn shown_commit(&self) -> Option<String> {
        match &self.screen {
            Screen::Commit(sha) if self.phone => Some(sha.clone()),
            _ => self.state.selected.clone(),
        }
    }

    fn tooltip_for(&self, hit: &LogHit) -> Option<String> {
        Some(match hit {
            LogHit::BranchesToggle => t(if self.branches_open { "workspace.git.log.toolbar.hideBranches" } else { "workspace.git.log.toolbar.showBranches" }),
            LogHit::Refresh => t("workspace.git.log.toolbar.refresh"),
            LogHit::SearchToggle => t(if self.search_open { "workspace.git.log.toolbar.closeSearch" } else { "workspace.git.log.toolbar.search" }),
            LogHit::Back => t("workspace.git.log.toolbar.back"),
            LogHit::SearchClear | LogHit::BranchSearchClear => t("workspace.git.log.toolbar.clear"),
            LogHit::CopyHash(_) => t("workspace.git.log.details.copyHash"),
            LogHit::RefMenu { .. } => t("workspace.git.log.menu.more"),
            LogHit::File(path) => path.clone(),
            _ => return None,
        })
    }

    fn region_at(&self, point: DVec2) -> Option<Region> {
        [
            (Region::Sheet, self.sheet.area),
            (Region::Info, self.info.area),
            (Region::Files, self.files.area),
            (Region::Branches, self.branches.area),
            (Region::List, self.list.area),
        ]
        .into_iter()
        .find(|(_, area)| area.contains(point))
        .map(|(region, _)| region)
    }

    fn scroller(&mut self, region: Region) -> &mut Scroller {
        match region {
            Region::List => &mut self.list,
            Region::Branches => &mut self.branches,
            Region::Files => &mut self.files,
            Region::Info => &mut self.info,
            Region::Sheet => &mut self.sheet,
        }
    }

    /// Keyboard moves through the commit table or the changed files.
    fn handle_key(&mut self, ui: &mut UiContext, key: &KeyEvent) -> bool {
        if key.key_code == KeyCode::Escape {
            return self.back();
        }
        if self.phone && matches!(self.screen, Screen::Commit(_)) {
            return false;
        }
        if self.focus == Focus::Files && !self.listed_files.is_empty() {
            let current = self.selected_file.as_ref().and_then(|p| self.listed_files.iter().position(|f| f == p));
            let last = self.listed_files.len() - 1;
            let next = match key.key_code {
                KeyCode::ArrowUp => current.map_or(0, |i| i.saturating_sub(1)),
                KeyCode::ArrowDown => current.map_or(0, |i| (i + 1).min(last)),
                KeyCode::Home => 0,
                KeyCode::End => last,
                KeyCode::ReturnKey | KeyCode::NumpadEnter => {
                    if let (Some(path), Some(sha)) = (self.selected_file.clone(), self.shown_commit()) {
                        self.open_diff(ui, &sha, Some(&path));
                    }
                    return false;
                }
                _ => return false,
            };
            self.selected_file = Some(self.listed_files[next].clone());
            self.files.reveal(next as f64 * self.file_row_height, self.file_row_height);
            return true;
        }
        let count = self.state.commits.len();
        if count == 0 {
            return false;
        }
        let current = self.state.selected.as_ref().and_then(|sha| self.state.index_of(sha));
        let page = ((self.list.area.size.y / self.row_height).floor() as usize).max(1);
        let next = match key.key_code {
            KeyCode::ArrowUp => current.map_or(0, |i| i.saturating_sub(1)),
            KeyCode::ArrowDown => current.map_or(0, |i| (i + 1).min(count - 1)),
            KeyCode::PageUp => current.map_or(0, |i| i.saturating_sub(page)),
            KeyCode::PageDown => current.map_or(0, |i| (i + page).min(count - 1)),
            KeyCode::Home => 0,
            KeyCode::End => count - 1,
            KeyCode::ReturnKey | KeyCode::NumpadEnter => {
                match current {
                    Some(index) if self.phone => self.open_commit_screen(ui, index),
                    Some(index) => {
                        let sha = self.state.commits[index].sha.clone();
                        self.open_diff(ui, &sha, None);
                    }
                    None => return false,
                }
                return true;
            }
            _ => return false,
        };
        self.select(ui, next);
        true
    }

    /// Pointer and keyboard input outside overlays; returns true when a redraw is due.
    pub fn handle_pointer(&mut self, cx: &mut Cx, event: &Event, area: Area, ui: &mut UiContext, inputs: &mut LogInputs<'_>) -> bool {
        if let Event::BackPressed { handled } = event
            && !handled.get()
            && self.back()
        {
            handled.set(true);
            return true;
        }
        let hit = event.hits(cx, area);
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
        }
        if let Some(delta) = self.touch_scroll.handle_event(cx, event, &hit) {
            self.pressed = None;
            let origin = self.touch_scroll.origin();
            let Some(region) = self.region_at(origin) else {
                self.touch_scroll.reset();
                return false;
            };
            let scroller = self.scroller(region);
            let (mut offset, max) = (scroller.offset, scroller.max());
            let moved = self.touch_scroll.apply_delta(&mut offset, delta, max);
            self.scroller(region).offset = offset;
            return moved;
        }
        match hit {
            Pointer::FingerHoverIn(e) | Pointer::FingerHoverOver(e) => {
                let (anchor, hovered) = self.hits.find(e.abs).map(|(anchor, hit)| (anchor, Some(hit))).unwrap_or_default();
                cx.set_cursor(if hovered.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
                if hovered != self.hovered {
                    let tip = hovered.as_ref().and_then(|hit| self.tooltip_for(hit));
                    tooltip_at(ui, tip.as_deref(), anchor, 0.3);
                    self.hovered = hovered;
                    return true;
                }
            }
            Pointer::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.3);
                    return true;
                }
            }
            Pointer::FingerScroll(e) => {
                let delta = if e.modifiers.shift { e.scroll.x } else { e.scroll.y };
                if let Some(region) = self.region_at(e.abs) {
                    return self.scroller(region).scroll_by(delta);
                }
            }
            Pointer::FingerDown(e) => {
                tooltip_at(ui, None, Rect::default(), 0.3);
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
                if self.region_at(e.abs).is_some() {
                    cx.set_key_focus(area);
                }
                // Phone rows show their pressed state.
                return self.phone && self.pressed.is_some();
            }
            Pointer::FingerLongPress(e) => {
                if let Some((rect, hit)) = self.hits.find(e.abs)
                    && matches!(hit, LogHit::Row(_) | LogHit::Ref { .. })
                {
                    self.pressed = None;
                    self.activate(cx, ui, hit, rect, true, inputs);
                    return true;
                }
            }
            Pointer::FingerUp(e) => {
                let pressed = self.pressed.take();
                if !e.is_over || e.has_long_press_occurred || (e.device.is_touch() && !e.was_tap()) {
                    return false;
                }
                let secondary = e.mouse_button().is_some_and(|b| b.is_secondary());
                let Some((rect, hit)) = self.hits.find(e.abs) else { return false };
                if pressed.as_ref() != Some(&hit) {
                    return false;
                }
                self.activate(cx, ui, hit, rect, secondary, inputs);
                return true;
            }
            Pointer::KeyDown(key) => return self.handle_key(ui, &key),
            _ => {}
        }
        false
    }
}
