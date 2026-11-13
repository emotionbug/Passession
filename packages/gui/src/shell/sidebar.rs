//! Port of `features/navigation/session_sidebar.dart` and the shell's sidebar column
//! (`_sidebarNavigation`, `_sidebar` footer): hosts → projects → sessions with status, search,
//! collapse state, row actions and the footer buttons.

use std::cmp::Ordering;
use std::collections::HashMap;

use chrono::{DateTime, Datelike, Local, Utc};
use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::paint::{Font, rect};
use crate::common::widgets::{PTextFieldWidgetRefExt, RowMetrics, StatusBucket, agent_live_status_bucket, paint_status_badge};
use crate::common::{PanelTarget, UiContext, UiEffect, clock, json as j, tr, tr_args};
use crate::features::workspace::observe::{ObsEvent, Observation};
use crate::shell::chrome::make_ui;
use crate::shell::hosts::{self, Host, Status, parent_agent_id};
use crate::shell::ui::{Ui, Variant};
use crate::shell::PaseoShell;
use crate::theme::{self, ControlSize};

#[derive(Default)]
pub struct SidebarState {
    pub query: String,
    pub scroll: f64,
    pub content_height: f64,
    pub viewport: Rect,
    /// Sidebar requests in flight (`_busy` progress bar).
    pub busy: usize,
    /// Live terminal streams of the workspaces the sidebar shows.
    pub terminals: TerminalDirectories,
    /// Session order per project key as last drawn; kept while the pointer is over the tree
    /// so rows do not move under it.
    pub held: HashMap<String, Vec<String>>,
    /// The session-switch modifier (Ctrl, ⌘ on macOS) is held alone: rows show 1–9 hints.
    pub modifier_held: bool,
    /// A host the footer's host picker chose: the next frame scrolls its row to the top.
    pub reveal_host: Option<String>,
    /// A host to expand (`navigation.collapsed`) once a `UiContext` is at hand.
    pub expand_host: Option<String>,
    motion: makepad_widgets::scroll_bars::TouchScroll,
}

impl SidebarState {
    pub fn stop_motion(&mut self) {
        self.motion.reset();
    }

    pub fn touch_down(&mut self, abs: DVec2, time: f64) {
        self.motion.begin(abs, time, true);
    }

    fn scroll_by(&mut self, delta: f64) -> bool {
        let max = (self.content_height - self.viewport.size.y).max(0.0);
        self.motion.apply_delta(&mut self.scroll, delta, max)
    }

    /// True once this touch owns vertical scrolling, including at an edge.
    pub fn touch_move(&mut self, abs: DVec2, time: f64) -> bool {
        let Some(delta) = self.motion.move_to(abs, time) else { return false };
        self.scroll_by(delta);
        true
    }
}

/// `preferences.navigation`.
pub fn nav_prefs(state: &Value) -> Value {
    j::object(j::object(state, "preferences"), "navigation").clone()
}

fn strings(prefs: &Value, key: &str) -> Vec<String> {
    j::array(prefs, key).iter().map(|v| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }).collect()
}

/// `sidebarProjectKey`.
pub fn project_key(host: &Host<'_>, project: &Value) -> String {
    format!("project:{}:{}", host.server_id(), j::text(project, "projectId"))
}

/// `sidebarProjects`: registered projects plus projects only known through a workspace.
pub fn projects(host: &Host<'_>) -> Vec<Value> {
    let mut out: Vec<Value> = host.projects().to_vec();
    for workspace in host.workspaces() {
        let id = j::text(workspace, "projectId");
        if !out.iter().any(|p| j::text(p, "projectId") == id) {
            out.push(json!({
                "projectId": id,
                "projectKey": j::text(j::object(workspace, "project"), "projectKey"),
                "projectDisplayName": j::text(workspace, "projectDisplayName"),
                "projectRootPath": j::text(workspace, "projectRootPath"),
            }));
        }
    }
    out
}

/// `sidebarLabelKey`.
pub fn label_key(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

pub fn label_filters(prefs: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for key in strings(prefs, "labelFilters").iter().map(|s| label_key(s)) {
        if !out.contains(&key) {
            out.push(key);
        }
    }
    out
}

/// `sidebarWorkspaceMatchesLabels`.
pub fn workspace_matches_labels(workspace: &Value, filters: &[String]) -> bool {
    if filters.is_empty() {
        return true;
    }
    let keys: Vec<String> = j::array(workspace, "labels")
        .iter()
        .map(|l| label_key(l.as_str().unwrap_or("")))
        .filter(|k| !k.is_empty())
        .collect();
    filters.iter().any(|f| if f.is_empty() { keys.is_empty() } else { keys.contains(f) })
}

pub fn host_filters(state: &Value, prefs: &Value) -> Vec<String> {
    let stored = strings(prefs, "hostFilters");
    hosts::hosts(state).iter().map(|h| h.server_id().to_string()).filter(|id| stored.contains(id)).collect()
}

pub fn project_filters(state: &Value, prefs: &Value) -> Vec<String> {
    let stored = strings(prefs, "projectFilters");
    if stored.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for host in hosts::hosts(state) {
        for project in projects(&host) {
            let key = project_key(&host, &project);
            if stored.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// `_compareNames`: case-insensitive, digit runs by value.
pub fn compare_names(left: &str, right: &str) -> Ordering {
    let a: Vec<char> = left.to_lowercase().chars().collect();
    let b: Vec<char> = right.to_lowercase().chars().collect();
    let (mut i, mut k) = (0, 0);
    while i < a.len() && k < b.len() {
        if a[i].is_ascii_digit() && b[k].is_ascii_digit() {
            let si = i;
            let sk = k;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while k < b.len() && b[k].is_ascii_digit() {
                k += 1;
            }
            let na: String = a[si..i].iter().collect::<String>();
            let nb: String = b[sk..k].iter().collect::<String>();
            let na = na.trim_start_matches('0');
            let nb = nb.trim_start_matches('0');
            let (na, nb) = (if na.is_empty() { "0" } else { na }, if nb.is_empty() { "0" } else { nb });
            let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
            if ord != Ordering::Equal {
                return ord;
            }
            continue;
        }
        if a[i] != b[k] {
            return a[i].cmp(&b[k]);
        }
        i += 1;
        k += 1;
    }
    (a.len() - i).cmp(&(b.len() - k))
}

/// `_orderedProjects`: sorted by name, then the stored order permutes its own slots.
pub fn ordered_projects(host: &Host<'_>, stored: &[String]) -> Vec<Value> {
    let mut list = projects(host);
    list.sort_by(|a, b| {
        compare_names(j::text(a, "projectDisplayName"), j::text(b, "projectDisplayName"))
            .then_with(|| project_key(host, a).cmp(&project_key(host, b)))
    });
    let keys: Vec<String> = list.iter().map(|p| project_key(host, p)).collect();
    let mut pruned: Vec<String> = Vec::new();
    for key in stored {
        if keys.contains(key) && !pruned.contains(key) {
            pruned.push(key.clone());
        }
    }
    if list.len() <= 1 || pruned.is_empty() {
        return list;
    }
    let mut next = 0;
    let by_key = |key: &str| list.iter().find(|p| project_key(host, p) == key).cloned();
    keys.iter()
        .map(|key| {
            if pruned.contains(key) {
                let picked = by_key(&pruned[next]).unwrap_or(json!({}));
                next += 1;
                picked
            } else {
                by_key(key).unwrap_or(json!({}))
            }
        })
        .collect()
}

fn time_ms(row: &Value, field: &str) -> i64 {
    DateTime::parse_from_rfc3339(j::text(row, field)).map(|d| d.timestamp_millis()).unwrap_or(0)
}

fn top_level(agent: &Value) -> bool {
    !hosts::is_archived(agent) && !j::text(agent, "workspaceId").is_empty() && parent_agent_id(agent).is_empty()
}

/// A session waiting on the user (a permission or question).
pub fn needs_input(agent: &Value) -> bool {
    agent_live_status_bucket(agent) == StatusBucket::NeedsInput
}

/// `_sessions`: a project's non-archived top-level agents in workspaces the filters keep;
/// sessions waiting on the user first, then newest activity, newest created, id.
pub fn sessions<'a>(host: &Host<'a>, project: &Value, prefs: &Value) -> Vec<&'a Value> {
    let project_id = j::text(project, "projectId");
    let labels = label_filters(prefs);
    let show_background = j::boolean(prefs, "showBackground", false);
    let mut out: Vec<&Value> = host
        .agents()
        .filter(|agent| {
            if !top_level(agent) {
                return false;
            }
            let Some(workspace) = host.workspace(j::text(agent, "workspaceId")) else { return false };
            if j::text(workspace, "projectId") != project_id {
                return false;
            }
            if !show_background && j::boolean(workspace, "background", false) {
                return false;
            }
            workspace_matches_labels(workspace, &labels)
        })
        .collect();
    out.sort_by(|a, b| {
        needs_input(b)
            .cmp(&needs_input(a))
            .then_with(|| time_ms(b, "updatedAt").cmp(&time_ms(a, "updatedAt")))
            .then_with(|| time_ms(b, "createdAt").cmp(&time_ms(a, "createdAt")))
            .then_with(|| j::text(a, "id").cmp(j::text(b, "id")))
    });
    out
}

/// Keeps `rows` in the previously drawn order `held`; sessions that were not drawn then lead in
/// their own order.
pub fn hold_order(rows: &mut [&Value], held: &[String]) {
    rows.sort_by_key(|agent| held.iter().position(|id| id == j::text(agent, "id")).map_or(0, |at| at + 1));
}

/// One project of the session tree as the sidebar lists it.
pub struct ProjectGroup<'a> {
    pub project: Value,
    pub key: String,
    pub collapsed: bool,
    /// The search matches the project's name or path.
    pub matches: bool,
    /// Sessions the search keeps, in display order.
    pub sessions: Vec<&'a Value>,
    /// Sessions waiting on the user, search aside (the collapsed row's badge).
    pub waiting: usize,
}

/// One host of the session tree, in sidebar order with the host/project filters applied.
pub struct HostGroup<'a> {
    pub host: Host<'a>,
    pub collapsed: bool,
    pub matches: bool,
    /// The host lists any project (else the tree offers "Add project").
    pub has_projects: bool,
    pub projects: Vec<ProjectGroup<'a>>,
}

