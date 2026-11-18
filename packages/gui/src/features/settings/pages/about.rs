//! Original `AboutSection`: app version, desktop release channel and app
//! updates, update diagnostics, connected host versions.

use serde_json::{Value, json};

use super::View;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::str_of;
use crate::features::settings::model::daemon_update::{self as du, normalize_version};
use crate::features::settings::platform::{self, DESKTOP};
use crate::features::settings::rpc::{Env, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::Font;
use crate::features::settings::ui::kit::{Ctl, Ui, Variant, button, row};

#[derive(Default)]
pub struct AboutState {
    pub settings: Value,
    pub update: Value,
    pub checking: bool,
    pub checked: bool,
    pub installing: bool,
    pub check_error: Option<String>,
    pub install_error: Option<String>,
    pub operation: Value,
    resumed: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub loaded: bool,
}

impl AboutState {
    pub(super) fn update_progress(&self) -> Option<String> {
        let key = match du::outcome(&self.operation) {
            du::Outcome::Committed => "phaseComplete",
            du::Outcome::RolledBack => "rolledBackTitle",
            du::Outcome::Failed => "requestFailedTitle",
            du::Outcome::Running if self.operation.is_null() && self.installing => "phaseStarting",
            du::Outcome::Running if self.installing => {
                du::phase_key(str_of(&self.operation, "phase"))
            }
            du::Outcome::Running => return None,
        };
        Some(t(&format!("settings.host.daemon.update.{key}")))
    }
}

const APP_UPDATE_KEY: &str = "paseo.desktopAppUpdate.operation";

/// The immutable release pin and app-side components approved by the user.
pub(super) fn app_approval(check: &Value) -> Option<Value> {
    if !du::can_update_checked(check) {
        return None;
    }
    let selected = du::changed_components(check);
    if selected.is_empty()
        || selected
            .iter()
            .any(|id| !matches!(id.as_str(), "app" | "tailnet" | "update-helper"))
    {
        return None;
    }
    Some(json!({
        "expectedRelease": {
            "version": check["latest"]["version"],
            "manifestSha512": check["latest"]["manifestSha512"],
        },
        "selectedComponents": selected,
    }))
}

fn version_label(version: Option<&str>) -> String {
    match version.map(str::trim).filter(|v| !v.is_empty()) {
        None => "—".into(),
        Some(v) => format!("v{}", v.strip_prefix(['v', 'V']).unwrap_or(v)),
    }
}

pub fn ensure_about(s: &mut Settings, env: &mut Env, _v: &View) {
    if s.about.loaded || !DESKTOP || !platform::desktop_available() {
        return;
    }
    s.about.loaded = true;
    load(s, env);
    ensure_update(s, env);
}

fn load(s: &mut Settings, env: &mut Env) {
    s.desktop(
        env,
        "desktop_settings_get",
        json!({}),
        |s, _, result| match result {
            Ok(settings) => s.about.settings = settings,
            Err(error) => s.about.error = Some(error.text()),
        },
    );
}

pub(super) fn ensure_update(s: &mut Settings, env: &mut Env) {
    if !s.about.resumed {
        s.about.resumed = true;
        if let Some(id) = env
            .ctx
            .state
            .prefs
            .get(APP_UPDATE_KEY)
            .and_then(Value::as_str)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .filter(|id| !id.is_nil())
        {
            s.about.installing = true;
            poll_update(s, env, id.to_string());
        }
    }
    if !s.about.checked && !s.about.checking {
        check(s, env, false);
    }
}

pub(super) fn check(s: &mut Settings, env: &mut Env, manual: bool) {
    if s.about.checking {
        return;
    }
    s.about.checking = true;
    s.about.checked = true;
    s.about.check_error = None;
    s.about.update = Value::Null;
    s.desktop(
        env,
        "check_app_update",
        json!({"intent": if manual { "manual" } else { "automatic" }}),
        move |s, _, result| {
            s.about.checking = false;
            match result {
                Ok(update) => s.about.update = update,
                Err(error) => s.about.check_error = Some(error.text()),
            }
        },
    );
}

pub(super) fn install_approved(s: &mut Settings, env: &mut Env, mut approval: Value) {
    if s.about.installing {
        return;
    }
    let id = approval["operationId"]
        .as_str()
        .map_or_else(du::new_operation_id, String::from);
    approval["operationId"] = json!(id);
    s.about.installing = true;
    s.about.install_error = None;
    s.about.operation = Value::Null;
    s.save_flushed(
        env,
        &format!("prefs:{APP_UPDATE_KEY}"),
        json!(id),
        move |s, env, saved| {
            if let Err(error) = saved {
                s.about.installing = false;
                s.about.install_error = Some(error.text());
                return;
            }
            s.desktop(
                env,
                "install_app_update",
                approval,
                move |s, env, result| {
                    accept_operation(s, env, id, result);
                },
            );
        },
    );
}

fn poll_update(s: &mut Settings, env: &mut Env, id: String) {
    s.desktop(
        env,
        "app_update_status",
        json!({"operationId": id}),
        move |s, env, result| accept_operation(s, env, id, result),
    );
}

fn accept_operation(
    s: &mut Settings,
    env: &mut Env,
    id: String,
    result: Result<Value, crate::features::settings::rpc::ReqError>,
) {
    match result {
        Ok(operation) if operation["operationId"] == id && operation["targetKind"] == "app" => {
            let running = du::outcome(&operation) == du::Outcome::Running;
            s.about.operation = operation;
            if running {
                s.after(env, 1.0, move |s, env| poll_update(s, env, id));
                return;
            }
        }
        Ok(_) => s.about.install_error = Some(t("settingsNative.identityChanged")),
        Err(error) => s.about.install_error = Some(error.text()),
    }
    s.about.installing = false;
    env.ctx
        .core
        .save_state(&format!("prefs:{APP_UPDATE_KEY}"), Value::Null);
    check(s, env, false);
}

fn install(s: &mut Settings, _env: &mut Env) {
    let Some(approved) = app_approval(&s.about.update) else {
        return;
    };
    let go = handler(move |s, env| {
        if app_approval(&s.about.update).as_ref() == Some(&approved) {
            install_approved(s, env, approved.clone());
        }
    });
    let message = format!(
        "{}\n{}",
        t("settings.about.updates.installMessage"),
        t("desktop.updates.callout.restartWarning")
    );
    s.confirm(
        t("settings.about.updates.installTitle"),
        message,
        t("settings.about.updates.installConfirm"),
        false,
        go,
    );
}

pub fn draw_about(ui: &mut Ui, s: &Settings, v: &View) {
    let state = &s.about;
    let pal = ui.pal;
    let client = platform::app_version();
    let desktop = DESKTOP && platform::desktop_available();
    let value = |text: String| Ctl::Label {
        text,
        color: pal.muted,
        size: 14.0,
        font: Font::Regular,
        max_w: 220.0,
    };
    ui.section_title(&t("settings.about.title"), None, None);
    ui.card_begin();
    ui.row(
        row(t("settings.about.appVersion"))
            .hint(t("settings.about.thisDevice"))
            .control(value(version_label(client.as_deref()))),
    );
    if desktop {
        let channel = if str_of(&state.settings, "releaseChannel") == "beta" {
            "beta"
        } else {
            "stable"
        };
        let enabled =
            !state.busy && !state.checking && !state.installing && state.settings.is_object();
        let segment = |id: &'static str| {
            let act = (enabled && channel != id).then(|| {
                handler(move |s, env| {
                    s.about.busy = true;
                    s.desktop(
                        env,
                        "desktop_settings_update",
                        json!({"patch": {"releaseChannel": id}}),
                        |s, env, result| {
                            s.about.busy = false;
                            match result {
                                Ok(settings) => {
                                    s.about.error = None;
                                    s.about.settings = settings;
                                    check(s, env, false);
                                }
                                Err(error) => s.about.error = Some(error.text()),
                            }
                        },
                    );
                })
            });
            (
                t(&format!("settings.about.releaseChannel.{id}")),
                channel == id,
                act,
            )
        };
        let control = Ctl::Segmented {
            key: "about.channel".into(),
            items: vec![segment("stable"), segment("beta")],
        };
        ui.row(
            row(t("settings.about.releaseChannel.label"))
                .hint(t("settings.about.releaseChannel.description"))
                .control(control),
        );

        let latest = state.update["latest"]["version"].as_str();
        let has_update = app_approval(&state.update).is_some();
        let mut hint = Vec::new();
        if has_update {
            hint.push(ta(
                "settings.about.updates.updateTo",
                &[("version", &version_label(latest))],
            ));
        } else if state.update.is_object()
            && !state
                .update
                .as_object()
                .map(|m| m.is_empty())
                .unwrap_or(true)
        {
            let current = state.update["current"]["version"]
                .as_str()
                .or(client.as_deref());
            hint.push(version_label(current));
        }
        if let Some(status) = state.update_progress() {
            hint.push(status);
        }
        let error = state
            .install_error
            .clone()
            .or_else(|| state.check_error.clone())
            .or_else(|| state.error.clone())
            .or_else(|| state.update["checkError"].as_str().map(String::from))
            .or_else(|| state.update["supportError"].as_str().map(String::from))
            .or_else(|| state.operation["error"].as_str().map(String::from));
        let check_act =
            (!state.checking && !state.installing).then(|| handler(|s, env| check(s, env, true)));
        let check_label = if state.checking {
            t("settings.about.updates.checking")
        } else {
            t("settings.about.updates.check")
        };
        let mut buttons = vec![button("about.check", check_label, check_act).ctl()];
        if has_update {
            let label = if state.installing {
                t("settings.about.updates.installing")
            } else if latest.is_none() {
                t("settings.about.updates.update")
            } else {
                ta(
                    "settings.about.updates.updateTo",
                    &[("version", &version_label(latest))],
                )
            };
            buttons.push(
                button(
                    "about.install",
                    label,
                    (!state.installing).then(|| handler(install)),
                )
                .variant(Variant::Primary)
                .ctl(),
            );
        }
        ui.row(
            row(t("settings.about.updates.label"))
                .hint(hint.join("\n"))
                .error(error)
                .control(Ctl::Row(buttons, 8.0)),
        );
        let diag = (!state.busy).then(|| {
            handler(|s, env| {
                s.about.busy = true;
                s.desktop(
                    env,
                    "desktop_update_diagnostics",
                    json!({}),
                    |s, _, result| {
                        s.about.busy = false;
                        match result {
                            Ok(report) => s.report(t("settingsNative.updateDiagnostics"), &report),
                            Err(error) => s.about.error = Some(error.text()),
                        }
                    },
                );
            })
        });
        ui.row(
            row(t("settingsNative.updateDiagnostics")).control(
                button("about.diagnostics", t("settings.diagnostics.app.run"), diag).ctl(),
            ),
        );
    }
    ui.card_end();
    ui.gap(24.0);
    if !v.hosts.is_empty() {
        ui.section_title(&t("settings.about.connectedHosts"), None, None);
        ui.card_begin();
        for host in &v.hosts {
            let connected = host.connected();
            let host_version = normalize_version(&host.version());
            let mismatch = matches!((&host_version, &client), (Some(h), Some(c)) if h != c);
            let text = if !connected {
                t("settings.about.offline")
            } else {
                version_label(host_version.as_deref())
            };
            let color = if mismatch { pal.warning } else { pal.muted };
            let hint = mismatch.then(|| t("settings.about.versionDiffers"));
            ui.row(row(host.label.clone()).hint_opt(hint).control(Ctl::Label {
                text,
                color,
                size: 14.0,
                font: Font::Regular,
                max_w: 200.0,
            }));
        }
        ui.card_end();
        ui.gap(24.0);
    }
}
