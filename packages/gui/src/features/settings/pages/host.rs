//! Host settings: overview, projects, connections, and the explicit Version
//! page. Durable updates use the daemon's persisted operation executor.

use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::View;
use super::about;
use super::app::{Pick, select_ctl_lead};
use crate::common::{Route, UiEffect};
use crate::features::settings::host::{
    self, HostView, Status, active_connection_label, config_with, connection_badge,
    connection_label,
};
use crate::features::settings::i18n::{t, ta, with_detail};
use crate::features::settings::json::{get, str_of};
use crate::features::settings::model::daemon_update::{self as du, Outcome};
use crate::features::settings::model::forms::{FormSave, FormSheet, SettingField};
use crate::features::settings::platform::{self, DESKTOP, MOBILE};
use crate::features::settings::rpc::{Env, ReqError, handler};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::kit::{
    BSize, Ctl, Lead, Ui, Variant, button, icon_button, row, switch,
};
use crate::features::settings::ui::theme::{IDENTITY_COLORS, identity_fill};

// ------------------------------------------------------------ no host / pairing

pub fn draw_no_host(ui: &mut Ui) {
    ui.section_title(&t("settings.groups.host"), None, None);
    ui.card_begin();
    let add = handler(|_, env| env.ctx.effects.push(UiEffect::OpenHostDialog));
    ui.row(
        row(t("settings.host.notFound"))
            .control(button("nohost.add", t("settings.addHost"), Some(add)).ctl()),
    );
    ui.card_end();
    ui.gap(24.0);
}

