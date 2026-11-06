//! All settings UI state plus the request/timer/bridge continuation tables.

use std::collections::HashMap;

use makepad_widgets::*;
use serde_json::Value;

use super::host::Status;
use super::model::forms::FormSheet;
use super::pages::{about, daemon, host as host_page, plugins, profiles, providers, shortcuts, system, usage};
use super::rpc::{Env, Handler, Later, ReqError, Then, check_response, message};
use super::ui::overlay::{Confirm, Menu};

pub enum SheetKind {
    Form(FormSheet),
    Report { title: String, text: String },
    Confirm(Confirm),
    SystemPrompt(daemon::SystemPromptSheet),
    TerminalProfile(daemon::TerminalProfileSheet),
    Skills(daemon::SkillsSheet),
    Provider(providers::ProviderSheet),
    AddModel(providers::AddModelSheet),
    Diagnostic(providers::DiagnosticSheet),
    Profile(Box<profiles::ProfileSheet>),
    UsageOptions { host: String },
    PluginLogs(plugins::LogsSheet),
}

pub struct Sheet {
    pub id: u64,
    pub max_w: f64,
    pub scroll: f64,
    pub kind: SheetKind,
}

/// A shown info tooltip (Flutter section `Tooltip`).
pub struct Tip {
    pub text: String,
    /// The glyph's hit rect; the tooltip sits 24px below its centre.
    pub anchor: Rect,
    /// Shown by a tap (hidden after 1.5 s) rather than by hover.
    pub tapped: bool,
}

#[derive(Default)]
pub struct Settings {
    /// `general` … `about`, `local-daemon` or `host/<section>`; `None` is
    /// the compact root list (wide layouts show General).
    pub section: Option<String>,
    pub host: Option<String>,
    pub project_root: Option<String>,
    pub route_seen: Option<(Option<String>, String)>,
    pub scroll: f64,
    pub sidebar_scroll: f64,
    pub content_h: f64,
    pub sidebar_h: f64,
    /// Rect of the last tapped zone (menus anchor here).
    pub anchor: Rect,
    pub sheets: Vec<Sheet>,
    pub menu: Option<Menu>,
    pub tip: Option<Tip>,
    /// Host whose agent profiles the last closed profile editor saved
    /// (reported by an embedded panel's `EditorClosed`).
    pub saved_profile_host: Option<String>,
    pub prefs_pending: HashMap<String, Option<Value>>,
    pub last_status: HashMap<String, Status>,
    next_id: u64,
    pending: HashMap<String, Then>,
    bridge_pending: HashMap<u64, Then>,
    timers: Vec<(Timer, Later)>,

    pub daemon: HashMap<String, daemon::DaemonData>,
    pub skills: HashMap<String, daemon::SkillsState>,
    pub hub: HashMap<String, daemon::HubState>,
    pub connected_hosts: HashMap<String, daemon::ConnectedHostsState>,
    pub providers_ui: HashMap<String, providers::ProvidersUi>,
    pub metadata: HashMap<String, profiles::MetadataState>,
    pub usage: HashMap<String, usage::UsageState>,
    pub plugins: HashMap<String, plugins::PluginsState>,
    pub update_cards: HashMap<String, host_page::UpdateCard>,
    pub restarting: HashMap<String, bool>,
    pub host_busy: HashMap<String, bool>,
    pub local_daemon: system::LocalDaemonState,
    pub notifications: system::NotificationsState,
    pub diagnostics: system::DiagnosticsState,
    pub about: about::AboutState,
    pub shortcuts: shortcuts::ShortcutsState,
}

impl Settings {
    pub fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // ------------------------------------------------------------ requests

    /// Sends `{type, ...fields}` to `host` and runs `then` with the checked
    /// response (Flutter `settingsRequest`).
    pub fn request(
        &mut self,
        env: &mut Env,
        host: &str,
        kind: &str,
        fields: Value,
        response: &str,
        then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
    ) {
        let id = env.ctx.core.request(host, message(kind, fields), response);
        self.pending.insert(id, Box::new(then));
    }

    /// [`request`] with an explicit deadline (`None` = no deadline). Flutter
    /// used 30 s for durable update status/submit and 10 min for the legacy
    /// `daemon.update.request`.
    #[allow(clippy::too_many_arguments)]
    pub fn request_timeout(
        &mut self,
        env: &mut Env,
        host: &str,
        kind: &str,
        fields: Value,
        response: &str,
        timeout: Option<std::time::Duration>,
        then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
    ) {
        let id = env.ctx.core.request_with_timeout(host, message(kind, fields), response, timeout);
        self.pending.insert(id, Box::new(then));
    }

    /// `save_state_flushed`: `then` runs once `value` is durable (or failed to be).
    pub fn save_flushed(&mut self, env: &mut Env, key: &str, value: Value, then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static) {
        let id = env.ctx.core.save_state_flushed(key, value);
        self.pending.insert(format!("raw:{id}"), Box::new(then));
    }

    /// A plain request whose correlated updates (`usage.list_reports.update`,
    /// `daemon.update.progress`) arrive as `HostEvent`s carrying the message's
    /// own `requestId`, in wire order before the final response (core-api,
    /// CoreTransport follow-up #1). `message_value` must carry `type` and
    /// `requestId`; the response payload is returned unchecked.
    pub fn request_streamed(
        &mut self,
        env: &mut Env,
        host: &str,
        message_value: Value,
        response: &str,
        timeout: Option<std::time::Duration>,
        then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
    ) {
        let id = env.ctx.core.request_with_timeout(host, message_value, response, timeout);
        self.pending.insert(format!("raw:{id}"), Box::new(then));
    }

    pub fn owns_request(&self, id: &str) -> bool {
        self.pending.contains_key(id) || self.pending.contains_key(&format!("raw:{id}"))
    }