impl HostGroup<'_> {
    pub fn waiting(&self) -> usize {
        self.projects.iter().map(|p| p.waiting).sum()
    }
}

/// Whether a session row matches the sidebar search: title, provider, workspace name or branch.
fn session_matches(host: &Host<'_>, agent: &Value, query: &str) -> bool {
    let workspace = host.workspace(j::text(agent, "workspaceId"));
    let name = workspace.map_or("", |w| j::text(w, "name"));
    let branch = workspace.map_or("", |w| j::text(j::object(w, "gitRuntime"), "currentBranch"));
    format!("{} {} {name} {branch}", j::text(agent, "title"), j::text(agent, "provider")).to_lowercase().contains(query)
}

/// The sidebar's session tree: hosts in `hostOrder`, `ordered_projects`, `sessions`, with the
/// host/project/label filters and the (lowercased) search applied. `held` keeps each project's
/// drawn order. Search ignores collapse; terminal rows are the caller's.
pub fn session_tree<'a>(state: &'a Value, query: &str, held: Option<&HashMap<String, Vec<String>>>) -> Vec<HostGroup<'a>> {
    let prefs = nav_prefs(state);
    let collapsed = strings(&prefs, "collapsed");
    let host_order = strings(&prefs, "hostOrder");
    let project_order = strings(&prefs, "projectOrder");
    let hf = host_filters(state, &prefs);
    let pf = project_filters(state, &prefs);
    let mut all = hosts::hosts(state);
    all.sort_by_key(|h| host_order.iter().position(|id| id == h.server_id()).unwrap_or(host_order.len()));
    let mut out = Vec::new();
    for host in all {
        let sid = host.server_id();
        if !hf.is_empty() && !hf.iter().any(|h| h == sid) {
            continue;
        }
        let label = host.display_label();
        let host_matches = label.to_lowercase().contains(query);
        let ordered = ordered_projects(&host, &project_order);
        let mut projects = Vec::new();
        for project in &ordered {
            let key = project_key(&host, project);
            if !pf.is_empty() && !pf.contains(&key) {
                continue;
            }
            let matches = format!("{} {}", j::text(project, "projectDisplayName"), j::text(project, "projectRootPath")).to_lowercase().contains(query);
            let all_rows = sessions(&host, project, &prefs);
            let waiting = all_rows.iter().filter(|a| needs_input(a)).count();
            let mut rows: Vec<&Value> = all_rows
                .into_iter()
                .filter(|agent| query.is_empty() || host_matches || matches || session_matches(&host, agent, query))
                .collect();
            if let Some(order) = held.and_then(|held| held.get(&key)) {
                hold_order(&mut rows, order);
            }
            let collapsed = query.is_empty() && collapsed.contains(&key);
            projects.push(ProjectGroup { project: project.clone(), key, collapsed, matches, sessions: rows, waiting });
        }
        let collapsed = query.is_empty() && collapsed.iter().any(|c| *c == format!("host:{sid}"));
        out.push(HostGroup { host, collapsed, matches: host_matches, has_projects: !ordered.is_empty(), projects });
    }
    out
}

/// Sessions in the order the sidebar shows them (session switching, Ctrl+1–9): rows inside a
/// collapsed host or project are skipped unless `include_collapsed`.
pub fn navigation_sessions(state: &Value, query: &str, held: Option<&HashMap<String, Vec<String>>>, include_collapsed: bool) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for group in session_tree(state, query, held) {
        if group.collapsed && !include_collapsed {
            continue;
        }
        for project in &group.projects {
            if project.collapsed && !include_collapsed {
                continue;
            }
            out.extend(project.sessions.iter().map(|agent| (group.host.server_id().to_string(), (*agent).clone())));
        }
    }
    out
}

/// Whether the pointer rests on the session tree (its rows must not reorder under it).
pub fn pointer_on_tree(hover: Option<&str>) -> bool {
    hover.is_some_and(|id| {
        ["session:", "sessionedit:", "sideterm:", "project:", "projplus:", "host:", "hostplus:"].iter().any(|prefix| id.starts_with(prefix))
    })
}

/// Whether a top-level session other than `focused` waits on the user (the header's ☰ dot).
pub fn others_need_input(state: &Value, focused: Option<&PanelTarget>) -> bool {
    hosts::hosts(state).iter().any(|host| {
        host.agents().any(|agent| {
            top_level(agent)
                && needs_input(agent)
                && !focused.is_some_and(|t| t.kind == "agent" && t.server_id == host.server_id() && t.id.as_deref() == Some(j::text(agent, "id")))
        })
    })
}

/// One shown workspace's `subscribe_terminals_request {cwd, workspaceId}` stream. Dropping it
/// releases the observation.
pub struct TerminalDirectory {
    observation: Observation,
    cwd: String,
    /// Live terminals in first-seen order: the daemon lists them in no stable order.
    terminals: Vec<Value>,
}

/// Terminal streams by (host, workspace).
pub type TerminalDirectories = HashMap<(String, String), TerminalDirectory>;

impl TerminalDirectory {
    /// Applies a `terminals_changed` list; true when the rows changed.
    fn apply(&mut self, listed: &[Value]) -> bool {
        let next = merge_terminals(&self.terminals, listed);
        if next == self.terminals {
            return false;
        }
        self.terminals = next;
        true
    }
}

