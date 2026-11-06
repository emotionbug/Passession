//! Host pages from `settings_screen.dart` and the restart/update cards of
//! `daemon_settings.dart`: overview (status badges, appearance, local daemon,
//! host configuration, update, danger zone), projects, connections and pairing.

use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::View;
use super::app::{Pick, select_ctl_lead};
use crate::common::{Route, UiEffect};
use crate::features::settings::host::{self, HostView, Status, active_connection_label, config_with, connection_badge, connection_label};
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{get, str_of};
use crate::features::settings::model::daemon_update::{self as du, Outcome};
use crate::features::settings::model::forms::{FormSave, FormSheet, SettingField};
use crate::features::settings::platform::{self, DESKTOP};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::kit::{BSize, Ctl, Lead, Ui, Variant, button, row};
use crate::features::settings::ui::theme::{IDENTITY_COLORS, identity_fill};

// ------------------------------------------------------------ no host / pairing

pub fn draw_no_host(ui: &mut Ui) {
    ui.section_title(&t("settings.groups.host"), None, None);
    ui.card_begin();
    let add = handler(|_, env| env.ctx.effects.push(UiEffect::OpenHostDialog));
    ui.row(row(t("settings.host.notFound")).control(button("nohost.add", t("settings.addHost"), Some(add)).ctl()));
    ui.card_end();
    ui.gap(24.0);
}

