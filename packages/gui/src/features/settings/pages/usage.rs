//! Original `HostUsageSection` / `usage/card.tsx` (Flutter `usage_settings.dart`):
//! streamed usage reports per host, refresh all / per report, pins and the
//! usage options sheet.

use std::time::{Duration, Instant};

use makepad_widgets::*;
use serde_json::{Value, json};

use super::View;
use super::app::Pick;
use crate::features::settings::host::HostView;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, get, obj, scalar_text, str_of, str_or};
use crate::features::settings::model::preferences::sidebar_nav_preferences;
use crate::features::settings::model::time::{is_future, relative};
use crate::features::settings::model::usage::{
    UsageView, display_percent, effective_pins, legacy_reports, problem_text, replace, resolve_view, settle, supports_usage, toggle_pin, upsert,
    usage_label,
};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::rpc::check_response;
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::kit::{BSize, Ctl, Ui, Variant, button, row, switch};
use crate::features::settings::ui::theme::alpha;

/// Original `HostUsageReports` for one host.
#[derive(Default)]
pub struct UsageState {
    pub data: Option<Vec<Value>>,
    pub error: Option<String>,
    pub updated_at: Option<Instant>,
    pub fetching: bool,
    /// The running list's request id and the reports it streamed so far.
    pub request_id: Option<String>,
    pub streamed: Vec<Value>,
    /// Per-report refresh: request id → report id, and `pending`/`failed`.
    pub refreshing: Vec<(String, String)>,
    pub refresh_states: std::collections::HashMap<String, &'static str>,
    pub refresh_streamed: Vec<(String, Value)>,
}

const STALE: Duration = Duration::from_secs(60);

pub fn on_status_change(s: &mut Settings, host: &str) {
    if let Some(u) = s.usage.get_mut(host) {
        u.updated_at = None;
    }
}

/// The query's mount/reconnect fetch: only when nothing or stale.
pub fn ensure_usage(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || !supports_usage(&host.server_info) {
        return;
    }
    let u = s.usage.entry(host.server_id.clone()).or_default();
    if u.fetching {
        return;
    }
    if u.data.is_some() && u.updated_at.map(|at| at.elapsed() < STALE).unwrap_or(false) {
        return;
    }
    fetch(s, env, &host.server_id, false);
}

fn new_request_id() -> String {
    let micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    format!("usage-{micros}")
}

/// Original `fetch`/`_list`: a running fetch is joined.
fn fetch(s: &mut Settings, env: &mut Env, host: &str, force: bool) {
    let Some(view) = env.host(host) else { return };
    let u = s.usage.entry(host.to_string()).or_default();
    if u.fetching {
        return;
    }
    if !view.connected() {
        u.error = Some(t("settingsNative.usageNotReady"));
        return;
    }
    if !supports_usage(&view.server_info) {
        u.error = Some(t("settingsNative.usageUpdateHost"));
        return;
    }
    u.fetching = true;
    let host_id = host.to_string();
    if !view.feature("usageSources") {
        // COMPAT(providerUsageList): pre-usageSources hosts.
        s.request(env, host, "provider.usage.list.request", json!({}), "provider.usage.list.response", move |s, _, result| {
            let u = s.usage.entry(host_id).or_default();
            u.fetching = false;
            match result {
                Ok(payload) => {
                    u.data = Some(settle(u.data.as_deref(), legacy_reports(&payload)));
                    u.updated_at = Some(Instant::now());
                    u.error = None;
                }
                Err(error) => u.error = Some(error.text()),
            }
        });
        return;
    }
    let request_id = new_request_id();
    u.request_id = Some(request_id.clone());
    u.streamed.clear();
    let message = json!({"type": "usage.list_reports.request", "requestId": request_id, "forceRefresh": force});
    s.request_streamed(env, host, message, "usage.list_reports.response", Some(Duration::from_secs(60)), move |s, _, result| {
        let result = result.and_then(check_response);
        let u = s.usage.entry(host_id).or_default();
        u.fetching = false;
        u.request_id = None;
        let finished = std::mem::take(&mut u.streamed);
        match result {
            Ok(r) if r.get("complete").and_then(Value::as_bool) == Some(false) => u.error = Some(t("settingsNative.usageIncomplete")),
            Ok(_) => {
                u.data = Some(settle(u.data.as_deref(), finished));
                u.updated_at = Some(Instant::now());
                u.error = None;
            }
            Err(error) => u.error = Some(error.text()),
        }
    });
}