/// The listed terminals in first-seen order: known ones keep their place (with fresh fields),
/// new ones follow in list order, gone ones drop.
fn merge_terminals(previous: &[Value], listed: &[Value]) -> Vec<Value> {
    let same = |a: &Value, b: &Value| j::text(a, "id") == j::text(b, "id");
    let mut next: Vec<Value> = previous.iter().filter_map(|old| listed.iter().find(|t| same(t, old)).cloned()).collect();
    for terminal in listed {
        if !j::text(terminal, "id").is_empty() && !next.iter().any(|t| same(t, terminal)) {
            next.push(terminal.clone());
        }
    }
    next
}

/// Workspaces whose terminals the sidebar lists, as (host, workspace, directory): every
/// non-archived workspace of a project the host/project/label/background filters keep, on a
/// connected host. Collapse and search only hide rows; their streams stay.
fn terminal_directories(state: &Value) -> Vec<(String, String, String)> {
    let prefs = nav_prefs(state);
    let hf = host_filters(state, &prefs);
    let pf = project_filters(state, &prefs);
    let labels = label_filters(&prefs);
    let show_background = j::boolean(&prefs, "showBackground", false);
    let mut out = Vec::new();
    for host in hosts::hosts(state) {
        let sid = host.server_id();
        if !host.connected() || (!hf.is_empty() && !hf.iter().any(|h| h == sid)) {
            continue;
        }
        let kept: Vec<String> = projects(&host)
            .iter()
            .filter(|p| pf.is_empty() || pf.contains(&project_key(&host, p)))
            .map(|p| j::text(p, "projectId").to_string())
            .collect();
        for workspace in host.workspaces() {
            let id = j::text(workspace, "id");
            let directory = j::text(workspace, "workspaceDirectory");
            if id.is_empty()
                || directory.is_empty()
                || !j::is_null_or_missing(workspace, "archivedAt")
                || !kept.iter().any(|p| p == j::text(workspace, "projectId"))
                || (!show_background && j::boolean(workspace, "background", false))
                || !workspace_matches_labels(workspace, &labels)
            {
                continue;
            }
            out.push((sid.to_string(), id.to_string(), directory.to_string()));
        }
    }
    out
}

/// Keeps one terminal stream per [`terminal_directories`] entry, applies the batch's
/// `terminals_changed` snapshots and releases streams of workspaces no longer shown. True when
/// the sidebar must redraw.
pub fn sync_terminals(sidebar: &mut SidebarState, ui: &UiContext) -> bool {
    let wanted = terminal_directories(&ui.state);
    let before = sidebar.terminals.len();
    sidebar.terminals.retain(|(host, workspace), dir| wanted.iter().any(|(h, w, cwd)| h == host && w == workspace && *cwd == dir.cwd));
    let mut changed = sidebar.terminals.len() != before;
    for (host_id, workspace_id, cwd) in wanted {
        sidebar.terminals.entry((host_id.clone(), workspace_id.clone())).or_insert_with(|| {
            let fields = json!({ "cwd": cwd, "workspaceId": workspace_id });
            let observation = Observation::start(&ui.core, &host_id, "subscribe_terminals_request", "terminals_changed", fields);
            TerminalDirectory { observation, cwd, terminals: Vec::new() }
        });
    }
    for dir in sidebar.terminals.values_mut() {
        for item in dir.observation.take(&ui.events) {
            changed |= match item {
                ObsEvent::Snapshot { payload, .. } => dir.apply(j::array(&payload, "terminals")),
                ObsEvent::Event(message) if j::text(&message, "type") == "terminals_changed" => dir.apply(j::array(j::object(&message, "payload"), "terminals")),
                ObsEvent::Failed(_) | ObsEvent::RestoreFailed(_) => dir.apply(&[]),
                ObsEvent::Event(_) => false,
            };
        }
    }
    changed
}

/// A live interactive terminal row of the sidebar.
#[derive(Clone, Debug)]
pub struct TerminalRow {
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    pub id: String,
    pub label: String,
    pub activity: Value,
}

impl TerminalRow {
    /// The terminal tab this row opens (`PanelTarget` kind `terminal`).
    pub fn target(&self) -> PanelTarget {
        PanelTarget::new("terminal", &self.host_id, &self.workspace_id, &self.cwd).with_id(&self.id).with_title(&self.label)
    }
}

/// Terminal row label: its title, else a given name; the daemon's default name "Terminal" and
/// an empty one read as the localized fallback.
pub fn terminal_label(terminal: &Value) -> String {
    let title = j::text(terminal, "title").trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let name = j::text(terminal, "name").trim();
    if name.is_empty() || name == "Terminal" { tr("workspace.tabs.fallback.terminal") } else { name.to_string() }
}

/// Live terminals of a project's shown workspaces in workspace order. Configured script and
/// service terminals (the workspace's `scripts[].terminalId`) stay in their log surfaces.
pub fn terminal_rows(terminals: &TerminalDirectories, host: &Host<'_>, project_id: &str) -> Vec<TerminalRow> {
    let sid = host.server_id();
    let mut out = Vec::new();
    for workspace in host.workspaces().iter().filter(|w| j::text(w, "projectId") == project_id) {
        let workspace_id = j::text(workspace, "id");
        let Some(dir) = terminals.get(&(sid.to_string(), workspace_id.to_string())) else { continue };
        let scripts: Vec<&str> = j::array(workspace, "scripts").iter().map(|s| j::text(s, "terminalId")).filter(|id| !id.is_empty()).collect();
        for terminal in &dir.terminals {
            let id = j::text(terminal, "id");
            if scripts.contains(&id) {
                continue;
            }
            out.push(TerminalRow {
                host_id: sid.to_string(),
                workspace_id: workspace_id.to_string(),
                cwd: dir.cwd.clone(),
                id: id.to_string(),
                label: terminal_label(terminal),
                activity: j::object(terminal, "activity").clone(),
            });
        }
    }
    out
}

/// The row behind a `sideterm:<host>:<terminal>` hit.
pub fn find_terminal(terminals: &TerminalDirectories, state: &Value, rest: &str) -> Option<TerminalRow> {
    let (sid, terminal_id) = rest.rsplit_once(':')?;
    let host = hosts::host(state, sid)?;
    let (_, workspace_id) = terminals.iter().find(|((h, _), dir)| h == sid && dir.terminals.iter().any(|t| j::text(t, "id") == terminal_id))?.0;
    let project_id = j::text(host.workspace(workspace_id)?, "projectId");
    terminal_rows(terminals, &host, project_id).into_iter().find(|row| row.id == terminal_id)
}

/// Session row subtitle: what the session needs from the user outranks what it is doing; a
/// settled session keeps the daemon's own status word.
fn agent_status_label(agent: &Value, bucket: StatusBucket) -> String {
    match bucket {
        StatusBucket::NeedsInput => tr("sidebar.statusBucket.needsInput"),
        StatusBucket::Failed => tr("sidebar.statusBucket.failed"),
        StatusBucket::Running => tr("sidebar.statusBucket.working"),
        StatusBucket::Attention => tr("sidebar.statusBucket.readyToReview"),
        StatusBucket::Done => {
            let status = j::text(agent, "status");
            let key = format!("agentList.status.{status}");
            let label = tr(&key);
            if label == key { status.to_string() } else { label }
        }
    }
}

/// `deriveTerminalActivityStatusBucket` as the row subtitle's status.
fn terminal_status(activity: &Value) -> String {
    match (j::text(activity, "attentionReason"), j::text(activity, "state")) {
        ("needs_input", _) => tr("sidebar.statusBucket.needsInput"),
        ("finished", _) => tr("sidebar.statusBucket.readyToReview"),
        (_, "working") => tr("sidebar.statusBucket.working"),
        _ => tr("agentList.status.idle"),
    }
}

