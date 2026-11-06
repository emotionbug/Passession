//! Port of `features/navigation/session_history_dialog.dart`: the History screen (original
//! `screens/sessions-screen.tsx`, shown as a shell page) with search, host filter, per-host
//! load errors, date sections, paginated `fetch_agent_history_request` across reachable
//! hosts and long-press archive; plus the import-session sheet
//! (`components/import-session-sheet.tsx`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Datelike, Local, TimeZone, Utc};
use makepad_widgets::*;
use paseo_gui_core::{CoreError, RequestId};
use serde_json::{Value, json};

use crate::common::overlay::{SelectOption, SelectRequest};
use crate::common::paint::{Font, TextSpec, rect};
use crate::common::provider_icon::ProviderIcon;
use crate::common::{TabPlacement, UiContext, json as j, tr, tr_args};
use crate::features::command_center::matching::highlight_ranges;
use crate::shell::chrome::make_ui;
use crate::shell::commands::{self, ShellOverlay};
use crate::shell::hosts::{self, Host, Status};
use crate::shell::sheets::{Field, FieldEvent, Sheet, SheetFlow, SheetOps, SheetStyle, spinner};
use crate::shell::ui::{Ui, Variant};
use crate::shell::{PaseoShell, Pending, response_message, sidebar};
use crate::theme::{ControlSize, argb, with_alpha};

/// Pseudo sheet id the page's requests are registered under: no sheet carries it, so
/// `sheets::route_response` hands them to [`on_response`].
const PAGE_SHEET: u64 = 0;
/// `AGENT_HISTORY_PAGE_LIMIT`.
const HISTORY_PAGE_LIMIT: i64 = 200;
/// Long enough that a typed word is one request, short enough to feel live.
const SEARCH_DEBOUNCE: f64 = 0.2;
/// Original palette red-300: the host-errors banner text.
const RED_300: u32 = 0xfffca5a5;

fn parse_ms(text: &str) -> Option<i64> {
    (!text.is_empty()).then(|| DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis())).flatten()
}

