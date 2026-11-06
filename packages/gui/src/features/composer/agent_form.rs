//! Port of `lib/features/chat/composer/agent_form.dart` (create-agent form resolution and the
//! remembered `createAgentPreferences`). Pure functions over provider snapshot JSON.

use super::json::*;
use serde_json::{json, Map, Value};

/// create-agent-preferences: the remembered new-agent selection, stored in the app
/// preferences under this key.
pub const FORM_PREFERENCES_KEY: &str = "createAgentPreferences";

/// preferences.ts parseFormPreferences.
pub fn parse_form_preferences(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return json!({});
    };
    let mut providers = Map::new();
    if let Some(entries) = get(value, "providerPreferences").as_object() {
        for (provider, prefs) in entries {
            let mut next = Map::new();
            if let Some(model) = prefs.get("model").and_then(Value::as_str) {
                next.insert("model".into(), Value::String(model.into()));
            }
            if let Some(mode) = prefs.get("mode").and_then(Value::as_str) {
                next.insert("mode".into(), Value::String(mode.into()));
            }
            let mut thinking = Map::new();
            if let Some(rows) = prefs.get("thinkingByModel").and_then(Value::as_object) {
                for (model, option) in rows {
                    if option.is_string() {
                        thinking.insert(model.clone(), option.clone());
                    }
                }
            }
            if !thinking.is_empty() {
                next.insert("thinkingByModel".into(), Value::Object(thinking));
            }
            if let Some(features) = prefs.get("featureValues").filter(|v| v.is_object()) {
                next.insert("featureValues".into(), features.clone());
            }
            providers.insert(provider.clone(), Value::Object(next));
        }
    }
    let mut out = Map::new();
    if let Some(provider) = source.get("provider").filter(|v| v.is_string()) {
        out.insert("provider".into(), provider.clone());
    }
    if !providers.is_empty() {
        out.insert("providerPreferences".into(), Value::Object(providers));
    }
    for key in ["favoriteModels", "isolation", "launchTarget"] {
        if let Some(field) = source.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), field.clone());
        }
    }
    Value::Object(out)
}

#[derive(Default)]
pub struct ProviderPrefsUpdate<'a> {
    pub model: Option<&'a str>,
    pub mode: Option<&'a str>,
    pub clear_mode: bool,
    pub thinking_by_model: Option<Map<String, Value>>,
    pub feature_values: Option<Map<String, Value>>,
}

/// preferences.ts mergeProviderPreferences.
pub fn merge_provider_preferences(
    preferences: &Value,
    provider: &str,
    update: ProviderPrefsUpdate,
) -> Value {
    let mut all = obj(get(preferences, "providerPreferences"));
    let mut next = obj(all.get(provider).unwrap_or(&Value::Null));
    if let Some(model) = update.model {
        next.insert("model".into(), Value::String(model.into()));
    }
    if update.clear_mode {
        next.remove("mode");
    } else if let Some(mode) = update.mode {
        next.insert("mode".into(), Value::String(mode.into()));
    }
    if let Some(thinking) = update.thinking_by_model {
        let mut merged_thinking = obj(next.get("thinkingByModel").unwrap_or(&Value::Null));
        merged_thinking.extend(thinking);
        next.insert("thinkingByModel".into(), Value::Object(merged_thinking));
    }
    if let Some(features) = update.feature_values {
        let mut merged_features = obj(next.get("featureValues").unwrap_or(&Value::Null));
        merged_features.extend(features);
        next.insert("featureValues".into(), Value::Object(merged_features));
    }
    all.insert(provider.into(), Value::Object(next));
    let mut out = obj(preferences);
    out.insert("provider".into(), Value::String(provider.into()));
    out.insert("providerPreferences".into(), Value::Object(all));
    Value::Object(out)
}

fn entry<'a>(providers: &'a [Value], provider: &str) -> Option<&'a Value> {
    providers
        .iter()
        .find(|entry| get(entry, "provider").as_str() == Some(provider))
}