/// `formatCompactTimeAgo`, localized.
pub fn compact_time_ago(iso: &str) -> String {
    let Ok(date) = DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let date = date.with_timezone(&Utc);
    let elapsed = Utc::now().signed_duration_since(date);
    if elapsed.num_minutes() < 1 {
        return tr("shell.time.now");
    }
    if elapsed.num_hours() < 1 {
        return tr_args("shell.time.minutes", &[("count", &elapsed.num_minutes().to_string())]);
    }
    if elapsed.num_days() < 1 {
        return tr_args("shell.time.hours", &[("count", &elapsed.num_hours().to_string())]);
    }
    if elapsed.num_days() < 7 {
        return tr_args("shell.time.days", &[("count", &elapsed.num_days().to_string())]);
    }
    let local = date.with_timezone(&Local);
    crate::common::month_day(local.month0(), local.day())
}

pub fn status_color(p: &theme::Palette, host: &Host<'_>) -> Vec4 {
    let live = host.connected() && !host.loading() && host.error().is_none();
    if live {
        p.success
    } else if host.status() == Status::Disconnected {
        p.danger
    } else {
        p.warning
    }
}

/// Original sidebar nav rows (`sidebarNavItems`, defaults new-workspace/history/search/schedules).
pub fn nav_order(preferences: &Value) -> Vec<&'static str> {
    const DEFAULTS: [&str; 4] = ["new-workspace", "history", "search", "schedules"];
    let mut order = Vec::new();
    let mut placed = Vec::new();
    for row in j::array(preferences, "sidebarNavItems") {
        let id = j::text(row, "key");
        if let Some(found) = DEFAULTS.iter().find(|d| **d == id)
            && !placed.contains(found)
        {
            placed.push(*found);
            if row.get("visible") != Some(&Value::Bool(false)) {
                order.push(*found);
            }
        }
    }
    order.extend(DEFAULTS.iter().filter(|d| !placed.contains(d)));
    order
}

/// Sidebar column: nav rows, section header + search, session tree, footer.
pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, state: &Value, r: Rect) {
    let p = theme::current_palette();
    let compact = shell.data.compact;
    shell.painter.fill(cx, r, p.sidebar, 0.0);
    let preferences = j::object(state, "preferences").clone();
    let mut y = r.pos.y;
    let typography = theme::current_typography();
    let title_h = shell.painter.line_height(cx, crate::common::paint::TextSpec::new(Font::Regular, typography.ui_size(14.0), p.foreground));
    let small_h = shell.painter.line_height(cx, crate::common::paint::TextSpec::new(Font::Regular, typography.ui_size(12.0), p.muted));
    let nav_h = RowMetrics::new(title_h, 0.0, 0.0).height(32.0, 6.0, 16.0);

    // Navigation rows (`SidebarHeaderRow`, 2px apart, top 8 bottom 6, bottom border).
    let order = nav_order(&preferences);
    if !order.is_empty() || compact {
        y += 8.0;
        let mut ui = make_ui(shell);
        for (index, id) in order.iter().enumerate() {
            let (icon, label) = match *id {
                "new-workspace" => ("plus", tr("sessionNavigation.newSession")),
                "history" => ("history", tr("sidebar.sections.sessions")),
                "search" => ("search", tr("sidebar.sections.search")),
                _ => ("calendar-clock", tr("sidebar.sections.schedules")),
            };
            let row_w = if compact && index == 0 { r.size.x - 48.0 } else { r.size.x };
            header_row(&mut ui, cx, rect(r.pos.x, y, row_w, nav_h), &format!("nav:{id}"), icon, &label);
            if compact && index == 0 {
                let close = rect(r.pos.x + r.size.x - 40.0, y + (nav_h - 32.0) * 0.5, 32.0, 32.0);
                ui.icon_button(cx, close, "nav:close", "x", 16.0, 16.0, &tr("sidebar.actions.closeSidebar"));
            }
            y += nav_h + 2.0;
        }
        y += 4.0;
        ui.painter.hline(cx, r.pos.x, y, r.size.x, p.border);
        y += 1.0;
    }

    // Section header: muted small title + display menu trigger.
    let header_h = small_h + if compact { 28.0 } else { 20.0 };
    {
        let mut ui = make_ui(shell);
        let spec = ui.small_muted();
        ui.text(cx, rect(r.pos.x + 8.0, y, r.size.x - 48.0, header_h), spec, &tr("sessionNavigation.sessions"));
        let trigger = rect(r.pos.x + r.size.x - 4.0 - 28.0, y + (header_h - 28.0) * 0.5, 28.0, 28.0);
        if ui.is_hover("side:display") {
            let hover = ui.p.sidebar_hover;
            ui.fill(cx, trigger, hover, 6.0);
        }
        let muted = ui.p.muted;
        ui.icon(cx, trigger, "settings-2", 14.0, muted);
        ui.hit_tip(trigger, "side:display", &tr("sidebar.display.trigger"));
    }
    y += header_h;

    // Search field (bare input, 6px radius divider border).
    if shell.data.search.is_none() {
        let field = WidgetRef::new_from_ptr(cx, shell.search_field);
        field.as_ptext_field().set_placeholder(cx, &tr("sessionNavigation.search"));
        shell.data.search = Some(field);
    }
    if let Some(field) = shell.data.search.clone() {
        // The input owns its font metrics and padding; a fixed-height clip cuts
        // off its bottom border when native text is taller.
        let walk = Walk {
            abs_pos: Some(dvec2(r.pos.x + 12.0, y)),
            width: Size::Fixed(r.size.x - 24.0),
            height: Size::Fit,
            ..Walk::default()
        };
        cx.begin_turtle(walk, Layout::flow_down());
        field.draw_all(cx, scope);
        y += cx.end_turtle().size.y;
    }
    y += 8.0;
    if shell.data.sidebar.busy > 0 {
        shell.painter.fill(cx, rect(r.pos.x, y - 2.0, r.size.x, 2.0), p.accent, 0.0);
    }

    // Footer (8/6 padding, top border).
    let footer_h = if compact { 56.0 } else { 40.0 };
    let footer = rect(r.pos.x, r.pos.y + r.size.y - footer_h, r.size.x, footer_h);
    let list = rect(r.pos.x, y, r.size.x, (footer.pos.y - y).max(0.0));
    shell.data.sidebar.viewport = list;
    draw_tree(shell, cx, state, list);
    draw_footer(shell, cx, footer);
}

fn header_row(ui: &mut Ui, cx: &mut Cx2d, r: Rect, id: &str, icon: &str, label: &str) {
    // Original `SidebarHeaderRow`: 8px inset, 6px radius, sidebar hover fill, md muted glyph.
    let body = rect(r.pos.x + 8.0, r.pos.y, r.size.x - 16.0, r.size.y);
    if ui.is_hover(id) || ui.is_pressed(id) {
        let hover = ui.p.sidebar_hover;
        ui.fill(cx, body, hover, 6.0);
    }
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 14.0, p.foreground);
    let metrics = RowMetrics::measure(ui.painter, cx, spec, None, 0.0);
    let slots = metrics.layout(body, 8.0, 16.0, 0.0, 8.0);
    ui.icon(cx, slots.leading, icon, 16.0, p.muted);
    ui.text(cx, slots.title, spec, label);
    ui.hit(body, id);
}

fn toolbar_button(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, id: &str, icon: &str, tooltip: &str) {
    // Row `ToolbarButton`: 20px hitbox, 6px radius, sm muted glyph.
    let r = rect(x, y, 20.0, 20.0);
    ui.icon_button(cx, r, id, icon, 14.0, 6.0, tooltip);
}

