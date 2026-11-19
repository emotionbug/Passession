//! Port of `features/navigation/session_sidebar.dart` and the shell's sidebar column
//! (`_sidebarNavigation`, `_sidebar` footer): hosts → projects → sessions with status, search,
//! collapse state, row actions and the footer buttons.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local, Utc};
use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::paint::{Font, HitViewport, rect};
use crate::common::widgets::{
    PTextFieldWidgetRefExt, RowMetrics, StatusBucket, agent_live_status_bucket, paint_status_badge,
};
use crate::common::{PanelTarget, UiContext, UiEffect, json as j, tr, tr_args};
use crate::features::workspace::observe::{ObsEvent, Observation};
use crate::shell::PaseoShell;
use crate::shell::chrome::make_ui;
use crate::shell::hosts::{self, Host, Status, parent_agent_id};
use crate::shell::ui::{Ui, Variant};
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
    presentation: PresentationCache,
    /// The session-switch modifier (Ctrl, ⌘ on macOS) is held alone: rows show 1–9 hints.
    pub modifier_held: bool,
    /// A host the footer's host picker chose: the next frame scrolls its row to the top.
    pub reveal_host: Option<String>,
    /// A host to expand (`navigation.collapsed`) once a `UiContext` is at hand.
    pub expand_host: Option<String>,
    pub(crate) header_layer: crate::common::layer::Layer,
    pub(crate) tree_layer: crate::common::layer::Layer,
    pub(crate) footer_layer: crate::common::layer::Layer,
    header_bottom: f64,
    tree_hits: HitViewport,
    tree_band: TreeBand,
    motion: makepad_widgets::scroll_bars::TouchScroll,
}

/// A bounded strip of retained rows in content coordinates. Common drags and
/// flings stay inside it; only crossing its edge needs newly exposed rows.
#[derive(Default)]
struct TreeBand {
    top: f64,
    bottom: f64,
}

impl TreeBand {
    fn around(scroll: f64, height: f64) -> Self {
        let overscan = height * 2.0;
        Self {
            top: (scroll - overscan).max(0.0),
            bottom: scroll + height + overscan,
        }
    }

    fn covers(&self, scroll: f64, height: f64, content_height: f64) -> bool {
        self.top <= scroll && self.bottom >= (scroll + height).min(content_height)
    }

    fn rect(&self, viewport: Rect) -> Rect {
        rect(
            viewport.pos.x,
            viewport.pos.y + self.top,
            viewport.size.x,
            self.bottom - self.top,
        )
    }
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

    fn place_tree(&self, cx: &mut Cx) {
        let shift = dvec2(0.0, -self.scroll);
        self.tree_hits.place(self.viewport, shift);
        self.tree_layer.place_viewport(cx, self.viewport, shift);
        if !self
            .tree_band
            .covers(self.scroll, self.viewport.size.y, self.content_height)
        {
            self.tree_layer.invalidate(cx);
        }
    }

    fn place_scrolled_tree(&self, cx: &mut Cx) {
        self.place_tree(cx);
        if cx.accessibility_enabled() {
            self.tree_layer.redraw_parent(cx);
        }
    }

    /// True once this touch owns vertical scrolling, including at an edge.
    pub fn touch_move(&mut self, cx: &mut Cx, abs: DVec2, time: f64) -> bool {
        let Some(delta) = self.motion.move_to(abs, time) else {
            return false;
        };
        if self.scroll_by(delta) {
            self.place_scrolled_tree(cx);
        }
        true
    }

    fn advance(&mut self, cx: &mut Cx, event: &Event) {
        if let Some(delta) = self.motion.frame(cx, event)
            && self.scroll_by(delta)
        {
            self.place_scrolled_tree(cx);
        }
    }
}

/// `preferences.navigation`.
pub fn nav_prefs(state: &paseo_gui_core::CoreState) -> Value {
    j::object(j::object(state, "preferences"), "navigation").clone()
}

