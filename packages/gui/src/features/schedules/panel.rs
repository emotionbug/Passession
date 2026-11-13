//! `SchedulesPanel` (Flutter `schedules_screen.dart`, original `screens/schedules-screen.tsx`):
//! the `MenuHeader`, then a spinner, load error or empty state, or the filter row (host
//! filter, Active/Ended, New schedule) over one card of schedule rows across every connected
//! host. The DSL lives in `schedules/mod.rs`.

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use makepad_widgets::*;
use crate::features::workspace::kit::{WaModalWidgetExt, WaModalWidgetRefExt};
use serde_json::{json, Value};

use super::form::{WaScheduleEditorWidgetExt, WaScheduleEditorWidgetRefExt};
use super::model::{self, ScheduleState};
use crate::common::provider_icon::ProviderIconWidgetRefExt;
use crate::common::{UiContext, UiEffect};
use crate::features::workspace::dialog::WaDialogWidgetRefExt;
use crate::features::workspace::host::{self, Connection};
use crate::features::workspace::i18n::{l, lf, t, ta};
use crate::common::widgets::{PLabelWidgetRefExt, PSegmentedWidgetRefExt, Segment, SurfaceWidgetRefExt};
use crate::features::workspace::kit::{
    clicked_in, set_badge, set_optional, sync_input, BadgeVariant, WaButtonWidgetRefExt, WaInputWidgetRefExt, WaRepeatWidgetRefExt,
};
use crate::features::workspace::rpc::{array_of, object_of, str_of, Pending};
use crate::features::workspace::select::{Leading, SelectOption, SelectRequest, WaSelectWidgetExt, WaSelectWidgetRefExt};

pub const SCHEDULES_DOCS_URL: &str = "https://paseo.sh/docs/schedules";

/// In-flight row actions by `serverId:scheduleId`.
type RowPending = HashMap<String, HashSet<String>>;

#[derive(Clone, Debug)]
enum Req {
    List { generation: u64, host_id: String, label: String },
    Action { key: String, action: String, previous: Value },
    Logs { key: String },
}

#[derive(Live, LiveHook, Widget)]
pub struct SchedulesPanel {
    #[deref]
    view: View,
    #[rust]
    schedules: Vec<Value>,
    /// Labels of connected hosts whose `schedule/list` failed.
    #[rust]
    host_errors: Vec<String>,
    #[rust]
    loading: bool,
    #[rust]
    loaded: bool,
    #[rust]
    load_failed: bool,
    #[rust]
    generation: u64,
    #[rust]
    outstanding: usize,
    #[rust]
    settling: bool,
    #[rust]
    connected_count: usize,
    #[rust]
    rows_in_flight: Vec<Value>,
    #[rust]
    failures_in_flight: Vec<String>,
    #[rust]
    host_signature: String,
    #[rust]
    selected_host: String,
    #[rust]
    status_filter: String,
    #[rust]
    timer: Timer,
    /// In-flight row actions keyed by `serverId:scheduleId`.
    #[rust]
    row_pending: RowPending,
    #[rust]
    pending: Pending<Req>,
    /// The rows shown, in order (for matching row actions).
    #[rust]
    visible_rows: Vec<Value>,
    #[rust]
    runs: Vec<Value>,
    #[rust]
    runs_schedule: Option<Value>,
    #[rust]
    compact: bool,
    /// The layout the header geometry was applied for.
    #[rust]
    header_compact: Option<bool>,
    /// Whether the body is centred for an empty state.
    #[rust]
    body_centred: Option<bool>,
}

fn row_key(schedule: &Value) -> String {
    format!("{}:{}", str_of(schedule, "_serverId"), schedule.get("id").map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).unwrap_or_default())
}

/// Roll back only this row: another schedule may have a concurrent optimistic mutation.
fn restore_row(rows: &mut Vec<Value>, key: &str, previous: Value) {
    if let Some(row) = rows.iter_mut().find(|row| row_key(row) == key) {
        *row = previous;
    } else {
        rows.push(previous);
        rows.sort_by(|a, b| str_of(b, "createdAt").cmp(str_of(a, "createdAt")));
    }
}

fn apply_pending(rows: &mut Vec<Value>, pending: &RowPending) {
    let now = Utc::now();
    rows.retain_mut(|row| {
        let Some(actions) = pending.get(&row_key(row)) else { return true };
        if actions.contains("delete") {
            return false;
        }
        for action in ["pause", "resume"] {
            if actions.contains(action) {
                *row = model::optimistic_status(row, action, now);
            }
        }
        true
    });
}