/// Pairing row on the Connections page (desktop only: a phone shows the QR to nobody).
fn draw_pair_device(ui: &mut Ui, host: &HostView) {
    ui.section_title(&t("settings.host.pairDevices.title"), None, None);
    ui.card_begin();
    let id = host.server_id.clone();
    let pair = handler(move |_, env| {
        env.ctx.effects.push(UiEffect::OpenHostPairing {
            host_id: id.clone(),
        })
    });
    ui.row(
        row(t("settings.host.pairDevices.rowTitle"))
            .hint(t("settings.host.pairDevices.rowHint"))
            .control(
                button("pair.open", t("settingsNative.showQrCode"), Some(pair))
                    .icon(Icon::Smartphone)
                    .ctl(),
            ),
    );
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ projects

/// Original `ProjectsScreen`; a row opens the project settings in place.
/// Loading, offline, failed and truly empty hosts each draw a distinct state.
pub fn draw_projects(ui: &mut Ui, _s: &Settings, host: &HostView) {
    let pal = ui.pal;
    let loading = host.loading_projects || host.status == Status::Connecting;
    if host.projects.is_empty() && loading {
        let (x, y, w) = (ui.x, ui.y, ui.w);
        ui.draw_ctl(
            Ctl::Spinner(36.0),
            makepad_widgets::dvec2(x + (w - 36.0) / 2.0, y + 24.0),
        );
        ui.y += 84.0;
        return;
    }
    let id = host.server_id.clone();
    let retry = (!loading).then(|| handler(move |_, env| env.ctx.core.refresh_host(&id)));
    let retry = button(
        format!("projects.{}.retry", host.server_id),
        t("common.actions.retry"),
        retry,
    )
    .icon(Icon::RotateCw)
    .ctl();
    let error = host
        .project_error
        .as_deref()
        .or(host.error.as_deref())
        .map(str::trim)
        .filter(|e| !e.is_empty());
    if !host.connected() {
        ui.warning(
            &ta(
                "settings.projectList.hostOfflineTitle",
                &[("hostName", &host.label)],
            ),
            Some(&match error {
                Some(error) => with_detail(&t("settings.projectList.hostOfflineMessage"), error),
                None => t("settings.projectList.hostOfflineMessage"),
            }),
        );
        ui.gap(12.0);
        ui.place(retry);
        ui.gap(24.0);
    } else if let Some(error) = error {
        ui.alert(
            false,
            &ta(
                "settings.projectList.hostLoadFailed",
                &[("hostName", &host.label), ("message", error)],
            ),
            None,
            Some(retry),
        );
        ui.gap(24.0);
    } else if host.projects.is_empty() {
        ui.empty_card(&t("sidebar.project.empty.title"), None, true);
        ui.gap(24.0);
    }
    if host.projects.is_empty() {
        return;
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
            r = r.tap(
                format!("projects.{i}"),
                handler(move |s, _| s.project_root = Some(root.clone())),
            );
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
        Status::Connected => (
            Some(pal.success),
            if dark {
                crate::features::settings::ui::theme::rgb(0x35c264)
            } else {
                crate::features::settings::ui::theme::rgb(0x299f51)
            },
            "Online",
        ),
        Status::Connecting => (
            Some(pal.warning),
            if dark {
                crate::features::settings::ui::theme::rgb(0xdb932e)
            } else {
                crate::features::settings::ui::theme::rgb(0xb37824)
            },
            "Connecting",
        ),
        Status::Error => (
            Some(pal.danger),
            if dark {
                crate::features::settings::ui::theme::rgb(0xf7796d)
            } else {
                crate::features::settings::ui::theme::rgb(0xf12e2f)
            },
            "Error",
        ),
        Status::Disconnected => (None, pal.muted, "Offline"),
    };
    Ctl::Badge {
        label: status_label(key),
        tone,
        xs: false,
        dot: Some(dot),
        icon: None,
    }
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

pub fn status_color(
    host: &HostView,
    pal: &crate::features::settings::ui::theme::Palette,
) -> makepad_widgets::Vec4 {
    match host.status {
        Status::Connected => pal.success,
        Status::Connecting => pal.warning,
        _ => pal.danger,
    }
}

pub fn draw_connection_error(ui: &mut Ui, host: &HostView) {
    if let Some(message) = host
        .error
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        ui.alert(
            false,
            &t("pairing.direct.errors.failedTitle"),
            Some(message),
            None,
        );
        ui.gap(24.0);
    }
}

fn rename(s: &mut Settings, host: &HostView) {
    let config = host.config.clone();
    let host_id = host.server_id.clone();
    let fields = vec![SettingField::text(
        "label",
        t("settings.host.daemon.rename.placeholder"),
        str_of(&config, "label"),
    )];
    let save: FormSave = Rc::new(move |s, env, sheet, values| {
        let label = values
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
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

pub fn draw_overview_page(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    let pal = ui.pal;
    let mut badges = vec![status_badge(host, &pal)];
    if let Some((remote, label)) = connection_badge(host) {
        badges.push(Ctl::Badge {
            label,
            tone: None,
            xs: false,
            dot: None,
            icon: Some(if remote { Icon::Globe } else { Icon::Monitor }),
        });
    }
    let version = host.version();
    if !version.is_empty() {
        let shown = if version.starts_with('v') {
            version
        } else {
            format!("v{version}")
        };
        badges.push(Ctl::Badge {
            label: shown,
            tone: None,
            xs: false,
            dot: None,
            icon: None,
        });
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
            Ctl::Label {
                text: host.label.clone(),
                color: pal.muted,
                size: 14.0,
                font: Font::Regular,
                max_w: 220.0,
            },
            Ctl::IconButton {
                key: format!("host.{}.rename", host.server_id),
                icon: Icon::Pencil,
                act: Some(rename_act),
                destructive: false,
                size: 24.0,
                tip: None,
            },
        ],
        8.0,
    );
    ui.row(
        row(t("settings.host.appearance.name.label"))
            .inline()
            .control(name_ctl),
    );
    let current = str_of(&host.config, "color");
    let current = if IDENTITY_COLORS.contains(&current) {
        current.to_string()
    } else {
        "none".into()
    };
    let mut options = vec![(
        "none".to_string(),
        t("settings.host.appearance.color.options.none"),
    )];
    let mut leads = vec![("none".to_string(), Lead::Swatch(None), false)];
    for name in IDENTITY_COLORS {
        options.push((
            name.to_string(),
            t(&format!("settings.host.appearance.color.options.{name}")),
        ));
        leads.push((name.to_string(), Lead::Swatch(identity_fill(name)), false));
    }
    let config = host.config.clone();
    let pick: Pick = Rc::new(move |_, env, picked| {
        let next = config_with(
            &config,
            None,
            Some(if picked == "none" {
                None
            } else {
                Some(picked.as_str())
            }),
        );
        if next != config {
            host::update_host_config(&env.ctx.core, next);
        }
    });
    let color = select_ctl_lead(
        &format!("host.{}.color", host.server_id),
        t("settings.host.appearance.color.label"),
        &current,
        options,
        leads,
        200.0,
        true,
        pick,
    );
    ui.row(row(t("settings.host.appearance.color.label")).control(color));
    ui.card_end();
    ui.gap(24.0);

    if DESKTOP && host.local && platform::desktop_available() {
        super::system::draw_local_daemon(ui, s, true);
    }
    super::daemon::draw_host_config(ui, s, host);
    draw_danger_zone(ui, s, host);
}

// ------------------------------------------------------------ connections

pub fn draw_connections_page(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    draw_connection_error(ui, host);
    ui.section_title(&t("settings.host.connections.title"), None, None);
    ui.card_begin();
    let active = str_of(&host.config, "id").to_string();
    let last = host.connections.len() <= 1;
    for (i, connection) in host.connections.iter().enumerate() {
        let label = connection_label(connection);
        let server = host.server_id.clone();
        let connection_id = str_of(connection, "id").to_string();
        let is_active = !connection_id.is_empty() && connection_id == active;
        let key = format!("conn.{}.{i}", host.server_id);
        let mut controls = Vec::new();
        if !is_active {
            let chosen = connection.clone();
            let use_it =
                handler(move |_, env| host::update_host_config(&env.ctx.core, chosen.clone()));
            controls.push(
                button(
                    format!("{key}.use"),
                    t("settingsNative.useConnection"),
                    Some(use_it),
                )
                .variant(Variant::Ghost)
                .ctl(),
            );
        }
        let edited = connection.clone();
        let edit = handler(move |_, env| {
            env.ctx.effects.push(UiEffect::OpenHostConnectionDialog {
                connection: edited.clone(),
            })
        });
        controls.push(
            icon_button(format!("{key}.edit"), Icon::Pencil, Some(edit))
                .tipped(t("settingsNative.editConnection")),
        );
        let remove = if last {
            // The last connection removes the whole host: say so (`remove.*`).
            remove_host_handler(host)
        } else {
            handler(move |s, _| {
                let server = server.clone();
                let connection_id = connection_id.clone();
                let go = handler(move |_, env| {
                    host::remove_connection(&env.ctx.core, &server, &connection_id)
                });
                s.confirm(
                    t("settings.host.connections.removeTitle"),
                    ta(
                        "settings.host.connections.removeMessage",
                        &[("name", &label)],
                    ),
                    t("settings.host.connections.removeAction"),
                    true,
                    go,
                );
            })
        };
        controls.push(
            button(
                format!("{key}.remove"),
                t("settings.host.connections.removeAction"),
                Some(remove),
            )
            .variant(Variant::Ghost)
            .danger_text()
            .ctl(),
        );
        let hint = is_active.then(|| t("settingsNative.activeConnection"));
        ui.row(
            row(connection_label(connection))
                .one_line()
                .hint_opt(hint)
                .inline()
                .control(Ctl::Row(controls, 4.0)),
        );
    }
    if host.connections.is_empty() {
        ui.row(row(t("settingsNative.noConnections")).one_line());
    }
    let (seed_label, seed_server, seed_color) = (
        str_of(&host.config, "label").to_owned(),
        host.server_id.clone(),
        host.config.get("color").cloned().unwrap_or(Value::Null),
    );
    let add = handler(move |_, env| {
        // Shell `ConnectionsSheet` seed: a fresh connection id expecting this server.
        let connection = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "label": seed_label,
            "endpoint": "",
            "expectedServerId": seed_server,
            "color": seed_color,
        });
        env.ctx
            .effects
            .push(UiEffect::OpenHostConnectionDialog { connection });
    });
    ui.row(
        row(t("settingsNative.addConnection"))
            .hint(t("settingsNative.addConnectionHint"))
            .control(
                button(
                    format!("conn.{}.add", host.server_id),
                    t("settingsNative.addConnection"),
                    Some(add),
                )
                .icon(Icon::Plus)
                .ctl(),
            ),
    );
    ui.card_end();
    ui.gap(24.0);
    if !MOBILE {
        draw_pair_device(ui, host);
    }
    if host.connected() {
        if host.feature("connectedHosts") {
            super::daemon::draw_connected_hosts(ui, s, host);
        }
        super::daemon::draw_hub(ui, s, host);
    }
}

// ------------------------------------------------------------ danger zone

/// Remove a saved connection after the shell's tab-close guards. The daemon
/// lifecycle and the application start preference are unaffected.
fn remove_host_handler(host: &HostView) -> crate::features::settings::rpc::Handler {
    let local = host.local;
    let id = host.server_id.clone();
    let label = host.label.clone();
    handler(move |s, _| {
        let id = id.clone();
        let go = handler(move |s, env| {
            env.ctx.effects.push(UiEffect::RemoveHost {
                host_id: id.clone(),
            });
            s.host = None;
            s.section = if env.ctx.compact {
                None
            } else {
                Some("general".into())
            };
            let section = s.section.clone().unwrap_or_default();
            env.navigate(Route::Settings {
                host_id: None,
                page: section,
            });
        });
        let (title, message, action) = if local {
            (
                t("settings.host.daemon.remove.localConfirmTitle"),
                t("settings.host.daemon.remove.localConfirmMessage"),
                t("settings.host.daemon.remove.localTitle"),
            )
        } else {
            (
                t("settings.host.daemon.remove.title"),
                ta(
                    "settings.host.daemon.remove.confirmMessage",
                    &[("name", &label)],
                ),
                t("settings.host.daemon.remove.title"),
            )
        };
        s.confirm(title, message, action, true, go);
    })
}

fn draw_danger_zone(ui: &mut Ui, s: &Settings, host: &HostView) {
    ui.section_title(&t("settings.host.daemon.dangerZone"), None, None);
    draw_restart_card(ui, s, host);
    ui.gap(12.0);
    ui.card_begin();
    let (title, hint) = if host.local {
        (
            "settings.host.daemon.remove.localTitle",
            "settings.host.daemon.remove.localHint",
        )
    } else {
        (
            "settings.host.daemon.remove.title",
            "settings.host.daemon.remove.hint",
        )
    };
    let remove = remove_host_handler(host);
    ui.row(
        row(t(title)).hint(t(hint)).control(
            button(
                format!("host.{}.remove", host.server_id),
                t("settings.host.connections.removeAction"),
                Some(remove),
            )
            .ctl(),
        ),
    );
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ restart

/// Original `readSelectedWorker`: worker status, refusing another daemon.
fn read_worker(
    s: &mut Settings,
    env: &mut Env,
    server_id: &str,
    then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
) {
    let id = server_id.to_string();
    s.request(
        env,
        server_id,
        "daemon.get_status.request",
        json!({}),
        "daemon.get_status.response",
        move |s, env, result| {
            let result = result.and_then(|status| identity_checked(env, &id).map(|_| status));
            then(s, env, result);
        },
    );
}

/// The host still reports the same daemon identity (`Daemon identity changed`).
fn identity_checked(env: &Env, server_id: &str) -> Result<(), ReqError> {
    match env.host(server_id) {
        Some(host) if host.reported_server_id() == server_id => Ok(()),
        Some(_) => Err(ReqError::msg(t("settingsNative.identityChanged"))),
        None => Err(ReqError {
            code: "DAEMON_NOT_CONNECTED".into(),
            message: t("settingsNative.identityChanged"),
            outcome_unknown: false,
        }),
    }
}

/// Repeatable completion shared by the restart/update observation retries.
type Observer = Rc<dyn Fn(&mut Settings, &mut Env, Result<Value, ReqError>)>;

/// Original `observeReplacement`: poll every 250 ms (10 min) until a worker
/// with a new PID answers; offline polls retry.
fn observe_replacement(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    previous: Value,
    deadline: Instant,
    then: Observer,
) {
    if Instant::now() >= deadline {
        then(
            s,
            env,
            Err(ReqError::msg(t("settingsNative.replacementUnconfirmed"))),
        );
        return;
    }
    let id = server_id.clone();
    read_worker(s, env, &server_id, move |s, env, result| match result {
        Ok(current) if current.get("pid") != previous.get("pid") => then(s, env, Ok(current)),
        Err(error) if !error.is_reconnect() => then(s, env, Err(error)),
        _ => s.after(env, 0.25, move |s, env| {
            observe_replacement(s, env, id, previous, deadline, then)
        }),
    });
}

pub(super) fn restart(s: &mut Settings, env: &mut Env, server_id: String) {
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
        s.request(
            env,
            &id,
            "restart_server_request",
            json!({"reason": format!("settings_daemon_restart_{id}")}),
            "status",
            move |s, env, result| {
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
                        let message = ta(
                            "settingsNative.restartAcknowledged",
                            &[
                                ("acknowledged", &acknowledged.to_string()),
                                ("error", &error.text()),
                            ],
                        );
                        env.toast(message, true);
                    }
                });
                observe_replacement(
                    s,
                    env,
                    id2,
                    previous,
                    Instant::now() + Duration::from_secs(600),
                    done,
                );
            },
        );
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
            s.confirm(
                ta(
                    "settings.host.daemon.restart.confirmTitle",
                    &[("name", &label)],
                ),
                rt("confirmMessage"),
                rt("confirm"),
                true,
                go,
            );
        })
    });
    ui.card_begin();
    let label = rt(if restarting { "restarting" } else { "confirm" });
    ui.row(
        row(rt("title")).hint(rt("hint")).control(
            button(format!("host.{}.restart", host.server_id), label, act)
                .icon(Icon::RotateCw)
                .ctl(),
        ),
    );
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
    pub operation_id: Option<String>,
    pub submit_request: Option<Value>,
    pub check: Option<Value>,
    pub selected_components: Vec<String>,
    pub checking: bool,
    pub checked: bool,
    pub check_generation: u64,
    pub check_error: String,
    pub force_running: bool,
    pub app_update: bool,
    pub action_required: bool,
}