/// Original `useReportRefresh`: one report asked again.
fn refresh_report(s: &mut Settings, env: &mut Env, host: &str, report_id: String) {
    let Some(view) = env.host(host) else { return };
    let u = s.usage.entry(host.to_string()).or_default();
    u.refresh_states.insert(report_id.clone(), "pending");
    let host_id = host.to_string();
    if !view.connected() || !supports_usage(&view.server_info) {
        u.refresh_states.insert(report_id, "failed");
        return;
    }
    if !view.feature("usageSources") {
        s.request(env, host, "provider.usage.list.request", json!({}), "provider.usage.list.response", move |s, _, result| {
            let u = s.usage.entry(host_id).or_default();
            match result {
                Ok(payload) => {
                    let refreshed = legacy_reports(&payload).into_iter().find(|r| str_of(r, "id") == report_id);
                    if let Some(data) = &mut u.data {
                        replace(data, &report_id, refreshed);
                    }
                    u.refresh_states.remove(&report_id);
                }
                Err(_) => {
                    u.refresh_states.insert(report_id, "failed");
                }
            }
        });
        return;
    }
    let request_id = new_request_id();
    u.refreshing.push((request_id.clone(), report_id.clone()));
    let message = json!({"type": "usage.list_reports.request", "requestId": request_id, "forceRefresh": true, "reportIds": [report_id.clone()]});
    let rid = request_id.clone();
    s.request_streamed(env, host, message, "usage.list_reports.response", Some(Duration::from_secs(60)), move |s, _, result: Result<Value, ReqError>| {
        let result = result.and_then(check_response);
        let u = s.usage.entry(host_id).or_default();
        u.refreshing.retain(|(id, _)| *id != rid);
        match result {
            Ok(r) if r.get("complete").and_then(Value::as_bool) != Some(false) => {
                let refreshed = u.streamed_for(&rid).into_iter().find(|r| str_of(r, "id") == report_id);
                if let Some(data) = &mut u.data {
                    replace(data, &report_id, refreshed);
                }
                u.refresh_states.remove(&report_id);
            }
            _ => {
                u.refresh_states.insert(report_id, "failed");
            }
        }
    });
}

impl UsageState {
    /// Reports streamed for one per-report refresh request.
    fn streamed_for(&mut self, request_id: &str) -> Vec<Value> {
        let mut out = Vec::new();
        self.refresh_streamed.retain(|(id, report)| {
            if id == request_id {
                out.push(report.clone());
                false
            } else {
                true
            }
        });
        out
    }
}

/// `usage.list_reports.update` events stream reports in by request id.
pub fn on_host_event(s: &mut Settings, host: &str, event: &Value) {
    if str_of(event, "type") != "usage.list_reports.update" {
        return;
    }
    let payload = get(event, "payload");
    let request_id = str_of(payload, "requestId");
    let Some(u) = s.usage.get_mut(host) else { return };
    let report = get(payload, "report").clone();
    if u.request_id.as_deref() == Some(request_id) {
        u.streamed.push(report.clone());
        let data = u.data.get_or_insert_with(Vec::new);
        upsert(data, report);
        u.updated_at = Some(Instant::now());
    } else if u.refreshing.iter().any(|(id, _)| id == request_id) {
        u.refresh_streamed.push((request_id.to_string(), report));
    }
}

fn in_sidebar(prefs: &Value) -> bool {
    arr(prefs, "sidebarFooterItems").iter().any(|i| str_of(i, "key") == "usage" && i.get("visible").and_then(Value::as_bool) == Some(true))
}