impl SchedulesPanel {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    /// `fetchAggregatedSchedules`: offline hosts are skipped, a failing connected host lands
    /// in the banner, and only when every connected host fails the full load error shows.
    /// Rows from the last load stay visible while a refresh is in flight.
    fn load(&mut self, ctx: &UiContext) {
        self.generation += 1;
        self.loading = true;
        let hosts = host::hosts(&ctx.state);
        let connected: Vec<_> = hosts.iter().filter(|h| h.connected()).collect();
        self.settling = hosts.iter().any(|h| h.status() == Connection::Connecting);
        self.connected_count = connected.len();
        self.outstanding = connected.len();
        self.rows_in_flight.clear();
        self.failures_in_flight.clear();
        for host in connected {
            let req = Req::List { generation: self.generation, host_id: host.id().to_owned(), label: host.label() };
            self.pending.send_typed(&ctx.core, host.id(), "schedule/list", json!({}), "schedule/list/response", req);
        }
        if self.outstanding == 0 {
            self.complete_load();
        }
    }

    fn complete_load(&mut self) {
        let mut rows = std::mem::take(&mut self.rows_in_flight);
        rows.sort_by(|a, b| str_of(b, "createdAt").cmp(str_of(a, "createdAt")));
        apply_pending(&mut rows, &self.row_pending);
        let failures = std::mem::take(&mut self.failures_in_flight);
        let failed = self.connected_count > 0 && rows.is_empty() && failures.len() == self.connected_count;
        self.loading = false;
        self.load_failed = failed;
        self.schedules = rows;
        self.host_errors = failures;
        if failed || !self.schedules.is_empty() || !self.settling {
            self.loaded = true;
        }
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        host::learn(&ctx.events);
        if self.status_filter.is_empty() {
            self.status_filter = "runnable".into();
        }
        if self.timer.is_empty() {
            // Reload every 30s while the page is mounted (`Timer.periodic`).
            self.timer = cx.start_timeout(30.0);
        }
        let signature = host::hosts(&ctx.state)
            .iter()
            .map(|h| format!("{}:{:?}", h.id(), h.status()))
            .collect::<Vec<_>>()
            .join("|");
        if signature != self.host_signature {
            self.host_signature = signature;
            self.load(ctx);
            self.view.redraw(cx);
        }
        for (req, result) in self.pending.take(&ctx.events) {
            self.view.redraw(cx);
            match req {
                Req::List { generation, host_id, label } => {
                    if generation != self.generation {
                        continue;
                    }
                    match result {
                        Ok(body) => {
                            for value in array_of(&body, "schedules") {
                                let mut row = value.clone();
                                row["_serverId"] = host_id.clone().into();
                                row["_hostLabel"] = label.clone().into();
                                self.rows_in_flight.push(row);
                            }
                        }
                        Err(_) => self.failures_in_flight.push(label),
                    }
                    self.outstanding = self.outstanding.saturating_sub(1);
                    if self.outstanding == 0 {
                        self.complete_load();
                    }
                }
                Req::Action { key, action, previous } => {
                    match &result {
                        // A failed pause, resume or delete rolls back and says why.
                        Err(error) => {
                            if action != "run-once" {
                                restore_row(&mut self.schedules, &key, previous);
                            }
                            let key = match action.as_str() {
                                "pause" => "workspace.schedules.errors.pause",
                                "resume" => "workspace.schedules.errors.resume",
                                "delete" => "workspace.schedules.errors.delete",
                                _ => "workspace.schedules.errors.runOnce",
                            };
                            ctx.effects.push(UiEffect::Toast { message: ta(key, &[("reason", &error.to_string())]), is_error: true });
                        }
                        Ok(_) if action == "run-once" => {
                            ctx.effects.push(UiEffect::Toast { message: t("workspace.schedules.runStarted"), is_error: false });
                        }
                        Ok(_) => {}
                    }
                    self.finish_row_action(&key, &action);
                    self.load(ctx);
                }
                Req::Logs { key } => {
                    self.finish_row_action(&key, "runs");
                    match result {
                        Ok(body) => {
                            let Some(schedule) = self.schedules.iter().find(|s| row_key(s) == key).cloned() else { continue };
                            let mut runs = array_of(&body, "runs").to_vec();
                            runs.sort_by(|a, b| str_of(b, "startedAt").cmp(str_of(a, "startedAt")));
                            self.runs = runs;
                            self.runs_schedule = Some(schedule);
                            self.view.wa_modal(id!(runs_modal)).open(cx);
                        }
                        Err(error) => {
                            let message = ta("workspace.schedules.errors.runHistory", &[("reason", &error.to_string())]);
                            ctx.effects.push(UiEffect::Toast { message, is_error: true });
                        }
                    }
                }
            }
        }
    }

