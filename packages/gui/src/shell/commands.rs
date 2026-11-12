//! Shell behaviour (Flutter `PaseoShell` methods, `navigation_actions.dart`,
//! `session_sidebar.dart` actions, `AppStore.open*`): commands and shortcuts, tab/pane/window
//! closing with guards, effects from feature roots, platform routes and lifecycle, and every
//! shell-owned menu and dialog answer.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use makepad_widgets::*;
use paseo_gui_core::CoreError;
use paseo_gui_platform::{CloseDecision, PlatformAction, PlatformEvent, PlatformRoute, PlatformValue, QuitReason, WindowGeometry};
use serde_json::{Map, Value, json};

use crate::common::overlay::{
    self as overlay_api, MenuEntry, MenuItem, MenuOptions, OverlayKind, OverlayRequest, SHELL_OWNER,
};
use crate::common::{MAIN_WINDOW_ID, PanelTarget, Route, TabPlacement, UiContext, UiEffect, json as j, tr, tr_args};
use crate::shell::hosts::{self, parent_agent_id};
use crate::shell::layout::{Axis, CompactDestination, Direction};
use crate::shell::sheets::{self, history, host_connections, host_dialog, labels, shortcut_help, workspace_tabs};
use crate::shell::ui::Press;
use crate::shell::{Page, PaseoShell, Pending, WindowRequest, WindowState, chrome, overlays, panels, platform, response_message, sidebar};

// ---------------------------------------------------------------------------------------------
// Types

/// What happens once a close queue passed its guards.
#[derive(Clone, Debug)]
pub enum AfterClose {
    Nothing,
    ClosePane(String),
    /// The OS window close continues (drafts flush, then the platform's decision).
    Window,
    /// A platform quit vote.
    Quit(u64),
    /// Saved host removal (`removeHost`) once its tabs closed.
    RemoveHost(String),
    /// `workspace.archive` shortcut once every guard passed (`canCloseWindow`).
    Archive { host_id: String, workspaces: Vec<Value> },
}

/// An in-progress close (Flutter `_closeWorkspaceTabs` / `canCloseWindow`).
#[derive(Debug)]
pub struct Closing {
    pub queue: VecDeque<String>,
    /// Remove the tabs (tab close) or only ask their guards (window close, quit, archive).
    pub remove: bool,
    pub after: AfterClose,
}

/// Why a host is being chosen (`_chooseHost` / `showHostChooser` callers).
#[derive(Clone, Debug)]
pub enum HostPurpose {
    NewSession { new_workspace: bool, worktree: bool },
    AddProject,
    Usage,
    ManageLabels,
    ProjectPath(String),
    SessionHistory,
}

/// Shell-owned menus and dialogs awaiting an answer.
#[derive(Clone, Debug)]
pub enum ShellOverlay {
    TabMenu { target: PanelTarget, pane_id: String, explorer: bool },
    WorkspaceTabMenu { tab: PanelTarget, workspace: PanelTarget },
    HostMenu { host_id: String, order: Vec<String> },
    ProjectMenu { host_id: String, project: Value, order: Vec<String> },
    SessionMenu { host_id: String, agent: Value },
    HeaderMenu { target: Option<PanelTarget> },
    ScriptsMenu { target: PanelTarget, workspace: Value },
    ExplorerNewTab,
    HelpMenu,
    HostPicker,
    DisplayMenu,
    Guard { key: String, file: bool },
    /// `handleCloseTerminalTab`: closing a live terminal tab kills its daemon terminal.
    CloseTerminal { key: String, host_id: String, terminal_id: String },
    BulkClose { keys: Vec<String>, after: AfterClose },
    RemoveHost { host_id: String },
    RemoveProject { host_id: String, project: Value },
    ArchiveSession { host_id: String, agent_id: String },
    ArchiveRisk { host_id: String, current: Value, remaining: Vec<Value>, confirmed: Vec<Value> },
    RestoreWorkspace { host_id: String, workspace_id: String },
    RenameTab { target: PanelTarget },
    RenameSession { host_id: String, agent_id: String },
    RenameProject { host_id: String, project_id: String },
    /// History page "Filter by host" select.
    HistoryHost,
}

/// Results posted back from worker threads.
#[derive(Debug)]
pub enum ShellAsync {
    /// `project.icon.set` upload bytes read off the GUI thread.
    ProjectIcon { window: String, host_id: String, project_id: String, data: Result<String, String> },
}

/// Pending project icon picks by platform request id.
#[derive(Clone, Debug)]
struct IconPick {
    window: String,
    host_id: String,
    project_id: String,
}

thread_local! {
    static ICON_PICKS: RefCell<HashMap<u64, IconPick>> = RefCell::new(HashMap::new());
    /// Quit votes: token → (approving windows, already answered).
    static QUIT_VOTES: RefCell<HashMap<u64, (usize, bool)>> = RefCell::new(HashMap::new());
}

// ---------------------------------------------------------------------------------------------
// Small helpers

pub(crate) fn toast(shell: &mut PaseoShell, cx: &mut Cx, message: impl Into<String>, error: bool) {
    overlays::toast(&mut shell.data.overlays, cx, message.into(), error);
}

pub(crate) fn request(shell: &mut PaseoShell, ui: &UiContext, host_id: &str, message: Value, response_type: &str, pending: Pending) {
    let id = ui.core.request(host_id, message, response_type);
    shell.data.pending.insert(id, pending);
}

fn open_shell_overlay(shell: &mut PaseoShell, cx: &mut Cx, kind: OverlayKind, overlay: ShellOverlay) -> u64 {
    let id = overlay_api::next_id();
    shell.data.menus.insert(id, overlay);
    let request = OverlayRequest { id, owner: SHELL_OWNER.to_string(), window: shell.data.window_id.clone(), kind };
    shell.open_overlay(cx, request);
    id
}

/// `showPaseoSelect` owned by the shell; the answer reaches [`shell_overlay_result`] as
/// `{value}` or `{cancelled}`, like a menu.
pub(crate) fn shell_select(shell: &mut PaseoShell, cx: &mut Cx, anchor: Rect, request: overlay_api::SelectRequest, overlay: ShellOverlay) -> u64 {
    let overlay_api::SelectRequest { title, options, value, searchable, search_placeholder, empty_text, min_width } = request;
    let kind = OverlayKind::Select { title, anchor, options, value, searchable, search_placeholder, empty_text, min_width };
    open_shell_overlay(shell, cx, kind, overlay)
}

fn menu(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    anchor: Rect,
    title: Option<String>,
    entries: Vec<MenuEntry>,
    options: MenuOptions,
    overlay: ShellOverlay,
) -> u64 {
    open_shell_overlay(shell, cx, overlay_api::menu_kind(anchor, title, entries, options), overlay)
}

fn confirm(shell: &mut PaseoShell, cx: &mut Cx, title: &str, message: &str, label: &str, destructive: bool, overlay: ShellOverlay) {
    let kind = OverlayKind::Confirm {
        title: title.to_string(),
        message: message.to_string(),
        confirm_label: label.to_string(),
        destructive,
    };
    open_shell_overlay(shell, cx, kind, overlay);
}

fn prompt(shell: &mut PaseoShell, cx: &mut Cx, title: &str, initial: &str, overlay: ShellOverlay) {
    let kind = OverlayKind::Prompt {
        title: title.to_string(),
        message: None,
        initial: initial.to_string(),
        placeholder: String::new(),
        confirm_label: tr("common.actions.save"),
        multiline: false,
    };
    open_shell_overlay(shell, cx, kind, overlay);
}

fn item(value: &str, label: impl Into<String>, icon: &str) -> MenuEntry {
    MenuItem::new(value, label).icon(icon).entry()
}

fn anchor_of(shell: &PaseoShell, id: &str) -> Rect {
    shell.data.hits.rect_of(&id.to_string()).unwrap_or_default()
}

fn point(abs: DVec2) -> Rect {
    Rect { pos: abs, size: DVec2::default() }
}

pub(crate) fn selected(shell: &PaseoShell) -> Option<PanelTarget> {
    shell.data.layout.selected().cloned()
}

fn in_workspace(tab: Option<&PanelTarget>, workspace: &PanelTarget) -> bool {
    tab.is_some_and(|t| t.server_id == workspace.server_id && t.workspace_id == workspace.workspace_id)
}