pub fn draw_pair_device(ui: &mut Ui, host: &HostView) {
    ui.section_title(&t("settings.host.pairDevices.title"), None, None);
    ui.card_begin();
    let id = host.server_id.clone();
    let pair = handler(move |_, env| env.ctx.effects.push(UiEffect::OpenHostPairing { host_id: id.clone() }));
    ui.row(
        row(t("settings.host.pairDevices.rowTitle"))
            .hint(t("settings.host.pairDevices.rowHint"))
            .control(button("pair.open", t("settings.host.pairDevices.rowTitle"), Some(pair)).ctl()),
    );
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ projects

/// Original `ProjectsScreen`; a row opens the project settings in place.
pub fn draw_projects(ui: &mut Ui, _s: &Settings, host: &HostView) {
    let pal = ui.pal;
    if host.projects.is_empty() {
        let (x, y, w) = (ui.x, ui.y, ui.w);
        if host.loading_projects {
            ui.draw_ctl(Ctl::Spinner(36.0), makepad_widgets::dvec2(x + (w - 36.0) / 2.0, y + 24.0));
            ui.y += 84.0;
        } else {
            let text = host.project_error.clone().or_else(|| host.error.clone()).unwrap_or_else(|| t("sidebar.project.empty.title"));
            let size = ui.measure(Font::Regular, 14.0, &text, Some(w - 48.0));
            ui.text(Font::Regular, 14.0, pal.muted, makepad_widgets::dvec2(x + (w - size.x) / 2.0, y + 24.0), Some(w - 48.0), &text);
            ui.y += size.y + 48.0;
        }
        return;
    }
    if let Some(error) = host.project_error.clone().or_else(|| host.error.clone()) {
        let text = ta("settings.projectList.hostLoadFailed", &[("hostName", &host.label), ("message", &error)]);
        let size = ui.measure(Font::Regular, 12.0, &text, Some(ui.w - 24.0));
        let (x, y, w) = (ui.x, ui.y, ui.w);
        ui.rect(makepad_widgets::Rect { pos: makepad_widgets::dvec2(x, y), size: makepad_widgets::dvec2(w, size.y + 24.0) }, pal.transparent, Some(pal.border), 8.0);
        ui.text(Font::Regular, 12.0, pal.field_error, makepad_widgets::dvec2(x + 12.0, y + 12.0), Some(w - 24.0), &text);
        ui.y += size.y + 24.0 + 12.0;
    }
    ui.card_begin();
    for (i, project) in host.projects.iter().enumerate() {
        let root = host::project_root(project);
        let name = host::project_name(project);
        let initial = name.chars().next().unwrap_or('?');
        let mut r = row(name)
            .one_line()
            .leading(Ctl::Lead(Lead::Monogram(initial), 16.0))
            .control(Ctl::Glyph(Icon::ChevronRight, 14.0, pal.muted))
            .inline();
        if !root.is_empty() {
            r = r.tap(format!("projects.{i}"), handler(move |s, _| s.project_root = Some(root.clone())));
        }
        ui.row(r);
    }
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ overview

fn status_badge(host: &HostView, pal: &crate::features::settings::ui::theme::Palette) -> Ctl {
    let dark = pal.dark;
    let (tone, dot, key) = match host.status {
        Status::Connected => (Some(pal.success), if dark { crate::features::settings::ui::theme::rgb(0x35c264) } else { crate::features::settings::ui::theme::rgb(0x299f51) }, "Online"),
        Status::Connecting => (Some(pal.warning), if dark { crate::features::settings::ui::theme::rgb(0xdb932e) } else { crate::features::settings::ui::theme::rgb(0xb37824) }, "Connecting"),
        Status::Error => (Some(pal.danger), if dark { crate::features::settings::ui::theme::rgb(0xf7796d) } else { crate::features::settings::ui::theme::rgb(0xf12e2f) }, "Error"),
        Status::Disconnected => (None, pal.muted, "Offline"),
    };
    Ctl::Badge { label: status_label(key), tone, xs: false, dot: Some(dot), icon: None }
}

/// Original `formatConnectionStatus` labels.
fn status_label(english: &str) -> String {
    let key = match english {
        "Online" => "settingsNative.statusOnline",
        "Connecting" => "settingsNative.statusConnecting",
        "Error" => "settingsNative.statusError",
        _ => "settingsNative.statusOffline",
    };
    t(key)
}

pub fn status_color(host: &HostView, pal: &crate::features::settings::ui::theme::Palette) -> makepad_widgets::Vec4 {
    match host.status {
        Status::Connected => pal.success,
        Status::Connecting => pal.warning,
        _ => pal.danger,
    }
}

pub fn draw_connection_error(ui: &mut Ui, host: &HostView) {
    if let Some(message) = host.error.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        ui.alert(false, message, None, None);
        ui.gap(24.0);
    }
}

fn rename(s: &mut Settings, host: &HostView) {
    let config = host.config.clone();
    let host_id = host.server_id.clone();
    let fields = vec![SettingField::text("label", t("settings.host.daemon.rename.placeholder"), str_of(&config, "label"))];
    let save: FormSave = Rc::new(move |s, env, sheet, values| {
        let label = values.get("label").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if !label.is_empty() && label != str_of(&config, "label") {
            host::update_host_config(&env.ctx.core, config_with(&config, Some(&label), None));
            s.host_busy.insert(host_id.clone(), true);
        }
        s.form_done(env, sheet, Ok(()));
    });
    let sheet = s.next_id();
    let form = FormSheet::new(sheet, t("settings.host.daemon.rename.title"), fields, save);
    s.push_sheet(sheet, SheetKind::Form(form), 480.0);
}

pub fn draw_overview_page(ui: &mut Ui, s: &Settings, v: &View, host: &HostView) {
    let pal = ui.pal;
    let mut badges = vec![status_badge(host, &pal)];
    if let Some((remote, label)) = connection_badge(host) {
        badges.push(Ctl::Badge { label, tone: None, xs: false, dot: None, icon: Some(if remote { Icon::Globe } else { Icon::Monitor }) });
    }
    let version = host.version();
    if !version.is_empty() {
        let shown = if version.starts_with('v') { version } else { format!("v{version}") };
        badges.push(Ctl::Badge { label: shown, tone: None, xs: false, dot: None, icon: None });
    }
    ui.wrap(badges, 4.0, 4.0);
    ui.gap(24.0);
    draw_connection_error(ui, host);

    // Host appearance: name + rename, identity color.
    ui.section_title(&t("settings.host.appearance.title"), None, None);
    ui.card_begin();
    let rename_host = host.clone();
    let rename_act = handler(move |s, _| rename(s, &rename_host));
    let name_ctl = Ctl::Row(
        vec![
            Ctl::Label { text: host.label.clone(), color: pal.muted, size: 14.0, font: Font::Regular, max_w: 220.0 },
            Ctl::IconButton { key: format!("host.{}.rename", host.server_id), icon: Icon::Pencil, act: Some(rename_act), destructive: false, size: 24.0, tip: None },
        ],
        8.0,
    );
    ui.row(row(t("settings.host.appearance.name.label")).inline().control(name_ctl));
    let current = str_of(&host.config, "color");
    let current = if IDENTITY_COLORS.contains(&current) { current.to_string() } else { "none".into() };
    let mut options = vec![("none".to_string(), t("settings.host.appearance.color.options.none"))];
    let mut leads = vec![("none".to_string(), Lead::Swatch(None), false)];
    for name in IDENTITY_COLORS {
        options.push((name.to_string(), t(&format!("settings.host.appearance.color.options.{name}"))));
        leads.push((name.to_string(), Lead::Swatch(identity_fill(name)), false));
    }
    let config = host.config.clone();
    let pick: Pick = Rc::new(move |_, env, picked| {
        let next = config_with(&config, None, Some(if picked == "none" { None } else { Some(picked.as_str()) }));
        if next != config {
            host::update_host_config(&env.ctx.core, next);
        }
    });
    let color = select_ctl_lead(&format!("host.{}.color", host.server_id), t("settings.host.appearance.color.label"), &current, options, leads, 200.0, true, pick);
    ui.row(row(t("settings.host.appearance.color.label")).control(color));
    ui.card_end();
    ui.gap(24.0);

    if DESKTOP && host.local && platform::desktop_available() {
        super::system::draw_local_daemon(ui, s);
    }
    super::daemon::draw_host_config(ui, s, host);
    if !host.local {
        draw_update_card(ui, s, v, host);
    }
    draw_danger_zone(ui, s, host);
}

// ------------------------------------------------------------ connections

pub fn draw_connections_page(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    draw_connection_error(ui, host);
    ui.section_title(&t("settings.host.connections.title"), None, None);
    ui.card_begin();
    for (i, connection) in host.connections.iter().enumerate() {
        let label = connection_label(connection);
        let server = host.server_id.clone();
        let connection_id = str_of(connection, "id").to_string();
        let last = host.connections.len() <= 1;
        let remove = handler(move |s, _| {
            let server = server.clone();
            let connection_id = connection_id.clone();
            // The last connection removes the host (Flutter `removeConnection` → `removeHost`,
            // tab close guards first).
            let go = handler(move |_, env| {
                if last {
                    env.ctx.effects.push(UiEffect::RemoveHost { host_id: server.clone() });
                } else {
                    host::remove_connection(&env.ctx.core, &server, &connection_id);
                }
            });
            s.confirm(
                t("settings.host.connections.removeTitle"),
                ta("settings.host.connections.removeMessage", &[("name", &label)]),
                None,
                true,
                go,
            );
        });
        let action = button(format!("conn.{}.{i}.remove", host.server_id), t("settings.host.connections.removeAction"), Some(remove)).variant(Variant::Ghost).danger_text().ctl();
        ui.row(row(connection_label(connection)).one_line().inline().control(action));
    }
    ui.card_end();
    ui.gap(24.0);
    if host.connected() {
        if host.feature("connectedHosts") {
            super::daemon::draw_connected_hosts(ui, s, host);
        }
        super::daemon::draw_hub(ui, s, host);
    }
}

// ------------------------------------------------------------ danger zone

fn draw_danger_zone(ui: &mut Ui, s: &Settings, host: &HostView) {
    ui.section_title(&t("settings.host.daemon.dangerZone"), None, None);
    draw_restart_card(ui, s, host);
    ui.gap(12.0);
    ui.card_begin();
    let local = host.local;
    let (title, hint) = if local {
        ("settings.host.daemon.remove.localTitle", "settings.host.daemon.remove.localHint")
    } else {
        ("settings.host.daemon.remove.title", "settings.host.daemon.remove.hint")
    };
    let id = host.server_id.clone();
    let label = host.label.clone();
    let remove = handler(move |s, _| {
        let id = id.clone();
        let go = handler(move |s, env| {
            let id = id.clone();
            let finish = move |s: &mut Settings, env: &mut Env| {
                // Flutter `removeHost`: the shell runs the host's tab close guards first.
                env.ctx.effects.push(UiEffect::RemoveHost { host_id: id.clone() });
                s.host = None;
                s.section = if env.ctx.compact { None } else { Some("general".into()) };
                let section = s.section.clone().unwrap_or_default();
                env.navigate(Route::Settings { host_id: None, page: section });
            };
            if local && DESKTOP {
                super::system::disable_builtin_then(s, env, finish);
            } else {
                finish(s, env);
            }
        });
        let (title, message) = if local {
            (t("settings.host.daemon.remove.localConfirmTitle"), t("settings.host.daemon.remove.localConfirmMessage"))
        } else {
            (t("settings.host.daemon.remove.title"), ta("settings.host.daemon.remove.confirmMessage", &[("name", &label)]))
        };
        s.confirm(title, message, None, true, go);
    });
    ui.row(row(t(title)).hint(t(hint)).control(button(format!("host.{}.remove", host.server_id), t("settings.host.connections.removeAction"), Some(remove)).ctl()));
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ restart

/// Original `readSelectedWorker`: worker status, refusing another daemon.
fn read_worker(s: &mut Settings, env: &mut Env, server_id: &str, then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static) {
    let id = server_id.to_string();
    s.request(env, server_id, "daemon.get_status.request", json!({}), "daemon.get_status.response", move |s, env, result| {
        let result = result.and_then(|status| identity_checked(env, &id).map(|_| status));
        then(s, env, result);
    });
}

/// The host still reports the same daemon identity (`Daemon identity changed`).
fn identity_checked(env: &Env, server_id: &str) -> Result<(), ReqError> {
    match env.host(server_id) {
        Some(host) if host.reported_server_id() == server_id => Ok(()),
        Some(_) => Err(ReqError::msg(t("settingsNative.identityChanged"))),
        None => Err(ReqError { code: "DAEMON_NOT_CONNECTED".into(), message: t("settingsNative.identityChanged"), outcome_unknown: false }),
    }
}

/// Repeatable completion shared by the restart/update observation retries.
type Observer = Rc<dyn Fn(&mut Settings, &mut Env, Result<Value, ReqError>)>;

/// Original `observeReplacement`: poll every 250 ms (10 min) until a worker
/// with a new PID answers; offline polls retry.
fn observe_replacement(s: &mut Settings, env: &mut Env, server_id: String, previous: Value, deadline: Instant, then: Observer) {
    if Instant::now() >= deadline {
        then(s, env, Err(ReqError::msg(t("settingsNative.replacementUnconfirmed"))));
        return;
    }
    let id = server_id.clone();
    read_worker(s, env, &server_id, move |s, env, result| match result {
        Ok(current) if current.get("pid") != previous.get("pid") => then(s, env, Ok(current)),
        Err(error) if !error.is_reconnect() => then(s, env, Err(error)),
        _ => s.after(env, 0.25, move |s, env| observe_replacement(s, env, id, previous, deadline, then)),
    });
}

fn restart(s: &mut Settings, env: &mut Env, server_id: String) {
    let rt = |k: &str| t(&format!("settings.host.daemon.restart.{k}"));
    if !env.host(&server_id).map(|h| h.connected()).unwrap_or(false) {
        env.toast(rt("offlineMessage"), true);
        return;
    }
    s.restarting.insert(server_id.clone(), true);
    let id = server_id.clone();
    read_worker(s, env, &server_id, move |s, env, previous| {
        let previous = match previous {
            Ok(previous) => previous,
            Err(error) => {
                s.restarting.remove(&id);
                env.error(&error);
                return;
            }
        };
        let id2 = id.clone();
        s.request(env, &id, "restart_server_request", json!({"reason": format!("settings_daemon_restart_{id}")}), "status", move |s, env, result| {
            let acknowledged = match result {
                Ok(_) => true,
                Err(error) if error.is_reconnect() => false,
                Err(error) => {
                    s.restarting.remove(&id2);
                    env.error(&error);
                    return;
                }
            };
            let done_id = id2.clone();
            let done: Observer = Rc::new(move |s, env, result| {
                s.restarting.remove(&done_id);
                if let Err(error) = result {
                    let message = ta("settingsNative.restartAcknowledged", &[("acknowledged", &acknowledged.to_string()), ("error", &error.text())]);
                    env.toast(message, true);
                }
            });
            observe_replacement(s, env, id2, previous, Instant::now() + Duration::from_secs(600), done);
        });
    });
}

fn draw_restart_card(ui: &mut Ui, s: &Settings, host: &HostView) {
    let rt = |k: &str| t(&format!("settings.host.daemon.restart.{k}"));
    let restarting = s.restarting.get(&host.server_id).copied().unwrap_or(false);
    let id = host.server_id.clone();
    let label = host.label.clone();
    let act = (!restarting && host.connected()).then(|| {
        handler(move |s, _| {
            let id = id.clone();
            let go = handler(move |s, env| restart(s, env, id.clone()));
            s.confirm(ta("settings.host.daemon.restart.confirmTitle", &[("name", &label)]), rt("confirmMessage"), Some(rt("confirm")), true, go);
        })
    });
    ui.card_begin();
    let label = rt(if restarting { "restarting" } else { "confirm" });
    ui.row(row(rt("title")).hint(rt("hint")).control(button(format!("host.{}.restart", host.server_id), label, act).icon(Icon::RotateCw).ctl()));
    ui.card_end();
}

// ------------------------------------------------------------ update card

#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdatePhase {
    #[default]
    Idle,
    Updating,
    Complete,
    Failed,
}

#[derive(Clone, Debug, Default)]
pub struct UpdateCard {
    pub state: UpdatePhase,
    pub phase: String,
    pub worker_version: String,
    pub failure_title: String,
    pub failure_message: String,
    pub complete_note: String,
    pub durable: bool,
    /// The persisted operation was already looked at once this panel lifetime.
    pub resumed: bool,
    /// Legacy `daemon.update.request` id whose progress events are shown.
    pub legacy_request: Option<String>,
}

fn ut(key: &str) -> String {
    t(&format!("settings.host.daemon.update.{key}"))
}

fn uta(key: &str, args: &[(&str, &str)]) -> String {
    ta(&format!("settings.host.daemon.update.{key}"), args)
}

fn card<'a>(s: &'a mut Settings, server_id: &str) -> &'a mut UpdateCard {
    s.update_cards.entry(server_id.to_string()).or_default()
}

