//! Original `HostPluginsPage` (`plugins-page.tsx`): global switch, source
//! install, plugin rows with enable/reload/remove/logs and the logs sheet.
//! Plugin-contributed settings screens are executable UI plugins and stay excluded.

use serde_json::{Value, json};

use super::daemon;
use crate::features::settings::host::HostView;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, get, str_of};
use crate::features::settings::model::plugins::{PageState, installation, resolve_page};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::field::FieldSpec;
use crate::features::settings::ui::kit::{Ctl, Lead, Ui, Variant, button, row, switch};
use crate::features::settings::ui::overlay::{Menu, SheetHead, item};

const SOURCE_DOCS: &str = "https://paseo.sh/docs/plugins/reference#plugin-sources";

#[derive(Default)]
pub struct PluginsState {
    pub plugins: Option<Vec<Value>>,
    pub loading: bool,
    pub error: Option<String>,
    pub requested: bool,
    /// The running action: (action, plugin id).
    pub pending: Option<(String, Option<String>)>,
    pub feedback: Option<(bool, String)>,
    /// A list refresh requested while one runs (original refresh queue).
    pub refresh_again: bool,
}

fn pt(key: &str) -> String {
    t(&format!("settings.plugins.{key}"))
}

fn source_field(host: &str) -> String {
    format!("plugins.{host}.source")
}

pub fn reset(s: &mut Settings, host: &str) {
    if let Some(p) = s.plugins.get_mut(host) {
        p.requested = false;
    }
}

pub fn ensure_plugins(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || !host.feature("pluginManagement") {
        return;
    }
    if s.plugins.get(&host.server_id).map(|p| p.requested).unwrap_or(false) {
        return;
    }
    s.plugins.entry(host.server_id.clone()).or_default().requested = true;
    refresh(s, env, &host.server_id);
}

/// Serialized list refreshes (original `refreshQueue`).
fn refresh(s: &mut Settings, env: &mut Env, host: &str) {
    let state = s.plugins.entry(host.to_string()).or_default();
    if state.loading {
        state.refresh_again = true;
        return;
    }
    state.loading = true;
    let host_id = host.to_string();
    s.request(env, host, "plugin.list.request", json!({}), "plugin.list.response", move |s, env, result| {
        let state = s.plugins.entry(host_id.clone()).or_default();
        state.loading = false;
        match result {
            Ok(r) => {
                state.plugins = Some(arr(&r, "plugins").to_vec());
                state.error = None;
            }
            Err(error) => state.error = Some(error.text()),
        }
        if std::mem::take(&mut state.refresh_again) {
            refresh(s, env, &host_id);
        }
    });
}

/// `status.plugin_catalog_changed` refreshes the list.
pub fn on_host_event(s: &mut Settings, env: &mut Env, host: &str, event: &Value) {
    if str_of(event, "type") == "status" && str_of(get(event, "payload"), "status") == "plugin_catalog_changed" && s.plugins.get(host).map(|p| p.requested).unwrap_or(false) {
        refresh(s, env, host);
    }
}

fn finish(s: &mut Settings, env: &mut Env, host: &str, result: Result<String, ReqError>) {
    let state = s.plugins.entry(host.to_string()).or_default();
    state.pending = None;
    match result {
        Ok(message) => {
            state.feedback = Some((true, message));
            refresh(s, env, host);
        }
        Err(error) => state.feedback = Some((false, error.text())),
    }
}

fn begin(s: &mut Settings, host: &str, action: &str, plugin: Option<String>) -> bool {
    let state = s.plugins.entry(host.to_string()).or_default();
    if state.pending.is_some() {
        return false;
    }
    state.feedback = None;
    state.pending = Some((action.to_string(), plugin));
    true
}

fn plugin_action(s: &mut Settings, env: &mut Env, host: &str, action: &'static str, id: String) {
    if action == "remove" {
        let host_id = host.to_string();
        let plugin = id.clone();
        let go = handler(move |s, env| {
            if !begin(s, &host_id, "remove", Some(plugin.clone())) {
                return;
            }
            let host2 = host_id.clone();
            let plugin2 = plugin.clone();
            s.request(env, &host_id, "plugin.remove.request", json!({"pluginId": plugin}), "plugin.remove.response", move |s, env, result| {
                finish(s, env, &host2, result.map(|_| ta("settings.plugins.feedback.remove", &[("id", &plugin2)])));
            });
        });
        // Declining keeps the plugin with the original "Kept" feedback.
        let kept_host = host.to_string();
        let kept_id = id.clone();
        let kept = handler(move |s, _| {
            s.plugins.entry(kept_host.clone()).or_default().feedback = Some((true, ta("settings.plugins.feedback.kept", &[("id", &kept_id)])));
        });
        s.confirm_or(ta("settings.plugins.removeConfirmTitle", &[("id", &id)]), pt("removeConfirmMessage"), pt("actions.remove"), true, go, kept);
        return;
    }
    if !begin(s, host, action, Some(id.clone())) {
        return;
    }
    let host_id = host.to_string();
    let timeout = if action == "reload" { None } else { Some(std::time::Duration::from_secs(60)) };
    s.request_timeout(env, host, &format!("plugin.{action}.request"), json!({"pluginId": id}), &format!("plugin.{action}.response"), timeout, move |s, env, result| {
        finish(s, env, &host_id, result.map(|_| ta(&format!("settings.plugins.feedback.{action}"), &[("id", &id)])));
    });
}

