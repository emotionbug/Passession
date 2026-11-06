//! Settings (SettingsProviders): the native port of the Flutter
//! `features/settings/**` screens — app preferences, theme/appearance,
//! language, keyboard shortcuts, host overview/connections/pairing, daemon
//! configuration, providers/models/profiles, metadata generation, skills,
//! Hub access and daemon-connected hosts, usage, plugins, and the restart /
//! durable daemon update flows. Root widget: `SettingsPanel`.

mod host;
mod i18n;
mod json;
mod model;
mod pages;
mod panel;
mod platform;
mod rpc;
mod state;
mod ui;

use serde_json::Value;

use crate::common::{Route, UiContext};

pub use panel::{SettingsPanel, SettingsPanelAction, SettingsPanelRef, SettingsPanelWidgetRefExt};

pub fn live_design(cx: &mut makepad_widgets::Cx) {
    ui::draw::live_design(cx);
    panel::live_design(cx);
}

/// Opens Settings › Host › Agents for `host_id` with the agent profile editor
/// sheet on top: `profile` edits that stored profile; otherwise `seed`
/// (`{"provider", "model" | "modelId", "name"}`, all optional) pre-fills a new
/// profile (Flutter `ProfileEditor(seed:)` from the composer model rows).
pub fn open_profile_editor(ui: &mut UiContext, host_id: &str, seed: Option<Value>, profile: Option<Value>) {
    panel::queue_intent(panel::Intent::ProfileEditor { host: host_id.to_string(), seed, profile });
    ui.push(crate::common::UiEffect::Navigate(Route::Settings { host_id: Some(host_id.to_string()), page: "host/agents".into() }));
}

/// Opens Settings › Host › Providers for `host_id` with `provider`'s settings
/// sheet (models, configure, diagnostic, refresh) on top.
pub fn open_provider_settings(ui: &mut UiContext, host_id: &str, provider: &str) {
    panel::queue_intent(panel::Intent::ProviderSheet { host: host_id.to_string(), provider: provider.to_string() });
    ui.push(crate::common::UiEffect::Navigate(Route::Settings { host_id: Some(host_id.to_string()), page: "host/providers".into() }));
}