/// Pill counting sessions that wait on the user, drawn right-aligned ending at `right`.
fn waiting_badge(ui: &mut Ui, cx: &mut Cx2d, right: f64, mid: f64, count: usize) {
    let p = ui.p;
    let text = count.to_string();
    let spec = ui.spec(Font::Medium, 11.0, p.warning);
    let w = waiting_badge_width(ui, cx, count);
    let r = rect(right - w, mid - 8.0, w, 16.0);
    ui.fill(cx, r, theme::with_alpha(p.warning, 0.16), 8.0);
    ui.text_centered(cx, r, spec, &text);
}

fn waiting_badge_width(ui: &mut Ui, cx: &mut Cx2d, count: usize) -> f64 {
    let spec = ui.spec(Font::Medium, 11.0, ui.p.warning);
    (ui.measure(cx, spec, &count.to_string()) + 10.0).max(18.0)
}

/// The 1–9 keycap a held Ctrl/⌘ shows on the first sessions (`workspace.navigate.index`).
fn index_hint(ui: &mut Ui, cx: &mut Cx2d, slot: Rect, index: usize) {
    let p = ui.p;
    let r = rect(slot.pos.x + slot.size.x - 18.0, slot.pos.y + (slot.size.y - 18.0) * 0.5, 18.0, 18.0);
    ui.painter.outline(cx, r, p.surface2, p.muted, 1.0, 4.0);
    let spec = ui.spec(Font::Medium, 11.0, p.foreground);
    ui.text_centered(cx, r, spec, &(index + 1).to_string());
}

