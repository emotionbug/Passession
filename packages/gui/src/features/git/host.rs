//! The git feature's single reading point for shell and core state
//! (ui-api.md §2/§8/§9, core-api.md "State").

use super::json::{array, text};
use crate::common::{PanelTarget, UiContext};
use serde_json::Value;

/// Catalog lookup through the shell's translation utility.
pub fn translate(key: &str, args: &[(&'static str, String)]) -> String {
    if args.is_empty() {
        return crate::common::tr(key);
    }
    let borrowed: Vec<(&str, &str)> = args.iter().map(|(name, value)| (*name, value.as_str())).collect();
    crate::common::tr_args(key, &borrowed)
}

pub fn host<'a>(ui: &'a UiContext, host_id: &str) -> Option<&'a Value> {
    ui.host(host_id)
}

/// `client.status == ConnectionStatus.connected`.
pub fn connected(ui: &UiContext, host_id: &str) -> bool {
    host(ui, host_id).is_some_and(|h| text(h, "status") == "connected")
}

fn server_info<'a>(ui: &'a UiContext, host_id: &str) -> Option<&'a Value> {
    ui.server_infos
        .get(host_id)
        .or_else(|| host(ui, host_id).and_then(|h| h.get("serverInfo")))
}

/// `client.supports(feature)`: `server_info.features[feature] == true`.
pub fn supports(ui: &UiContext, host_id: &str, feature: &str) -> bool {
    server_info(ui, host_id).is_some_and(|info| info["features"][feature] == Value::Bool(true))
}

pub fn workspaces<'a>(ui: &'a UiContext, host_id: &str) -> &'a [Value] {
    host(ui, host_id).map(|h| array(h, "workspaces")).unwrap_or(&[])
}

/// Original `CheckoutGitActions._workspace`: by id, else by checkout directory.
pub fn workspace<'a>(ui: &'a UiContext, host_id: &str, workspace_id: &str, directory: &str) -> Option<&'a Value> {
    let all = workspaces(ui, host_id);
    if !workspace_id.is_empty()
        && let Some(found) = all.iter().find(|w| text(w, "id") == workspace_id)
    {
        return Some(found);
    }
    all.iter().find(|w| text(w, "workspaceDirectory") == directory)
}

pub fn preferences(ui: &UiContext) -> &Value {
    ui.preferences()
}

pub fn preference<'a>(ui: &'a UiContext, key: &str) -> &'a Value {
    &ui.preferences()[key]
}

/// A SharedPreferences value saved through `save_state("prefs:<key>")`.
pub fn prefs_value<'a>(ui: &'a UiContext, key: &str) -> Option<&'a Value> {
    ui.state.get("prefs").and_then(|prefs| prefs.get(key))
}

pub fn tabs(ui: &UiContext) -> &[PanelTarget] {
    &ui.layout.tabs
}

/// Original `readRecipients` label: the agent's title or name.
pub fn agent_label(ui: &UiContext, host_id: &str, agent_id: &str) -> String {
    let Some(host) = host(ui, host_id) else {
        return String::new();
    };
    for row in array(host, "agents") {
        let agent = match row.get("agent") {
            Some(agent @ Value::Object(_)) => agent,
            _ => row,
        };
        if text(agent, "id") == agent_id {
            let title = text(agent, "title");
            return if title.is_empty() { text(agent, "name").to_owned() } else { title.to_owned() };
        }
    }
    String::new()
}

pub fn is_mobile_platform() -> bool {
    cfg!(any(target_os = "android", target_os = "ios"))
}