fn ut(key: &str) -> String {
    t(&format!("settings.host.daemon.update.{key}"))
}

fn uta(key: &str, args: &[(&str, &str)]) -> String {
    ta(&format!("settings.host.daemon.update.{key}"), args)
}

/// Localized description for a failed update check/support probe. Known
/// transport codes and daemon phrases map to update copy; the raw daemon text
/// stays as the trailing detail, and unknown failures keep it alone under the
/// alert's localized title.
fn update_error_text(code: &str, raw: &str) -> String {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    let key = match code {
        "DAEMON_NOT_CONNECTED" | "DAEMON_CONNECTION_LOST" | "DAEMON_UNREACHABLE" => {
            "unavailableMessage"
        }
        "DAEMON_REQUEST_TIMEOUT" => "timedOutMessage",
        _ if lower.contains("timed out") || lower.contains("timeout") => "timedOutMessage",
        _ if lower.contains("not connected") || lower.contains("offline") => "unavailableMessage",
        _ if lower.contains("unsupported")
            || lower.contains("not supported")
            || lower.contains("unknown request") =>
        {
            "unsupportedLegacy"
        }
        _ => return raw.to_string(),
    };
    with_detail(&ut(key), raw)
}

pub(super) fn release_check_error_text(check: &Value) -> Option<String> {
    let code = check["checkErrorCode"]
        .as_str()
        .or_else(|| check["errorCode"].as_str());
    if code == Some("github_rate_limited")
        && let Some(retry) = check["retryAt"]
            .as_str()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    {
        let time = retry
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        return Some(ta("settingsNative.githubRateLimited", &[("time", &time)]));
    }
    check["checkError"]
        .as_str()
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(|raw| update_error_text("", raw))
}

/// The daemon (`updater.rs`) and desktop updater report a release channel with
/// no component inventory for this platform with this phrase; the check itself
/// succeeded, there is simply nothing published yet.
fn not_published(raw: &str) -> bool {
    raw.contains("component inventory is published")
}

/// Localized support-probe failure. A service-manager query that cannot run on
/// this installation explains why self-update is unavailable and keeps only the
/// tool's final message, never the full command line, as detail.
fn support_error_text(raw: &str) -> String {
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if [
        "systemctl",
        "launchctl",
        "user scope bus",
        "service manager",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return with_detail(&ut("serviceUnmanaged"), short_detail(raw));
    }
    update_error_text("", raw)
}