fn fail(s: &mut Settings, server_id: &str, title: String, message: String) {
    let c = card(s, server_id);
    c.state = UpdatePhase::Failed;
    c.failure_title = title;
    c.failure_message = message;
    c.legacy_request = None;
}

/// Resumes observing an operation this app submitted before a reconnect,
/// rebuilt view or restart. It never submits a new update.
pub fn ensure_update_card(s: &mut Settings, env: &mut Env, host: &HostView) {
    if host.local || host.server_id.is_empty() {
        return;
    }
    let c = card(s, &host.server_id);
    if c.resumed || c.state != UpdatePhase::Idle {
        return;
    }
    c.resumed = true;
    if let Some(operation) = host::stored_update_operation(&env.ctx.state, &host.server_id) {
        durable_update(s, env, host.server_id.clone(), operation, true);
    }
}

/// Status/submit request against the host's current connection with the
/// Flutter 30 s deadline and the identity check.
fn update_request(s: &mut Settings, env: &mut Env, server_id: &str, kind: &str, fields: Value, response: &str, then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static) {
    let id = server_id.to_string();
    s.request_timeout(env, server_id, kind, fields, response, Some(Duration::from_secs(30)), move |s, env, result| {
        let result = result.and_then(|value| identity_checked(env, &id).map(|_| value));
        then(s, env, result);
    });
}

