//! Host pages backed by the daemon config (`daemon_settings.dart`): config +
//! provider snapshot loading, agents (MCP tools, system prompt, skills),
//! workspaces, host configuration, terminals, daemon-connected hosts and Hub
//! access.

use serde_json::{Map, Value, json};

use crate::features::settings::host::{HostView, Status};
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, get, lines_text, obj, pairs_text, scalar_text, str_of, str_or};
use crate::features::settings::model::forms::{FormSave, FormSheet, Parse, SettingField};
use crate::features::settings::model::profile_form::radix36;
use crate::features::settings::model::provider_catalog::{default_terminal_profiles, provider_entries};
use crate::features::settings::rpc::{Env, Handler, ReqError, handler, not_manageable};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::field::FieldSpec;
use crate::features::settings::ui::kit::{BSize, Ctl, Ui, Variant, button, danger_icon_button, icon_button, row, switch};
use crate::features::settings::ui::overlay::SheetHead;

use std::rc::Rc;

// ------------------------------------------------------------ data

#[derive(Default)]
pub struct DaemonData {
    pub config: Value,
    pub providers: Vec<Value>,
    pub loading: bool,
    pub error: Option<ReqError>,
    pub generation: u64,
    pub loaded_for: Option<Status>,
    pub busy: bool,
}

pub fn data<'a>(s: &'a Settings, host: &str) -> Option<&'a DaemonData> {
    s.daemon.get(host)
}

pub fn reset(s: &mut Settings, host: &str) {
    if let Some(d) = s.daemon.get_mut(host) {
        d.loaded_for = None;
    }
    s.skills.remove(host);
    s.hub.remove(host);
    s.connected_hosts.remove(host);
}

pub fn ensure_daemon(s: &mut Settings, env: &mut Env, host: &HostView) {
    let current = s.daemon.get(&host.server_id).and_then(|d| d.loaded_for);
    if current == Some(host.status) {
        return;
    }
    load(s, env, host);
}

/// Flutter `_DaemonSettingsState.load`.
pub fn load(s: &mut Settings, env: &mut Env, host: &HostView) {
    let generation = s.next_id();
    let id = host.server_id.clone();
    let d = s.daemon.entry(id.clone()).or_default();
    d.generation = generation;
    d.loaded_for = Some(host.status);
    d.error = None;
    if !host.connected() {
        d.loading = false;
        d.config = json!({});
        d.providers = Vec::new();
        return;
    }
    d.loading = true;
    let host_id = id.clone();
    s.request(env, &id, "get_daemon_config_request", json!({}), "get_daemon_config_response", move |s, env, config| {
        let still = |s: &Settings| s.daemon.get(&host_id).map(|d| d.generation) == Some(generation);
        if !still(s) {
            return;
        }
        match config {
            Err(error) => {
                if let Some(d) = s.daemon.get_mut(&host_id) {
                    d.error = Some(error);
                    d.loading = false;
                }
            }
            Ok(config) => {
                let host_id2 = host_id.clone();
                s.request(env, &host_id, "get_providers_snapshot_request", json!({}), "get_providers_snapshot_response", move |s, _, snapshot| {
                    let Some(d) = s.daemon.get_mut(&host_id2) else { return };
                    if d.generation != generation {
                        return;
                    }
                    d.loading = false;
                    match snapshot {
                        Ok(snapshot) => {
                            d.config = get(&config, "config").clone();
                            d.providers = provider_entries(&snapshot);
                        }
                        Err(error) => d.error = Some(error),
                    }
                });
            }
        }
    });
}

/// Completion of a background provider snapshot reload.
pub type Reloaded = Box<dyn FnOnce(&mut Settings, &mut Env, Result<(), ReqError>)>;

/// Re-reads the provider snapshot without the page-level loading state.
pub fn reload_providers(s: &mut Settings, env: &mut Env, host: &str, then: Option<Reloaded>) {
    let generation = s.daemon.get(host).map(|d| d.generation).unwrap_or(0);
    let host_id = host.to_string();
    s.request(env, host, "get_providers_snapshot_request", json!({}), "get_providers_snapshot_response", move |s, env, result| {
        let outcome = match result {
            Ok(snapshot) => {
                if let Some(d) = s.daemon.get_mut(&host_id)
                    && d.generation == generation
                {
                    d.providers = provider_entries(&snapshot);
                }
                Ok(())
            }
            Err(error) => Err(error),
        };
        if let Some(then) = then {
            then(s, env, outcome);
        }
    });
}

/// Original `useProvidersSnapshot().refresh(providers)`.
pub fn refresh_providers(s: &mut Settings, env: &mut Env, host: &str, ids: Vec<String>, then: impl FnOnce(&mut Settings, &mut Env, Result<(), ReqError>) + 'static) {
    let host_id = host.to_string();
    s.request(env, host, "refresh_providers_snapshot_request", json!({"providers": ids}), "refresh_providers_snapshot_response", move |s, env, result| match result {
        Err(error) => then(s, env, Err(error)),
        Ok(_) => reload_providers(s, env, &host_id, Some(Box::new(then))),
    });
}

/// Flutter `patch`: `set_daemon_config_request` with the returned config.
pub fn patch(s: &mut Settings, env: &mut Env, host: &str, value: Value, then: impl FnOnce(&mut Settings, &mut Env, Result<(), ReqError>) + 'static) {
    let manageable = env.host(host).map(|h| h.can_manage()).unwrap_or(false);
    if !manageable {
        then(s, env, Err(not_manageable()));
        return;
    }
    let generation = s.daemon.get(host).map(|d| d.generation).unwrap_or(0);
    let host_id = host.to_string();
    s.request(env, host, "set_daemon_config_request", json!({"config": value}), "set_daemon_config_response", move |s, env, result| match result {
        Ok(response) => {
            if let Some(d) = s.daemon.get_mut(&host_id)
                && d.generation == generation
            {
                d.config = get(&response, "config").clone();
            }
            then(s, env, Ok(()));
        }
        Err(error) => then(s, env, Err(error)),
    });
}

/// Flutter `run`: one busy operation, errors shown as a toast.
pub fn run_patch(s: &mut Settings, env: &mut Env, host: &str, value: Value) {
    if s.daemon.get(host).map(|d| d.busy).unwrap_or(false) {
        return;
    }
    if let Some(d) = s.daemon.get_mut(host) {
        d.busy = true;
    }
    let host_id = host.to_string();
    patch(s, env, host, value, move |s, env, result| {
        if let Some(d) = s.daemon.get_mut(&host_id) {
            d.busy = false;
        }
        if let Err(error) = result {
            env.error(&error);
        }
    });
}

pub fn on_host_event(s: &mut Settings, env: &mut Env, host: &str, event: &Value) {
    let payload = get(event, "payload");
    match str_of(event, "type") {
        "status" if str_of(payload, "status") == "daemon_config_changed" => {
            if let Some(d) = s.daemon.get_mut(host) {
                d.config = get(payload, "config").clone();
            }
        }
        "providers_snapshot_update" if payload.get("cwd").map(Value::is_null).unwrap_or(true) => {
            super::profiles::on_snapshot_update(s, env, host, payload);
            if !s.daemon.contains_key(host) {
                return;
            }
            if payload.get("entries").is_some() || payload.get("compactSnapshot").is_some() {
                if let Some(d) = s.daemon.get_mut(host) {
                    d.providers = provider_entries(payload);
                }
            } else {
                reload_providers(s, env, host, None);
            }
        }
        _ => {}
    }
}

/// Error / loading states shared by every daemon page; `true` when drawn.
fn draw_state(ui: &mut Ui, s: &Settings, host: &HostView) -> bool {
    let Some(d) = data(s, &host.server_id) else {
        loading(ui);
        return true;
    };
    if let Some(error) = &d.error {
        let id = host.server_id.clone();
        let retry = button(format!("daemon.{id}.retry"), t("common.actions.retry"), Some(handler(move |s, env| {
            if let Some(host) = env.host(&id) {
                load(s, env, &host);
            }
        })))
        .ctl();
        ui.alert(false, &error.text(), None, Some(retry));
        ui.gap(24.0);
        return true;
    }
    if d.loading {
        loading(ui);
        return true;
    }
    false
}