/// The innermost cause of a `command … failed (exit status: N): cause` chain.
fn short_detail(raw: &str) -> &str {
    raw.rsplit_once("): ")
        .map_or(raw, |(_, cause)| cause)
        .trim()
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
    if host.server_id.is_empty() {
        return;
    }
    let c = card(s, &host.server_id);
    if c.resumed || c.state != UpdatePhase::Idle {
        return;
    }
    if c.operation_id.is_none() {
        if host::has_unusable_update_record(&env.ctx.state, &host.server_id) {
            host::clear_update_operation(&env.ctx.core, &host.server_id);
        }
        c.operation_id = host::stored_update_operation(&env.ctx.state, &host.server_id);
        c.submit_request = host::stored_update_request(&env.ctx.state, &host.server_id);
        c.app_update = c
            .submit_request
            .as_ref()
            .is_some_and(|v| v["targetKind"] == "app");
        c.selected_components = c
            .submit_request
            .as_ref()
            .and_then(|request| request["selectedComponents"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str().map(String::from))
            .collect();
        c.force_running = c
            .submit_request
            .as_ref()
            .is_some_and(|request| request["forceRunning"] == true);
    }
    if !host.connected() {
        return;
    }
    c.resumed = true;
    if let Some(operation) = c.operation_id.clone() {
        durable_update(s, env, host.server_id.clone(), operation, true);
    }
}

pub fn reset_version_check(s: &mut Settings, server_id: &str) {
    if let Some(c) = s.update_cards.get_mut(server_id) {
        c.check_generation = c.check_generation.wrapping_add(1);
        c.checking = false;
        c.checked = false;
        c.check = None;
    }
}

pub fn ensure_version_page(s: &mut Settings, env: &mut Env, host: &HostView) {
    ensure_update_card(s, env, host);
    if has_local_app(host) {
        about::ensure_update(s, env);
    }
    let c = card(s, &host.server_id);
    if host.connected() && !c.checked && !c.checking {
        check_version(s, env, host.server_id.clone());
    }
}

fn check_version(s: &mut Settings, env: &mut Env, server_id: String) {
    let Some(host) = env.host(&server_id).filter(HostView::connected) else {
        return;
    };
    let c = card(s, &server_id);
    if c.checking {
        return;
    }
    c.checked = true;
    c.check_error.clear();
    c.check = None;
    c.checking = true;
    c.check_generation = c.check_generation.wrapping_add(1);
    let generation = c.check_generation;
    let id = server_id.clone();
    if !du::supports_check(&host.server_info) {
        read_worker(s, env, &server_id, move |s, _, result| {
            let c = card(s, &id);
            if c.check_generation != generation {
                return;
            }
            c.checking = false;
            match result {
                Ok(worker) => c.check = Some(json!({"current": worker})),
                Err(error) => c.check_error = update_error_text(&error.code, &error.message),
            }
        });
        return;
    }
    update_request(
        s,
        env,
        &server_id,
        "daemon.update.check.request",
        json!({}),
        "daemon.update.check.response",
        move |s, _, result| {
            let c = card(s, &id);
            if c.check_generation != generation {
                return;
            }
            c.checking = false;
            match result {
                Ok(value) if str_of(&value, "serverId") == id => {
                    if c.operation_id.is_none() {
                        c.selected_components = du::changed_components(&value);
                    }
                    c.check = Some(value);
                }
                Ok(_) => c.check_error = t("settingsNative.identityChanged"),
                Err(error) => c.check_error = update_error_text(&error.code, &error.message),
            }
        },
    );
}

/// Status/submit request against the host's current connection with the
/// Flutter 30 s deadline and the identity check.
fn update_request(
    s: &mut Settings,
    env: &mut Env,
    server_id: &str,
    kind: &str,
    fields: Value,
    response: &str,
    then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
) {
    if let Err(error) = identity_checked(env, server_id) {
        then(s, env, Err(error));
        return;
    }
    let id = server_id.to_string();
    s.request_timeout(
        env,
        server_id,
        kind,
        fields,
        response,
        Some(Duration::from_secs(30)),
        move |s, env, result| {
            let result = result.and_then(|value| identity_checked(env, &id).map(|_| value));
            then(s, env, result);
        },
    );
}

fn durable_update(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    operation_id: String,
    resumed: bool,
) {
    {
        let c = card(s, &server_id);
        c.state = UpdatePhase::Updating;
        c.durable = true;
        c.operation_id = Some(operation_id.clone());
        c.phase = ut("phaseStarting");
        c.action_required = false;
    }
    if resumed {
        if card(s, &server_id).submit_request.is_some() {
            // This exact request was flushed before its first submission.
            // Idempotent resubmission also recovers a crash before that send.
            submit(
                s,
                env,
                server_id,
                operation_id,
                Instant::now() + Duration::from_secs(120),
            );
            return;
        }
        observe(
            s,
            env,
            server_id,
            operation_id,
            Instant::now() + Duration::from_secs(30 * 60),
            None,
        );
        return;
    }
    // Persist the complete approval before sending; never recreate a request
    // from its ID after a reconnect.
    let Some(record) = card(s, &server_id).submit_request.clone() else {
        fail(
            s,
            &server_id,
            ut("requestFailedTitle"),
            ut("approvalMissing"),
        );
        return;
    };
    let key = format!("prefs:{}", host::update_operation_key(&server_id));
    s.save_flushed(env, &key, record, move |s, env, saved| {
        if let Err(error) = saved {
            card(s, &server_id).operation_id = None;
            card(s, &server_id).submit_request = None;
            fail(
                s,
                &server_id,
                ut("requestFailedTitle"),
                uta("requestFailedMessage", &[("error", &error.text())]),
            );
            return;
        }
        submit(
            s,
            env,
            server_id,
            operation_id,
            Instant::now() + Duration::from_secs(120),
        );
    });
}

/// Original `submitDaemonUpdate`: while the outcome is unknown the SAME
/// operation id is resubmitted (the daemon returns the admitted operation).
fn submit(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    operation_id: String,
    deadline: Instant,
) {
    let Some(mut fields) = card(s, &server_id).submit_request.clone() else {
        fail(
            s,
            &server_id,
            ut("requestFailedTitle"),
            ut("approvalMissing"),
        );
        return;
    };
    // This is an app-local continuation, never part of the daemon wire request.
    fields
        .as_object_mut()
        .unwrap()
        .remove("afterDaemonAppUpdate");
    let sid = server_id.clone();
    let oid = operation_id.clone();
    update_request(
        s,
        env,
        &server_id,
        "daemon.update.submit.request",
        fields,
        "daemon.update.submit.response",
        move |s, env, result| match result
            .and_then(|r| du::operation_of(&r, &oid, &sid).map_err(ReqError::msg))
        {
            Ok(operation) if terminal_update_ready(&operation) => {
                // Resuming a saved approval can return an already settled
                // journal from the submit acknowledgement itself.
                finish(s, env, &sid, &oid, operation);
            }
            Ok(_) => observe(
                s,
                env,
                sid,
                oid,
                Instant::now() + Duration::from_secs(30 * 60),
                None,
            ),
            Err(error) if error.is_reconnect() && Instant::now() < deadline => {
                s.after(env, 1.0, move |s, env| submit(s, env, sid, oid, deadline));
            }
            Err(error) => settle_error(s, env, &sid, error),
        },
    );
}

/// Keep the retry identity even on an observation error: transport errors do
/// not prove that admission or the detached executor failed.
fn settle_error(s: &mut Settings, _env: &mut Env, server_id: &str, error: ReqError) {
    if error.is_reconnect() {
        fail(
            s,
            server_id,
            ut("stillRunningTitle"),
            ut("stillRunningHint"),
        );
    } else {
        fail(
            s,
            server_id,
            ut("requestFailedTitle"),
            uta("requestFailedMessage", &[("error", &error.text())]),
        );
    }
}

fn terminal_update_ready(operation: &Value) -> bool {
    operation["terminal"] == true && operation["continuation"]["state"] != "pending"
}

/// Original `observeDaemonUpdate`: poll status every second until terminal
/// and any session resume has left `pending`; connection loss only pauses.
fn observe(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    operation_id: String,
    deadline: Instant,
    last: Option<Value>,
) {
    if Instant::now() >= deadline {
        match last {
            Some(last) if last.get("terminal").and_then(Value::as_bool) == Some(true) => {
                finish(s, env, &server_id, &operation_id, last)
            }
            _ => fail(
                s,
                &server_id,
                ut("stillRunningTitle"),
                ut("stillRunningHint"),
            ),
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
        move |s, env, result| match result
            .and_then(|r| du::operation_of(&r, &oid, &sid).map_err(ReqError::msg))
        {
            Ok(operation) => {
                let c = card(s, &sid);
                c.phase = ut(du::phase_key(str_of(&operation, "phase")));
                c.app_update = operation["targetKind"] == "app";
                c.action_required = operation["actionRequired"] == "osAuthorization"
                    || operation["phase"] == "waitingForAuthorization";
                if terminal_update_ready(&operation) {
                    finish(s, env, &sid, &oid, operation);
                } else {
                    s.after(env, 1.0, move |s, env| {
                        observe(s, env, sid, oid, deadline, Some(operation))
                    });
                }
            }
            Err(error)
                if error.code == "UPDATE_OPERATION_NOT_FOUND"
                    && card(s, &sid).submit_request.is_some() =>
            {
                submit(s, env, sid, oid, Instant::now() + Duration::from_secs(120));
            }
            Err(error) if error.is_reconnect() => s.after(env, 1.0, move |s, env| {
                observe(s, env, sid, oid, deadline, last)
            }),
            Err(error) => settle_error(s, env, &sid, error),
        },
    );
}

fn finish(s: &mut Settings, env: &mut Env, server_id: &str, operation_id: &str, operation: Value) {
    match du::outcome(&operation) {
        Outcome::Committed if operation["targetKind"] == "app" => {
            card(s, server_id).phase = ut("phaseVerifying");
            confirm_committed_app(
                s,
                env,
                server_id.to_string(),
                operation_id.to_string(),
                operation,
                Instant::now() + Duration::from_secs(120),
            );
        }
        Outcome::Committed
            if !du::committed_daemon_matches(&operation, server_id)
                || !du::committed_release_matches(
                    &operation,
                    card(s, server_id).submit_request.as_ref(),
                ) =>
        {
            fail(
                s,
                server_id,
                ut("requestFailedTitle"),
                ut("versionUnconfirmed"),
            );
        }
        _ => complete_operation(s, env, server_id, operation_id, operation),
    }
}

fn confirm_committed_app(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    operation_id: String,
    operation: Value,
    deadline: Instant,
) {
    let sid = server_id.clone();
    let then = move |s: &mut Settings, env: &mut Env, worker: Result<Value, ReqError>| {
        let approved =
            du::committed_release_matches(&operation, card(s, &sid).submit_request.as_ref());
        let confirmed = approved
            && worker
                .as_ref()
                .is_ok_and(|worker| du::committed_app_components_match(&operation, worker, &sid));
        if confirmed {
            complete_operation(s, env, &sid, &operation_id, operation);
        } else if Instant::now() < deadline
            && worker.as_ref().err().is_none_or(ReqError::is_reconnect)
        {
            s.after(env, 1.0, move |s, env| {
                confirm_committed_app(s, env, sid, operation_id, operation, deadline)
            });
        } else {
            fail(
                s,
                &sid,
                ut("unableToReconnectTitle"),
                ut("versionUnconfirmed"),
            );
        }
    };
    update_request(
        s,
        env,
        &server_id,
        "daemon.update.check.request",
        json!({}),
        "daemon.update.check.response",
        then,
    );
}

fn complete_operation(
    s: &mut Settings,
    env: &mut Env,
    server_id: &str,
    operation_id: &str,
    operation: Value,
) {
    let app = if operation["targetKind"] != "app"
        && env.host(server_id).is_some_and(|host| has_local_app(&host))
    {
        card(s, server_id)
            .submit_request
            .as_mut()
            .and_then(|request| request.as_object_mut()?.remove("afterDaemonAppUpdate"))
    } else {
        None
    };
    let has_deferred_app = app.is_some();
    if !du::retain_operation(&operation) {
        host::clear_update_operation(&env.ctx.core, server_id);
        let c = card(s, server_id);
        c.operation_id = None;
        c.submit_request = None;
    }
    let error = str_of(&operation, "error").to_string();
    match du::outcome(&operation) {
        Outcome::Committed => {
            let resume = du::unconfirmed_continuation(&operation);
            let requested = operation
                .get("continuation")
                .map(Value::is_object)
                .unwrap_or(false);
            let note = match (requested, resume) {
                (false, _) => ut(if operation["targetKind"] == "app" {
                    "appCommittedMessage"
                } else {
                    "committedMessage"
                }),
                (true, None) => ut("continuationAccepted"),
                (true, Some(state)) if du::retain_operation(&operation) => {
                    uta("continuationRetained", &[("state", &state)])
                }
                (true, Some(state)) => uta("continuationUnconfirmed", &[("state", &state)]),
            };
            let c = card(s, server_id);
            c.state = UpdatePhase::Complete;
            c.worker_version = str_of(get(&operation, "candidate"), "version").to_string();
            c.complete_note = note;
            reset_version_check(s, server_id);
            if let Some(approved) = app {
                about::install_approved(s, env, approved);
            }
        }
        Outcome::RolledBack => fail(
            s,
            server_id,
            ut("rolledBackTitle"),
            uta(
                if operation["targetKind"] == "app" {
                    "appRolledBackMessage"
                } else {
                    "rolledBackMessage"
                },
                &[("id", operation_id), ("error", &error)],
            ),
        ),
        Outcome::Failed | Outcome::Running => fail(
            s,
            server_id,
            ut("requestFailedTitle"),
            uta(
                "operationFailedMessage",
                &[("id", operation_id), ("error", &error)],
            ),
        ),
    }
    if card(s, server_id).state == UpdatePhase::Failed && has_deferred_app {
        s.about.install_error = Some(ut("appLocal.notStarted"));
    }
}

fn has_local_app(host: &HostView) -> bool {
    DESKTOP && host.local
}

fn start_update(
    s: &mut Settings,
    env: &mut Env,
    server_id: String,
    after_daemon_app_update: Option<Value>,
) {
    let Some(host) = env.host(&server_id) else {
        return;
    };
    if !host.connected() {
        fail(s, &server_id, ut("offlineTitle"), ut("offlineMessage"));
        return;
    }
    if card(s, &server_id).state == UpdatePhase::Updating {
        return;
    }
    if let Some(operation) = card(s, &server_id).operation_id.clone() {
        durable_update(s, env, server_id, operation, true);
        return;
    }
    if has_local_app(&host) && du::target_kind(&host.server_info) != "daemon" {
        return;
    }
    if du::supports_durable(&host.server_info) {
        let c = card(s, &server_id);
        let expected = c.check.as_ref().and_then(|v| v.get("latest")).cloned();
        if du::supports_check(&host.server_info)
            && !c
                .check
                .as_ref()
                .is_some_and(|v| du::can_update_host(&host.server_info, v))
        {
            return;
        }
        let operation = du::new_operation_id();
        c.app_update = du::target_kind(&host.server_info) == "app";
        if c.app_update
            && (c.selected_components.is_empty()
                || !c
                    .check
                    .as_ref()
                    .is_some_and(|check| du::selection_valid(check, &c.selected_components)))
        {
            return;
        }
        let force = c.force_running
            && (!c.app_update || du::restart_group(&c.selected_components) != "none");
        let mut request = json!({"operationId": operation, "source": "release",
            "targetKind": du::target_kind(&host.server_info),
            "expectedRelease": expected, "forceRunning": force, "idleTimeoutMs": 300_000});
        if c.app_update {
            request["selectedComponents"] = json!(c.selected_components);
        }
        if let Some(approved) = after_daemon_app_update {
            request["afterDaemonAppUpdate"] = approved;
        }
        c.submit_request = Some(request);
        durable_update(s, env, server_id, operation, false);
        return;
    }
    if host.server_info["desktopManaged"] == true || !card(s, &server_id).force_running {
        return;
    }
    legacy_update(s, env, server_id);
}

/// Pre-durable daemons: `daemon.update.request` with progress events, then
/// the replacement worker's version is confirmed.
fn legacy_update(s: &mut Settings, env: &mut Env, server_id: String) {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
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
            Err(error) => {
                return fail(
                    s,
                    &sid,
                    ut("requestFailedTitle"),
                    uta("requestFailedMessage", &[("error", &error.text())]),
                );
            }
        };
        let sid2 = sid.clone();
        let message = json!({"type": "daemon.update.request", "requestId": request_id});
        s.request_streamed(
            env,
            &sid,
            message,
            "daemon.update.response",
            Some(Duration::from_secs(600)),
            move |s, env, installed| {
                let installed = match installed {
                    Ok(installed)
                        if installed.get("success").and_then(Value::as_bool) == Some(true) =>
                    {
                        installed
                    }
                    Ok(installed) => {
                        let error = crate::features::settings::json::str_or(
                            &installed,
                            "error",
                            &t("settingsNative.packageFailed"),
                        )
                        .to_string();
                        return fail(
                            s,
                            &sid2,
                            ut("requestFailedTitle"),
                            uta("requestFailedMessage", &[("error", &error)]),
                        );
                    }
                    Err(error) => {
                        return fail(
                            s,
                            &sid2,
                            ut("requestFailedTitle"),
                            uta("requestFailedMessage", &[("error", &error.text())]),
                        );
                    }
                };
                let new_version = str_of(&installed, "newVersion").to_string();
                let sid3 = sid2.clone();
                let done: Observer = Rc::new(move |s, env, worker| {
                    let failure = |s: &mut Settings, detail: String| {
                        let error = ta("settingsNative.packageUnconfirmed", &[("error", &detail)]);
                        fail(
                            s,
                            &sid3,
                            ut("requestFailedTitle"),
                            uta("requestFailedMessage", &[("error", &error)]),
                        );
                    };
                    match worker {
                        Err(error) => failure(s, error.text()),
                        Ok(worker) => {
                            let reported = env.host(&sid3).map(|h| h.version()).unwrap_or_default();
                            let version = if reported.is_empty() {
                                str_of(&worker, "version").to_string()
                            } else {
                                reported
                            };
                            if version.is_empty()
                                || new_version.is_empty()
                                || version != new_version
                            {
                                let expected = if new_version.is_empty() {
                                    t("settingsNative.unknownVersion")
                                } else {
                                    new_version.clone()
                                };
                                failure(
                                    s,
                                    ta(
                                        "settingsNative.versionMismatch",
                                        &[("expected", &expected), ("observed", &version)],
                                    ),
                                );
                            } else {
                                let c = card(s, &sid3);
                                c.state = UpdatePhase::Complete;
                                c.worker_version = version;
                                c.legacy_request = None;
                                reset_version_check(s, &sid3);
                            }
                        }
                    }
                });
                observe_replacement(
                    s,
                    env,
                    sid2,
                    previous,
                    Instant::now() + Duration::from_secs(600),
                    done,
                );
            },
        );
    });
}