/// Original `describeTimeAgo`: never sub-minute, then the date itself once a week old.
fn time_ago(at: Option<i64>) -> String {
    let Some(at) = at else { return String::new() };
    let elapsed = Utc::now().timestamp_millis() - at;
    if elapsed < 60_000 {
        return "just now".into();
    }
    if elapsed < 3_600_000 {
        return format!("{}m ago", elapsed / 60_000);
    }
    if elapsed < 86_400_000 {
        return format!("{}h ago", elapsed / 3_600_000);
    }
    if elapsed < 7 * 86_400_000 {
        return format!("{}d ago", elapsed / 86_400_000);
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    Local.timestamp_millis_opt(at).single().map(|t| format!("{} {}", MONTHS[t.month0() as usize], t.day())).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DateSection {
    Today,
    Yesterday,
    ThisWeek,
    ThisMonth,
    Older,
}

const SECTIONS: [(DateSection, &str); 5] = [
    (DateSection::Today, "today"),
    (DateSection::Yesterday, "yesterday"),
    (DateSection::ThisWeek, "thisWeek"),
    (DateSection::ThisMonth, "thisMonth"),
    (DateSection::Older, "older"),
];

/// Original `deriveDateSectionKey`: calendar days, not elapsed time.
fn date_section(at: Option<i64>) -> DateSection {
    let Some(at) = at else { return DateSection::Older };
    let Some(local) = Local.timestamp_millis_opt(at).single() else { return DateSection::Older };
    let today = Local::now().date_naive();
    let day = local.date_naive();
    if day >= today {
        return DateSection::Today;
    }
    if day >= today.pred_opt().unwrap_or(today) {
        return DateSection::Yesterday;
    }
    let days = (today - day).num_days();
    if days <= 7 {
        DateSection::ThisWeek
    } else if days <= 30 {
        DateSection::ThisMonth
    } else {
        DateSection::Older
    }
}

/// One history agent: its host, the daemon's agent record (with its project placement under
/// `project`) and when it was last active.
#[derive(Clone)]
struct HistoryAgent {
    server_id: String,
    agent: Value,
    last_activity: Option<i64>,
}

impl HistoryAgent {
    fn new(server_id: String, agent: Value) -> Self {
        let last = j::text(&agent, "lastActivityAt");
        let last_activity = parse_ms(if last.is_empty() { j::text(&agent, "updatedAt") } else { last });
        Self { server_id, agent, last_activity }
    }

    fn key(&self) -> String {
        format!("{}:{}", self.server_id, j::text(&self.agent, "id"))
    }
}

/// One in-flight `fetchAgentHistoryBatch`.
struct Batch {
    generation: u64,
    append: bool,
    search: String,
    unreachable: Vec<(String, String)>,
    pending: HashSet<RequestId>,
    results: Vec<(String, String, Option<Value>)>,
}

pub struct HistoryPage {
    cwd: String,
    project_key: String,
    /// `""` = All hosts.
    selected_host: String,
    search_field: Field,
    search: String,
    debounce: Timer,
    clock: Timer,
    agents: Vec<HistoryAgent>,
    cursors: HashMap<String, String>,
    host_errors: Vec<(String, String)>,
    initial_load: bool,
    loading_more: bool,
    is_error: bool,
    search_truncated: bool,
    generation: u64,
    fetched_hosts: Option<String>,
    batch: Option<Batch>,
    started: bool,
    scroll: f64,
    content_h: f64,
    viewport: Rect,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    /// Fetch requested by an interaction; issued on the next tick (needs the core handle).
    fetch: Option<bool>,
}

impl HistoryPage {
    pub fn new(host_id: &str, all_hosts: bool, cwd: &str, project_key: &str) -> Self {
        let selected_host = if all_hosts && cwd.is_empty() && project_key.is_empty() { String::new() } else { host_id.to_string() };
        Self {
            cwd: cwd.to_string(),
            project_key: project_key.to_string(),
            selected_host,
            search_field: Field::new("", &tr("sessions.searchPlaceholder")),
            search: String::new(),
            debounce: Timer::default(),
            clock: Timer::default(),
            agents: Vec::new(),
            cursors: HashMap::new(),
            host_errors: Vec::new(),
            initial_load: true,
            loading_more: false,
            is_error: false,
            search_truncated: false,
            generation: 0,
            fetched_hosts: None,
            batch: None,
            started: false,
            scroll: 0.0,
            content_h: 0.0,
            viewport: Rect::default(),
            touch_scroll: Default::default(),
            fetch: None,
        }
    }

    fn target_hosts<'a>(&self, state: &'a Value) -> Vec<Host<'a>> {
        hosts::hosts(state).into_iter().filter(|h| self.selected_host.is_empty() || h.server_id() == self.selected_host).collect()
    }

    fn reachable_hosts<'a>(&self, state: &'a Value) -> Vec<Host<'a>> {
        self.target_hosts(state).into_iter().filter(|h| h.status() == Status::Connected).collect()
    }

    /// One gate: a fleet where any host predates history search has no search field.
    fn search_supported(&self, state: &Value) -> bool {
        let hosts = self.reachable_hosts(state);
        !hosts.is_empty() && hosts.iter().all(|h| h.supports("agentHistorySearch"))
    }

    fn is_searching(&self, state: &Value) -> bool {
        self.search_supported(state) && !self.search.is_empty()
    }

    fn hosts_key(hosts: &[Host<'_>]) -> String {
        hosts.iter().map(|h| h.server_id()).collect::<Vec<_>>().join("\u{0}")
    }

    /// A new query key: no data yet, so the screen shows its initial spinner again.
    fn reset(&mut self) {
        self.agents.clear();
        self.cursors.clear();
        self.initial_load = true;
        self.is_error = false;
        self.search_truncated = false;
        self.scroll = 0.0;
        self.fetch = Some(false);
    }

    /// A project-scoped history without a project key keeps sessions under its root.
    fn in_scope(&self, agent: &Value) -> bool {
        if self.cwd.is_empty() || !self.project_key.is_empty() {
            return true;
        }
        let checkout = j::object(j::object(agent, "project"), "checkout");
        [j::text(agent, "cwd"), j::text(checkout, "cwd"), j::text(checkout, "mainRepoRoot"), j::text(checkout, "worktreeRoot")].contains(&self.cwd.as_str())
    }

    /// Original `fetchAgentHistoryBatch`: every reachable target host in parallel (a next page
    /// only from hosts that reported more), merged most recent first.
    fn start_fetch(&mut self, shell_pending: &mut HashMap<RequestId, Pending>, ui: &UiContext, append: bool) {
        self.generation += 1;
        let state = ui.state.clone();
        let targets = self.target_hosts(&state);
        let reachable = self.reachable_hosts(&state);
        self.fetched_hosts = Some(Self::hosts_key(&reachable));
        let unreachable: Vec<(String, String)> = targets
            .iter()
            .filter(|h| h.status() != Status::Connected)
            .map(|h| (h.server_id().to_string(), h.display_label()))
            .collect();
        let search = if self.search_supported(&state) { self.search.clone() } else { String::new() };
        let cursors = if append { self.cursors.clone() } else { HashMap::new() };
        let hosts: Vec<&Host<'_>> = reachable.iter().filter(|h| !append || cursors.contains_key(h.server_id())).collect();
        if hosts.is_empty() {
            self.initial_load = false;
            self.loading_more = false;
            if !append {
                self.is_error = false;
                self.agents.clear();
                self.cursors.clear();
                self.host_errors = unreachable;
            }
            self.batch = None;
            return;
        }
        if append {
            self.loading_more = true;
        }
        let mut batch = Batch { generation: self.generation, append, search: search.clone(), unreachable, pending: HashSet::new(), results: Vec::new() };
        for host in hosts {
            let mut filter = json!({ "includeArchived": true });
            if !self.project_key.is_empty() {
                filter["projectKeys"] = json!([self.project_key]);
            }
            let mut page = json!({ "limit": HISTORY_PAGE_LIMIT });
            if let Some(cursor) = cursors.get(host.server_id()) {
                page["cursor"] = Value::from(cursor.clone());
            }
            let mut message = json!({
                "type": "fetch_agent_history_request",
                "filter": filter,
                "sort": [{ "key": "updated_at", "direction": "desc" }],
                "page": page,
            });
            if !search.is_empty() {
                message["search"] = Value::from(search.clone());
            }
            let id = ui.core.request(host.server_id(), message, "fetch_agent_history_response");
            shell_pending.insert(id.clone(), Pending::Sheet { sheet: PAGE_SHEET, tag: format!("hist:{}:{}", self.generation, host.server_id()) });
            batch.pending.insert(id);
            batch.results.push((host.server_id().to_string(), host.display_label(), None));
        }
        self.batch = Some(batch);
    }

    /// One host page arrived; the batch merges once every host answered.
    fn host_page(&mut self, generation: u64, server_id: &str, result: &Result<Value, CoreError>) {
        let Some(batch) = self.batch.as_mut().filter(|b| b.generation == generation) else { return };
        let payload = match response_message(result) {
            Some(_) => None,
            None => result.as_ref().ok().cloned(),
        };
        if let Some(slot) = batch.results.iter_mut().find(|(id, _, _)| id == server_id) {
            slot.2 = Some(payload.unwrap_or(Value::Null));
        }
        if batch.results.iter().any(|(_, _, p)| p.is_none()) {
            return;
        }
        let batch = self.batch.take().unwrap();
        let failed: Vec<(String, String)> = batch.results.iter().filter(|(_, _, p)| p.as_ref().is_some_and(Value::is_null)).map(|(id, label, _)| (id.clone(), label.clone())).collect();
        let loaded: Vec<(String, Value)> = batch.results.into_iter().filter_map(|(id, _, p)| p.filter(|p| !p.is_null()).map(|p| (id, p))).collect();
        let merge_errors = |base: &[(String, String)]| -> Vec<(String, String)> {
            let mut errors: Vec<(String, String)> = Vec::new();
            for error in base.iter().chain(failed.iter()) {
                match errors.iter_mut().find(|(id, _)| *id == error.0) {
                    Some(existing) => *existing = error.clone(),
                    None => errors.push(error.clone()),
                }
            }
            errors
        };
        self.initial_load = false;
        self.loading_more = false;
        if loaded.is_empty() {
            // "No connected hosts could load agent history": the query errors.
            if !batch.append {
                self.agents.clear();
                self.cursors.clear();
                self.is_error = true;
                self.host_errors = merge_errors(&batch.unreachable);
            } else {
                self.host_errors = merge_errors(&self.host_errors.clone());
            }
            return;
        }
        let mut next = HashMap::new();
        let mut truncated = false;
        let mut rows = Vec::new();
        for (server_id, payload) in &loaded {
            for entry in j::array(payload, "entries") {
                let mut agent = j::object(entry, "agent").clone();
                if let Value::Object(map) = &mut agent {
                    map.insert("project".into(), j::object(entry, "project").clone());
                }
                if !self.in_scope(&agent) {
                    continue;
                }
                rows.push(HistoryAgent::new(server_id.clone(), agent));
            }
            let page_info = j::object(payload, "pageInfo");
            let cursor = j::text(page_info, "nextCursor");
            if j::boolean(page_info, "hasMore", false) && !cursor.is_empty() {
                next.insert(server_id.clone(), cursor.to_string());
            }
            truncated |= j::boolean(payload, "searchTruncated", false);
        }
        let mut merged: Vec<HistoryAgent> = if batch.append { self.agents.clone() } else { Vec::new() };
        for row in rows {
            match merged.iter_mut().find(|a| a.key() == row.key()) {
                Some(existing) => *existing = row,
                None => merged.push(row),
            }
        }
        merged.sort_by(|a, b| b.last_activity.unwrap_or(0).cmp(&a.last_activity.unwrap_or(0)));
        self.agents = merged;
        self.cursors = next;
        self.is_error = false;
        self.search_truncated = !batch.search.is_empty() && ((batch.append && self.search_truncated) || truncated);
        let base = if batch.append { self.host_errors.clone() } else { batch.unreachable.clone() };
        self.host_errors = merge_errors(&base);
    }
}

// ---------------------------------------------------------------------------------------------
// Shell entry points

/// Per event: store changes, the search debounce, scrolling and the search field.
pub fn tick(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, ui: &mut UiContext) {
    let Some(mut page) = shell.data.history.take() else { return };
    let state = ui.state.clone();
    if !page.started {
        page.started = true;
        // Relative times keep themselves current.
        page.clock = cx.start_interval(60.0);
        if !page.selected_host.is_empty() && hosts::host(&state, &page.selected_host).is_none() {
            page.selected_host.clear();
        }
        page.fetch = Some(false);
    }
    // A host that disappeared drops the filter back to "All hosts".
    if !page.selected_host.is_empty() && hosts::host(&state, &page.selected_host).is_none() {
        page.selected_host.clear();
        page.reset();
    }
    // The query is keyed on the reachable hosts, so a host (re)connecting refetches.
    if page.fetch.is_none() && page.fetched_hosts.as_deref().is_some_and(|key| key != HistoryPage::hosts_key(&page.reachable_hosts(&state))) {
        page.reset();
    }
    if page.clock.is_event(event).is_some() {
        shell.redraw(cx);
    }
    if page.debounce.is_event(event).is_some() {
        let next = page.search_field.text().trim().to_string();
        if next != page.search {
            page.search = next;
            page.reset();
        }
    }
    match page.search_field.handle_event(cx, event, &mut Scope::with_data(ui)) {
        Some(FieldEvent::Changed(_)) => {
            cx.stop_timer(page.debounce);
            page.debounce = cx.start_timeout(SEARCH_DEBOUNCE);
            shell.redraw(cx);
        }
        Some(FieldEvent::Returned(text)) => {
            cx.stop_timer(page.debounce);
            if text.trim() != page.search {
                page.search = text.trim().to_string();
                page.reset();
            }
        }
        None => {}
    }
    if matches!(event, Event::TouchUpdate(_) | Event::LongPress(_)) {
        let hit = event.hits(cx, shell.data.bg_area);
        if let Some(dy) = page.touch_scroll.handle_hit(&hit)
            && let Hit::FingerMove(e) = &hit
            && page.viewport.contains(e.abs_start)
        {
            let max = (page.content_h - page.viewport.size.y).max(0.0);
            page.scroll = (page.scroll + dy).clamp(0.0, max);
            shell.redraw(cx);
        }
    }
    if let Event::Scroll(e) = event
        && page.viewport.contains(e.abs)
    {
        let max = (page.content_h - page.viewport.size.y).max(0.0);
        page.scroll = (page.scroll + e.scroll.y).clamp(0.0, max);
        shell.redraw(cx);
    }
    if let Some(append) = page.fetch.take() {
        page.start_fetch(&mut shell.data.pending, ui, append);
        shell.redraw(cx);
    }
    shell.data.history = Some(page);
}