fn loading(ui: &mut Ui) {
    let (x, y, w) = (ui.x, ui.y, ui.w);
    ui.draw_ctl(Ctl::Spinner(24.0), makepad_widgets::dvec2(x + (w - 24.0) / 2.0, y + 78.0));
    ui.y += 180.0;
}

/// The boolean daemon config value a [`toggle`] row edits.
struct ConfigSwitch {
    group: Option<&'static str>,
    key: &'static str,
    /// Shown when the config has no value.
    fallback: bool,
    /// Confirms before switching to this value (it can cut connections).
    confirm: Option<SwitchConfirm>,
}

/// A destructive confirmation for one direction of a [`ConfigSwitch`].
struct SwitchConfirm {
    when: bool,
    title: String,
    message: String,
    action: String,
}

/// Flutter `toggle`: a switch row writing `{group: {key: v}}` or `{key: v}`.
fn toggle(ui: &mut Ui, s: &Settings, host: &HostView, switch_of: ConfigSwitch, label: String, hint: Option<String>) {
    let ConfigSwitch { group, key, fallback, confirm } = switch_of;
    let d = data(s, &host.server_id);
    let config = d.map(|d| &d.config).cloned().unwrap_or(Value::Null);
    let values = match group {
        Some(g) => get(&config, g).clone(),
        None => config,
    };
    let on = values.get(key).and_then(Value::as_bool).unwrap_or(fallback);
    let enabled = !d.map(|d| d.busy).unwrap_or(false) && host.can_manage();
    let id = host.server_id.clone();
    let confirm = confirm.filter(|c| c.when == !on).map(|c| (c.title, c.message, c.action));
    let act = enabled.then(|| {
        handler(move |s, env| {
            let value = match group {
                Some(g) => json!({ g: { key: !on } }),
                None => json!({ key: !on }),
            };
            match &confirm {
                Some((title, message, action)) => {
                    let id = id.clone();
                    let go = handler(move |s, env| run_patch(s, env, &id, value.clone()));
                    s.confirm(title.clone(), message.clone(), action.clone(), true, go);
                }
                None => run_patch(s, env, &id, value),
            }
        })
    });
    ui.row(row(label).hint_opt(hint).control(switch(format!("daemon.{}.{}.{key}", host.server_id, group.unwrap_or("")), on, act)));
}

// ------------------------------------------------------------ agents page

pub fn draw_agents(ui: &mut Ui, s: &Settings, v: &super::View, host: &HostView) {
    let disconnected = !host.connected();
    if !disconnected && draw_state(ui, s, host) {
        return;
    }
    if disconnected {
        ui.empty_card(&t("settings.host.agents.unavailable"), None, false);
        ui.gap(24.0);
    } else {
        ui.section_title(&t("settings.hostSections.agents"), None, None);
        ui.card_begin();
        toggle(ui, s, host, ConfigSwitch { group: Some("mcp"), key: "injectIntoAgents", fallback: true, confirm: None }, t("settings.host.orchestration.enableTools.title"), Some(t("settings.host.orchestration.enableTools.hint")));
        ui.card_end();
        ui.gap(12.0);
        ui.card_begin();
        let id = host.server_id.clone();
        let edit = host.can_manage().then(|| {
            handler(move |s, _| {
                let initial = data(s, &id).map(|d| str_of(&d.config, "appendSystemPrompt").to_string()).unwrap_or_default();
                let sheet = s.next_id();
                s.push_sheet(sheet, SheetKind::SystemPrompt(SystemPromptSheet { host: id.clone(), initial, saving: false }), 560.0);
            })
        });
        ui.row(
            row(t("settings.host.orchestration.systemPrompt.title"))
                .hint(t("settings.host.orchestration.systemPrompt.hint"))
                .control(button(format!("daemon.{}.prompt", host.server_id), t("settings.host.orchestration.systemPrompt.edit"), edit).ctl()),
        );
        ui.card_end();
        ui.gap(24.0);
    }
    draw_skills(ui, s, host);
    if !disconnected && host.can_manage() && host.feature("agentProfiles") {
        super::profiles::draw_profiles(ui, s, v, host);
    }
}