fn draw_tree(shell: &mut PaseoShell, cx: &mut Cx2d, state: &Value, list: Rect) {
    let query = shell.data.sidebar.query.trim().to_lowercase();
    let trailing_diff = j::text(j::object(state, "preferences"), "sidebarWorkspaceTrailing") == "diff";
    let host_count = hosts::hosts(state).len();
    // While the pointer rests on the tree, rows keep the order they were drawn in.
    let holding = pointer_on_tree(shell.data.pointer.hover.as_deref());
    let held = std::mem::take(&mut shell.data.sidebar.held);
    let tree = session_tree(state, &query, holding.then_some(&held));
    let mut drawn_order: HashMap<String, Vec<String>> = HashMap::new();
    let selected = shell.data.layout.selected().cloned();
    let scroll = shell.data.sidebar.scroll;
    let hints = shell.data.sidebar.modifier_held && !shell.data.compact;
    let reveal = shell.data.sidebar.reveal_host.take();
    let mut reveal_offset: Option<f64> = None;
    // The rows read the streams while `ui` borrows the shell; they go back after drawing.
    let terminals = std::mem::take(&mut shell.data.sidebar.terminals);

    cx.begin_turtle(Walk::abs_rect(list), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    let mut ui = make_ui(shell);
    ui.hits.set_clip(Some(list));
    let p = ui.p;
    let mut y = list.pos.y - scroll;
    let x0 = list.pos.x;
    let w = list.size.x;
    let group = ui.spec(Font::Medium, 14.0, p.muted);
    // Uniform line metrics keep culling O(1) per row without shaping invisible titles.
    let regular = ui.spec(Font::Regular, 14.0, p.foreground);
    let subtitle = ui.spec(Font::Regular, 12.0, p.muted);
    let title_h = ui.painter.line_height(cx, regular).max(ui.painter.line_height(cx, group));
    let small_h = ui.painter.line_height(cx, subtitle);
    let group_metrics = RowMetrics::new(title_h, 0.0, 0.0);
    let session_metrics = RowMetrics::new(title_h, small_h, 2.0);
    let group_h = group_metrics.height(36.0, 8.0, 14.0);
    let session_h = session_metrics.height(0.0, 7.0, 14.0);
    let button_h = theme::control_height(ControlSize::Sm);
    let mut any = false;
    // Position in the session-switch order (Ctrl+1–9), counted like `navigation_sessions`.
    let mut nav_index = 0usize;
    for host_group in &tree {
        let host = &host_group.host;
        let sid = host.server_id();
        let label = host.display_label();
        let host_matches = host_group.matches;
        let host_collapsed = host_group.collapsed;
        // Visible project blocks first (search may hide the host); live terminals follow the
        // directory's sessions and search matches their label.
        let mut blocks: Vec<(&ProjectGroup, Vec<TerminalRow>)> = Vec::new();
        for project in &host_group.projects {
            let terms: Vec<TerminalRow> = terminal_rows(&terminals, host, j::text(&project.project, "projectId"))
                .into_iter()
                .filter(|row| query.is_empty() || host_matches || project.matches || row.label.to_lowercase().contains(&query))
                .collect();
            if !query.is_empty() && !host_matches && !project.matches && project.sessions.is_empty() && terms.is_empty() {
                continue;
            }
            drawn_order.insert(project.key.clone(), project.sessions.iter().map(|a| j::text(a, "id").to_string()).collect());
            blocks.push((project, terms));
        }
        if !query.is_empty() && !host_matches && blocks.is_empty() {
            continue;
        }
        any = true;
        // Host row.
        y += 8.0;
        if reveal.as_deref() == Some(sid) {
            reveal_offset = Some((y - 8.0 + scroll - list.pos.y).max(0.0));
        }
        let row = rect(x0 + 8.0, y, w - 16.0, group_h);
        // A collapsed host still says how many of its sessions wait on the user.
        let waiting = if host_collapsed { host_group.waiting() } else { 0 };
        let badge_w = if waiting > 0 { waiting_badge_width(&mut ui, cx, waiting) + 4.0 } else { 0.0 };
        let slots = group_metrics.layout(row, 0.0, 26.0, 20.0 + badge_w, 4.0);
        let mid = row.pos.y + row.size.y * 0.5;
        let plus_x = slots.trailing.pos.x + slots.trailing.size.x - 20.0;
        ui.icon(cx, rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0), if host_collapsed { "chevron-right" } else { "chevron-down" }, 14.0, p.muted);
        ui.dot(cx, dvec2(slots.leading.pos.x + 22.0, mid), 8.0, status_color(&p, host));
        ui.text(cx, slots.title, group, &label);
        if waiting > 0 {
            waiting_badge(&mut ui, cx, plus_x - 4.0, mid, waiting);
        }
        // The whole row toggles; the trailing button, registered after it, wins its own square.
        ui.hit(row, format!("host:{sid}"));
        toolbar_button(&mut ui, cx, plus_x, mid - 10.0, &format!("hostplus:{sid}"), "plus", &tr("sessionNavigation.newSession"));
        y += group_h;
        if host_collapsed {
            continue;
        }
        let small = ui.spec(Font::Regular, 12.0, p.foreground);
        let loading = host.loading_lists();
        if host.loading() || !loading.is_empty() {
            ui.fill(cx, rect(x0 + 12.0, y, w - 24.0, 2.0), p.accent, 1.0);
            y += 2.0;
        }
        if !loading.is_empty() {
            let lists: Vec<String> = loading.iter().map(|k| tr(&format!("sidebar.refresh.{k}"))).collect();
            let text = tr_args("sidebar.refresh.loading", &[("lists", &lists.join(", "))]);
            ui.text(cx, rect(x0 + 32.0, y + 4.0, w - 40.0, small_h), small, &text);
            y += small_h + 8.0;
        }
        let errors = host.list_errors();
        for (kind, error) in &errors {
            let text = tr_args("sidebar.refresh.failed", &[("list", &tr(&format!("sidebar.refresh.{kind}"))), ("error", error)]);
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            ui.text(cx, rect(x0 + 32.0, y + 4.0, w - 40.0, small_h), spec, &text);
            y += small_h + 8.0;
        }
        if !errors.is_empty() {
            let label = tr("common.actions.retry");
            let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
            ui.button(cx, rect(x0 + 24.0, y, bw, button_h), &format!("hostretry:{sid}"), &label, None, Variant::Ghost, ControlSize::Sm, loading.is_empty());
            y += button_h + 4.0;
        }
        if let Some(error) = host.error() {
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            let tw = w - 40.0;
            let clipped: String = error.lines().take(3).collect::<Vec<_>>().join("\n");
            let h = ui.painter.paragraph_height(cx, tw, spec, &clipped);
            ui.painter.paragraph(cx, dvec2(x0 + 32.0, y), tw, spec, &clipped);
            y += h + 4.0;
            let retry = tr("common.actions.retry");
            let rw = ui.button_width(cx, &retry, None, ControlSize::Sm);
            ui.button(cx, rect(x0 + 24.0, y, rw, button_h), &format!("hostretry:{sid}"), &retry, None, Variant::Ghost, ControlSize::Sm, true);
            let auth = tr("shell.sidebar.authConnection");
            let aw = ui.button_width(cx, &auth, None, ControlSize::Sm).min((w - 24.0 - rw - 4.0 - 8.0).max(0.0));
            ui.button(cx, rect(x0 + 24.0 + rw + 4.0, y, aw, button_h), &format!("hostauth:{sid}"), &auth, None, Variant::Ghost, ControlSize::Sm, true);
            y += button_h + 4.0;
        }
        for (project, terms) in &blocks {
            let key = &project.key;
            let name = j::text_or(&project.project, "projectDisplayName", j::text(&project.project, "projectRootPath"));
            let name = if name.is_empty() { j::text(&project.project, "projectRootPath") } else { name };
            let row = rect(x0 + 24.0, y, w - 32.0, group_h);
            let waiting = if project.collapsed { project.waiting } else { 0 };
            let badge_w = if waiting > 0 { waiting_badge_width(&mut ui, cx, waiting) + 4.0 } else { 0.0 };
            let slots = group_metrics.layout(row, 0.0, 32.0, 20.0 + badge_w, 4.0);
            let mid = row.pos.y + row.size.y * 0.5;
            let plus_x = slots.trailing.pos.x + slots.trailing.size.x - 20.0;
            ui.icon(cx, rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0), if project.collapsed { "chevron-right" } else { "chevron-down" }, 14.0, p.muted);
            ui.icon(cx, rect(slots.leading.pos.x + 18.0, mid - 7.0, 14.0, 14.0), "folder", 14.0, p.muted);
            ui.text(cx, slots.title, group, name);
            if waiting > 0 {
                waiting_badge(&mut ui, cx, plus_x - 4.0, mid, waiting);
            }
            ui.hit(row, format!("project:{key}"));
            toolbar_button(&mut ui, cx, plus_x, mid - 10.0, &format!("projplus:{key}"), "plus", &tr("sessionNavigation.newSession"));
            y += group_h;
            if project.collapsed {
                continue;
            }
            for agent in &project.sessions {
                let index = nav_index;
                nav_index += 1;
                let row = rect(x0 + 48.0, y, w - 56.0, session_h);
                y += row.size.y;
                // Clipping alone still shapes and paints every off-screen row
                // on each touch move. Keep its extent, not its draw/hit work.
                if row.pos.y + row.size.y <= list.pos.y || row.pos.y >= list.pos.y + list.size.y {
                    continue;
                }
                let agent_id = j::text(agent, "id");
                let id = format!("session:{sid}:{agent_id}");
                let is_selected = selected.as_ref().is_some_and(|t| t.kind == "agent" && t.server_id == sid && t.id.as_deref() == Some(agent_id));
                if is_selected {
                    ui.fill(cx, row, p.border, 6.0);
                }
                let hovered = ui.is_hover(&id) || ui.is_hover(&format!("sessionedit:{sid}:{agent_id}"));
                let attention = j::boolean(agent, "requiresAttention", false);
                let title = j::text(agent, "title");
                let title = if title.is_empty() { tr("sessionNavigation.untitled") } else { title.to_string() };
                let hint = hints && index < 9;
                let trailing = if trailing_diff || hint { String::new() } else { compact_time_ago(j::text_or(agent, "updatedAt", j::text(agent, "createdAt"))) };
                let trailing_w = if hovered { 32.0 } else if hint { 30.0 } else if trailing.is_empty() { 12.0 } else { ui.measure(cx, ui.spec(Font::Regular, 12.0, p.muted), &trailing) + 16.0 };
                let slots = session_metrics.layout(row, 12.0, 14.0, (trailing_w - 12.0).max(0.0), 8.0);
                ui.icon(cx, slots.leading, "message-square", 14.0, if attention { p.foreground } else { p.muted });
                let bucket = agent_live_status_bucket(agent);
                let backdrop = if is_selected { p.border } else { p.sidebar };
                let glyph = rect(slots.leading.pos.x, slots.leading.pos.y + (slots.leading.size.y - 14.0) * 0.5, 14.0, 14.0);
                if paint_status_badge(ui.painter, cx, glyph, Some(bucket).filter(|b| *b != StatusBucket::Done), &p, backdrop) {
                    let area = ui.painter.draw_box.area();
                    clock::register(cx, area);
                }
                let title_spec = ui.spec(if attention { Font::Medium } else { Font::Regular }, 14.0, p.foreground);
                ui.text(cx, slots.title, title_spec, &title);
                let sub = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, slots.subtitle, sub, &format!("{} · {}", j::text(agent, "provider"), agent_status_label(agent, bucket)));
                ui.hit(row, id.clone());
                if hovered {
                    toolbar_button(&mut ui, cx, slots.trailing.pos.x, row.pos.y + (row.size.y - 20.0) * 0.5, &format!("sessionedit:{sid}:{agent_id}"), "pencil", &tr("workspace.tabs.menu.renameAgent"));
                } else if hint {
                    index_hint(&mut ui, cx, slots.trailing, index);
                } else if !trailing.is_empty() {
                    let spec = ui.spec(Font::Regular, 12.0, p.muted);
                    ui.text_right(cx, slots.trailing, spec, &trailing);
                }
            }
            for terminal in terms {
                let row = rect(x0 + 48.0, y, w - 56.0, session_h);
                y += row.size.y;
                if row.pos.y + row.size.y <= list.pos.y || row.pos.y >= list.pos.y + list.size.y {
                    continue;
                }
                let is_selected = selected.as_ref().is_some_and(|t| t.kind == "terminal" && t.server_id == sid && t.id.as_deref() == Some(terminal.id.as_str()));
                if is_selected {
                    ui.fill(cx, row, p.border, 6.0);
                }
                let attention = !j::text(&terminal.activity, "attentionReason").is_empty();
                let slots = session_metrics.layout(row, 12.0, 14.0, 0.0, 8.0);
                ui.icon(cx, slots.leading, "square-terminal", 14.0, if attention { p.foreground } else { p.muted });
                let title_spec = ui.spec(if attention { Font::Medium } else { Font::Regular }, 14.0, p.foreground);
                ui.text(cx, slots.title, title_spec, &terminal.label);
                let sub = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, slots.subtitle, sub, &format!("{} · {}", tr("workspace.tabs.fallback.terminal"), terminal_status(&terminal.activity)));
                ui.hit(row, format!("sideterm:{sid}:{}", terminal.id));
            }
        }
        let project_list_loading = host.loading() || loading.contains(&"project") || errors.iter().any(|(k, _)| *k == "project");
        if !host_group.has_projects && !project_list_loading && query.is_empty() {
            let row = rect(x0 + 24.0, y, w - 32.0, group_h);
            let slots = group_metrics.layout(row, 0.0, 32.0, 0.0, 4.0);
            let mid = row.pos.y + row.size.y * 0.5;
            ui.icon(cx, rect(slots.leading.pos.x + 18.0, mid - 7.0, 14.0, 14.0), "folder-plus", 14.0, p.muted);
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text(cx, slots.title, spec, &tr("sidebar.actions.addProject"));
            ui.hit(row, format!("addproject:{sid}"));
            y += group_h;
        }
    }
    if !any {
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let message = if host_count == 0 {
            tr("shell.sidebar.connectHost")
        } else if !query.is_empty() {
            tr("shell.sidebar.noMatches")
        } else {
            tr("sessionNavigation.empty")
        };
        let text_w = w - 32.0;
        let text_h = ui.painter.paragraph_height(cx, text_w, spec, &message).max(title_h);
        ui.painter.paragraph(cx, dvec2(x0 + 16.0, y + 16.0), text_w, spec, &message);
        y += text_h + 24.0;
        // Archived sessions and older history are not in the tree: offer the History search.
        let (id, label, variant) = if host_count == 0 {
            ("side:addhost", tr("settings.addHost"), Variant::Outline)
        } else if !query.is_empty() {
            let raw: String = query.chars().take(40).collect();
            ("side:searchhistory", tr_args("shell.sidebar.searchHistory", &[("query", &raw)]), Variant::Outline)
        } else {
            ("side:display", tr("sidebar.display.trigger"), Variant::Ghost)
        };
        let bw = ui.button_width(cx, &label, None, ControlSize::Sm).min(text_w);
        ui.button(cx, rect(x0 + 16.0, y + 8.0, bw, button_h), id, &label, None, variant, ControlSize::Sm, true);
        y += button_h + 16.0;
    }
    y += 8.0;
    ui.hits.set_clip(None);
    cx.end_turtle();
    shell.data.sidebar.held = drawn_order;
    shell.data.sidebar.terminals = terminals;
    shell.data.sidebar.content_height = y + scroll - list.pos.y;
    let max = (shell.data.sidebar.content_height - list.size.y).max(0.0);
    if let Some(offset) = reveal_offset {
        shell.data.sidebar.scroll = offset.min(max);
        shell.redraw(cx);
    } else if shell.data.sidebar.scroll > max {
        shell.data.sidebar.scroll = max;
    }
}