/// Responses routed to the page (requests registered under [`PAGE_SHEET`]).
pub fn on_response(shell: &mut PaseoShell, cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, _ops: &mut SheetOps) {
    let Some(page) = shell.data.history.as_mut() else { return };
    if let Some(rest) = tag.strip_prefix("hist:") {
        if let Some((generation, server_id)) = rest.split_once(':')
            && let Ok(generation) = generation.parse::<u64>()
        {
            page.host_page(generation, server_id, result);
        }
    } else if let Some(rest) = tag.strip_prefix("archive:") {
        // Timeout errors are swallowed — the daemon still processes the archive.
        if response_message(result).is_none()
            && let Some((server_id, agent_id)) = rest.split_once(':')
        {
            let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
            for row in page.agents.iter_mut().filter(|a| a.server_id == server_id && j::text(&a.agent, "id") == agent_id) {
                row.agent["archivedAt"] = Value::from(now.clone());
            }
        }
    }
    shell.redraw(cx);
}

fn with_page(shell: &mut PaseoShell, f: impl FnOnce(&mut HistoryPage)) {
    if let Some(page) = shell.data.history.as_mut() {
        f(page);
    }
}

pub fn click(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str) {
    let state = ui.state.clone();
    let Some(rest) = id.strip_prefix("hist:") else { return };
    match rest {
        "menu" => {
            // `SidebarMenuToggle`: beside the sidebar it shows or hides it; over the shell
            // (compact) revealing the agent list leaves this screen.
            if shell.data.compact {
                shell.data.layout.show_sidebar(true);
                commands::close_page(shell, cx);
            } else {
                let visible = shell.data.layout.sidebar_visible;
                shell.data.layout.show_sidebar(!visible);
            }
        }
        "clear" => with_page(shell, |page| {
            page.search_field.set_text(cx, "");
            if !page.search.is_empty() {
                page.search.clear();
                page.reset();
            }
        }),
        "retry" => with_page(shell, |page| page.fetch = Some(false)),
        "more" => with_page(shell, |page| {
            if !page.cursors.is_empty() && !page.loading_more {
                page.fetch = Some(true);
            }
        }),
        "back" => commands::close_page(shell, cx),
        "import" => {
            let host = shell.data.history.as_ref().map(|p| p.selected_host.clone()).unwrap_or_default();
            let host = if host.is_empty() { hosts::hosts(&state).first().map(|h| h.server_id().to_string()).unwrap_or_default() } else { host };
            if !host.is_empty() {
                shell.data.overlays.open_sheet(Box::new(ImportSheet::new(&host, "")));
            }
        }
        "filter" => {
            // Original `_HostFilter`: `showPaseoSelect` "Filter by host".
            let p = crate::theme::current_palette();
            let anchor = shell.data.hits.rect_of(&id.to_string()).unwrap_or_default();
            let selected = shell.data.history.as_ref().map(|p| p.selected_host.clone()).unwrap_or_default();
            let mut options = vec![SelectOption::new("", "All hosts").icon_sized("server", 14.0)];
            for host in hosts::hosts(&state) {
                options.push(SelectOption::new(host.server_id(), host.display_label()).dot(sidebar::status_color(&p, &host)));
            }
            commands::shell_select(shell, cx, anchor, SelectRequest::new("Filter by host", options).value(selected), ShellOverlay::HistoryHost);
        }
        _ => {
            if let Some(index) = rest.strip_prefix("row:").and_then(|i| i.parse::<usize>().ok()) {
                // agent-list.tsx `handleAgentPress`: opening any row only navigates to it.
                let Some(row) = shell.data.history.as_ref().and_then(|p| p.agents.get(index).cloned()) else { return };
                commands::open_agent(shell, ui, &row.server_id, j::text(&row.agent, "id"), None, TabPlacement::Main);
                commands::close_page(shell, cx);
            }
        }
    }
    shell.redraw(cx);
}

/// Host filter answer (`ShellOverlay::HistoryHost`).
pub fn select_host(shell: &mut PaseoShell, cx: &mut Cx, server_id: &str) {
    with_page(shell, |page| {
        if page.selected_host != server_id {
            page.selected_host = server_id.to_string();
            page.reset();
        }
    });
    shell.redraw(cx);
}

/// Original long-press: a running agent, or one whose host is offline, asks first; anything
/// else is archived straight away.
pub fn secondary(shell: &mut PaseoShell, cx: &mut Cx, ui: &mut UiContext, id: &str) {
    let Some(index) = id.strip_prefix("hist:row:").and_then(|i| i.parse::<usize>().ok()) else { return };
    let Some(row) = shell.data.history.as_ref().and_then(|p| p.agents.get(index).cloned()) else { return };
    let connected = hosts::host(&ui.state, &row.server_id).is_some_and(|h| h.connected());
    if j::text(&row.agent, "status") == "running" || !connected {
        shell.data.overlays.open_sheet(Box::new(ArchiveSheet { server_id: row.server_id, agent_id: j::text(&row.agent, "id").to_string(), host_offline: !connected }));
        shell.redraw(cx);
        return;
    }
    archive(shell, ui, &row.server_id, j::text(&row.agent, "id"));
}

fn archive(shell: &mut PaseoShell, ui: &UiContext, server_id: &str, agent_id: &str) {
    let id = ui.core.request(server_id, json!({ "type": "archive_agent_request", "agentId": agent_id }), "agent_archived");
    shell.data.pending.insert(id, Pending::Sheet { sheet: PAGE_SHEET, tag: format!("archive:{server_id}:{agent_id}") });
}

/// The original running/offline archive sheet (`_showArchiveSheet`, a fading bottom panel):
/// a centred semibold title, then Cancel / Archive (16px tall padding, 8px radius); Archive is
/// disabled while the host is offline.
struct ArchiveSheet {
    server_id: String,
    agent_id: String,
    host_offline: bool,
}

impl Sheet for ArchiveSheet {
    fn title(&self) -> String {
        tr(if self.host_offline { "agentList.archiveSheet.hostOffline" } else { "agentList.archiveSheet.runningAgent" })
    }