/// The provider entry for `provider` in the snapshot.
pub fn provider_entry<'a>(providers: &'a [Value], provider: &str) -> Option<&'a Value> {
    entry(providers, provider)
}

/// model-catalog.ts filterSelectableModels; None while the provider is not ready.
pub fn ready_models(providers: &[Value], provider: &str) -> Option<Vec<Value>> {
    let entry = entry(providers, provider)?;
    if get(entry, "status").as_str() != Some("ready") {
        return None;
    }
    Some(selectable_models(entry))
}

pub fn selectable_models(entry: &Value) -> Vec<Value> {
    list_ref(get(entry, "models"))
        .iter()
        .filter(|model| model.is_object() && get(model, "isSelectable") != &Value::Bool(false))
        .cloned()
        .collect()
}

/// model-catalog.ts findModelByReference: by id, then by alias.
pub fn find_model_by_reference<'a>(models: Option<&'a [Value]>, model_id: &str) -> Option<&'a Value> {
    let id = model_id.trim();
    let models = models?;
    if models.is_empty() || id.is_empty() {
        return None;
    }
    models
        .iter()
        .find(|model| s(get(model, "id")) == id)
        .or_else(|| {
            models.iter().find(|model| {
                list_ref(get(model, "aliases"))
                    .iter()
                    .any(|alias| alias.as_str() == Some(id))
            })
        })
}

/// resolve-agent-form.ts resolveDefaultModelId.
pub fn resolve_default_model_id(models: Option<&[Value]>) -> String {
    let Some(models) = models.filter(|m| !m.is_empty()) else {
        return String::new();
    };
    let model = models
        .iter()
        .find(|model| get(model, "isDefault") == &Value::Bool(true))
        .unwrap_or(&models[0]);
    owned(get(model, "id"))
}

fn canonical_model_id(models: Option<&[Value]>, model_id: &str) -> String {
    let id = model_id.trim();
    if id.is_empty() || models.is_none() {
        return id.to_owned();
    }
    find_model_by_reference(models, id)
        .map(|model| owned(get(model, "id")))
        .unwrap_or_default()
}

/// resolve-agent-form.ts resolveThinkingOptionId.
pub fn resolve_thinking_option_id(models: Option<&[Value]>, model_id: &str, requested: &str) -> String {
    let model = find_model_by_reference(models, model_id);
    let options = model.map(|m| maps(get(m, "thinkingOptions"))).unwrap_or_default();
    if options.is_empty() {
        return String::new();
    }
    let id = requested.trim();
    if !id.is_empty() && options.iter().any(|option| s(get(option, "id")) == id) {
        return id.to_owned();
    }
    let fallback = owned(get(&options[0], "id"));
    model
        .map(|m| s_or(get(m, "defaultThinkingOptionId"), &fallback).to_owned())
        .unwrap_or(fallback)
}

fn preferred_thinking(models: Option<&[Value]>, provider_prefs: &Value, model_id: &str) -> String {
    let model = find_model_by_reference(models, model_id);
    let references: Vec<String> = match model {
        None => vec![model_id.to_owned()],
        Some(model) => {
            let mut refs = vec![owned(get(model, "id"))];
            refs.extend(list_ref(get(model, "aliases")).iter().map(display));
            refs
        }
    };
    let by_model = get(provider_prefs, "thinkingByModel");
    for reference in references {
        let id = s(get(by_model, &reference)).trim();
        if !id.is_empty() {
            return id.to_owned();
        }
    }
    String::new()
}

