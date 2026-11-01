//! Open provider identifiers and mode visuals; plugins may add arbitrary IDs.
use crate::ProtocolError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::LazyLock};
pub const PROVIDER_DEFINITIONS_JSON: &str = include_str!("provider-definitions.json");
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderMode {
    pub id: String,
    pub label: String,
    pub description: String,
    pub icon: String,
    pub color_tier: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_unattended: Option<bool>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDefinition {
    pub id: String,
    pub label: String,
    pub description: String,
    pub default_mode_id: Option<String>,
    pub modes: Vec<ProviderMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_by_default: Option<bool>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct ProviderDefinitions {
    pub builtins: Vec<ProviderDefinition>,
    pub development: Vec<ProviderDefinition>,
}
static DEFINITIONS: LazyLock<ProviderDefinitions> = LazyLock::new(|| {
    serde_json::from_str(PROVIDER_DEFINITIONS_JSON).expect("embedded provider metadata")
});
pub fn definitions() -> &'static ProviderDefinitions {
    &DEFINITIONS
}
pub fn get_provider_definition(id: &str) -> Option<&'static ProviderDefinition> {
    DEFINITIONS
        .builtins
        .iter()
        .chain(&DEFINITIONS.development)
        .find(|p| p.id == id)
}
pub fn is_builtin_provider(id: &str) -> bool {
    DEFINITIONS.builtins.iter().any(|p| p.id == id)
}
pub fn unattended_mode_id(id: &str) -> Option<&'static str> {
    get_provider_definition(id)?
        .modes
        .iter()
        .find(|mode| mode.is_unattended == Some(true))
        .map(|mode| mode.id.as_str())
}
pub fn mode_visuals(provider: &str, mode: &str) -> Option<(&'static str, &'static str)> {
    let mode = get_provider_definition(provider)?
        .modes
        .iter()
        .find(|m| m.id == mode)?;
    Some((&mode.icon, &mode.color_tier))
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactProviderSnapshot {
    pub entries: Vec<Value>,
    pub thinking_sets: Vec<Value>,
}

/// Takes ownership to retain extension fields and move model data without
/// cloning the whole snapshot. Identical thinking sets share one wire entry.
pub fn compact_provider_snapshot(
    mut entries: Vec<Value>,
) -> Result<CompactProviderSnapshot, ProtocolError> {
    let mut thinking_sets = Vec::new();
    let mut indexes = HashMap::<String, usize>::new();
    for entry in &mut entries {
        let entry = entry
            .as_object_mut()
            .ok_or(ProtocolError::Invalid("provider entry"))?;
        let Some(models) = entry.get_mut("models") else {
            continue;
        };
        for model in models
            .as_array_mut()
            .ok_or(ProtocolError::Invalid("provider models"))?
        {
            let model = model
                .as_object_mut()
                .ok_or(ProtocolError::Invalid("provider model"))?;
            model.remove("provider");
            let Some(options) = model.remove("thinkingOptions") else {
                continue;
            };
            if !options.is_array() {
                return Err(ProtocolError::Invalid("thinkingOptions"));
            }
            let mut set = Map::new();
            set.insert("options".into(), options);
            if let Some(default) = model.remove("defaultThinkingOptionId") {
                set.insert("defaultOptionId".into(), default);
            }
            let set = Value::Object(set);
            let key = serde_json::to_string(&set).map_err(|_| ProtocolError::Json)?;
            let index = if let Some(index) = indexes.get(&key) {
                *index
            } else {
                let index = thinking_sets.len();
                thinking_sets.push(set);
                indexes.insert(key, index);
                index
            };
            model.insert("thinkingSet".into(), index.into());
        }
    }
    Ok(CompactProviderSnapshot {
        entries,
        thinking_sets,
    })
}
pub fn expand_provider_snapshot(
    mut snapshot: CompactProviderSnapshot,
) -> Result<Vec<Value>, ProtocolError> {
    for entry in &mut snapshot.entries {
        let entry = entry
            .as_object_mut()
            .ok_or(ProtocolError::Invalid("provider entry"))?;
        let provider = entry
            .get("provider")
            .and_then(Value::as_str)
            .ok_or(ProtocolError::Invalid("provider"))?
            .to_owned();
        let Some(models) = entry.get_mut("models") else {
            continue;
        };
        for model in models
            .as_array_mut()
            .ok_or(ProtocolError::Invalid("provider models"))?
        {
            let model = model
                .as_object_mut()
                .ok_or(ProtocolError::Invalid("provider model"))?;
            model
                .entry("provider")
                .or_insert_with(|| Value::String(provider.clone()));
            if let Some(index) = model.remove("thinkingSet") {
                let index = index
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or(ProtocolError::Invalid("thinkingSet"))?;
                let set = snapshot
                    .thinking_sets
                    .get(index)
                    .ok_or(ProtocolError::Invalid("missing thinking set"))?;
                let options = set
                    .get("options")
                    .filter(|v| v.is_array())
                    .ok_or(ProtocolError::Invalid("thinking options"))?;
                model.insert("thinkingOptions".into(), options.clone());
                if let Some(default) = set.get("defaultOptionId") {
                    model.insert("defaultThinkingOptionId".into(), default.clone());
                }
            }
        }
    }
    Ok(snapshot.entries)
}