    fn style(&self) -> SheetStyle {
        SheetStyle::FadeBottom
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, _scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let title = ui.spec(Font::Medium, 14.0, p.foreground);
        let title_h = ui.painter.paragraph_height(cx, w, title, &self.title());
        let title_w = ui.painter.measure(cx, title, &self.title()).x.min(w);
        ui.painter.paragraph(cx, dvec2(x + (w - title_w) * 0.5, y), title_w, title, &self.title());
        let by = y + title_h + 16.0;
        let bw = (w - 12.0) * 0.5;
        let buttons = [
            ("cancel", tr("common.actions.cancel"), p.surface1, p.foreground, true),
            ("archive", tr("agentList.archiveSheet.archive"), p.primary, p.on_primary, !self.host_offline),
        ];
        for (i, (id, label, fill, text, enabled)) in buttons.into_iter().enumerate() {
            let r = rect(x + i as f64 * (bw + 12.0), by, bw, 52.0);
            ui.fill(cx, r, fill, 8.0);
            if interactive && enabled {
                if ui.is_hover(id) || ui.is_pressed(id) {
                    ui.fill(cx, r, with_alpha(p.foreground, if ui.is_pressed(id) { 0.1 } else { 0.08 }), 8.0);
                }
                ui.hit(r, id);
            }
            let spec = ui.spec(Font::Medium, 14.0, with_alpha(text, if enabled { text.w } else { text.w * 0.5 }));
            ui.text_centered(cx, r, spec, &label);
        }
        title_h + 16.0 + 52.0
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "archive" => {
                let (server_id, agent_id) = (self.server_id.clone(), self.agent_id.clone());
                ops.run(move |shell, _, ui| archive(shell, ui, &server_id, &agent_id));
                SheetFlow::Close
            }
            "cancel" => SheetFlow::Close,
            _ => SheetFlow::Stay,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Drawing

/// Text with the query's terms marked (surface3 behind, foreground text); single line, clipped.
fn highlighted(ui: &mut Ui, cx: &mut Cx2d, r: Rect, spec: TextSpec, text: &str, query: &str) {
    let fitted = ui.painter.fit(cx, spec, text, r.size.x);
    ui.text(cx, r, spec, &fitted);
    if query.is_empty() {
        return;
    }
    let chars: Vec<char> = fitted.chars().collect();
    let mark = TextSpec { color: ui.p.foreground, ..spec };
    let line = ui.painter.line_height(cx, spec);
    for (start, len) in highlight_ranges(query, &fitted, true) {
        let end = (start + len).min(chars.len());
        if start >= end {
            continue;
        }
        let prefix: String = chars[..start].iter().collect();
        let part: String = chars[start..end].iter().collect();
        let x0 = r.pos.x + ui.painter.measure(cx, spec, &prefix).x;
        let w = ui.painter.measure(cx, spec, &part).x;
        let ty = r.pos.y + (r.size.y - line) * 0.5;
        let surface3 = ui.p.surface3;
        ui.fill(cx, rect(x0, ty, w, line), surface3, 0.0);
        ui.painter.text(cx, dvec2(x0, ty), mark, &part);
    }
}

/// Original `StatusBadge` (settings_host_widgets.dart): a fully round pill, 8/3 padding, 6px
/// gap after the leading glyph, 12px label. Muted: surface3 with a divider border; warning:
/// the warning tone at 16% (dark) / 12% (light) with a transparent border and warning text.
/// Centred on the text line at `y` (height `line`); returns its width.
#[allow(clippy::too_many_arguments)]
fn badge(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, line: f64, label: &str, icon: Option<&str>, warning: bool) -> f64 {
    let p = ui.p;
    let tone = if warning { p.warning } else { p.muted };
    let spec = ui.spec(Font::Regular, 12.0, tone);
    let w = badge_width(ui, cx, label, icon.is_some());
    let text_h = ui.painter.line_height(cx, spec);
    let h = text_h + 6.0 + 2.0;
    let r = rect(x, y + (line - h) * 0.5, w, h);
    let (fill, border) = if warning { (with_alpha(p.warning, if p.is_dark { 0.16 } else { 0.12 }), Vec4::default()) } else { (p.surface3, p.border) };
    ui.painter.outline(cx, r, fill, border, 1.0, h * 0.5);
    let mut tx = r.pos.x + 1.0 + 8.0;
    if let Some(icon) = icon {
        ui.icon(cx, rect(tx, r.pos.y + (h - 12.0) * 0.5, 12.0, 12.0), icon, 12.0, p.muted);
        tx += 12.0 + 6.0;
    }
    ui.text(cx, rect(tx, r.pos.y, r.pos.x + w - 9.0 - tx, h), spec, label);
    w
}

fn badge_width(ui: &mut Ui, cx: &mut Cx2d, label: &str, icon: bool) -> f64 {
    let spec = ui.spec(Font::Regular, 12.0, ui.p.muted);
    1.0 + 8.0 + if icon { 12.0 + 6.0 } else { 0.0 } + ui.measure(cx, spec, label) + 8.0 + 1.0
}

/// The History page beside the sidebar (wide) or over the shell (compact).
pub fn draw_page(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, area: Rect) {
    let Some(mut page) = shell.data.history.take() else { return };
    let state: Arc<Value> = scope.data.get::<UiContext>().map(|ui| ui.state.clone()).unwrap_or_default();
    let compact = shell.data.compact;
    let inset = if compact { 12.0 } else { 24.0 };
    let host_count = hosts::hosts(&state).len();
    let search_supported = page.search_supported(&state);
    let searching = page.is_searching(&state);
    let mut ui = make_ui(shell);
    let p = ui.p;
    ui.fill(cx, area, p.surface0, 0.0);
    let mut y = area.pos.y;

    // `MenuHeader`: compact 12/8 insets and a 56px row, else a 36px bordered row.
    let title = tr("sessions.title");
    let (row_y, row_h) = if compact { (y + 8.0, 56.0) } else { (y, 36.0) };
    let extent = if compact { 32.0 } else { 26.0 };
    let island = rect(area.pos.x + 12.0, row_y + (row_h - extent) * 0.5, extent, extent);
    if ui.is_hover("hist:menu") || ui.is_pressed("hist:menu") {
        let highlight = p.interaction_highlight;
        ui.fill(cx, island, highlight, extent * 0.5);
    }
    if compact {
        // MobileMenuIcon: 16/16/8 × 1.5 muted lines in a 16×12 box.
        let (bx, by) = (island.pos.x + 8.0, island.pos.y + 10.0);
        for (i, w) in [16.0, 16.0, 8.0].iter().enumerate() {
            ui.fill(cx, rect(bx, by + i as f64 * 5.25, *w, 1.5), p.muted, 1.0);
        }
    } else {
        ui.icon(cx, rect(island.pos.x + 5.0, island.pos.y + 5.0, 16.0, 16.0), "panel-left", 16.0, p.muted);
    }
    ui.hit_tip(island, "hist:menu", &tr(if compact { "shell.menu.open" } else { "shell.menu.toggleSidebar" }));
    let spec = ui.spec(if compact { Font::Medium } else { Font::Regular }, if compact { 16.0 } else { 14.0 }, p.foreground);
    let tx = island.pos.x + extent + 8.0;
    ui.text(cx, rect(tx, row_y, area.pos.x + area.size.x - 12.0 - tx, row_h), spec, &title);
    y = row_y + row_h;
    if !compact {
        ui.painter.hline(cx, area.pos.x, y - 1.0, area.size.x, p.border);
    }

    // Search + host filter row.
    let x = area.pos.x + inset;
    let w = area.size.x - inset * 2.0;
    if host_count > 1 || search_supported {
        y += 16.0;
        let mut fx = x;
        if search_supported {
            let sw = w.min(420.0) - if host_count > 1 { 180.0 } else { 0.0 };
            let r = rect(fx, y, sw.max(160.0), 34.0);
            ui.painter.outline(cx, r, p.surface1, p.border, 1.0, 6.0);
            ui.icon(cx, rect(r.pos.x + 12.0, r.pos.y + 10.0, 14.0, 14.0), "search", 14.0, p.muted);
            let has_text = !page.search_field.text().is_empty();
            let field_w = r.size.x - 12.0 - 14.0 - 8.0 - if has_text { 30.0 } else { 12.0 };
            page.search_field.draw(cx, scope, rect(r.pos.x + 34.0, r.pos.y + 1.0, field_w, 32.0));
            if has_text {
                let clear = rect(r.pos.x + r.size.x - 30.0, r.pos.y + 2.0, 30.0, 30.0);
                ui.icon(cx, rect(clear.pos.x + 8.0, clear.pos.y + 8.0, 14.0, 14.0), "x", 14.0, p.muted);
                ui.hit_tip(clear, "hist:clear", &tr("sessions.actions.clearSearch"));
            }
            fx = r.pos.x + r.size.x + 8.0;
        }
        if host_count > 1 {
            let host = hosts::host(&state, &page.selected_host).filter(|_| !page.selected_host.is_empty());
            let label = host.map(|h| h.display_label()).unwrap_or_else(|| "All hosts".to_string());
            let spec = ui.spec(Font::Medium, 14.0, p.foreground);
            let lw = ui.measure(cx, spec, &label).min(180.0);
            let r = rect(fx, y, 12.0 + 14.0 + 6.0 + lw + 6.0 + 14.0 + 12.0, 34.0);
            let bg = if ui.is_pressed("hist:filter") { p.surface3 } else if ui.is_hover("hist:filter") { p.surface2 } else { p.surface1 };
            ui.painter.outline(cx, r, bg, p.border, 1.0, 6.0);
            match host {
                Some(host) => {
                    let color = sidebar::status_color(&p, &host);
                    ui.dot(cx, dvec2(r.pos.x + 12.0 + 7.0, r.pos.y + 17.0), 8.0, color);
                }
                None => ui.icon(cx, rect(r.pos.x + 12.0, r.pos.y + 10.0, 14.0, 14.0), "server", 14.0, p.muted),
            }
            ui.text(cx, rect(r.pos.x + 32.0, r.pos.y, lw, 34.0), spec, &label);
            ui.icon(cx, rect(r.pos.x + 32.0 + lw + 6.0, r.pos.y + 10.0, 14.0, 14.0), "chevron-down", 14.0, p.muted);
            ui.hit(r, "hist:filter");
        }
        y += 34.0;
    }
    if !page.host_errors.is_empty() {
        y += 12.0;
        let spec = ui.spec(Font::Regular, 12.0, argb(RED_300));
        let lines: Vec<String> = page.host_errors.iter().map(|(_, label)| tr_args("sessions.hostLoadFailed", &[("host", label)])).collect();
        let h = 24.0 + lines.len() as f64 * 16.0 + (lines.len().saturating_sub(1)) as f64 * 4.0;
        ui.painter.outline(cx, rect(x, y, w, h), Vec4::default(), p.border, 1.0, 8.0);
        for (i, line) in lines.iter().enumerate() {
            ui.text(cx, rect(x + 12.0, y + 12.0 + i as f64 * 20.0, w - 24.0, 16.0), spec, line);
        }
        y += h;
    }

    let body = rect(area.pos.x, y, area.size.x, (area.pos.y + area.size.y - y).max(0.0));
    page.viewport = body;
    let muted = ui.spec(Font::Regular, 14.0, p.muted);
    let empty_state = |ui: &mut Ui, cx: &mut Cx2d, text: &str, actions: &[(&str, &str, Option<&str>)]| {
        let tw = ui.painter.measure(cx, muted, text).x.min(body.size.x - 48.0);
        let th = ui.painter.paragraph_height(cx, tw.max(1.0), muted, text);
        let total = th + actions.len() as f64 * (24.0 + 32.0);
        let mut cy = body.pos.y + ((body.size.y - total) * 0.5).max(24.0);
        ui.painter.paragraph(cx, dvec2(body.pos.x + (body.size.x - tw) * 0.5, cy), tw.max(1.0), muted, text);
        cy += th;
        for (id, label, icon) in actions {
            cy += 24.0;
            let bw = ui.button_width(cx, label, *icon, ControlSize::Sm);
            ui.button(cx, rect(body.pos.x + (body.size.x - bw) * 0.5, cy, bw, 32.0), id, label, *icon, Variant::Ghost, ControlSize::Sm, true);
            cy += 32.0;
        }
    };
    if page.initial_load {
        spinner(&mut ui, cx, rect(body.pos.x + body.size.x * 0.5 - 18.0, body.pos.y + body.size.y * 0.5 - 18.0, 36.0, 36.0), p.muted);
        // Pages animate through the shell's own redraw (no sheet overlay frame driver).
        crate::shell::sheets::take_spinning();
        ui.painter.draw_box.area().redraw(cx);
    } else if page.is_error && page.agents.is_empty() {
        empty_state(&mut ui, cx, "Unable to load sessions", &[("hist:retry", "Try again", None)]);
    } else if page.agents.is_empty() {
        let text = if searching {
            tr("sessions.noMatches")
        } else if page.selected_host.is_empty() {
            tr("sessions.empty")
        } else {
            "No sessions for this host".to_string()
        };
        let first = if searching { ("hist:clear", tr("sessions.actions.clearSearch"), None) } else { ("hist:back", "Back".to_string(), Some("chevron-left")) };
        let import = tr("importSession.title");
        empty_state(&mut ui, cx, &text, &[(first.0, first.1.as_str(), first.2), ("hist:import", import.as_str(), Some("import"))]);
    } else {
        let query = if searching { page.search.clone() } else { String::new() };
        cx.begin_turtle(Walk::abs_rect(body), Layout { clip_x: true, clip_y: true, ..Layout::default() });
        ui.hits.set_clip(Some(body));
        let start = body.pos.y + 16.0 - page.scroll;
        let mut ry = start;
        let lx = body.pos.x + inset;
        let lw = body.size.x - inset * 2.0;
        let section_spec = ui.spec(Font::Medium, 14.0, p.muted);
        let agents = page.agents.clone();
        for (section, key) in SECTIONS {
            let rows: Vec<(usize, &HistoryAgent)> = agents.iter().enumerate().filter(|(_, a)| date_section(a.last_activity) == section).collect();
            if rows.is_empty() {
                continue;
            }
            ui.text(cx, rect(lx + 12.0, ry + 8.0, lw - 24.0, 20.0), section_spec, &tr(&format!("agentList.dateSections.{key}")));
            ry += 36.0 + 4.0;
            for (index, agent) in rows {
                let host_label = hosts::host(&state, &agent.server_id).map(|h| h.display_label()).unwrap_or_else(|| agent.server_id.clone());
                ry += session_row(&mut ui, cx, lx, ry, lw, index, agent, &host_label, &query, compact) + 4.0;
            }
        }
        if page.search_truncated {
            ry += 16.0;
            let text = tr("sessions.tooManyMatches");
            ui.text_centered(cx, rect(lx, ry, lw, 20.0), muted, &text);
            ry += 20.0 + 16.0;
        } else if !page.cursors.is_empty() {
            ry += 16.0;
            let label = if page.loading_more { "Loading...".to_string() } else { tr("sessions.actions.loadMore") };
            let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
            ui.button(cx, rect(lx + (lw - bw) * 0.5, ry, bw, 32.0), "hist:more", &label, None, Variant::Ghost, ControlSize::Sm, !page.loading_more);
            ry += 32.0 + 16.0;
        }
        ry += 24.0;
        ui.hits.set_clip(None);
        cx.end_turtle();
        page.content_h = ry - start + 16.0;
    }
    shell.data.history = Some(page);
}

/// Original `SessionRow`. Phones: workspace title and badges, then the provider + agent title,
/// then `project · branch · time · host`. Wider layouts: one line with fixed project (132),
/// host (120, right-aligned), branch (132) and time (72, right-aligned) columns.
#[allow(clippy::too_many_arguments)]
fn session_row(ui: &mut Ui, cx: &mut Cx2d, x: f64, y: f64, w: f64, index: usize, row: &HistoryAgent, host_label: &str, query: &str, compact: bool) -> f64 {
    let p = ui.p;
    let agent = &row.agent;
    let project = j::object(agent, "project");
    let project_name = j::text(project, "projectName");
    let workspace_name = j::text(project, "workspaceName");
    let branch = j::text(j::object(project, "checkout"), "currentBranch");
    let title = if j::text(agent, "title").is_empty() { tr("agentList.fallbackTitle") } else { j::text(agent, "title").to_string() };
    let time = time_ago(row.last_activity);
    let pending = j::array(agent, "pendingPermissions").len();
    let base = ui.spec(Font::Regular, 14.0, p.foreground);
    let muted = ui.spec(Font::Regular, 14.0, p.muted);
    let line = 20.0;
    let h = if compact { 8.0 + line * 3.0 + 2.0 + 8.0 } else { 8.0 + line + 8.0 };
    let r = rect(x, y, w, h);
    let id = format!("hist:row:{index}");
    let bg = if ui.is_pressed(&id) { Some(p.surface2) } else if ui.is_hover(&id) { Some(p.surface1) } else { None };
    if let Some(bg) = bg {
        ui.fill(cx, r, bg, if compact { 8.0 } else { 0.0 });
    }
    ui.hit(r, id);
    let cx0 = x + 12.0;
    let inner_w = w - 24.0;
    let workspace_title = if workspace_name.is_empty() { project_name } else { workspace_name };
    let draw_agent_title = |ui: &mut Ui, cx: &mut Cx2d, ax: f64, ay: f64, aw: f64| {
        ProviderIcon::paint(ui.painter, ui.draw_icon, cx, rect(ax, ay + 3.0, 14.0, 14.0), j::text(agent, "provider"), None, p.muted);
        highlighted(ui, cx, rect(ax + 22.0, ay, (aw - 22.0).max(0.0), line), muted, &title, query);
    };
    let badges = |ui: &mut Ui, cx: &mut Cx2d, mut bx: f64, by: f64| {
        if !agent.get("archivedAt").is_none_or(Value::is_null) {
            bx += badge(ui, cx, bx, by, line, &tr("agentList.badges.archived"), Some("archive"), false) + 8.0;
        }
        if pending > 0 {
            badge(ui, cx, bx, by, line, &tr_args("agentList.badges.pending", &[("count", &pending.to_string())]), None, true);
        }
    };
    let badges_w = {
        let mut total = 0.0;
        if !agent.get("archivedAt").is_none_or(Value::is_null) {
            total += badge_width(ui, cx, &tr("agentList.badges.archived"), true) + 8.0;
        }
        if pending > 0 {
            total += badge_width(ui, cx, &tr_args("agentList.badges.pending", &[("count", &pending.to_string())]), false) + 8.0;
        }
        total
    };
    let ty = y + 8.0;
    if compact {
        let ww = (inner_w - badges_w).max(0.0);
        let shown = ui.measure(cx, base, workspace_title).min(ww);
        highlighted(ui, cx, rect(cx0, ty, shown, line), base, workspace_title, query);
        badges(ui, cx, cx0 + shown + 8.0, ty);
        draw_agent_title(ui, cx, cx0, ty + line, inner_w);
        let sep = " · ";
        let meta: Vec<String> = [project_name.to_string(), branch.to_string(), time.clone(), host_label.to_string()].into_iter().filter(|s| !s.is_empty()).collect();
        let text = meta.join(sep);
        highlighted(ui, cx, rect(cx0, ty + line * 2.0 + 2.0, inner_w, line), muted, &text, query);
    } else {
        let columns = 12.0 + 132.0 + 12.0 + 16.0 + 120.0 + 12.0 + 132.0 + 12.0 + 72.0;
        let title_w = (inner_w - columns).max(0.0);
        let ws_w = ui.measure(cx, base, workspace_title).min(320.0).min(title_w);
        highlighted(ui, cx, rect(cx0, ty, ws_w, line), base, workspace_title, query);
        let mut tx = cx0 + ws_w + 8.0;
        ui.icon(cx, rect(tx, ty + 4.0, 12.0, 12.0), "chevron-right", 12.0, p.muted);
        tx += 12.0 + 8.0;
        let agent_w = (cx0 + title_w - tx - badges_w).max(0.0);
        draw_agent_title(ui, cx, tx, ty, agent_w);
        if badges_w > 0.0 {
            badges(ui, cx, tx + agent_w + 8.0, ty);
        }
        let mut colx = cx0 + title_w + 12.0;
        highlighted(ui, cx, rect(colx, ty, 132.0, line), muted, project_name, query);
        colx += 132.0 + 12.0 + 16.0;
        let hw = ui.measure(cx, muted, host_label).min(120.0);
        let fitted = ui.painter.fit(cx, muted, host_label, 120.0);
        ui.text(cx, rect(colx + 120.0 - hw, ty, hw, line), muted, &fitted);
        colx += 120.0 + 12.0;
        highlighted(ui, cx, rect(colx, ty, 132.0, line), muted, branch, query);
        colx += 132.0 + 12.0;
        ui.text_right(cx, rect(colx, ty, 72.0, line), muted, &time);
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Import sessions

/// The original import-session sheet: recent provider sessions on a host (or under one
/// workspace), filterable by provider, imported into Paseo on tap.
pub struct ImportSheet {
    server_id: String,
    scope_cwd: String,
    limit: i64,
    search: Field,
    entries: Vec<Value>,
    providers: Vec<String>,
    provider: Option<String>,
    more: bool,
    busy: bool,
    loaded: bool,
    started: bool,
    error: Option<String>,
    notice: Option<String>,
    catalog_error: Option<String>,
    importing: Option<String>,
    generation: u64,
    debounce_at: Option<f64>,
    load_now: bool,
    /// Provider filter rows drawn last frame (window coordinates).
    filter_rect: Rect,
    filter_open: bool,
    state: Arc<Value>,
}

impl ImportSheet {
    pub fn new(server_id: &str, cwd: &str) -> Self {
        Self {
            server_id: server_id.to_string(),
            scope_cwd: cwd.to_string(),
            limit: 50,
            search: Field::new("", &tr("importSession.searchPlaceholder")),
            entries: Vec::new(),
            providers: Vec::new(),
            provider: None,
            more: false,
            busy: false,
            loaded: false,
            started: false,
            error: None,
            notice: None,
            catalog_error: None,
            importing: None,
            generation: 0,
            debounce_at: None,
            load_now: false,
            filter_rect: Rect::default(),
            filter_open: false,
            state: Arc::new(Value::Null),
        }
    }

    fn key(row: &Value) -> String {
        format!("{}:{}", j::text(row, "providerId"), j::text(row, "providerHandleId"))
    }

    fn load(&mut self, ops: &mut SheetOps) {
        self.generation += 1;
        self.busy = true;
        self.error = None;
        self.notice = None;
        self.catalog_error = None;
        let mut message = json!({
            "type": "fetch_recent_provider_sessions_request",
            "query": self.search.text().trim(),
            "limit": self.limit,
        });
        if !self.scope_cwd.is_empty() {
            message["cwd"] = Value::from(self.scope_cwd.clone());
        }
        if let Some(provider) = &self.provider {
            message["providers"] = json!([provider]);
        }
        ops.request(&self.server_id, message, "fetch_recent_provider_sessions_response", &format!("sessions:{}", self.generation));
        if self.providers.is_empty() {
            ops.request(&self.server_id, json!({ "type": "list_available_providers_request" }), "list_available_providers_response", &format!("providers:{}", self.generation));
        }
    }
}

impl Sheet for ImportSheet {
    fn title(&self) -> String {
        tr("importSession.title")
    }

    fn max_width(&self) -> f64 {
        560.0
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &Value) {
        self.state = Arc::new(state.clone());
    }

    fn draw(&mut self, cx: &mut Cx2d, ui: &mut Ui, scope: &mut Scope, x: f64, y: f64, w: f64, interactive: bool) -> f64 {
        let p = ui.p;
        let start = y;
        let mut y = y;
        // Subtitle: scope + "Show all", refresh on the right.
        let host_label = hosts::host(&self.state, &self.server_id).map(|h| h.display_label()).unwrap_or_else(|| self.server_id.clone());
        let scope_text = if self.scope_cwd.is_empty() { tr_args("importSession.scope.host", &[("host", &host_label)]) } else { tr("importSession.scope.workspace") };
        let small = ui.spec(Font::Regular, 12.0, p.muted);
        let refresh = rect(x + w - 32.0, y - 4.0, 32.0, 32.0);
        let sw = ui.measure(cx, small, &scope_text).min(w - 40.0 - 90.0);
        ui.text(cx, rect(x, y, sw, 24.0), small, &scope_text);
        if !self.scope_cwd.is_empty() {
            let label = tr("importSession.actions.showAll");
            let spec = ui.spec(Font::Medium, 12.0, p.accent);
            let lw = ui.measure(cx, spec, &label) + 16.0;
            let r = rect(x + sw + 8.0, y + 2.0, lw, 20.0);
            ui.text_centered(cx, r, spec, &label);
            if interactive {
                ui.hit(r, "showall".to_string());
            }
        }
        if self.busy {
            spinner(ui, cx, rect(refresh.pos.x + 8.0, refresh.pos.y + 8.0, 16.0, 16.0), p.muted);
        } else {
            ui.icon_button(cx, refresh, if interactive { "refresh" } else { "" }, "rotate-cw", 16.0, 8.0, &tr("importSession.actions.refresh"));
        }
        y += 24.0 + 8.0;
        // Search row.
        let fh = if ui.compact { 44.0 } else { 32.0 };
        ui.icon(cx, rect(x, y + (fh - 16.0) * 0.5, 16.0, 16.0), "search", 16.0, p.muted);
        self.search.draw(cx, scope, rect(x + 24.0, y, w - 24.0, fh));
        y += fh + 12.0;
        let mut providers = self.providers.clone();
        providers.sort();
        if providers.len() > 1 {
            let label = self.provider.clone().unwrap_or_else(|| tr("importSession.filters.all"));
            let spec = ui.spec(Font::Medium, 14.0, p.foreground);
            let lw = ui.measure(cx, spec, &label);
            let r = rect(x, y, 12.0 + 14.0 + 6.0 + lw + 6.0 + 14.0 + 12.0, 32.0);
            ui.painter.outline(cx, r, p.surface1, p.border, 1.0, 6.0);
            match &self.provider {
                Some(provider) => {
                    ProviderIcon::paint(ui.painter, ui.draw_icon, cx, rect(r.pos.x + 12.0, r.pos.y + 9.0, 14.0, 14.0), provider, None, p.muted);
                }
                None => ui.icon(cx, rect(r.pos.x + 12.0, r.pos.y + 9.0, 14.0, 14.0), "layers", 14.0, p.muted),
            }
            ui.text(cx, rect(r.pos.x + 32.0, r.pos.y, lw, 32.0), spec, &label);
            ui.icon(cx, rect(r.pos.x + 32.0 + lw + 6.0, r.pos.y + 9.0, 14.0, 14.0), "chevron-down", 14.0, p.muted);
            if interactive {
                ui.hit_tip(r, "provider", "Filter by provider");
            }
            self.filter_rect = r;
            y += 32.0 + 8.0;
        }
        if self.busy && self.entries.is_empty() {
            spinner(ui, cx, rect(x, y + 11.0, 14.0, 14.0), p.muted);
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text(cx, rect(x + 22.0, y + 8.0, w - 22.0, 20.0), spec, &tr("importSession.status.loading"));
            y += 36.0 + 4.0;
        }
        if let Some(error) = self.error.clone() {
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            let h = ui.painter.paragraph_height(cx, w, spec, &error);
            ui.painter.paragraph(cx, dvec2(x, y), w, spec, &error);
            y += h + 4.0;
        }
        if let Some(notice) = self.notice.clone() {
            let spec = ui.spec(Font::Regular, 12.0, p.danger);
            let h = ui.painter.paragraph_height(cx, w - 24.0, spec, &notice);
            ui.painter.outline(cx, rect(x, y, w, h + 8.0), Vec4::default(), p.border, 1.0, 8.0);
            ui.painter.paragraph(cx, dvec2(x + 12.0, y + 4.0), w - 24.0, spec, &notice);
            y += h + 8.0 + 8.0 + 4.0;
        }
        for (i, row) in self.entries.clone().iter().enumerate() {
            let title = if j::text(row, "title").is_empty() { tr("importSession.preview.untitledSession") } else { j::text(row, "title").to_string() };
            let preview = {
                let first = j::text(row, "firstPromptPreview");
                if first.is_empty() { j::text(row, "promptPreview").to_string() } else { first.to_string() }
            };
            let importing = self.importing.as_deref() == Some(Self::key(row).as_str());
            let body_w = w - 8.0 - 16.0 - 8.0 - 8.0 + 8.0;
            let preview_spec = ui.spec(Font::Regular, 14.0, p.muted);
            let preview_text = if preview.is_empty() { tr("importSession.preview.noPrompt") } else { preview };
            let preview_h = ui.painter.paragraph_height(cx, body_w, preview_spec, &preview_text).min(40.0);
            let show_cwd = self.scope_cwd.is_empty() && !j::text(row, "cwd").is_empty();
            let h = 8.0 + 20.0 + 4.0 + preview_h + if show_cwd { 4.0 + 16.0 } else { 0.0 } + 8.0;
            let r = rect(x - 8.0, y, w + 8.0, h);
            let id = format!("row:{i}");
            if interactive && !self.busy {
                if ui.is_pressed(&id) {
                    ui.fill(cx, r, p.surface2, 8.0);
                } else if ui.is_hover(&id) {
                    ui.fill(cx, r, p.surface1, 8.0);
                }
                ui.hit(r, id);
            }
            ProviderIcon::paint(ui.painter, ui.draw_icon, cx, rect(r.pos.x + 8.0, r.pos.y + 10.0, 16.0, 16.0), j::text(row, "providerId"), None, p.muted);
            let tx = r.pos.x + 8.0 + 16.0 + 8.0;
            let time = if importing { "Importing…".to_string() } else { time_ago(parse_ms(j::text(row, "lastActivityAt"))) };
            let small = ui.spec(Font::Regular, 12.0, p.muted);
            let time_w = ui.measure(cx, small, &time);
            let title_spec = ui.spec(Font::Regular, 14.0, p.foreground);
            let fitted = ui.painter.fit(cx, title_spec, &title, (body_w - time_w - 8.0).max(0.0));
            ui.text(cx, rect(tx, r.pos.y + 8.0, body_w - time_w - 8.0, 20.0), title_spec, &fitted);
            ui.text_right(cx, rect(tx + body_w - time_w, r.pos.y + 8.0, time_w, 20.0), small, &time);
            // Two lines of preview at most.
            cx.begin_turtle(Walk::abs_rect(rect(tx, r.pos.y + 32.0, body_w, preview_h)), Layout { clip_x: true, clip_y: true, ..Layout::default() });
            ui.painter.paragraph(cx, dvec2(tx, r.pos.y + 32.0), body_w, preview_spec, &preview_text);
            cx.end_turtle();
            if show_cwd {
                let fitted = ui.painter.fit(cx, small, j::text(row, "cwd"), body_w);
                ui.text(cx, rect(tx, r.pos.y + 32.0 + preview_h + 4.0, body_w, 16.0), small, &fitted);
            }
            y += h + 4.0;
        }
        if self.more {
            y += 8.0;
            let label = tr("importSession.actions.loadMore");
            let bw = ui.button_width(cx, &label, None, ControlSize::Sm);
            ui.button(cx, rect(x + (w - bw) * 0.5, y, bw, 32.0), "more", &label, None, Variant::Ghost, ControlSize::Sm, interactive && !self.busy);
            y += 32.0;
        }
        if self.loaded && !self.busy && self.entries.is_empty() && self.error.is_none() {
            y += 32.0;
            ui.icon(cx, rect(x + w * 0.5 - 10.0, y, 20.0, 20.0), "inbox", 20.0, with_alpha(p.muted, 0.6));
            y += 28.0;
            let text = tr(if self.search.text().trim().is_empty() { "importSession.empty.noRecent" } else { "sessions.noMatches" });
            let spec = ui.spec(Font::Regular, 14.0, p.foreground);
            ui.text_centered(cx, rect(x, y, w, 20.0), spec, &text);
            y += 20.0 + 32.0;
        }
        if self.filter_open && providers.len() > 1 {
            self.draw_provider_menu(cx, ui, &providers, interactive);
        }
        if self.debounce_at.is_some() {
            crate::shell::sheets::SPINNING.with(|s| s.set(true));
        }
        if let Some(at) = self.debounce_at
            && cx.seconds_since_app_start() >= at
        {
            self.debounce_at = None;
            self.load_now = true;
        }
        y - start
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        match id {
            "refresh" => self.load(ops),
            "showall" => {
                self.scope_cwd.clear();
                self.load(ops);
            }
            "provider" => self.filter_open = !self.filter_open,
            "pick:dismiss" => self.filter_open = false,
            "more" => {
                self.limit += 50;
                self.load(ops);
            }
            _ => {
                if let Some(index) = id.strip_prefix("pick:").and_then(|i| i.parse::<usize>().ok()) {
                    let mut providers = self.providers.clone();
                    providers.sort();
                    self.provider = if index == 0 { None } else { providers.get(index - 1).cloned() };
                    self.filter_open = false;
                    self.load(ops);
                } else if let Some(row) = id.strip_prefix("row:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| self.entries.get(i).cloned()) {
                    self.busy = true;
                    self.error = None;
                    self.importing = Some(Self::key(&row));
                    if j::text(&row, "cwd").is_empty() {
                        self.busy = false;
                        self.importing = None;
                        self.error = Some("Provider session has no working directory.".into());
                        return SheetFlow::Stay;
                    }
                    ops.request(
                        &self.server_id,
                        json!({
                            "type": "import_agent_request",
                            "providerId": j::text(&row, "providerId"),
                            "providerHandleId": j::text(&row, "providerHandleId"),
                            "cwd": j::text(&row, "cwd"),
                        }),
                        "status",
                        "import",
                    );
                }
            }
        }
        SheetFlow::Stay
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> SheetFlow {
        if let Some(FieldEvent::Changed(_) | FieldEvent::Returned(_)) = self.search.handle_event(cx, event, scope) {
            self.debounce_at = Some(cx.seconds_since_app_start() + 0.3);
        }
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        if !self.started {
            self.started = true;
            self.load(ops);
        }
        if std::mem::take(&mut self.load_now) {
            self.load(ops);
        }
        SheetFlow::Stay
    }

    fn on_response(&mut self, _cx: &mut Cx, tag: &str, result: &Result<Value, CoreError>, ops: &mut SheetOps) -> SheetFlow {
        let failure = response_message(result);
        let payload = result.as_ref().ok().cloned().unwrap_or(Value::Null);
        if let Some(generation) = tag.strip_prefix("providers:").and_then(|g| g.parse::<u64>().ok()) {
            if generation == self.generation {
                match failure {
                    Some(error) => {
                        self.catalog_error = Some(format!("Provider list: {error}"));
                        self.notice = Some(self.notices(&Value::Null));
                    }
                    None => {
                        for item in j::array(&payload, "providers") {
                            let id = j::text(item, "provider");
                            if !id.is_empty() && !self.providers.iter().any(|p| p == id) {
                                self.providers.push(id.to_string());
                            }
                        }
                    }
                }
            }
            return SheetFlow::Stay;
        }
        if let Some(generation) = tag.strip_prefix("sessions:").and_then(|g| g.parse::<u64>().ok()) {
            if generation != self.generation {
                return SheetFlow::Stay;
            }
            self.busy = false;
            self.loaded = true;
            match failure {
                Some(error) => self.error = Some(error),
                None => {
                    let rows = j::array(&payload, "entries").to_vec();
                    self.entries.clear();
                    for row in &rows {
                        let key = Self::key(row);
                        self.entries.retain(|old| Self::key(old) != key);
                        self.entries.push(row.clone());
                        let provider = j::text(row, "providerId");
                        if !provider.is_empty() && !self.providers.iter().any(|p| p == provider) {
                            self.providers.push(provider.to_string());
                        }
                    }
                    self.more = rows.len() as i64 >= self.limit;
                    let notice = self.notices(&payload);
                    self.notice = (!notice.is_empty()).then_some(notice);
                }
            }
            return SheetFlow::Stay;
        }
        if tag == "import" {
            self.busy = false;
            self.importing = None;
            if let Some(error) = failure {
                self.error = Some(error);
                return SheetFlow::Stay;
            }
            let agent = j::object(&payload, "agent");
            let agent_id = j::text(agent, "id").to_string();
            if agent_id.is_empty() {
                self.error = Some(tr("importSession.status.failedImport"));
                return SheetFlow::Stay;
            }
            ops.core.refresh_host(&self.server_id);
            let server_id = self.server_id.clone();
            ops.run(move |shell, _, ui| commands::open_agent(shell, ui, &server_id, &agent_id, None, TabPlacement::Main));
            return SheetFlow::Close;
        }
        SheetFlow::Stay
    }
}

impl ImportSheet {
    /// Material 3 `PopupMenuButton` (position over the button): surfaceContainer (surface2),
    /// 4px radius, elevation 3, 8px vertical padding, 36px items with 12px padding and the
    /// labelLarge (medium 14) label, width stepped to 56px within 112–280. "All", then each
    /// provider with its 14px icon. A tap outside closes it.
    fn draw_provider_menu(&mut self, cx: &mut Cx2d, ui: &mut Ui, providers: &[String], interactive: bool) {
        let p = ui.p;
        ui.painter.new_layer(cx);
        ui.draw_icon.new_draw_call(cx);
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        let mut options = vec![(String::new(), tr("importSession.filters.all"))];
        options.extend(providers.iter().map(|p| (p.clone(), p.clone())));
        let natural = options
            .iter()
            .map(|(value, label)| 24.0 + if value.is_empty() { 0.0 } else { 14.0 + 8.0 } + ui.measure(cx, spec, label))
            .fold(0.0, f64::max);
        let width = ((natural / 56.0).ceil() * 56.0).clamp(112.0, 280.0);
        let anchor = self.filter_rect;
        let list = rect(anchor.pos.x, anchor.pos.y, width, 8.0 + options.len() as f64 * 36.0 + 8.0);
        if interactive {
            ui.hit(rect(-1.0e5, -1.0e5, 2.0e5, 2.0e5), "pick:dismiss");
        }
        ui.fill(cx, rect(list.pos.x, list.pos.y + 1.0, list.size.x, list.size.y + 2.0), argb(0x26000000), 5.0);
        ui.fill(cx, rect(list.pos.x - 1.0, list.pos.y + 2.0, list.size.x + 2.0, list.size.y + 3.0), argb(0x14000000), 6.0);
        ui.fill(cx, list, p.surface2, 4.0);
        for (i, (value, label)) in options.iter().enumerate() {
            let row = rect(list.pos.x, list.pos.y + 8.0 + i as f64 * 36.0, list.size.x, 36.0);
            let id = format!("pick:{i}");
            if ui.is_hover(&id) || ui.is_pressed(&id) {
                ui.fill(cx, row, with_alpha(p.foreground, if ui.is_pressed(&id) { 0.1 } else { 0.08 }), 0.0);
            }
            let mut lx = row.pos.x + 12.0;
            if !value.is_empty() {
                ProviderIcon::paint(ui.painter, ui.draw_icon, cx, rect(lx, row.pos.y + 11.0, 14.0, 14.0), value, None, p.muted);
                lx += 14.0 + 8.0;
            }
            ui.text(cx, rect(lx, row.pos.y, row.size.x - (lx - row.pos.x) - 12.0, 36.0), spec, label);
            if interactive {
                ui.hit(row, id);
            }
        }
    }

    /// The catalog error plus one line per failed provider.
    fn notices(&self, payload: &Value) -> String {
        let mut notices: Vec<String> = self.catalog_error.iter().cloned().collect();
        for error in j::array(payload, "providerErrors") {
            notices.push(tr_args("importSession.status.failedProvider", &[("provider", j::text(error, "provider"))]));
        }
        notices.join("\n")
    }
}