/// Legacy `daemon.update.progress` events for the running request.
pub fn on_host_event(s: &mut Settings, host_id: &str, event: &Value) {
    if str_of(event, "type") != "daemon.update.progress" {
        return;
    }
    let payload = get(event, "payload");
    let Some(c) = s.update_cards.get_mut(host_id) else {
        return;
    };
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

fn component_name(id: &str) -> String {
    ut(match id {
        "app" => "componentApp",
        "daemon" => "componentDaemon",
        "tailnet" => "componentTailnet",
        "update-helper" => "componentHelper",
        _ => "componentUnknown",
    })
}

fn restart_hint(group: &str) -> String {
    ut(match group {
        "none" => "restartNone",
        "daemon" => "restartDaemon",
        _ => "restartGui",
    })
}

fn component_identity(identity: &Value) -> String {
    let version = str_of(identity, "version");
    if version.is_empty() {
        return t("settingsNative.unknownVersion");
    }
    uta(
        "componentIdentity",
        &[
            ("version", version),
            ("source", str_of(identity, "sourceSha")),
        ],
    )
}

fn draw_component_plan(ui: &mut Ui, host: &HostView, c: &UpdateCard, check: &Value) {
    let available = check["availableComponents"].as_array();
    let editable = host.connected()
        && c.operation_id.is_none()
        && c.state != UpdatePhase::Updating
        && du::can_update_host(&host.server_info, check);
    ui.card_begin();
    for (index, id) in du::COMPONENT_IDS.into_iter().enumerate() {
        if index > 0 {
            ui.card_row_separator();
        }
        let current = &check["components"][id];
        let change = available.and_then(|entries| entries.iter().find(|entry| entry["id"] == id));
        let mut hint = uta(
            "componentCurrent",
            &[("identity", &component_identity(current))],
        );
        if let Some(change) = change {
            hint.push_str(&format!(
                "\n{}",
                uta(
                    "componentAvailable",
                    &[
                        ("identity", &component_identity(&change["target"])),
                        (
                            "bytes",
                            &change["downloadSize"].as_u64().unwrap_or(0).to_string()
                        ),
                    ]
                )
            ));
            if change["current"] == change["target"] {
                hint.push_str(&format!("\n{}", ut("componentCohort")));
            }
        } else {
            hint.push_str(&format!("\n{}", ut("componentRetained")));
        }
        let requires = change
            .map(|entry| &entry["requires"])
            .unwrap_or(&check["componentRequirements"][id]);
        if let Some(requires) = requires.as_object().filter(|requires| !requires.is_empty()) {
            let dependencies = requires
                .iter()
                .map(|(dependency, abi)| format!("{} · ABI {}", component_name(dependency), abi))
                .collect::<Vec<_>>()
                .join(", ");
            hint.push_str(&format!(
                "\n{}",
                uta("componentRequires", &[("components", &dependencies)])
            ));
        }
        let mut component = row(component_name(id)).hint(hint);
        if change.is_some() {
            let server_id = host.server_id.clone();
            let toggle = editable.then(|| {
                handler(move |s, _| {
                    let selected = &mut card(s, &server_id).selected_components;
                    if selected.iter().any(|selected| selected == id) {
                        selected.retain(|selected| selected != id);
                    } else {
                        selected.push(id.into());
                        selected.sort_unstable();
                    }
                })
            });
            component = component.control(switch(
                format!("host.{}.component.{id}", host.server_id),
                c.selected_components.iter().any(|selected| selected == id),
                toggle,
            ));
        }
        ui.row(component);
    }
    for group in check["atomicGroups"].as_array().into_iter().flatten() {
        let names = group
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(component_name)
            .collect::<Vec<_>>()
            .join(", ");
        ui.card_row_separator();
        ui.row(row(ut("atomicGroup")).hint(names));
    }
    let bytes = du::download_bytes(check, &c.selected_components);
    ui.card_row_separator();
    ui.row(row(ut("componentPlan")).hint(uta(
        "componentDownload",
        &[
            ("count", &c.selected_components.len().to_string()),
            ("bytes", &bytes.to_string()),
        ],
    )));
    ui.card_row_separator();
    ui.row(row(ut("restartGroup")).hint(restart_hint(du::restart_group(&c.selected_components))));
    if !du::selection_valid(check, &c.selected_components) {
        ui.card_row_separator();
        inset_alert(
            ui,
            false,
            &ut("selectionInvalidTitle"),
            &ut("selectionInvalid"),
        );
    }
    ui.card_end();
    ui.gap(24.0);
}

#[derive(Clone, Debug, PartialEq)]
struct LocalUpdatePlan {
    daemon: Option<Value>,
    app: Option<Value>,
    force: bool,
}

fn local_update_plan(
    host: &HostView,
    daemon: &UpdateCard,
    app: &about::AboutState,
) -> Option<LocalUpdatePlan> {
    if !host.local
        || daemon.checking
        || daemon.state == UpdatePhase::Updating
        || daemon.operation_id.is_some()
        || host.maintenance_operation.is_some()
        || app.checking
        || app.installing
        || !app.checked
    {
        return None;
    }
    let target = daemon
        .check
        .as_ref()
        .filter(|check| {
            host.connected()
                && du::target_kind(&host.server_info) == "daemon"
                && du::supports_durable(&host.server_info)
                && du::can_update_host(&host.server_info, check)
        })
        .map(|check| check["latest"].clone());
    let app = about::app_approval(&app.update);
    (target.is_some() || app.is_some()).then_some(LocalUpdatePlan {
        force: target.is_some() && daemon.force_running,
        daemon: target,
        app,
    })
}

fn local_version_update(
    host: &HostView,
    c: &UpdateCard,
    app: &about::AboutState,
) -> Option<crate::features::settings::rpc::Handler> {
    let id = host.server_id.clone();
    if c.operation_id.is_some() {
        return (host.connected()
            && c.state != UpdatePhase::Updating
            && !app.installing
            && host
                .maintenance_operation
                .as_ref()
                .is_none_or(|operation| c.operation_id.as_ref() == Some(operation)))
        .then(|| handler(move |s, env| start_update(s, env, id.clone(), None)));
    }
    let plan = local_update_plan(host, c, app)?;
    let label = host.label.clone();
    Some(handler(move |s, _| {
        let mut message = Vec::new();
        if let Some(target) = &plan.daemon {
            message.push(uta(
                "approvedVersion",
                &[("version", str_of(target, "version"))],
            ));
            message.push(ut(if plan.force {
                "confirmInterrupt"
            } else {
                "confirmMessage"
            }));
        }
        if let Some(app) = &plan.app {
            let components = app["selectedComponents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(component_name)
                .collect::<Vec<_>>()
                .join(", ");
            let version = str_of(&app["expectedRelease"], "version");
            message.push(uta("appLocal.approvedVersion", &[("version", version)]));
            message.push(uta("selectedComponents", &[("components", &components)]));
            message.push(ut(if plan.daemon.is_some() {
                "appLocal.confirmAfterDaemon"
            } else {
                "appLocal.confirm"
            }));
        }
        let id = id.clone();
        let approved = plan.clone();
        s.confirm(
            uta("confirmTitle", &[("name", &label)]),
            message.join("\n"),
            ut("confirm"),
            plan.force,
            handler(move |s, env| {
                let Some(current) = env.host(&id) else {
                    return;
                };
                let Some(c) = s.update_cards.get(&id) else {
                    return;
                };
                if local_update_plan(&current, c, &s.about).as_ref() != Some(&approved) {
                    return;
                }
                if approved.daemon.is_some() {
                    let app = approved.app.clone().map(|mut request| {
                        request["operationId"] = json!(du::new_operation_id());
                        request
                    });
                    if app.is_some() {
                        s.about.operation = Value::Null;
                        s.about.install_error = None;
                    }
                    start_update(s, env, id.clone(), app);
                } else if let Some(app) = &approved.app {
                    about::install_approved(s, env, app.clone());
                }
            }),
        );
    }))
}

fn draw_local_app_versions(ui: &mut Ui, app: &about::AboutState, daemon: &UpdateCard) {
    ui.card_row_separator();
    let current = app.update["current"]["version"]
        .as_str()
        .map(String::from)
        .or_else(platform::app_version)
        .unwrap_or_else(|| t("settingsNative.unknownVersion"));
    ui.row(row(ut("appLocal.current")).hint(current));
    ui.card_row_separator();
    let release_error = release_check_error_text(&app.update);
    let check_error = app
        .check_error
        .as_deref()
        .or(release_error.as_deref())
        .filter(|error| !error.is_empty());
    let support_error = app.update["supportError"].as_str();
    let latest = app.update["latest"]["version"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            if app.checking {
                ut("checking")
            } else if check_error.is_some_and(not_published) {
                ut("notPublished")
            } else if check_error.is_some() {
                ut("checkFailed")
            } else if support_error.is_some() {
                ut("unsupportedTitle")
            } else {
                ut("notChecked")
            }
        });
    let waiting_for_daemon = daemon.state == UpdatePhase::Updating
        && daemon
            .submit_request
            .as_ref()
            .is_some_and(|request| request["afterDaemonAppUpdate"].is_object());
    let status = app.update_progress().or_else(|| {
        if waiting_for_daemon {
            Some(ut("appLocal.waitingForDaemon"))
        } else if app.update["supported"] == true
            && app.update["updateAvailable"] == false
            && check_error.is_none()
        {
            Some(ut("appLocal.upToDate"))
        } else {
            None
        }
    });
    let mut detail = latest;
    if let Some(status) = status {
        detail.push('\n');
        detail.push_str(&status);
    }
    let error = app
        .install_error
        .as_deref()
        .or_else(|| check_error.filter(|error| !not_published(error)))
        .or_else(|| app.operation["error"].as_str())
        .or(support_error)
        .filter(|error| !error.is_empty())
        .map(String::from);
    ui.row(row(ut("appLocal.latest")).hint(detail).error(error));
}

/// The local host also shows the app release checked by this desktop.
pub fn draw_version_page(ui: &mut Ui, s: &Settings, host: &HostView) {
    let empty = UpdateCard::default();
    let c = s.update_cards.get(&host.server_id).unwrap_or(&empty);
    let local_app = has_local_app(host);
    let checked = c
        .check
        .as_ref()
        .filter(|check| !local_app || check["targetKind"] != "app");
    let updating = c.state == UpdatePhase::Updating;
    let pending = c.operation_id.is_some();
    let durable = du::supports_durable(&host.server_info);
    let check_supported = du::supports_check(&host.server_info);
    let desktop_managed = host.server_info["desktopManaged"] == true;
    let app_update = !local_app && (desktop_managed || c.app_update);
    let component_update = !local_app && desktop_managed && du::supports_app(&host.server_info);
    let restart = du::restart_group(&c.selected_components);
    let selection_ready = !component_update
        || (!c.selected_components.is_empty()
            && checked.is_some_and(|check| du::selection_valid(check, &c.selected_components)));
    let supported = if local_app && desktop_managed {
        false
    } else if check_supported {
        checked.is_some_and(|v| du::can_update_host(&host.server_info, v))
    } else {
        !desktop_managed && (durable || host.feature("daemonSelfUpdate"))
    };
    let id = host.server_id.clone();
    let checking = c.checking || (local_app && s.about.checking);
    let installing = local_app && s.about.installing;
    let refresh =
        ((host.connected() || local_app) && !checking && !updating && !installing).then(|| {
            handler(move |s, env| {
                if local_app {
                    about::check(s, env, true);
                }
                check_version(s, env, id.clone());
            })
        });
    ui.section_title(&t("settings.hostSections.version"), None, None);
    ui.card_begin();
    let current = if app_update && !checked.is_some_and(|v| v["targetKind"] == "app") {
        String::new()
    } else {
        checked
            .and_then(|v| v["current"]["version"].as_str())
            .filter(|v| !v.is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                if app_update {
                    String::new()
                } else {
                    host.version()
                }
            })
    };
    ui.row(
        row(ut(if app_update {
            "currentAppVersion"
        } else {
            "currentVersion"
        }))
        .hint(if current.is_empty() {
            t("settingsNative.unknownVersion")
        } else {
            current
        }),
    );
    ui.card_row_separator();
    let raw_check_error = if !c.check_error.is_empty() {
        c.check_error.clone()
    } else {
        checked
            .map(|v| str_of(v, "checkError"))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    // No published inventory for this platform is an outcome, not a failure.
    let unpublished = not_published(&raw_check_error);
    let check_error = if unpublished {
        String::new()
    } else if !c.check_error.is_empty() {
        c.check_error.clone()
    } else {
        checked
            .and_then(release_check_error_text)
            .unwrap_or_default()
    };
    let latest = checked
        .and_then(|v| v["latest"]["version"].as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            if c.checking {
                ut("checking")
            } else if unpublished {
                with_detail(&ut("notPublished"), &raw_check_error)
            } else if !check_error.is_empty() {
                ut("checkFailed")
            } else if desktop_managed && !du::supports_app(&host.server_info) {
                ut("appUpgradePrerequisite")
            } else if !check_supported {
                ut("legacyCheckUnavailable")
            } else {
                ut("notChecked")
            }
        });
    ui.row(
        row(ut(if app_update {
            "latestAppVersion"
        } else {
            "latestVersion"
        }))
        .hint(latest)
        .control(
            button(
                format!("host.{}.checkVersion", host.server_id),
                ut("check"),
                refresh,
            )
            .loading(checking)
            .ctl(),
        ),
    );
    if !host.connected() {
        ui.card_row_separator();
        inset_warning(ui, &ut("offlineTitle"), &ut("offlineMessage"));
    }
    // Offline already has its own notice; a stale transport failure would repeat it.
    if host.connected() && !check_error.is_empty() {
        ui.card_row_separator();
        inset_alert(ui, false, &ut("checkFailed"), &check_error);
    }
    let support_error =
        support_error_text(checked.map(|v| str_of(v, "supportError")).unwrap_or(""));
    if local_app && desktop_managed {
        ui.card_row_separator();
        inset_warning(ui, &ut("unsupportedTitle"), &ut("appLocal.legacyDaemon"));
    } else if desktop_managed && !du::supports_app(&host.server_info) {
        ui.card_row_separator();
        inset_warning(ui, &ut("unsupportedTitle"), &ut("appUpgradePrerequisite"));
    } else if !support_error.is_empty() {
        ui.card_row_separator();
        inset_warning(ui, &ut("unsupportedTitle"), &support_error);
    } else if desktop_managed && checked.is_some_and(|v| v["owner"]["kind"] != "desktopApp") {
        ui.card_row_separator();
        inset_warning(ui, &ut("unsupportedTitle"), &ut("appOwnerUnavailable"));
    } else if !check_supported && !supported {
        ui.card_row_separator();
        inset_warning(ui, &ut("unsupportedTitle"), &ut("unsupportedLegacy"));
    } else if checked.is_some_and(|v| v["latest"].is_object() && v["updateAvailable"] == false) {
        ui.card_row_separator();
        ui.row(row(ut(if app_update {
            "appUpToDate"
        } else {
            "upToDate"
        })));
    }
    if local_app {
        draw_local_app_versions(ui, &s.about, c);
    }
    ui.card_end();
    ui.gap(24.0);
    if component_update && let Some(check) = checked.filter(|check| check["components"].is_object())
    {
        draw_component_plan(ui, host, c, check);
    }
    if let Some(operation) = &host.maintenance_operation {
        ui.card_begin();
        ui.row(
            row(t("settingsNative.maintenance"))
                .hint(ut("stillRunningHint"))
                .control(operation_id_ctl(
                    format!("host.{}.maintenanceId", host.server_id),
                    operation,
                )),
        );
        ui.card_end();
        ui.gap(12.0);
    }
    let busy_elsewhere = host
        .maintenance_operation
        .as_ref()
        .is_some_and(|id| c.operation_id.as_ref() != Some(id));
    let id = host.server_id.clone();
    let toggle = (!updating && !pending && supported && (!component_update || restart != "none"))
        .then(|| {
            handler(move |s, _| {
                let c = card(s, &id);
                c.force_running = !c.force_running;
            })
        });
    ui.card_begin();
    ui.row(
        row(ut("interruptRunning"))
            .hint(if component_update {
                restart_hint(restart)
            } else {
                ut(if durable {
                    "waitPolicy"
                } else {
                    "legacyPolicy"
                })
            })
            .control(switch(
                format!("host.{}.forceUpdate", host.server_id),
                c.force_running && (!component_update || restart != "none"),
                toggle,
            )),
    );
    ui.card_row_separator();
    let force = c.force_running;
    let remote_act = (!local_app
        && host.connected()
        && !updating
        && !busy_elsewhere
        && (pending || (supported && selection_ready && (durable || force))))
        .then(|| {
            let id = host.server_id.clone();
            let label = host.label.clone();
            let target = checked.and_then(|v| v.get("latest")).cloned();
            let approved_kind = du::target_kind(&host.server_info);
            let selected = c.selected_components.clone();
            handler(move |s, env| {
                if pending {
                    start_update(s, env, id.clone(), None);
                    return;
                }
                let id = id.clone();
                let approved_target = target.clone();
                let approved_components = selected.clone();
                let go = handler(move |s, env| {
                    if env
                        .host(&id)
                        .is_none_or(|h| du::target_kind(&h.server_info) != approved_kind)
                    {
                        return;
                    }
                    // Approval belongs to exactly the version/policy shown, even if
                    // a check or connection event occurred while the sheet was open.
                    let c = card(s, &id);
                    if c.check.as_ref().and_then(|v| v.get("latest")).cloned() != approved_target
                        || c.force_running != force
                        || c.selected_components != approved_components
                    {
                        return;
                    }
                    start_update(s, env, id.clone(), None);
                });
                let mut message = if component_update {
                    format!(
                        "{}\n{}\n{}",
                        restart_hint(restart),
                        uta(
                            "selectedComponents",
                            &[(
                                "components",
                                &selected
                                    .iter()
                                    .map(|id| component_name(id))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )]
                        ),
                        ut(if force && restart != "none" {
                            "componentConfirmInterrupt"
                        } else {
                            "componentConfirm"
                        })
                    )
                } else {
                    ut(if force {
                        "confirmInterrupt"
                    } else {
                        "confirmMessage"
                    })
                };
                if let Some(version) = target.as_ref().and_then(|v| v["version"].as_str()) {
                    message = format!(
                        "{}\n{}",
                        uta(
                            if app_update {
                                "approvedAppVersion"
                            } else {
                                "approvedVersion"
                            },
                            &[("version", version)]
                        ),
                        message
                    );
                }
                s.confirm(
                    uta("confirmTitle", &[("name", &label)]),
                    message,
                    ut("confirm"),
                    force,
                    go,
                );
            })
        });
    let act = if local_app {
        local_version_update(host, c, &s.about)
    } else {
        remote_act
    };
    // The phase reads in the row hint; the button keeps its short label so a
    // narrow row does not squeeze the control.
    let button_label = if pending && !updating {
        ut("resumeObservation")
    } else {
        ut("confirm")
    };
    let hint = if updating && !c.phase.is_empty() {
        c.phase.clone()
    } else {
        ut(if local_app {
            "appLocal.hint"
        } else if app_update {
            "appHint"
        } else {
            "hint"
        })
    };
    ui.row(
        row(ut(if local_app {
            "appLocal.title"
        } else if app_update {
            "appTitle"
        } else {
            "title"
        }))
        .hint(hint)
        .control(
            button(format!("host.{}.update", host.server_id), button_label, act)
                .icon(Icon::ArrowUpToLine)
                .size(BSize::Sm)
                .loading(updating || installing)
                .ctl(),
        ),
    );
    if let Some(operation) = &c.operation_id {
        ui.card_row_separator();
        ui.row(row(ut("operation")).control(operation_id_ctl(
            format!("host.{}.operationId", host.server_id),
            operation,
        )));
    }
    if c.action_required {
        ui.card_row_separator();
        inset_alert(
            ui,
            false,
            &ut("phaseWaitingForAuthorization"),
            &ut("authorizationHint"),
        );
    }
    match c.state {
        UpdatePhase::Complete => {
            ui.card_row_separator();
            inset_alert(
                ui,
                true,
                &uta(
                    if c.app_update {
                        "appCommittedTitle"
                    } else {
                        "committedTitle"
                    },
                    &[("version", &c.worker_version)],
                ),
                &if c.durable {
                    c.complete_note.clone()
                } else {
                    ut("committedMessage")
                },
            );
        }
        UpdatePhase::Failed => {
            ui.card_row_separator();
            inset_alert(ui, false, &c.failure_title, &c.failure_message);
        }
        _ => {}
    }
    ui.card_end();
    ui.gap(24.0);
}