fn durable_update(s: &mut Settings, env: &mut Env, server_id: String, operation_id: String, resumed: bool) {
    {
        let c = card(s, &server_id);
        c.state = UpdatePhase::Updating;
        c.durable = true;
        c.phase = ut("phaseStarting");
    }
    if resumed {
        observe(s, env, server_id, operation_id, Instant::now() + Duration::from_secs(30 * 60), None);
        return;
    }
    // Persisted before sending: an unacknowledged submit stays observable.
    let key = format!("prefs:{}", host::update_operation_key(&server_id));
    s.save_flushed(env, &key, Value::String(operation_id.clone()), move |s, env, saved| {
        if let Err(error) = saved {
            fail(s, &server_id, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error.text())]));
            return;
        }
        submit(s, env, server_id, operation_id, Instant::now() + Duration::from_secs(120));
    });
}

/// Original `submitDaemonUpdate`: while the outcome is unknown the SAME
/// operation id is resubmitted (the daemon returns the admitted operation).
fn submit(s: &mut Settings, env: &mut Env, server_id: String, operation_id: String, deadline: Instant) {
    let sid = server_id.clone();
    let oid = operation_id.clone();
    update_request(
        s,
        env,
        &server_id,
        "daemon.update.submit.request",
        json!({"operationId": operation_id, "source": "release"}),
        "daemon.update.submit.response",
        move |s, env, result| match result.and_then(|r| du::operation_of(&r, &oid).map_err(ReqError::msg)) {
            Ok(_) => observe(s, env, sid, oid, Instant::now() + Duration::from_secs(30 * 60), None),
            Err(error) if error.is_reconnect() && Instant::now() < deadline => {
                s.after(env, 1.0, move |s, env| submit(s, env, sid, oid, deadline));
            }
            Err(error) => settle_error(s, env, &sid, &oid, error),
        },
    );
}

