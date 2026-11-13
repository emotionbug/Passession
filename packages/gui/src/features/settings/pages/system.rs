//! Desktop-host pages from `settings_screen.dart`: Local daemon
//! (`LocalDaemonSection`), Notifications and Diagnostics.

use serde_json::{Value, json};

use crate::features::settings::host::Status;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{get, is_true, scalar_text, str_of};
use crate::features::settings::platform::{self, DESKTOP};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::Icon;
use crate::features::settings::ui::kit::{Ctl, Ui, Variant, button, row, small, switch};

#[derive(Default)]
pub struct LocalDaemonState {
    pub settings: Value,
    pub daemon: Value,
    pub loading: bool,
    pub loaded: bool,
    pub busy: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct NotificationsState {
    pub settings: Value,
    pub granted: Option<bool>,
    pub test_sent: bool,
    pub test_error: Option<String>,
    pub busy: bool,
    pub error: Option<String>,
    pub loaded: bool,
}

#[derive(Default)]
pub struct DiagnosticsState {
    pub running: bool,
    pub playing: bool,
    pub playback_result: Option<String>,
}

// ------------------------------------------------------------ local daemon

pub fn ensure_local_daemon(s: &mut Settings, env: &mut Env) {
    if s.local_daemon.loaded || s.local_daemon.loading || !DESKTOP {
        return;
    }
    load_local_daemon(s, env);
}

fn load_local_daemon(s: &mut Settings, env: &mut Env) {
    s.local_daemon.loading = true;
    s.desktop(env, "desktop_settings_get", json!({}), |s, env, settings| match settings {
        Err(error) => {
            s.local_daemon.loading = false;
            s.local_daemon.loaded = true;
            s.local_daemon.error = Some(error.text());
        }
        Ok(settings) => s.desktop(env, "desktop_daemon_status", json!({}), move |s, _, daemon| {
            s.local_daemon.loading = false;
            s.local_daemon.loaded = true;
            match daemon {
                Ok(daemon) => {
                    s.local_daemon.settings = settings;
                    s.local_daemon.daemon = daemon;
                    s.local_daemon.error = None;
                }
                Err(error) => s.local_daemon.error = Some(error.text()),
            }
        }),
    });
}

/// `_DesktopInvoke.run`: one operation at a time; failures land in `error`.
fn run_local(s: &mut Settings, env: &mut Env, command: &'static str, args: Value, reload: bool) {
    if s.local_daemon.busy {
        return;
    }
    s.local_daemon.busy = true;
    s.desktop(env, command, args, move |s, env, result| {
        s.local_daemon.busy = false;
        match result {
            Ok(_) => {
                s.local_daemon.error = None;
                if reload {
                    load_local_daemon(s, env);
                }
            }
            Err(error) => s.local_daemon.error = Some(error.text()),
        }
    });
}

fn report_local(s: &mut Settings, env: &mut Env, command: &'static str, title: String) {
    if s.local_daemon.busy {
        return;
    }
    s.local_daemon.busy = true;
    s.desktop(env, command, json!({}), move |s, _, result| {
        s.local_daemon.busy = false;
        match result {
            Ok(report) => {
                s.local_daemon.error = None;
                s.report(title, &report);
            }
            Err(error) => s.local_daemon.error = Some(error.text()),
        }
    });
}

fn patch_daemon(patch: Value) -> crate::features::settings::rpc::Handler {
    handler(move |s, env| run_local(s, env, "desktop_settings_update", json!({"patch": {"daemon": patch.clone()}}), true))
}

pub fn draw_local_daemon(ui: &mut Ui, s: &Settings) {
    let state = &s.local_daemon;
    let pal = ui.pal;
    ui.section_title(&t("desktop.daemon.title"), None, None);
    if state.loading && !state.loaded {
        ui.card_begin();
        ui.card_row_separator();
        let (x, y, w) = (ui.x, ui.y, ui.w);
        ui.draw_ctl(Ctl::Spinner(20.0), makepad_widgets::dvec2(x + (w - 20.0) / 2.0, y + 24.0));
        ui.y += 68.0;
        ui.card_end();
        ui.gap(24.0);
        return;
    }
    let daemon_settings = get(&state.settings, "daemon");
    let running = str_of(&state.daemon, "status") == "running";
    let owned = is_true(&state.daemon, "ownedByDesktop");
    let pid = state.daemon.get("pid").filter(|p| !p.is_null()).map(scalar_text);
    let busy = state.busy;
    ui.card_begin();
    let status = Ctl::Col(
        vec![
            Ctl::Label { text: t(if running { "desktop.daemon.status.running" } else { "desktop.daemon.status.notRunning" }), color: pal.muted, size: 14.0, font: crate::features::settings::ui::draw::Font::Regular, max_w: 200.0 },
            small(ta("desktop.daemon.status.pid", &[("pid", pid.as_deref().unwrap_or("—"))]), pal.muted),
        ],
        0.0,
        true,
    );
    ui.row(row(t("desktop.daemon.status.title")).hint(t("desktop.daemon.status.builtInOnly")).error(state.error.clone()).control(status));
    let manage = daemon_settings.get("manageBuiltInDaemon").and_then(Value::as_bool) != Some(false);
    let toggle_manage = (!busy).then(|| {
        handler(move |s, env| {
            if manage {
                let confirmed = patch_daemon(json!({"manageBuiltInDaemon": false}));
                s.confirm(t("desktop.daemon.management.pauseTitle"), t("desktop.daemon.management.pauseMessage"), None, true, confirmed);
            } else {
                patch_daemon(json!({"manageBuiltInDaemon": true}))(s, env);
            }
        })
    });
    ui.row(row(t("desktop.daemon.management.title")).hint(t("desktop.daemon.management.hint")).control(switch("local.manage", manage, toggle_manage)));
    let keep = is_true(daemon_settings, "keepRunningAfterQuit");
    ui.row(
        row(t("desktop.daemon.keepRunning.title"))
            .hint(t("desktop.daemon.keepRunning.hint"))
            .control(switch("local.keep", keep, (!busy).then(|| patch_daemon(json!({"keepRunningAfterQuit": !keep}))))),
    );
    let log_hint = state.daemon.get("logPath").filter(|p| !p.is_null()).map(scalar_text).unwrap_or_else(|| t("desktop.daemon.logs.unavailable"));
    let logs = (!busy).then(|| handler(|s, env| report_local(s, env, "desktop_daemon_logs", t("desktop.daemon.logs.modalTitle"))));
    ui.row(row(t("desktop.daemon.logs.title")).hint(log_hint).control(button("local.logs", t("desktop.daemon.logs.open"), logs).icon(Icon::FileText).ctl()));
    let status_report = (!busy).then(|| handler(|s, env| report_local(s, env, "desktop_daemon_status", t("desktop.daemon.fullStatus.modalTitle"))));
    ui.row(
        row(t("desktop.daemon.fullStatus.title"))
            .hint(t("desktop.daemon.fullStatus.hint"))
            .control(button("local.status", t("desktop.daemon.fullStatus.view"), status_report).icon(Icon::Activity).ctl()),
    );
    ui.card_end();
    ui.gap(12.0);

    ui.card_begin();
    let label = if pid.is_none() {
        t("desktop.daemon.status.notRunning")
    } else {
        t(if owned { "desktop.daemon.lifecycle.owned" } else { "desktop.daemon.lifecycle.attached" })
    };
    let version = state.daemon.get("version").filter(|v| !v.is_null()).map(|v| format!("v{}", scalar_text(v)));
    let mut buttons = Vec::new();
    if !running {
        let start = (!busy).then(|| handler(|s, env| run_local(s, env, "start_desktop_daemon", json!({}), true)));
        buttons.push(button("local.start", t("settingsNative.startDaemon"), start).ctl());
    }
    if running && owned {
        let restart = (!busy).then(|| {
            handler(|s, _| {
                let go = handler(|s, env| run_local(s, env, "restart_desktop_daemon", json!({"reason": "settings"}), true));
                s.confirm(t("settings.host.daemon.restart.title"), t("settings.host.daemon.restart.hint"), None, false, go);
            })
        });
        buttons.push(button("local.restart", t("settings.host.daemon.restart.title"), restart).ctl());
        let stop = (!busy).then(|| {
            handler(|s, _| {
                let go = handler(|s, env| run_local(s, env, "stop_desktop_daemon", json!({"reason": "settings"}), true));
                s.confirm(t("desktop.daemon.lifecycle.stopTitle"), t("desktop.daemon.lifecycle.ownedMessage"), None, true, go);
            })
        });
        buttons.push(button("local.stop", t("desktop.daemon.lifecycle.stop"), stop).ctl());
    }
    ui.row(row(label).hint_opt(version).control(Ctl::Row(buttons, 8.0)));
    ui.card_end();
    ui.gap(24.0);
}

/// Remove-host flow for the local host: stop managing the built-in daemon first.
pub fn disable_builtin_then(s: &mut Settings, env: &mut Env, then: impl FnOnce(&mut Settings, &mut Env) + 'static) {
    if !platform::desktop_available() {
        then(s, env);
        return;
    }
    s.desktop(env, "desktop_settings_update", json!({"patch": {"daemon": {"manageBuiltInDaemon": false}}}), move |s, env, result| match result {
        Ok(_) => then(s, env),
        Err(error) => env.error(&error),
    });
}

// ------------------------------------------------------------ notifications

pub fn ensure_notifications(s: &mut Settings, env: &mut Env) {
    if s.notifications.loaded || !DESKTOP {
        return;
    }
    s.notifications.loaded = true;
    load_notifications(s, env);
}

fn load_notifications(s: &mut Settings, env: &mut Env) {
    s.desktop(env, "desktop_settings_get", json!({}), |s, _, result| match result {
        Ok(settings) => s.notifications.settings = settings,
        Err(error) => s.notifications.error = Some(error.text()),
    });
}

pub fn draw_notifications(ui: &mut Ui, s: &Settings) {
    let state = &s.notifications;
    let busy = state.busy;
    let available = platform::desktop_available();
    ui.section_title(&t("settings.notifications.title"), None, None);
    ui.card_begin();
    let request = (!busy && available).then(|| {
        handler(|s, env| {
            s.notifications.busy = true;
            let token = env.bridge.request_notification_permission();
            s.bridge_job(token, |s, _, result| {
                s.notifications.busy = false;
                match result {
                    Ok(granted) => {
                        s.notifications.granted = granted.as_bool();
                        s.notifications.error = None;
                    }
                    Err(error) => s.notifications.error = Some(error.text()),
                }
            });
        })
    });
    let granted_hint = (state.granted == Some(true)).then(|| t("settings.permissions.actions.granted"));
    ui.row(
        row(t("settings.notifications.permission"))
            .hint_opt(granted_hint)
            .control(button("notifications.request", t("settings.permissions.actions.request"), request).icon(Icon::RotateCw).ctl()),
    );
    let play_sound = get(&state.settings, "notifications").get("playSound").and_then(Value::as_bool) != Some(false);
    let toggle = (!busy && state.settings.is_object()).then(|| {
        handler(move |s, env| {
            s.notifications.busy = true;
            s.desktop(env, "desktop_settings_update", json!({"patch": {"notifications": {"playSound": !play_sound}}}), |s, env, result| {
                s.notifications.busy = false;
                match result {
                    Ok(_) => {
                        s.notifications.error = None;
                        load_notifications(s, env);
                    }
                    Err(error) => s.notifications.error = Some(error.text()),
                }
            });
        })
    });
    ui.row(row(t("settings.notifications.playSound")).hint(t("settings.notifications.playSoundHint")).control(switch("notifications.sound", play_sound, toggle)));
    let send = (!busy && available && state.granted != Some(false)).then(|| {
        handler(|s, env| {
            s.notifications.busy = true;
            s.notifications.test_sent = false;
            s.notifications.test_error = None;
            let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let id = (millis & 0x7fff_ffff) as i32;
            let token = env.bridge.notify(id, "Passession".into(), t("settings.notifications.testHint"));
            s.bridge_job(token, |s, _, result| {
                s.notifications.busy = false;
                match result {
                    Ok(_) => s.notifications.test_sent = true,
                    Err(error) => s.notifications.test_error = Some(error.text()),
                }
            });
        })
    });
    let hint = if state.granted == Some(false) { t("settings.notifications.permissionRequired") } else { t("settings.notifications.testHint") };
    let label = if busy { t("settings.notifications.sending") } else { t("settings.notifications.send") };
    ui.row(row(t("settings.notifications.test")).hint(hint).error(state.error.clone()).control(button("notifications.send", label, send).ctl()));
    ui.card_end();
    if state.test_sent {
        ui.gap(12.0);
        ui.alert(true, &t("settings.notifications.sentTitle"), Some(&t("settings.notifications.sentDescription")), None);
    } else if let Some(error) = &state.test_error {
        ui.gap(12.0);
        ui.alert(false, &t("settings.notifications.sendFailedTitle"), Some(error), None);
    }
    ui.gap(24.0);
}

// ------------------------------------------------------------ diagnostics

/// Original `DiagnosticsSection` app diagnostic: client, hosts, desktop
/// runtime/daemon/update/log reports and each connected host's diagnostic.
fn run_diagnostic(s: &mut Settings, env: &mut Env) {
    if s.diagnostics.running {
        return;
    }
    s.diagnostics.running = true;
    let hosts = env.hosts();
    let report = json!({
        "client": {
            "version": platform::app_version(),
            "platform": platform::platform_name(),
        },
        "hosts": hosts.iter().map(|h| {
            let mut entry = json!({
                "label": h.label,
                "serverId": h.server_id,
                "status": format!("{:?}", h.status).to_lowercase(),
                "version": get(&h.server_info, "version"),
            });
            if let Some(error) = &h.error {
                entry["error"] = Value::String(error.clone());
            }
            entry
        }).collect::<Vec<_>>(),
    });
    let desktop_steps: Vec<(&'static str, &'static str)> = if DESKTOP && platform::desktop_available() {
        vec![("desktop", "desktop_get_runtime_info"), ("daemon", "desktop_daemon_status"), ("updates", "desktop_update_diagnostics"), ("logs", "desktop_app_logs")]
    } else {
        Vec::new()
    };
    let connected: Vec<String> = hosts.iter().filter(|h| h.status == Status::Connected).map(|h| h.server_id.clone()).collect();
    diagnostic_desktop_step(s, env, report, desktop_steps, connected);
}

fn diagnostic_desktop_step(s: &mut Settings, env: &mut Env, report: Value, mut steps: Vec<(&'static str, &'static str)>, hosts: Vec<String>) {
    if steps.is_empty() {
        diagnostic_host_step(s, env, report, hosts);
        return;
    }
    let (key, command) = steps.remove(0);
    s.desktop(env, command, json!({}), move |s, env, result| {
        let mut report = report;
        report[key] = match result {
            Ok(value) => value,
            Err(error) => json!({"error": error.text()}),
        };
        diagnostic_desktop_step(s, env, report, steps, hosts);
    });
}

fn diagnostic_host_step(s: &mut Settings, env: &mut Env, report: Value, mut hosts: Vec<String>) {
    if hosts.is_empty() {
        s.diagnostics.running = false;
        s.report(t("settings.diagnostics.app.title"), &report);
        return;
    }
    let host = hosts.remove(0);
    let key = host.clone();
    s.request(env, &host, "diagnostics.request", json!({}), "diagnostics.response", move |s, env, result| {
        let mut report = report;
        if !report["hostDiagnostics"].is_object() {
            report["hostDiagnostics"] = json!({});
        }
        report["hostDiagnostics"][key] = match result {
            Ok(value) => get(&value, "diagnostic").clone(),
            Err(error) => json!({"error": error.text()}),
        };
        diagnostic_host_step(s, env, report, hosts);
    });
}

pub fn draw_diagnostics(ui: &mut Ui, s: &Settings) {
    let state = &s.diagnostics;
    ui.section_title(&t("settings.diagnostics.title"), None, None);
    ui.card_begin();
    let run = (!state.running).then(|| handler(run_diagnostic));
    let label = if state.running { t("settings.diagnostics.app.running") } else { t("settings.diagnostics.app.run") };
    ui.row(row(t("settings.diagnostics.app.rowTitle")).hint(t("settings.diagnostics.app.rowHint")).control(button("diagnostics.run", label, run).ctl()));
    let play = (!state.playing).then(|| {
        handler(|s, env| {
            s.diagnostics.playing = true;
            s.diagnostics.playback_result = None;
            let token = env.bridge.play_test_tone();
            s.bridge_job(token, |s, _, result: Result<Value, ReqError>| {
                s.diagnostics.playing = false;
                if let Err(error) = result {
                    s.diagnostics.playback_result = Some(ta("settings.diagnostics.playbackFailed", &[("message", &error.text())]));
                }
            });
        })
    });
    let label = if state.playing { t("settings.diagnostics.playing") } else { t("settings.diagnostics.playTest") };
    ui.row(row(t("settings.diagnostics.testAudio")).hint_opt(state.playback_result.clone()).control(button("diagnostics.play", label, play).variant(Variant::Outline).ctl()));
    ui.card_end();
    ui.gap(24.0);
}
