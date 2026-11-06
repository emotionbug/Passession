//! The one adapter between the files feature and the rest of the client: core state paths,
//! server features, preferences, shell effects, the platform adapter and the composer inbox.
//! A different shape elsewhere is a change here only.

use serde_json::Value;

use crate::common::{UiContext, UiEffect, host_record};

/// The `state.hosts[i]` record of a host.
pub fn host<'a>(state: &'a Value, host_id: &str) -> Option<&'a Value> {
    host_record(state, host_id)
}

pub fn connected(state: &Value, host_id: &str) -> bool {
    host(state, host_id).and_then(|h| h.get("status")).and_then(Value::as_str) == Some("connected")
}

/// The host's `server_info` (state record, falling back to the last `Connected`).
pub fn server_info<'a>(ui: &'a UiContext, host_id: &str) -> Option<&'a Value> {
    host(&ui.state, host_id)
        .and_then(|h| h.get("serverInfo"))
        .filter(|info| info.is_object())
        .or_else(|| ui.server_infos.get(host_id))
}

/// `server_info.features.<key> == true` (COMPAT gates: fsEntryOps, fsEntryDuplicate,
/// workspaceFileEditing).
pub fn feature(ui: &UiContext, host_id: &str, key: &str) -> bool {
    server_info(ui, host_id)
        .and_then(|info| info.get("features"))
        .and_then(|features| features.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The cwd of a workspace: its `workspaceDirectory` on that host.
pub fn workspace_directory(state: &Value, host_id: &str, workspace_id: &str) -> Option<String> {
    host(state, host_id)?
        .get("workspaces")?
        .as_array()?
        .iter()
        .find(|w| w.get("id").and_then(Value::as_str) == Some(workspace_id))?
        .get("workspaceDirectory")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
        .map(str::to_owned)
}

pub fn preferences(state: &Value) -> &Value {
    state.get("preferences").unwrap_or(&Value::Null)
}

pub fn set_preference(ui: &mut UiContext, key: &str, value: Value) {
    ui.push(UiEffect::SetPreference { key: key.to_owned(), value });
}

/// Flutter `_localHost`: a desktop-managed host reached through a local socket or loopback.
/// The core publishes the rule as `state.hosts[i].isLocal`.
pub fn is_local_desktop_host(ui: &UiContext, host_id: &str) -> bool {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return false;
    }
    host(&ui.state, host_id).and_then(|record| record.get("isLocal")).and_then(Value::as_bool).unwrap_or(false)
}

pub fn copy_text(ui: &mut UiContext, text: String, toast: Option<String>) {
    ui.push(UiEffect::CopyText { text, toast });
}

pub fn toast(ui: &mut UiContext, message: impl Into<String>, is_error: bool) {
    ui.toast(message, is_error);
}

pub fn open_external(ui: &mut UiContext, url: &str) {
    ui.push(UiEffect::OpenExternal { url: url.to_owned() });
}

/// Opens a file tab; `beside` is "open to the side" (Flutter `open(beside: true)`).
pub fn open_file(ui: &mut UiContext, host_id: &str, workspace_id: &str, path: &str, beside: bool) {
    let effect = if beside {
        UiEffect::OpenFileBeside { host_id: host_id.to_owned(), workspace_id: workspace_id.to_owned(), path: path.to_owned() }
    } else {
        UiEffect::OpenFile { host_id: host_id.to_owned(), workspace_id: workspace_id.to_owned(), path: path.to_owned() }
    };
    ui.push(effect);
}

pub fn set_close_guard(ui: &mut UiContext, owner: &str, dirty: bool) {
    ui.push(UiEffect::SetCloseGuard { owner: owner.to_owned(), dirty });
}

pub fn close_tab(ui: &mut UiContext, tab_key: &str) {
    ui.push(UiEffect::CloseTab { tab_key: tab_key.to_owned() });
}

/// Flutter `ChatDrafts.instance.addAttachments(serverId, workspaceId, [...])`.
pub fn add_to_chat(host_id: &str, workspace_id: &str, attachments: Vec<Value>) {
    crate::features::composer::add_workspace_attachments(host_id, workspace_id, attachments, None);
}

pub fn platform() -> Option<paseo_gui_platform::PlatformHandle> {
    paseo_gui_platform::platform()
}

/// Whether a platform save dialog exists (desktop) — mobile always shares.
pub fn has_save_dialog() -> bool {
    !cfg!(any(target_os = "android", target_os = "ios"))
}