pub fn draw_usage(ui: &mut Ui, s: &Settings, v: &View, host: &HostView) {
    let pal = ui.pal;
    let id = host.server_id.clone();
    let default = UsageState::default();
    let u = s.usage.get(&id).unwrap_or(&default);
    let supported = supports_usage(&host.server_info);
    let view = resolve_view(host.connected(), supported, u.data.is_some(), u.error.is_some());
    let busy = view == UsageView::Loading || (view == UsageView::Ready && u.fetching);
    let trailing = (view != UsageView::Unavailable).then(|| {
        let size = if ui.compact { BSize::Sm } else { BSize::Xs };
        let refresh_id = id.clone();
        let refresh = (!busy).then(|| handler(move |s, env| fetch(s, env, &refresh_id, true)));
        let options_id = id.clone();
        let options = handler(move |s, _| {
            let sheet = s.next_id();
            s.push_sheet(sheet, crate::features::settings::state::SheetKind::UsageOptions { host: options_id.clone() }, 340.0);
        });
        let mut refresh_button = button(format!("usage.{id}.refreshAll"), String::new(), refresh).variant(Variant::Ghost).size(size).loading(busy).tip(t("providerUsage.refreshAll"));
        if !busy {
            refresh_button = refresh_button.icon(Icon::RotateCw);
        }
        Ctl::Row(vec![refresh_button.ctl(), button(format!("usage.{id}.options"), String::new(), Some(options)).icon(Icon::Settings).variant(Variant::Ghost).size(size).tip(t("providerUsage.options")).ctl()], 8.0)
    });
    ui.section_title(&t("providerUsage.title"), None, trailing);
    match view {
        UsageView::Unavailable => {
            let key = if host.connected() { "providerUsage.hostUpgradeRequired" } else { "providerUsage.hostUnavailable" };
            ui.empty_card(&ta(key, &[("host", &host.label)]), None, false);
        }
        UsageView::Loading => ui.empty_card(&t("providerUsage.loading"), None, false),
        UsageView::Error => {
            let retry_id = id.clone();
            let retry = button(format!("usage.{id}.retry"), t("providerUsage.retry"), Some(handler(move |s, env| fetch(s, env, &retry_id, true)))).size(BSize::Sm).ctl();
            let description = u.error.clone().unwrap_or_default();
            ui.tinted_alert(&format!("usage.{id}.error"), &t("providerUsage.errorTitle"), &description, Some(retry));
        }
        UsageView::Ready => {
            let reports = u.data.clone().unwrap_or_default();
            if reports.is_empty() {
                ui.empty_card(&t("providerUsage.empty"), None, false);
            }
            let pinnable = in_sidebar(&v.prefs);
            let prefs = obj(&v.prefs, "usage").clone();
            let prefs = Value::Object(prefs);
            for (i, entry) in reports.iter().enumerate() {
                if i > 0 {
                    ui.gap(12.0);
                }
                draw_card(ui, u, &id, entry, &reports, &prefs, pinnable, pal);
            }
        }
    }
    ui.gap(24.0);
}