    fn finish_row_action(&mut self, key: &str, action: &str) {
        if let Some(actions) = self.row_pending.get_mut(key) {
            actions.remove(action);
            if actions.is_empty() {
                self.row_pending.remove(key);
            }
        }
    }

    /// `SchedulesTableRow` + `useScheduleMutations`: pause/resume flip the status, delete drops
    /// the row, each rolls back on failure and every action refetches when it settles.
    fn act(&mut self, ctx: &UiContext, schedule: &Value, action: &str) {
        let key = row_key(schedule);
        if !matches!(action, "pause" | "resume" | "delete" | "run-once")
            || self.row_pending.get(&key).is_some_and(|actions| actions.iter().any(|action| action != "runs"))
            || !host::host(&ctx.state, str_of(schedule, "_serverId")).is_some_and(|h| h.connected() && h.has_permission("automation.manage"))
        {
            return;
        }
        let previous = schedule.clone();
        // `cancelQueries` before the optimistic write: an in-flight load must not overwrite it.
        self.generation += 1;
        self.loading = false;
        self.row_pending.entry(key.clone()).or_default().insert(action.to_owned());
        let now = Utc::now();
        match action {
            "pause" | "resume" => {
                for row in &mut self.schedules {
                    if row_key(row) == key {
                        *row = model::optimistic_status(row, action, now);
                    }
                }
            }
            "delete" => self.schedules.retain(|row| row_key(row) != key),
            _ => {}
        }
        let req = Req::Action { key, action: action.to_owned(), previous };
        self.pending.send_typed(
            &ctx.core,
            str_of(schedule, "_serverId"),
            &format!("schedule/{action}"),
            json!({ "scheduleId": schedule["id"] }),
            &format!("schedule/{action}/response"),
            req,
        );
    }

    fn state(&self, ctx: &UiContext, schedule: &Value) -> ScheduleState {
        model::schedule_state(schedule, host::host(&ctx.state, str_of(schedule, "_serverId")), Utc::now())
    }

    fn open_editor(&mut self, cx: &mut Cx, ctx: &UiContext, schedule: Option<Value>) {
        let hosts: Vec<String> = host::hosts(&ctx.state).iter().map(|h| h.id().to_owned()).collect();
        self.view.wa_schedule_editor(id!(editor)).open(cx, &hosts, schedule);
    }