fn install(s: &mut Settings, env: &mut Env, host: &str) {
    let field = source_field(host);
    let source = env.fields.text(&field).trim().to_string();
    if source.is_empty() || !begin(s, host, "install", None) {
        return;
    }
    let host_id = host.to_string();
    s.request_timeout(env, host, "plugin.source.install.request", json!({"source": source}), "plugin.source.install.response", Some(std::time::Duration::from_secs(300)), move |s, env, result| {
        let outcome = result.map(|r| {
            env.fields.set_text(env.cx, &source_field(&host_id), "");
            ta("settings.plugins.feedback.installed", &[("id", str_of(get(&r, "plugin"), "id"))])
        });
        finish(s, env, &host_id, outcome);
    });
}

fn toggle_global(s: &mut Settings, env: &mut Env, host: &str, enabled: bool) {
    if !begin(s, host, if enabled { "global-enable" } else { "global-disable" }, None) {
        return;
    }
    let host_id = host.to_string();
    daemon::patch(s, env, host, json!({"pluginsEnabled": enabled}), move |s, env, result| {
        let message = pt(if enabled { "feedback.globalEnabled" } else { "feedback.globalDisabled" });
        finish(s, env, &host_id, result.map(|_| message));
    });
}

pub fn draw_plugins(ui: &mut Ui, s: &Settings, host: &HostView) {
    let id = host.server_id.clone();
    let default = PluginsState::default();
    let state = s.plugins.get(&id).unwrap_or(&default);
    let page = resolve_page(host.connected(), host.feature("pluginManagement"), state.plugins.is_none() && state.error.is_none(), state.error.is_some() && state.plugins.is_none(), state.plugins.as_ref().map(Vec::len).unwrap_or(0));
    match page {
        PageState::Offline => {
            ui.warning(&pt("states.offlineTitle"), Some(&pt("states.offlineDescription")));
            return;
        }
        PageState::Unsupported => {
            ui.warning(&pt("states.updateTitle"), None);
            return;
        }
        _ => {}
    }
    let pal = ui.pal;
    let pending = state.pending.is_some();
    ui.section_title(&pt("title"), None, None);
    ui.warning(&pt("trustedTitle"), Some(&pt("trustedDescription")));
    ui.gap(12.0);
    ui.card_begin();
    let enabled = daemon::data(s, &id).map(|d| d.config.get("pluginsEnabled").and_then(Value::as_bool) == Some(true)).unwrap_or(false);
    let global_id = id.clone();
    let global = (!pending).then(|| handler(move |s, env| toggle_global(s, env, &global_id, !enabled)));
    ui.row(row(pt("globalTitle")).hint(pt("globalHint")).control(switch(format!("plugins.{id}.global"), enabled, global)));
    ui.card_end();
    ui.gap(12.0);
    if host.feature("pluginSourceInstallation") {
        let (x, y, w) = (ui.x, ui.y, ui.w);
        let card = ui.rect(makepad_widgets::Rect { pos: makepad_widgets::dvec2(x, y), size: makepad_widgets::dvec2(w, 1.0) }, pal.surface1, Some(pal.border), 8.0);
        ui.x = x + 16.0;
        ui.w = w - 32.0;
        ui.y = y + 16.0;
        let docs = Ctl::Link { key: format!("plugins.{id}.docs"), label: pt("docs"), url: SOURCE_DOCS.into(), leading: false };
        let docs_w = ui.measure_ctl(&docs).x;
        ui.text(Font::Regular, 14.0, pal.muted, makepad_widgets::dvec2(ui.x, ui.y), Some(ui.w - docs_w - 8.0), &pt("sourceLabel"));
        ui.draw_ctl(docs, makepad_widgets::dvec2(ui.x + ui.w - docs_w, ui.y + 2.0));
        ui.gap(28.0);
        let install_id = id.clone();
        let submit = handler(move |s, env| install(s, env, &install_id));
        let field = source_field(&id);
        let can_submit = !ui.fields.text(&field).trim().is_empty() && !pending;
        let spec = FieldSpec::new(field, "").placeholder(pt("sourcePlaceholder")).enabled(!pending).on_submit(submit.clone());
        let fw = ui.w;
        ui.place(Ctl::Field { spec, w: fw, h: 40.0 });
        ui.gap(12.0);
        let installing = matches!(&state.pending, Some((action, _)) if action == "install");
        ui.place(button(format!("plugins.{id}.install"), pt(if installing { "installing" } else { "install" }), can_submit.then_some(submit)).variant(Variant::Primary).loading(installing).ctl());
        ui.gap(16.0);
        card.set_rect(ui.cx, &makepad_widgets::Rect { pos: makepad_widgets::dvec2(x, y), size: makepad_widgets::dvec2(w, ui.y - y) });
        ui.x = x;
        ui.w = w;
    } else {
        ui.warning(&pt("states.sourceUpdateTitle"), None);
    }
    ui.gap(12.0);
    if let Some((success, message)) = &state.feedback {
        ui.alert(*success, message, None, None);
        ui.gap(12.0);
    }
    match page {
        PageState::Loading => ui.alert(true, &pt("states.loading"), None, None),
        PageState::Error => {
            let retry_id = id.clone();
            let retry = button(format!("plugins.{id}.retry"), pt("states.retry"), Some(handler(move |s, env| refresh(s, env, &retry_id)))).ctl();
            ui.alert(false, &pt("states.errorTitle"), state.error.as_deref(), Some(retry));
        }
        PageState::Empty => {
            ui.card_begin();
            ui.card_row_separator();
            let text = pt("states.empty");
            let tw = ui.measure(Font::Regular, 12.0, &text, None).x;
            ui.text(Font::Regular, 12.0, pal.muted, makepad_widgets::dvec2(ui.x + (ui.w - tw) / 2.0, ui.y + 16.0), None, &text);
            ui.gap(48.0);
            ui.card_end();
        }
        _ => {
            ui.card_begin();
            let logs_supported = host.feature("pluginLogs");
            for plugin in state.plugins.as_deref().unwrap_or(&[]) {
                draw_row(ui, &id, plugin, state, logs_supported);
            }
            ui.card_end();
        }
    }
    ui.gap(24.0);
}