/// resolve-agent-form.ts resolvePreferredModeId.
fn preferred_mode_id(providers: &[Value], provider: &str, initial: &str, preferred: &str) -> String {
    if !initial.trim().is_empty() {
        return initial.trim().to_owned();
    }
    if !preferred.trim().is_empty() {
        return preferred.trim().to_owned();
    }
    let Some(entry) = entry(providers, provider) else {
        return String::new();
    };
    let status = s(get(entry, "status"));
    if get(entry, "enabled") == &Value::Bool(false) || !matches!(status, "ready" | "loading") {
        return String::new();
    }
    let modes = maps(get(entry, "modes"));
    let default_mode = s(get(entry, "defaultModeId"));
    if !default_mode.is_empty()
        && (modes.is_empty() || modes.iter().any(|mode| s(get(mode, "id")) == default_mode))
    {
        return default_mode.to_owned();
    }
    modes.first().map(|mode| owned(get(mode, "id"))).unwrap_or_default()
}

fn config(provider: Option<&str>, model: &str, mode_id: &str, thinking: &str, features: &Value) -> Value {
    let mut out = Map::new();
    if let Some(provider) = provider.filter(|p| !p.is_empty()) {
        out.insert("provider".into(), Value::String(provider.into()));
    }
    if !model.is_empty() {
        out.insert("model".into(), Value::String(model.into()));
    }
    if !mode_id.is_empty() {
        out.insert("modeId".into(), Value::String(mode_id.into()));
    }
    if !thinking.is_empty() {
        out.insert("thinkingOptionId".into(), Value::String(thinking.into()));
    }
    if non_empty_obj(features) {
        out.insert("featureValues".into(), features.clone());
    }
    Value::Object(out)
}

fn provider_prefs<'a>(preferences: &'a Value, provider: &str) -> &'a Value {
    get(get(preferences, "providerPreferences"), provider)
}

/// resolve-agent-form.ts resolveFormStateFromProviderModels with nothing user-modified.
pub fn resolve_initial_agent_config(initial: &Value, preferences: &Value, providers: &[Value]) -> Value {
    let provider = s_or(get(initial, "provider"), s(get(preferences, "provider"))).to_owned();
    if provider.is_empty() {
        return config(None, "", "", "", &Value::Null);
    }
    let prefs = provider_prefs(preferences, &provider);
    let models = ready_models(providers, &provider);
    let models_ref = models.as_deref();
    let mode_id = preferred_mode_id(providers, &provider, s(get(initial, "modeId")), s(get(prefs, "mode")));
    let initial_model = s(get(initial, "model")).trim().to_owned();
    let preferred_model = s(get(prefs, "model")).trim().to_owned();
    let chosen = if initial_model.is_empty() { &preferred_model } else { &initial_model };
    let model = if chosen == "default" && models_ref.is_some_and(|m| !m.is_empty()) {
        let found = find_model_by_reference(models_ref, "default").map(|m| owned(get(m, "id"))).unwrap_or_default();
        if found.is_empty() { resolve_default_model_id(models_ref) } else { found }
    } else if !initial_model.is_empty() {
        let id = if models_ref.is_none() { initial_model.clone() } else { canonical_model_id(models_ref, &initial_model) };
        if id.is_empty() { initial_model.clone() } else { id }
    } else if !preferred_model.is_empty() {
        let id = if models_ref.is_none() { preferred_model.clone() } else { canonical_model_id(models_ref, &preferred_model) };
        if id.is_empty() { preferred_model.clone() } else { id }
    } else {
        String::new()
    };
    let initial_thinking = s(get(initial, "thinkingOptionId")).trim().to_owned();
    let mut thinking = if initial_thinking.is_empty() {
        preferred_thinking(models_ref, prefs, &model)
    } else {
        initial_thinking
    };
    if models_ref.is_some() {
        thinking = resolve_thinking_option_id(models_ref, &model, &thinking);
    }
    config(Some(&provider), &model, &mode_id, &thinking, get(initial, "featureValues"))
}