    fn open_kebab(&mut self, cx: &mut Cx, ctx: &UiContext, schedule: &Value) {
        let state = self.state(ctx, schedule);
        let product = model::product_name(schedule);
        let lower = model::product_name_lower(schedule);
        let target_type = str_of(object_of(schedule, "target"), "type");
        // Without `automation.manage` (or while offline) only reading actions stay available.
        let can_manage = host::host(&ctx.state, str_of(schedule, "_serverId")).is_some_and(|h| h.connected() && h.has_permission("automation.manage"));
        let can_run = target_type == "new-agent" && state.runnable() && can_manage;
        let busy = self.row_pending.get(&row_key(schedule)).cloned().unwrap_or_default();
        let mut options = vec![SelectOption::new("edit", lf("Edit {product}", &[("product", lower)])).glyph("pencil")];
        if target_type != "agent" {
            if str_of(schedule, "status") == "paused" {
                let label = if busy.contains("resume") { l("Resuming...") } else { l("Resume schedule") };
                options.push(SelectOption::new("resume", label).glyph("play").enabled(can_run && !busy.contains("resume")));
            } else {
                let label = if busy.contains("pause") { l("Pausing...") } else { l("Pause schedule") };
                let enabled = str_of(schedule, "status") != "completed" && can_run && !busy.contains("pause");
                options.push(SelectOption::new("pause", label).glyph("pause").enabled(enabled));
            }
            let label = if busy.contains("run-once") { l("Starting...") } else { l("Run now") };
            options.push(SelectOption::new("run-once", label).glyph("rotate-cw").enabled(can_run && !busy.contains("run-once")));
        }
        let runs = if busy.contains("runs") { t("common.states.loading") } else { l("Run history").to_owned() };
        options.push(SelectOption::new("runs", runs).glyph("history").enabled(!busy.contains("runs")));
        let delete = if busy.contains("delete") { l("Deleting...").to_owned() } else { lf("Delete {product}", &[("product", lower)]) };
        options.push(SelectOption::new("delete", delete).glyph("trash-2").destructive().separated(true).enabled(can_manage && !busy.contains("delete")));
        let title = lf("{product} actions", &[("product", product)]);
        self.view.wa_select(id!(select)).open(cx, &format!("row:{}", row_key(schedule)), SelectRequest::new(title, options));
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        let view = self.root();
        if view.wa_button(id!(menu_toggle)).clicked(actions) {
            ctx.effects.push(UiEffect::RunCommand { action: "sidebar.toggle.left".into(), args: Value::Null });
        }
        if view.wa_button(id!(try_again)).clicked(actions) && !self.loading {
            self.load(ctx);
        }
        if view.wa_button(id!(new_schedule)).clicked(actions) || view.wa_button(id!(empty_new)).clicked(actions) {
            self.open_editor(cx, ctx, None);
        }
        if view.wa_button(id!(docs)).clicked(actions) {
            ctx.effects.push(UiEffect::OpenExternal { url: SCHEDULES_DOCS_URL.into() });
        }
        if let Some(filter) = view.psegmented(id!(status_filter)).changed(actions) {
            self.status_filter = filter;
            self.view.redraw(cx);
        }
        if view.wa_button(id!(host_filter)).clicked(actions) {
            let mut options = vec![SelectOption::new("", l("All hosts")).glyph("server")];
            options.extend(host::hosts(&ctx.state).iter().map(|h| SelectOption::new(h.id(), h.label()).leading(Leading::Status(h.status()))));
            view.wa_select(id!(select)).open(cx, "host-filter", SelectRequest::new(l("Filter by host"), options).selected(Some(&self.selected_host)));
        }
        if view.wa_schedule_editor(id!(editor)).saved(actions) {
            self.load(ctx);
        }
        let rows = view.wa_repeat(id!(rows));
        if let Some(index) = clicked_in(&rows, id!(kebab), actions) {
            if let Some(schedule) = self.visible_rows.get(index).cloned() {
                self.open_kebab(cx, ctx, &schedule);
            }
        } else if let Some(index) = rows.items().iter().position(|item| item.surface(id!(row)).clicked(actions))
            && let Some(schedule) = self.visible_rows.get(index).cloned()
        {
            self.open_editor(cx, ctx, Some(schedule));
        }
        if let Some((token, value)) = view.wa_select(id!(select)).picked(actions) {
            if token == "host-filter" {
                self.selected_host = value;
            } else if let Some(key) = token.strip_prefix("row:") && let Some(schedule) = self.schedules.iter().find(|s| row_key(s) == key).cloned() {
                match value.as_str() {
                    "edit" => self.open_editor(cx, ctx, Some(schedule)),
                    "runs" => {
                        // The row's menu button spins until the history arrives.
                        self.row_pending.entry(key.to_owned()).or_default().insert("runs".to_owned());
                        let req = Req::Logs { key: key.to_owned() };
                        self.pending.send_typed(&ctx.core, str_of(&schedule, "_serverId"), "schedule/logs", json!({ "scheduleId": schedule["id"] }), "schedule/logs/response", req);
                    }
                    "delete" => {
                        let product = model::product_name(&schedule).to_lowercase();
                        let title = lf("Delete {product}", &[("product", &product)]);
                        let message = lf("Delete \"{title}\"? This cannot be undone.", &[("title", &model::schedule_title(&schedule))]);
                        view.wa_dialog(id!(dialog)).confirm(cx, &format!("delete:{key}"), &title, &message, l("Delete"), true);
                    }
                    action => self.act(ctx, &schedule, action),
                }
            }
            self.view.redraw(cx);
        }
        if let Some(token) = view.wa_dialog(id!(dialog)).confirmed(actions) {
            if let Some(key) = token.strip_prefix("delete:") && let Some(schedule) = self.schedules.iter().find(|s| row_key(s) == key).cloned() {
                self.act(ctx, &schedule, "delete");
            }
            self.view.redraw(cx);
        }
        // Run history.
        let runs_modal = view.wa_modal(id!(runs_modal));
        if view.wa_button(id!(runs_modal.sheet.header.close)).clicked(actions) {
            runs_modal.close(cx);
        }
        let run_rows = view.wa_repeat(id!(run_rows));
        if let Some(index) = clicked_in(&run_rows, id!(open_session), actions) && let (Some(run), Some(schedule)) = (self.runs.get(index), &self.runs_schedule) {
            ctx.effects.push(UiEffect::OpenSession {
                host_id: str_of(schedule, "_serverId").to_owned(),
                agent_id: str_of(run, "agentId").to_owned(),
            });
            runs_modal.close(cx);
            // The original pops the schedules screen after opening the session.
            ctx.effects.push(UiEffect::Navigate(crate::common::Route::Welcome));
        }
    }