/// An unknown outcome keeps the persisted id; a definitive rejection leaves
/// nothing to observe.
fn settle_error(s: &mut Settings, env: &mut Env, server_id: &str, operation_id: &str, error: ReqError) {
    if error.is_reconnect() {
        fail(s, server_id, ut("stillRunningTitle"), uta("stillRunningMessage", &[("id", operation_id)]));
    } else {
        host::clear_update_operation(&env.ctx.core, server_id);
        fail(s, server_id, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error.text())]));
    }
}

/// Original `observeDaemonUpdate`: poll status every second until terminal
/// and any session resume has left `pending`; connection loss only pauses.
fn observe(s: &mut Settings, env: &mut Env, server_id: String, operation_id: String, deadline: Instant, last: Option<Value>) {
    if Instant::now() >= deadline {
        match last {
            Some(last) if last.get("terminal").and_then(Value::as_bool) == Some(true) => finish(s, env, &server_id, &operation_id, last),
            _ => fail(s, &server_id, ut("stillRunningTitle"), uta("stillRunningMessage", &[("id", &operation_id)])),
        }
        return;
    }
    let sid = server_id.clone();
    let oid = operation_id.clone();
    update_request(
        s,
        env,
        &server_id,
        "daemon.update.status.request",
        json!({"operationId": operation_id}),
        "daemon.update.status.response",
        move |s, env, result| match result.and_then(|r| du::operation_of(&r, &oid).map_err(ReqError::msg)) {
            Ok(operation) => {
                card(s, &sid).phase = ut(du::phase_key(str_of(&operation, "phase")));
                if operation.get("terminal").and_then(Value::as_bool) == Some(true) && du::unconfirmed_continuation(&operation).as_deref() != Some("pending") {
                    finish(s, env, &sid, &oid, operation);
                } else {
                    s.after(env, 1.0, move |s, env| observe(s, env, sid, oid, deadline, Some(operation)));
                }
            }
            Err(error) if error.is_reconnect() => s.after(env, 1.0, move |s, env| observe(s, env, sid, oid, deadline, last)),
            Err(error) => settle_error(s, env, &sid, &oid, error),
        },
    );
}