#[allow(clippy::too_many_arguments)]
fn draw_card(ui: &mut Ui, u: &UsageState, host: &str, entry: &Value, reports: &[Value], prefs: &Value, pinnable: bool, pal: crate::features::settings::ui::theme::Palette) {
    let report = get(entry, "report");
    let available = str_of(report, "status") == "available";
    let snapshot = (!available).then(|| entry.get("lastSuccessful").filter(|v| !v.is_null())).flatten();
    let content = if available { report.clone() } else { snapshot.map(|s| get(s, "report").clone()).unwrap_or(Value::Null) };
    let id = str_of(entry, "id").to_string();
    let title = str_or(entry, "sourceLabel", &id).to_string();
    let status = str_of(report, "status");
    let remaining = str_of(prefs, "displayAs") == "remaining";
    let pins = effective_pins(prefs, reports);
    let retained = snapshot.is_some();
    let successful_at = if available {
        Some(str_or(entry, "successfulAt", str_of(entry, "fetchedAt")).to_string())
    } else {
        snapshot.and_then(|s| s.get("fetchedAt")).and_then(Value::as_str).map(String::from)
    };
    let ago = relative(successful_at.as_deref(), true);
    let freshness = match ago {
        None => t("providerUsage.timing.noSuccessfulUpdate"),
        Some(ago) => ta(if retained { "providerUsage.timing.lastUpdated" } else { "providerUsage.timing.updated" }, &[("relativeTime", &ago)]),
    };
    let mut messages = Vec::new();
    if !available {
        match entry.get("loginErrors").and_then(Value::as_array) {
            Some(logins) => {
                for login in logins {
                    messages.push(format!("{}: {}", scalar_text(get(login, "harness")), problem_text(get(login, "report"))));
                }
            }
            None => messages.push(problem_text(report)),
        }
    }
    messages.retain(|m| !m.is_empty());
    let (x0, w0) = (ui.x, ui.w);
    let top = ui.y;
    let bg = ui.rect(Rect { pos: dvec2(x0, top), size: dvec2(w0, 1.0) }, pal.surface1, Some(pal.border), 8.0);
    ui.x = x0 + 16.0;
    ui.w = w0 - 32.0;
    ui.y = top + 16.0;
    // Header: icon, name, plan badge; status and refresh.
    let mut header = Vec::new();
    if let Some(svg) = entry.get("icon").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        header.push(Ctl::Provider(String::new(), Some(svg.to_string()), 14.0, pal.foreground));
    }
    header.push(Ctl::Label { text: title, color: pal.foreground, size: 14.0, font: Font::Regular, max_w: ui.w - 160.0 });
    let plan = str_of(&content, "planLabel");
    if !plan.is_empty() {
        header.push(Ctl::Badge { label: plan.to_string(), tone: None, xs: true, dot: None, icon: None });
    }
    let mut right = Vec::new();
    if !status.is_empty() && status != "available" {
        let color = if status == "error" { pal.danger } else { pal.muted };
        right.push(Ctl::Dot(color, 8.0));
        right.push(Ctl::Label { text: t(if status == "error" { "providerUsage.status.error" } else { "providerUsage.status.unavailable" }), color: pal.muted, size: 12.0, font: Font::Regular, max_w: 120.0 });
    }
    let refreshing = u.refresh_states.get(&id) == Some(&"pending");
    let refresh_host = host.to_string();
    let refresh_id = id.clone();
    let refresh = (!refreshing).then(|| handler(move |s, env| refresh_report(s, env, &refresh_host, refresh_id.clone())));
    right.push(Ctl::IconButton { key: format!("usage.{host}.{id}.refresh"), icon: if refreshing { Icon::Spinner } else { Icon::RotateCw }, act: refresh, destructive: false, size: 20.0, tip: None });
    let header_ctl = Ctl::Row(header, 8.0);
    let right_ctl = Ctl::Row(right, 8.0);
    let hs = ui.measure_ctl(&header_ctl);
    let rs = ui.measure_ctl(&right_ctl);
    let hh = hs.y.max(rs.y);
    let (hx, hy) = (ui.x, ui.y);
    ui.draw_ctl(header_ctl, dvec2(hx, hy + (hh - hs.y) / 2.0));
    ui.draw_ctl(right_ctl, dvec2(hx + ui.w - rs.x, hy + (hh - rs.y) / 2.0));
    ui.y += hh + 12.0;
    if !messages.is_empty() {
        ui.selectable_para(&format!("usage.{host}.{id}.messages"), &messages.join("\n"), 12.0, pal.danger);
        ui.gap(12.0);
    }
    let windows = arr(&content, "windows").to_vec();
    let balances = arr(&content, "balances").to_vec();
    for window in &windows {
        let pinned = pinnable && pins.iter().any(|p| p.get("sourceId") == entry.get("sourceId") && p.get("windowId") == window.get("id"));
        draw_window(ui, host, entry, window, reports, prefs, remaining, retained, pinnable, pinned, pal);
        ui.gap(4.0);
    }
    for balance in &balances {
        draw_balance(ui, balance, pal);
        ui.gap(4.0);
    }
    if !windows.is_empty() || !balances.is_empty() {
        ui.gap(8.0);
    }
    for detail in arr(&content, "details") {
        let value = scalar_text(get(detail, "value"));
        let vw = ui.measure(Font::Regular, 12.0, &value, None).x;
        ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(ui.x, ui.y), ui.w - vw - 8.0, str_of(detail, "label"));
        let s = ui.text(Font::Regular, 12.0, pal.foreground, dvec2(ui.x + ui.w - vw, ui.y), None, &value);
        ui.y += s.y + 4.0;
    }
    let footer = str_of(get(entry, "account"), "label").to_string();
    let fw = ui.measure(Font::Regular, 12.0, &freshness, None).x;
    ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(ui.x, ui.y), ui.w - fw - 8.0, &footer);
    let s1 = ui.text(Font::Regular, 12.0, pal.muted, dvec2(ui.x + ui.w - fw, ui.y), None, &freshness);
    ui.y += s1.y;
    if u.refresh_states.get(&id) == Some(&"failed") {
        ui.gap(8.0);
        ui.para(&t("providerUsage.refreshFailed"), 12.0, pal.danger);
    }
    ui.y += 16.0;
    bg.set_rect(ui.cx, &Rect { pos: dvec2(x0, top), size: dvec2(w0, ui.y - top) });
    ui.x = x0;
    ui.w = w0;
}

fn tone_color(pal: &crate::features::settings::ui::theme::Palette, tone: &Value, used: Option<f64>) -> Vec4 {
    let resolved = match tone.as_str() {
        Some(tone) => tone.to_string(),
        None => match used {
            Some(u) if u > 90.0 => "danger".into(),
            Some(u) if u >= 70.0 => "warning".into(),
            _ => "default".into(),
        },
    };
    match resolved.as_str() {
        "ok" => pal.success,
        "warning" => pal.warning,
        "danger" => pal.danger,
        _ => pal.muted,
    }
}