fn strings(prefs: &Value, key: &str) -> Vec<String> {
    j::array(prefs, key)
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// `sidebarProjectKey`.
pub fn project_key(host: &Host<'_>, project: &Value) -> String {
    format!(
        "project:{}:{}",
        host.server_id(),
        j::text(project, "projectId")
    )
}

/// `sidebarProjects`: registered projects plus projects only known through a workspace.
pub fn projects(host: &Host<'_>) -> Vec<Value> {
    let mut out: Vec<Value> = host.projects().iter().map(|row| (**row).clone()).collect();
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
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
    filters.iter().any(|f| {
        if f.is_empty() {
            keys.is_empty()
        } else {
            keys.contains(f)
        }
    })
}

pub fn host_filters(state: &paseo_gui_core::CoreState, prefs: &Value) -> Vec<String> {
    let stored = strings(prefs, "hostFilters");
    hosts::hosts(state)
        .iter()
        .map(|h| h.server_id().to_string())
        .filter(|id| stored.contains(id))
        .collect()
}

pub fn project_filters(state: &paseo_gui_core::CoreState, prefs: &Value) -> Vec<String> {
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
            let (na, nb) = (
                if na.is_empty() { "0" } else { na },
                if nb.is_empty() { "0" } else { nb },
            );
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
        compare_names(
            j::text(a, "projectDisplayName"),
            j::text(b, "projectDisplayName"),
        )
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
    DateTime::parse_from_rfc3339(j::text(row, field))
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

fn top_level(agent: &Value) -> bool {
    !hosts::is_archived(agent)
        && !j::text(agent, "workspaceId").is_empty()
        && parent_agent_id(agent).is_empty()
}

/// A session waiting on the user (a permission or question).
pub fn needs_input(agent: &Value) -> bool {
    agent_live_status_bucket(agent) == StatusBucket::NeedsInput
}

/// One project of the session tree as the sidebar lists it.
pub struct ProjectGroup {
    pub project: Value,
    pub key: String,
    pub collapsed: bool,
    /// The search matches the project's name or path.
    pub matches: bool,
    /// Sessions the search keeps, in display order.
    pub sessions: Vec<SessionRow>,
    /// Sessions waiting on the user, search aside (the collapsed row's badge).
    pub waiting: usize,
}

/// One host of the session tree, in sidebar order with the host/project filters applied.
pub struct HostGroup {
    pub server_id: String,
    pub collapsed: bool,
    pub matches: bool,
    /// The host lists any project (else the tree offers "Add project").
    pub has_projects: bool,
    pub projects: Vec<ProjectGroup>,
}

impl HostGroup {
    pub fn waiting(&self) -> usize {
        self.projects.iter().map(|p| p.waiting).sum()
    }
}

pub struct SessionRow {
    row: Arc<Value>,
    updated_at: i64,
    created_at: i64,
    waiting: bool,
}

impl SessionRow {
    fn agent(&self) -> &Value {
        j::agent_of(&self.row)
    }
}

#[derive(Default)]
struct SessionTree {
    hosts: Vec<HostGroup>,
    visible: HashMap<String, HashSet<String>>,
}

impl SessionTree {
    fn touches_activity(&self, changed: &HashSet<(String, String)>) -> bool {
        changed
            .iter()
            .any(|(server, id)| self.visible.get(server).is_some_and(|ids| ids.contains(id)))
    }
}

struct HostKey {
    server_id: String,
    directory_key: Option<u64>,
    revision: Option<u64>,
    label: String,
    endpoint: String,
    hostname: String,
}

impl HostKey {
    fn matches(&self, state: &paseo_gui_core::CoreState, host: &Value) -> bool {
        self.server_id == j::text(host, "serverId")
            && self.directory_key == host.get("directoryKey").and_then(Value::as_u64)
            && self.revision == state.directory(host).map(|directory| directory.revision)
            && self.label == j::text(j::object(host, "config"), "label")
            && self.endpoint == j::text(j::object(host, "config"), "endpoint")
            && self.hostname == j::text(j::object(host, "serverInfo"), "hostname")
    }
}

struct PresentationKey {
    hosts: Vec<HostKey>,
    navigation: Value,
    query: String,
    held: Option<HashMap<String, Vec<String>>>,
}

impl PresentationKey {
    fn matches(
        &self,
        state: &paseo_gui_core::CoreState,
        query: &str,
        held: Option<&HashMap<String, Vec<String>>>,
    ) -> bool {
        let hosts = j::array(state, "hosts");
        self.query == query
            && self.held.as_ref() == held
            && &self.navigation == j::object(j::object(state, "preferences"), "navigation")
            && self.hosts.len() == hosts.len()
            && self
                .hosts
                .iter()
                .zip(hosts)
                .all(|(key, host)| key.matches(state, host))
    }

    fn new(
        state: &paseo_gui_core::CoreState,
        query: &str,
        held: Option<&HashMap<String, Vec<String>>>,
    ) -> Self {
        let hosts = j::array(state, "hosts")
            .iter()
            .map(|host| HostKey {
                server_id: j::text(host, "serverId").to_string(),
                directory_key: host.get("directoryKey").and_then(Value::as_u64),
                revision: state.directory(host).map(|directory| directory.revision),
                label: j::text(j::object(host, "config"), "label").to_string(),
                endpoint: j::text(j::object(host, "config"), "endpoint").to_string(),
                hostname: j::text(j::object(host, "serverInfo"), "hostname").to_string(),
            })
            .collect();
        Self {
            hosts,
            navigation: nav_prefs(state),
            query: query.to_string(),
            held: held.cloned(),
        }
    }
}

#[derive(Default)]
struct PresentationCache {
    key: Option<PresentationKey>,
    tree: Arc<SessionTree>,
}

impl PresentationCache {
    fn tree(
        &mut self,
        state: &paseo_gui_core::CoreState,
        query: &str,
        held: Option<&HashMap<String, Vec<String>>>,
    ) -> Arc<SessionTree> {
        if !self
            .key
            .as_ref()
            .is_some_and(|key| key.matches(state, query, held))
        {
            self.tree = Arc::new(session_tree(state, query, held));
            self.key = Some(PresentationKey::new(state, query, held));
        }
        Arc::clone(&self.tree)
    }
}

/// Activity-only publications cannot change search/filter/collapse membership. Query the
/// last drawn model, not the new directory revision, until the recency draw is due.
pub fn touches_visible_activity(
    sidebar: &SidebarState,
    changed: &HashSet<(String, String)>,
) -> bool {
    sidebar.presentation.tree.touches_activity(changed)
}

/// The sidebar's session tree: hosts in `hostOrder`, `ordered_projects`, `sessions`, with the
/// host/project/label filters and the (lowercased) search applied. `held` keeps each project's
/// drawn order. Search ignores collapse; terminal rows are the caller's.
fn session_tree(
    state: &paseo_gui_core::CoreState,
    query: &str,
    held: Option<&HashMap<String, Vec<String>>>,
) -> SessionTree {
    let prefs = nav_prefs(state);
    let collapsed = strings(&prefs, "collapsed");
    let host_order = strings(&prefs, "hostOrder");
    let project_order = strings(&prefs, "projectOrder");
    let hf = host_filters(state, &prefs);
    let pf = project_filters(state, &prefs);
    let labels = label_filters(&prefs);
    let show_background = j::boolean(&prefs, "showBackground", false);
    let mut all = hosts::hosts(state);
    all.sort_by_key(|h| {
        host_order
            .iter()
            .position(|id| id == h.server_id())
            .unwrap_or(host_order.len())
    });
    let mut out = SessionTree::default();
    for host in all {
        let sid = host.server_id();
        if !hf.is_empty() && !hf.iter().any(|h| h == sid) {
            continue;
        }
        let label = host.display_label();
        let host_matches = label.to_lowercase().contains(query);
        let ordered = ordered_projects(&host, &project_order);
        // Index eligible workspaces once, then partition the host's agents in one pass.
        let workspaces: HashMap<&str, &Value> = host
            .workspaces()
            .iter()
            .map(Arc::as_ref)
            .filter(|workspace| {
                (show_background || !j::boolean(workspace, "background", false))
                    && workspace_matches_labels(workspace, &labels)
            })
            .map(|workspace| (j::text(workspace, "id"), workspace))
            .collect();
        let mut by_project: HashMap<&str, Vec<SessionRow>> = HashMap::new();
        for row in host
            .1
            .into_iter()
            .flat_map(|directory| directory.agents.iter())
        {
            let agent = j::agent_of(row);
            if !top_level(agent) {
                continue;
            }
            let Some(workspace) = workspaces.get(j::text(agent, "workspaceId")) else {
                continue;
            };
            by_project
                .entry(j::text(workspace, "projectId"))
                .or_default()
                .push(SessionRow {
                    row: Arc::clone(row),
                    updated_at: time_ms(agent, "updatedAt"),
                    created_at: time_ms(agent, "createdAt"),
                    waiting: needs_input(agent),
                });
        }
        let mut projects = Vec::new();
        for project in &ordered {
            let key = project_key(&host, project);
            if !pf.is_empty() && !pf.contains(&key) {
                continue;
            }
            let matches = format!(
                "{} {}",
                j::text(project, "projectDisplayName"),
                j::text(project, "projectRootPath")
            )
            .to_lowercase()
            .contains(query);
            let mut rows = by_project
                .remove(j::text(project, "projectId"))
                .unwrap_or_default();
            let waiting = rows.iter().filter(|row| row.waiting).count();
            rows.retain(|row| {
                if query.is_empty() || host_matches || matches {
                    return true;
                }
                let agent = row.agent();
                let workspace = workspaces[j::text(agent, "workspaceId")];
                format!(
                    "{} {} {} {}",
                    j::text(agent, "title"),
                    j::text(agent, "provider"),
                    j::text(workspace, "name"),
                    j::text(j::object(workspace, "gitRuntime"), "currentBranch"),
                )
                .to_lowercase()
                .contains(query)
            });
            rows.sort_by(|a, b| {
                b.waiting
                    .cmp(&a.waiting)
                    .then_with(|| b.updated_at.cmp(&a.updated_at))
                    .then_with(|| b.created_at.cmp(&a.created_at))
                    .then_with(|| j::text(a.agent(), "id").cmp(j::text(b.agent(), "id")))
            });
            if let Some(order) = held.and_then(|held| held.get(&key)) {
                let ranks: HashMap<&str, usize> = order
                    .iter()
                    .enumerate()
                    .map(|(index, id)| (id.as_str(), index + 1))
                    .collect();
                rows.sort_by_key(|row| ranks.get(j::text(row.agent(), "id")).copied().unwrap_or(0));
            }
            let collapsed = query.is_empty() && collapsed.contains(&key);
            projects.push(ProjectGroup {
                project: project.clone(),
                key,
                collapsed,
                matches,
                sessions: rows,
                waiting,
            });
        }
        let collapsed = query.is_empty() && collapsed.iter().any(|c| *c == format!("host:{sid}"));
        if !collapsed {
            let visible = out.visible.entry(sid.to_string()).or_default();
            for project in projects.iter().filter(|project| !project.collapsed) {
                visible.extend(
                    project
                        .sessions
                        .iter()
                        .map(|row| j::text(row.agent(), "id").to_string()),
                );
            }
        }
        out.hosts.push(HostGroup {
            server_id: sid.to_string(),
            collapsed,
            matches: host_matches,
            has_projects: !ordered.is_empty(),
            projects,
        });
    }
    out
}

/// Sessions in the order the sidebar shows them (session switching, Ctrl+1–9): rows inside a
/// collapsed host or project are skipped unless `include_collapsed`.
pub fn navigation_sessions(
    state: &paseo_gui_core::CoreState,
    query: &str,
    held: Option<&HashMap<String, Vec<String>>>,
    include_collapsed: bool,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for group in session_tree(state, query, held).hosts {
        if group.collapsed && !include_collapsed {
            continue;
        }
        for project in &group.projects {
            if project.collapsed && !include_collapsed {
                continue;
            }
            out.extend(
                project
                    .sessions
                    .iter()
                    .map(|row| (group.server_id.clone(), row.agent().clone())),
            );
        }
    }
    out
}

/// Whether the pointer rests on the session tree (its rows must not reorder under it).
pub fn pointer_on_tree(hover: Option<&str>) -> bool {
    hover.is_some_and(|id| {
        [
            "session:",
            "sessionedit:",
            "sideterm:",
            "sidetermmenu:",
            "project:",
            "projplus:",
            "host:",
            "hostplus:",
        ]
        .iter()
        .any(|prefix| id.starts_with(prefix))
    })
}

/// Whether a top-level session other than `focused` waits on the user (the header's ☰ dot).
pub fn others_need_input(state: &paseo_gui_core::CoreState, focused: Option<&PanelTarget>) -> bool {
    hosts::hosts(state).iter().any(|host| {
        host.agents().any(|agent| {
            top_level(agent)
                && needs_input(agent)
                && !focused.is_some_and(|t| {
                    t.kind == "agent"
                        && t.server_id == host.server_id()
                        && t.id.as_deref() == Some(j::text(agent, "id"))
                })
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
    fn apply(&mut self, listed: &[Value], retired: Option<&HashSet<String>>) -> bool {
        let next = merge_terminals(&self.terminals, listed, retired);
        if next == self.terminals {
            return false;
        }
        self.terminals = next;
        true
    }
}

/// The listed terminals in first-seen order: known ones keep their place (with fresh fields),
/// new ones follow in list order, gone ones drop.
fn merge_terminals(
    previous: &[Value],
    listed: &[Value],
    retired: Option<&HashSet<String>>,
) -> Vec<Value> {
    let active = |t: &&Value| {
        let id = j::text(t, "id");
        !id.is_empty() && retired.is_none_or(|retired| !retired.contains(id))
    };
    let same = |a: &Value, b: &Value| j::text(a, "id") == j::text(b, "id");
    let mut next: Vec<Value> = previous
        .iter()
        .filter(active)
        .filter_map(|old| listed.iter().find(|t| same(t, old)).cloned())
        .collect();
    for terminal in listed.iter().filter(active) {
        if !next.iter().any(|t| same(t, terminal)) {
            next.push(terminal.clone());
        }
    }
    next
}

fn retire_terminal_rows(rows: &mut Vec<Value>, retired: &HashSet<String>) -> bool {
    let before = rows.len();
    rows.retain(|row| !retired.contains(j::text(row, "id")));
    rows.len() != before
}

/// Workspaces whose terminals the sidebar lists, as (host, workspace, directory): every
/// non-archived workspace of a project the host/project/label/background filters keep, on a
/// connected host. Collapse and search only hide rows; their streams stay.
fn terminal_directories(state: &paseo_gui_core::CoreState) -> Vec<(String, String, String)> {
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
    sidebar.terminals.retain(|(host, workspace), dir| {
        wanted
            .iter()
            .any(|(h, w, cwd)| h == host && w == workspace && *cwd == dir.cwd)
    });
    let mut changed = sidebar.terminals.len() != before;
    for (host_id, workspace_id, cwd) in wanted {
        sidebar
            .terminals
            .entry((host_id.clone(), workspace_id.clone()))
            .or_insert_with(|| {
                let fields = json!({ "cwd": cwd, "workspaceId": workspace_id });
                let observation = Observation::start(
                    &ui.core,
                    &host_id,
                    "subscribe_terminals_request",
                    "terminals_changed",
                    fields,
                );
                TerminalDirectory {
                    observation,
                    cwd,
                    terminals: Vec::new(),
                }
            });
    }
    for ((host, _), dir) in &mut sidebar.terminals {
        let retired = ui.terminal_terminations.retired(host);
        if let Some(retired) = retired
            && ui.terminal_terminations.removed_this_batch
        {
            changed |= retire_terminal_rows(&mut dir.terminals, retired);
        }
        for item in dir.observation.take(&ui.events) {
            changed |= match item {
                ObsEvent::Snapshot { payload, .. } => {
                    dir.apply(j::array(&payload, "terminals"), retired)
                }
                ObsEvent::Event(message) if j::text(&message, "type") == "terminals_changed" => dir
                    .apply(
                        j::array(j::object(&message, "payload"), "terminals"),
                        retired,
                    ),
                ObsEvent::Failed(_) | ObsEvent::RestoreFailed(_) => dir.apply(&[], retired),
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
        PanelTarget::new("terminal", &self.host_id, &self.workspace_id, &self.cwd)
            .with_id(&self.id)
            .with_title(&self.label)
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
    if name.is_empty() || name == "Terminal" {
        tr("workspace.tabs.fallback.terminal")
    } else {
        name.to_string()
    }
}

fn workspaces_by_project<'a>(
    workspaces: impl IntoIterator<Item = &'a Value>,
) -> HashMap<&'a str, Vec<&'a Value>> {
    let mut projects: HashMap<&str, Vec<&Value>> = HashMap::new();
    for workspace in workspaces {
        projects
            .entry(j::text(workspace, "projectId"))
            .or_default()
            .push(workspace);
    }
    projects
}

/// Live terminals of a project's shown workspaces in workspace order. Configured script and
/// service terminals (the workspace's `scripts[].terminalId`) stay in their log surfaces.
fn terminal_rows<'a>(
    terminals: &TerminalDirectories,
    sid: &str,
    workspaces: impl IntoIterator<Item = &'a Value>,
) -> Vec<TerminalRow> {
    let mut out = Vec::new();
    for workspace in workspaces {
        let workspace_id = j::text(workspace, "id");
        let Some(dir) = terminals.get(&(sid.to_string(), workspace_id.to_string())) else {
            continue;
        };
        let scripts: Vec<&str> = j::array(workspace, "scripts")
            .iter()
            .map(|s| j::text(s, "terminalId"))
            .filter(|id| !id.is_empty())
            .collect();
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
pub fn find_terminal(
    terminals: &TerminalDirectories,
    state: &paseo_gui_core::CoreState,
    rest: &str,
) -> Option<TerminalRow> {
    let (sid, terminal_id) = rest.rsplit_once(':')?;
    let host = hosts::host(state, sid)?;
    let (_, workspace_id) = terminals
        .iter()
        .find(|((h, _), dir)| {
            h == sid
                && dir
                    .terminals
                    .iter()
                    .any(|t| j::text(t, "id") == terminal_id)
        })?
        .0;
    let project_id = j::text(host.workspace(workspace_id)?, "projectId");
    let workspaces = host
        .workspaces()
        .iter()
        .map(Arc::as_ref)
        .filter(|workspace| j::text(workspace, "projectId") == project_id);
    terminal_rows(terminals, sid, workspaces)
        .into_iter()
        .find(|row| row.id == terminal_id)
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
            if label == key {
                status.to_string()
            } else {
                label
            }
        }
    }
}

/// `deriveTerminalActivityStatusBucket` as the row subtitle's status.
fn terminal_status(activity: &Value) -> String {
    match (
        j::text(activity, "attentionReason"),
        j::text(activity, "state"),
    ) {
        ("needs_input", _) => tr("sidebar.statusBucket.needsInput"),
        ("finished", _) => tr("sidebar.statusBucket.readyToReview"),
        (_, "working") => tr("sidebar.statusBucket.working"),
        _ => tr("agentList.status.idle"),
    }
}

/// `formatCompactTimeAgo`, localized.
pub fn compact_time_ago(iso: &str) -> String {
    let Ok(date) = DateTime::parse_from_rfc3339(iso) else {
        return String::new();
    };
    let date = date.with_timezone(&Utc);
    let elapsed = Utc::now().signed_duration_since(date);
    if elapsed.num_minutes() < 1 {
        return tr("shell.time.now");
    }
    if elapsed.num_hours() < 1 {
        return tr_args(
            "shell.time.minutes",
            &[("count", &elapsed.num_minutes().to_string())],
        );
    }
    if elapsed.num_days() < 1 {
        return tr_args(
            "shell.time.hours",
            &[("count", &elapsed.num_hours().to_string())],
        );
    }
    if elapsed.num_days() < 7 {
        return tr_args(
            "shell.time.days",
            &[("count", &elapsed.num_days().to_string())],
        );
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
fn draw_header(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    state: &paseo_gui_core::CoreState,
    r: Rect,
) -> f64 {
    let p = theme::current_palette();
    let compact = shell.data.compact;
    shell.painter.fill(cx, r, p.sidebar, 0.0);
    let preferences = j::object(state, "preferences").clone();
    let mut y = r.pos.y;
    let typography = theme::current_typography();
    let title_h = shell.painter.line_height(
        cx,
        crate::common::paint::TextSpec::new(Font::Regular, typography.ui_size(14.0), p.foreground),
    );
    let small_h = shell.painter.line_height(
        cx,
        crate::common::paint::TextSpec::new(Font::Regular, typography.ui_size(12.0), p.muted),
    );
    let nav_h = RowMetrics::new(title_h, 0.0, 0.0).height(32.0, 6.0, 16.0);
    // Phone: nav rows are 44px touch targets; desktop density is unchanged.
    let nav_h = if compact { nav_h.max(44.0) } else { nav_h };

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
            let row_w = if compact && index == 0 {
                r.size.x - 48.0
            } else {
                r.size.x
            };
            header_row(
                &mut ui,
                cx,
                rect(r.pos.x, y, row_w, nav_h),
                &format!("nav:{id}"),
                icon,
                &label,
            );
            if compact && index == 0 {
                let close = rect(
                    r.pos.x + r.size.x - 40.0,
                    y + (nav_h - 32.0) * 0.5,
                    32.0,
                    32.0,
                );
                ui.icon_button(
                    cx,
                    close,
                    "nav:close",
                    "x",
                    16.0,
                    16.0,
                    &tr("sidebar.actions.closeSidebar"),
                );
                // Phone: a 44px square at the row's trailing edge.
                ui.hit(rect(r.pos.x + r.size.x - 44.0, y, 44.0, nav_h), "nav:close");
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
        let heading = rect(r.pos.x + 8.0, y, r.size.x - 48.0, header_h);
        let label = tr("sessionNavigation.sessions");
        ui.text(cx, heading, spec, &label);
        ui.semantic_text(
            cx,
            heading,
            "sidebar:sessions",
            &label,
            AccessibilityRole::Header,
        );
        let trigger = rect(
            r.pos.x + r.size.x - 4.0 - 28.0,
            y + (header_h - 28.0) * 0.5,
            28.0,
            28.0,
        );
        if ui.is_hover("side:display") {
            let hover = ui.p.sidebar_hover;
            ui.fill(cx, trigger, hover, 6.0);
        }
        let muted = ui.p.muted;
        ui.icon(cx, trigger, "settings-2", 14.0, muted);
        ui.hit_tip(cx, trigger, "side:display", &tr("sidebar.display.trigger"));
        if compact {
            // Phone: the filter trigger takes a 44px square over the full header height.
            ui.hit(
                rect(r.pos.x + r.size.x - 44.0, y, 44.0, header_h),
                "side:display",
            );
        }
    }
    y += header_h;

    // Search field (bare input, 6px radius divider border).
    if shell.data.search.is_none() {
        let field = WidgetRef::new_from_ptr(cx, shell.search_field);
        field
            .as_ptext_field()
            .set_placeholder(cx, &tr("sessionNavigation.search"));
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
        shell
            .painter
            .fill(cx, rect(r.pos.x, y - 2.0, r.size.x, 2.0), p.accent, 0.0);
    }

    y
}

pub fn draw(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    state: &paseo_gui_core::CoreState,
    r: Rect,
) {
    let compact = shell.data.compact;
    if shell.data.sidebar.header_layer.begin(
        cx,
        r,
        0,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        shell.data.sidebar.header_bottom = draw_header(shell, cx, scope, state, r);
        shell
            .data
            .sidebar
            .header_layer
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
    let y = shell.data.sidebar.header_bottom;
    // Footer (8/6 padding, top border).
    let footer_h = if compact { 56.0 } else { 40.0 };
    let footer = rect(r.pos.x, r.pos.y + r.size.y - footer_h, r.size.x, footer_h);
    let list = rect(r.pos.x, y, r.size.x, (footer.pos.y - y).max(0.0));
    shell.data.sidebar.viewport = list;
    shell
        .painter
        .fill(cx, list, theme::current_palette().sidebar, 0.0);
    let inputs = crate::common::layer::inputs((
        shell.data.layout.selected().map(PanelTarget::key),
        &shell.data.sidebar.query,
        shell.data.sidebar.modifier_held,
    ));
    if shell.data.sidebar.tree_layer.begin(
        cx,
        list,
        inputs,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        let sidebar = &mut shell.data.sidebar;
        sidebar.tree_band = TreeBand::around(sidebar.scroll, list.size.y);
        let band = sidebar.tree_band.rect(list);
        shell
            .data
            .hits
            .set_viewport(Some(sidebar.tree_hits.clone()));
        draw_tree(shell, cx, state, list, band);
        shell.data.hits.set_viewport(None);
        shell
            .data
            .sidebar
            .tree_layer
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
    shell.data.sidebar.place_tree(cx);
    if shell.data.sidebar.footer_layer.begin(
        cx,
        footer,
        0,
        &mut shell.data.hits,
        &mut shell.data.tooltips,
    ) {
        shell
            .painter
            .fill(cx, footer, theme::current_palette().sidebar, 0.0);
        draw_footer(shell, cx, footer);
        shell
            .data
            .sidebar
            .footer_layer
            .end(cx, &shell.data.hits, &shell.data.tooltips);
    }
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
    ui.control(
        cx,
        body,
        id,
        label,
        AccessibilityRole::Button,
        AccessibilityState::default(),
    );
}

fn toolbar_button(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, id: &str, icon: &str, tooltip: &str) {
    // Row `ToolbarButton`: 20px hitbox, 6px radius, sm muted glyph.
    let r = rect(x, y, 20.0, 20.0);
    ui.icon_button(cx, r, id, icon, 14.0, 6.0, tooltip);
    if ui.compact {
        // Phone: a 44x44 touch target ending 8px past the glyph (the row's trailing inset),
        // so it stays inside the list clip.
        ui.hit(
            rect(x + 28.0 - 44.0, y + 10.0 - 22.0, 44.0, 44.0),
            id.to_string(),
        );
    }
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

/// The 1–9 keycap a held Ctrl/⌘ shows on the first sessions (`session.navigate.index`).
fn index_hint(ui: &mut Ui, cx: &mut Cx2d, slot: Rect, index: usize) {
    let p = ui.p;
    let r = rect(
        slot.pos.x + slot.size.x - 18.0,
        slot.pos.y + (slot.size.y - 18.0) * 0.5,
        18.0,
        18.0,
    );
    ui.painter.outline(cx, r, p.surface2, p.muted, 1.0, 4.0);
    let spec = ui.spec(Font::Medium, 11.0, p.foreground);
    ui.text_centered(cx, r, spec, &(index + 1).to_string());
}

fn draw_tree(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    state: &paseo_gui_core::CoreState,
    list: Rect,
    band: Rect,
) {
    let query = shell.data.sidebar.query.trim().to_lowercase();
    let host_count = hosts::hosts(state).len();
    // While the pointer rests on the tree, rows keep the order they were drawn in.
    let holding = pointer_on_tree(shell.data.pointer.hover.as_deref());
    let held = std::mem::take(&mut shell.data.sidebar.held);
    let tree = shell
        .data
        .sidebar
        .presentation
        .tree(state, &query, holding.then_some(&held));
    let mut drawn_order: HashMap<String, Vec<String>> = HashMap::new();
    let selected = shell.data.layout.selected().cloned();
    let hints = shell.data.sidebar.modifier_held && !shell.data.compact;
    let reveal = shell.data.sidebar.reveal_host.take();
    let mut reveal_offset: Option<f64> = None;
    // The rows read the streams while `ui` borrows the shell; they go back after drawing.
    let terminals = std::mem::take(&mut shell.data.sidebar.terminals);

    // Overscan can extend past the window's turtle. Isolate its alignment
    // so only the movable draw-list viewport clips it to the screen.
    cx.begin_pass_sized_turtle_no_clip(Layout::default());
    cx.begin_turtle(
        Walk::abs_rect(band),
        Layout {
            clip_x: true,
            clip_y: true,
            ..Layout::default()
        },
    );
    let mut ui = make_ui(shell);
    // Reserve the selection behind the tree. Icons can batch with an earlier
    // host glyph; status badges must still paint over those icons.
    let backdrop_group = ui.painter.new_layer(cx);
    ui.painter
        .draw_box
        .draw_vars
        .set_draw_call_group(live_id!(sidebar_foreground));
    let mut selected_row = None;
    ui.hits.set_clip(Some(band));
    let p = ui.p;
    let mut y = list.pos.y;
    let x0 = list.pos.x;
    let w = list.size.x;
    let group = ui.spec(Font::Medium, 14.0, p.muted);
    // Uniform line metrics keep culling O(1) per row without shaping invisible titles.
    let regular = ui.spec(Font::Regular, 14.0, p.foreground);
    let subtitle = ui.spec(Font::Regular, 12.0, p.muted);
    let title_h = ui
        .painter
        .line_height(cx, regular)
        .max(ui.painter.line_height(cx, group));
    let small_h = ui.painter.line_height(cx, subtitle);
    let group_metrics = RowMetrics::new(title_h, 0.0, 0.0);
    let session_metrics = RowMetrics::new(title_h, small_h, 2.0);
    let group_h = group_metrics.height(36.0, 8.0, 14.0);
    // Phone: host/project rows are 44px touch targets.
    let group_h = if ui.compact {
        group_h.max(44.0)
    } else {
        group_h
    };
    let session_h = session_metrics.height(0.0, 7.0, 14.0);
    let button_h = theme::control_height(ControlSize::Sm);
    let mut any = false;
    // Position in the session-switch order (Ctrl+1–9), counted like `navigation_sessions`.
    let mut nav_index = 0usize;
    for host_group in &tree.hosts {
        let host = hosts::host(state, &host_group.server_id).expect("cached sidebar host");
        let host = &host;
        let sid = host.server_id();
        let label = host.display_label();
        let host_matches = host_group.matches;
        let host_collapsed = host_group.collapsed;
        // Visible project blocks first (search may hide the host); live terminals follow the
        // directory's sessions and search matches their label.
        let workspaces = workspaces_by_project(host.workspaces().iter().map(Arc::as_ref));
        let mut blocks: Vec<(&ProjectGroup, Vec<TerminalRow>)> = Vec::new();
        for project in &host_group.projects {
            let project_workspaces = workspaces
                .get(j::text(&project.project, "projectId"))
                .map_or(&[][..], Vec::as_slice);
            let mut terms = terminal_rows(&terminals, sid, project_workspaces.iter().copied());
            if !query.is_empty() && !host_matches && !project.matches {
                terms.retain(|row| row.label.to_lowercase().contains(&query));
            }
            if !query.is_empty()
                && !host_matches
                && !project.matches
                && project.sessions.is_empty()
                && terms.is_empty()
            {
                continue;
            }
            drawn_order.insert(
                project.key.clone(),
                project
                    .sessions
                    .iter()
                    .map(|row| j::text(row.agent(), "id").to_string())
                    .collect(),
            );
            blocks.push((project, terms));
        }
        if !query.is_empty() && !host_matches && blocks.is_empty() {
            continue;
        }
        any = true;
        // Host row.
        y += 8.0;
        if reveal.as_deref() == Some(sid) {
            reveal_offset = Some((y - 8.0 - list.pos.y).max(0.0));
        }
        let row = rect(x0 + 8.0, y, w - 16.0, group_h);
        // A collapsed host still says how many of its sessions wait on the user.
        let waiting = if host_collapsed {
            host_group.waiting()
        } else {
            0
        };
        let badge_w = if waiting > 0 {
            waiting_badge_width(&mut ui, cx, waiting) + 4.0
        } else {
            0.0
        };
        let slots = group_metrics.layout(row, 0.0, 26.0, 20.0 + badge_w, 4.0);
        let mid = row.pos.y + row.size.y * 0.5;
        let plus_x = slots.trailing.pos.x + slots.trailing.size.x - 20.0;
        ui.icon(
            cx,
            rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0),
            if host_collapsed {
                "chevron-right"
            } else {
                "chevron-down"
            },
            14.0,
            p.muted,
        );
        ui.dot(
            cx,
            dvec2(slots.leading.pos.x + 22.0, mid),
            8.0,
            status_color(&p, host),
        );
        ui.text(cx, slots.title, group, &label);
        if waiting > 0 {
            waiting_badge(&mut ui, cx, plus_x - 4.0, mid, waiting);
        }
        // The whole row toggles; the trailing button, registered after it, wins its own square.
        ui.control(
            cx,
            row,
            &format!("host:{sid}"),
            &label,
            AccessibilityRole::ListItem,
            AccessibilityState::default(),
        );
        toolbar_button(
            &mut ui,
            cx,
            plus_x,
            mid - 10.0,
            &format!("hostplus:{sid}"),
            "plus",
            &tr("sessionNavigation.newSession"),
        );
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
            let lists: Vec<String> = loading
                .iter()
                .map(|k| tr(&format!("sidebar.refresh.{k}")))
                .collect();
            let text = tr_args("sidebar.refresh.loading", &[("lists", &lists.join(", "))]);
            ui.text(
                cx,
                rect(x0 + 32.0, y + 4.0, w - 40.0, small_h),
                small,
                &text,
            );
            y += small_h + 8.0;
        }
        let errors = host.list_errors();
        for (kind, error) in &errors {
            let text = tr_args(
                "sidebar.refresh.failed",
                &[
                    ("list", &tr(&format!("sidebar.refresh.{kind}"))),
                    ("error", error),
                ],
            );
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            ui.text(cx, rect(x0 + 32.0, y + 4.0, w - 40.0, small_h), spec, &text);
            y += small_h + 8.0;
        }
        if !errors.is_empty() {
            let label = tr("common.actions.retry");
            let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
            ui.button(
                cx,
                rect(x0 + 24.0, y, bw, button_h),
                &format!("hostretry:{sid}"),
                &label,
                None,
                Variant::Ghost,
                ControlSize::Sm,
                loading.is_empty(),
            );
            y += button_h + 4.0;
        }
        if let Some(error) = host.error() {
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            let tw = w - 40.0;
            let clipped: String = error.lines().take(3).collect::<Vec<_>>().join("\n");
            let h = ui.painter.paragraph_height(cx, tw, spec, &clipped);
            ui.painter
                .paragraph(cx, dvec2(x0 + 32.0, y), tw, spec, &clipped);
            y += h + 4.0;
            let retry = tr("common.actions.retry");
            let rw = ui.button_width(cx, &retry, None, ControlSize::Sm);
            ui.button(
                cx,
                rect(x0 + 24.0, y, rw, button_h),
                &format!("hostretry:{sid}"),
                &retry,
                None,
                Variant::Ghost,
                ControlSize::Sm,
                true,
            );
            let auth = tr("shell.sidebar.authConnection");
            let aw = ui
                .button_width(cx, &auth, None, ControlSize::Sm)
                .min((w - 24.0 - rw - 4.0 - 8.0).max(0.0));
            ui.button(
                cx,
                rect(x0 + 24.0 + rw + 4.0, y, aw, button_h),
                &format!("hostauth:{sid}"),
                &auth,
                None,
                Variant::Ghost,
                ControlSize::Sm,
                true,
            );
            y += button_h + 4.0;
        }
        for (project, terms) in &blocks {
            let key = &project.key;
            let name = j::text_or(
                &project.project,
                "projectDisplayName",
                j::text(&project.project, "projectRootPath"),
            );
            let name = if name.is_empty() {
                j::text(&project.project, "projectRootPath")
            } else {
                name
            };
            let row = rect(x0 + 24.0, y, w - 32.0, group_h);
            let waiting = if project.collapsed {
                project.waiting
            } else {
                0
            };
            let badge_w = if waiting > 0 {
                waiting_badge_width(&mut ui, cx, waiting) + 4.0
            } else {
                0.0
            };
            let slots = group_metrics.layout(row, 0.0, 32.0, 20.0 + badge_w, 4.0);
            let mid = row.pos.y + row.size.y * 0.5;
            let plus_x = slots.trailing.pos.x + slots.trailing.size.x - 20.0;
            ui.icon(
                cx,
                rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0),
                if project.collapsed {
                    "chevron-right"
                } else {
                    "chevron-down"
                },
                14.0,
                p.muted,
            );
            ui.icon(
                cx,
                rect(slots.leading.pos.x + 18.0, mid - 7.0, 14.0, 14.0),
                "folder",
                14.0,
                p.muted,
            );
            ui.text(cx, slots.title, group, name);
            if waiting > 0 {
                waiting_badge(&mut ui, cx, plus_x - 4.0, mid, waiting);
            }
            ui.control(
                cx,
                row,
                &format!("project:{key}"),
                name,
                AccessibilityRole::ListItem,
                AccessibilityState::default(),
            );
            toolbar_button(
                &mut ui,
                cx,
                plus_x,
                mid - 10.0,
                &format!("projplus:{key}"),
                "plus",
                &tr("sessionNavigation.newSession"),
            );
            y += group_h;
            if project.collapsed {
                continue;
            }
            for session in &project.sessions {
                let agent = session.agent();
                let index = nav_index;
                nav_index += 1;
                let row = rect(x0 + 48.0, y, w - 56.0, session_h);
                y += row.size.y;
                // Skip layout outside the retained strip, not at the transient viewport edge.
                if row.pos.y + row.size.y <= band.pos.y || row.pos.y >= band.pos.y + band.size.y {
                    continue;
                }
                let agent_id = j::text(agent, "id");
                let id = format!("session:{sid}:{agent_id}");
                let is_selected = selected.as_ref().is_some_and(|t| {
                    t.kind == "agent" && t.server_id == sid && t.id.as_deref() == Some(agent_id)
                });
                if is_selected {
                    selected_row = Some(row);
                }
                let hovered =
                    ui.is_hover(&id) || ui.is_hover(&format!("sessionedit:{sid}:{agent_id}"));
                let attention = j::boolean(agent, "requiresAttention", false);
                let title = j::text(agent, "title");
                let title = if title.is_empty() {
                    tr("sessionNavigation.untitled")
                } else {
                    title.to_string()
                };
                let hint = hints && index < 9;
                let trailing = if hint {
                    String::new()
                } else {
                    compact_time_ago(j::text_or(agent, "updatedAt", j::text(agent, "createdAt")))
                };
                let trailing_w = if hovered {
                    32.0
                } else if hint {
                    30.0
                } else if trailing.is_empty() {
                    12.0
                } else {
                    ui.measure(cx, ui.spec(Font::Regular, 12.0, p.muted), &trailing) + 16.0
                };
                let slots =
                    session_metrics.layout(row, 12.0, 14.0, (trailing_w - 12.0).max(0.0), 8.0);
                ui.icon(
                    cx,
                    slots.leading,
                    "message-square",
                    14.0,
                    if attention { p.foreground } else { p.muted },
                );
                let bucket = agent_live_status_bucket(agent);
                let backdrop = if is_selected { p.border } else { p.sidebar };
                let glyph = rect(
                    slots.leading.pos.x,
                    slots.leading.pos.y + (slots.leading.size.y - 14.0) * 0.5,
                    14.0,
                    14.0,
                );
                paint_status_badge(
                    ui.painter,
                    cx,
                    glyph,
                    Some(bucket).filter(|b| *b != StatusBucket::Done),
                    &p,
                    backdrop,
                );
                let title_spec = ui.spec(
                    if attention {
                        Font::Medium
                    } else {
                        Font::Regular
                    },
                    14.0,
                    p.foreground,
                );
                ui.text(cx, slots.title, title_spec, &title);
                let sub = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(
                    cx,
                    slots.subtitle,
                    sub,
                    &format!(
                        "{} · {}",
                        j::text(agent, "provider"),
                        agent_status_label(agent, bucket)
                    ),
                );
                ui.control(
                    cx,
                    row,
                    &id,
                    &title,
                    AccessibilityRole::ListItem,
                    AccessibilityState {
                        selected: is_selected,
                        ..AccessibilityState::default()
                    },
                );
                if hovered {
                    toolbar_button(
                        &mut ui,
                        cx,
                        slots.trailing.pos.x,
                        row.pos.y + (row.size.y - 20.0) * 0.5,
                        &format!("sessionedit:{sid}:{agent_id}"),
                        "pencil",
                        &tr("workspace.tabs.menu.renameAgent"),
                    );
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
                if row.pos.y + row.size.y <= band.pos.y || row.pos.y >= band.pos.y + band.size.y {
                    continue;
                }
                let is_selected = selected.as_ref().is_some_and(|t| {
                    t.kind == "terminal"
                        && t.server_id == sid
                        && t.id.as_deref() == Some(terminal.id.as_str())
                });
                if is_selected {
                    selected_row = Some(row);
                }
                let attention = !j::text(&terminal.activity, "attentionReason").is_empty();
                let trailing = if ui.compact { 36.0 } else { 20.0 };
                let slots = session_metrics.layout(row, 12.0, 14.0, trailing, 8.0);
                ui.icon(
                    cx,
                    slots.leading,
                    "square-terminal",
                    14.0,
                    if attention { p.foreground } else { p.muted },
                );
                let title_spec = ui.spec(
                    if attention {
                        Font::Medium
                    } else {
                        Font::Regular
                    },
                    14.0,
                    p.foreground,
                );
                ui.text(cx, slots.title, title_spec, &terminal.label);
                let sub = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(
                    cx,
                    slots.subtitle,
                    sub,
                    &format!(
                        "{} · {}",
                        tr("workspace.tabs.fallback.terminal"),
                        terminal_status(&terminal.activity)
                    ),
                );
                ui.control(
                    cx,
                    row,
                    &format!("sideterm:{sid}:{}", terminal.id),
                    &terminal.label,
                    AccessibilityRole::ListItem,
                    AccessibilityState {
                        selected: is_selected,
                        ..AccessibilityState::default()
                    },
                );
                toolbar_button(
                    &mut ui,
                    cx,
                    slots.trailing.pos.x + slots.trailing.size.x - 20.0,
                    row.pos.y + (row.size.y - 20.0) * 0.5,
                    &format!("sidetermmenu:{sid}:{}", terminal.id),
                    "ellipsis",
                    &tr("workspace.terminal.menu"),
                );
            }
        }
        let project_list_loading = host.loading()
            || loading.contains(&"project")
            || errors.iter().any(|(k, _)| *k == "project");
        if !host_group.has_projects && !project_list_loading && query.is_empty() {
            let row = rect(x0 + 24.0, y, w - 32.0, group_h);
            let slots = group_metrics.layout(row, 0.0, 32.0, 0.0, 4.0);
            let mid = row.pos.y + row.size.y * 0.5;
            ui.icon(
                cx,
                rect(slots.leading.pos.x + 18.0, mid - 7.0, 14.0, 14.0),
                "folder-plus",
                14.0,
                p.muted,
            );
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            let label = tr("sidebar.actions.addProject");
            ui.text(cx, slots.title, spec, &label);
            ui.hit_tip(cx, row, &format!("addproject:{sid}"), &label);
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
        let text_h = ui
            .painter
            .paragraph_height(cx, text_w, spec, &message)
            .max(title_h);
        ui.painter
            .paragraph(cx, dvec2(x0 + 16.0, y + 16.0), text_w, spec, &message);
        y += text_h + 24.0;
        // Archived sessions and older history are not in the tree: offer the History search.
        let (id, label, variant) = if host_count == 0 {
            ("side:addhost", tr("settings.addHost"), Variant::Outline)
        } else if !query.is_empty() {
            let raw: String = query.chars().take(40).collect();
            (
                "side:searchhistory",
                tr_args("shell.sidebar.searchHistory", &[("query", &raw)]),
                Variant::Outline,
            )
        } else {
            (
                "side:display",
                tr("sidebar.display.trigger"),
                Variant::Ghost,
            )
        };
        let bw = ui
            .button_width(cx, &label, None, ControlSize::Sm)
            .min(text_w);
        ui.button(
            cx,
            rect(x0 + 16.0, y + 8.0, bw, button_h),
            id,
            &label,
            None,
            variant,
            ControlSize::Sm,
            true,
        );
        y += button_h + 16.0;
    }
    ui.painter
        .draw_box
        .draw_vars
        .set_draw_call_group(backdrop_group);
    if let Some(row) = selected_row {
        ui.fill(cx, row, p.border, 6.0);
    }
    y += 8.0;
    ui.hits.set_clip(None);
    cx.end_turtle();
    cx.end_pass_sized_turtle_no_clip();
    shell.data.sidebar.held = drawn_order;
    shell.data.sidebar.terminals = terminals;
    shell.data.sidebar.content_height = y - list.pos.y;
    let max = (shell.data.sidebar.content_height - list.size.y).max(0.0);
    let sidebar = &mut shell.data.sidebar;
    sidebar.scroll = reveal_offset.unwrap_or(sidebar.scroll).min(max);
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
    let button =
        |ui: &mut Ui, cx: &mut Cx2d, x: f64, id: &str, glyph: &str, size: f64, tooltip: &str| {
            let b = rect(x, y, extent, extent);
            if ui.is_pressed(id) {
                let h = ui.p.interaction_highlight;
                ui.fill(cx, b, h, extent * 0.5);
            }
            let color = if ui.is_hover(id) {
                ui.p.foreground
            } else {
                ui.p.muted
            };
            ui.icon(cx, b, glyph, size, color);
            ui.hit_tip(cx, b, id, tooltip);
        };
    let mut x = r.pos.x + 8.0;
    button(
        &mut ui,
        cx,
        x,
        "foot:addproject",
        "folder-plus",
        icon,
        &tr("sidebar.actions.addProject"),
    );
    x += extent;
    button(
        &mut ui,
        cx,
        x,
        "foot:usage",
        "circle-gauge",
        icon,
        &tr("sidebar.footer.usage"),
    );
    x += extent;
    button(
        &mut ui,
        cx,
        x,
        "foot:hosts",
        "server",
        icon - 1.0,
        &tr("sidebar.actions.hosts"),
    );
    let right = r.pos.x + r.size.x - 8.0;
    button(
        &mut ui,
        cx,
        right - extent,
        "foot:settings",
        "settings",
        icon,
        &crate::common::tooltip_with_shortcut(&tr("sidebar.actions.settings"), "toggle-settings"),
    );
    button(
        &mut ui,
        cx,
        right - extent * 2.0,
        "foot:help",
        "circle-help",
        icon,
        &tr("sidebar.help.trigger"),
    );
}

pub fn scroll(shell: &mut PaseoShell, cx: &mut Cx, delta: f64) {
    let s = &mut shell.data.sidebar;
    s.stop_motion();
    if s.scroll_by(delta) {
        s.place_scrolled_tree(cx);
    }
}

pub fn touch_up(shell: &mut PaseoShell, cx: &mut Cx, time: f64) -> bool {
    let motion = &mut shell.data.sidebar.motion;
    let panned = motion.release(time);
    motion.schedule(cx);
    panned
}

pub fn animate(shell: &mut PaseoShell, cx: &mut Cx, event: &Event) {
    shell.data.sidebar.advance(cx, event);
}

/// Search field changes (Event::Actions).
pub fn handle_actions(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, _ui: &mut UiContext) {
    let Event::Actions(actions) = event else {
        return;
    };
    let Some(field) = shell.data.search.clone() else {
        return;
    };
    if let Some(text) = field.as_ptext_field().changed(actions) {
        shell.data.sidebar.stop_motion();
        shell.data.sidebar.query = text.trim().to_lowercase();
        shell.data.sidebar.scroll = 0.0;
        shell.data.sidebar.place_tree(cx);
        shell.data.sidebar.tree_layer.invalidate(cx);
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
    let modifier = if cfg!(target_os = "macos") {
        KeyCode::Logo
    } else {
        KeyCode::Control
    };
    let held = match event {
        Event::KeyDown(key) => {
            key.key_code == modifier && !key.modifiers.shift && !key.modifiers.alt
        }
        Event::KeyUp(_)
        | Event::AppLostFocus
        | Event::Pause
        | Event::Background
        | Event::MouseDown(_) => false,
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
    ui.push(UiEffect::SetPreference {
        key: "navigation".into(),
        value: prefs.clone(),
    });
}

/// `_move`: reorder within `hostOrder`/`projectOrder`, keeping unrelated stored keys.
pub fn move_in_order(ui: &mut UiContext, field: &str, keys: &[String], key: &str, delta: i64) {
    let Some(index) = keys.iter().position(|k| k == key) else {
        return;
    };
    let destination = index as i64 + delta;
    if destination < 0 || destination >= keys.len() as i64 {
        return;
    }
    let mut keys = keys.to_vec();
    let item = keys.remove(index);
    keys.insert(destination as usize, item);
    let mut prefs = nav_prefs(&ui.state);
    let unrelated: Vec<String> = strings(&prefs, field)
        .into_iter()
        .filter(|k| !keys.contains(k))
        .collect();
    keys.extend(unrelated);
    set_nav(ui, &mut prefs, field, json!(keys));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_fling_composes_retained_tree_and_hits_until_overscan_is_exhausted() {
        use crate::common::paint::HitMap;
        use crate::common::transition;
        use makepad_widgets::makepad_draw::CxDraw;

        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::makepad_draw::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        crate::theme::DrawBox::register_main_module(&mut cx);
        cx.live_expand();
        let mut background = crate::theme::DrawBox::new_local(&mut cx);
        let viewport = rect(0.0, 30.0, 100.0, 100.0);
        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(100.0, 200.0));
        let mut window = DrawList2d::new(&mut cx);
        let mut s = SidebarState {
            content_height: 1000.0,
            viewport,
            tree_band: TreeBand::around(0.0, viewport.size.y),
            motion: crate::common::widgets::touch_scroll(),
            ..Default::default()
        };
        let mut hits = HitMap::default();
        let mut tips = HashMap::new();
        let row = "newly revealed row".to_owned();
        {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(&mut cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            window.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            assert!(
                s.tree_layer
                    .begin(&mut cx, viewport, 0, &mut hits, &mut tips)
            );
            // The row is below the viewport initially, but inside the retained band.
            hits.set_viewport(Some(s.tree_hits.clone()));
            let bounds = rect(0.0, 155.0, 100.0, 10.0);
            hits.push(bounds, row.clone());
            hits.set_viewport(None);
            background.paint(&mut cx, bounds, vec4(1.0, 1.0, 1.0, 1.0), 0.0);
            s.tree_layer.end(&mut cx, &hits, &tips);
            s.place_tree(&mut cx);
            cx.end_pass_sized_turtle();
            window.end(&mut cx);
            cx.end_pass(&pass);
        }
        let area = background.area();
        let row_y = 155.0;
        assert!(hits.rect_of(&row).is_none());
        cx.new_draw_event = DrawEvent::default();
        s.touch_down(dvec2(10.0, 90.0), 0.0);
        assert!(s.touch_move(&mut cx, dvec2(10.0, 40.0), 0.05));
        assert!(s.motion.release(0.06));
        s.motion.schedule(&mut cx);
        for time in [10.0, 10.016, 10.032, 10.048] {
            let event = Event::NextFrame(NextFrameEvent {
                frame: 0,
                time,
                set: [transition::request_frame(&mut cx)].into_iter().collect(),
            });
            transition::begin_event(&event);
            s.advance(&mut cx, &event);
            let hit = hits.rect_of(&row).unwrap();
            assert_eq!(hits.at(hit.center()), Some(row.clone()));
            assert!((hit.pos.y - (row_y - s.scroll)).abs() < 0.001);
            assert!((area.clipped_rect(&cx).pos.y - hit.pos.y).abs() < 0.001);
            assert!(
                !cx.new_draw_event.will_redraw(),
                "scroll requested tree paint"
            );
        }
        assert!(s.scroll > 50.0);
        assert!(s.scroll_by(300.0));
        s.place_tree(&mut cx);
        assert!(cx.new_draw_event.will_redraw(), "new strip needs one paint");
        assert!(hits.rect_of(&row).is_none());
    }

    fn published(mut value: Value) -> paseo_gui_core::CoreState {
        use paseo_gui_core::{CoreState, DirectorySnapshot};
        use std::sync::Arc;
        let mut directories = HashMap::new();
        for (index, host) in value["hosts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            let key = index as u64 + 1;
            let record = host.as_object_mut().unwrap();
            let mut rows = |kind: &str| match record.remove(kind) {
                Some(Value::Array(rows)) => rows.into_iter().map(Arc::new).collect(),
                _ => Vec::new(),
            };
            let directory = DirectorySnapshot {
                revision: key,
                projects: Arc::new(rows("projects")),
                workspaces: Arc::new(rows("workspaces")),
                agents: rows("agents").into_iter().collect(),
                cached_at: None,
            };
            record.insert("directoryKey".into(), key.into());
            record.insert("directoryRevision".into(), key.into());
            directories.insert(key, Arc::new(directory));
        }
        let mut state = CoreState::new(value);
        state.directories = Arc::new(directories);
        state
    }

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
        assert!(!workspace_matches_labels(
            &json!({"labels": ["Bug"]}),
            &filters
        ));
        assert!(workspace_matches_labels(
            &json!({"labels": ["  Bug   fix "]}),
            &["bug fix".to_string()]
        ));
    }

    #[test]
    fn nav_order_keeps_stored_order_and_appends_missing_defaults() {
        let prefs =
            json!({"sidebarNavItems": [{"key": "search"}, {"key": "history", "visible": false}]});
        assert_eq!(
            nav_order(&prefs),
            vec!["search", "new-workspace", "schedules"]
        );
    }

    #[test]
    fn perf_workspace_buckets_match_the_old_project_filter() {
        let workspaces = [
            json!({"id": "p-local", "projectId": "p"}),
            json!({"id": "q-remote", "projectId": "q"}),
            json!({"id": "p-remote", "projectId": "p", "background": true}),
            json!({"id": "unassigned"}),
        ];
        let mut visited = 0;
        let buckets = workspaces_by_project(workspaces.iter().inspect(|_| visited += 1));
        for project in ["p", "q", "", "empty"] {
            let old: Vec<_> = workspaces
                .iter()
                .filter(|workspace| j::text(workspace, "projectId") == project)
                .collect();
            assert_eq!(
                buckets.get(project).map_or(&[][..], Vec::as_slice),
                old.as_slice()
            );
        }
        assert_eq!(visited, workspaces.len());
    }

    #[test]
    fn terminal_merge_keeps_first_seen_order_and_fresh_fields() {
        let previous = vec![json!({"id": "a", "name": "one"}), json!({"id": "b"})];
        let listed = vec![
            json!({"id": "c"}),
            json!({"id": "a", "name": "renamed"}),
            json!({"id": ""}),
        ];
        let merged = merge_terminals(&previous, &listed, None);
        assert_eq!(
            merged,
            vec![json!({"id": "a", "name": "renamed"}), json!({"id": "c"})]
        );
    }

    #[test]
    fn terminal_termination_removes_sidebar_row_before_directory_refresh() {
        let closed = json!({"id": "closed"});
        let live = json!({"id": "live"});
        let stale_snapshot = vec![closed.clone(), live.clone()];
        let mut displayed = stale_snapshot.clone();
        // A rejection adds no retirement and leaves the same live rows.
        assert_eq!(
            merge_terminals(&displayed, &stale_snapshot, None),
            displayed
        );

        let retired = HashSet::from(["closed".to_owned()]);
        assert!(retire_terminal_rows(&mut displayed, &retired));
        assert_eq!(displayed, vec![live]);
        assert_eq!(
            merge_terminals(&displayed, &stale_snapshot, Some(&retired)),
            displayed,
        );
    }

    #[test]
    fn terminal_directories_follow_connection_and_sidebar_filters() {
        let workspace = |id: &str, project: &str, extra: Value| {
            let mut w =
                json!({"id": id, "projectId": project, "workspaceDirectory": format!("/w/{id}")});
            w.as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap_or_default());
            w
        };
        let state = published(json!({
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
        }));
        assert_eq!(
            terminal_directories(&state),
            vec![("on".to_string(), "a".to_string(), "/w/a".to_string())]
        );
    }

    #[test]
    fn terminal_labels_prefer_title_then_a_given_name() {
        assert_eq!(
            terminal_label(&json!({"title": " vim ", "name": "dev"})),
            "vim"
        );
        assert_eq!(terminal_label(&json!({"name": "dev"})), "dev");
        assert_eq!(
            terminal_label(&json!({"name": "Terminal"})),
            tr("workspace.tabs.fallback.terminal")
        );
    }

    #[test]
    fn session_status_puts_needed_input_before_activity() {
        let label = |agent: Value| agent_status_label(&agent, agent_live_status_bucket(&agent));
        assert_eq!(
            label(json!({"status": "running", "pendingPermissions": [{"id": "q"}]})),
            tr("sidebar.statusBucket.needsInput")
        );
        assert_eq!(
            label(
                json!({"status": "error", "requiresAttention": true, "attentionReason": "error"})
            ),
            tr("sidebar.statusBucket.failed")
        );
        assert_eq!(
            label(json!({"status": "idle", "activeTurn": {"id": "t"}})),
            tr("sidebar.statusBucket.working")
        );
        assert_eq!(
            label(
                json!({"status": "idle", "requiresAttention": true, "attentionReason": "finished"})
            ),
            tr("sidebar.statusBucket.readyToReview")
        );
        assert_eq!(
            label(json!({"status": "idle"})),
            tr("agentList.status.idle")
        );
    }

    fn tree_state(navigation: Value) -> paseo_gui_core::CoreState {
        let agent = |id: &str, ws: &str, updated: &str, extra: Value| {
            let mut a =
                json!({"id": id, "workspaceId": ws, "updatedAt": updated, "status": "idle"});
            a.as_object_mut()
                .unwrap()
                .extend(extra.as_object().cloned().unwrap_or_default());
            a
        };
        published(json!({
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
        }))
    }

    fn ids(list: &[(String, Value)]) -> Vec<String> {
        list.iter()
            .map(|(_, a)| j::text(a, "id").to_string())
            .collect()
    }

    #[test]
    fn sidebar_model_reuses_unchanged_inputs_and_rebuilds_on_directory_revision() {
        let mut state = tree_state(json!({"collapsed": ["host:a", "project:b:q"]}));
        let mut cache = PresentationCache::default();
        let initial = cache.tree(&state, "", None);
        assert!(Arc::ptr_eq(&initial, &cache.tree(&state, "", None)));
        assert!(initial.touches_activity(&HashSet::from([("b".into(), "old".into())])));
        assert!(!initial.touches_activity(&HashSet::from([
            ("a".into(), "first".into()),
            ("b".into(), "alpha".into()),
        ])));

        let directory = Arc::make_mut(Arc::make_mut(&mut state.directories).get_mut(&1).unwrap());
        directory.revision += 1;
        directory.agents = directory
            .agents
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let mut row = row.clone();
                if index == 0 {
                    Arc::make_mut(&mut row)["updatedAt"] = "2027-01-01T00:00:00Z".into();
                }
                row
            })
            .collect();
        let updated = cache.tree(&state, "", None);
        assert!(!Arc::ptr_eq(&initial, &updated));
        assert!(Arc::ptr_eq(&updated, &cache.tree(&state, "", None)));
        let project = updated
            .hosts
            .iter()
            .find(|host| host.server_id == "b")
            .unwrap()
            .projects
            .iter()
            .find(|project| project.key == "project:b:p")
            .unwrap();
        let order: Vec<&str> = project
            .sessions
            .iter()
            .map(|row| j::text(row.agent(), "id"))
            .collect();
        assert_eq!(order, ["asks", "old", "new"]);

        state.value_mut()["preferences"]["navigation"]["collapsed"] = json!([]);
        let expanded = cache.tree(&state, "", None);
        assert!(!Arc::ptr_eq(&updated, &expanded));
        assert!(expanded.touches_activity(&HashSet::from([("a".into(), "first".into())])));
        let searched = cache.tree(&state, "alp", None);
        assert!(!Arc::ptr_eq(&expanded, &searched));
        assert!(searched.touches_activity(&HashSet::from([("b".into(), "alpha".into())])));
        assert!(!searched.touches_activity(&HashSet::from([("b".into(), "old".into())])));
        let held = HashMap::from([("project:b:p".to_string(), vec!["old".to_string()])]);
        let holding = cache.tree(&state, "", Some(&held));
        assert!(!Arc::ptr_eq(&searched, &holding));
        assert!(Arc::ptr_eq(&holding, &cache.tree(&state, "", Some(&held))));
    }

    #[test]
    fn navigation_follows_sidebar_order_filters_and_collapse() {
        let state = tree_state(json!({"hostOrder": ["a", "b"]}));
        // Host order, project name order, waiting sessions pinned, then newest first.
        assert_eq!(
            ids(&navigation_sessions(&state, "", None, false)),
            ["first", "alpha", "asks", "new", "old"]
        );
        let collapsed =
            tree_state(json!({"hostOrder": ["a", "b"], "collapsed": ["host:a", "project:b:q"]}));
        assert_eq!(
            ids(&navigation_sessions(&collapsed, "", None, false)),
            ["asks", "new", "old"]
        );
        assert_eq!(
            ids(&navigation_sessions(&collapsed, "", None, true)).len(),
            5
        );
        let filtered = tree_state(json!({"hostFilters": ["b"], "projectFilters": ["project:b:p"]}));
        assert_eq!(
            ids(&navigation_sessions(&filtered, "", None, false)),
            ["asks", "new", "old"]
        );
        assert_eq!(
            ids(&navigation_sessions(&state, "alp", None, false)),
            ["alpha"]
        );
    }

    #[test]
    fn held_order_keeps_drawn_rows_and_puts_new_ones_first() {
        let state = tree_state(json!({}));
        let mut held = HashMap::new();
        held.insert(
            "project:b:p".to_string(),
            vec!["old".to_string(), "asks".to_string()],
        );
        let order = ids(&navigation_sessions(&state, "", Some(&held), false));
        let p: Vec<&str> = order
            .iter()
            .map(String::as_str)
            .filter(|id| ["old", "new", "asks"].contains(id))
            .collect();
        assert_eq!(p, ["new", "old", "asks"]);
    }

    #[test]
    fn collapsed_groups_count_waiting_sessions() {
        let state = tree_state(json!({"collapsed": ["host:b"]}));
        let tree = session_tree(&state, "", None);
        let b = tree.hosts.iter().find(|g| g.server_id == "b").unwrap();
        assert!(b.collapsed);
        assert_eq!(b.waiting(), 1);
        assert!(others_need_input(&state, None));
        let focused = PanelTarget::new("agent", "b", "wp", "").with_id("asks");
        assert!(!others_need_input(&state, Some(&focused)));
    }
}