    /// Original `MenuHeader`: the phone `CompactHeader` (12px insets, 8px top, 56px row, round
    /// 32px island with the MobileMenuIcon, lg medium title) or the wide `ScreenHeader`
    /// (36px bordered row, 26px island with PanelLeft, base title).
    fn present_header(&mut self, cx: &mut Cx) {
        let view = self.root();
        view.widget(id!(header.title)).set_text(cx, l("Schedules"));
        if self.header_compact == Some(self.compact) {
            return;
        }
        self.header_compact = Some(self.compact);
        let header = view.widget(id!(header));
        let toggle = view.wa_button(id!(menu_toggle));
        if self.compact {
            header.apply_over(cx, live! { height: 64., padding: { left: 12., right: 12., top: 8. } });
            view.widget(id!(header.title)).apply_over(cx, live! { size: 16., weight: Medium });
            toggle.set_icon(cx, crate::features::workspace::kit::MOBILE_MENU);
            toggle.set_tooltip(cx, "");
        } else {
            header.apply_over(cx, live! { height: 36., padding: { left: 12., right: 12., top: 0. } });
            view.widget(id!(header.title)).apply_over(cx, live! { size: 14., weight: Regular });
            toggle.set_icon(cx, "panel-left");
            toggle.set_tooltip(cx, &t("shell.menu.toggleSidebar"));
        }
        view.widget(id!(header_rule)).set_visible(cx, !self.compact);
        let inset = if self.compact { 12.0 } else { 24.0 };
        view.widget(id!(filters)).apply_over(cx, live! { padding: { left: (inset), right: (inset) } });
        view.widget(id!(body)).apply_over(cx, live! { padding: { left: (inset), right: (inset) } });
    }

