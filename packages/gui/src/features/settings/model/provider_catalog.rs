//! Provider catalog rules from `provider_catalog.dart`: the canonical ACP
//! registry, install patches, ranking, status and discovered-model caching.

use std::sync::LazyLock;

use serde_json::{Value, json};

use super::match_score::{compare, score_text_fields};
use crate::features::settings::json::{arr, obj, str_of};

/// Canonical ACP catalog (`acp_catalog.json`, extracted from the Flutter
/// `acpProviderCatalog`; package pins are updated in that file).
pub static ACP_CATALOG: LazyLock<Vec<Value>> =
    LazyLock::new(|| serde_json::from_str(include_str!("acp_catalog.json")).expect("acp_catalog.json is valid JSON"));

/// Original `buildAcpProviderConfigPatch`: bundled providers are enabled in
/// place; registry entries become `extends: acp` providers.
pub fn acp_install_patch(entry: &Value, supports_hoplite: bool) -> Result<Value, String> {
    let id = str_of(entry, "id");
    if entry.get("bundledProvider").and_then(Value::as_bool) == Some(true) {
        if !supports_hoplite {
            return Err(crate::features::settings::i18n::t("settingsNative.hopliteUpdate"));
        }
        return Ok(json!({ "providers": { id: { "enabled": true } } }));
    }
    let mut provider = json!({
        "extends": "acp",
        "label": entry.get("title").cloned().unwrap_or(Value::Null),
        "description": entry.get("description").cloned().unwrap_or(Value::Null),
        "command": Value::Array(arr(entry, "command").to_vec()),
        "env": Value::Object(obj(entry, "env").clone()),
    });
    if let Some(params) = entry.get("params").filter(|p| !p.is_null()) {
        provider["params"] = Value::Object(params.as_object().cloned().unwrap_or_default());
    }
    Ok(json!({ "providers": { id: provider } }))
}

/// Original `rankProviderCatalog`: alphabetical without a query; otherwise
/// best `scoreTextFields` match over title and id first, ties alphabetical.
pub fn rank_catalog(entries: Vec<Value>, query: &str) -> Vec<Value> {
    let by_title = |a: &Value, b: &Value| str_of(a, "title").to_lowercase().cmp(&str_of(b, "title").to_lowercase());
    if query.trim().is_empty() {
        let mut sorted = entries;
        sorted.sort_by(by_title);
        return sorted;
    }
    let mut scored: Vec<(Value, super::match_score::MatchScore)> = entries
        .into_iter()
        .filter_map(|entry| {
            let score = score_text_fields(query, &[str_of(&entry, "title"), str_of(&entry, "id")])?;
            Some((entry, score))
        })
        .collect();
    scored.sort_by(|a, b| compare(&a.1, &b.1).then_with(|| by_title(&a.0, &b.0)));
    scored.into_iter().map(|(entry, _)| entry).collect()
}

/// Catalog entries not yet installed, ranked.
pub fn available_catalog(installed: &[String], query: &str) -> Vec<Value> {
    let entries = ACP_CATALOG.iter().filter(|e| !installed.iter().any(|id| id == str_of(e, "id"))).cloned().collect();
    rank_catalog(entries, query)
}

/// Original `isConfigOwnedProvider`: derived (`extends`) providers and
/// opted-in bundled catalog providers live in config.json.
pub fn config_owned(config: &Value, id: &str) -> bool {
    let providers = obj(config, "providers");
    let Some(entry) = providers.get(id) else { return false };
    !entry.get("extends").map(Value::is_null).unwrap_or(true)
        || ACP_CATALOG.iter().any(|e| str_of(e, "id") == id && e.get("bundledProvider").and_then(Value::as_bool) == Some(true))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Muted,
    Loading,
}