/// resolve-agent-form.ts SET_PROVIDER_AND_MODEL_FROM_USER / SET_MODEL_FROM_USER.
pub fn apply_user_model_choice(
    previous: &Value,
    provider: &str,
    model_id: &str,
    preferences: &Value,
    providers: &[Value],
) -> Value {
    let models = ready_models(providers, provider);
    let models_ref = models.as_deref();
    let prefs = provider_prefs(preferences, provider);
    let mut model = canonical_model_id(models_ref, model_id);
    if model.is_empty() {
        model = resolve_default_model_id(models_ref);
    }
    let same_provider = s(get(previous, "provider")) == provider && get(previous, "provider").is_string();
    let requested_thinking = if same_provider
        && canonical_model_id(models_ref, s(get(previous, "model"))) == model
    {
        owned(get(previous, "thinkingOptionId"))
    } else {
        preferred_thinking(models_ref, prefs, &model)
    };
    let current_mode = s(get(previous, "modeId")).trim().to_owned();
    let mode = if same_provider && !current_mode.is_empty() {
        current_mode
    } else {
        preferred_mode_id(providers, provider, "", s(get(prefs, "mode")))
    };
    let features = if same_provider { get(previous, "featureValues").clone() } else { Value::Null };
    config(
        Some(provider),
        &model,
        &mode,
        &resolve_thinking_option_id(models_ref, &model, &requested_thinking),
        &features,
    )
}

/// resolve-agent-form.ts applyProfile.
pub fn apply_profile_choice(profile: &Value, preferences: &Value, providers: &[Value]) -> Value {
    let provider = owned(get(profile, "provider"));
    let models = ready_models(providers, &provider);
    let models_ref = models.as_deref();
    let prefs = provider_prefs(preferences, &provider);
    let preferred_model = s_or(get(profile, "model"), s(get(prefs, "model"))).to_owned();
    let mut model = canonical_model_id(models_ref, &preferred_model);
    if model.is_empty() {
        model = resolve_default_model_id(models_ref);
    }
    let entry = entry(providers, &provider);
    let mode_ids: Vec<String> = entry
        .map(|e| maps(get(e, "modes")).iter().map(|m| owned(get(m, "id"))).collect())
        .unwrap_or_default();
    let preferred_mode = s_or(get(profile, "modeId"), s(get(prefs, "mode"))).to_owned();
    let default_mode = entry.map(|e| owned(get(e, "defaultModeId"))).unwrap_or_default();
    let mode_id = if mode_ids.contains(&preferred_mode) {
        preferred_mode
    } else if mode_ids.contains(&default_mode) {
        default_mode
    } else {
        String::new()
    };
    let profile_thinking = owned(get(profile, "thinkingOptionId"));
    let thinking = if profile_thinking.is_empty() {
        resolve_thinking_option_id(models_ref, &model, &preferred_thinking(models_ref, prefs, &model))
    } else {
        profile_thinking
    };
    config(Some(&provider), &model, &mode_id, &thinking, get(profile, "featureValues"))
}

fn single(model: &str, thinking: &str) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(model.into(), Value::String(thinking.into()));
    map
}

fn opt(text: &str) -> Option<&str> {
    if text.is_empty() { None } else { Some(text) }
}

/// The preferences after the user changed `previous` to `next`.
pub fn remember_user_selection(preferences: &Value, previous: &Value, next: &Value) -> Value {
    let provider = s(get(next, "provider"));
    if provider.is_empty() {
        return preferences.clone();
    }
    let model = s(get(next, "model"));
    if get(previous, "provider") != get(next, "provider") || get(previous, "model") != get(next, "model") {
        return merge_provider_preferences(preferences, provider, ProviderPrefsUpdate { model: opt(model), ..Default::default() });
    }
    if get(previous, "modeId") != get(next, "modeId") {
        return merge_provider_preferences(preferences, provider, ProviderPrefsUpdate { mode: opt(s(get(next, "modeId"))), ..Default::default() });
    }
    if get(previous, "thinkingOptionId") != get(next, "thinkingOptionId") && !model.is_empty() {
        return merge_provider_preferences(
            preferences,
            provider,
            ProviderPrefsUpdate { thinking_by_model: Some(single(model, s(get(next, "thinkingOptionId")))), ..Default::default() },
        );
    }
    let before = obj(get(previous, "featureValues"));
    let mut changed = Map::new();
    if let Some(after) = get(next, "featureValues").as_object() {
        for (key, value) in after {
            if before.get(key) != Some(value) {
                changed.insert(key.clone(), value.clone());
            }
        }
    }
    if !changed.is_empty() {
        return merge_provider_preferences(preferences, provider, ProviderPrefsUpdate { feature_values: Some(changed), ..Default::default() });
    }
    preferences.clone()
}

