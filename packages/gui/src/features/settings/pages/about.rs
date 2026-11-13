//! Original `AboutSection`: app version, desktop release channel and app
//! updates, update diagnostics, connected host versions.

use serde_json::{Value, json};

use super::View;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{is_true, scalar_text, str_of};
use crate::features::settings::model::daemon_update::normalize_version;
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
    pub installing: bool,
    pub busy: bool,
    pub error: Option<String>,
    pub loaded: bool,
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
    check(s, env, false);
}

fn load(s: &mut Settings, env: &mut Env) {
    s.desktop(env, "desktop_settings_get", json!({}), |s, _, result| match result {
        Ok(settings) => s.about.settings = settings,
        Err(error) => s.about.error = Some(error.text()),
    });
}

fn check(s: &mut Settings, env: &mut Env, manual: bool) {
    s.about.checking = true;
    s.desktop(env, "check_app_update", json!({"intent": if manual { "manual" } else { "automatic" }}), move |s, _, result| {
        s.about.checking = false;
        match result {
            Ok(update) => s.about.update = update,
            Err(error) if manual => s.about.error = Some(error.text()),
            Err(_) => {}
        }
    });
}

fn install(s: &mut Settings, _env: &mut Env) {
    let go = handler(|s, env| {
        s.about.installing = true;
        s.desktop(env, "install_app_update", json!({}), |s, _, result| {
            s.about.installing = false;
            match result {
                Ok(report) => s.report(t("settings.about.updates.label"), &report),
                Err(error) => s.about.error = Some(error.text()),
            }
        });
    });
    let message = format!("{}\n{}", t("settings.about.updates.installMessage"), t("desktop.updates.callout.restartWarning"));
    s.confirm(t("settings.about.updates.installTitle"), message, t("settings.about.updates.installConfirm"), false, go);
}

pub fn draw_about(ui: &mut Ui, s: &Settings, v: &View) {
    let state = &s.about;
    let pal = ui.pal;
    let client = platform::app_version();
    let desktop = DESKTOP && platform::desktop_available();
    let value = |text: String| Ctl::Label { text, color: pal.muted, size: 14.0, font: Font::Regular, max_w: 220.0 };
    ui.section_title(&t("settings.about.title"), None, None);
    ui.card_begin();
    ui.row(row(t("settings.about.appVersion")).hint(t("settings.about.thisDevice")).control(value(version_label(client.as_deref()))));
    if desktop {
        let channel = if str_of(&state.settings, "releaseChannel") == "beta" { "beta" } else { "stable" };
        let enabled = !state.busy && state.settings.is_object();
        let segment = |id: &'static str| {
            let act = (enabled && channel != id).then(|| {
                handler(move |s, env| {
                    s.about.busy = true;
                    s.desktop(env, "desktop_settings_update", json!({"patch": {"releaseChannel": id}}), |s, env, result| {
                        s.about.busy = false;
                        match result {
                            Ok(_) => {
                                s.about.error = None;
                                load(s, env);
                            }
                            Err(error) => s.about.error = Some(error.text()),
                        }
                    });
                })
            });
            (t(&format!("settings.about.releaseChannel.{id}")), channel == id, act)
        };
        let control = Ctl::Segmented { key: "about.channel".into(), items: vec![segment("stable"), segment("beta")] };
        ui.row(row(t("settings.about.releaseChannel.label")).hint(t("settings.about.releaseChannel.description")).control(control));

        let latest = state.update.get("latestVersion").filter(|v| !v.is_null()).map(scalar_text);
        let ready = is_true(&state.update, "readyToInstall");
        let has_update = is_true(&state.update, "hasUpdate") || ready;
        let mut hint = Vec::new();
        if has_update {
            hint.push(ta("settings.about.updates.updateTo", &[("version", &version_label(latest.as_deref()))]));
        } else if state.update.is_object() && !state.update.as_object().map(|m| m.is_empty()).unwrap_or(true) {
            let current = state.update.get("currentVersion").filter(|v| !v.is_null()).map(scalar_text).or(client.clone());
            hint.push(version_label(current.as_deref()));
        }
        if ready {
            hint.push(ta("settings.about.updates.readyToInstall", &[("version", &version_label(latest.as_deref()))]));
        }
        let error = state.error.clone().or_else(|| state.update.get("errorMessage").filter(|v| !v.is_null()).map(scalar_text));
        let check_act = (!state.checking && !state.installing).then(|| handler(|s, env| check(s, env, true)));
        let check_label = if state.checking { t("settings.about.updates.checking") } else { t("settings.about.updates.check") };
        let mut buttons = vec![button("about.check", check_label, check_act).ctl()];
        if has_update {
            let label = if state.installing {
                t("settings.about.updates.installing")
            } else if latest.is_none() {
                t("settings.about.updates.update")
            } else {
                ta("settings.about.updates.updateTo", &[("version", &version_label(latest.as_deref()))])
            };
            buttons.push(button("about.install", label, (!state.installing).then(|| handler(install))).variant(Variant::Primary).ctl());
        }
        ui.row(row(t("settings.about.updates.label")).hint(hint.join("\n")).error(error).control(Ctl::Row(buttons, 8.0)));
        let diag = (!state.busy).then(|| {
            handler(|s, env| {
                s.about.busy = true;
                s.desktop(env, "desktop_update_diagnostics", json!({}), |s, _, result| {
                    s.about.busy = false;
                    match result {
                        Ok(report) => s.report(t("settingsNative.updateDiagnostics"), &report),
                        Err(error) => s.about.error = Some(error.text()),
                    }
                });
            })
        });
        ui.row(row(t("settingsNative.updateDiagnostics")).control(button("about.diagnostics", t("settings.diagnostics.app.run"), diag).ctl()));
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
            let text = if !connected { t("settings.about.offline") } else { version_label(host_version.as_deref()) };
            let color = if mismatch { pal.warning } else { pal.muted };
            let hint = mismatch.then(|| t("settings.about.versionDiffers"));
            ui.row(row(host.label.clone()).hint_opt(hint).control(Ctl::Label { text, color, size: 14.0, font: Font::Regular, max_w: 200.0 }));
        }
        ui.card_end();
        ui.gap(24.0);
    }
}