fn copy(shell: &mut PaseoShell, cx: &mut Cx, text: &str, label_key: &str) {
    if platform::copy_text(text.to_string()).is_none() {
        cx.copy_to_clipboard(text);
    }
    toast(shell, cx, tr_args("common.states.copiedLabel", &[("label", &tr(label_key))]), false);
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Applies one preference immediately (readers in this batch see it) and persists the whole
/// object; the shell is the only `preferences` writer. Flutter `AppPreferenceSettings.save`:
/// when the durable write fails the previous value comes back and the error toasts.
pub fn set_preference(shell: &mut PaseoShell, ui: &mut UiContext, key: &str, value: Value) {
    let previous = ui.preference(key).cloned();
    let whole = write_preference(ui, key, value);
    let request = ui.core.save_state_flushed("preferences", whole);
    shell.data.pending.insert(request, Pending::SavePreference { key: key.to_string(), previous });
}

/// Replaces `key` (`Null` removes it) in this dispatch's state; returns the whole object.
fn write_preference(ui: &mut UiContext, key: &str, value: Value) -> Value {
    let mut state = (*ui.state).clone();
    if !state.get("preferences").is_some_and(Value::is_object) {
        state["preferences"] = json!({});
    }
    let preferences = state["preferences"].as_object_mut().expect("preferences is an object");
    if value.is_null() {
        preferences.remove(key);
    } else {
        preferences.insert(key.to_string(), value);
    }
    let whole = state["preferences"].clone();
    ui.state = std::sync::Arc::new(state);
    whole
}

// ---------------------------------------------------------------------------------------------
// Targets and opening (AppStore.open / openExplorer / openAgent, navigation_actions)

/// `navigationAgentTarget`.
pub fn navigation_agent_target(server_id: &str, agent: &Value, project: Option<&Value>) -> PanelTarget {
    let placement = project.cloned().unwrap_or_else(|| j::object(agent, "project").clone());
    let cwd = j::text(agent, "cwd");
    let cwd = if cwd.is_empty() { j::text(j::object(&placement, "checkout"), "cwd").to_string() } else { cwd.to_string() };
    PanelTarget::new("agent", server_id, j::text(agent, "workspaceId"), &cwd)
        .with_id(j::text(agent, "id"))
        .with_title(j::text_or(agent, "title", "Session"))
        .with_data(json!({ "project": placement }))
}

pub fn open_tab(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, target: PanelTarget, placement: TabPlacement) {
    mark_child_tab_open(shell, ui, &target);
    match placement {
        TabPlacement::Main => shell.data.layout.open(target, false),
        TabPlacement::Beside => shell.data.layout.open(target, true),
        TabPlacement::Explorer => shell.data.layout.open_explorer(target, true),
        TabPlacement::ExplorerBackground => shell.data.layout.open_explorer(target, false),
    }
    // A retargeted root (title/data changed) gets the new target.
    if let Some(panel) = shell.data.panels.get_mut(&shell.data.layout.selected().map(PanelTarget::key).unwrap_or_default())
        && let Some(fresh) = shell.data.layout.target(&panel.target.key()).cloned()
        && fresh != panel.target
    {
        panel.target = fresh;
        panels::retarget(cx, panel);
    }
    shell.redraw(cx);
}

/// `openAgent`: `fetch_agent_request`, then open (or select) its tab.
pub fn open_agent(shell: &mut PaseoShell, ui: &UiContext, host_id: &str, agent_id: &str, error: Option<String>, placement: TabPlacement) {
    request(
        shell,
        ui,
        host_id,
        json!({ "type": "fetch_agent_request", "agentId": agent_id }),
        "fetch_agent_response",
        Pending::OpenAgent { host_id: host_id.to_string(), error, placement },
    );
}

/// `openNavigationAgent`.
pub fn open_navigation_agent(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, server_id: &str, agent: &Value, project: Option<&Value>) {
    open_tab(shell, cx, ui, navigation_agent_target(server_id, agent, project), TabPlacement::Main);
}

/// `_markChildTabOpen`: child agents carry `paseo.open-agent-tab.<clientId>` while open.
fn mark_child_tab_open(shell: &mut PaseoShell, ui: &UiContext, target: &PanelTarget) {
    if target.kind != "agent" || shell.data.marked_child_tabs.contains(&target.key()) {
        return;
    }
    let Some(id) = target.id.as_deref() else { return };
    let Some(host) = hosts::host(&ui.state, &target.server_id) else { return };
    if !host.connected() {
        return;
    }
    let Some(agent) = host.agent(id) else { return };
    if parent_agent_id(agent).is_empty() {
        return;
    }
    let client = j::text(&ui.state, "clientId");
    if client.is_empty() {
        return;
    }
    let label = format!("paseo.open-agent-tab.{client}");
    shell.data.marked_child_tabs.insert(target.key());
    if j::text(j::object(agent, "labels"), &label) == "true" {
        return;
    }
    let mut labels = Map::new();
    labels.insert(label, json!("true"));
    request(
        shell,
        ui,
        &target.server_id,
        json!({ "type": "update_agent_request", "agentId": id, "labels": labels }),
        "update_agent_response",
        Pending::Plain,
    );
}

/// `_chooseHost`: the selected tab's host, else none (opens add-host) / the only host / a chooser.
fn choose_host(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, purpose: HostPurpose) {
    if let Some(target) = selected(shell)
        && hosts::host(&ui.state, &target.server_id).is_some()
    {
        host_chosen(shell, cx, ui, purpose, target.server_id);
        return;
    }
    choose_any_host(shell, cx, ui, purpose, None);
}

/// `showHostChooser`: local hosts first; one candidate is chosen without asking.
fn choose_any_host(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, purpose: HostPurpose, connected_only: Option<bool>) {
    let all = hosts::hosts(&ui.state);
    let mut candidates: Vec<String> = all.iter().filter(|h| h.is_local()).map(|h| h.server_id().to_string()).collect();
    candidates.extend(all.iter().filter(|h| !h.is_local()).map(|h| h.server_id().to_string()));
    if connected_only == Some(true) {
        candidates.retain(|id| hosts::host(&ui.state, id).is_some_and(|h| h.connected()));
    }
    match candidates.len() {
        0 => {
            if matches!(purpose, HostPurpose::NewSession { .. } | HostPurpose::AddProject) {
                host_dialog::open_flow(shell, cx, None, None, None);
            }
        }
        1 => host_chosen(shell, cx, ui, purpose, candidates.remove(0)),
        _ => {
            let title = match purpose {
                HostPurpose::ManageLabels => tr("workspaceLabels.manage.title"),
                _ => "Choose host".to_string(),
            };
            shell.data.overlays.open_sheet(Box::new(host_connections::ChooserSheet::new(title, candidates, purpose)));
            shell.redraw(cx);
        }
    }
}

pub fn host_chosen(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, purpose: HostPurpose, host_id: String) {
    match purpose {
        HostPurpose::NewSession { new_workspace, worktree } => {
            let current = selected(shell);
            let mut target = PanelTarget::new(
                "new_tab",
                &host_id,
                if new_workspace { "" } else { current.as_ref().map_or("", |t| t.workspace_id.as_str()) },
                current.as_ref().map_or("", |t| t.cwd.as_str()),
            )
            .with_id(new_id())
            .with_title(tr("sessionNavigation.newSession"));
            if worktree {
                target.data = json!({ "source": "branch-off" });
            }
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        HostPurpose::AddProject => {
            let target = PanelTarget::new("new_tab", &host_id, "", "").with_title("Add project").with_data(json!({ "addProject": true }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        HostPurpose::Usage => show_page(shell, cx, Page::Settings { host_id: Some(host_id), page: "host/usage".into() }),
        HostPurpose::ManageLabels => {
            shell.data.overlays.open_sheet(Box::new(labels::LabelsSheet::new(host_id, None)));
            shell.redraw(cx);
        }
        HostPurpose::ProjectPath(path) => open_project_path(shell, ui, &host_id, &path),
        HostPurpose::SessionHistory => show_sessions(shell, cx, &host_id, true, "", ""),
    }
}

/// `openProjectPath`: `project.add` on a capable connected host; the sidebar shows it.
fn open_project_path(shell: &mut PaseoShell, ui: &UiContext, host_id: &str, path: &str) {
    let trimmed = path.trim();
    let Some(host) = hosts::host(&ui.state, host_id) else { return };
    if trimmed.is_empty() || !host.connected() || !host.supports("projectAdd") || !host.supports("stableProjectIdentity") {
        return;
    }
    request(shell, ui, host_id, json!({ "type": "project.add.request", "cwd": trimmed }), "project.add.response", Pending::Refresh { host_id: host_id.to_string() });
}

pub(crate) fn new_session(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, new_workspace: bool, worktree: bool) {
    choose_host(shell, cx, ui, HostPurpose::NewSession { new_workspace, worktree });
}

/// `_openWorkspacePanel`.
fn open_workspace_panel(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, kind: &str, explorer: bool) {
    let Some(target) = selected(shell).filter(|t| !t.workspace_id.is_empty()) else {
        toast(shell, cx, "Open a session or workspace first.", true);
        return;
    };
    let next = PanelTarget::new(kind, &target.server_id, &target.workspace_id, &target.cwd);
    open_tab(shell, cx, ui, next, if explorer { TabPlacement::Explorer } else { TabPlacement::Main });
}

/// `_openDefaultExplorer`: Files and Changes, Changes in front.
fn open_default_explorer(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext) {
    open_workspace_panel(shell, cx, ui, "files", true);
    open_workspace_panel(shell, cx, ui, "changes_tree", true);
}

/// `_toggleCompactExplorer`.
fn toggle_compact_explorer(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext) {
    let layout = &shell.data.layout;
    if layout.compact_destination == CompactDestination::Explorer {
        shell.data.layout.show_compact_explorer(false);
        shell.redraw(cx);
        return;
    }
    let Some(target) = layout.selected().cloned().filter(|t| !t.workspace_id.is_empty()) else { return };
    if in_workspace(layout.selected_explorer_target(), &target) {
        shell.data.layout.show_compact_explorer(true);
        shell.redraw(cx);
        return;
    }
    let views: Vec<&PanelTarget> = layout.explorer_targets().into_iter().filter(|t| in_workspace(Some(t), &target)).collect();
    let existing = layout
        .explorer_focus_for(&target)
        .or_else(|| views.iter().find(|t| t.kind == "changes_tree").or(views.first()).map(|t| t.key()));
    match existing {
        Some(key) => shell.data.layout.select_explorer_tab(&key),
        None => open_default_explorer(shell, cx, ui),
    }
    shell.redraw(cx);
}

/// `_splitWithLauncher`.
fn split_with_launcher(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, axis: Axis) {
    let source = selected(shell);
    let focused = shell.data.layout.focused().id.clone();
    shell.data.layout.split_pane(&focused, axis);
    if let Some(source) = source {
        let target = PanelTarget::new("new_tab", &source.server_id, &source.workspace_id, &source.cwd)
            .with_id(new_id())
            .with_title(tr("sessionNavigation.newSession"));
        open_tab(shell, cx, ui, target, TabPlacement::Main);
    }
    shell.redraw(cx);
}

/// `_workspaceRootAgents`.
fn workspace_root_agents<'a>(state: &'a Value, workspace: &PanelTarget) -> Vec<&'a Value> {
    let Some(host) = hosts::host(state, &workspace.server_id) else { return Vec::new() };
    let agents: Vec<&Value> = host.agents().collect();
    let parent_workspace = |id: &str| agents.iter().find(|a| j::text(a, "id") == id).map(|a| j::text(a, "workspaceId")).unwrap_or("");
    agents
        .iter()
        .copied()
        .filter(|agent| {
            if j::text(agent, "workspaceId") != workspace.workspace_id || hosts::is_archived(agent) || j::text(agent, "id").is_empty() {
                return false;
            }
            let parent = parent_agent_id(agent);
            if parent.is_empty() {
                return true;
            }
            let pw = parent_workspace(&parent);
            !pw.is_empty() && pw != workspace.workspace_id
        })
        .collect()
}

/// `_workspaceTabs`: pane tabs of the workspace (not Files/Changes), root agents in agent-id
/// order across the slots the opened ones hold, unopened ones right after the last slot.
pub fn workspace_tabs(shell: &PaseoShell, state: &Value, target: &PanelTarget) -> Vec<PanelTarget> {
    let layout = &shell.data.layout;
    let tabs: Vec<PanelTarget> = layout
        .panes()
        .iter()
        .flat_map(|pane| pane.tabs.iter())
        .filter_map(|key| layout.target(key))
        .filter(|tab| in_workspace(Some(tab), target) && tab.kind != "files" && tab.kind != "changes_tree")
        .cloned()
        .collect();
    if target.workspace_id.is_empty() {
        return tabs;
    }
    let roots = workspace_root_agents(state, target);
    let root_ids: Vec<&str> = roots.iter().map(|a| j::text(a, "id")).collect();
    let reconciled = |tab: &PanelTarget| tab.kind == "agent" && tab.id.as_deref().is_some_and(|id| root_ids.contains(&id));
    let open: Vec<String> = tabs.iter().filter(|t| t.kind == "agent").filter_map(|t| t.id.clone()).collect();
    let mut agents: Vec<PanelTarget> = tabs.iter().filter(|t| reconciled(t)).cloned().collect();
    agents.extend(
        roots
            .iter()
            .filter(|a| !open.iter().any(|id| id == j::text(a, "id")))
            .map(|a| navigation_agent_target(&target.server_id, a, None)),
    );
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    let slots = tabs.iter().filter(|t| reconciled(t)).count();
    let last_slot = tabs.iter().rposition(&reconciled);
    let mut out = Vec::new();
    if last_slot.is_none() {
        out.extend(agents.iter().cloned());
    }
    let mut slot = 0;
    for (index, tab) in tabs.iter().enumerate() {
        if reconciled(tab) {
            out.push(agents[slot].clone());
            slot += 1;
        } else {
            out.push(tab.clone());
        }
        if Some(index) == last_slot {
            out.extend(agents.iter().skip(slots).cloned());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Closing (closeTab / closePane / canCloseWindow with guards)

/// Starts closing `keys` (tabs) or asking their guards, then `after`.
pub fn close_tabs(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, keys: Vec<String>, remove: bool, after: AfterClose) {
    // The lifecycle smoke's registered dirty-document guard refuses window close and quit.
    #[cfg(feature = "desktop-smoke")]
    if matches!(after, AfterClose::Window | AfterClose::Quit(_)) && crate::smoke::close_guard_refuses() {
        if let AfterClose::Quit(token) = after {
            vote_quit(token, false);
        }
        return;
    }
    if shell.data.closing.is_some() {
        // One close flow at a time; a second request waits for the user to finish the first.
        if let AfterClose::Quit(token) = after {
            vote_quit(token, false);
        }
        return;
    }
    shell.data.closing = Some(Closing { queue: keys.into(), remove, after });
    advance_closing(shell, cx, ui);
}

/// `_closeWorkspaceTabs`: the bulk confirmation, then each tab's guard.
fn close_bulk(shell: &mut PaseoShell, cx: &mut Cx, keys: Vec<String>, title_key: &str, after: AfterClose) {
    if keys.is_empty() {
        return;
    }
    let count = keys.len().to_string();
    let message = tr_args("workspace.tabs.confirmations.bulk.tabs", &[("tabs", &count)]);
    confirm(shell, cx, &tr(title_key), &message, &tr("workspace.tabs.confirmations.close"), false, ShellOverlay::BulkClose { keys, after });
}

fn advance_closing(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext) {
    loop {
        let Some(closing) = shell.data.closing.as_mut() else { return };
        let Some(key) = closing.queue.pop_front() else { break };
        let remove = closing.remove;
        // Removing a saved host only forgets its tabs; the daemon keeps its terminals.
        let kills_terminals = remove && !matches!(closing.after, AfterClose::RemoveHost(_));
        let Some(target) = shell.data.layout.target(&key).cloned() else { continue };
        if shell.data.dirty.contains(&key) {
            if target.kind == "file" {
                if panels::file_close_requested(&key) {
                    confirm(
                        shell,
                        cx,
                        &tr("workspace.tabs.confirmations.unsavedTitle"),
                        &tr("workspace.tabs.confirmations.unsavedMessage"),
                        &tr("workspace.tabs.confirmations.closeWithoutSaving"),
                        true,
                        ShellOverlay::Guard { key, file: true },
                    );
                    return;
                }
            } else if matches!(target.kind.as_str(), "agent" | "draft" | "provider_subagent") {
                // Agent panel guard: a send or upload in flight refuses the close.
                toast(shell, cx, "Wait for the current send or upload to finish before closing this session.", true);
                abort_closing(shell);
                return;
            } else {
                confirm(
                    shell,
                    cx,
                    &tr("workspace.tabs.confirmations.unsavedTitle"),
                    &tr("workspace.tabs.confirmations.unsavedMessage"),
                    &tr("workspace.tabs.confirmations.closeWithoutSaving"),
                    true,
                    ShellOverlay::Guard { key, file: false },
                );
                return;
            }
        }
        if kills_terminals
            && target.kind == "terminal"
            && let Some(terminal_id) = target.id.clone().filter(|id| !id.is_empty())
        {
            check_terminal_close(shell, cx, ui, key, &target, terminal_id);
            return;
        }
        if remove {
            remove_tab(shell, cx, ui, &key);
        }
    }
    let Some(closing) = shell.data.closing.take() else { return };
    finish_closing(shell, cx, ui, closing.after);
}

fn abort_closing(shell: &mut PaseoShell) {
    if let Some(closing) = shell.data.closing.take()
        && let AfterClose::Quit(token) = closing.after
    {
        vote_quit(token, false);
    }
}

/// A live terminal asks `closeTerminalTitle` first; one that already exited (gone from the
/// daemon's live list) closes directly. A host without a connection cannot answer, so the
/// terminal counts as live.
fn check_terminal_close(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, key: String, target: &PanelTarget, terminal_id: String) {
    let host_id = target.server_id.clone();
    if !hosts::host(&ui.state, &host_id).is_some_and(|h| h.connected()) {
        confirm_terminal_close(shell, cx, key, host_id, terminal_id);
        return;
    }
    let mut message = json!({ "type": "list_terminals_request" });
    if target.workspace_id.is_empty() {
        message["cwd"] = json!(target.cwd);
    } else {
        message["workspaceId"] = json!(target.workspace_id);
    }
    let pending = Pending::TerminalCloseCheck { key, host_id: host_id.clone(), terminal_id };
    request(shell, ui, &host_id, message, "list_terminals_response", pending);
}

fn confirm_terminal_close(shell: &mut PaseoShell, cx: &mut Cx, key: String, host_id: String, terminal_id: String) {
    confirm(
        shell,
        cx,
        &tr("workspace.tabs.confirmations.closeTerminalTitle"),
        &tr("workspace.tabs.confirmations.closeTerminalMessage"),
        &tr("workspace.tabs.confirmations.close"),
        true,
        ShellOverlay::CloseTerminal { key, host_id, terminal_id },
    );
}

/// The confirmed terminal tab leaves the layout, then its daemon terminal is killed; a failed
/// kill stays silent like the original's `killTerminal(..).catch(invalidate)`.
fn close_terminal_confirmed(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, key: &str, host_id: &str, terminal_id: &str) {
    remove_tab(shell, cx, ui, key);
    request(shell, ui, host_id, json!({ "type": "kill_terminal_request", "terminalId": terminal_id }), "kill_terminal_response", Pending::Silent);
    advance_closing(shell, cx, ui);
}

/// `_removeTab` plus the shell's release work and the child-tab label (`_closeWorkspaceTab`).
fn remove_tab(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, key: &str) {
    let target = shell.data.layout.target(key).cloned();
    if let Some(panel) = shell.data.panels.remove(key) {
        panels::release(cx, ui, &panel);
    }
    shell.data.layout.close_tab(key);
    shell.data.dirty.remove(key);
    if let Some(target) = target
        && shell.data.marked_child_tabs.remove(key)
    {
        let client = j::text(&ui.state, "clientId").to_string();
        if !client.is_empty() {
            let mut labels = Map::new();
            labels.insert(format!("paseo.open-agent-tab.{client}"), json!("false"));
            request(
                shell,
                ui,
                &target.server_id,
                json!({ "type": "update_agent_request", "agentId": target.id, "labels": labels }),
                "update_agent_response",
                Pending::Plain,
            );
        }
    }
    shell.redraw(cx);
}

fn finish_closing(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, after: AfterClose) {
    match after {
        AfterClose::Nothing => {}
        AfterClose::ClosePane(pane) => {
            shell.data.layout.close_pane(&pane);
        }
        AfterClose::Window => {
            if let Err(error) = crate::features::composer::flush_drafts(ui) {
                toast(shell, cx, error, true);
                return;
            }
            match platform::window_close_requested(crate::shell::open_windows()) {
                Some(request) => shell.data.close_request = Some(request),
                None => close_os_window(shell, cx),
            }
        }
        AfterClose::Quit(token) => {
            if let Err(error) = crate::features::composer::flush_drafts(ui) {
                toast(shell, cx, error, true);
                vote_quit(token, false);
                return;
            }
            vote_quit(token, true);
        }
        AfterClose::RemoveHost(host_id) => {
            // Original `registerHostRemovalGuard`: revoke the host's mobile push registration
            // first and wait for it (best effort, at most 2.5 s).
            match platform::revoke_push_host(&host_id) {
                Some(request) => {
                    let timer = cx.start_timeout(2.5);
                    shell.data.revoking.push((request, host_id, timer));
                }
                None => ui.core.remove_host(&host_id),
            }
        }
        AfterClose::Archive { host_id, workspaces } => archive_workspaces(shell, cx, ui, &host_id, workspaces.into(), Vec::new()),
    }
    shell.redraw(cx);
}

fn close_os_window(shell: &mut PaseoShell, cx: &mut Cx) {
    if let Some(window) = shell.data.os_window {
        shell.data.close_approved = true;
        cx.push_unique_platform_op(CxOsOp::CloseWindow(window));
    }
}

/// Original `_finishWindowClose`: other windows are destroyed; the first window carries daemon
/// and tray ownership, so it is hidden and kept for the next New Window.
fn retire_window(shell: &mut PaseoShell, cx: &mut Cx) {
    if !shell.is_main() || !platform::desktop_host() {
        close_os_window(shell, cx);
        return;
    }
    if let Some(window) = shell.data.os_window {
        platform::hide_window(cx, window);
    }
    crate::shell::set_window_state(&shell.data.window_id, WindowState::Dormant);
}

/// Every open window votes; any refusal answers the quit at once.
fn vote_quit(token: u64, allowed: bool) {
    let respond = QUIT_VOTES.with(|votes| {
        let mut votes = votes.borrow_mut();
        let entry = votes.entry(token).or_insert((0, false));
        if entry.1 {
            return None;
        }
        if !allowed {
            entry.1 = true;
            return Some(false);
        }
        entry.0 += 1;
        if entry.0 >= crate::shell::open_windows() {
            entry.1 = true;
            return Some(true);
        }
        None
    });
    if let Some(allowed) = respond {
        platform::respond_quit(token, allowed);
    }
}

/// Close-guard keys of this window (`canCloseWindow`).
fn guarded_keys(shell: &PaseoShell) -> Vec<String> {
    shell.data.dirty.iter().filter(|k| shell.data.layout.target(k).is_some()).cloned().collect()
}

/// `_closePane`: the last pane stays; otherwise its tabs close in bulk, then the pane.
fn close_pane(shell: &mut PaseoShell, cx: &mut Cx, pane_id: &str) {
    if shell.data.layout.panes().len() <= 1 {
        return;
    }
    let Some(pane) = shell.data.layout.tree.pane(pane_id) else { return };
    let keys = pane.tabs.clone();
    if keys.is_empty() {
        shell.data.layout.close_pane(pane_id);
        shell.redraw(cx);
        return;
    }
    close_bulk(shell, cx, keys, "workspace.tabs.confirmations.closePaneTitle", AfterClose::ClosePane(pane_id.to_string()));
}

/// Archived/removed workspaces leave the layout.
pub fn close_workspace_tabs(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, workspace_ids: &[String]) {
    let keys: Vec<String> = shell
        .data
        .layout
        .tabs
        .iter()
        .filter(|t| t.server_id == host_id && workspace_ids.contains(&t.workspace_id))
        .map(PanelTarget::key)
        .collect();
    close_tabs(shell, cx, ui, keys, true, AfterClose::Nothing);
}

// ---------------------------------------------------------------------------------------------
// Archive / recover (navigation_actions.dart)

fn archive_workspaces(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, mut remaining: VecDeque<Value>, mut confirmed: Vec<Value>) {
    use crate::features::git::policy::{resolve_risk_reasons, worktree_archive_risk_reasons};
    while let Some(workspace) = remaining.pop_front() {
        if j::text(&workspace, "workspaceKind") == "worktree" {
            let git = j::object(&workspace, "gitRuntime");
            let diff = workspace.get("diffStat").filter(|v| v.is_object());
            let reasons = worktree_archive_risk_reasons(
                git.get("isDirty").and_then(Value::as_bool),
                git.get("aheadOfOrigin").and_then(Value::as_i64),
                diff,
            );
            let lines = resolve_risk_reasons(&reasons, &|msg| {
                let args: Vec<(&str, &str)> = msg.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
                tr_args(&msg.key, &args)
            });
            if !lines.is_empty() {
                let name = j::text_or(&workspace, "name", j::text(&workspace, "id")).to_string();
                confirm(
                    shell,
                    cx,
                    &tr_args("workspace.git.actions.archiveWarning.title", &[("workspaceName", &name)]),
                    &lines.join("\n"),
                    &tr("workspace.git.actions.archiveWarning.confirm"),
                    true,
                    ShellOverlay::ArchiveRisk { host_id: host_id.to_string(), current: workspace, remaining: remaining.into(), confirmed },
                );
                return;
            }
        }
        confirmed.push(workspace);
    }
    for workspace in confirmed {
        let workspace_id = j::text(&workspace, "id").to_string();
        request(
            shell,
            ui,
            host_id,
            json!({ "type": "archive_workspace_request", "workspaceId": workspace_id }),
            "archive_workspace_response",
            Pending::ArchiveWorkspace { host_id: host_id.to_string(), workspace_id },
        );
    }
}

fn recover_workspace(shell: &mut PaseoShell, ui: &UiContext, host_id: &str, workspace_id: &str) {
    request(
        shell,
        ui,
        host_id,
        json!({ "type": "workspace.recovery.inspect.request", "workspaceId": workspace_id }),
        "workspace.recovery.inspect.response",
        Pending::RecoverInspect { host_id: host_id.to_string(), workspace_id: workspace_id.to_string() },
    );
}

fn workspace_value(state: &Value, host_id: &str, workspace_id: &str) -> Option<Value> {
    hosts::host(state, host_id).and_then(|h| h.workspace(workspace_id).cloned())
}

// ---------------------------------------------------------------------------------------------
// Pages

pub fn page_route(page: &Page) -> Route {
    match page {
        Page::Settings { host_id, page } => Route::Settings { host_id: host_id.clone(), page: page.clone() },
        Page::Schedules { host_id } => Route::Schedules { host_id: host_id.clone() },
        Page::Sessions { host_id, all_hosts, .. } => Route::Sessions { host_id: (!*all_hosts).then(|| host_id.clone()) },
    }
}

pub fn show_page(shell: &mut PaseoShell, cx: &mut Cx, page: Page) {
    if let Page::Sessions { host_id, all_hosts, cwd, project_key } = &page {
        shell.data.history = Some(history::HistoryPage::new(host_id, *all_hosts, cwd, project_key));
    }
    shell.data.page_serial = shell.data.layout.navigation_serial;
    shell.data.page = Some(page);
    shell.redraw(cx);
}

pub(crate) fn show_sessions(shell: &mut PaseoShell, cx: &mut Cx, host_id: &str, all_hosts: bool, cwd: &str, project_key: &str) {
    show_page(
        shell,
        cx,
        Page::Sessions { host_id: host_id.to_string(), all_hosts, cwd: cwd.to_string(), project_key: project_key.to_string() },
    );
}

pub fn close_page(shell: &mut PaseoShell, cx: &mut Cx) {
    shell.data.page = None;
    shell.data.page_root = None;
    shell.data.history = None;
    shell.redraw(cx);
}

pub(crate) fn open_settings(shell: &mut PaseoShell, cx: &mut Cx, host_id: Option<String>, page: &str) {
    show_page(shell, cx, Page::Settings { host_id, page: page.to_string() });
}

fn open_command_center(shell: &mut PaseoShell, cx: &mut Cx, files: bool) {
    if shell.data.command_center.is_none() {
        shell.data.command_center = Some(panels::command_center(cx, &shell.templates));
    }
    if let Some(widget) = shell.data.command_center.clone() {
        shell.data.router.reset(cx);
        panels::open_command_center(cx, &widget, files);
        shell.data.command_center_open = true;
    }
    shell.redraw(cx);
}

// ---------------------------------------------------------------------------------------------
// Commands (`_execute`)

/// Offers `action` to the focused tab first (Flutter `PaseoCommands.invoke`); the tab returns
/// it unhandled to [`execute`] otherwise.
pub fn run_command(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, action: &str, args: &Value) {
    if let Some(key) = chrome::focused_tab(shell).filter(|k| shell.data.panels.contains_key(k)) {
        shell.queue_command(key, action.to_string(), args.clone());
        return;
    }
    execute(shell, cx, ui, action, args);
}

fn direction(action: &str) -> Direction {
    match action.rsplit('.').next().unwrap_or("") {
        "left" => Direction::Left,
        "right" => Direction::Right,
        "up" => Direction::Up,
        _ => Direction::Down,
    }
}

pub fn execute(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, action: &str, args: &Value) {
    let target = selected(shell);
    let compact = shell.data.compact;
    let int = |key: &str, fallback: i64| j::int(args, key, fallback);
    match action {
        "agent.new" | "workspace.project.pick" | "workspace.new" => new_session(shell, cx, ui, true, false),
        "worktree.new" => new_session(shell, cx, ui, true, true),
        "workspace.tab.menu.open" | "workspace.tab.target.agent" => new_session(shell, cx, ui, false, false),
        "workspace.terminal.new" => new_terminal(shell, cx, ui, TabPlacement::Main),
        "workspace.tab.target.changes" => open_workspace_panel(shell, cx, ui, "changes_tree", true),
        "workspace.tab.target.files" => open_workspace_panel(shell, cx, ui, "files", true),
        // Closing a view must not change the session's lifecycle.
        "workspace.tab.close.current" => {
            if let Some(target) = target {
                close_tabs(shell, cx, ui, vec![target.key()], true, AfterClose::Nothing);
            }
        }
        "workspace.tab.close-left" | "workspace.tab.close-right" | "workspace.tab.close-others" => {
            let pane = shell.data.layout.focused().clone();
            let Some(active) = pane.selected.as_ref().and_then(|s| pane.tabs.iter().position(|k| k == s)) else { return };
            let keys: Vec<String> = pane
                .tabs
                .iter()
                .enumerate()
                .filter(|(index, _)| match action {
                    "workspace.tab.close-left" => *index < active,
                    "workspace.tab.close-right" => *index > active,
                    _ => *index != active,
                })
                .map(|(_, k)| k.clone())
                .collect();
            let title = match action {
                "workspace.tab.close-left" => "workspace.tabs.confirmations.closeTabsLeftTitle",
                "workspace.tab.close-right" => "workspace.tabs.confirmations.closeTabsRightTitle",
                _ => "workspace.tabs.confirmations.closeOtherTabsTitle",
            };
            close_bulk(shell, cx, keys, title, AfterClose::Nothing);
        }
        "workspace.tab.navigate.index" => {
            let keys = shell.data.layout.focused().tabs.clone();
            let index = int("index", 1) - 1;
            if index >= 0 && (index as usize) < keys.len() {
                shell.data.layout.select_tab(&keys[index as usize]);
            }
        }
        "workspace.tab.navigate.relative" => {
            let pane = shell.data.layout.focused().clone();
            if !pane.tabs.is_empty() {
                let current = pane.selected.as_ref().and_then(|s| pane.tabs.iter().position(|k| k == s)).map_or(-1, |i| i as i64);
                let len = pane.tabs.len() as i64;
                let next = (current + int("delta", 1)).rem_euclid(len) as usize;
                shell.data.layout.select_tab(&pane.tabs[next]);
            }
        }
        "workspace.pane.split.right" => split_with_launcher(shell, cx, ui, Axis::Horizontal),
        "workspace.pane.split.down" => split_with_launcher(shell, cx, ui, Axis::Vertical),
        "workspace.pane.close" => {
            let pane = shell.data.layout.focused().id.clone();
            close_pane(shell, cx, &pane);
        }
        "workspace.pane.focus.left" | "workspace.pane.focus.right" | "workspace.pane.focus.up" | "workspace.pane.focus.down" => {
            shell.data.layout.focus_adjacent_pane(direction(action));
        }
        "workspace.pane.move-tab.left" | "workspace.pane.move-tab.right" | "workspace.pane.move-tab.up" | "workspace.pane.move-tab.down" => {
            shell.data.layout.move_focused_tab(direction(action));
        }
        "sidebar.toggle.left" => {
            let show = if compact {
                shell.data.page.is_some() || shell.data.layout.compact_destination != CompactDestination::Sidebar
            } else {
                !shell.data.layout.sidebar_visible
            };
            shell.data.layout.show_sidebar(show);
            if compact && show && shell.data.page.is_some() {
                close_page(shell, cx);
            }
        }
        "sidebar.toggle.right" => {
            if compact {
                toggle_compact_explorer(shell, cx, ui);
            } else if shell.data.layout.explorer_tabs.is_empty() {
                open_default_explorer(shell, cx, ui);
            } else {
                let visible = shell.data.layout.explorer_visible;
                shell.data.layout.show_explorer(!visible);
            }
        }
        "sidebar.toggle.both" => {
            let layout = &shell.data.layout;
            let show = !layout.sidebar_visible || !layout.explorer_visible;
            shell.data.layout.show_sidebar(show);
            if !compact {
                shell.data.layout.show_explorer(show);
            }
        }
        "view.toggle.focus" => {
            let maximized = shell.data.layout.maximized_pane.clone();
            let focused = shell.data.layout.focused().id.clone();
            shell.data.layout.maximize_pane(if maximized.is_none() { Some(focused) } else { None });
        }
        "settings.toggle" => open_settings(shell, cx, None, ""),
        "command-center.toggle" => open_command_center(shell, cx, false),
        "command-center.files" => open_command_center(shell, cx, true),
        "shortcuts.dialog.toggle" => {
            shell.data.overlays.open_sheet(Box::new(shortcut_help::ShortcutHelpSheet::new(shell.data.compact)));
        }
        // New Tab / Command Center "Import session": `{serverId, cwd?}`.
        "session.import" => {
            let host_id = j::text(args, "serverId").to_string();
            let host_id = if host_id.is_empty() { selected(shell).map(|t| t.server_id).unwrap_or_default() } else { host_id };
            if !host_id.is_empty() {
                shell.data.overlays.open_sheet(Box::new(history::ImportSheet::new(&host_id, j::text(args, "cwd"))));
            }
        }
        "theme.cycle" => {
            let current = ui.preference("theme").and_then(Value::as_str).unwrap_or("auto").to_string();
            let next = match current.as_str() {
                "auto" => "light",
                "light" => "dark",
                _ => "auto",
            };
            set_preference(shell, ui, "theme", json!(next));
        }
        "workspace.archive" => {
            let Some(target) = target else { return };
            let Some(workspace) = workspace_value(&ui.state, &target.server_id, &target.workspace_id) else { return };
            let keys = guarded_keys(shell);
            close_tabs(shell, cx, ui, keys, false, AfterClose::Archive { host_id: target.server_id, workspaces: vec![workspace] });
        }
        "workspace.pin" => {
            let Some(target) = target.filter(|t| !t.workspace_id.is_empty()) else { return };
            let pinned = workspace_value(&ui.state, &target.server_id, &target.workspace_id)
                .is_none_or(|w| j::is_null_or_missing(&w, "pinnedAt"));
            request(
                shell,
                ui,
                &target.server_id,
                json!({ "type": "workspace.pin.set.request", "workspaceId": target.workspace_id, "pinned": pinned }),
                "workspace.pin.set.response",
                Pending::Refresh { host_id: target.server_id.clone() },
            );
        }
        "workspace.navigate.index" | "workspace.navigate.relative" => navigate_session(shell, cx, ui, action, args),
        "agent.interrupt" => {
            if let Some(target) = target.filter(|t| t.kind == "agent" && t.id.is_some()) {
                request(
                    shell,
                    ui,
                    &target.server_id,
                    json!({ "type": "cancel_agent_request", "agentId": target.id }),
                    "cancel_agent_response",
                    Pending::Plain,
                );
            }
        }
        "app.new-window" => request_new_window(),
        _ => {
            if action.starts_with("message-input.") && !target.as_ref().is_some_and(|t| t.kind == "agent" || t.kind == "draft") {
                return;
            }
            if action.starts_with("conversation") {
                return;
            }
            toast(shell, cx, format!("Command is unavailable in this view: {action}"), true);
        }
    }
    shell.redraw(cx);
}

fn request_new_window() {
    if platform::desktop_host() {
        crate::shell::request_window(WindowRequest::default());
    }
}

/// `_navigateSession`.
fn navigate_session(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, action: &str, args: &Value) {
    let state = ui.state.clone();
    let mut sessions: Vec<(String, Value)> = Vec::new();
    for host in hosts::hosts(&state) {
        for agent in host.agents() {
            if !hosts::is_archived(agent) && j::is_null_or_missing(agent, "parentAgentId") && !j::text(agent, "workspaceId").is_empty() {
                sessions.push((host.server_id().to_string(), agent.clone()));
            }
        }
    }
    if sessions.is_empty() {
        return;
    }
    let current = selected(shell);
    let len = sessions.len() as i64;
    let index = if action.ends_with(".index") {
        j::int(args, "index", 1) - 1
    } else {
        let at = sessions
            .iter()
            .position(|(sid, agent)| current.as_ref().is_some_and(|c| c.server_id == *sid && c.id.as_deref() == Some(j::text(agent, "id"))))
            .map_or(-1, |i| i as i64);
        (at + j::int(args, "delta", 1)).rem_euclid(len)
    };
    if index >= 0 && index < len {
        let (sid, agent) = sessions[index as usize].clone();
        open_navigation_agent(shell, cx, ui, &sid, &agent, None);
    }
}

// ---------------------------------------------------------------------------------------------
// Keyboard and back

/// Returns true when the key was consumed by a shortcut.
pub fn handle_shortcut(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, key: &KeyEvent) -> bool {
    if shell.data.focus_scope.name() == "shortcut-capture" {
        return false;
    }
    // Original application menu "New Window" (CmdOrCtrl+Shift+N).
    let m = key.modifiers;
    let mac = cfg!(target_os = "macos");
    if key.key_code == KeyCode::KeyN && m.shift && !m.alt && if mac { m.logo && !m.control } else { m.control && !m.logo } && platform::desktop_host() {
        request_new_window();
        return true;
    }
    let focus = shell.data.focus_scope.name();
    // A focused terminal tab owns the keyboard unless a field claimed another scope: its shell
    // keys (Ctrl+W, Ctrl+L, Ctrl+K, ...) must not become app shortcuts (`terminal: false`).
    let terminal = focus == "workspace" && shell.data.page.is_none() && chrome::focused_target(shell).is_some_and(|t| t.kind == "terminal");
    let scope = if terminal { "terminal" } else { focus };
    let command_center = shell.data.command_center_open;
    match shell.data.router.handle_key(cx, key, scope, command_center) {
        None => false,
        Some(None) => true,
        Some(Some(dispatch)) => {
            run_command(shell, cx, ui, &dispatch.action, &dispatch.args);
            !dispatch.pass_through
        }
    }
}

/// `_back`: compact detail tabs close (back to the tab they were opened from), then explorer →
/// agent → sidebar; false lets the platform background the app (mobile) — the session never stops.
pub fn back(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext) -> bool {
    if shell.data.page.is_some() {
        close_page(shell, cx);
        return true;
    }
    let compact = shell.data.compact;
    if let Some(key) = shell.data.layout.compact_back_tab(compact) {
        close_tabs(shell, cx, ui, vec![key], true, AfterClose::Nothing);
        return true;
    }
    if shell.data.layout.handle_back(compact) {
        shell.redraw(cx);
        return true;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Effects from feature roots

pub fn apply_effect(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, effect: UiEffect) {
    match effect {
        UiEffect::Navigate(route) => navigate(shell, cx, ui, route),
        UiEffect::OpenSession { host_id, agent_id } => open_agent(shell, ui, &host_id, &agent_id, None, TabPlacement::Main),
        UiEffect::OpenSessionWithError { host_id, agent_id, error } => {
            open_agent(shell, ui, &host_id, &agent_id, Some(error), TabPlacement::Main)
        }
        UiEffect::CloseTab { tab_key } => close_tabs(shell, cx, ui, vec![tab_key], true, AfterClose::Nothing),
        UiEffect::Toast { message, is_error } => toast(shell, cx, message, is_error),
        UiEffect::OpenExternal { url } => {
            if platform::open_external(&url).is_none() {
                toast(shell, cx, "Could not open the link in the system browser.", true);
            }
        }
        UiEffect::OpenFile { host_id, workspace_id, path } => open_file(shell, cx, ui, &host_id, &workspace_id, &path, false),
        UiEffect::OpenFileBeside { host_id, workspace_id, path } => open_file(shell, cx, ui, &host_id, &workspace_id, &path, true),
        UiEffect::SetCloseGuard { owner, dirty } => {
            let changed = if dirty { shell.data.dirty.insert(owner) } else { shell.data.dirty.remove(&owner) };
            if changed {
                shell.redraw(cx);
            }
        }
        UiEffect::OpenTab { target, placement } => open_tab(shell, cx, ui, target, placement),
        UiEffect::OpenDraft { host_id, workspace_id, cwd, draft_id, title, data } => {
            let target = PanelTarget::new("draft", &host_id, &workspace_id, &cwd).with_id(draft_id).with_title(title).with_data(data);
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        UiEffect::ReplaceTab { tab_key, target } => {
            if let Some(panel) = shell.data.panels.remove(&tab_key) {
                panels::release(cx, ui, &panel);
            }
            shell.data.dirty.remove(&tab_key);
            mark_child_tab_open(shell, ui, &target);
            shell.data.layout.replace_tab(&tab_key, target);
            shell.redraw(cx);
        }
        UiEffect::SetPreference { key, value } => {
            set_preference(shell, ui, &key, value);
            refresh_display_menu(shell, cx, ui);
            shell.redraw(cx);
        }
        UiEffect::SetFocusScope { owner, scope } => shell.data.focus_scope.update(owner, scope),
        UiEffect::RunCommand { action, args } => run_command(shell, cx, ui, &action, &args),
        UiEffect::CopyText { text, toast: label } => {
            if platform::copy_text(text.clone()).is_none() {
                cx.copy_to_clipboard(&text);
            }
            let message = label.unwrap_or_else(|| tr("common.states.copied"));
            toast(shell, cx, message, false);
        }
        UiEffect::OpenHostDialog => host_dialog::open_flow(shell, cx, None, None, None),
        UiEffect::OpenHostPairing { host_id } => {
            shell.data.overlays.open_sheet(Box::new(host_dialog::PairingSheet::new(host_id)));
        }
        UiEffect::CloseSettings => close_page(shell, cx),
        UiEffect::RemoveHost { host_id } => {
            let keys: Vec<String> = shell.data.layout.tabs.iter().filter(|t| t.server_id == host_id).map(PanelTarget::key).collect();
            close_tabs(shell, cx, ui, keys, true, AfterClose::RemoveHost(host_id));
        }
        UiEffect::Redraw => shell.redraw(cx),
    }
}

/// `Route::Files { path }`: `None` explorer, a trailing `/` reveals that folder, else a file.
fn open_file(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, host_id: &str, workspace_id: &str, path: &str, beside: bool) {
    let cwd = workspace_value(&ui.state, host_id, workspace_id)
        .map(|w| j::text(&w, "workspaceDirectory").to_string())
        .unwrap_or_default();
    if path.ends_with('/') {
        let target = PanelTarget::new("files", host_id, workspace_id, &cwd).with_data(json!({ "reveal": path }));
        open_tab(shell, cx, ui, target, TabPlacement::Explorer);
        return;
    }
    let (file, location) = match path.find("#L") {
        Some(i) => (&path[..i], Some(&path[i + 1..])),
        None => (path, None),
    };
    let mut data = json!({ "path": file });
    if let Some(location) = location {
        data["location"] = json!(location);
        if let Some(line) = location.trim_start_matches('L').split(['-', ':']).next().and_then(|l| l.parse::<i64>().ok()) {
            data["line"] = json!(line);
        }
    }
    let target = PanelTarget::new("file", host_id, workspace_id, &cwd).with_id(file).with_data(data);
    open_tab(shell, cx, ui, target, if beside { TabPlacement::Beside } else { TabPlacement::Main });
}

fn navigate(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, route: Route) {
    let state = ui.state.clone();
    let cwd_of = |host: &str, ws: &str| workspace_value(&state, host, ws).map(|w| j::text(&w, "workspaceDirectory").to_string()).unwrap_or_default();
    match route {
        Route::Welcome => close_page(shell, cx),
        Route::Session { host_id, agent_id } => open_agent(shell, ui, &host_id, &agent_id, None, TabPlacement::Main),
        Route::Subagent { host_id, parent_agent_id, subagent_id } => {
            let parent = hosts::host(&state, &host_id).and_then(|h| h.agent(&parent_agent_id).cloned()).unwrap_or(Value::Null);
            let target = PanelTarget::new("provider_subagent", &host_id, j::text(&parent, "workspaceId"), j::text(&parent, "cwd"))
                .with_id(subagent_id)
                .with_title("Subagent")
                .with_data(json!({ "parentAgentId": parent_agent_id }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        Route::Draft { host_id, workspace_id, draft_id } => {
            let cwd = cwd_of(&host_id, &workspace_id);
            let target = PanelTarget::new("draft", &host_id, &workspace_id, &cwd).with_id(draft_id).with_title(tr("sessionNavigation.newSession"));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        Route::Files { host_id, workspace_id, path: None } => {
            let cwd = cwd_of(&host_id, &workspace_id);
            open_tab(shell, cx, ui, PanelTarget::new("files", &host_id, &workspace_id, &cwd), TabPlacement::Explorer);
        }
        Route::Files { host_id, workspace_id, path: Some(path) } => open_file(shell, cx, ui, &host_id, &workspace_id, &path, false),
        Route::Git { host_id, workspace_id } => {
            let cwd = cwd_of(&host_id, &workspace_id);
            open_tab(shell, cx, ui, PanelTarget::new("changes_tree", &host_id, &workspace_id, &cwd), TabPlacement::Explorer);
        }
        Route::Settings { host_id, page } => open_settings(shell, cx, host_id, &page),
        Route::Schedules { host_id } => show_page(shell, cx, Page::Schedules { host_id }),
        Route::Sessions { host_id } => {
            let all = host_id.is_none();
            let id = host_id.or_else(|| hosts::hosts(&state).first().map(|h| h.server_id().to_string())).unwrap_or_default();
            show_sessions(shell, cx, &id, all, "", "");
        }
        Route::CommandCenter => open_command_center(shell, cx, false),
        Route::Workspace { host_id, workspace_id } => {
            let ws = workspace_id.unwrap_or_default();
            let cwd = cwd_of(&host_id, &ws);
            let target = PanelTarget::new("new_tab", &host_id, &ws, &cwd).with_id(new_id()).with_title(tr("sessionNavigation.newSession"));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        Route::Setup { host_id, workspace_id } => {
            let workspace = workspace_value(&state, &host_id, &workspace_id).unwrap_or(Value::Null);
            let cwd = j::text(&workspace, "workspaceDirectory").to_string();
            let root = j::text_or(&workspace, "projectRootPath", &cwd).to_string();
            let target = PanelTarget::new("setup", &host_id, &workspace_id, &cwd).with_data(json!({ "repoRoot": root }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        Route::ProjectSettings { host_id, project_key } => {
            let target = PanelTarget::new("project_settings", &host_id, "", &project_key).with_data(json!({ "projectKey": project_key }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        Route::ExecutionLog { host_id, workspace_id, terminal_id } => {
            // An unknown workspace refreshes its host instead of opening.
            open_execution_log(shell, cx, ui, &host_id, &workspace_id, &terminal_id, None);
        }
        Route::Terminal { host_id, workspace_id, terminal_id } => {
            let cwd = cwd_of(&host_id, &workspace_id);
            open_tab(shell, cx, ui, PanelTarget::new("terminal", &host_id, &workspace_id, &cwd).with_id(terminal_id), TabPlacement::Main);
        }
    }
}

/// `openExecutionLog`: the workspace must be known (refresh when it is not yet). A terminal
/// already open as an interactive tab (attention notifications) is shown there instead.
fn open_execution_log(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, workspace_id: &str, terminal_id: &str, title: Option<String>) -> bool {
    let open = shell.data.layout.tabs.iter().find(|t| t.kind == "terminal" && t.server_id == host_id && t.id.as_deref() == Some(terminal_id)).cloned();
    if let Some(target) = open {
        open_tab(shell, cx, ui, target, TabPlacement::Main);
        return true;
    }
    let Some(workspace) = workspace_value(&ui.state, host_id, workspace_id) else {
        ui.core.refresh_host(host_id);
        return false;
    };
    let cwd = j::text_or(&workspace, "workspaceDirectory", j::text(&workspace, "projectRootPath")).to_string();
    let mut target = PanelTarget::new("execution_log", host_id, workspace_id, &cwd).with_id(terminal_id);
    target.title = title;
    open_tab(shell, cx, ui, target, TabPlacement::Main);
    true
}

// ---------------------------------------------------------------------------------------------
// Responses

pub fn on_response(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, _request_id: &str, pending: Pending, result: &Result<Value, CoreError>) {
    match pending {
        Pending::OpenAgent { host_id, error, placement } => {
            let payload = match result {
                Ok(payload) => payload,
                Err(e) => return toast(shell, cx, crate::common::error_message(e), true),
            };
            let agent = j::object(payload, "agent");
            if agent.as_object().is_none_or(Map::is_empty) {
                let message = j::text_or(payload, "error", "Session no longer exists.").to_string();
                return toast(shell, cx, message, true);
            }
            let workspace_id = j::text(agent, "workspaceId").to_string();
            let workspace_dir = workspace_value(&ui.state, &host_id, &workspace_id).map(|w| j::text(&w, "workspaceDirectory").to_string()).unwrap_or_default();
            let cwd = j::text_or(agent, "cwd", &workspace_dir).to_string();
            let mut data = json!({ "agent": agent });
            if let Some(error) = error {
                data["submissionError"] = json!(error);
            }
            let host_id = hosts::host(&ui.state, &host_id).map(|h| h.server_id().to_string()).unwrap_or(host_id);
            let target = PanelTarget::new("agent", &host_id, &workspace_id, &cwd)
                .with_id(j::text(agent, "id"))
                .with_title(j::text(agent, "title"))
                .with_data(data);
            open_tab(shell, cx, ui, target, placement);
        }
        Pending::Refresh { host_id } => match response_message(result) {
            Some(message) => toast(shell, cx, message, true),
            None => ui.core.refresh_host(&host_id),
        },
        Pending::ArchiveWorkspace { host_id, workspace_id } => match response_message(result) {
            Some(message) => toast(shell, cx, message, true),
            None => {
                close_workspace_tabs(shell, cx, ui, &host_id, &[workspace_id]);
                ui.core.refresh_host(&host_id);
            }
        },
        Pending::RemoveProject { host_id } => match response_message(result) {
            Some(message) => toast(shell, cx, message, true),
            None => {
                let removed: Vec<String> = result
                    .as_ref()
                    .map(|p| j::array(p, "removedWorkspaceIds").iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                close_workspace_tabs(shell, cx, ui, &host_id, &removed);
                ui.core.refresh_host(&host_id);
            }
        },
        Pending::ReloadAgent { tab_key } => match response_message(result) {
            Some(message) => toast(shell, cx, message, true),
            None => {
                shell.queue_command(tab_key, "conversation.sync".into(), json!({}));
                toast(shell, cx, tr("workspace.tabs.toasts.reloadedAgent"), false);
            }
        },
        Pending::Plain => {
            if let Some(message) = response_message(result) {
                toast(shell, cx, message, true);
            }
        }
        Pending::Silent => {}
        Pending::TerminalCloseCheck { key, host_id, terminal_id } => {
            let live = match result {
                Ok(payload) => j::array(payload, "terminals").iter().any(|t| j::text(t, "id") == terminal_id),
                Err(_) => true,
            };
            if live {
                confirm_terminal_close(shell, cx, key, host_id, terminal_id);
            } else {
                remove_tab(shell, cx, ui, &key);
                advance_closing(shell, cx, ui);
            }
        }
        Pending::CreateTerminal { host_id, workspace_id, cwd, placement } => {
            if let Some(message) = response_message(result) {
                return toast(shell, cx, message, true);
            }
            let terminal = result.as_ref().map(|p| j::object(p, "terminal").clone()).unwrap_or(Value::Null);
            let terminal_id = j::text(&terminal, "id");
            if terminal_id.is_empty() {
                return toast(shell, cx, tr("newWorkspace.errors.createWorktreeFailed"), true);
            }
            let target = PanelTarget::new("terminal", &host_id, j::text_or(&terminal, "workspaceId", &workspace_id), j::text_or(&terminal, "cwd", &cwd))
                .with_id(terminal_id);
            open_tab(shell, cx, ui, target, placement);
        }
        Pending::SavePreference { key, previous } => {
            if let Err(error) = result {
                // The core already holds the failed value in memory: put the previous one back
                // there too (the file on disk still has it).
                let whole = write_preference(ui, &key, previous.unwrap_or(Value::Null));
                ui.core.save_state("preferences", whole);
                toast(shell, cx, crate::common::error_message(error), true);
                cx.redraw_all();
            }
        }
        Pending::Sheet { sheet, tag } => sheets::route_response(shell, cx, ui, sheet, &tag, result),
        Pending::RecoverInspect { host_id, workspace_id } => {
            if let Some(message) = response_message(result) {
                return toast(shell, cx, message, true);
            }
            let state = result.as_ref().map(|p| j::object(p, "state").clone()).unwrap_or(Value::Null);
            if j::text(&state, "kind") != "recoverable" {
                let message = j::text_or(&state, "message", "This workspace cannot be restored.").to_string();
                return toast(shell, cx, message, true);
            }
            let message = format!("{}\n{}\n{}", j::text(&state, "workspaceName"), j::text(&state, "action"), j::text(&state, "branch"));
            confirm(shell, cx, "Restore workspace?", &message, "Restore", false, ShellOverlay::RestoreWorkspace { host_id, workspace_id });
        }
        Pending::LabelCatalog => {
            if let Ok(payload) = result {
                for label in j::array(payload, "labels") {
                    let key = sidebar::label_key(j::text(label, "name"));
                    if !key.is_empty() && !shell.data.label_catalog.iter().any(|(k, _, _)| *k == key) {
                        shell.data.label_catalog.push((key, j::text(label, "name").to_string(), j::text(label, "color").to_string()));
                    }
                }
                refresh_display_menu(shell, cx, ui);
            }
        }
    }
    shell.redraw(cx);
}

// ---------------------------------------------------------------------------------------------
// Pointer: clicks on chrome/sidebar/welcome/page ids

pub fn click(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str, _modifiers: KeyModifiers) {
    let state = ui.state.clone();
    let compact = shell.data.compact;
    if let Some(rest) = id.strip_prefix("welcome:") {
        sheets::welcome::click(shell, cx, ui, rest);
        return;
    }
    if id.starts_with("hist:") {
        history::click(shell, cx, ui, id);
        return;
    }
    match id {
        "banner:settings" => open_settings(shell, cx, None, ""),
        "hdr:sidebar" => {
            let visible = shell.data.layout.sidebar_visible;
            shell.data.layout.show_sidebar(if compact { true } else { !visible });
        }
        "hdr:import" => {
            if let Some(target) = selected(shell) {
                shell.data.overlays.open_sheet(Box::new(history::ImportSheet::new(&target.server_id, "")));
            }
        }
        "hdr:explorer" => {
            if compact {
                toggle_compact_explorer(shell, cx, ui);
            } else if !shell.data.layout.explorer_visible && shell.data.layout.explorer_tabs.is_empty() {
                if selected(shell).is_some_and(|t| !t.workspace_id.is_empty()) {
                    open_default_explorer(shell, cx, ui);
                }
            } else {
                let visible = shell.data.layout.explorer_visible;
                shell.data.layout.show_explorer(!visible);
            }
        }
        "hdr:menu" => header_menu(shell, cx, &state, anchor_of(shell, id)),
        "hdr:scripts" => {
            if let Some(target) = selected(shell)
                && let Some(workspace) = workspace_value(&state, &target.server_id, &target.workspace_id)
            {
                scripts_menu(shell, cx, target, workspace, anchor_of(shell, id));
            }
        }
        "hdr:tabs" => {
            if let Some(target) = selected(shell) {
                shell.data.overlays.open_sheet(Box::new(workspace_tabs::WorkspaceTabsSheet::new(target)));
            }
        }
        "explorer:new" => explorer_new_tab(shell, cx, anchor_of(shell, id)),
        "explorer:close" => {
            if compact {
                shell.data.layout.show_compact_explorer(false);
            } else {
                shell.data.layout.show_explorer(false);
            }
        }
        "empty:new" => new_session(shell, cx, ui, false, false),
        "nav:new-workspace" => new_session(shell, cx, ui, true, false),
        "nav:history" => {
            let host = selected(shell).map(|t| t.server_id).or_else(|| hosts::hosts(&state).first().map(|h| h.server_id().to_string()));
            match host {
                Some(host) => show_sessions(shell, cx, &host, true, "", ""),
                None => host_dialog::open_flow(shell, cx, None, None, None),
            }
        }
        "nav:search" => open_command_center(shell, cx, false),
        "nav:schedules" => {
            let host = selected(shell).map(|t| t.server_id).or_else(|| hosts::hosts(&state).first().map(|h| h.server_id().to_string())).unwrap_or_default();
            show_page(shell, cx, Page::Schedules { host_id: host });
        }
        "nav:close" => shell.data.layout.show_sidebar(false),
        "side:display" => display_menu(shell, cx, ui, anchor_of(shell, id)),
        "side:addhost" => host_dialog::open_flow(shell, cx, None, None, None),
        "foot:addproject" => choose_host(shell, cx, ui, HostPurpose::AddProject),
        "foot:usage" => choose_host(shell, cx, ui, HostPurpose::Usage),
        "foot:hosts" => host_picker(shell, cx, &state, anchor_of(shell, id)),
        "foot:help" => help_menu(shell, cx, anchor_of(shell, id)),
        "foot:settings" => open_settings(shell, cx, None, ""),
        _ => click_prefixed(shell, cx, ui, &state, id),
    }
    shell.redraw(cx);
}

fn click_prefixed(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, state: &Value, id: &str) {
    if let Some(rest) = id.strip_prefix("tab:") {
        if let Some((pane, key)) = rest.split_once(':') {
            if pane == "explorer" {
                shell.data.layout.select_explorer_tab(key);
            } else {
                shell.data.layout.select_tab(key);
                if let Some(target) = shell.data.layout.target(key).cloned() {
                    mark_child_tab_open(shell, ui, &target);
                }
            }
        }
    } else if let Some(rest) = id.strip_prefix("tabmenu:") {
        if let Some((pane, key)) = rest.split_once(':')
            && let Some(target) = shell.data.layout.target(key).cloned()
        {
            tab_menu(shell, cx, target, pane, anchor_of(shell, id), pane == "explorer", false);
        }
    } else if let Some(rest) = id.strip_prefix("tabclose:") {
        if let Some((_, key)) = rest.split_once(':') {
            close_tabs(shell, cx, ui, vec![key.to_string()], true, AfterClose::Nothing);
        }
    } else if let Some(pane) = id.strip_prefix("panenew:") {
        shell.data.layout.focus_pane(pane);
        new_session(shell, cx, ui, false, false);
    } else if let Some(sid) = id.strip_prefix("host:") {
        sidebar::toggle_collapsed(ui, &format!("host:{sid}"));
    } else if let Some(sid) = id.strip_prefix("hostplus:") {
        host_action(shell, cx, ui, sid, "new", &[]);
    } else if let Some(sid) = id.strip_prefix("hostretry:") {
        ui.core.refresh_host(sid);
    } else if let Some(sid) = id.strip_prefix("hostauth:") {
        if let Some(host) = hosts::host(state, sid) {
            host_dialog::open_flow(shell, cx, None, Some(host.config().clone()), None);
        }
    } else if let Some(key) = id.strip_prefix("project:") {
        sidebar::toggle_collapsed(ui, key);
    } else if let Some(key) = id.strip_prefix("projplus:") {
        if let Some((sid, project)) = find_project(state, key) {
            project_action(shell, cx, ui, &sid, &project, "new", &[]);
        }
    } else if let Some(sid) = id.strip_prefix("addproject:") {
        host_chosen(shell, cx, ui, HostPurpose::AddProject, sid.to_string());
    } else if let Some(rest) = id.strip_prefix("session:") {
        if let Some((sid, agent)) = find_session(state, rest) {
            open_navigation_agent(shell, cx, ui, &sid, &agent, None);
            if shell.data.compact {
                shell.data.layout.show_sidebar(false);
            }
        }
    } else if let Some(rest) = id.strip_prefix("sideterm:") {
        if let Some(row) = sidebar::find_terminal(&shell.data.sidebar.terminals, state, rest) {
            open_tab(shell, cx, ui, row.target(), TabPlacement::Main);
            if shell.data.compact {
                shell.data.layout.show_sidebar(false);
            }
        }
    } else if let Some(rest) = id.strip_prefix("sessionedit:")
        && let Some((sid, agent)) = find_session(state, rest)
    {
        session_action(shell, cx, ui, &sid, &agent, "rename");
    }
}

/// Right click / long press.
pub fn secondary(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str, abs: DVec2) {
    let state = ui.state.clone();
    if let Some(rest) = id.strip_prefix("tab:") {
        if let Some((pane, key)) = rest.split_once(':')
            && let Some(target) = shell.data.layout.target(key).cloned()
        {
            tab_menu(shell, cx, target, pane, point(abs), pane == "explorer", true);
        }
    } else if let Some(sid) = id.strip_prefix("host:").or_else(|| id.strip_prefix("hostplus:")) {
        host_menu(shell, cx, &state, sid, point(abs));
    } else if let Some(key) = id.strip_prefix("project:").or_else(|| id.strip_prefix("projplus:")) {
        if let Some((sid, project)) = find_project(&state, key) {
            project_menu(shell, cx, &state, &sid, project, point(abs));
        }
    } else if let Some(rest) = id.strip_prefix("session:").or_else(|| id.strip_prefix("sessionedit:")) {
        if let Some((sid, agent)) = find_session(&state, rest) {
            session_menu(shell, cx, &state, &sid, agent, point(abs));
        }
    } else if id.starts_with("hist:row:") {
        history::secondary(shell, cx, ui, id);
    }
}

fn find_project(state: &Value, key: &str) -> Option<(String, Value)> {
    hosts::hosts(state).into_iter().find_map(|host| {
        sidebar::projects(&host)
            .into_iter()
            .find(|p| sidebar::project_key(&host, p) == key)
            .map(|p| (host.server_id().to_string(), p))
    })
}

fn find_session(state: &Value, rest: &str) -> Option<(String, Value)> {
    let (sid, agent_id) = rest.rsplit_once(':')?;
    let host = hosts::host(state, sid)?;
    host.agent(agent_id).map(|a| (host.server_id().to_string(), a.clone()))
}

// ---------------------------------------------------------------------------------------------
// Menus

/// Original desktop tab menu (`buildWorkspaceTabMenuEntries`).
fn tab_menu(shell: &mut PaseoShell, cx: &mut Cx, target: PanelTarget, pane_id: &str, anchor: Rect, explorer: bool, from_pointer: bool) {
    let agent = target.kind == "agent";
    let id = target.id.clone().unwrap_or_default();
    let keys: Vec<String> = if explorer {
        shell.data.layout.explorer_tabs.clone()
    } else {
        shell.data.layout.tree.pane(pane_id).map(|p| p.tabs.clone()).unwrap_or_default()
    };
    let index = keys.iter().position(|k| *k == target.key());
    let mounted = shell.data.panels.contains_key(&target.key());
    let conversation: Vec<(String, String)> = if mounted {
        panels::conversation_actions(&target).into_iter().filter(|(value, _)| value != "rename").collect()
    } else {
        Vec::new()
    };
    let conversation_reloads = conversation.iter().any(|(value, _)| value == "refresh");
    let maximized = shell.data.layout.maximized_pane.as_deref() == Some(pane_id);
    let compact = shell.data.compact;
    let mut entries = Vec::new();
    if agent {
        entries.push(item("resume", tr("workspace.tabs.menu.copyResumeCommand"), "copy"));
        entries.push(MenuItem::new("agentId", tr("workspace.tabs.menu.copyAgentId")).icon("copy").hint(id.chars().take(7).collect::<String>()).entry());
    }
    if target.kind == "file" {
        entries.push(item("path", tr("workspace.tabs.menu.copyFilePath"), "copy"));
    }
    entries.push(item("rename", tr(if agent { "workspace.tabs.menu.renameAgent" } else { "workspace.tabs.menu.rename" }), "pencil"));
    if !conversation.is_empty() {
        entries.push(MenuEntry::Separator);
        for (value, label) in &conversation {
            entries.push(MenuItem::new(format!("conversation:{value}"), label.clone()).entry());
        }
    }
    entries.push(MenuEntry::Separator);
    if !explorer {
        entries.push(MenuItem::new("left", tr("workspace.tabs.menu.closeLeft")).icon("arrow-left-to-line").enabled(index.is_some_and(|i| i > 0)).entry());
        entries.push(
            MenuItem::new("right", tr("workspace.tabs.menu.closeRight"))
                .icon("arrow-right-to-line")
                .enabled(index.is_some_and(|i| i + 1 < keys.len()))
                .entry(),
        );
        entries.push(MenuItem::new("others", tr("workspace.tabs.menu.closeOthers")).icon("copy-x").enabled(keys.len() > 1).entry());
        if !compact {
            entries.push(MenuEntry::Separator);
            entries.push(item("splitRight", tr("workspace.tabs.actions.splitRight"), "columns-2"));
            entries.push(item("splitDown", tr("workspace.tabs.actions.splitDown"), "rows-2"));
            entries.push(item(
                "maximize",
                tr(if maximized { "workspace.tabs.actions.restorePane" } else { "workspace.tabs.actions.maximizePane" }),
                if maximized { "minimize-2" } else { "maximize-2" },
            ));
            entries.push(item("closePane", tr("workspace.tabs.actions.closePane"), "square-x"));
        }
        entries.push(MenuEntry::Separator);
    }
    if agent && !conversation_reloads {
        entries.push(item("reload", tr("workspace.tabs.menu.reloadAgent"), "rotate-cw"));
    }
    entries.push(item("close", tr("workspace.tabs.menu.close"), "x"));
    let title = chrome::kind_label(&target);
    let options = MenuOptions { align_end: !from_pointer, ..MenuOptions::default() };
    menu(shell, cx, anchor, Some(title), entries, options, ShellOverlay::TabMenu { target, pane_id: pane_id.to_string(), explorer });
}

fn tab_menu_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, target: PanelTarget, pane_id: &str, explorer: bool, value: &str) {
    let keys: Vec<String> = if explorer {
        shell.data.layout.explorer_tabs.clone()
    } else {
        shell.data.layout.tree.pane(pane_id).map(|p| p.tabs.clone()).unwrap_or_default()
    };
    let key = target.key();
    let index = keys.iter().position(|k| *k == key).unwrap_or(0);
    if let Some(action) = value.strip_prefix("conversation:") {
        if let Some(panel) = shell.data.panels.get(&key) {
            panels::run_conversation_action(cx, panel, action);
        }
        return;
    }
    match value {
        "resume" => copy_resume_command(shell, cx, ui, &target),
        "agentId" => copy(shell, cx, target.id.as_deref().unwrap_or(""), "workspace.tabs.toasts.agentIdCopiedLabel"),
        "path" => {
            let path = j::text_or(&target.data, "path", target.id.as_deref().unwrap_or("")).to_string();
            copy(shell, cx, &path, "workspace.tabs.toasts.filePathCopiedLabel");
        }
        "rename" => rename_tab(shell, cx, ui, target),
        "left" => close_bulk(shell, cx, keys[..index].to_vec(), "workspace.tabs.confirmations.closeTabsLeftTitle", AfterClose::Nothing),
        "right" => close_bulk(shell, cx, keys[index + 1..].to_vec(), "workspace.tabs.confirmations.closeTabsRightTitle", AfterClose::Nothing),
        "others" => close_bulk(
            shell,
            cx,
            keys.into_iter().filter(|k| *k != key).collect(),
            "workspace.tabs.confirmations.closeOtherTabsTitle",
            AfterClose::Nothing,
        ),
        "splitRight" | "splitDown" => {
            let axis = if value == "splitRight" { Axis::Horizontal } else { Axis::Vertical };
            if let Some(next) = shell.data.layout.split_pane(pane_id, axis) {
                shell.data.layout.move_tab(&key, &next, None);
            }
        }
        "maximize" => {
            let maximized = shell.data.layout.maximized_pane.as_deref() == Some(pane_id);
            shell.data.layout.maximize_pane(if maximized { None } else { Some(pane_id.to_string()) });
        }
        "closePane" => close_pane(shell, cx, pane_id),
        "reload" => reload_agent(shell, cx, ui, &target),
        "close" => close_tabs(shell, cx, ui, vec![key], true, AfterClose::Nothing),
        _ => {}
    }
}

/// Original `provider-command-templates.ts`.
fn resume_template(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "codex" => "codex resume {sessionId}",
        "claude" => "claude --resume {sessionId}",
        "hermes" => "hermes --resume {sessionId}",
        "pi" => "pi --session {sessionId}",
        "omp" => "omp --session {sessionId}",
        "opencode" => "opencode --session {sessionId}",
        _ => return None,
    })
}

fn copy_resume_command(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, target: &PanelTarget) {
    let agent = hosts::host(&ui.state, &target.server_id).and_then(|h| h.agent(target.id.as_deref().unwrap_or("")).cloned());
    let session = agent
        .as_ref()
        .map(|a| {
            let runtime = j::text(j::object(a, "runtimeInfo"), "sessionId");
            if runtime.is_empty() { j::text(j::object(a, "persistence"), "sessionId").to_string() } else { runtime.to_string() }
        })
        .unwrap_or_default();
    if session.is_empty() {
        return toast(shell, cx, tr("workspace.tabs.toasts.resumeIdUnavailable"), true);
    }
    let Some(template) = agent.as_ref().and_then(|a| resume_template(j::text(a, "provider"))) else {
        return toast(shell, cx, tr("workspace.tabs.toasts.resumeCommandUnavailable"), true);
    };
    copy(shell, cx, &template.replace("{sessionId}", &session), "workspace.tabs.toasts.resumeCommandCopiedLabel");
}

/// `_reloadAgent`.
fn reload_agent(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, target: &PanelTarget) {
    toast(shell, cx, tr("workspace.tabs.toasts.reloadingAgent"), false);
    request(
        shell,
        ui,
        &target.server_id,
        json!({ "type": "refresh_agent_request", "agentId": target.id }),
        "status",
        Pending::ReloadAgent { tab_key: target.key() },
    );
}

/// `_renameTab`.
fn rename_tab(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, target: PanelTarget) {
    let title = tr(if target.kind == "agent" { "workspace.tabs.menu.renameAgent" } else { "workspace.tabs.menu.rename" });
    let current = chrome::title(&ui.state, &target);
    prompt(shell, cx, &title, &current, ShellOverlay::RenameTab { target });
}

/// Compact switcher row menu (`MobileTabTrailingAccessory`).
pub fn workspace_tab_menu(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, tab: PanelTarget, workspace: PanelTarget, anchor: Rect) {
    let tabs = workspace_tabs(shell, state, &workspace);
    let index = tabs.iter().position(|t| t.key() == tab.key());
    let agent = tab.kind == "agent";
    let id = tab.id.clone().unwrap_or_default();
    let mut entries = Vec::new();
    if agent {
        entries.push(item("resume", tr("workspace.tabs.menu.copyResumeCommand"), "copy"));
        entries.push(MenuItem::new("agentId", tr("workspace.tabs.menu.copyAgentId")).icon("copy").hint(id.chars().take(7).collect::<String>()).entry());
    }
    if tab.kind == "file" {
        entries.push(item("path", tr("workspace.tabs.menu.copyFilePath"), "copy"));
    }
    if agent {
        entries.push(item("rename", tr("workspace.tabs.menu.rename"), "pencil"));
        entries.push(MenuEntry::Separator);
    }
    entries.push(MenuItem::new("above", tr("workspace.tabs.menu.closeAbove")).icon("arrow-left-to-line").enabled(index.is_some_and(|i| i > 0)).entry());
    entries.push(
        MenuItem::new("below", tr("workspace.tabs.menu.closeBelow"))
            .icon("arrow-right-to-line")
            .enabled(index.is_some_and(|i| i + 1 < tabs.len()))
            .entry(),
    );
    entries.push(MenuItem::new("others", tr("workspace.tabs.menu.closeOthers")).icon("copy-x").enabled(tabs.len() > 1).entry());
    if agent {
        entries.push(item("reload", tr("workspace.tabs.menu.reloadAgent"), "rotate-cw"));
    }
    entries.push(item("close", tr("workspace.tabs.menu.close"), "x"));
    let title = chrome::kind_label(&tab);
    menu(shell, cx, anchor, Some(title), entries, MenuOptions { align_end: true, ..MenuOptions::default() }, ShellOverlay::WorkspaceTabMenu { tab, workspace });
}

fn workspace_tab_menu_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, tab: PanelTarget, workspace: PanelTarget, value: &str) {
    let tabs = workspace_tabs(shell, &ui.state.clone(), &workspace);
    let index = tabs.iter().position(|t| t.key() == tab.key()).unwrap_or(0);
    // Unopened root agents in the switcher have no tab to close.
    let open = |list: Vec<PanelTarget>, shell: &PaseoShell| -> Vec<String> {
        list.into_iter().map(|t| t.key()).filter(|k| shell.data.layout.target(k).is_some()).collect()
    };
    match value {
        "resume" => copy_resume_command(shell, cx, ui, &tab),
        "agentId" => copy(shell, cx, tab.id.as_deref().unwrap_or(""), "workspace.tabs.toasts.agentIdCopiedLabel"),
        "path" => {
            let path = j::text_or(&tab.data, "path", tab.id.as_deref().unwrap_or("")).to_string();
            copy(shell, cx, &path, "workspace.tabs.toasts.filePathCopiedLabel");
        }
        "rename" => rename_tab(shell, cx, ui, tab),
        "above" => {
            let keys = open(tabs[..index].to_vec(), shell);
            close_bulk(shell, cx, keys, "workspace.tabs.confirmations.closeTabsLeftTitle", AfterClose::Nothing);
        }
        "below" => {
            let keys = open(tabs[index + 1..].to_vec(), shell);
            close_bulk(shell, cx, keys, "workspace.tabs.confirmations.closeTabsRightTitle", AfterClose::Nothing);
        }
        "others" => {
            let key = tab.key();
            let keys = open(tabs.into_iter().filter(|t| t.key() != key).collect(), shell);
            close_bulk(shell, cx, keys, "workspace.tabs.confirmations.closeOtherTabsTitle", AfterClose::Nothing);
        }
        "reload" => reload_agent(shell, cx, ui, &tab),
        "close" => close_tabs(shell, cx, ui, vec![tab.key()], true, AfterClose::Nothing),
        _ => {}
    }
}

/// Original explorer "+" (`SUPPORTING_LAUNCH_ORDER` minus browser).
fn explorer_new_tab(shell: &mut PaseoShell, cx: &mut Cx, anchor: Rect) {
    let (keys, _) = chrome::explorer_keys(shell);
    let kinds: Vec<String> = keys.iter().filter_map(|k| shell.data.layout.target(k)).map(|t| t.kind.clone()).collect();
    let mut entries = Vec::new();
    if !kinds.iter().any(|k| k == "changes_tree") {
        entries.push(item("changes_tree", tr("panels.diff.changesLabel"), crate::common::icons::for_tab_kind("changes_tree")));
    }
    entries.push(item("terminal", tr("workspace.tabs.fallback.terminal"), crate::common::icons::for_tab_kind("terminal")));
    if !shell.data.compact {
        entries.push(item("working_diff", tr("panels.diff.diffLabel"), crate::common::icons::for_tab_kind("working_diff")));
    }
    if !kinds.iter().any(|k| k == "files") {
        entries.push(item("files", tr("panels.files.label"), crate::common::icons::for_tab_kind("files")));
    }
    entries.push(item("new_tab", tr("workspace.tabs.fallback.agent"), "square-pen"));
    entries.push(item("pull_request", tr("panels.pullRequest.label"), crate::common::icons::for_tab_kind("pull_request")));
    menu(shell, cx, anchor, Some(tr("workspace.tabs.actions.newTab")), entries, MenuOptions::default(), ShellOverlay::ExplorerNewTab);
}

fn explorer_new_tab_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, value: &str) {
    if value == "terminal" {
        new_terminal(shell, cx, ui, TabPlacement::Explorer);
        return;
    }
    if value != "new_tab" {
        open_workspace_panel(shell, cx, ui, value, true);
        return;
    }
    let Some(target) = selected(shell).filter(|t| !t.workspace_id.is_empty()) else {
        return toast(shell, cx, "Open a session or workspace first.", true);
    };
    let draft = PanelTarget::new("new_tab", &target.server_id, &target.workspace_id, &target.cwd)
        .with_id(new_id())
        .with_title(tr("sessionNavigation.newSession"));
    open_tab(shell, cx, ui, draft, TabPlacement::Explorer);
}

/// `workspace.terminal.new` (`createTerminal` of `useWorkspaceTerminals`): a default shell in
/// the focused workspace, opened as its terminal tab once the daemon created it.
fn new_terminal(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, placement: TabPlacement) {
    let Some(target) = selected(shell).filter(|t| !t.workspace_id.is_empty()) else {
        return toast(shell, cx, "Open a session or workspace first.", true);
    };
    let Some(host) = hosts::host(&ui.state, &target.server_id) else { return };
    if !host.connected() {
        return toast(shell, cx, tr("workspace.terminal.hostDisconnected"), true);
    }
    let cwd = host
        .workspace(&target.workspace_id)
        .map(|w| j::text(w, "workspaceDirectory"))
        .filter(|dir| !dir.is_empty())
        .unwrap_or(target.cwd.as_str())
        .to_string();
    let message = json!({ "type": "create_terminal_request", "cwd": cwd, "workspaceId": target.workspace_id });
    let pending = Pending::CreateTerminal { host_id: target.server_id.clone(), workspace_id: target.workspace_id.clone(), cwd, placement };
    request(shell, ui, &target.server_id, message, "create_terminal_response", pending);
}

/// `WorkspaceHeaderMenu` (copy path/branch, import session, show setup; compact scripts first).
fn header_menu(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, anchor: Rect) {
    let target = selected(shell);
    let workspace = target.as_ref().and_then(|t| workspace_value(state, &t.server_id, &t.workspace_id));
    let directory = workspace
        .as_ref()
        .map(|w| j::text_or(w, "workspaceDirectory", target.as_ref().map_or("", |t| t.cwd.as_str())).to_string())
        .unwrap_or_else(|| target.as_ref().map(|t| t.cwd.clone()).unwrap_or_default());
    let branch = workspace.as_ref().map(|w| j::text(j::object(w, "gitRuntime"), "currentBranch").trim().to_string()).unwrap_or_default();
    let has_branch = !branch.is_empty() && branch != "HEAD";
    let show_setup = workspace.as_ref().is_some_and(|w| {
        !j::is_null_or_missing(w, "setupStatus")
            || !target.as_ref().and_then(|t| hosts::host(state, &t.server_id)).is_some_and(|h| h.supports("workspaceSetupObservation"))
    });
    let mut entries = Vec::new();
    if shell.data.compact && workspace.as_ref().is_some_and(|w| !j::array(w, "scripts").is_empty()) {
        entries.push(item("scripts", tr("workspace.scripts.title"), "play"));
    }
    entries.push(MenuItem::new("copyPath", tr("workspace.header.actions.copyPath")).icon("copy").enabled(!directory.is_empty()).entry());
    if has_branch {
        entries.push(item("copyBranch", tr("workspace.header.actions.copyBranchName"), "copy"));
    }
    entries.push(MenuItem::new("import", tr("workspace.header.actions.importSession")).icon("import").enabled(target.is_some()).entry());
    if show_setup {
        entries.push(MenuEntry::Separator);
        entries.push(item("setup", tr("workspace.header.actions.showSetup"), "settings"));
    }
    menu(shell, cx, anchor, Some(tr("workspace.header.actions.workspaceActions")), entries, MenuOptions::default(), ShellOverlay::HeaderMenu { target });
}

fn header_menu_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, target: Option<PanelTarget>, value: &str) {
    let state = ui.state.clone();
    let workspace = target.as_ref().and_then(|t| workspace_value(&state, &t.server_id, &t.workspace_id));
    let directory = workspace
        .as_ref()
        .map(|w| j::text_or(w, "workspaceDirectory", target.as_ref().map_or("", |t| t.cwd.as_str())).to_string())
        .unwrap_or_else(|| target.as_ref().map(|t| t.cwd.clone()).unwrap_or_default());
    match value {
        "copyPath" => copy(shell, cx, &directory, "workspace.header.toasts.workspacePathCopiedLabel"),
        "copyBranch" => {
            let branch = workspace.as_ref().map(|w| j::text(j::object(w, "gitRuntime"), "currentBranch").trim().to_string()).unwrap_or_default();
            copy(shell, cx, &branch, "workspace.header.toasts.branchNameCopiedLabel");
        }
        "import" => {
            if let Some(target) = target {
                shell.data.overlays.open_sheet(Box::new(history::ImportSheet::new(&target.server_id, &directory)));
            }
        }
        "setup" | "scripts" => {
            if let (Some(target), Some(workspace)) = (target, workspace) {
                let root = j::text_or(&workspace, "projectRootPath", &directory).to_string();
                let next = PanelTarget::new("setup", &target.server_id, &target.workspace_id, &directory).with_data(json!({ "repoRoot": root }));
                open_tab(shell, cx, ui, next, TabPlacement::Main);
            }
        }
        _ => {}
    }
}

fn script_url(script: &Value) -> String {
    for key in ["proxyUrl", "publicProxyUrl", "localProxyUrl"] {
        let url = j::text(script, key);
        if !url.is_empty() {
            return url.to_string();
        }
    }
    String::new()
}

/// Original desktop `WorkspaceScriptsButton` menu.
fn scripts_menu(shell: &mut PaseoShell, cx: &mut Cx, target: PanelTarget, workspace: Value, anchor: Rect) {
    let mut entries = Vec::new();
    for script in j::array(&workspace, "scripts") {
        let name = j::text(script, "scriptName");
        let lifecycle = j::text(script, "lifecycle");
        let mut hint = Vec::new();
        if !lifecycle.is_empty() {
            hint.push(lifecycle.to_string());
        }
        if let Some(code) = script.get("exitCode").filter(|v| !v.is_null()) {
            hint.push(tr_args("workspace.scripts.states.exitCode", &[("code", &code.to_string())]));
        }
        entries.push(
            MenuItem::new(format!("toggle:{name}"), name)
                .icon(if lifecycle == "running" { "square" } else { "play" })
                .hint(hint.join(" · "))
                .entry(),
        );
        if !j::text(script, "terminalId").is_empty() {
            entries.push(MenuItem::new(format!("log:{name}"), tr("workspace.scripts.actions.view")).icon("square-terminal").muted(true).entry());
        }
        if !script_url(script).is_empty() {
            entries.push(MenuItem::new(format!("open:{name}"), tr("workspace.scripts.actions.openService")).icon("external-link").muted(true).entry());
        }
    }
    entries.push(MenuEntry::Separator);
    entries.push(item("setup", tr("workspace.header.actions.showSetup"), "settings"));
    menu(
        shell,
        cx,
        anchor,
        Some(tr("workspace.scripts.title")),
        entries,
        MenuOptions { align_end: true, ..MenuOptions::default() },
        ShellOverlay::ScriptsMenu { target, workspace },
    );
}

fn scripts_menu_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, target: PanelTarget, workspace: Value, value: &str) {
    if value == "setup" {
        let root = j::text_or(&workspace, "projectRootPath", &target.cwd).to_string();
        let next = PanelTarget::new("setup", &target.server_id, &target.workspace_id, &target.cwd).with_data(json!({ "repoRoot": root }));
        open_tab(shell, cx, ui, next, TabPlacement::Main);
        return;
    }
    let Some((verb, name)) = value.split_once(':') else { return };
    let Some(script) = j::array(&workspace, "scripts").iter().find(|s| j::text(s, "scriptName") == name).cloned() else { return };
    match verb {
        "log" => {
            let next = PanelTarget::new("execution_log", &target.server_id, &target.workspace_id, &target.cwd)
                .with_id(j::text(&script, "terminalId"))
                .with_title(name);
            open_tab(shell, cx, ui, next, TabPlacement::Main);
        }
        "open" => {
            let url = script_url(&script);
            let valid = url.starts_with("http://") || url.starts_with("https://");
            if !valid || platform::open_external(&url).is_none() {
                toast(shell, cx, "Could not open service URL in the system browser.", true);
            }
        }
        _ => {
            let command = if j::text(&script, "lifecycle") == "running" { "stop" } else { "start" };
            request(
                shell,
                ui,
                &target.server_id,
                json!({ "type": format!("workspace.script.{command}.request"), "workspaceId": target.workspace_id, "scriptName": name }),
                &format!("workspace.script.{command}.response"),
                Pending::Plain,
            );
        }
    }
}

/// Original `SidebarHelpMenu`.
fn help_menu(shell: &mut PaseoShell, cx: &mut Cx, anchor: Rect) {
    let mut entries = vec![MenuEntry::Label(tr("sidebar.help.sectionHelp"))];
    if platform::desktop_host() {
        entries.push(item("shortcuts", tr("sidebar.help.shortcuts"), "keyboard"));
    }
    entries.push(item("diagnostics", tr("sidebar.help.diagnostics"), "activity"));
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::Label(tr("sidebar.help.reportIssue")));
    entries.push(item("discord", tr("sidebar.help.discord"), "message-circle"));
    entries.push(item("github", tr("sidebar.help.github"), "bug"));
    entries.push(MenuEntry::Separator);
    entries.push(MenuItem::new("version", tr("sidebar.help.appName")).hint(crate::app::app_version()).enabled(false).entry());
    menu(shell, cx, anchor, None, entries, MenuOptions { width: 280.0, align_end: true, sheet: false }, ShellOverlay::HelpMenu);
}

fn help_menu_result(shell: &mut PaseoShell, cx: &mut Cx, value: &str) {
    match value {
        "shortcuts" => {
            shell.data.overlays.open_sheet(Box::new(shortcut_help::ShortcutHelpSheet::new(shell.data.compact)));
        }
        "diagnostics" => open_settings(shell, cx, None, "diagnostics"),
        "discord" => {
            platform::open_external("https://discord.gg/jz8T2uahpH");
        }
        "github" => {
            platform::open_external("https://github.com/getpaseo/paseo/issues/new");
        }
        _ => {}
    }
}

/// Original sidebar `HostPicker`: hosts (status dot, active connection), "Add host".
fn host_picker(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, anchor: Rect) {
    let all = hosts::hosts(state);
    if all.len() > 10 {
        // Searchable above ten hosts.
        let ids = all.iter().map(|h| h.server_id().to_string()).collect();
        shell.data.overlays.open_sheet(Box::new(host_connections::ChooserSheet::picker(ids)));
        return;
    }
    let p = crate::theme::current_palette();
    let mut entries: Vec<MenuEntry> = all
        .iter()
        .map(|host| {
            MenuItem::new(host.server_id(), host.display_label())
                .dot(sidebar::status_color(&p, host))
                .hint(host_connections::active_connection_label(host.config()))
                .entry()
        })
        .collect();
    entries.push(item("add", "Add host", "plus"));
    menu(shell, cx, anchor, Some("Host".into()), entries, MenuOptions { width: 280.0, ..MenuOptions::default() }, ShellOverlay::HostPicker);
}

/// `_openHost`: add host, or that host's settings.
pub fn open_host(shell: &mut PaseoShell, cx: &mut Cx, value: &str) {
    if value == "add" {
        host_dialog::open_flow(shell, cx, None, None, None);
        return;
    }
    let id = value.strip_prefix("settings:").unwrap_or(value).to_string();
    open_settings(shell, cx, Some(id), "host/host");
}

fn host_menu(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, host_id: &str, anchor: Rect) {
    let order = sidebar_host_order(state);
    let entries = [
        ("new", "sessionNavigation.newSession"),
        ("project", "sidebar.actions.addProject"),
        ("history", "sessions.title"),
        ("import", "importSession.title"),
        ("refresh", "sidebar.host.actions.reconnect"),
        ("edit", "sidebar.host.actions.edit"),
        ("connections", "settings.hostSections.connections"),
        ("pair", "settings.host.pairDevices.rowTitle"),
        ("up", "settings.appearance.sidebar.moveUp"),
        ("down", "settings.appearance.sidebar.moveDown"),
        ("remove", "settings.host.daemon.remove.title"),
    ]
    .iter()
    .map(|(value, key)| MenuItem::new(*value, tr(key)).entry())
    .collect();
    menu(shell, cx, anchor, None, entries, MenuOptions::default(), ShellOverlay::HostMenu { host_id: host_id.to_string(), order });
}

fn sidebar_host_order(state: &Value) -> Vec<String> {
    let prefs = sidebar::nav_prefs(state);
    let stored: Vec<String> = j::array(&prefs, "hostOrder").iter().filter_map(Value::as_str).map(str::to_string).collect();
    let mut ids: Vec<String> = hosts::hosts(state).iter().map(|h| h.server_id().to_string()).collect();
    ids.sort_by_key(|id| stored.iter().position(|s| s == id).unwrap_or(stored.len()));
    ids
}

fn host_action(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, action: &str, order: &[String]) {
    let state = ui.state.clone();
    let Some(host) = hosts::host(&state, host_id) else { return };
    match action {
        "new" => {
            let target = PanelTarget::new("new_tab", host_id, "", "").with_title("New session");
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        "project" => host_chosen(shell, cx, ui, HostPurpose::AddProject, host_id.to_string()),
        "history" => show_sessions(shell, cx, host_id, false, "", ""),
        "import" => {
            shell.data.overlays.open_sheet(Box::new(history::ImportSheet::new(host_id, "")));
        }
        "refresh" => ui.core.refresh_host(host_id),
        "edit" => host_dialog::open_flow(shell, cx, None, Some(host.config().clone()), None),
        "connections" => {
            shell.data.overlays.open_sheet(Box::new(host_connections::ConnectionsSheet::new(host_id.to_string())));
        }
        "pair" => {
            shell.data.overlays.open_sheet(Box::new(host_dialog::PairingSheet::new(host_id.to_string())));
        }
        "up" => sidebar::move_in_order(ui, "hostOrder", order, host_id, -1),
        "down" => sidebar::move_in_order(ui, "hostOrder", order, host_id, 1),
        "remove" => confirm(
            shell,
            cx,
            &format!("Remove {}?", host.label()),
            "Remove this saved host, all its connections and credentials from this device? Sessions on the daemon are not deleted.",
            "Remove",
            true,
            ShellOverlay::RemoveHost { host_id: host_id.to_string() },
        ),
        _ => {}
    }
    shell.redraw(cx);
}

fn project_menu(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, host_id: &str, project: Value, anchor: Rect) {
    let Some(host) = hosts::host(state, host_id) else { return };
    let stored: Vec<String> = j::array(&sidebar::nav_prefs(state), "projectOrder").iter().filter_map(Value::as_str).map(str::to_string).collect();
    let order: Vec<String> = sidebar::ordered_projects(&host, &stored).iter().map(|p| sidebar::project_key(&host, p)).collect();
    let entries = [
        ("new", "sessionNavigation.newSession"),
        ("history", "sessions.title"),
        ("import", "importSession.title"),
        ("rename", "sidebar.project.actions.rename"),
        ("reset", "sidebar.project.actions.resetName"),
        ("icon", "sidebar.project.actions.chooseIcon"),
        ("resetIcon", "sidebar.project.actions.resetIcon"),
        ("settings", "sidebar.project.actions.openSettings"),
        ("archive", "sidebar.project.actions.archiveAll"),
        ("up", "settings.appearance.sidebar.moveUp"),
        ("down", "settings.appearance.sidebar.moveDown"),
        ("remove", "sidebar.project.actions.remove"),
    ]
    .iter()
    .map(|(value, key)| MenuItem::new(*value, tr(key)).entry())
    .collect();
    menu(shell, cx, anchor, None, entries, MenuOptions::default(), ShellOverlay::ProjectMenu { host_id: host_id.to_string(), project, order });
}

fn project_history_key(state: &Value, host_id: &str, project: &Value) -> String {
    let key = j::text(project, "projectKey");
    if !key.is_empty() {
        return key.to_string();
    }
    let id = j::text(project, "projectId");
    hosts::host(state, host_id)
        .and_then(|h| {
            h.workspaces()
                .iter()
                .filter(|w| j::text(w, "projectId") == id)
                .map(|w| j::text(j::object(w, "project"), "projectKey"))
                .find(|k| !k.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn project_action(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, project: &Value, action: &str, order: &[String]) {
    let state = ui.state.clone();
    let project_id = j::text(project, "projectId").to_string();
    let root = j::text(project, "projectRootPath").to_string();
    let Some(host) = hosts::host(&state, host_id) else { return };
    let key = sidebar::project_key(&host, project);
    match action {
        "new" => {
            let target = PanelTarget::new("new_tab", host_id, "", &root).with_title("New session").with_data(json!({ "project": project }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        "history" => {
            let project_key = project_history_key(&state, host_id, project);
            show_sessions(shell, cx, host_id, false, &root, &project_key);
        }
        "import" => {
            shell.data.overlays.open_sheet(Box::new(history::ImportSheet::new(host_id, &root)));
        }
        "settings" => {
            let name = j::text(project, "projectDisplayName");
            let target = PanelTarget::new("project_settings", host_id, "", &root).with_title(format!("{name} settings")).with_data(json!({ "project": project }));
            open_tab(shell, cx, ui, target, TabPlacement::Main);
        }
        "up" => sidebar::move_in_order(ui, "projectOrder", order, &key, -1),
        "down" => sidebar::move_in_order(ui, "projectOrder", order, &key, 1),
        "icon" => {
            if let Some(request) = platform::pick_image_file() {
                ICON_PICKS.with(|picks| {
                    picks.borrow_mut().insert(request, IconPick { window: shell.data.window_id.clone(), host_id: host_id.to_string(), project_id });
                });
            }
        }
        "resetIcon" => request(
            shell,
            ui,
            host_id,
            json!({ "type": "project.icon.set.request", "projectId": project_id, "source": { "type": "automatic" } }),
            "project.icon.set.response",
            Pending::Refresh { host_id: host_id.to_string() },
        ),
        "rename" => prompt(
            shell,
            cx,
            "Rename project",
            j::text(project, "projectDisplayName"),
            ShellOverlay::RenameProject { host_id: host_id.to_string(), project_id },
        ),
        "reset" => request(
            shell,
            ui,
            host_id,
            json!({ "type": "project.rename.request", "projectId": project_id, "customName": null }),
            "project.rename.response",
            Pending::Refresh { host_id: host_id.to_string() },
        ),
        "archive" => {
            let workspaces: VecDeque<Value> = host.workspaces().iter().filter(|w| j::text(w, "projectId") == project_id).cloned().collect();
            archive_workspaces(shell, cx, ui, host_id, workspaces, Vec::new());
        }
        "remove" => {
            let count = host.workspaces().iter().filter(|w| j::text(w, "projectId") == project_id).count();
            confirm(
                shell,
                cx,
                &format!("Remove project from {}?", host.label()),
                &format!("{root}\n{count} active workspaces are affected on this host. Other hosts are unchanged."),
                "Remove",
                true,
                ShellOverlay::RemoveProject { host_id: host_id.to_string(), project: project.clone() },
            );
        }
        _ => {}
    }
}

fn session_menu(shell: &mut PaseoShell, cx: &mut Cx, state: &Value, host_id: &str, agent: Value, anchor: Rect) {
    let pinned = workspace_value(state, host_id, j::text(&agent, "workspaceId")).is_some_and(|w| !j::is_null_or_missing(&w, "pinnedAt"));
    let entries = vec![
        MenuItem::new("rename", tr("workspace.tabs.menu.renameAgent")).entry(),
        MenuItem::new("archive", tr("agentList.archiveSheet.archive")).entry(),
        MenuItem::new("labels", tr("workspaceLabels.manage.open")).entry(),
        MenuItem::new("pin", tr(if pinned { "sidebar.workspace.actions.unpin" } else { "sidebar.workspace.actions.pin" })).entry(),
        MenuItem::new("unread", tr("sidebar.workspace.actions.markUnread")).entry(),
        MenuItem::new("recover", tr("sidebar.workspace.actions.recover")).entry(),
        MenuItem::new("archiveWorkspace", tr("workspace.git.actions.archive.label")).entry(),
    ];
    menu(shell, cx, anchor, None, entries, MenuOptions::default(), ShellOverlay::SessionMenu { host_id: host_id.to_string(), agent });
}

fn session_action(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, host_id: &str, agent: &Value, action: &str) {
    let state = ui.state.clone();
    let agent_id = j::text(agent, "id").to_string();
    let workspace_id = j::text(agent, "workspaceId").to_string();
    let workspace = workspace_value(&state, host_id, &workspace_id);
    let title = {
        let t = j::text(agent, "title");
        if t.is_empty() { tr("sessionNavigation.untitled") } else { t.to_string() }
    };
    let refresh = Pending::Refresh { host_id: host_id.to_string() };
    match action {
        "labels" => match workspace {
            Some(workspace) => {
                shell.data.overlays.open_sheet(Box::new(labels::LabelsSheet::new(host_id.to_string(), Some(workspace))));
            }
            None => toast(shell, cx, "The workspace is unavailable. Refresh this host.", true),
        },
        "recover" => recover_workspace(shell, ui, host_id, &workspace_id),
        "archiveWorkspace" => {
            let list: VecDeque<Value> = workspace.into_iter().collect();
            archive_workspaces(shell, cx, ui, host_id, list, Vec::new());
        }
        "pin" => {
            let pinned = workspace.as_ref().is_none_or(|w| j::is_null_or_missing(w, "pinnedAt"));
            request(
                shell,
                ui,
                host_id,
                json!({ "type": "workspace.pin.set.request", "workspaceId": workspace_id, "pinned": pinned }),
                "workspace.pin.set.response",
                refresh,
            );
        }
        "unread" => request(
            shell,
            ui,
            host_id,
            json!({ "type": "workspace.mark_unread.request", "workspaceId": workspace_id }),
            "workspace.mark_unread.response",
            refresh,
        ),
        "rename" => prompt(shell, cx, "Rename session", &title, ShellOverlay::RenameSession { host_id: host_id.to_string(), agent_id }),
        "archive" => confirm(shell, cx, "Archive session?", &title, "Archive", false, ShellOverlay::ArchiveSession { host_id: host_id.to_string(), agent_id }),
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Sidebar display preferences (`SidebarDisplayPreferencesMenu`)

fn display_entries(shell: &PaseoShell, state: &Value) -> Vec<MenuEntry> {
    let prefs = sidebar::nav_prefs(state);
    let all = hosts::hosts(state);
    let p = crate::theme::current_palette();
    let host_filters = sidebar::host_filters(state, &prefs);
    let project_filters = sidebar::project_filters(state, &prefs);
    let label_filters = sidebar::label_filters(&prefs);
    let mut labels: Vec<(String, String, String)> = shell.data.label_catalog.clone();
    for host in &all {
        for workspace in host.workspaces() {
            for raw in j::array(workspace, "labels").iter().filter_map(Value::as_str) {
                let key = sidebar::label_key(raw);
                if !key.is_empty() && !labels.iter().any(|(k, _, _)| *k == key) {
                    labels.push((key, raw.trim().to_string(), String::new()));
                }
            }
        }
    }
    let project_count: usize = all.iter().map(|h| sidebar::projects(h).len()).sum();
    let mut entries = Vec::new();
    if all.len() > 1 {
        let mut page = vec![MenuItem::new("host:all", tr("sidebar.display.hostFilter.all")).checked(j::array(&prefs, "hostFilters").is_empty()).keep_open().entry()];
        for host in &all {
            page.push(
                MenuItem::new(format!("host:{}", host.server_id()), host.display_label())
                    .dot(sidebar::status_color(&p, host))
                    .checked(host_filters.iter().any(|h| h == host.server_id()))
                    .keep_open()
                    .entry(),
            );
        }
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Submenu {
            label: tr("sidebar.display.hostFilter.label"),
            icon: None,
            value: None,
            indicator: !j::array(&prefs, "hostFilters").is_empty(),
            entries: page,
        });
    }
    if project_count > 1 {
        let mut page = vec![MenuItem::new("project:all", tr("sidebar.display.projectFilter.all")).checked(project_filters.is_empty()).keep_open().entry()];
        for host in &all {
            for project in sidebar::projects(host) {
                let key = sidebar::project_key(host, &project);
                let name = j::text_or(&project, "projectDisplayName", j::text(&project, "projectRootPath")).to_string();
                page.push(MenuItem::new(format!("project:{key}"), name).icon("folder").checked(project_filters.contains(&key)).keep_open().entry());
            }
        }
        if all.len() <= 1 {
            entries.push(MenuEntry::Separator);
        }
        entries.push(MenuEntry::Submenu {
            label: tr("sidebar.display.projectFilter.label"),
            icon: None,
            value: None,
            indicator: !project_filters.is_empty(),
            entries: page,
        });
    }
    if !labels.is_empty() || !label_filters.is_empty() {
        let mut page: Vec<MenuEntry> = labels
            .iter()
            .map(|(key, name, color)| {
                let mut entry = MenuItem::new(format!("label:{key}"), name.clone()).checked(label_filters.contains(key)).keep_open();
                if color.is_empty() {
                    entry = entry.icon("circle");
                } else {
                    entry = entry.dot(labels::label_color(&p, color));
                }
                entry.entry()
            })
            .collect();
        page.push(
            MenuItem::new("label:", tr("workspaceLabels.unlabelled"))
                .icon("circle")
                .checked(label_filters.iter().any(String::is_empty))
                .keep_open()
                .entry(),
        );
        if !label_filters.is_empty() {
            page.push(MenuEntry::Separator);
            page.push(MenuItem::new("labels:clear", tr("workspaceLabels.filter.clear")).keep_open().entry());
        }
        page.push(MenuEntry::Separator);
        page.push(MenuItem::new("labels:manage", tr("workspaceLabels.manage.open")).entry());
        entries.push(MenuEntry::Separator);
        entries.push(MenuEntry::Submenu { label: tr("workspaceLabels.title"), icon: None, value: None, indicator: !label_filters.is_empty(), entries: page });
    }
    entries.push(MenuEntry::Separator);
    entries.push(
        MenuItem::new("background", tr("sidebar.display.showBackground"))
            .checked(j::boolean(&prefs, "showBackground", false))
            .keep_open()
            .entry(),
    );
    entries
}

fn display_menu(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext, anchor: Rect) {
    shell.data.label_catalog.clear();
    for host in hosts::hosts(&ui.state) {
        if host.connected() {
            // A host without the label catalog contributes only its workspace labels.
            let id = ui.core.request(host.server_id(), json!({ "type": "workspace.label.list.request" }), "workspace.label.list.response");
            shell.data.pending.insert(id, Pending::LabelCatalog);
        }
    }
    let entries = display_entries(shell, &ui.state);
    let id = menu(
        shell,
        cx,
        anchor,
        Some(tr("sidebar.display.heading")),
        entries,
        MenuOptions { width: 232.0, align_end: true, sheet: true },
        ShellOverlay::DisplayMenu,
    );
    shell.data.display_menu = Some(id);
}

fn refresh_display_menu(shell: &mut PaseoShell, cx: &mut Cx, ui: &UiContext) {
    if let Some(id) = shell.data.display_menu {
        let entries = display_entries(shell, &ui.state);
        shell.data.overlays.update_menu(id, entries);
        shell.redraw(cx);
    }
}

fn display_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, value: &str) {
    let mut prefs = sidebar::nav_prefs(&ui.state);
    let toggle = |prefs: &Value, field: &str, item: &str| -> Value {
        let mut values: Vec<String> = j::array(prefs, field).iter().map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).collect();
        match values.iter().position(|v| v == item) {
            Some(i) => {
                values.remove(i);
            }
            None => values.push(item.to_string()),
        }
        json!(values)
    };
    if value == "host:all" {
        sidebar::set_nav(ui, &mut prefs, "hostFilters", json!([]));
    } else if let Some(host) = value.strip_prefix("host:") {
        let next = toggle(&prefs, "hostFilters", host);
        sidebar::set_nav(ui, &mut prefs, "hostFilters", next);
    } else if value == "project:all" {
        sidebar::set_nav(ui, &mut prefs, "projectFilters", json!([]));
    } else if let Some(key) = value.strip_prefix("project:") {
        let next = toggle(&prefs, "projectFilters", key);
        sidebar::set_nav(ui, &mut prefs, "projectFilters", next);
    } else if value == "labels:clear" {
        sidebar::set_nav(ui, &mut prefs, "labelFilters", json!([]));
    } else if value == "labels:manage" {
        shell.data.display_menu = None;
        choose_any_host(shell, cx, ui, HostPurpose::ManageLabels, Some(true));
        return;
    } else if let Some(key) = value.strip_prefix("label:") {
        let mut values = sidebar::label_filters(&prefs);
        match values.iter().position(|v| v == key) {
            Some(i) => {
                values.remove(i);
            }
            None => values.push(key.to_string()),
        }
        sidebar::set_nav(ui, &mut prefs, "labelFilters", json!(values));
    } else if value == "background" {
        let show = !j::boolean(&prefs, "showBackground", false);
        sidebar::set_nav(ui, &mut prefs, "showBackground", json!(show));
    }
    // `set_nav` queued the preference effect; apply it now so the menu rebuilds from it.
    let effects = std::mem::take(&mut ui.effects);
    for effect in effects {
        apply_effect(shell, cx, ui, effect);
    }
}

// ---------------------------------------------------------------------------------------------
// Overlay answers

pub fn shell_overlay_result(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: u64, args: &Value) {
    let Some(overlay) = shell.data.menus.remove(&id) else { return };
    let cancelled = j::boolean(args, "cancelled", false);
    let value = j::text(args, "value").to_string();
    let confirmed = j::boolean(args, "confirmed", false);
    let text = j::text(args, "text").trim().to_string();
    let state = ui.state.clone();
    match overlay {
        ShellOverlay::DisplayMenu => {
            if cancelled {
                shell.data.display_menu = None;
            } else {
                shell.data.menus.insert(id, ShellOverlay::DisplayMenu);
                display_result(shell, cx, ui, &value);
            }
        }
        _ if cancelled => match &overlay {
            ShellOverlay::Guard { key, file } => {
                if *file {
                    panels::file_close_cancelled(key);
                }
                abort_closing(shell);
            }
            ShellOverlay::CloseTerminal { .. } => abort_closing(shell),
            _ => {}
        },
        ShellOverlay::TabMenu { target, pane_id, explorer } => tab_menu_result(shell, cx, ui, target, &pane_id, explorer, &value),
        ShellOverlay::WorkspaceTabMenu { tab, workspace } => workspace_tab_menu_result(shell, cx, ui, tab, workspace, &value),
        ShellOverlay::HostMenu { host_id, order } => host_action(shell, cx, ui, &host_id, &value, &order),
        ShellOverlay::ProjectMenu { host_id, project, order } => project_action(shell, cx, ui, &host_id, &project, &value, &order),
        ShellOverlay::SessionMenu { host_id, agent } => session_action(shell, cx, ui, &host_id, &agent, &value),
        ShellOverlay::HeaderMenu { target } => header_menu_result(shell, cx, ui, target, &value),
        ShellOverlay::ScriptsMenu { target, workspace } => scripts_menu_result(shell, cx, ui, target, workspace, &value),
        ShellOverlay::ExplorerNewTab => explorer_new_tab_result(shell, cx, ui, &value),
        ShellOverlay::HelpMenu => help_menu_result(shell, cx, &value),
        ShellOverlay::HostPicker => open_host(shell, cx, &value),
        ShellOverlay::HistoryHost => history::select_host(shell, cx, &value),
        ShellOverlay::Guard { key, file } => {
            if confirmed {
                if file {
                    panels::file_close_confirmed(&key);
                }
                let remove = shell.data.closing.as_ref().is_some_and(|c| c.remove);
                if remove {
                    remove_tab(shell, cx, ui, &key);
                } else {
                    // Window/quit: the user discarded this tab's changes; it stays in the layout.
                    shell.data.dirty.remove(&key);
                }
                advance_closing(shell, cx, ui);
            } else {
                if file {
                    panels::file_close_cancelled(&key);
                }
                abort_closing(shell);
            }
        }
        ShellOverlay::CloseTerminal { key, host_id, terminal_id } => {
            if confirmed {
                close_terminal_confirmed(shell, cx, ui, &key, &host_id, &terminal_id);
            } else {
                abort_closing(shell);
            }
        }
        ShellOverlay::BulkClose { keys, after } => {
            if confirmed {
                close_tabs(shell, cx, ui, keys, true, after);
            }
        }
        ShellOverlay::RemoveHost { host_id } => {
            if confirmed {
                let keys: Vec<String> = shell.data.layout.tabs.iter().filter(|t| t.server_id == host_id).map(PanelTarget::key).collect();
                close_tabs(shell, cx, ui, keys, true, AfterClose::RemoveHost(host_id));
            }
        }
        ShellOverlay::RemoveProject { host_id, project } => {
            if confirmed {
                request(
                    shell,
                    ui,
                    &host_id,
                    json!({ "type": "project.remove.request", "projectId": j::text(&project, "projectId") }),
                    "project.remove.response",
                    Pending::RemoveProject { host_id: host_id.clone() },
                );
            }
        }
        ShellOverlay::ArchiveSession { host_id, agent_id } => {
            if confirmed {
                request(
                    shell,
                    ui,
                    &host_id,
                    json!({ "type": "archive_agent_request", "agentId": agent_id }),
                    "agent_archived",
                    Pending::Refresh { host_id: host_id.clone() },
                );
            }
        }
        ShellOverlay::ArchiveRisk { host_id, current, remaining, confirmed: mut list } => {
            if confirmed {
                list.push(current);
            }
            archive_workspaces(shell, cx, ui, &host_id, remaining.into(), list);
        }
        ShellOverlay::RestoreWorkspace { host_id, workspace_id } => {
            if confirmed {
                let id = ui.core.request_with_timeout(
                    &host_id,
                    json!({ "type": "workspace.recovery.restore.request", "workspaceId": workspace_id }),
                    "workspace.recovery.restore.response",
                    Some(std::time::Duration::from_secs(150)),
                );
                shell.data.pending.insert(id, Pending::Refresh { host_id });
            }
        }
        ShellOverlay::RenameTab { target } => {
            if !text.is_empty() {
                if target.kind == "agent" {
                    request(
                        shell,
                        ui,
                        &target.server_id,
                        json!({ "type": "update_agent_request", "agentId": target.id, "name": text }),
                        "update_agent_response",
                        Pending::Plain,
                    );
                } else {
                    let renamed = target.with_title(text);
                    open_tab(shell, cx, ui, renamed, TabPlacement::Main);
                }
            }
        }
        ShellOverlay::RenameSession { host_id, agent_id } => {
            // `navigationName(maxLength: 200)`.
            let name: String = text.chars().take(200).collect();
            if !name.is_empty() {
                request(
                    shell,
                    ui,
                    &host_id,
                    json!({ "type": "update_agent_request", "agentId": agent_id, "name": name }),
                    "update_agent_response",
                    Pending::Refresh { host_id: host_id.clone() },
                );
            }
        }
        ShellOverlay::RenameProject { host_id, project_id } => {
            if !text.is_empty() {
                request(
                    shell,
                    ui,
                    &host_id,
                    json!({ "type": "project.rename.request", "projectId": project_id, "customName": text }),
                    "project.rename.response",
                    Pending::Refresh { host_id: host_id.clone() },
                );
            }
        }
    }
    let _ = state;
    shell.redraw(cx);
}

// ---------------------------------------------------------------------------------------------
// Platform, window and lifecycle

pub fn handle_platform_actions(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, ui: &mut UiContext) {
    process_routes(shell, cx, ui);
    // Host removals waiting on push revocation: the 2.5 s bound passed.
    if let Some(index) = shell.data.revoking.iter().position(|(_, _, timer)| timer.is_event(event).is_some()) {
        let (_, host_id, _) = shell.data.revoking.remove(index);
        ui.core.remove_host(&host_id);
    }
    let Event::Actions(actions) = event else { return };
    let main = shell.is_main();
    for action in platform::actions(actions) {
        match action {
            PlatformAction::Completed { request, result } => {
                if let Some(index) = shell.data.revoking.iter().position(|(r, _, _)| *r == request) {
                    // Best effort: a failed revocation still removes the host.
                    let (_, host_id, timer) = shell.data.revoking.remove(index);
                    cx.stop_timer(timer);
                    ui.core.remove_host(&host_id);
                    continue;
                }
                if shell.data.close_request == Some(request) {
                    shell.data.close_request = None;
                    match result {
                        Ok(value) => match platform::close_decision(&value) {
                            // The platform already hid every window (on the GUI thread).
                            Some(CloseDecision::HideToTray) => crate::shell::set_tray_hidden(true),
                            Some(CloseDecision::CloseWindow) | None => retire_window(shell, cx),
                            // The platform already started the guarded quit (`QuitRequested`).
                            Some(CloseDecision::QuitApplication) => {}
                        },
                        Err(error) => toast(shell, cx, error.message, true),
                    }
                    continue;
                }
                let pick = ICON_PICKS.with(|picks| picks.borrow_mut().remove(&request));
                if let Some(pick) = pick {
                    if let Ok(PlatformValue::Files(files)) = result {
                        if let Some(file) = files.into_iter().next() {
                            read_icon(pick, file.path);
                        }
                    } else if let Err(error) = result
                        && error.code != "cancelled"
                    {
                        toast(shell, cx, error.message, true);
                    }
                }
            }
            PlatformAction::Event(event) => match event {
                PlatformEvent::Route(route) if main => queue_route(shell, route),
                PlatformEvent::ConnectionLink { url } if main => host_dialog::open_flow(shell, cx, None, None, Some(url)),
                PlatformEvent::QuitRequested { token, .. } => {
                    match crate::shell::window_state(&shell.data.window_id) {
                        // A dormant first window is not open: it has no say.
                        Some(WindowState::Dormant) => {}
                        state => {
                            let keys = guarded_keys(shell);
                            if state == Some(WindowState::Tray) && !keys.is_empty() {
                                // Dirty-document prompts must not be trapped in a hidden window.
                                platform::show_windows();
                                crate::shell::set_tray_hidden(false);
                            }
                            close_tabs(shell, cx, ui, keys, false, AfterClose::Quit(token));
                        }
                    }
                }
                PlatformEvent::QuitReady if main => crate::shell::request_quit_now(),
                PlatformEvent::QuitCancelled => {}
                PlatformEvent::ShowWindow if main => crate::shell::set_tray_hidden(false),
                PlatformEvent::OpenWindow { route } if main => crate::shell::request_window(WindowRequest { route }),
                PlatformEvent::LocalDaemonStatus { .. } if main => ui.core.discover_local_daemon(),
                PlatformEvent::HostError { message } if main => toast(shell, cx, message, true),
                PlatformEvent::Insets(_) => shell.redraw(cx),
                _ => {}
            },
        }
    }
    for action in actions.iter() {
        if let Some(ShellAsync::ProjectIcon { window, host_id, project_id, data }) = action.downcast_ref::<ShellAsync>() {
            if *window != shell.data.window_id {
                continue;
            }
            match data {
                Ok(data) => request(
                    shell,
                    ui,
                    host_id,
                    json!({ "type": "project.icon.set.request", "projectId": project_id, "source": { "type": "upload", "data": data } }),
                    "project.icon.set.response",
                    Pending::Refresh { host_id: host_id.clone() },
                ),
                Err(error) => toast(shell, cx, error.clone(), true),
            }
        }
    }
    for (action, args) in panels::command_center_commands(actions) {
        if shell.data.command_center_open {
            run_command(shell, cx, ui, &action, &args);
        }
    }
    if let Some(widget) = shell.data.command_center.clone() {
        let open = panels::command_center_open(&widget);
        if open != shell.data.command_center_open {
            shell.data.command_center_open = open;
            shell.data.router.reset(cx);
        }
    }
}

/// Reads and encodes a picked project icon off the GUI thread.
fn read_icon(pick: IconPick, path: std::path::PathBuf) {
    std::thread::spawn(move || {
        use base64::Engine;
        let data = std::fs::read(&path)
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .map_err(|e| format!("{}: {e}", path.display()));
        Cx::post_action(ShellAsync::ProjectIcon { window: pick.window, host_id: pick.host_id, project_id: pick.project_id, data });
    });
}

thread_local! {
    static ROUTES: RefCell<VecDeque<PlatformRoute>> = const { RefCell::new(VecDeque::new()) };
}

fn queue_route(_shell: &mut PaseoShell, route: PlatformRoute) {
    ROUTES.with(|routes| routes.borrow_mut().push_back(route));
}

/// `_drainRoutes`: routes wait for their host to be known (and connected for projects).
fn process_routes(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext) {
    if let Some(route) = shell.data.pending_route.clone()
        && open_route(shell, cx, ui, &route)
    {
        shell.data.pending_route = None;
    }
    if !shell.is_main() || hosts::hosts(&ui.state).is_empty() {
        return;
    }
    while let Some(route) = ROUTES.with(|routes| routes.borrow().front().cloned()) {
        if !open_route(shell, cx, ui, &route) {
            return;
        }
        ROUTES.with(|routes| routes.borrow_mut().pop_front());
    }
}

/// Opens `route` in this window; `false` while its host is not known (or not connected for
/// projects) yet.
fn open_route(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, route: &PlatformRoute) -> bool {
    match route {
        PlatformRoute::Agent { server_id, agent_id } => {
            if hosts::host(&ui.state, server_id).is_none() {
                return false;
            }
            open_agent(shell, ui, server_id, agent_id, None, TabPlacement::Main);
            true
        }
        PlatformRoute::ExecutionLog { server_id, workspace_id, terminal_id } => {
            hosts::host(&ui.state, server_id).is_some() && open_execution_log(shell, cx, ui, server_id, workspace_id, terminal_id, None)
        }
        PlatformRoute::Project { path, server_id } => {
            let connected = hosts::hosts(&ui.state)
                .iter()
                .any(|h| h.connected() && server_id.as_deref().is_none_or(|s| s == h.server_id()));
            if !connected {
                return false;
            }
            match server_id {
                Some(id) => open_project_path(shell, ui, id, path),
                None => choose_any_host(shell, cx, ui, HostPurpose::ProjectPath(path.clone()), None),
            }
            true
        }
        PlatformRoute::HostNewTab { server_id } => {
            if hosts::host(&ui.state, server_id).is_none() {
                return false;
            }
            host_chosen(shell, cx, ui, HostPurpose::NewSession { new_workspace: true, worktree: false }, server_id.clone());
            true
        }
    }
}

pub fn handle_window_events(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, ui: &mut UiContext) {
    if std::mem::take(&mut shell.data.close_requested) {
        let keys = guarded_keys(shell);
        close_tabs(shell, cx, ui, keys, false, AfterClose::Window);
    }
    match event {
        Event::WindowCloseRequested(e) if Some(e.window_id) == shell.data.os_window => {
            if std::mem::take(&mut shell.data.close_approved) {
                return;
            }
            e.accept_close.set(false);
            let keys = guarded_keys(shell);
            close_tabs(shell, cx, ui, keys, false, AfterClose::Window);
        }
        // The application menu is process-wide: the first window answers it once.
        Event::MacosMenuCommand(command) if shell.is_main() => {
            if *command == live_id!(new_window) {
                request_new_window();
            } else if *command == live_id!(paseo_quit) {
                if let Some(handle) = paseo_gui_platform::platform() {
                    handle.request_quit(QuitReason::Quit, false);
                } else {
                    crate::shell::request_quit_now();
                }
            }
        }
        Event::Foreground | Event::Resume | Event::AppGotFocus => {
            if std::mem::take(&mut shell.data.app_inactive) {
                // Original host-runtime-bootstrap: an active app verifies its connections.
                ui.core.set_app_visible(true);
            }
        }
        Event::Background | Event::Pause | Event::AppLostFocus => shell.data.app_inactive = true,
        _ => {}
    }
    // A page shown beside the sidebar ends when a session or tab opens.
    if let Some(page) = &shell.data.page
        && !matches!(page, Page::Settings { .. })
        && shell.data.layout.navigation_serial != shell.data.page_serial
    {
        close_page(shell, cx);
    }
    // Hosts removed elsewhere drop their history page scope; the page tracks reconnects.
    history::tick(shell, cx, event, ui);
}

fn geometry_key(shell: &PaseoShell) -> String {
    if shell.is_main() { MAIN_WINDOW_ID.to_string() } else { shell.data.window_id.clone() }
}

pub fn record_geometry(shell: &mut PaseoShell, geom: &makepad_widgets::makepad_platform::event::WindowGeom) {
    if !shell.data.geometry_applied {
        return;
    }
    // AppKit's fullscreen Space is not NSWindow zoom, and its animated bounds
    // must never replace the normal frame saved for the next launch.
    #[cfg(target_os = "macos")]
    if geom.is_fullscreen || geom.is_fullscreen_transitioning {
        return;
    }
    #[cfg(target_os = "macos")]
    let (size, maximized) = (geom.outer_size, geom.is_maximized);
    #[cfg(not(target_os = "macos"))]
    let (size, maximized) = (geom.inner_size, geom.is_fullscreen);
    platform::record_window_geometry(
        &geometry_key(shell),
        WindowGeometry {
            x: Some(geom.position.x),
            y: Some(geom.position.y),
            width: size.x,
            height: size.y,
            maximized,
        },
    );
}

/// Restores this window's saved geometry once its OS window exists.
pub fn apply_initial_geometry(shell: &mut PaseoShell, cx: &mut Cx) {
    if shell.data.geometry_applied {
        return;
    }
    let Some(window) = shell.data.os_window else { return };
    shell.data.geometry_applied = true;
    let Some(geometry) = platform::window_geometry(&geometry_key(shell)) else { return };
    // AppKit drains platform_ops with pop(): enqueue zoom first so restoring
    // the normal frame happens before zoom captures its unzoom destination.
    #[cfg(target_os = "macos")]
    if geometry.maximized {
        cx.push_unique_platform_op(CxOsOp::MaximizeWindow(window));
    }
    if geometry.width > 0.0 && geometry.height > 0.0 {
        cx.push_unique_platform_op(CxOsOp::ResizeWindow(window, dvec2(geometry.width, geometry.height)));
    }
    if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
        cx.push_unique_platform_op(CxOsOp::RepositionWindow(window, dvec2(x, y)));
    }
    #[cfg(not(target_os = "macos"))]
    if geometry.maximized {
        cx.push_unique_platform_op(CxOsOp::MaximizeWindow(window));
    }
}

/// Overlay pointer results that land on shell chrome ids (compact explorer overlay rail).
pub fn overlay_press(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, press: Press) {
    match press {
        Press::Click(id, modifiers) => click(shell, cx, ui, &id, modifiers),
        Press::Secondary(id, abs) => secondary(shell, cx, ui, &id, abs),
    }
}