fn meter(ui: &mut Ui, percent: f64, fill: Vec4, pal: &crate::features::settings::ui::theme::Palette) {
    let (x, y, w) = (ui.x, ui.y, ui.w);
    ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, 4.0) }, pal.surface3, None, 2.0);
    let fw = (percent / 100.0).clamp(0.0, 1.0) * w;
    if fw > 0.0 {
        ui.rect(Rect { pos: dvec2(x, y), size: dvec2(fw, 4.0) }, fill, None, 2.0);
    }
    ui.y += 4.0;
}

/// Original `UsageWindowBar`.
#[allow(clippy::too_many_arguments)]
fn draw_window(ui: &mut Ui, host: &str, entry: &Value, window: &Value, reports: &[Value], prefs: &Value, remaining: bool, retained: bool, pinnable: bool, pinned: bool, pal: crate::features::settings::ui::theme::Palette) {
    let used = display_percent(window, false);
    let shown = display_percent(window, remaining);
    let pct = shown.map(|p| format!("{}%", p.clamp(0.0, 100.0).round()));
    let value = match &pct {
        None => "—".to_string(),
        Some(p) if remaining => ta("providerUsage.values.remaining", &[("amount", p)]),
        Some(p) => ta("providerUsage.values.used", &[("percentage", p)]),
    };
    let runs_out = window.get("runsOutAt").and_then(Value::as_str);
    let at_risk = runs_out.is_some() && !window.get("shortfallPct").map(Value::is_null).unwrap_or(true) && (!retained || runs_out.map(is_future).unwrap_or(false));
    let trailing = if at_risk {
        relative(runs_out, false).map(|d| if d.is_empty() { t("providerUsage.timing.runsOutNow") } else { ta("providerUsage.timing.runsOutIn", &[("duration", &d)]) })
    } else {
        relative(window.get("resetsAt").and_then(Value::as_str), false).map(|d| {
            if d.is_empty() {
                t(if retained { "providerUsage.timing.resetTimePassed" } else { "providerUsage.timing.resettingNow" })
            } else {
                ta("providerUsage.timing.resetsIn", &[("duration", &d)])
            }
        })
    };
    let wid = str_of(window, "id").to_string();
    let key = format!("usage.{host}.{}.{wid}.pin", str_of(entry, "id"));
    let (x, y, w) = (ui.x - 8.0, ui.y, ui.w + 16.0);
    let h = 6.0 + 16.0 + 3.0 + 4.0 + 6.0;
    let (hovered, _) = ui.state(&key);
    if pinned {
        ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, h) }, pal.surface2, None, 6.0);
    } else if pinnable && hovered {
        ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, h) }, alpha(pal.surface2, 0.5), None, 6.0);
    }
    let inner_w = ui.w - if pinnable { 20.0 } else { 0.0 };
    let label = usage_label(&wid, str_or(window, "label", &wid));
    let mut value_text = value.clone();
    if let Some(trailing) = &trailing {
        value_text = format!("{value} · {trailing}");
    }
    let vw = ui.measure(Font::Medium, 12.0, &value_text, None).x.min(inner_w - 40.0);
    ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(ui.x, y + 6.0), inner_w - vw - 8.0, &label);
    let vx = ui.x + inner_w - vw;
    let used_w = ui.text_line(Font::Medium, 12.0, pal.foreground, dvec2(vx, y + 6.0), vw, &value).x;
    if let Some(trailing) = trailing {
        let color = if at_risk { pal.danger } else { pal.muted };
        ui.text_line(Font::Regular, 12.0, color, dvec2(vx + used_w, y + 6.0), (vw - used_w).max(0.0), &format!(" · {trailing}"));
    }
    let saved_w = ui.w;
    ui.w = inner_w;
    ui.y = y + 6.0 + 16.0 + 3.0;
    let fill = tone_color(&pal, get(window, "tone"), used);
    meter(ui, used.unwrap_or(0.0), fill, &pal);
    ui.w = saved_w;
    if pinnable {
        let icon = if pinned { Icon::PinOff } else { Icon::Pin };
        ui.icon(icon, dvec2(ui.x + ui.w - 12.0, y + (h - 12.0) / 2.0), 12.0, pal.muted);
        let prefs = prefs.clone();
        let source = get(entry, "sourceId").clone();
        let window_id = get(window, "id").clone();
        let reports = reports.to_vec();
        let toggle = handler(move |s, env| {
            let next = toggle_pin(&prefs, &source, &window_id, &reports);
            s.save_pref(env, "usage", Some(next));
        });
        ui.zone(&key, Rect { pos: dvec2(x, y), size: dvec2(w, h) }, Some(toggle));
    }
    ui.y = y + h;
}