fn draw_row(ui: &mut Ui, host: &str, plugin: &Value, state: &PluginsState, logs_supported: bool) {
    let pal = ui.pal;
    let pid = str_of(plugin, "id").to_string();
    let status = str_of(plugin, "status").to_string();
    let tone = match status.as_str() {
        "running" => Some(pal.success),
        "failed" => Some(pal.danger),
        _ => None,
    };
    let badge = Ctl::Badge { label: t(&format!("settings.plugins.status.{status}")), tone, xs: false, dot: None, icon: None };
    let mut hint = Vec::new();
    let description = str_of(plugin, "description");
    if !description.is_empty() {
        hint.push(description.to_string());
    }
    if plugin.get("installation").map(Value::is_object).unwrap_or(false) {
        hint.push(installation(get(plugin, "installation")));
    }
    let pending = state.pending.is_some();
    let enabled = plugin.get("enabled").and_then(Value::as_bool) == Some(true);
    let toggle_host = host.to_string();
    let toggle_id = pid.clone();
    let toggle = (!pending).then(|| {
        handler(move |s, env| plugin_action(s, env, &toggle_host, if enabled { "disable" } else { "enable" }, toggle_id.clone()))
    });
    let menu_host = host.to_string();
    let menu_id = pid.clone();
    let pending_action = state.pending.as_ref().filter(|(_, p)| p.as_deref() == Some(pid.as_str())).map(|(a, _)| a.clone());
    let menu = (!pending).then(|| {
        handler(move |s, _| {
            let mut items = Vec::new();
            if logs_supported {
                let (h, p) = (menu_host.clone(), menu_id.clone());
                items.push(item(pt("logs.action"), handler(move |s, env| open_logs(s, env, &h, &p))));
            }
            let (h, p) = (menu_host.clone(), menu_id.clone());
            let reload_label = pt(if pending_action.as_deref() == Some("reload") { "actions.reloading" } else { "actions.reload" });
            items.push(item(reload_label, handler(move |s, env| plugin_action(s, env, &h, "reload", p.clone()))).enabled(enabled));
            let (h, p) = (menu_host.clone(), menu_id.clone());
            let remove_label = pt(if pending_action.as_deref() == Some("remove") { "actions.removing" } else { "actions.remove" });
            items.push(item(remove_label, handler(move |s, env| plugin_action(s, env, &h, "remove", p.clone()))).lead(Lead::Icon(Icon::Trash, None)).destructive());
            let menu = Menu::new(s, ta("settings.plugins.actions.menu", &[("id", &menu_id)]), items);
            s.open_menu(menu);
        })
    });
    let controls = Ctl::Row(
        vec![
            switch(format!("plugins.{host}.{pid}.enabled"), enabled, toggle),
            Ctl::IconButton { key: format!("plugins.{host}.{pid}.menu"), icon: Icon::MoreHorizontal, act: menu, destructive: false, size: 32.0, tip: None },
        ],
        8.0,
    );
    let error = Some(str_of(plugin, "error").to_string()).filter(|e| !e.is_empty());
    ui.row(row(pid.clone()).accessory(badge).hint(hint.join("\n")).error(error).control(controls));
}