fn draw_footer(shell: &mut PaseoShell, cx: &mut Cx2d, r: Rect) {
    let compact = shell.data.compact;
    let mut ui = make_ui(shell);
    let p = ui.p;
    ui.painter.hline(cx, r.pos.x, r.pos.y, r.size.x, p.border);
    let extent = if compact { 44.0 } else { 28.0 };
    let icon = if compact { 20.0 } else { 16.0 };
    let y = r.pos.y + (r.size.y - extent) * 0.5;
    // `FooterIconButton`: muted glyph, foreground while hovered, round highlight only pressed.
    let button = |ui: &mut Ui, cx: &mut Cx2d, x: f64, id: &str, glyph: &str, size: f64, tooltip: &str| {
        let b = rect(x, y, extent, extent);
        if ui.is_pressed(id) {
            let h = ui.p.interaction_highlight;
            ui.fill(cx, b, h, extent * 0.5);
        }
        let color = if ui.is_hover(id) { ui.p.foreground } else { ui.p.muted };
        ui.icon(cx, b, glyph, size, color);
        ui.hit_tip(b, id, tooltip);
    };
    let mut x = r.pos.x + 8.0;
    button(&mut ui, cx, x, "foot:addproject", "folder-plus", icon, &tr("sidebar.actions.addProject"));
    x += extent;
    button(&mut ui, cx, x, "foot:usage", "circle-gauge", icon, &tr("sidebar.footer.usage"));
    x += extent;
    button(&mut ui, cx, x, "foot:hosts", "server", icon - 1.0, &tr("sidebar.actions.hosts"));
    let right = r.pos.x + r.size.x - 8.0;
    button(&mut ui, cx, right - extent, "foot:settings", "settings", icon, &crate::common::tooltip_with_shortcut(&tr("sidebar.actions.settings"), "toggle-settings"));
    button(&mut ui, cx, right - extent * 2.0, "foot:help", "circle-help", icon, &tr("sidebar.help.trigger"));
}

pub fn scroll(shell: &mut PaseoShell, cx: &mut Cx, delta: f64) {
    let s = &mut shell.data.sidebar;
    s.stop_motion();
    if s.scroll_by(delta) {
        shell.redraw(cx);
    }
}

pub fn touch_up(shell: &mut PaseoShell, cx: &mut Cx, time: f64) -> bool {
    let motion = &mut shell.data.sidebar.motion;
    let panned = motion.release(time);
    motion.schedule(cx);
    panned
}

pub fn animate(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) {
    let s = &mut shell.data.sidebar;
    let Some(delta) = s.motion.frame(cx, event) else { return };
    let changed = s.scroll_by(delta);
    if changed {
        shell.redraw(cx);
    }
}

/// Search field changes (Event::Actions).
pub fn handle_actions(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, _ui: &mut UiContext) {
    let Event::Actions(actions) = event else { return };
    let Some(field) = shell.data.search.clone() else { return };
    if let Some(text) = field.as_ptext_field().changed(actions) {
        shell.data.sidebar.stop_motion();
        shell.data.sidebar.query = text.trim().to_lowercase();
        shell.data.sidebar.scroll = 0.0;
        shell.redraw(cx);
    }
}

/// Expands a host (`host:<id>`) or project key if it is collapsed.
pub fn expand(ui: &mut UiContext, key: &str) {
    let mut prefs = nav_prefs(&ui.state);
    let mut collapsed = strings(&prefs, "collapsed");
    let before = collapsed.len();
    collapsed.retain(|c| c != key);
    if collapsed.len() != before {
        set_nav(ui, &mut prefs, "collapsed", json!(collapsed));
    }
}

/// Tracks the session-switch modifier (Ctrl; ⌘ on macOS) held on its own, for the 1–9 row
/// hints. Returns true when the state changed.
pub fn track_modifier(state: &mut SidebarState, event: &Event) -> bool {
    let modifier = if cfg!(target_os = "macos") { KeyCode::Logo } else { KeyCode::Control };
    let held = match event {
        Event::KeyDown(key) => key.key_code == modifier && !key.modifiers.shift && !key.modifiers.alt,
        Event::KeyUp(_) | Event::AppLostFocus | Event::Pause | Event::Background | Event::MouseDown(_) => false,
        _ => return false,
    };
    if held == state.modifier_held {
        return false;
    }
    state.modifier_held = held;
    true
}

/// `_toggle`: collapse/expand a host (`host:<id>`) or project key in `navigation.collapsed`.
pub fn toggle_collapsed(ui: &mut UiContext, key: &str) {
    let mut prefs = nav_prefs(&ui.state);
    let mut collapsed = strings(&prefs, "collapsed");
    if let Some(index) = collapsed.iter().position(|c| c == key) {
        collapsed.remove(index);
    } else {
        collapsed.push(key.to_string());
    }
    set_nav(ui, &mut prefs, "collapsed", json!(collapsed));
}

pub fn set_nav(ui: &mut UiContext, prefs: &mut Value, field: &str, value: Value) {
    if !prefs.is_object() {
        *prefs = json!({});
    }
    prefs[field] = value;
    ui.push(UiEffect::SetPreference { key: "navigation".into(), value: prefs.clone() });
}