/// The archive-merged option at the end of the Projects page.
pub fn draw_workspace_options(ui: &mut Ui, s: &Settings, host: &HostView) {
    // Secondary to the project list: it shows once the config is loaded; a
    // disconnected host or failed load is already explained above it.
    let ready = data(s, &host.server_id).is_some_and(|d| !d.loading && d.error.is_none());
    if !host.connected() || !ready {
        return;
    }
    ui.section_title(&t("settings.hostSections.workspaces"), None, None);
    ui.card_begin();
    toggle(ui, s, host, ConfigSwitch { group: None, key: "autoArchiveAfterMerge", fallback: false, confirm: None }, t("settingsNative.archiveMerged"), Some(t("settingsNative.archiveMergedHint")));
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ host configuration

fn report_request(s: &mut Settings, env: &mut Env, host: &str, kind: &'static str, response: &'static str, title: String, field: Option<&'static str>) {
    if s.daemon.get(host).map(|d| d.busy).unwrap_or(false) {
        return;
    }
    if let Some(d) = s.daemon.get_mut(host) {
        d.busy = true;
    }
    let host_id = host.to_string();
    s.request(env, host, kind, json!({}), response, move |s, env, result| {
        if let Some(d) = s.daemon.get_mut(&host_id) {
            d.busy = false;
        }
        match result {
            Ok(value) => {
                let shown = field.map(|f| get(&value, f).clone()).unwrap_or(value);
                s.report(title, &shown);
            }
            Err(error) => env.error(&error),
        }
    });
}

/// Host configuration and diagnostics rows (part of the host overview page).
pub fn draw_host_config(ui: &mut Ui, s: &Settings, host: &HostView) {
    if !host.connected() {
        return;
    }
    if draw_state(ui, s, host) {
        return;
    }
    let d = data(s, &host.server_id).expect("drawn after load");
    let busy = d.busy;
    let manage = host.can_manage();
    let id = host.server_id.clone();
    ui.section_title(&t("settingsNative.hostConfiguration"), None, None);
    ui.card_begin();
    let mut hint = vec![host.server_id.clone()];
    if let Some(perms) = host.server_info.get("permissions").and_then(Value::as_array) {
        let list = perms.iter().map(scalar_text).collect::<Vec<_>>().join(", ");
        hint.push(ta("settingsNative.grantedPermissions", &[("permissions", &list)]));
    }
    if !manage {
        hint.push(t("settingsNative.readOnlyConfig"));
    }
    let refresh_id = id.clone();
    let refresh = handler(move |s, env| {
        if let Some(host) = env.host(&refresh_id) {
            load(s, env, &host);
        }
    });
    ui.row(row(t("settingsNative.serverId")).hint(hint.join("\n")).control(icon_button(format!("daemon.{id}.refresh"), Icon::RotateCw, Some(refresh)).tipped(t("usage.refresh"))));
    let diag_id = id.clone();
    let diag = (!busy).then(|| handler(move |s, env| report_request(s, env, &diag_id, "diagnostics.request", "diagnostics.response", t("settingsNative.hostDiagnostics"), Some("diagnostic"))));
    ui.row(row(t("settingsNative.hostDiagnostics")).control(button(format!("daemon.{id}.diag"), t("settings.diagnostics.app.run"), diag).ctl()));
    let status_id = id.clone();
    let status = (!busy).then(|| handler(move |s, env| report_request(s, env, &status_id, "daemon.get_status.request", "daemon.get_status.response", t("desktop.daemon.fullStatus.modalTitle"), None)));
    ui.row(row(t("desktop.daemon.fullStatus.modalTitle")).control(button(format!("daemon.{id}.status"), t("desktop.daemon.fullStatus.view"), status).ctl()));
    let relay_off = SwitchConfirm { when: false, title: t("settingsNative.relayOffTitle"), message: t("settingsNative.relayOffMessage"), action: t("settingsNative.relayOffAction") };
    toggle(ui, s, host, ConfigSwitch { group: Some("relay"), key: "enabled", fallback: false, confirm: Some(relay_off) }, t("settingsNative.relayEnabled"), Some(t("settingsNative.relayEnabledHint")));
    if obj(&d.config, "relay").contains_key("requireProtectedSecurity") {
        let protect_on = SwitchConfirm { when: true, title: t("settingsNative.requireProtectedTitle"), message: t("settingsNative.requireProtectedMessage"), action: t("settingsNative.requireProtectedAction") };
        toggle(ui, s, host, ConfigSwitch { group: Some("relay"), key: "requireProtectedSecurity", fallback: false, confirm: Some(protect_on) }, t("settingsNative.requireProtectedRelay"), Some(t("settingsNative.requireProtectedRelayHint")));
    }
    let limits_id = id.clone();
    let limits = (manage && !busy).then(|| handler(move |s, env| edit_network_limits(s, env, &limits_id)));
    ui.row(
        row(t("settingsNative.networkLimits"))
            .hint(t("settingsNative.networkLimitsHint"))
            .control(button(format!("daemon.{id}.limits"), t("settings.host.orchestration.systemPrompt.edit"), limits).ctl()),
    );
    let reload_id = id.clone();
    let reload = (manage && !busy).then(|| {
        handler(move |s, env| {
            if let Some(d) = s.daemon.get_mut(&reload_id) {
                d.busy = true;
            }
            let host_id = reload_id.clone();
            s.request(env, &reload_id, "daemon.config.reload.request", json!({}), "daemon.config.reload.response", move |s, env, result| {
                if let Some(d) = s.daemon.get_mut(&host_id) {
                    d.busy = false;
                }
                match result {
                    Ok(report) => {
                        let sheet = s.next_id();
                        s.push_sheet(sheet, SheetKind::ConfigReload(ConfigReloadSheet::new(&host_id, &report)), 560.0);
                        if let Some(host) = env.host(&host_id) {
                            load(s, env, &host);
                        }
                    }
                    Err(error) => env.error(&error),
                }
            });
        })
    });
    ui.row(
        row(t("settingsNative.reloadConfig"))
            .hint(t("settingsNative.reloadConfigHint"))
            .control(button(format!("daemon.{id}.reload"), t("common.actions.reload"), reload).ctl()),
    );
    ui.card_end();
    ui.gap(24.0);
}

/// `daemon.config.reload.response` shown as the CLI's three lists, with a
/// Restart action when changes wait for a daemon restart.
pub struct ConfigReloadSheet {
    pub host: String,
    pub applied: Vec<String>,
    pub restart: Vec<String>,
    pub overrides: Vec<String>,
}

impl ConfigReloadSheet {
    fn new(host: &str, report: &Value) -> Self {
        let paths = |key: &str| arr(report, key).iter().map(scalar_text).filter(|p| !p.is_empty()).collect::<Vec<_>>();
        Self { host: host.to_string(), applied: paths("appliedPaths"), restart: paths("restartRequiredPaths"), overrides: paths("overrideControlledPaths") }
    }
}

pub fn config_reload_head(id: u64, p: &ConfigReloadSheet) -> SheetHead {
    let mut head = SheetHead::new(t("settingsNative.configReload"));
    if !p.restart.is_empty() {
        let host = p.host.clone();
        let restart = handler(move |s, env| {
            s.close_sheet(env, id);
            super::host::restart(s, env, host.clone());
        });
        head.actions.push(button(format!("sheet.{id}.restart"), t("settings.host.daemon.restart.title"), Some(restart)).icon(Icon::RotateCw).ctl());
    }
    head.actions.push(button(format!("sheet.{id}.closebtn"), t("common.actions.close"), Some(handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    head
}

pub fn config_reload_body(ui: &mut Ui, p: &ConfigReloadSheet) {
    let pal = ui.pal;
    if p.applied.is_empty() && p.restart.is_empty() && p.overrides.is_empty() {
        let size = ui.text(Font::Regular, 14.0, pal.muted, makepad_widgets::dvec2(ui.x, ui.y), Some(ui.w), &t("settingsNative.reloadNoChanges"));
        ui.y += size.y;
        return;
    }
    let groups = [
        ("settingsNative.reloadRestartRequired", &p.restart, pal.warning),
        ("settingsNative.reloadOverrides", &p.overrides, pal.muted),
        ("settingsNative.reloadApplied", &p.applied, pal.success),
    ];
    let mut first = true;
    for (title, paths, tone) in groups {
        if paths.is_empty() {
            continue;
        }
        if !first {
            ui.y += 16.0;
        }
        first = false;
        let size = ui.text(Font::Medium, 14.0, tone, makepad_widgets::dvec2(ui.x, ui.y), Some(ui.w), &t(title));
        ui.y += size.y + 6.0;
        for path in paths {
            let size = ui.text(Font::Code, 12.0, pal.foreground, makepad_widgets::dvec2(ui.x + 8.0, ui.y), Some(ui.w - 8.0), path);
            ui.y += size.y + 4.0;
        }
    }
}

fn wildcard_text(value: &Value) -> String {
    if value.as_bool() == Some(true) { "*".into() } else { lines_text(value.as_array().map(Vec::as_slice).unwrap_or(&[])) }
}

fn int_text(value: &Value, fallback: i64) -> String {
    value.as_i64().unwrap_or(fallback).to_string()
}

/// Flutter `editNetworkLimits`.
fn edit_network_limits(s: &mut Settings, _env: &mut Env, host: &str) {
    let config = data(s, host).map(|d| d.config.clone()).unwrap_or(Value::Null);
    let git = get(&config, "git");
    let fields = vec![
        SettingField::text("hostnames", t("settingsNative.hostnames"), wildcard_text(get(&config, "hostnames"))).multiline().parse(Parse::LinesOrWildcard),
        SettingField::text("trustedProxies", t("settingsNative.trustedProxies"), wildcard_text(get(&config, "trustedProxies"))).multiline().parse(Parse::LinesOrWildcard),
        SettingField::text("origins", t("settingsNative.corsOrigins"), lines_text(arr(get(&config, "cors"), "allowedOrigins"))).multiline().parse(Parse::Lines),
        SettingField::text("rate", t("settingsNative.gitRate"), int_text(get(git, "maxProcessesPerSecond"), 10)).parse(Parse::PositiveInt),
        SettingField::text("concurrency", t("settingsNative.gitConcurrency"), int_text(get(git, "maxProcessConcurrency"), 4)).parse(Parse::PositiveInt),
        SettingField::text("catalogRefreshTimeoutMs", t("settingsNative.catalogTimeout"), int_text(get(&config, "catalogRefreshTimeoutMs"), 60000)).parse(Parse::PositiveInt),
    ];
    let host_id = host.to_string();
    let save: FormSave = Rc::new(move |s, env, id, v| {
        let value = json!({
            "hostnames": v["hostnames"],
            "trustedProxies": v["trustedProxies"],
            "cors": {"allowedOrigins": v["origins"]},
            "git": {"maxProcessesPerSecond": v["rate"], "maxProcessConcurrency": v["concurrency"]},
            "catalogRefreshTimeoutMs": v["catalogRefreshTimeoutMs"],
        });
        patch(s, env, &host_id, value, move |s, env, result| s.form_done(env, id, result));
    });
    let sheet = s.next_id();
    let form = FormSheet::new(sheet, t("settingsNative.networkLimits"), fields, save);
    s.push_sheet(sheet, SheetKind::Form(form), 600.0);
}

/// Flutter `providerForm`: label, command lines, env, Paseo tools.
pub fn edit_provider_config(s: &mut Settings, _env: &mut Env, host: &str, entry: &Value) {
    let id = str_of(entry, "provider").to_string();
    let existing = data(s, host).map(|d| obj(&d.config, "providers").get(&id).cloned().unwrap_or(Value::Null)).unwrap_or(Value::Null);
    let tools = get(&existing, "paseoTools");
    let label = existing.get("label").filter(|l| !l.is_null()).or_else(|| entry.get("label")).map(scalar_text).unwrap_or_default();
    let fields = vec![
        SettingField::text("label", t("settingsNative.displayName"), label),
        SettingField::text("command", t("settingsNative.commandLines"), lines_text(arr(&existing, "command"))).multiline().parse(Parse::Lines),
        SettingField::text("env", t("settingsNative.envLines"), pairs_text(obj(&existing, "env"))).multiline().parse(Parse::EnvPairs),
        SettingField::toggle("paseoTools", t("settingsNative.paseoTools"), tools.get("enabled").and_then(Value::as_bool) != Some(false)),
        SettingField::text("disabledTools", t("settingsNative.disabledTools"), lines_text(arr(tools, "disabledTools"))).multiline().parse(Parse::Lines),
    ];
    let host_id = host.to_string();
    let provider = id.clone();
    let save: FormSave = Rc::new(move |s, env, sheet, v| {
        let mut next: Map<String, Value> = existing.as_object().cloned().unwrap_or_default();
        next.insert("label".into(), v["label"].clone());
        if v["command"].as_array().map(|c| !c.is_empty()).unwrap_or(false) {
            next.insert("command".into(), v["command"].clone());
        }
        next.insert("env".into(), v["env"].clone());
        let mut tools = obj(&existing, "paseoTools").clone();
        tools.insert("enabled".into(), v["paseoTools"].clone());
        tools.insert("disabledTools".into(), v["disabledTools"].clone());
        next.insert("paseoTools".into(), Value::Object(tools));
        let value = json!({ "providers": { provider.clone(): Value::Object(next) } });
        patch(s, env, &host_id, value, move |s, env, result| s.form_done(env, sheet, result));
    });
    let sheet = s.next_id();
    let form = FormSheet::new(sheet, ta("settingsNative.providerConfig", &[("id", &id)]), fields, save);
    s.push_sheet(sheet, SheetKind::Form(form), 600.0);
}

// ------------------------------------------------------------ system prompt sheet

pub struct SystemPromptSheet {
    pub host: String,
    pub initial: String,
    pub saving: bool,
}

fn prompt_field(id: u64) -> String {
    format!("sheet.{id}.prompt")
}

pub fn system_prompt_head(p: &SystemPromptSheet) -> SheetHead {
    let mut head = SheetHead::new(t("settings.host.orchestration.systemPrompt.sheetTitle"));
    head.dismissible = !p.saving;
    head
}

pub fn system_prompt_body(ui: &mut Ui, id: u64, p: &SystemPromptSheet) {
    let field = prompt_field(id);
    let text = if ui.fields.exists(&field) { ui.fields.text(&field) } else { p.initial.clone() };
    let changed = text != p.initial;
    let spec = FieldSpec::new(field.clone(), p.initial.clone()).multiline().placeholder(t("settings.host.orchestration.systemPrompt.placeholder")).enabled(!p.saving).autofocus();
    let h = ui.multiline_height(&text, 96.0).max(96.0);
    let (x, w) = (ui.x, ui.w);
    let (fill, border) = (ui.pal.surface1, ui.pal.border);
    ui.rect(makepad_widgets::Rect { pos: makepad_widgets::dvec2(x, ui.y), size: makepad_widgets::dvec2(w, h) }, fill, Some(border), 8.0);
    ui.draw_ctl(Ctl::Field { spec, w, h }, makepad_widgets::dvec2(x, ui.y));
    ui.y += h + 16.0;
    let initial = p.initial.clone();
    let reset_field = field.clone();
    let reset = (changed && !p.saving).then(|| handler(move |_, env| env.fields.set_text(env.cx, &reset_field, &initial)));
    let host = p.host.clone();
    let save = (changed && !p.saving).then(|| {
        handler(move |s, env| {
            let value = env.fields.text(&prompt_field(id));
            if let Some(SheetKind::SystemPrompt(p)) = s.sheet_mut(id) {
                p.saving = true;
            }
            patch(s, env, &host, json!({"appendSystemPrompt": value}), move |s, env, result| {
                if let Some(SheetKind::SystemPrompt(p)) = s.sheet_mut(id) {
                    p.saving = false;
                }
                match result {
                    Ok(()) => s.close_sheet(env, id),
                    Err(error) => env.error(&error),
                }
            });
        })
    });
    let key = if p.saving { "saving" } else { "save" };
    let controls = Ctl::Row(
        vec![
            button(format!("sheet.{id}.reset"), t("settings.host.orchestration.systemPrompt.reset"), reset).variant(Variant::Ghost).ctl(),
            button(format!("sheet.{id}.save"), t(&format!("settings.host.orchestration.systemPrompt.{key}")), save).variant(Variant::Primary).ctl(),
        ],
        8.0,
    );
    let size = ui.measure_ctl(&controls);
    let pos = makepad_widgets::dvec2(ui.x + ui.w - size.x, ui.y);
    ui.draw_ctl(controls, pos);
    ui.y += size.y;
}

// ------------------------------------------------------------ terminals

pub struct TerminalProfileSheet {
    pub host: String,
    pub profile: Option<Value>,
    pub name_error: Option<String>,
    pub command_error: Option<String>,
    pub submit_error: Option<String>,
    pub pending: bool,
}

fn terminal_profiles(s: &Settings, host: &str) -> Vec<Value> {
    match data(s, host).and_then(|d| d.config.get("terminalProfiles")) {
        Some(Value::Array(list)) => list.clone(),
        _ => default_terminal_profiles(),
    }
}

fn save_terminal_profiles(s: &mut Settings, env: &mut Env, host: &str, next: Vec<Value>) {
    patch(s, env, host, json!({"terminalProfiles": next}), |_, env, result| {
        if let Err(error) = result {
            env.error(&error);
        }
    });
}

/// Protocol `getTerminalProfileIcon`: the explicit icon, else a provider glyph
/// whose name equals the command's base name (`cursor-agent` → `cursor`).
fn terminal_profile_icon(profile: &Value) -> Option<String> {
    let explicit = str_of(profile, "icon");
    if !explicit.is_empty() {
        return Some(explicit.to_string());
    }
    let command = str_of(profile, "command");
    let start = command.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
    let mut base = command[start..].to_lowercase();
    if let Some(dot) = base.find('.').filter(|d| *d > 0) {
        base.truncate(dot);
    }
    if base == "cursor-agent" {
        return Some("cursor".into());
    }
    crate::common::provider_icon::svg_for(&base, None).map(|_| base)
}

pub fn draw_terminals(ui: &mut Ui, s: &Settings, host: &HostView) {
    let disconnected = !host.connected();
    if !disconnected && draw_state(ui, s, host) {
        return;
    }
    ui.section_title(&t("settingsNative.terminalAgents"), None, None);
    if !disconnected {
        ui.card_begin();
        toggle(ui, s, host, ConfigSwitch { group: None, key: "enableTerminalAgentHooks", fallback: false, confirm: None }, t("settingsNative.terminalHooks"), Some(t("settingsNative.terminalHooksHint")));
        ui.card_end();
    }
    ui.gap(24.0);
    if disconnected {
        ui.empty_card(&t("settings.host.terminalProfiles.unavailable"), None, false);
        return;
    }
    let manage = host.can_manage();
    let profiles = terminal_profiles(s, &host.server_id);
    let id = host.server_id.clone();
    let add = manage.then(|| {
        let host_id = id.clone();
        handler(move |s, _| open_terminal_profile(s, &host_id, None))
    });
    ui.section_title(&t("settings.host.terminalProfiles.sectionTitle"), None, Some(icon_button(format!("terminals.{id}.add"), Icon::Plus, add).tipped(t("settings.host.terminalProfiles.addProfileTitle"))));
    if profiles.is_empty() {
        ui.empty_card(&t("settings.host.terminalProfiles.emptyState"), None, false);
        ui.gap(24.0);
        return;
    }
    ui.card_begin();
    let pal = ui.pal;
    for (i, profile) in profiles.iter().enumerate() {
        ui.card_row_separator();
        let pid = str_of(profile, "id").to_string();
        let mover = |next: Vec<Value>| {
            let host_id = id.clone();
            handler(move |s, env| save_terminal_profiles(s, env, &host_id, next.clone()))
        };
        let up = (manage && i > 0).then(|| {
            let mut next = profiles.clone();
            next.swap(i - 1, i);
            mover(next)
        });
        let down = (manage && i + 1 < profiles.len()).then(|| {
            let mut next = profiles.clone();
            next.swap(i, i + 1);
            mover(next)
        });
        let edit = manage.then(|| {
            let host_id = id.clone();
            let p = profile.clone();
            handler(move |s, _| open_terminal_profile(s, &host_id, Some(p.clone())))
        });
        let remove = manage.then(|| {
            let host_id = id.clone();
            let name = str_of(profile, "name").to_string();
            let remaining: Vec<Value> = profiles.iter().filter(|p| str_of(p, "id") != pid).cloned().collect();
            handler(move |s, _| {
                let host_id = host_id.clone();
                let remaining = remaining.clone();
                let go = handler(move |s, env| save_terminal_profiles(s, env, &host_id, remaining.clone()));
                let tp = |k: &str| t(&format!("settings.host.terminalProfiles.{k}"));
                s.confirm(tp("removeConfirmTitle"), ta("settings.host.terminalProfiles.removeConfirmMessage", &[("name", &name)]), tp("remove"), true, go);
            })
        });
        let command = std::iter::once(str_of(profile, "command").to_string()).chain(arr(profile, "args").iter().map(scalar_text)).collect::<Vec<_>>().join(" ");
        let lead = match terminal_profile_icon(profile) {
            Some(provider) => Ctl::Provider(provider, None, 16.0, pal.muted),
            None => Ctl::Glyph(Icon::SquareTerminal, 16.0, pal.muted),
        };
        let controls = Ctl::Row(
            vec![
                icon_button(format!("terminals.{id}.{pid}.up"), Icon::ArrowUp, up).tipped(t("settings.host.terminalProfiles.moveUp")),
                icon_button(format!("terminals.{id}.{pid}.down"), Icon::ArrowDown, down).tipped(t("settings.host.terminalProfiles.moveDown")),
                icon_button(format!("terminals.{id}.{pid}.edit"), Icon::Pencil, edit).tipped(t("settings.host.terminalProfiles.editProfile")),
                danger_icon_button(format!("terminals.{id}.{pid}.remove"), Icon::Trash, remove).tipped(t("settings.host.terminalProfiles.remove")),
            ],
            0.0,
        );
        ui.row(row(str_or(profile, "name", "").to_string()).one_line().hint(command).leading(lead).inline().control(controls));
    }
    ui.card_end();
    ui.gap(24.0);
}

fn open_terminal_profile(s: &mut Settings, host: &str, profile: Option<Value>) {
    let sheet = s.next_id();
    s.push_sheet(
        sheet,
        SheetKind::TerminalProfile(TerminalProfileSheet { host: host.to_string(), profile, name_error: None, command_error: None, submit_error: None, pending: false }),
        480.0,
    );
}

fn tp(key: &str) -> String {
    t(&format!("settings.host.terminalProfiles.{key}"))
}

pub fn terminal_profile_head(id: u64, p: &TerminalProfileSheet) -> SheetHead {
    let mut head = SheetHead::new(tp(if p.profile.is_none() { "addProfileTitle" } else { "editProfileTitle" }));
    head.dismissible = !p.pending;
    head.stretch_actions = true;
    head.actions.push(button(format!("sheet.{id}.cancel"), t("common.actions.cancel"), (!p.pending).then(|| handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    head.actions.push(button(format!("sheet.{id}.save"), tp(if p.pending { "saving" } else { "save" }), (!p.pending).then(|| handler(move |s, env| save_terminal_profile(s, env, id)))).variant(Variant::Primary).ctl());
    head
}

fn save_terminal_profile(s: &mut Settings, env: &mut Env, id: u64) {
    let name = env.fields.text(&format!("sheet.{id}.name")).trim().to_string();
    let command = env.fields.text(&format!("sheet.{id}.command")).trim().to_string();
    let args_text = env.fields.text(&format!("sheet.{id}.args"));
    let Some(SheetKind::TerminalProfile(p)) = s.sheet_mut(id) else { return };
    if p.pending {
        return;
    }
    p.submit_error = None;
    p.name_error = name.is_empty().then(|| tp("nameRequired"));
    p.command_error = command.is_empty().then(|| tp("commandRequired"));
    if p.name_error.is_some() || p.command_error.is_some() {
        return;
    }
    p.pending = true;
    let host = p.host.clone();
    let original = p.profile.clone();
    let args: Option<Vec<Value>> = if args_text.trim().is_empty() {
        None
    } else {
        Some(args_text.split_whitespace().map(|a| Value::String(a.to_string())).collect())
    };
    let apply = |base: &Value| {
        let mut map = base.as_object().cloned().unwrap_or_default();
        map.insert("name".into(), Value::String(name.clone()));
        map.insert("command".into(), Value::String(command.clone()));
        match &args {
            Some(list) => {
                map.insert("args".into(), Value::Array(list.clone()));
            }
            None => {
                map.remove("args");
            }
        }
        Value::Object(map)
    };
    let profiles = terminal_profiles(s, &host);
    let next: Vec<Value> = match &original {
        None => {
            let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let random = uuid::Uuid::new_v4().as_u128() & 0xffff_ffff;
            let mut list = profiles;
            list.push(apply(&json!({"id": format!("profile_{}_{}", radix36(millis), radix36(random))})));
            list
        }
        Some(profile) => profiles.iter().map(|p| if p.get("id") == profile.get("id") { apply(p) } else { p.clone() }).collect(),
    };
    patch(s, env, &host, json!({"terminalProfiles": next}), move |s, env, result| match result {
        Ok(()) => s.close_sheet(env, id),
        Err(error) => {
            if let Some(SheetKind::TerminalProfile(p)) = s.sheet_mut(id) {
                p.pending = false;
                let message = error.text();
                p.submit_error = Some(if message.is_empty() { t("common.errors.unableToSave") } else { message });
            }
        }
    });
}

pub fn terminal_profile_body(ui: &mut Ui, id: u64, p: &TerminalProfileSheet) {
    let profile = p.profile.clone().unwrap_or(Value::Null);
    let enabled = !p.pending;
    let command_id = format!("sheet.{id}.command");
    let args_id = format!("sheet.{id}.args");
    let focus_command = {
        let command_id = command_id.clone();
        handler(move |_, env| env.fields.focus(&command_id))
    };
    let focus_args = {
        let args_id = args_id.clone();
        handler(move |_, env| env.fields.focus(&args_id))
    };
    let name = FieldSpec::new(format!("sheet.{id}.name"), str_of(&profile, "name")).placeholder(tp("namePlaceholder")).enabled(enabled).autofocus().on_submit(focus_command);
    ui.form_field(Some(&tp("nameLabel")), None, p.name_error.as_deref(), name, 40.0);
    ui.gap(16.0);
    let command = FieldSpec::new(command_id, str_of(&profile, "command")).placeholder(tp("commandPlaceholder")).enabled(enabled).on_submit(focus_args);
    ui.form_field(Some(&tp("commandLabel")), None, p.command_error.as_deref(), command, 40.0);
    ui.gap(16.0);
    let args_text = arr(&profile, "args").iter().map(scalar_text).collect::<Vec<_>>().join(" ");
    let save = handler(move |s, env| save_terminal_profile(s, env, id));
    let args = FieldSpec::new(args_id, args_text).placeholder(tp("argsPlaceholder")).enabled(enabled).on_submit(save);
    ui.form_field(Some(&tp("argsLabel")), Some(&tp("argsHint")), None, args, 40.0);
    if let Some(error) = &p.submit_error {
        ui.gap(16.0);
        let color = ui.pal.field_error;
        ui.para(error, 14.0, color);
    }
    ui.gap(8.0);
}

// ------------------------------------------------------------ daemon-connected hosts

#[derive(Default)]
pub struct ConnectedHostsState {
    pub hosts: Vec<Value>,
    pub error: Option<String>,
    pub loading: bool,
    pub loaded: bool,
}

pub fn ensure_connected_hosts(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || !host.feature("connectedHosts") {
        return;
    }
    if s.connected_hosts.get(&host.server_id).map(|c| c.loaded || c.loading).unwrap_or(false) {
        return;
    }
    load_connected_hosts(s, env, &host.server_id);
}

fn load_connected_hosts(s: &mut Settings, env: &mut Env, host: &str) {
    s.connected_hosts.entry(host.to_string()).or_default().loading = true;
    let host_id = host.to_string();
    s.request(env, host, "hosts.list.request", json!({}), "hosts.list.response", move |s, _, result| {
        let state = s.connected_hosts.entry(host_id).or_default();
        state.loading = false;
        state.loaded = true;
        match result {
            Ok(r) => {
                state.hosts = arr(get(&r, "directory"), "hosts").to_vec();
                state.error = None;
            }
            Err(error) => state.error = Some(error.text()),
        }
    });
}

fn edit_connected_host(s: &mut Settings, host: &str, existing: Option<Value>) {
    let editing = existing.is_some();
    let current = existing.clone().unwrap_or(Value::Null);
    let mut fields = vec![SettingField::text("label", t("settingsNative.label"), str_of(&current, "label"))];
    let mut connection = SettingField::text("connection", t(if editing { "settingsNative.connectionLinkKeep" } else { "settingsNative.connectionLink" }), "").secret();
    if !editing {
        connection = connection.required();
    }
    fields.push(connection);
    if editing {
        fields.push(SettingField::toggle("enabled", t("settingsNative.enabled"), current.get("enabled").and_then(Value::as_bool) != Some(false)));
        fields.push(SettingField::toggle("replaceIdentity", t("settingsNative.replaceIdentity"), false));
    }
    fields.push(SettingField::text("metadata", t("settingsNative.metadataLines"), pairs_text(obj(&current, "metadata"))).multiline().parse(Parse::KeyPairs));
    let host_id = host.to_string();
    let save: FormSave = Rc::new(move |s, env, sheet, mut v| {
        if v.get("connection").and_then(Value::as_str).map(str::is_empty).unwrap_or(true) {
            v.remove("connection");
        }
        let (kind, response, fields) = match &existing {
            None => ("hosts.add.request", "hosts.add.response", json!({"input": Value::Object(v)})),
            Some(entry) => ("hosts.update.request", "hosts.update.response", json!({"registrationId": get(entry, "registrationId"), "patch": Value::Object(v)})),
        };
        let host = host_id.clone();
        s.request(env, &host_id, kind, fields, response, move |s, env, result| match result {
            Ok(_) => {
                s.form_done(env, sheet, Ok(()));
                load_connected_hosts(s, env, &host);
            }
            Err(error) => s.form_done(env, sheet, Err(error)),
        });
    });
    let sheet = s.next_id();
    let form = FormSheet::new(sheet, t(if editing { "settingsNative.editConnectedHost" } else { "settingsNative.registerConnectedHost" }), fields, save)
        .describe(t("settingsNative.connectedHostsDescription"));
    s.push_sheet(sheet, SheetKind::Form(form), 600.0);
}

pub fn draw_connected_hosts(ui: &mut Ui, s: &Settings, host: &HostView) {
    let id = host.server_id.clone();
    let state = s.connected_hosts.get(&id);
    let loading = state.map(|c| c.loading).unwrap_or(true);
    let info = t("settingsNative.connectedHostsInfo");
    let refresh_id = id.clone();
    let refresh = (!loading).then(|| handler(move |s, env| load_connected_hosts(s, env, &refresh_id)));
    let add_id = id.clone();
    let add = handler(move |s, _| edit_connected_host(s, &add_id, None));
    // Flutter labels the add button with the untranslated "Register host".
    let trailing = Ctl::Row(
        vec![
            icon_button(format!("chosts.{id}.refresh"), Icon::RotateCw, refresh).tipped(t("usage.refresh")),
            icon_button(format!("chosts.{id}.add"), Icon::Plus, Some(add)).tipped("Register host"),
        ],
        0.0,
    );
    ui.section_title(&t("settingsNative.connectedHosts"), Some((format!("chosts.{id}.info"), info)), Some(trailing));
    match state {
        Some(ConnectedHostsState { error: Some(error), .. }) => ui.alert(false, error, None, None),
        Some(state) if !state.hosts.is_empty() => {
            ui.card_begin();
            for (i, entry) in state.hosts.iter().enumerate() {
                let label = str_or(entry, "label", str_or(entry, "serverId", str_of(entry, "registrationId"))).to_string();
                let mut hint = vec![format!("{} · {}", scalar_text(get(entry, "status")), scalar_text(get(entry, "targetDescription")))];
                if let Some(message) = get(entry, "error").get("message").and_then(Value::as_str) {
                    hint.push(message.to_string());
                }
                let edit_id = id.clone();
                let edit_entry = entry.clone();
                let edit = handler(move |s, _| edit_connected_host(s, &edit_id, Some(edit_entry.clone())));
                let remove_id = id.clone();
                let registration = get(entry, "registrationId").clone();
                let remove = handler(move |s, _| {
                    let host = remove_id.clone();
                    let registration = registration.clone();
                    let go = handler(move |s, env| {
                        let host2 = host.clone();
                        s.request(env, &host, "hosts.remove.request", json!({"registrationId": registration}), "hosts.remove.response", move |s, env, result| match result {
                            Ok(_) => load_connected_hosts(s, env, &host2),
                            Err(error) => env.error(&error),
                        });
                    });
                    s.confirm(t("settingsNative.removeConnectedHostTitle"), t("settingsNative.removeConnectedHostMessage"), t("settings.host.connections.removeAction"), true, go);
                });
                ui.row(
                    row(label)
                        .hint(hint.join("\n"))
                        .tap(format!("chosts.{id}.{i}.row"), edit)
                        .control(danger_icon_button(format!("chosts.{id}.{i}.remove"), Icon::Trash, Some(remove)).tipped(t("settings.host.connections.removeAction"))),
                );
            }
            ui.card_end();
        }
        _ => {
            let text = if loading { t("settings.providers.loading") } else { t("settingsNative.noConnectedHosts") };
            ui.empty_card(&text, None, false);
        }
    }
    ui.gap(24.0);
}

// ------------------------------------------------------------ hub access

#[derive(Default)]
pub struct HubState {
    pub status: Value,
    pub error: Option<String>,
    pub loaded: bool,
}

pub const DAEMON_PERMISSIONS: [&str; 9] = [
    "daemon.read",
    "daemon.manage",
    "tunnel.manage",
    "access.manage",
    "workspace.read",
    "workspace.write",
    "workspace.manage",
    "automation.manage",
    "hub.execute",
];

pub fn ensure_hub(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || s.hub.get(&host.server_id).map(|h| h.loaded).unwrap_or(false) {
        return;
    }
    s.hub.entry(host.server_id.clone()).or_default().loaded = true;
    hub_request(s, env, &host.server_id, "get_status", json!({}), None);
}

fn hub_request(s: &mut Settings, env: &mut Env, host: &str, action: &str, fields: Value, sheet: Option<u64>) {
    let host_id = host.to_string();
    s.request(
        env,
        host,
        &format!("hub.management.daemon.{action}.request"),
        fields,
        &format!("hub.management.daemon.{action}.response"),
        move |s, env, result| {
            let outcome = match result {
                Ok(r) => {
                    let state = s.hub.entry(host_id).or_default();
                    state.status = get(&r, "status").clone();
                    state.error = None;
                    Ok(())
                }
                Err(error) => {
                    s.hub.entry(host_id).or_default().error = Some(error.text());
                    Err(error)
                }
            };
            if let Some(sheet) = sheet {
                s.form_done(env, sheet, outcome);
            }
        },
    );
}

pub fn draw_hub(ui: &mut Ui, s: &Settings, host: &HostView) {
    let id = host.server_id.clone();
    let state = s.hub.get(&id);
    let status = state.map(|h| h.status.clone()).unwrap_or(Value::Null);
    let editable = status.as_object().map(|m| !m.is_empty()).unwrap_or(false) && str_of(&status, "state") != "not_connected";
    let problem = state.and_then(|h| h.error.clone()).or_else(|| status.get("lastError").filter(|e| !e.is_null()).map(scalar_text));
    let refresh_id = id.clone();
    let refresh = handler(move |s, env| hub_request(s, env, &refresh_id, "get_status", json!({}), None));
    ui.section_title(&t("settingsNative.hubAccess"), None, Some(icon_button(format!("hub.{id}.refresh"), Icon::RotateCw, Some(refresh)).tipped(t("usage.refresh"))));
    ui.card_begin();
    let mut hint = vec![str_or(&status, "state", &t("settings.providers.loading")).to_string()];
    if let Some(origin) = status.get("hubOrigin").filter(|o| !o.is_null()) {
        hint.push(scalar_text(origin));
    }
    let connect_id = id.clone();
    let connect = handler(move |s, _| {
        let fields = vec![
            SettingField::text("hubUrl", t("settingsNative.hubUrl"), "").required(),
            SettingField::text("token", t("settingsNative.registrationToken"), "").required().secret(),
        ];
        let host = connect_id.clone();
        let save: FormSave = Rc::new(move |s, env, sheet, v| {
            let mut fields = v;
            fields.insert("permissions".into(), json!([]));
            hub_request(s, env, &host, "connect", Value::Object(fields), Some(sheet));
        });
        let sheet = s.next_id();
        let form = FormSheet::new(sheet, t("settingsNative.connectHub"), fields, save);
        s.push_sheet(sheet, SheetKind::Form(form), 600.0);
    });
    let disconnect_id = id.clone();
    let disconnect = editable.then(|| {
        handler(move |s, _| {
            let host = disconnect_id.clone();
            let go = handler(move |s, env| hub_request(s, env, &host, "disconnect", json!({}), None));
            s.confirm(t("settingsNative.disconnectHubTitle"), t("settingsNative.disconnectHubMessage"), t("settingsNative.disconnect"), true, go);
        })
    });
    let controls = Ctl::Row(
        vec![
            button(format!("hub.{id}.connect"), t("settingsNative.connectHub"), Some(connect)).ctl(),
            button(format!("hub.{id}.disconnect"), t("settingsNative.disconnect"), disconnect).ctl(),
        ],
        8.0,
    );
    ui.row(row(t("settingsNative.relationship")).hint(hint.join("\n")).error(problem).control(controls));
    let granted = arr(&status, "permissions").to_vec();
    for permission in DAEMON_PERMISSIONS {
        let on = granted.iter().any(|p| p.as_str() == Some(permission));
        let perm_id = id.clone();
        let act = editable.then(|| {
            handler(move |s, env| {
                let fields = if on { json!({"revoke": [permission]}) } else { json!({"grant": [permission]}) };
                hub_request(s, env, &perm_id, "permissions.update", fields, None);
            })
        });
        checkbox_row(ui, &format!("hub.{id}.{permission}"), permission, None, on, act);
    }
    ui.card_end();
    ui.gap(24.0);
}

/// Flutter `SettingsCheckboxRow`.
pub fn checkbox_row(ui: &mut Ui, key: &str, label: &str, hint: Option<String>, checked: bool, act: Option<Handler>) {
    let fade = if act.is_some() { 1.0 } else { 0.5 };
    let mut r = row(label.to_string()).hint_opt(hint).leading(Ctl::Checkbox { key: format!("{key}.box"), on: checked, act: act.clone() });
    if fade < 1.0 {
        r.muted_label = true;
    }
    if let Some(act) = act {
        r = r.tap(key.to_string(), act);
    }
    ui.row(r);
}

// ------------------------------------------------------------ skills

#[derive(Default)]
pub struct SkillsState {
    pub status: Option<Value>,
    pub working: bool,
    pub requested: bool,
}

const SKILLS_DOCS: &str = "https://paseo.sh/docs/skills";

fn sk(key: &str) -> String {
    t(&format!("settings.host.skills.{key}"))
}

pub fn ensure_skills(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || !host.feature("skillManagement") || s.skills.get(&host.server_id).map(|k| k.requested).unwrap_or(false) {
        return;
    }
    s.skills.entry(host.server_id.clone()).or_default().requested = true;
    load_skills(s, env, &host.server_id);
}

fn load_skills(s: &mut Settings, env: &mut Env, host: &str) {
    let host_id = host.to_string();
    s.request(env, host, "agent.skills.get_status.request", json!({}), "agent.skills.get_status.response", move |s, env, result| match result {
        Ok(status) => s.skills.entry(host_id).or_default().status = Some(status),
        Err(_) => env.toast(sk("statusFailed"), true),
    });
}

fn mutate_skills(s: &mut Settings, env: &mut Env, host: &str, name: &'static str, failure: &'static str) {
    let state = s.skills.entry(host.to_string()).or_default();
    if state.working {
        return;
    }
    state.working = true;
    let host_id = host.to_string();
    s.request(env, host, &format!("agent.skills.{name}.request"), json!({}), &format!("agent.skills.{name}.response"), move |s, env, result| {
        s.skills.entry(host_id.clone()).or_default().working = false;
        match result {
            Ok(_) => load_skills(s, env, &host_id),
            Err(_) => env.toast(sk(failure), true),
        }
    });
}

fn maintenance_ops(status: &Value) -> Vec<Value> {
    arr(status, "ops").iter().filter(|op| str_of(op, "kind") != "delete").cloned().collect()
}

pub fn draw_skills(ui: &mut Ui, s: &Settings, host: &HostView) {
    let id = host.server_id.clone();
    let docs = Ctl::Link { key: format!("skills.{id}.docs"), label: sk("docs"), url: SKILLS_DOCS.into(), leading: true };
    ui.section_title(&sk("sectionTitle"), None, Some(docs));
    let connected = host.connected();
    let pal = ui.pal;
    if !connected || !host.feature("skillManagement") {
        ui.card_begin();
        ui.card_row_separator();
        let (x, y, w) = (ui.x, ui.y, ui.w);
        let text = sk(if connected { "unsupported" } else { "unavailable" });
        let size = ui.text(Font::Regular, 12.0, pal.muted, makepad_widgets::dvec2(x + 16.0, y + 16.0), Some(w - 32.0), &text);
        ui.y += size.y + 32.0;
        ui.card_end();
        ui.gap(24.0);
        return;
    }
    let state = s.skills.get(&id);
    let status = state.and_then(|k| k.status.clone());
    let working = state.map(|k| k.working).unwrap_or(false);
    let selection = status.as_ref().map(|st| get(st, "selection").clone()).unwrap_or(Value::Null);
    let available: Vec<String> = status.as_ref().map(|st| arr(st, "available").iter().map(scalar_text).collect()).unwrap_or_default();
    let raw_state = status.as_ref().map(|st| str_of(st, "state").to_string()).unwrap_or_default();
    let ops = status.as_ref().map(maintenance_ops).unwrap_or_default();
    let skill_state = if raw_state == "drift" && ops.is_empty() { "up-to-date".to_string() } else { raw_state };
    let has_selected = (str_of(&selection, "mode") == "all" && !available.is_empty())
        || (str_of(&selection, "mode") == "custom" && arr(&selection, "skills").iter().any(|skill| available.iter().any(|a| Some(a.as_str()) == skill.as_str())));
    let enabled = host.can_manage() && !working;
    let outline = |key: &str, label: String, act: Handler| button(format!("skills.{id}.{key}"), label, enabled.then_some(act)).size(BSize::Sm).ctl();
    let choose_id = id.clone();
    let choose_available = available.clone();
    let choose_selection = selection.clone();
    let choose = (status.is_some() && !working).then(|| {
        handler(move |s, _| {
            let all = str_of(&choose_selection, "mode") == "all";
            let skills: Vec<String> = arr(&choose_selection, "skills").iter().map(scalar_text).collect();
            let sheet = s.next_id();
            s.push_sheet(
                sheet,
                SheetKind::Skills(SkillsSheet { host: choose_id.clone(), available: choose_available.clone(), all, skills, error: None, saving: false }),
                520.0,
            );
        })
    });
    let mut actions = vec![icon_button(format!("skills.{id}.choose"), Icon::Settings2, choose).tipped(sk("choose"))];
    let uninstall_id = id.clone();
    let uninstall = handler(move |s, _| {
        let host = uninstall_id.clone();
        let go = handler(move |s, env| mutate_skills(s, env, &host, "uninstall", "uninstallFailed"));
        s.confirm(sk("uninstallTitle"), sk("uninstallMessage"), sk("actions.uninstall"), true, go);
    });
    if has_selected {
        match skill_state.as_str() {
            "up-to-date" => {
                actions.push(Ctl::Row(vec![Ctl::Glyph(Icon::Check, 14.0, pal.muted), crate::features::settings::ui::kit::small(sk("actions.installed"), pal.muted)], 4.0));
                actions.push(outline("uninstall", sk("actions.uninstall"), uninstall));
            }
            "drift" => {
                let reconcile_id = id.clone();
                let ordered_ops = ops.clone();
                let reconcile = handler(move |s, _| {
                    let mut ops = ordered_ops.clone();
                    let order = |k: &str| match k {
                        "add" => 0,
                        "update" => 1,
                        "delete" => 2,
                        _ => 0,
                    };
                    ops.sort_by(|a, b| order(str_of(a, "kind")).cmp(&order(str_of(b, "kind"))).then(str_of(a, "name").cmp(str_of(b, "name"))));
                    let message = if ops.is_empty() {
                        sk("updateFallback")
                    } else {
                        ops.iter().map(|op| format!("{} {}", sk(&format!("operations.{}", str_of(op, "kind"))), str_of(op, "name"))).collect::<Vec<_>>().join("\n")
                    };
                    let host = reconcile_id.clone();
                    let go = handler(move |s, env| mutate_skills(s, env, &host, "reconcile", "updateFailed"));
                    s.confirm(sk("updateTitle"), message, sk("actions.update"), false, go);
                });
                actions.push(outline("update", sk(if working { "actions.working" } else { "actions.update" }), reconcile));
                actions.push(outline("uninstall", sk("actions.uninstall"), uninstall));
            }
            _ => {
                let install_id = id.clone();
                let install = handler(move |s, env| mutate_skills(s, env, &install_id, "reconcile", "updateFailed"));
                actions.push(outline("install", sk(if working { "actions.installing" } else { "actions.install" }), install));
            }
        }
    }
    ui.card_begin();
    let description = sk(if skill_state == "drift" { "updateAvailable" } else { "description" });
    ui.row(row(sk("title")).hint(description).leading(Ctl::Glyph(Icon::Blocks, 16.0, pal.foreground)).control(Ctl::Row(actions, 8.0)));
    ui.card_end();
    ui.gap(24.0);
}

pub struct SkillsSheet {
    pub host: String,
    pub available: Vec<String>,
    pub all: bool,
    pub skills: Vec<String>,
    pub error: Option<String>,
    pub saving: bool,
}

fn skills_draft(p: &SkillsSheet) -> Value {
    if p.all { json!({"mode": "all"}) } else { json!({"mode": "custom", "skills": p.skills}) }
}

fn save_skills(s: &mut Settings, env: &mut Env, id: u64, removals: Vec<Value>, retry: bool) {
    let Some(SheetKind::Skills(p)) = s.sheet_mut(id) else { return };
    p.error = None;
    p.saving = true;
    let host = p.host.clone();
    let draft = skills_draft(p);
    s.skills.entry(host.clone()).or_default().working = true;
    let host_id = host.clone();
    s.request(env, &host, "agent.skills.save_selection.request", json!({"selection": draft, "confirmedRemovals": removals}), "agent.skills.save_selection.response", move |s, env, result| {
        s.skills.entry(host_id.clone()).or_default().working = false;
        let set_error = |s: &mut Settings| {
            if let Some(SheetKind::Skills(p)) = s.sheet_mut(id) {
                p.saving = false;
                p.error = Some(sk("saveFailed"));
            }
        };
        match result {
            Err(_) => {
                env.toast(sk("saveSelectionFailed"), true);
                set_error(s);
            }
            Ok(attempt) => {
                load_skills(s, env, &host_id);
                let required = attempt.get("confirmationRequired").filter(|c| !c.is_null());
                match required {
                    None => s.close_sheet(env, id),
                    Some(_) if retry => set_error(s),
                    Some(required) => {
                        let removals = arr(required, "removals").to_vec();
                        if let Some(SheetKind::Skills(p)) = s.sheet_mut(id) {
                            p.saving = false;
                        }
                        let names = removals.iter().map(scalar_text).collect::<Vec<_>>().join(", ");
                        let go = handler(move |s, env| save_skills(s, env, id, removals.clone(), true));
                        s.confirm(sk("removeTitle"), ta("settings.host.skills.removeMessage", &[("skills", &names)]), sk("actions.remove"), true, go);
                    }
                }
            }
        }
    });
}

pub fn skills_head(id: u64, p: &SkillsSheet) -> SheetHead {
    let mut head = SheetHead::new(sk("choose"));
    head.stretch_actions = true;
    head.dismissible = !p.saving;
    head.actions.push(button(format!("sheet.{id}.cancel"), t("common.actions.cancel"), (!p.saving).then(|| handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    head.actions.push(
        button(format!("sheet.{id}.save"), sk(if p.saving { "actions.saving" } else { "actions.save" }), Some(handler(move |s, env| save_skills(s, env, id, Vec::new(), false))))
            .variant(Variant::Primary)
            .loading(p.saving)
            .ctl(),
    );
    head
}

pub fn skills_body(ui: &mut Ui, id: u64, p: &SkillsSheet) {
    let pal = ui.pal;
    ui.card_begin();
    let toggle_all = (!p.saving).then(|| {
        handler(move |s, _| {
            if let Some(SheetKind::Skills(p)) = s.sheet_mut(id) {
                p.all = !p.all;
                if !p.all {
                    p.skills = p.available.clone();
                }
            }
        })
    });
    ui.row(row(sk("chooseAll")).hint(sk("chooseAllHint")).control(switch(format!("sheet.{id}.all"), p.all, toggle_all)));
    ui.card_end();
    ui.gap(16.0);
    ui.section_title(&sk("chooseList"), None, None);
    ui.card_begin();
    if p.available.is_empty() {
        ui.card_row_separator();
        let (x, y, w) = (ui.x, ui.y, ui.w);
        let size = ui.text(Font::Regular, 12.0, pal.muted, makepad_widgets::dvec2(x + 16.0, y + 16.0), Some(w - 32.0), &sk("chooseEmpty"));
        ui.y += size.y + 32.0;
    }
    for name in &p.available {
        let checked = p.all || p.skills.contains(name);
        let skill = name.clone();
        let act = (!p.all && !p.saving).then(|| {
            handler(move |s, _| {
                if let Some(SheetKind::Skills(p)) = s.sheet_mut(id) {
                    if p.skills.contains(&skill) {
                        p.skills.retain(|x| *x != skill);
                    } else {
                        p.skills.push(skill.clone());
                        p.skills.sort();
                    }
                }
            })
        });
        checkbox_row(ui, &format!("sheet.{id}.skill.{name}"), name, None, checked, act);
    }
    ui.card_end();
    if let Some(error) = &p.error {
        ui.gap(16.0);
        ui.para(error, 12.0, pal.danger);
    }
}