/// Original `UsageBalanceBar`.
fn draw_balance(ui: &mut Ui, balance: &Value, pal: crate::features::settings::ui::theme::Palette) {
    let unit = str_of(balance, "unit").to_string();
    let format = |value: f64| {
        let number = if value.fract() == 0.0 { format!("{}", value as i64) } else { format!("{value}") };
        if unit.is_empty() { number } else { format!("{number} {unit}") }
    };
    let limit = balance.get("limit").and_then(Value::as_f64);
    let used_raw = balance.get("used").and_then(Value::as_f64);
    let remaining = balance.get("remaining").and_then(Value::as_f64);
    let mut used_pct = None;
    let amount = match (limit, remaining, used_raw) {
        (Some(limit), _, _) if limit > 0.0 => {
            let used = used_raw.or(remaining.map(|r| limit - r));
            used_pct = used.map(|u| u / limit * 100.0);
            format!("{} / {}", used.map(format).unwrap_or_else(|| "—".into()), format(limit))
        }
        (_, Some(remaining), _) => ta("providerUsage.values.remaining", &[("amount", &format(remaining))]),
        (_, _, Some(used)) => format(used),
        _ => "—".into(),
    };
    let reset = relative(balance.get("resetsAt").and_then(Value::as_str), false).filter(|r| !r.is_empty());
    let id = str_of(balance, "id");
    let label = usage_label(id, str_of(balance, "label"));
    let aw = ui.measure(Font::Medium, 12.0, &amount, None).x;
    ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(ui.x, ui.y), ui.w - aw - 8.0, &label);
    let s = ui.text(Font::Medium, 12.0, pal.foreground, dvec2(ui.x + ui.w - aw, ui.y), None, &amount);
    ui.y += s.y;
    if let Some(reset) = reset {
        let text = ta("providerUsage.timing.resetsIn", &[("duration", &reset)]);
        let rw = ui.measure(Font::Regular, 12.0, &text, None).x;
        let s = ui.text(Font::Regular, 12.0, pal.muted, dvec2(ui.x + ui.w - rw, ui.y), None, &text);
        ui.y += s.y;
    }
    ui.y += 3.0;
    if let Some(pct) = used_pct {
        let fill = tone_color(&pal, get(balance, "tone"), Some(pct));
        meter(ui, pct, fill, &pal);
    }
}

/// Original `UsageOptions` sheet: sidebar summary and used/remaining.
pub fn options_body(ui: &mut Ui, _s: &Settings, v: &View, _host: &str) {
    let visible = in_sidebar(&v.prefs);
    let stored = arr(&v.prefs, "sidebarFooterItems").to_vec();
    let toggle = handler(move |s, env| {
        let next = sidebar_nav_preferences(&[json!({"key": "usage", "visible": !visible})], &stored);
        s.save_pref(env, "sidebarFooterItems", Some(Value::Array(next)));
    });
    ui.card_begin();
    ui.row(row(t("providerUsage.showInSidebar")).hint(t("providerUsage.showInSidebarHint")).control(switch("usage.options.sidebar", visible, Some(toggle))));
    let usage_prefs = obj(&v.prefs, "usage").clone();
    let display = if usage_prefs.get("displayAs").and_then(Value::as_str) == Some("remaining") { "remaining" } else { "used" };
    let pick: Pick = std::rc::Rc::new(move |s, env, value| {
        let mut next = usage_prefs.clone();
        next.insert("displayAs".into(), Value::String(value));
        s.save_pref(env, "usage", Some(Value::Object(next)));
    });
    let seg = Ctl::Segmented {
        key: "usage.options.display".into(),
        items: ["used", "remaining"]
            .iter()
            .map(|value| {
                let pick = pick.clone();
                let label = t(if *value == "used" { "providerUsage.displayUsed" } else { "providerUsage.displayRemaining" });
                let selected = display == *value;
                (label, selected, (!selected).then(|| handler(move |s, env| pick(s, env, value.to_string()))))
            })
            .collect(),
    };
    ui.row(row(t("providerUsage.displayAs")).control(seg));
    ui.card_end();
}
