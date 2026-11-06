//! Settings pages. Each module draws its section with `ui::kit`, owns its
//! request flows, and contributes sheet kinds.

pub mod about;
pub mod app;
pub mod appearance;
pub mod daemon;
pub mod host;
pub mod plugins;
pub mod profiles;
pub mod providers;
pub mod shortcuts;
pub mod system;
pub mod usage;


use serde_json::Value;

use super::host::HostView;
use super::rpc::Env;
use super::state::{Settings, Sheet, SheetKind};
use super::ui::kit::Ui;
use super::ui::overlay::{self, SheetHead};

/// Read-only data every page draws from.
pub struct View {
    pub hosts: Vec<HostView>,
    pub prefs: Value,
    pub compact: bool,
    pub desktop: bool,
}

impl View {
    pub fn host(&self, id: &str) -> Option<&HostView> {
        self.hosts.iter().find(|h| h.server_id == id)
    }

    /// Flutter `activeHost`: the chosen host, else the first saved one.
    pub fn active_host(&self, s: &Settings) -> Option<&HostView> {
        s.host.as_deref().and_then(|id| self.host(id)).or(self.hosts.first())
    }
}

/// The section body for `section` (`host/<x>` sections need a host).
pub fn draw_section(ui: &mut Ui, s: &Settings, v: &View, section: &str) {
    if let Some(host_section) = section.strip_prefix("host/") {
        let Some(host) = v.active_host(s) else {
            host::draw_no_host(ui);
            return;
        };
        match host_section {
            "host" => host::draw_overview_page(ui, s, v, host),
            "projects" => host::draw_projects(ui, s, host),
            "connections" => host::draw_connections_page(ui, s, v, host),
            "pair-device" => host::draw_pair_device(ui, host),
            "agents" => daemon::draw_agents(ui, s, v, host),
            "metadata" => profiles::draw_metadata_page(ui, s, host),
            "workspaces" => daemon::draw_workspaces(ui, s, host),
            "providers" => providers::draw_providers(ui, s, v, host),
            "usage" => usage::draw_usage(ui, s, v, host),
            "terminals" => daemon::draw_terminals(ui, s, host),
            "plugins" => plugins::draw_plugins(ui, s, host),
            _ => {}
        }
        return;
    }
    match section {
        "local-daemon" => system::draw_local_daemon(ui, s),
        "notifications" => system::draw_notifications(ui, s),
        "diagnostics" => system::draw_diagnostics(ui, s),
        "about" => about::draw_about(ui, s, v),
        "appearance" => appearance::draw_appearance(ui, s, v),
        "shortcuts" => shortcuts::draw_shortcuts(ui, s, v),
        other => app::draw_app_section(ui, s, v, other),
    }
}

/// Starts the loads the visible section needs (once per connection).
pub fn ensure(s: &mut Settings, env: &mut Env, v: &View) {
    let section = s.section.clone().unwrap_or_else(|| if v.compact { String::new() } else { "general".into() });
    if let Some(host_section) = section.strip_prefix("host/") {
        let Some(host) = v.active_host(s).cloned() else { return };
        match host_section {
            "host" => {
                daemon::ensure_daemon(s, env, &host);
                host::ensure_update_card(s, env, &host);
                if host.local && v.desktop {
                    system::ensure_local_daemon(s, env);
                }
            }
            "connections" => {
                daemon::ensure_daemon(s, env, &host);
                daemon::ensure_connected_hosts(s, env, &host);
                daemon::ensure_hub(s, env, &host);
            }
            "agents" => {
                daemon::ensure_daemon(s, env, &host);
                daemon::ensure_skills(s, env, &host);
            }
            "metadata" => {
                daemon::ensure_daemon(s, env, &host);
                profiles::ensure_metadata(s, env, &host);
            }
            "workspaces" | "terminals" | "providers" => daemon::ensure_daemon(s, env, &host),
            "usage" => usage::ensure_usage(s, env, &host),
            "plugins" => {
                daemon::ensure_daemon(s, env, &host);
                plugins::ensure_plugins(s, env, &host);
            }
            _ => {}
        }
        return;
    }
    match section.as_str() {
        "local-daemon" => system::ensure_local_daemon(s, env),
        "notifications" => system::ensure_notifications(s, env),
        "about" => about::ensure_about(s, env, v),
        _ => {}
    }
}