    fn present(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let view = self.root();
        let width = view.area().rect(cx).size.x;
        if width > 0.0 {
            self.compact = host::is_compact(width);
        }
        self.present_header(cx);
        let banner_text = self
            .host_errors
            .iter()
            .map(|h| lf("{host}: Could not load schedules", &[("host", h)]))
            .collect::<Vec<_>>()
            .join("\n");
        let fail_view = view.widget(id!(failed));
        fail_view.set_visible(cx, self.load_failed);
        fail_view.widget(id!(text)).set_text(cx, l("Unable to load schedules"));
        let try_again = view.wa_button(id!(try_again));
        try_again.set_text(cx, l("Try again"));
        try_again.set_enabled(cx, !self.loading);
        view.widget(id!(spinner_view)).set_visible(cx, !self.load_failed && !self.loaded);
        let ready = !self.load_failed && self.loaded;
        let hosts = host::hosts(&ctx.state);
        let selected_host = if hosts.iter().any(|h| h.id() == self.selected_host) { self.selected_host.clone() } else { String::new() };
        let single_host = hosts.len() <= 1;
        let now = Utc::now();
        let rows: Vec<Value> = self
            .schedules
            .iter()
            .filter(|s| selected_host.is_empty() || str_of(s, "_serverId") == selected_host)
            .filter(|s| self.state(ctx, s).runnable() == (self.status_filter == "runnable"))
            .cloned()
            .collect();
        // Every host offline is its own state, not "no schedules".
        let offline_state = ready && self.connected_count == 0 && !self.settling;
        let offline = view.widget(id!(offline_state));
        offline.set_visible(cx, offline_state);
        if offline_state {
            offline.widget(id!(title)).set_text(cx, &t("workspace.schedules.noConnectedHosts"));
            offline.widget(id!(description)).set_text(cx, &t("workspace.schedules.noConnectedHostsDescription"));
        }
        // Hosts that are not connected have their schedules hidden; say which.
        let offline_text = if ready && !offline_state {
            hosts
                .iter()
                .filter(|h| !h.connected())
                .map(|h| {
                    let key = if h.status() == Connection::Connecting { "workspace.schedules.hostConnecting" } else { "workspace.schedules.hostOffline" };
                    ta(key, &[("host", &h.label())])
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        set_optional(cx, &view.widget(id!(offline_banner)), &offline_text);
        let empty_all = ready && !offline_state && self.schedules.is_empty();
        view.widget(id!(filters)).set_visible(cx, ready && !offline_state && !empty_all);
        set_optional(cx, &view.widget(id!(banner)), if ready { &banner_text } else { "" });
        let empty_state = ready && !offline_state && (empty_all || (rows.is_empty() && self.status_filter == "runnable"));
        let empty = view.widget(id!(empty_state));
        empty.set_visible(cx, empty_state);
        empty.widget(id!(title)).set_text(cx, l("No active schedules"));
        empty.widget(id!(description)).set_text(cx, l("Schedules run agents on a cadence."));
        view.wa_button(id!(docs)).set_text(cx, l("See docs"));
        view.wa_button(id!(empty_new)).set_text(cx, l("New schedule"));
        // The text column is at most 420px, narrower on phones.
        let body_width = view.widget(id!(body)).area().rect(cx).size.x;
        let inset = if self.compact { 24.0 } else { 48.0 };
        let texts_width = if body_width > 0.0 { (body_width - inset).clamp(0.0, 420.0) } else { 420.0 };
        for path in [id!(empty_state.texts), id!(offline_state.texts)] {
            view.widget(path).apply_over(cx, live! { width: (texts_width) });
        }
        let ended = view.widget(id!(ended_empty));
        let ended_state = ready && !offline_state && !empty_all && rows.is_empty() && self.status_filter == "ended";
        ended.set_visible(cx, ended_state);
        // Original `_centered` / `SliverFillRemaining`: empty states sit centred in the body.
        let centred = empty_state || ended_state || offline_state;
        if self.body_centred != Some(centred) {
            self.body_centred = Some(centred);
            let align_y = if centred { 0.5 } else { 0.0 };
            view.widget(id!(body)).apply_over(cx, live! { align: { x: 0.5, y: (align_y) } });
        }
        ended.widget(id!(text)).set_text(cx, l("No ended schedules"));
        if ready && !offline_state && !empty_all {
            let host_filter = view.wa_button(id!(host_filter));
            host_filter.set_visible(cx, !single_host);
            let label = hosts.iter().find(|h| h.id() == selected_host).map(|h| h.label()).unwrap_or_else(|| l("All hosts").to_owned());
            host_filter.set_text(cx, &label);
            let status = hosts.iter().find(|h| h.id() == selected_host).map(|h| h.status());
            host_filter.set_dot(cx, status);
            host_filter.set_icon(cx, if status.is_some() { "" } else { "server" });
            let segment = |value: &str, label: &str| Segment { value: value.into(), label: label.into(), icon: None, enabled: true };
            view.psegmented(id!(status_filter)).set_options(cx, vec![segment("runnable", l("Active")), segment("ended", l("Ended"))], &self.status_filter);
            view.wa_button(id!(new_schedule)).set_text(cx, l("New schedule"));
        }
        let card = view.widget(id!(card));
        card.set_visible(cx, ready && !rows.is_empty());
        let repeat = view.wa_repeat(id!(rows));
        repeat.set_count(cx, if ready { rows.len() } else { 0 });
        if ready {
            for (index, schedule) in rows.iter().enumerate() {
                let item = repeat.item(cx, index);
                let host = host::host(&ctx.state, str_of(schedule, "_serverId"));
                let state = model::schedule_state(schedule, host, now);
                let (target, provider) = model::target_label(schedule, host);
                item.widget(id!(rule)).set_visible(cx, index > 0);
                item.widget(id!(title)).set_text(cx, &model::schedule_title(schedule));
                item.widget(id!(target)).set_text(cx, &target);
                let host_label = (!single_host).then(|| str_of(schedule, "_hostLabel").to_owned());
                item.widget(id!(meta)).set_text(cx, &model::meta(schedule, state, host_label.as_deref(), now));
                let icon = item.provider_icon(id!(provider));
                item.widget(id!(provider_box)).set_visible(cx, provider.is_some());
                if let Some(provider) = &provider {
                    icon.set_provider(cx, provider, None);
                }
                let (label, variant) = match state {
                    ScheduleState::Active => (l("Active"), BadgeVariant::Success),
                    ScheduleState::Paused => (l("Paused"), BadgeVariant::Muted),
                    ScheduleState::Expired => (l("Expired"), BadgeVariant::Muted),
                    ScheduleState::Finished => (l("Finished"), BadgeVariant::Muted),
                    ScheduleState::TargetGone => (l("Target gone"), BadgeVariant::Error),
                };
                set_badge(cx, &item.widget(id!(badge)), label, variant);
                // An action or run-history request in flight spins the row's menu button.
                item.wa_button(id!(kebab)).set_busy(cx, self.row_pending.get(&row_key(schedule)).is_some_and(|busy| !busy.is_empty()));
            }
        }
        self.visible_rows = rows;
        // Run history.
        if let Some(schedule) = &self.runs_schedule {
            view.widget(id!(runs_modal.sheet.header.title)).set_text(cx, l("Run history"));
            view.widget(id!(runs_subtitle)).set_text(cx, &model::schedule_title(schedule));
            set_optional(cx, &view.widget(id!(runs_empty)), if self.runs.is_empty() { l("No runs yet") } else { "" });
            let run_rows = view.wa_repeat(id!(run_rows));
            run_rows.set_count(cx, self.runs.len());
            for (index, run) in self.runs.iter().enumerate() {
                let item = run_rows.item(cx, index);
                item.widget(id!(rule)).set_visible(cx, index > 0);
                let (label, variant) = match str_of(run, "status") {
                    "succeeded" => (l("Succeeded"), BadgeVariant::Success),
                    "failed" => (l("Failed"), BadgeVariant::Error),
                    _ => (l("Running"), BadgeVariant::Warning),
                };
                set_badge(cx, &item.widget(id!(status)), label, variant);
                item.widget(id!(started)).set_text(cx, &lf("Started {time}", &[("time", &model::time_ago(run.get("startedAt"), now))]));
                let open = item.wa_button(id!(open_session));
                open.set_visible(cx, !str_of(run, "agentId").is_empty());
                open.set_text(cx, l("Open session"));
                let output = str_of(run, "output").trim_end();
                let output_view = item.widget(id!(output_box));
                output_view.set_visible(cx, !output.trim().is_empty());
                if !output.trim().is_empty() {
                    sync_input(cx, &item.wa_input(id!(output)), output);
                    let height = (output.lines().count() as f64 * 19.2 + 16.0).clamp(36.0, 200.0);
                    item.widget(id!(output_scroll)).apply_over(cx, live! { height: (height) });
                }
                let run_error = item.widget(id!(run_error));
                set_optional(cx, &run_error, str_of(run, "error"));
                run_error.plabel(id!(text)).set_color(cx, Some(crate::theme::current_palette().danger));
            }
        }
    }
}

impl Widget for SchedulesPanel {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
            if self.timer.is_event(event).is_some() {
                self.timer = cx.start_timeout(30.0);
                if !self.loading {
                    self.load(ctx);
                    self.view.redraw(cx);
                }
            }
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.present(cx, ctx);
        }
        self.view.draw_walk(cx, scope, walk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_schedule_mutations_keep_their_own_optimistic_state() {
        let first = json!({"id": "a", "_serverId": "h", "status": "active"});
        let second = json!({"id": "b", "_serverId": "h", "status": "active"});
        let mut rows = vec![first.clone(), second.clone()];
        let pending = HashMap::from([
            (row_key(&first), HashSet::from(["pause".to_owned()])),
            (row_key(&second), HashSet::from(["delete".to_owned()])),
        ]);
        apply_pending(&mut rows, &pending);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], "paused");
        restore_row(&mut rows, &row_key(&first), first);
        assert_eq!(rows.len(), 1, "rolling back a pause must not resurrect another deletion");
        assert_eq!(rows[0]["status"], "active");
        let mut refreshed = vec![second];
        apply_pending(&mut refreshed, &pending);
        assert!(refreshed.is_empty(), "refreshes must retain pending deletions");
    }
}
