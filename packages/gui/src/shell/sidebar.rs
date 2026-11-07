//! Port of `features/navigation/session_sidebar.dart` and the shell's sidebar column
//! (`_sidebarNavigation`, `_sidebar` footer): hosts → projects → sessions with status, search,
//! collapse state, row actions and the footer buttons.

use std::cmp::Ordering;

use chrono::{DateTime, Datelike, Local, Utc};
use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::paint::{Font, rect};
use crate::common::widgets::{PTextFieldWidgetRefExt, RowMetrics};
use crate::common::{UiContext, UiEffect, json as j, tr, tr_args};
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

/// `_sessions`: a project's non-archived top-level agents in workspaces the filters keep,
/// newest activity, then newest created, then id.
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
        time_ms(b, "updatedAt")
            .cmp(&time_ms(a, "updatedAt"))
            .then_with(|| time_ms(b, "createdAt").cmp(&time_ms(a, "createdAt")))
            .then_with(|| j::text(a, "id").cmp(j::text(b, "id")))
    });
    out
}

/// `formatCompactTimeAgo`.
pub fn compact_time_ago(iso: &str) -> String {
    let Ok(date) = DateTime::parse_from_rfc3339(iso) else { return String::new() };
    let date = date.with_timezone(&Utc);
    let elapsed = Utc::now().signed_duration_since(date);
    if elapsed.num_minutes() < 1 {
        return "now".into();
    }
    if elapsed.num_hours() < 1 {
        return format!("{}m", elapsed.num_minutes());
    }
    if elapsed.num_days() < 1 {
        return format!("{}h", elapsed.num_hours());
    }
    if elapsed.num_days() < 7 {
        return format!("{}d", elapsed.num_days());
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = date.with_timezone(&Local);
    format!("{} {}", MONTHS[local.month0() as usize], local.day())
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

fn draw_tree(shell: &mut PaseoShell, cx: &mut Cx2d, state: &Value, list: Rect) {
    let query = shell.data.sidebar.query.trim().to_lowercase();
    let prefs = nav_prefs(state);
    let collapsed = strings(&prefs, "collapsed");
    let host_order = strings(&prefs, "hostOrder");
    let project_order = strings(&prefs, "projectOrder");
    let trailing_diff = j::text(j::object(state, "preferences"), "sidebarWorkspaceTrailing") == "diff";
    let mut all = hosts::hosts(state);
    let order_index = |id: &str| host_order.iter().position(|h| h == id).unwrap_or(host_order.len());
    all.sort_by_key(|h| order_index(h.server_id()));
    let hf = host_filters(state, &prefs);
    let pf = project_filters(state, &prefs);
    let selected = shell.data.layout.selected().cloned();
    let scroll = shell.data.sidebar.scroll;
    let compact = shell.data.compact;

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
    for host in &all {
        if !hf.is_empty() && !hf.iter().any(|h| h == host.server_id()) {
            continue;
        }
        let sid = host.server_id();
        let label = host.display_label();
        let host_matches = label.to_lowercase().contains(&query);
        let host_collapsed = query.is_empty() && collapsed.iter().any(|c| *c == format!("host:{sid}"));
        let projects = ordered_projects(host, &project_order);
        // Build visible project blocks first (search may hide the host).
        let mut blocks: Vec<(Value, String, bool, Vec<&Value>)> = Vec::new();
        for project in &projects {
            let key = project_key(host, project);
            if !pf.is_empty() && !pf.contains(&key) {
                continue;
            }
            let project_matches = format!("{} {}", j::text(project, "projectDisplayName"), j::text(project, "projectRootPath")).to_lowercase().contains(&query);
            let rows: Vec<&Value> = sessions(host, project, &prefs)
                .into_iter()
                .filter(|agent| {
                    query.is_empty() || host_matches || project_matches || format!("{} {}", j::text(agent, "title"), j::text(agent, "provider")).to_lowercase().contains(&query)
                })
                .collect();
            if !query.is_empty() && !host_matches && !project_matches && rows.is_empty() {
                continue;
            }
            let project_collapsed = query.is_empty() && collapsed.contains(&key);
            blocks.push((project.clone(), key, project_collapsed, rows));
        }
        if !query.is_empty() && !host_matches && blocks.is_empty() {
            continue;
        }
        any = true;
        // Host row.
        y += 8.0;
        let row = rect(x0 + 8.0, y, w - 16.0, group_h);
        let slots = group_metrics.layout(row, 0.0, 26.0, 20.0, 4.0);
        let mid = row.pos.y + row.size.y * 0.5;
        ui.icon(cx, rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0), if host_collapsed { "chevron-right" } else { "chevron-down" }, 14.0, p.muted);
        ui.dot(cx, dvec2(slots.leading.pos.x + 22.0, mid), 8.0, status_color(&p, host));
        ui.text(cx, slots.title, group, &label);
        ui.hit(rect(row.pos.x, row.pos.y, row.size.x - 24.0, row.size.y), format!("host:{sid}"));
        toolbar_button(&mut ui, cx, slots.trailing.pos.x, mid - 10.0, &format!("hostplus:{sid}"), "plus", &tr("sessionNavigation.newSession"));
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
            let spec = ui.spec(Font::Regular, 12.0, p.destructive);
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
            let spec = ui.spec(Font::Regular, 12.0, p.destructive);
            let tw = w - 40.0;
            let clipped: String = error.lines().take(3).collect::<Vec<_>>().join("\n");
            let h = ui.painter.paragraph_height(cx, tw, spec, &clipped);
            ui.painter.paragraph(cx, dvec2(x0 + 32.0, y), tw, spec, &clipped);
            y += h + 4.0;
            let retry = tr("common.actions.retry");
            let rw = ui.button_width(cx, &retry, None, ControlSize::Sm);
            ui.button(cx, rect(x0 + 24.0, y, rw, button_h), &format!("hostretry:{sid}"), &retry, None, Variant::Ghost, ControlSize::Sm, true);
            let auth = "Authentication / connection";
            let aw = ui.button_width(cx, auth, None, ControlSize::Sm);
            ui.button(cx, rect(x0 + 24.0 + rw + 4.0, y, aw, button_h), &format!("hostauth:{sid}"), auth, None, Variant::Ghost, ControlSize::Sm, true);
            y += button_h + 4.0;
        }
        for (project, key, project_collapsed, rows) in &blocks {
            let name = j::text_or(project, "projectDisplayName", j::text(project, "projectRootPath"));
            let name = if name.is_empty() { j::text(project, "projectRootPath") } else { name };
            let row = rect(x0 + 24.0, y, w - 32.0, group_h);
            let slots = group_metrics.layout(row, 0.0, 32.0, 20.0, 4.0);
            let mid = row.pos.y + row.size.y * 0.5;
            ui.icon(cx, rect(slots.leading.pos.x, mid - 7.0, 14.0, 14.0), if *project_collapsed { "chevron-right" } else { "chevron-down" }, 14.0, p.muted);
            ui.icon(cx, rect(slots.leading.pos.x + 18.0, mid - 7.0, 14.0, 14.0), "folder", 14.0, p.muted);
            ui.text(cx, slots.title, group, name);
            ui.hit(rect(row.pos.x, row.pos.y, row.size.x - 24.0, row.size.y), format!("project:{key}"));
            toolbar_button(&mut ui, cx, slots.trailing.pos.x, mid - 10.0, &format!("projplus:{key}"), "plus", &tr("sessionNavigation.newSession"));
            y += group_h;
            if *project_collapsed {
                continue;
            }
            for agent in rows {
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
                let trailing = if trailing_diff { String::new() } else { compact_time_ago(j::text_or(agent, "updatedAt", j::text(agent, "createdAt"))) };
                let trailing_w = if hovered { 32.0 } else if trailing.is_empty() { 12.0 } else { ui.measure(cx, ui.spec(Font::Regular, 12.0, p.extra_muted), &trailing) + 16.0 };
                let slots = session_metrics.layout(row, 12.0, 14.0, (trailing_w - 12.0).max(0.0), 8.0);
                ui.icon(cx, slots.leading, "message-square", 14.0, if attention { p.foreground } else { p.muted });
                let title_spec = ui.spec(if attention { Font::Medium } else { Font::Regular }, 14.0, p.foreground);
                ui.text(cx, slots.title, title_spec, &title);
                let status = j::text(agent, "status");
                let status_key = format!("agentList.status.{status}");
                let status_label = tr(&status_key);
                let status_label = if status_label == status_key { status.to_string() } else { status_label };
                let sub = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, slots.subtitle, sub, &format!("{} · {}", j::text(agent, "provider"), status_label));
                ui.hit(rect(row.pos.x, row.pos.y, row.size.x - if hovered { 32.0 } else { 0.0 }, row.size.y), id.clone());
                if hovered {
                    toolbar_button(&mut ui, cx, slots.trailing.pos.x, row.pos.y + (row.size.y - 20.0) * 0.5, &format!("sessionedit:{sid}:{agent_id}"), "pencil", &tr("workspace.tabs.menu.renameAgent"));
                } else if !trailing.is_empty() {
                    let spec = ui.spec(Font::Regular, 12.0, p.extra_muted);
                    ui.text_right(cx, slots.trailing, spec, &trailing);
                }
            }
        }
        let project_list_loading = host.loading() || loading.contains(&"project") || errors.iter().any(|(k, _)| *k == "project");
        if projects.is_empty() && !project_list_loading && query.is_empty() {
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
        let message = if all.is_empty() { "Connect to a Paseo host to see your sessions.".to_string() } else { tr("sessionNavigation.empty") };
        ui.text(cx, rect(x0 + 16.0, y + 16.0, w - 32.0, title_h), spec, &message);
        y += title_h + 24.0;
        let (id, label, variant) = if all.is_empty() {
            ("side:addhost", tr("settings.addHost"), Variant::Outline)
        } else {
            ("side:display", tr("sidebar.display.trigger"), Variant::Ghost)
        };
        let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
        ui.button(cx, rect(x0 + 16.0, y + 8.0, bw, button_h), id, &label, None, variant, ControlSize::Sm, true);
        y += button_h + 16.0;
    }
    y += 8.0;
    ui.hits.set_clip(None);
    let _ = compact;
    cx.end_turtle();
    shell.data.sidebar.content_height = y + scroll - list.pos.y;
    let max = (shell.data.sidebar.content_height - list.size.y).max(0.0);
    if shell.data.sidebar.scroll > max {
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
    button(&mut ui, cx, right - extent, "foot:settings", "settings", icon, &tr("sidebar.actions.settings"));
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
}