/// The feature values remembered for `provider`.
pub fn remembered_feature_values(preferences: &Value, provider: &str) -> Value {
    Value::Object(obj(get(provider_prefs(preferences, provider), "featureValues")))
}

/// feature-preferences.ts resolveFeatureValues.
pub fn resolve_feature_values(features: &[Value], remembered: &Value, local: &Value) -> Value {
    let mut out = Map::new();
    for feature in features {
        let id = s(get(feature, "id"));
        if let Some(value) = local.get(id) {
            out.insert(id.into(), value.clone());
        } else if let Some(value) = remembered.get(id) {
            out.insert(id.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// feature-preferences.ts applyFeatureValues.
pub fn apply_feature_values(features: &[Value], values: &Value) -> Vec<Value> {
    features
        .iter()
        .map(|feature| match values.get(s(get(feature, "id"))) {
            Some(value) => {
                let mut next = feature.clone();
                set(&mut next, "value", value.clone());
                next
            }
            None => feature.clone(),
        })
        .collect()
}

/// preferences.ts applyAgentProfilePreferences.
pub fn remember_profile_selection(
    preferences: &Value,
    previous_provider: &str,
    providers: &[Value],
    config: &Value,
    profile_feature_values: &Value,
) -> Value {
    let mut result = preferences.clone();
    if !previous_provider.is_empty() {
        let previous_mode = owned(get(provider_prefs(&result, previous_provider), "mode"));
        let offered: Vec<String> = entry(providers, previous_provider)
            .map(|e| maps(get(e, "modes")).iter().map(|m| owned(get(m, "id"))).collect())
            .unwrap_or_default();
        if !previous_mode.is_empty() && !offered.contains(&previous_mode) {
            result = merge_provider_preferences(&result, previous_provider, ProviderPrefsUpdate { clear_mode: true, ..Default::default() });
        }
    }
    let model = s(get(config, "model"));
    let mode = s(get(config, "modeId"));
    let thinking = s(get(config, "thinkingOptionId"));
    merge_provider_preferences(
        &result,
        s(get(config, "provider")),
        ProviderPrefsUpdate {
            model: opt(model),
            mode: opt(mode),
            clear_mode: mode.is_empty(),
            thinking_by_model: (!model.is_empty() && !thinking.is_empty()).then(|| single(model, thinking)),
            feature_values: Some(obj(profile_feature_values)),
        },
    )
}

/// use-agent-form-state.ts persistProviderPreferences, on submit.
pub fn remember_submitted_selection(preferences: &Value, config: &Value, providers: &[Value]) -> Value {
    let provider = s(get(config, "provider"));
    if provider.is_empty() {
        return preferences.clone();
    }
    let models = ready_models(providers, provider);
    let typed = s(get(config, "model"));
    let resolved = if typed.trim().is_empty() {
        None
    } else {
        find_model_by_reference(models.as_deref(), typed)
    };
    let model = resolved.map(|m| s_or(get(m, "id"), typed).to_owned()).unwrap_or_else(|| typed.to_owned());
    let mode = s(get(config, "modeId"));
    let thinking = s(get(config, "thinkingOptionId"));
    merge_provider_preferences(
        preferences,
        provider,
        ProviderPrefsUpdate {
            model: opt(&model),
            mode: opt(mode),
            thinking_by_model: (!model.is_empty() && !thinking.is_empty()).then(|| single(&model, thinking)),
            ..Default::default()
        },
    )
}

/// provider-selection.ts resolveEffectiveComposer*: the config a draft submits.
pub fn effective_submit_config(config: &Value, providers: &[Value]) -> Value {
    let provider = owned(get(config, "provider"));
    let entry = entry(providers, &provider);
    let models = entry.map(selectable_models).unwrap_or_default();
    let mut model = s(get(config, "model")).trim().to_owned();
    if model.is_empty() {
        model = models
            .iter()
            .find(|m| get(m, "isDefault") == &Value::Bool(true))
            .or_else(|| models.first())
            .map(|m| owned(get(m, "id")))
            .unwrap_or_default();
    }
    let loading = entry.is_none_or(|e| s(get(e, "status")) == "loading");
    let selected_mode = s(get(config, "modeId")).trim().to_owned();
    let mode_ids: Vec<String> = entry
        .map(|e| maps(get(e, "modes")).iter().map(|m| owned(get(m, "id"))).collect())
        .unwrap_or_default();
    let mode_id = if loading || mode_ids.contains(&selected_mode) {
        selected_mode
    } else {
        mode_ids.first().cloned().unwrap_or_default()
    };
    let mut thinking = s(get(config, "thinkingOptionId")).trim().to_owned();
    if thinking.is_empty() {
        thinking = models
            .iter()
            .find(|m| s(get(m, "id")) == model)
            .map(|m| owned(get(m, "defaultThinkingOptionId")))
            .unwrap_or_default();
    }
    let mut out = obj(config);
    out.insert("provider".into(), Value::String(provider));
    out.remove("model");
    out.remove("modeId");
    out.remove("thinkingOptionId");
    if !model.is_empty() {
        out.insert("model".into(), Value::String(model));
    }
    if !mode_id.is_empty() {
        out.insert("modeId".into(), Value::String(mode_id));
    }
    if !thinking.is_empty() {
        out.insert("thinkingOptionId".into(), Value::String(thinking));
    }
    Value::Object(out)
}

/// provider-selection.ts resolveSubmissionReadiness: the localization key of the first
/// reason a draft cannot be created yet, or None.
pub fn draft_submission_blocker(
    config: &Value,
    providers: &[Value],
    connected: bool,
    catalog_loading: bool,
) -> Option<&'static str> {
    if !connected {
        return Some("providerSelection.readiness.hostDisconnected");
    }
    if providers.is_empty() {
        return Some("providerSelection.readiness.noProviders");
    }
    let provider = s(get(config, "provider"));
    if provider.is_empty() {
        return Some("providerSelection.selectModel");
    }
    if catalog_loading || entry(providers, provider).is_some_and(|e| s(get(e, "status")) == "loading") {
        return Some("providerSelection.readiness.modelDefaultsLoading");
    }
    None
}

/// settings_forms.dart providerEntries: the snapshot's entries; a compact snapshot expands
/// each model's `thinkingSet` index into its `thinkingOptions` and stamps `provider`.
pub fn provider_entries(payload: &Value) -> Vec<Value> {
    let compact = get(payload, "compactSnapshot");
    if !non_empty_obj(compact) {
        return list_ref(get(payload, "entries")).iter().map(|e| Value::Object(obj(e))).collect();
    }
    let sets = list_ref(get(compact, "thinkingSets"));
    list_ref(get(compact, "entries"))
        .iter()
        .map(|value| {
            let mut entry = obj(value);
            let provider = entry.get("provider").cloned().unwrap_or(Value::Null);
            let models: Vec<Value> = list_ref(get(value, "models"))
                .iter()
                .map(|model| {
                    let mut model = obj(model);
                    model.insert("provider".into(), provider.clone());
                    if let Some(index) = model.get("thinkingSet").and_then(Value::as_i64)
                        && index >= 0
                        && (index as usize) < sets.len()
                    {
                        model.insert("thinkingOptions".into(), get(&sets[index as usize], "options").clone());
                    }
                    Value::Object(model)
                })
                .collect();
            entry.insert("models".into(), Value::Array(models));
            Value::Object(entry)
        })
        .collect()
}