// ------------------------------------------------------------ logs sheet

pub struct LogsSheet {
    pub host: String,
    pub plugin: String,
    pub entries: Option<Vec<Value>>,
    pub error: Option<String>,
    pub fetching: bool,
}

fn open_logs(s: &mut Settings, env: &mut Env, host: &str, plugin: &str) {
    let id = s.next_id();
    s.push_sheet(id, SheetKind::PluginLogs(LogsSheet { host: host.into(), plugin: plugin.into(), entries: None, error: None, fetching: false }), 600.0);
    fetch_logs(s, env, id);
}

fn fetch_logs(s: &mut Settings, env: &mut Env, id: u64) {
    let Some(SheetKind::PluginLogs(p)) = s.sheet_mut(id) else { return };
    if p.fetching {
        return;
    }
    p.fetching = true;
    let (host, plugin) = (p.host.clone(), p.plugin.clone());
    s.request(env, &host, "plugin.logs.get.request", json!({"pluginId": plugin}), "plugin.logs.get.response", move |s, _, result| {
        if let Some(SheetKind::PluginLogs(p)) = s.sheet_mut(id) {
            p.fetching = false;
            match result {
                Ok(r) => {
                    p.entries = Some(arr(&r, "entries").to_vec());
                    p.error = None;
                }
                Err(error) => p.error = Some(error.text()),
            }
        }
    });
}

pub fn logs_head(id: u64, p: &LogsSheet) -> SheetHead {
    let mut head = SheetHead::new(ta("settings.plugins.logs.title", &[("id", &p.plugin)]));
    let label = pt(if p.fetching { "logs.refreshing" } else { "logs.refresh" });
    head.header_actions.push(button(format!("sheet.{id}.refresh"), label, (!p.fetching).then(|| handler(move |s, env| fetch_logs(s, env, id)))).ctl());
    head
}

pub fn logs_body(ui: &mut Ui, p: &LogsSheet) {
    let pal = ui.pal;
    let centered = |ui: &mut Ui, text: &str| {
        let size = ui.measure(Font::Regular, 14.0, text, Some(ui.w));
        ui.text(Font::Regular, 14.0, pal.muted, makepad_widgets::dvec2(ui.x + (ui.w - size.x) / 2.0, ui.y + 24.0), Some(ui.w), text);
        ui.y += size.y + 48.0;
    };
    if let Some(error) = &p.error {
        ui.alert(false, &pt("logs.errorTitle"), Some(error), None);
        return;
    }
    let Some(entries) = &p.entries else {
        centered(ui, &pt("logs.loading"));
        return;
    };
    if entries.is_empty() {
        centered(ui, &pt("logs.empty"));
        return;
    }
    for entry in entries {
        let stderr = str_of(entry, "stream") == "stderr";
        let head = format!("{} {}", str_of(entry, "timestamp"), str_of(entry, "stream"));
        let message = str_of(entry, "message");
        let hs = ui.measure(Font::Code, 12.0, &head, Some(ui.w - 24.0));
        let ms = ui.measure(Font::Code, 14.0, message, Some(ui.w - 24.0));
        let (x, y, w) = (ui.x, ui.y, ui.w);
        ui.rect(makepad_widgets::Rect { pos: makepad_widgets::dvec2(x, y), size: makepad_widgets::dvec2(w, hs.y + ms.y + 28.0) }, pal.surface2, None, 6.0);
        ui.text(Font::Code, 12.0, if stderr { pal.danger } else { pal.muted }, makepad_widgets::dvec2(x + 12.0, y + 12.0), Some(w - 24.0), &head);
        ui.text(Font::Code, 14.0, pal.foreground, makepad_widgets::dvec2(x + 12.0, y + 16.0 + hs.y), Some(w - 24.0), message);
        ui.y += hs.y + ms.y + 28.0 + 8.0;
    }
}