/// `_move`: reorder within `hostOrder`/`projectOrder`, keeping unrelated stored keys.
pub fn move_in_order(ui: &mut UiContext, field: &str, keys: &[String], key: &str, delta: i64) {
    let Some(index) = keys.iter().position(|k| k == key) else { return };
    let destination = index as i64 + delta;
    if destination < 0 || destination >= keys.len() as i64 {
        return;
    }
    let mut keys = keys.to_vec();
    let item = keys.remove(index);
    keys.insert(destination as usize, item);
    let mut prefs = nav_prefs(&ui.state);
    let unrelated: Vec<String> = strings(&prefs, field).into_iter().filter(|k| !keys.contains(k)).collect();
    keys.extend(unrelated);
    set_nav(ui, &mut prefs, field, json!(keys));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_names_orders_digit_runs_by_value() {
        assert_eq!(compare_names("item2", "item10"), Ordering::Less);
        assert_eq!(compare_names("Alpha", "alpha"), Ordering::Equal);
        assert_eq!(compare_names("b", "a"), Ordering::Greater);
        assert_eq!(compare_names("v007", "v7"), Ordering::Equal);
    }

    #[test]
    fn unlabelled_filter_matches_only_label_free_workspaces() {
        let filters = vec![String::new()];
        assert!(workspace_matches_labels(&json!({"labels": []}), &filters));
        assert!(!workspace_matches_labels(&json!({"labels": ["Bug"]}), &filters));
        assert!(workspace_matches_labels(&json!({"labels": ["  Bug   fix "]}), &["bug fix".to_string()]));
    }

    #[test]
    fn nav_order_keeps_stored_order_and_appends_missing_defaults() {
        let prefs = json!({"sidebarNavItems": [{"key": "search"}, {"key": "history", "visible": false}]});
        assert_eq!(nav_order(&prefs), vec!["search", "new-workspace", "schedules"]);
    }

    #[test]
    fn terminal_merge_keeps_first_seen_order_and_fresh_fields() {
        let previous = vec![json!({"id": "a", "name": "one"}), json!({"id": "b"})];
        let listed = vec![json!({"id": "c"}), json!({"id": "a", "name": "renamed"}), json!({"id": ""})];
        let merged = merge_terminals(&previous, &listed);
        assert_eq!(merged, vec![json!({"id": "a", "name": "renamed"}), json!({"id": "c"})]);
    }

    #[test]
    fn terminal_directories_follow_connection_and_sidebar_filters() {
        let workspace = |id: &str, project: &str, extra: Value| {
            let mut w = json!({"id": id, "projectId": project, "workspaceDirectory": format!("/w/{id}")});
            w.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
            w
        };
        let state = json!({
            "hosts": [
                {"serverId": "on", "status": "connected", "workspaces": [
                    workspace("a", "p", json!({})),
                    workspace("archived", "p", json!({"archivedAt": "2026-01-01T00:00:00Z"})),
                    workspace("bg", "p", json!({"background": true})),
                    workspace("other", "q", json!({})),
                ]},
                {"serverId": "off", "status": "disconnected", "workspaces": [workspace("z", "p", json!({}))]},
            ],
            "preferences": {"navigation": {"projectFilters": ["project:on:p"]}},
        });
        assert_eq!(terminal_directories(&state), vec![("on".to_string(), "a".to_string(), "/w/a".to_string())]);
    }

    #[test]
    fn terminal_labels_prefer_title_then_a_given_name() {
        assert_eq!(terminal_label(&json!({"title": " vim ", "name": "dev"})), "vim");
        assert_eq!(terminal_label(&json!({"name": "dev"})), "dev");
        assert_eq!(terminal_label(&json!({"name": "Terminal"})), tr("workspace.tabs.fallback.terminal"));
    }

    #[test]
    fn session_status_puts_needed_input_before_activity() {
        let label = |agent: Value| agent_status_label(&agent, agent_live_status_bucket(&agent));
        assert_eq!(label(json!({"status": "running", "pendingPermissions": [{"id": "q"}]})), tr("sidebar.statusBucket.needsInput"));
        assert_eq!(label(json!({"status": "error", "requiresAttention": true, "attentionReason": "error"})), tr("sidebar.statusBucket.failed"));
        assert_eq!(label(json!({"status": "idle", "activeTurn": {"id": "t"}})), tr("sidebar.statusBucket.working"));
        assert_eq!(label(json!({"status": "idle", "requiresAttention": true, "attentionReason": "finished"})), tr("sidebar.statusBucket.readyToReview"));
        assert_eq!(label(json!({"status": "idle"})), tr("agentList.status.idle"));
    }

    fn tree_state(navigation: Value) -> Value {
        let agent = |id: &str, ws: &str, updated: &str, extra: Value| {
            let mut a = json!({"id": id, "workspaceId": ws, "updatedAt": updated, "status": "idle"});
            a.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap_or_default());
            a
        };
        json!({
            "hosts": [
                {"serverId": "b", "status": "connected",
                 "projects": [{"projectId": "p", "projectDisplayName": "Zeta"}, {"projectId": "q", "projectDisplayName": "Alpha"}],
                 "workspaces": [{"id": "wp", "projectId": "p"}, {"id": "wq", "projectId": "q"}],
                 "agents": [
                    agent("old", "wp", "2026-01-01T00:00:00Z", json!({})),
                    agent("new", "wp", "2026-03-01T00:00:00Z", json!({})),
                    agent("asks", "wp", "2025-01-01T00:00:00Z", json!({"pendingPermissions": [{"id": "x"}]})),
                    agent("alpha", "wq", "2026-01-01T00:00:00Z", json!({})),
                 ]},
                {"serverId": "a", "status": "connected", "projects": [{"projectId": "r", "projectDisplayName": "R"}],
                 "workspaces": [{"id": "wr", "projectId": "r"}],
                 "agents": [agent("first", "wr", "2026-01-01T00:00:00Z", json!({}))]},
            ],
            "preferences": {"navigation": navigation},
        })
    }

    fn ids(list: &[(String, Value)]) -> Vec<String> {
        list.iter().map(|(_, a)| j::text(a, "id").to_string()).collect()
    }

    #[test]
    fn navigation_follows_sidebar_order_filters_and_collapse() {
        let state = tree_state(json!({"hostOrder": ["a", "b"]}));
        // Host order, project name order, waiting sessions pinned, then newest first.
        assert_eq!(ids(&navigation_sessions(&state, "", None, false)), ["first", "alpha", "asks", "new", "old"]);
        let collapsed = tree_state(json!({"hostOrder": ["a", "b"], "collapsed": ["host:a", "project:b:q"]}));
        assert_eq!(ids(&navigation_sessions(&collapsed, "", None, false)), ["asks", "new", "old"]);
        assert_eq!(ids(&navigation_sessions(&collapsed, "", None, true)).len(), 5);
        let filtered = tree_state(json!({"hostFilters": ["b"], "projectFilters": ["project:b:p"]}));
        assert_eq!(ids(&navigation_sessions(&filtered, "", None, false)), ["asks", "new", "old"]);
        assert_eq!(ids(&navigation_sessions(&state, "alp", None, false)), ["alpha"]);
    }

    #[test]
    fn held_order_keeps_drawn_rows_and_puts_new_ones_first() {
        let state = tree_state(json!({}));
        let mut held = HashMap::new();
        held.insert("project:b:p".to_string(), vec!["old".to_string(), "asks".to_string()]);
        let order = ids(&navigation_sessions(&state, "", Some(&held), false));
        let p: Vec<&str> = order.iter().map(String::as_str).filter(|id| ["old", "new", "asks"].contains(id)).collect();
        assert_eq!(p, ["new", "old", "asks"]);
    }

    #[test]
    fn collapsed_groups_count_waiting_sessions() {
        let state = tree_state(json!({"collapsed": ["host:b"]}));
        let tree = session_tree(&state, "", None);
        let b = tree.iter().find(|g| g.host.server_id() == "b").unwrap();
        assert!(b.collapsed);
        assert_eq!(b.waiting(), 1);
        assert!(others_need_input(&state, None));
        let focused = PanelTarget::new("agent", "b", "wp", "").with_id("asks");
        assert!(!others_need_input(&state, Some(&focused)));
    }
}