fn finish(s: &mut Settings, env: &mut Env, server_id: &str, operation_id: &str, operation: Value) {
    // An unsettled session resume stays inspectable: reopening re-reads status.
    if !du::retain_operation(&operation) {
        host::clear_update_operation(&env.ctx.core, server_id);
    }
    let error = str_of(&operation, "error").to_string();
    match du::outcome(&operation) {
        Outcome::Committed => {
            let resume = du::unconfirmed_continuation(&operation);
            let requested = operation.get("continuation").map(Value::is_object).unwrap_or(false);
            let note = match (requested, resume) {
                (false, _) => ut("committedMessage"),
                (true, None) => ut("continuationAccepted"),
                (true, Some(state)) if du::retain_operation(&operation) => uta("continuationRetained", &[("state", &state)]),
                (true, Some(state)) => uta("continuationUnconfirmed", &[("state", &state)]),
            };
            let c = card(s, server_id);
            c.state = UpdatePhase::Complete;
            c.worker_version = str_of(get(&operation, "candidate"), "version").to_string();
            c.complete_note = note;
        }
        Outcome::RolledBack => fail(s, server_id, ut("rolledBackTitle"), uta("rolledBackMessage", &[("id", operation_id), ("error", &error)])),
        Outcome::Failed | Outcome::Running => fail(s, server_id, ut("requestFailedTitle"), uta("operationFailedMessage", &[("id", operation_id), ("error", &error)])),
    }
}

