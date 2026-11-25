//! Local daemon, notification and diagnostic settings.

use serde_json::{Value, json};

use crate::features::settings::host::Status;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::get;
#[cfg(not(target_arch = "wasm32"))]
use crate::features::settings::json::{scalar_text, str_of};
use crate::features::settings::platform::{self, DESKTOP};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::Icon;
use crate::features::settings::ui::kit::{Ctl, Ui, Variant, button, row, switch};

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct LocalDaemonState {
    pub settings: Value,
    pub daemon: Value,
    pub loading: bool,
    pub(crate) loaded: bool,
    saving: bool,
    starting: bool,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct NotificationsState {
    pub settings: Value,
    /// `granted` / `denied` / `blocked` / `notDetermined`; `None` until known.
    pub permission: Option<String>,
    push_available: Option<bool>,
    /// A shown page reads the permission: returning to the app reads it again.
    pub watched: bool,
    /// The non-prompting status query is in flight.
    pub checking: bool,
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

#[cfg(not(target_arch = "wasm32"))]
pub fn ensure_local_daemon(s: &mut Settings, env: &mut Env) {
    if s.local_daemon.loaded || s.local_daemon.loading || !DESKTOP {
        return;
    }
    load_local_daemon(s, env);
}

#[cfg(not(target_arch = "wasm32"))]
fn load_local_daemon(s: &mut Settings, env: &mut Env) {
    let state = &mut s.local_daemon;
    state.loading = true;
    s.desktop(env, "desktop_settings_get", json!({}), |s, env, result| {
        match result {
            Ok(settings) => s.local_daemon.settings = settings,
            Err(error) => s.local_daemon.error = Some(error.text()),
        }
        refresh_local_daemon(s, env);
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn refresh_local_daemon(s: &mut Settings, env: &mut Env) {
    s.desktop(env, "desktop_daemon_status", json!({}), |s, _, result| {
        let state = &mut s.local_daemon;
        state.loading = false;
        state.loaded = true;
        match result {
            Ok(daemon) => state.daemon = daemon,
            Err(error) => state.error = Some(error.text()),
        }
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn set_local_autostart(s: &mut Settings, env: &mut Env, enabled: bool) {
    let state = &mut s.local_daemon;
    if !state.settings.is_object() || state.saving {
        return;
    }
    state.saving = true;
    state.error = None;
    s.desktop(
        env,
        "desktop_settings_update",
        json!({"patch": {"daemon": {"startDaemonIfNotRunning": enabled}}}),
        move |s, env, result| {
            let state = &mut s.local_daemon;
            state.saving = false;
            match result {
                Ok(settings) => {
                    state.settings = settings;
                    if enabled {
                        env.ctx.core.connect_local_daemon();
                    }
                }
                Err(error) => state.error = Some(error.text()),
            }
        },
    );
}

#[cfg(not(target_arch = "wasm32"))]
fn start_local_daemon(s: &mut Settings, env: &mut Env) {
    let state = &mut s.local_daemon;
    if state.starting || matches!(str_of(&state.daemon, "status"), "starting" | "running") {
        return;
    }
    state.starting = true;
    state.error = None;
    s.desktop(env, "start_desktop_daemon", json!({}), |s, env, result| {
        let state = &mut s.local_daemon;
        state.starting = false;
        match result {
            Ok(_) => env.ctx.core.connect_local_daemon(),
            Err(error) => state.error = Some(error.text()),
        }
        refresh_local_daemon(s, env);
    });
}

#[cfg(not(target_arch = "wasm32"))]
pub fn draw_local_daemon(ui: &mut Ui, s: &Settings, has_local_host: bool) {
    let state = &s.local_daemon;
    ui.section_title(&t("desktop.daemon.title"), None, None);
    if state.loading && !state.loaded {
        ui.card_begin();
        ui.card_row_separator();
        let (x, y, w) = (ui.x, ui.y, ui.w);
        ui.draw_ctl(
            Ctl::Spinner(20.0),
            makepad_widgets::dvec2(x + (w - 20.0) / 2.0, y + 24.0),
        );
        ui.y += 68.0;
        ui.card_end();
        ui.gap(24.0);
        return;
    }
    let daemon_settings = get(&state.settings, "daemon");
    let status = if state.starting {
        "starting"
    } else {
        match str_of(&state.daemon, "status") {
            status @ ("running" | "starting" | "stopped" | "failed") => status,
            _ => "unknown",
        }
    };
    let owner = match str_of(&state.daemon, "owner") {
        "systemd-user" => Some("desktop.daemon.owner.service"),
        "detached" => Some("desktop.daemon.owner.background"),
        "desktop-legacy" => Some("desktop.daemon.owner.legacy"),
        _ => None,
    };
    let enabled = daemon_settings["startDaemonIfNotRunning"]
        .as_bool()
        .unwrap_or(true);
    let toggle = (state.settings.is_object() && !state.saving)
        .then(|| handler(move |s, env| set_local_autostart(s, env, !enabled)));
    ui.card_begin();
    ui.row(
        row(t("desktop.daemon.autostart.title"))
            .hint(t("desktop.daemon.autostart.hint"))
            .control(switch("local.autostart", enabled, toggle)),
    );
    let (label, action) = if status == "running" && !has_local_host {
        (
            "desktop.daemon.connect",
            Some(handler(|_, env| env.ctx.core.connect_local_daemon())),
        )
    } else {
        (
            "desktop.daemon.startNow",
            matches!(status, "stopped" | "failed" | "unknown").then(|| handler(start_local_daemon)),
        )
    };
    let mut detail = t(&format!("desktop.daemon.status.{status}"));
    if let Some(owner) = owner {
        detail.push_str(" · ");
        detail.push_str(&t(owner));
    }
    ui.card_row_separator();
    ui.row(
        row(t("desktop.daemon.status.title"))
            .hint(detail)
            .error(
                state
                    .error
                    .clone()
                    .or_else(|| state.daemon["error"].as_str().map(String::from)),
            )
            .control(
                button("local.start", t(label), action)
                    .loading(status == "starting")
                    .ctl(),
            ),
    );
    ui.card_row_separator();
    let log_hint = state
        .daemon
        .get("logPath")
        .filter(|p| !p.is_null())
        .map(scalar_text)
        .unwrap_or_else(|| t("desktop.daemon.logs.unavailable"));
    let logs = Some(handler(|s, env| {
        s.desktop(
            env,
            "desktop_daemon_logs",
            json!({}),
            |s, _, result| match result {
                Ok(report) => s.report(t("desktop.daemon.logs.modalTitle"), &report),
                Err(error) => s.local_daemon.error = Some(error.text()),
            },
        );
    }));
    ui.row(
        row(t("desktop.daemon.logs.title")).hint(log_hint).control(
            button("local.logs", t("desktop.daemon.logs.open"), logs)
                .icon(Icon::FileText)
                .ctl(),
        ),
    );
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ notifications

pub fn ensure_notifications(s: &mut Settings, env: &mut Env) {
    if s.notifications.loaded {
        return;
    }
    s.notifications.loaded = true;
    // Play sound is a desktop host setting; mobile plays the platform's own sound.
    if DESKTOP {
        load_notifications(s, env);
    }
    check_notification_permission(s, env);
    if let Some(token) = env.bridge.push_availability() {
        s.bridge_job(token, |s, _, result| {
            s.notifications.push_available = result.ok().and_then(|value| value.as_bool());
        });
    }
}

/// Reads the notification permission without prompting. Pages that show it call this once;
/// [`refresh_notification_permission`] then follows changes made in system settings.
pub fn check_notification_permission(s: &mut Settings, env: &mut Env) {
    s.notifications.watched = true;
    if s.notifications.checking || s.notifications.busy {
        return;
    }
    s.notifications.checking = true;
    let token = env.bridge.notification_permission_status();
    s.bridge_job(token, |s, _, result| {
        s.notifications.checking = false;
        match result {
            Ok(state) => s.notifications.permission = permission_of(&state),
            Err(error) => s.notifications.error = Some(error.text()),
        }
    });
}

/// OS settings can change permission while a page showing it is in the background.
pub fn refresh_notification_permission(s: &mut Settings, env: &mut Env) {
    if s.notifications.watched {
        check_notification_permission(s, env);
    }
}

/// The Allow actions: the OS prompt where it can still show, else the current state.
pub fn request_notification_permission(s: &mut Settings, env: &mut Env) {
    if s.notifications.busy {
        return;
    }
    s.notifications.busy = true;
    let token = env.bridge.request_notification_permission();
    s.bridge_job(token, |s, _, result| {
        s.notifications.busy = false;
        match result {
            Ok(state) => {
                s.notifications.permission = permission_of(&state);
                s.notifications.error = None;
            }
            Err(error) => s.notifications.error = Some(error.text()),
        }
    });
}

/// The Open settings actions: the system screen where notifications are turned back on
/// (`channel`: one Android channel's screen). Returning to the app reads the permission again.
pub fn open_notification_settings(s: &mut Settings, env: &mut Env, channel: Option<&str>) {
    let token = env.bridge.open_notification_settings(channel);
    s.bridge_job(token, |_, env, result| {
        if let Err(error) = result {
            env.toast(error.text(), true);
        }
    });
}

/// A bridge permission result (`granted`/`denied`/`blocked`/`notDetermined`, or a bare bool).
fn permission_of(value: &Value) -> Option<String> {
    match value {
        Value::String(state) => Some(state.clone()),
        Value::Bool(true) => Some("granted".into()),
        Value::Bool(false) => Some("denied".into()),
        _ => None,
    }
}

fn load_notifications(s: &mut Settings, env: &mut Env) {
    s.desktop(
        env,
        "desktop_settings_get",
        json!({}),
        |s, _, result| match result {
            Ok(settings) => s.notifications.settings = settings,
            Err(error) => s.notifications.error = Some(error.text()),
        },
    );
}

pub fn draw_notifications(ui: &mut Ui, s: &Settings) {
    let state = &s.notifications;
    let busy = state.busy;
    let permission = state.permission.as_deref();
    let granted = permission == Some("granted");
    let blocked = permission == Some("blocked");
    // Linux reports Blocked when no notification server answers: nothing to open.
    let no_server = blocked && cfg!(target_os = "linux");
    ui.section_title(&t("settings.notifications.title"), None, None);
    ui.card_begin();
    let status_hint = if state.checking && permission.is_none() {
        t("common.states.loading")
    } else {
        match permission {
            Some("granted") => t("settings.notifications.status.allowed"),
            Some("denied") => t("settings.notifications.status.denied"),
            Some("blocked") if no_server => t("settings.notifications.status.noServer"),
            Some("blocked") => t("settings.notifications.status.blocked"),
            _ => t("settings.notifications.status.notDetermined"),
        }
    };
    let control = if granted {
        Ctl::Badge {
            label: t("settings.permissions.actions.granted"),
            tone: Some(ui.pal.success),
            xs: false,
            dot: None,
            icon: None,
        }
    } else if no_server {
        Ctl::Badge {
            label: t("settings.notifications.unavailable"),
            tone: Some(ui.pal.danger),
            xs: false,
            dot: None,
            icon: None,
        }
    } else if blocked {
        let open = handler(|s, env| open_notification_settings(s, env, None));
        button(
            "notifications.settings",
            t("systemPermissions.openSettings"),
            Some(open),
        )
        .icon(Icon::Settings)
        .ctl()
    } else {
        let label = if busy {
            t("settings.permissions.actions.requesting")
        } else {
            t("systemPermissions.allow")
        };
        let request = (!busy && !state.checking).then(|| handler(request_notification_permission));
        button("notifications.request", label, request)
            .icon(Icon::Bell)
            .ctl()
    };
    ui.row(
        row(t("settings.notifications.permission"))
            .hint(status_hint)
            .control(control),
    );
    if state.push_available == Some(false) {
        ui.row(
            row(t("settings.notifications.pushTitle"))
                .hint(t("settings.notifications.pushUnavailable")),
        );
    }
    if DESKTOP {
        let play_sound = get(&state.settings, "notifications")
            .get("playSound")
            .and_then(Value::as_bool)
            != Some(false);
        let toggle = (!busy && state.settings.is_object()).then(|| {
            handler(move |s, env| {
                s.notifications.busy = true;
                s.desktop(
                    env,
                    "desktop_settings_update",
                    json!({"patch": {"notifications": {"playSound": !play_sound}}}),
                    |s, env, result| {
                        s.notifications.busy = false;
                        match result {
                            Ok(_) => {
                                s.notifications.error = None;
                                load_notifications(s, env);
                            }
                            Err(error) => s.notifications.error = Some(error.text()),
                        }
                    },
                );
            })
        });
        ui.row(
            row(t("settings.notifications.playSound"))
                .hint(t("settings.notifications.playSoundHint"))
                .control(switch("notifications.sound", play_sound, toggle)),
        );
    }
    let send = (!busy && granted).then(|| {
        handler(|s, env| {
            s.notifications.busy = true;
            s.notifications.test_sent = false;
            s.notifications.test_error = None;
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let id = (millis & 0x7fff_ffff) as i32;
            let token = env.bridge.notify(
                id,
                "Passession".into(),
                t("settings.notifications.testHint"),
            );
            s.bridge_job(token, |s, _, result| {
                s.notifications.busy = false;
                match result {
                    Ok(_) => s.notifications.test_sent = true,
                    Err(error) => s.notifications.test_error = Some(error.text()),
                }
            });
        })
    });
    let hint = if granted {
        t("settings.notifications.testHint")
    } else {
        t("settings.notifications.permissionRequired")
    };
    let label = if busy {
        t("settings.notifications.sending")
    } else {
        t("settings.notifications.send")
    };
    ui.row(
        row(t("settings.notifications.test"))
            .hint(hint)
            .error(state.error.clone())
            .control(button("notifications.send", label, send).ctl()),
    );
    ui.card_end();
    if state.test_sent {
        ui.gap(12.0);
        ui.alert(
            true,
            &t("settings.notifications.sentTitle"),
            Some(&t("settings.notifications.sentDescription")),
            None,
        );
    } else if let Some(error) = &state.test_error {
        ui.gap(12.0);
        ui.alert(
            false,
            &t("settings.notifications.sendFailedTitle"),
            Some(error),
            None,
        );
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
    let renderer = env.cx.gpu_info().native_renderer.as_ref().map(|gpu| {
        json!({
            "api": gpu.api,
            "renderer": gpu.renderer,
            "vendor": gpu.vendor,
            "version": gpu.version,
            "shadingLanguageVersion": gpu.shading_language_version,
            "eglVendor": gpu.egl_vendor,
            "warp": gpu.warp,
            "software": gpu.software,
        })
    });
    let report = json!({
        "client": {
            "version": platform::app_version(),
            "platform": platform::platform_name(),
            "gpu": renderer,
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
    #[cfg(not(target_arch = "wasm32"))]
    let desktop_steps: Vec<(&'static str, &'static str)> =
        if DESKTOP && platform::desktop_available() {
            vec![
                ("desktop", "desktop_get_runtime_info"),
                ("daemon", "desktop_daemon_status"),
                ("updates", "desktop_update_diagnostics"),
                ("logs", "desktop_app_logs"),
            ]
        } else {
            Vec::new()
        };
    let connected: Vec<String> = hosts
        .iter()
        .filter(|h| h.status == Status::Connected)
        .map(|h| h.server_id.clone())
        .collect();
    #[cfg(not(target_arch = "wasm32"))]
    diagnostic_desktop_step(s, env, report, desktop_steps, connected);
    #[cfg(target_arch = "wasm32")]
    diagnostic_host_step(s, env, report, connected);
}

#[cfg(not(target_arch = "wasm32"))]
fn diagnostic_desktop_step(
    s: &mut Settings,
    env: &mut Env,
    report: Value,
    mut steps: Vec<(&'static str, &'static str)>,
    hosts: Vec<String>,
) {
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
    s.request(
        env,
        &host,
        "diagnostics.request",
        json!({}),
        "diagnostics.response",
        move |s, env, result| {
            let mut report = report;
            if !report["hostDiagnostics"].is_object() {
                report["hostDiagnostics"] = json!({});
            }
            report["hostDiagnostics"][key] = match result {
                Ok(value) => get(&value, "diagnostic").clone(),
                Err(error) => json!({"error": error.text()}),
            };
            diagnostic_host_step(s, env, report, hosts);
        },
    );
}

pub fn draw_diagnostics(ui: &mut Ui, s: &Settings) {
    let state = &s.diagnostics;
    ui.section_title(&t("settings.diagnostics.title"), None, None);
    ui.card_begin();
    let run = (!state.running).then(|| handler(run_diagnostic));
    let label = if state.running {
        t("settings.diagnostics.app.running")
    } else {
        t("settings.diagnostics.app.run")
    };
    ui.row(
        row(t("settings.diagnostics.app.rowTitle"))
            .hint(t("settings.diagnostics.app.rowHint"))
            .control(button("diagnostics.run", label, run).ctl()),
    );
    let play = (!state.playing).then(|| {
        handler(|s, env| {
            s.diagnostics.playing = true;
            s.diagnostics.playback_result = None;
            let token = env.bridge.play_test_tone();
            s.bridge_job(token, |s, _, result: Result<Value, ReqError>| {
                s.diagnostics.playing = false;
                if let Err(error) = result {
                    s.diagnostics.playback_result = Some(ta(
                        "settings.diagnostics.playbackFailed",
                        &[("message", &error.text())],
                    ));
                }
            });
        })
    });
    let label = if state.playing {
        t("settings.diagnostics.playing")
    } else {
        t("settings.diagnostics.playTest")
    };
    ui.row(
        row(t("settings.diagnostics.testAudio"))
            .hint_opt(state.playback_result.clone())
            .control(
                button("diagnostics.play", label, play)
                    .variant(Variant::Outline)
                    .ctl(),
            ),
    );
    ui.card_end();
    ui.gap(24.0);
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use makepad_widgets::{Cx, Event};
    use paseo_gui_core::{CoreConfig, GuiCore};
    use paseo_gui_platform::{PlatformAction, PlatformValue};
    use std::{fs, sync::Arc};

    #[test]
    fn local_daemon_toggle_only_persists_and_start_now_uses_ensure() {
        let dir = std::env::temp_dir().join(format!("paseo-settings-{}", uuid::Uuid::new_v4()));
        let core = GuiCore::start(
            CoreConfig {
                state_dir: dir.clone(),
                daemon_home: Some(dir.join("daemon")),
                default_endpoint: None,
                tailnet_key_store: None,
            },
            Arc::new(|| {}),
        )
        .unwrap();
        let mut ctx = crate::common::UiContext::new(core.handle());
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut fields = Default::default();
        let mut bridge = platform::Bridge::record_desktop();
        let mut env = Env {
            cx: &mut cx,
            ctx: &mut ctx,
            fields: &mut fields,
            bridge: &mut bridge,
        };
        let mut s = Settings::default();
        let reply = |s: &mut Settings, env: &mut Env, command: &str, args: Value, result: Value| {
            let requests = env.bridge.desktop_requests.as_mut().unwrap();
            assert_eq!(requests.len(), 1);
            let (request, sent, sent_args) = requests.pop().unwrap();
            assert_eq!(sent, command);
            assert_eq!(sent_args, args);
            let event = Event::Actions(vec![Box::new(PlatformAction::Completed {
                request,
                result: Ok(PlatformValue::Json(result)),
            })]);
            assert!(platform::handle_event(s, env, &event));
        };
        load_local_daemon(&mut s, &mut env);
        let initial = json!({"daemon": {"startDaemonIfNotRunning": true}});
        reply(
            &mut s,
            &mut env,
            "desktop_settings_get",
            json!({}),
            initial.clone(),
        );
        reply(
            &mut s,
            &mut env,
            "desktop_daemon_status",
            json!({}),
            json!({"status": "stopped", "owner": "detached"}),
        );
        assert!(s.local_daemon.loaded);

        let disabled = json!({"daemon": {"startDaemonIfNotRunning": false}});
        set_local_autostart(&mut s, &mut env, false);
        assert_eq!(s.local_daemon.settings, initial);
        reply(
            &mut s,
            &mut env,
            "desktop_settings_update",
            json!({"patch": disabled}),
            disabled.clone(),
        );
        assert_eq!(s.local_daemon.settings, disabled);
        assert!(env.bridge.desktop_requests.as_ref().unwrap().is_empty());

        start_local_daemon(&mut s, &mut env);
        start_local_daemon(&mut s, &mut env);
        assert!(s.local_daemon.starting);
        let running = json!({"status": "running", "owner": "detached"});
        reply(
            &mut s,
            &mut env,
            "start_desktop_daemon",
            json!({}),
            running.clone(),
        );
        reply(
            &mut s,
            &mut env,
            "desktop_daemon_status",
            json!({}),
            running.clone(),
        );
        assert_eq!(s.local_daemon.daemon, running);
        assert_eq!(s.local_daemon.settings, disabled);
        assert!(!s.local_daemon.starting);

        drop(core);
        fs::remove_dir_all(dir).unwrap();
    }
}