    pub fn on_response(&mut self, env: &mut Env, id: &str, result: Result<Value, ReqError>) {
        if let Some(then) = self.pending.remove(id) {
            then(self, env, result.and_then(check_response));
        } else if let Some(then) = self.pending.remove(&format!("raw:{id}")) {
            then(self, env, result);
        }
    }

    pub fn desktop(
        &mut self,
        env: &mut Env,
        command: &str,
        args: Value,
        then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static,
    ) {
        let token = env.bridge.desktop(command, args);
        self.bridge_pending.insert(token, Box::new(then));
    }

    pub fn bridge_job(&mut self, token: u64, then: impl FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>) + 'static) {
        self.bridge_pending.insert(token, Box::new(then));
    }

    pub fn on_bridge(&mut self, env: &mut Env, token: u64, result: Result<Value, String>) {
        if let Some(then) = self.bridge_pending.remove(&token) {
            then(self, env, result.map_err(ReqError::msg));
        }
    }

    pub fn after(&mut self, env: &mut Env, seconds: f64, later: impl FnOnce(&mut Settings, &mut Env) + 'static) {
        let timer = env.cx.start_timeout(seconds);
        self.timers.push((timer, Box::new(later)));
    }

    /// Runs due timers; returns whether any fired.
    pub fn on_timer(&mut self, env: &mut Env, event: &Event) -> bool {
        let mut due = Vec::new();
        let mut index = 0;
        while index < self.timers.len() {
            if self.timers[index].0.is_event(event).is_some() {
                due.push(self.timers.remove(index).1);
            } else {
                index += 1;
            }
        }
        let fired = !due.is_empty();
        for later in due {
            later(self, env);
        }
        fired
    }

    // ------------------------------------------------------------ preferences

    /// The effective preferences: the state's object with this panel's
    /// not-yet-reflected writes applied (`None` = removed).
    pub fn prefs(&self, state_prefs: &Value) -> Value {
        let mut map = state_prefs.as_object().cloned().unwrap_or_default();
        for (key, value) in &self.prefs_pending {
            match value {
                Some(value) => {
                    map.insert(key.clone(), value.clone());
                }
                None => {
                    map.remove(key);
                }
            }
        }
        Value::Object(map)
    }

    /// Drops pending writes the core state now reflects.
    pub fn settle_prefs(&mut self, state_prefs: &Value) {
        self.prefs_pending.retain(|key, value| state_prefs.get(key) != value.as_ref());
    }

    /// Writes one preference key (`None` removes it). The shell merges it into
    /// the whole preferences object, keeping every other key (ui-api §12).
    pub fn save_pref(&mut self, env: &mut Env, key: &str, value: Option<Value>) {
        env.ctx.effects.push(crate::common::UiEffect::SetPreference { key: key.to_string(), value: value.clone().unwrap_or(Value::Null) });
        self.prefs_pending.insert(key.to_string(), value);
    }

    // ------------------------------------------------------------ sheets

    pub fn open_sheet(&mut self, kind: SheetKind, max_w: f64) -> u64 {
        let id = self.next_id();
        self.sheets.push(Sheet { id, max_w, scroll: 0.0, kind });
        id
    }

    /// Opens a sheet whose state already carries its id.
    pub fn push_sheet(&mut self, id: u64, kind: SheetKind, max_w: f64) {
        self.sheets.push(Sheet { id, max_w, scroll: 0.0, kind });
    }

    pub fn close_sheet(&mut self, env: &mut Env, id: u64) {
        let Some(index) = self.sheets.iter().position(|s| s.id == id) else { return };
        let sheet = self.sheets.remove(index);
        env.fields.forget_prefix(env.cx, &format!("sheet.{id}."));
        if let SheetKind::Confirm(Confirm { on_decline: Some(decline), .. }) = sheet.kind {
            decline(self, env);
        }
    }

    pub fn close_top_sheet(&mut self, env: &mut Env) {
        if let Some(id) = self.sheets.last().map(|s| s.id) {
            self.close_sheet(env, id);
        }
    }

    pub fn sheet_mut(&mut self, id: u64) -> Option<&mut SheetKind> {
        self.sheets.iter_mut().find(|s| s.id == id).map(|s| &mut s.kind)
    }

    pub fn report(&mut self, title: String, value: &Value) {
        self.open_sheet(SheetKind::Report { title, text: super::json::pretty(value) }, 760.0);
    }

    /// Flutter `confirmAction`.
    pub fn confirm(&mut self, title: String, message: String, confirm_label: Option<String>, destructive: bool, on_confirm: Handler) {
        self.open_sheet(SheetKind::Confirm(Confirm { title, message, confirm_label, destructive, on_confirm, on_decline: None }), 420.0);
    }

    /// [`Settings::confirm`] that also runs `on_decline` when the dialog is
    /// dismissed without confirming (original `if (!confirmed) return …`).
    pub fn confirm_or(&mut self, title: String, message: String, confirm_label: Option<String>, destructive: bool, on_confirm: Handler, on_decline: Handler) {
        self.open_sheet(SheetKind::Confirm(Confirm { title, message, confirm_label, destructive, on_confirm, on_decline: Some(on_decline) }), 420.0);
    }

    /// Finishes a form sheet: closes it, or shows the error under its fields.
    pub fn form_done(&mut self, env: &mut Env, id: u64, result: Result<(), ReqError>) {
        match result {
            Ok(()) => self.close_sheet(env, id),
            Err(error) => {
                if let Some(SheetKind::Form(form)) = self.sheet_mut(id) {
                    form.busy = false;
                    form.error = Some(error.text());
                }
            }
        }
    }

    pub fn open_menu(&mut self, menu: Menu) {
        self.menu = Some(menu);
    }
}