/// [`inset_alert`] for an unavailable (not failed) state: the warning style.
fn inset_warning(ui: &mut Ui, title: &str, description: &str) {
    let (x, w) = (ui.x, ui.w);
    ui.x = x + 16.0;
    ui.w = w - 32.0;
    ui.warning(title, Some(description));
    ui.gap(16.0);
    ui.x = x;
    ui.w = w;
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

/// Copy control for an internal durable operation ID: the ID is reachable only
/// through its tooltip and the clipboard, never as visible row text.
fn operation_id_ctl(key: String, operation: &str) -> Ctl {
    let id = operation.to_string();
    let copy = handler(move |_, env| {
        env.copy(
            &id,
            Some(ta(
                "common.states.copiedLabel",
                &[("label", &ut("operation"))],
            )),
        )
    });
    icon_button(key, Icon::Copy, Some(copy)).tipped(uta("copyOperationId", &[("id", operation)]))
}

/// Active-connection label for the sidebar host picker.
pub fn picker_hint(host: &HostView) -> String {
    active_connection_label(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn local_version_update_and_removal_use_separate_authorities() {
        use makepad_widgets::Cx;
        use paseo_gui_core::{CoreConfig, CoreState, GuiCore};
        use std::{fs, sync::Arc};

        let dir =
            std::env::temp_dir().join(format!("paseo-local-version-{}", du::new_operation_id()));
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
        ctx.state = Arc::new(CoreState::new(json!({"hosts": [{
            "serverId": "local", "status": "connected",
            "config": {
                "id": "localhost",
                "endpoint": "127.0.0.1:9",
                "localCredentialPath": dir.join("daemon/local-credential"),
            },
            "serverInfo": {"serverId": "local", "features": {
                "durableDaemonUpdates": true, "daemonUpdateCheck": true,
            }},
        }]})));
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
        let host = env.host("local").unwrap();
        let daemon_release = json!({"version": "2.0.0", "manifestSha512": "A".repeat(86) + "=="});
        let app_release =
            json!({"version": "3.0.0", "manifestSha512": format!("B{}==", "A".repeat(85))});
        card(&mut s, "local").check = Some(json!({
            "supported": true, "targetKind": "daemon", "updateAvailable": true,
            "latest": daemon_release,
        }));
        s.about.checked = true;
        s.about.update = json!({
            "supported": true, "updateAvailable": true, "latest": app_release,
            "availableComponents": [{"id": "app"}, {"id": "tailnet"}, {"id": "update-helper"}],
        });
        local_version_update(&host, &s.update_cards["local"], &s.about).unwrap()(&mut s, &mut env);
        let SheetKind::Confirm(approval) = s.sheets.pop().unwrap().kind else {
            panic!("update must request confirmation");
        };
        (approval.on_confirm)(&mut s, &mut env);
        let request = s.update_cards["local"].submit_request.clone().unwrap();
        assert_eq!(request["targetKind"], "daemon");
        assert_eq!(request["expectedRelease"], daemon_release);
        assert!(request.get("selectedComponents").is_none());
        let app = &request["afterDaemonAppUpdate"];
        assert_eq!(app["expectedRelease"], app_release);
        assert_eq!(
            app["selectedComponents"],
            json!(["app", "tailnet", "update-helper"]),
        );
        assert!(!s.about.installing);
        assert!(env.bridge.desktop_requests.as_ref().unwrap().is_empty());

        let operation_id = request["operationId"].as_str().unwrap();
        finish(
            &mut s,
            &mut env,
            "local",
            operation_id,
            json!({
                "operationId": operation_id, "serverId": "local", "targetKind": "daemon",
                "phase": "committed", "terminal": true,
                "candidate": {"version": "2.0.0", "executableSha256": "a".repeat(64)},
                "worker": {"serverId": "local", "version": "2.0.0", "executableSha256": "a".repeat(64)},
            }),
        );
        assert_eq!(s.update_cards["local"].state, UpdatePhase::Complete);
        assert!(
            s.about.installing,
            "only a verified daemon commit advances the app update",
        );

        remove_host_handler(&host)(&mut s, &mut env);
        let SheetKind::Confirm(removal) = s.sheets.pop().unwrap().kind else {
            panic!("removal must request confirmation");
        };
        (removal.on_confirm)(&mut s, &mut env);
        assert!(env.ctx.effects.iter().any(|effect| {
            matches!(effect, UiEffect::RemoveHost { host_id } if host_id == "local")
        }));
        assert!(env.bridge.desktop_requests.as_ref().unwrap().is_empty());

        drop(core);
        let prefs = fs::read(dir.join("shared_preferences.json")).unwrap();
        fs::remove_dir_all(dir).unwrap();
        let prefs: Value = serde_json::from_slice(&prefs).unwrap();
        assert_eq!(
            prefs["flutter.paseo.desktopAppUpdate.operation"],
            app["operationId"],
        );
    }

    #[test]
    fn restored_standalone_terminal_status_settles_card_and_clears_saved_approval() {
        use makepad_widgets::Cx;
        use paseo_gui_core::{CoreConfig, GuiCore};
        use std::{fs, sync::Arc};

        let dir =
            std::env::temp_dir().join(format!("paseo-update-result-{}", du::new_operation_id()));
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
        let mut bridge = Default::default();
        let mut env = Env {
            cx: &mut cx,
            ctx: &mut ctx,
            fields: &mut fields,
            bridge: &mut bridge,
        };
        let mut settings = Settings::default();
        let cases = [
            ("committed", UpdatePhase::Complete, "committedMessage"),
            ("rolled_back", UpdatePhase::Failed, "rolledBackTitle"),
            ("failed", UpdatePhase::Failed, "requestFailedTitle"),
        ];
        for (phase, expected, message) in cases.iter().cloned() {
            let id = du::new_operation_id();
            let key = host::update_operation_key(phase);
            let approval = json!({"operationId":id,"source":"release","targetKind":"daemon",
                "expectedRelease":{"version":"0.13.36","manifestSha512":"A".repeat(88)}});
            env.ctx
                .core
                .save_state(&format!("prefs:{key}"), approval.clone());
            Arc::make_mut(&mut Arc::make_mut(&mut env.ctx.state).prefs)
                .insert(key, json!(approval.to_string()));
            let c = card(&mut settings, phase);
            c.operation_id = host::stored_update_operation(&env.ctx.state, phase);
            c.submit_request = host::stored_update_request(&env.ctx.state, phase);
            assert_eq!(c.operation_id.as_deref(), Some(id.as_str()));
            assert_eq!(c.submit_request, Some(approval));
            c.durable = true;
            c.state = UpdatePhase::Updating;

            // No live worker reply is needed: the cutover evidence remains
            // true even if the operation is read after another update.
            let response = json!({"operation":{"operationId":id,"serverId":phase,"terminal":true,
                "phase":phase,"candidate":{"version":"0.13.36","executableSha256":"a".repeat(64)},
                "worker":{"serverId":phase,"version":"0.13.36","executableSha256":"a".repeat(64)},
                "continuation":null}});
            finish(
                &mut settings,
                &mut env,
                phase,
                &id,
                du::operation_of(&response, &id, phase).unwrap(),
            );
            let c = card(&mut settings, phase);
            assert_eq!(c.state, expected, "{phase}");
            assert!(c.operation_id.is_none());
            assert!(c.submit_request.is_none());
            if phase == "committed" {
                assert_eq!(c.worker_version, "0.13.36");
                assert_eq!(c.complete_note, ut(message));
            } else {
                assert_eq!(c.failure_title, ut(message));
            }
        }
        drop(core); // Flushes preferences after the queued clear commands.
        let persisted = fs::read(dir.join("shared_preferences.json"));
        fs::remove_dir_all(dir).unwrap();
        let persisted: Value = serde_json::from_slice(&persisted.unwrap()).unwrap();
        for (phase, _, _) in cases {
            let key = format!("flutter.{}", host::update_operation_key(phase));
            assert!(persisted.get(&key).is_none(), "{key}");
        }
    }

    #[test]
    fn github_rate_limit_has_local_retry_time() {
        let retry_at = "2026-11-18T16:37:00Z";
        let local_time = chrono::DateTime::parse_from_rfc3339(retry_at)
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        for code_key in ["checkErrorCode", "errorCode"] {
            let mut check = json!({"retryAt":retry_at,"checkError":"raw rate limit"});
            check[code_key] = json!("github_rate_limited");
            assert_eq!(
                release_check_error_text(&check).unwrap(),
                ta("settingsNative.githubRateLimited", &[("time", &local_time)])
            );
            check["retryAt"] = json!("invalid");
            assert_eq!(
                release_check_error_text(&check).as_deref(),
                Some("raw rate limit")
            );
        }
    }

    #[test]
    fn update_failures_are_localized_with_raw_text_as_detail() {
        let mapped = update_error_text("DAEMON_REQUEST_TIMEOUT", "request timed out after 30s");
        assert!(mapped.starts_with(&ut("timedOutMessage")));
        assert!(mapped.contains("request timed out after 30s"));
        assert!(
            update_error_text("DAEMON_NOT_CONNECTED", "").starts_with(&ut("unavailableMessage"))
        );
        assert_eq!(update_error_text("", " disk full "), "disk full");

        assert!(not_published(
            "No stable component inventory is published for linux/x64 in emotionbug/Passession"
        ));
        assert!(not_published(
            "No authenticated stable component inventory is published"
        ));
        assert!(!not_published("GitHub API rate limited"));

        let raw = "systemctl --user show paseo.service --property=MainPID failed \
                   (exit status: 1): Failed to connect to user scope bus via local \
                   transport: No such file or directory";
        let support = support_error_text(raw);
        assert!(support.starts_with(&ut("serviceUnmanaged")));
        assert!(support.contains("Failed to connect to user scope bus"));
        assert!(!support.contains("systemctl"));
    }
}