/// Original `getProviderStatus`: (tone, label key, model count).
pub fn provider_status(status: &str, enabled: bool, model_count: usize) -> (Tone, &'static str, Option<usize>) {
    if !enabled {
        return (Tone::Muted, "settings.providers.statuses.disabled", None);
    }
    match status {
        "loading" => (Tone::Loading, "settings.providers.statuses.loading", None),
        "error" => (Tone::Danger, "settings.providers.statuses.error", None),
        "ready" => (Tone::Success, "settings.providers.statuses.available", (model_count > 0).then_some(model_count)),
        _ => (Tone::Warning, "settings.providers.statuses.notInstalled", None),
    }
}

pub fn selectable_models(entry: &Value) -> Vec<Value> {
    arr(entry, "models").iter().filter(|m| m.get("isSelectable").and_then(Value::as_bool) != Some(false)).cloned().collect()
}

/// `providers.<id>.additionalModels` from the daemon config.
pub fn additional_models(config: &Value, provider: &str) -> Vec<Value> {
    obj(config, "providers").get(provider).map(|p| arr(p, "additionalModels").to_vec()).unwrap_or_default()
}

/// The last non-empty discovered model list for one host/provider.
#[derive(Clone, Debug, Default)]
pub struct DiscoveredCache {
    pub server_id: String,
    pub provider: String,
    pub models: Vec<Value>,
}

/// Original `resolveProviderDiscoveredModels`: while the snapshot refreshes
/// with none, the last list for the same host and provider stays.
pub fn resolve_discovered(
    server_id: &str,
    provider: &str,
    current: &Value,
    refreshing: bool,
    cache: &mut Option<DiscoveredCache>,
) -> Vec<Value> {
    let selectable = selectable_models(current);
    if !selectable.is_empty() {
        *cache = Some(DiscoveredCache { server_id: server_id.into(), provider: provider.into(), models: selectable.clone() });
        return selectable;
    }
    match cache {
        Some(previous) if refreshing && previous.server_id == server_id && previous.provider == provider => previous.models.clone(),
        _ => Vec::new(),
    }
}

/// Flutter `providerEntries`: expands `compactSnapshot` thinking sets.
pub fn provider_entries(payload: &Value) -> Vec<Value> {
    let compact = obj(payload, "compactSnapshot");
    if compact.is_empty() {
        return arr(payload, "entries").to_vec();
    }
    let compact = Value::Object(compact.clone());
    let sets = arr(&compact, "thinkingSets");
    arr(&compact, "entries")
        .iter()
        .map(|entry| {
            let mut entry = entry.clone();
            let provider = entry.get("provider").cloned().unwrap_or(Value::Null);
            let models: Vec<Value> = arr(&entry, "models")
                .iter()
                .map(|model| {
                    let mut model = model.clone();
                    if let Some(map) = model.as_object_mut() {
                        map.insert("provider".into(), provider.clone());
                        if let Some(index) = map.get("thinkingSet").and_then(Value::as_i64)
                            && index >= 0
                            && (index as usize) < sets.len()
                        {
                            map.insert("thinkingOptions".into(), sets[index as usize].get("options").cloned().unwrap_or(Value::Null));
                        }
                    }
                    model
                })
                .collect();
            if let Some(map) = entry.as_object_mut() {
                map.insert("models".into(), Value::Array(models));
            }
            entry
        })
        .collect()
}

/// Protocol `PROMPT_SENTINEL`.
pub const PROMPT_SENTINEL: &str = "{{{prompt}}}";

/// Protocol `DEFAULT_TERMINAL_PROFILES`, shown until the host persists a list.
pub fn default_terminal_profiles() -> Vec<Value> {
    vec![
        json!({"id": "claude", "name": "Claude Code", "command": "claude", "args": [PROMPT_SENTINEL], "icon": "claude"}),
        json!({"id": "codex", "name": "Codex", "command": "codex", "args": [PROMPT_SENTINEL], "icon": "codex"}),
        json!({"id": "opencode", "name": "OpenCode", "command": "opencode", "args": [format!("--prompt={PROMPT_SENTINEL}")], "icon": "opencode"}),
        json!({"id": "pi", "name": "Pi", "command": "pi", "args": [PROMPT_SENTINEL], "icon": "pi"}),
    ]
}