fn start_update(s: &mut Settings, env: &mut Env, server_id: String) {
    let Some(host) = env.host(&server_id) else { return };
    if !host.connected() {
        fail(s, &server_id, ut("offlineTitle"), ut("offlineMessage"));
        return;
    }
    if du::supports_durable(&host.server_info) {
        durable_update(s, env, server_id, du::new_operation_id(), false);
        return;
    }
    legacy_update(s, env, server_id);
}

/// Pre-durable daemons: `daemon.update.request` with progress events, then
/// the replacement worker's version is confirmed.
fn legacy_update(s: &mut Settings, env: &mut Env, server_id: String) {
    let micros = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    let request_id = format!("settings_daemon_update_{server_id}_{micros}");
    {
        let c = card(s, &server_id);
        c.state = UpdatePhase::Updating;
        c.durable = false;
        c.phase = ut("phaseStarting");
        c.legacy_request = Some(request_id.clone());
    }
    let sid = server_id.clone();
    read_worker(s, env, &server_id, move |s, env, previous| {
        let previous = match previous {
            Ok(previous) => previous,
            Err(error) => return fail(s, &sid, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error.text())])),
        };
        let sid2 = sid.clone();
        let message = json!({"type": "daemon.update.request", "requestId": request_id});
        s.request_streamed(env, &sid, message, "daemon.update.response", Some(Duration::from_secs(600)), move |s, env, installed| {
            let installed = match installed {
                Ok(installed) if installed.get("success").and_then(Value::as_bool) == Some(true) => installed,
                Ok(installed) => {
                    let error = crate::features::settings::json::str_or(&installed, "error", &t("settingsNative.packageFailed")).to_string();
                    return fail(s, &sid2, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error)]));
                }
                Err(error) => return fail(s, &sid2, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error.text())])),
            };
            let new_version = str_of(&installed, "newVersion").to_string();
            let sid3 = sid2.clone();
            let done: Observer = Rc::new(move |s, env, worker| {
                let failure = |s: &mut Settings, detail: String| {
                    let error = ta("settingsNative.packageUnconfirmed", &[("error", &detail)]);
                    fail(s, &sid3, ut("requestFailedTitle"), uta("requestFailedMessage", &[("error", &error)]));
                };
                match worker {
                    Err(error) => failure(s, error.text()),
                    Ok(worker) => {
                        let reported = env.host(&sid3).map(|h| h.version()).unwrap_or_default();
                        let version = if reported.is_empty() { str_of(&worker, "version").to_string() } else { reported };
                        if version.is_empty() || new_version.is_empty() || version != new_version {
                            let expected = if new_version.is_empty() { t("settingsNative.unknownVersion") } else { new_version.clone() };
                            failure(s, ta("settingsNative.versionMismatch", &[("expected", &expected), ("observed", &version)]));
                        } else {
                            let c = card(s, &sid3);
                            c.state = UpdatePhase::Complete;
                            c.worker_version = version;
                            c.legacy_request = None;
                        }
                    }
                }
            });
            observe_replacement(s, env, sid2, previous, Instant::now() + Duration::from_secs(600), done);
        });
    });
}