/// A host event (`CoreEvent::HostEvent`) for settings.
pub fn on_host_event(s: &mut Settings, env: &mut Env, host_id: &str, event: &Value) {
    daemon::on_host_event(s, env, host_id, event);
    usage::on_host_event(s, host_id, event);
    plugins::on_host_event(s, env, host_id, event);
    host::on_host_event(s, host_id, event);
}

/// Connection status changed: flows bound to the previous connection reload.
pub fn on_status_change(s: &mut Settings, host_id: &str) {
    daemon::reset(s, host_id);
    usage::on_status_change(s, host_id);
    plugins::reset(s, host_id);
    profiles::reset(s, host_id);
}

pub fn sheet_head(s: &Settings, v: &View, sheet: &Sheet) -> SheetHead {
    match &sheet.kind {
        SheetKind::Form(form) => overlay::form_head(form),
        SheetKind::Report { title, text } => overlay::report_head(sheet.id, title, text),
        // Drawn as an alert dialog by `overlay::draw_alert`.
        SheetKind::Confirm(confirm) => SheetHead::new(confirm.title.clone()),
        SheetKind::SystemPrompt(p) => daemon::system_prompt_head(p),
        SheetKind::TerminalProfile(p) => daemon::terminal_profile_head(sheet.id, p),
        SheetKind::Skills(p) => daemon::skills_head(sheet.id, p),
        SheetKind::Provider(p) => providers::provider_sheet_head(s, v, sheet.id, p),
        SheetKind::AddModel(p) => providers::add_model_head(sheet.id, p),
        SheetKind::Diagnostic(p) => providers::diagnostic_head(sheet.id, p),
        SheetKind::Profile(p) => profiles::profile_head(sheet.id, p),
        SheetKind::UsageOptions { .. } => SheetHead::new(super::i18n::t("providerUsage.options")),
        SheetKind::PluginLogs(p) => plugins::logs_head(sheet.id, p),
    }
}

pub fn sheet_body(ui: &mut Ui, s: &Settings, v: &View, sheet: &Sheet) {
    match &sheet.kind {
        SheetKind::Form(form) => overlay::form_body(ui, form),
        SheetKind::Report { text, .. } => overlay::report_body(ui, sheet.id, text),
        SheetKind::Confirm(_) => {}
        SheetKind::SystemPrompt(p) => daemon::system_prompt_body(ui, sheet.id, p),
        SheetKind::TerminalProfile(p) => daemon::terminal_profile_body(ui, sheet.id, p),
        SheetKind::Skills(p) => daemon::skills_body(ui, sheet.id, p),
        SheetKind::Provider(p) => providers::provider_sheet_body(ui, s, v, sheet.id, p),
        SheetKind::AddModel(p) => providers::add_model_body(ui, s, sheet.id, p),
        SheetKind::Diagnostic(p) => providers::diagnostic_body(ui, p),
        SheetKind::Profile(p) => profiles::profile_body(ui, sheet.id, p),
        SheetKind::UsageOptions { host } => usage::options_body(ui, s, v, host),
        SheetKind::PluginLogs(p) => plugins::logs_body(ui, p),
    }
}

/// Whether a sheet is mid-save (scrim taps and the close glyph are ignored).
pub fn sheet_busy(sheet: &Sheet) -> bool {
    match &sheet.kind {
        SheetKind::Form(form) => form.busy,
        SheetKind::SystemPrompt(p) => p.saving,
        SheetKind::TerminalProfile(p) => p.pending,
        SheetKind::Skills(p) => p.saving,
        SheetKind::AddModel(p) => p.saving,
        SheetKind::Profile(p) => p.form.submitting,
        _ => false,
    }
}
