//! Where the composer's persisted data lives (core-api.md "State" / "save_state keys").

use super::drafts::DRAFTS_STORAGE_KEY;
use serde_json::Value;
use std::path::PathBuf;

/// The SharedPreferences draft payload as the core publishes it (`state.prefs[key]`,
/// a JSON string in the Flutter convention). Null until the core state is initialized.
pub fn drafts_payload(state: &Value) -> Option<&Value> {
    if state.get("initialized") == Some(&Value::Bool(false)) {
        return None;
    }
    let prefs = state.get("prefs")?;
    Some(prefs.get(DRAFTS_STORAGE_KEY).unwrap_or(&Value::Null))
}

/// `save_state` key that writes the draft payload.
pub fn drafts_save_key() -> String {
    format!("prefs:{DRAFTS_STORAGE_KEY}")
}

/// Whether the core can persist at all (`storage.writable`).
pub fn storage_error(state: &Value) -> Option<String> {
    let storage = state.get("storage")?;
    match storage.get("error") {
        Some(Value::String(error)) => Some(error.clone()),
        Some(Value::Object(error)) => error.get("message").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Retained composer image blobs: immutable, content-addressed files in the application
/// support directory (never the daemon home).
pub fn blob_dir() -> PathBuf {
    paseo_gui_platform::paths::app_paths()
        .support_dir
        .join("paseo-native")
        .join("composer-blobs")
}