/// Legacy `daemon.update.progress` events for the running request.
pub fn on_host_event(s: &mut Settings, host_id: &str, event: &Value) {
    if str_of(event, "type") != "daemon.update.progress" {
        return;
    }
    let payload = get(event, "payload");
    let Some(c) = s.update_cards.get_mut(host_id) else { return };
    if c.legacy_request.as_deref() != Some(str_of(payload, "requestId")) {
        return;
    }
    let key = match str_of(payload, "phase") {
        "starting" => "phaseStarting",
        "downloading" => "phaseDownloading",
        "installing" => "phaseInstalling",
        "complete" => "phaseComplete",
        _ => return,
    };
    c.phase = ut(key);
}

fn draw_update_card(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    let empty = UpdateCard::default();
    let c = s.update_cards.get(&host.server_id).unwrap_or(&empty);
    let desktop_managed = host.server_info.get("desktopManaged").and_then(Value::as_bool) == Some(true);
    let self_update = host.feature("daemonSelfUpdate");
    let app = platform::app_version().and_then(|v| du::normalize_version(&v));
    let daemon = du::normalize_version(&host.version());
    let mismatch = matches!((&app, &daemon), (Some(a), Some(d)) if a != d);
    let maintenance = host.maintenance_operation.clone();
    if !(mismatch && (self_update || desktop_managed)) && c.state == UpdatePhase::Idle {
        if let Some(operation) = maintenance {
            // Another client's durable update is pausing new work on this host.
            ui.card_begin();
            let hint = ta("settings.host.daemon.update.stillRunningMessage", &[("id", &operation)]);
            ui.row(row(t("settingsNative.maintenance")).hint(hint).leading(Ctl::Spinner(14.0)));
            ui.card_end();
            ui.gap(24.0);
        }
        return;
    }
    let updating = c.state == UpdatePhase::Updating;
    let id = host.server_id.clone();
    let label = host.label.clone();
    let act = (!desktop_managed && !updating && host.connected()).then(|| {
        handler(move |s, _| {
            let id = id.clone();
            let go = handler(move |s, env| start_update(s, env, id.clone()));
            s.confirm(uta("confirmTitle", &[("name", &label)]), ut("confirmMessage"), Some(ut("confirm")), false, go);
        })
    });
    ui.card_begin();
    let button_label = if updating { c.phase.clone() } else { ut("confirm") };
    let hint = ut(if desktop_managed { "desktopManagedHint" } else { "hint" });
    ui.row(row(ut("title")).hint(hint).control(button(format!("host.{}.update", host.server_id), button_label, act).icon(Icon::ArrowUpToLine).size(BSize::Sm).loading(updating).ctl()));
    match c.state {
        UpdatePhase::Complete => {
            ui.card_row_separator();
            let (title, description) = if c.durable {
                (uta("committedTitle", &[("version", &c.worker_version)]), c.complete_note.clone())
            } else {
                (ta("desktop.daemon.lifecycle.workerUpdated", &[("version", &c.worker_version)]), t("desktop.daemon.lifecycle.supervisorRefresh"))
            };
            inset_alert(ui, true, &title, &description);
        }
        UpdatePhase::Failed => {
            ui.card_row_separator();
            inset_alert(ui, false, &c.failure_title.clone(), &c.failure_message.clone());
        }
        _ => {}
    }
    ui.card_end();
    ui.gap(24.0);
}

fn inset_alert(ui: &mut Ui, success: bool, title: &str, description: &str) {
    let (x, w) = (ui.x, ui.w);
    ui.x = x + 16.0;
    ui.w = w - 32.0;
    ui.gap(0.0);
    ui.alert(success, title, Some(description), None);
    ui.gap(16.0);
    ui.x = x;
    ui.w = w;
}

/// Active-connection label for the sidebar host picker.
pub fn picker_hint(host: &HostView) -> String {
    active_connection_label(host)
}
