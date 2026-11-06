//! Port of `agent_profile_form.dart` (original `profile-form-model.ts`): the
//! create/edit form's selections seeded to catalog defaults, provider-scoped
//! resets, feature pruning and the submit value. Pure state; the sheet in
//! `pages/profiles.rs` drives it.

use serde_json::{Map, Value, json};

use crate::features::settings::i18n::t;
use crate::features::settings::json::{arr, as_obj, str_of};

/// Original `generateAgentProfileId`.
pub fn generate_profile_id() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("agent_profile_{}_{}", radix36(millis), &random[..10])
}

pub fn radix36(mut value: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".into();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

fn sentence_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// `_splitCompactLabel`.
fn split_compact(value: &str, split_hyphen: bool) -> String {
    let mut spaced = String::new();
    for c in value.chars() {
        if c == '_' || (split_hyphen && c == '-') {
            spaced.push(' ');
        } else {
            spaced.push(c);
        }
    }
    // camelCase and ACRONYMWord boundaries.
    let chars: Vec<char> = spaced.chars().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            let prev = chars[i - 1];
            let next_lower = chars.get(i + 1).map(|n| n.is_ascii_lowercase()).unwrap_or(false);
            if prev.is_ascii_lowercase() || (prev.is_ascii_uppercase() && next_lower) {
                out.push(' ');
            }
        }
        out.push(*c);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `formatAgentModeLabel`.
pub fn mode_label(mode: &Value) -> String {
    let raw = str_or_id(mode);
    sentence_case(&split_compact(raw.trim(), mode.get("label").map(Value::is_null).unwrap_or(true)))
}

/// `formatThinkingOptionLabel`.
pub fn thinking_label(option: &Value) -> String {
    let raw = str_or_id(option);
    let compact: String = raw.chars().filter(|c| !c.is_whitespace() && *c != '_' && *c != '-').collect::<String>().to_lowercase();
    if compact == "xhigh" || compact == "extrahigh" {
        return t("agentControls.thinking.extraHigh");
    }
    sentence_case(&split_compact(raw.trim(), true))
}

fn str_or_id(value: &Value) -> String {
    let label = str_of(value, "label");
    if label.is_empty() { str_of(value, "id").to_string() } else { label.to_string() }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Opt {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Resolution {
    Idle,
    Pending,
    Complete,
}

pub struct ProfileForm {
    pub name: String,
    pub icon: String,
    pub color: String,
    pub notes: String,
    pub provider: String,
    pub model_id: String,
    pub mode_id: String,
    pub thinking_id: String,
    pub feature_values: Map<String, Value>,
    pub provider_display: Option<Display>,
    pub model_display: Option<Display>,
    pub mode_display: Option<Display>,
    pub thinking_display: Option<Display>,
    pub provider_options: Vec<Opt>,
    pub model_options: Vec<Opt>,
    pub mode_options: Vec<Opt>,
    pub thinking_options: Vec<Opt>,
    pub features: Vec<Value>,
    pub catalog: Resolution,
    pub feature_resolution: Resolution,
    pub feature_request: Option<Value>,
    pub feature_key: Option<String>,
    pub show_model: bool,
    pub show_mode: bool,
    pub show_thinking: bool,
    pub show_features: bool,
    pub submitting: bool,
    pub submit_error: Option<String>,
    pub can_submit: bool,
    pub submit_value: Option<Value>,
    entries: Vec<Value>,
    resolved_feature_key: Option<String>,
    resolved_features: Vec<Value>,
}

fn seed(value: &str) -> Option<Display> {
    if value.is_empty() { None } else { Some(Display { label: value.to_string(), description: None }) }
}

fn refine(current: Option<Display>, selected: &str, resolved: Option<String>) -> Option<Display> {
    if selected.is_empty() {
        return None;
    }
    match resolved {
        Some(label) if !label.is_empty() => Some(Display { label, description: None }),
        _ => current.or_else(|| seed(selected)),
    }
}

impl ProfileForm {
    /// `profile` is the stored profile when editing; `seed` pre-fills create.
    pub fn new(profile: Option<&Value>, create_seed: Option<&Value>) -> Self {
        let editing = profile.is_some();
        let empty = json!({});
        let stored = profile.unwrap_or(&empty);
        let seed_value = if editing { None } else { create_seed };
        let pick = |seed_key: &[&str], stored_key: &str| -> String {
            if let Some(s) = seed_value {
                for key in seed_key {
                    let value = str_of(s, key);
                    if !value.is_empty() {
                        return value.to_string();
                    }
                }
            }
            str_of(stored, stored_key).to_string()
        };
        let name = pick(&["name"], "name");
        let provider = pick(&["provider"], "provider");
        let model_id = pick(&["modelId", "model"], "model");
        let mut form = Self {
            provider_display: seed(&provider),
            model_display: seed(&model_id),
            mode_display: seed(str_of(stored, "modeId")),
            thinking_display: seed(str_of(stored, "thinkingOptionId")),
            name,
            icon: str_of(stored, "icon").to_string(),
            color: str_of(stored, "color").to_string(),
            notes: str_of(stored, "notes").to_string(),
            mode_id: str_of(stored, "modeId").to_string(),
            thinking_id: str_of(stored, "thinkingOptionId").to_string(),
            feature_values: as_obj(stored.get("featureValues").unwrap_or(&Value::Null)).clone(),
            provider,
            model_id,
            provider_options: Vec::new(),
            model_options: Vec::new(),
            mode_options: Vec::new(),
            thinking_options: Vec::new(),
            features: Vec::new(),
            catalog: Resolution::Idle,
            feature_resolution: Resolution::Idle,
            feature_request: None,
            feature_key: None,
            show_model: false,
            show_mode: false,
            show_thinking: false,
            show_features: false,
            submitting: false,
            submit_error: None,
            can_submit: false,
            submit_value: None,
            entries: Vec::new(),
            resolved_feature_key: None,
            resolved_features: Vec::new(),
        };
        form.derive();
        form
    }

    fn entry(&self, id: &str) -> Option<&Value> {
        self.entries.iter().find(|e| str_of(e, "provider") == id)
    }

    fn models(&self, id: &str) -> Vec<Value> {
        self.entry(id)
            .map(|e| arr(e, "models").iter().filter(|m| m.get("isSelectable").and_then(Value::as_bool) != Some(false)).cloned().collect())
            .unwrap_or_default()
    }

    fn modes(&self, id: &str) -> Vec<Value> {
        self.entry(id).map(|e| arr(e, "modes").to_vec()).unwrap_or_default()
    }

    fn effective_model<'a>(models: &'a [Value], id: &str) -> Option<&'a Value> {
        let trimmed = id.trim();
        if !trimmed.is_empty() {
            return models.iter().find(|m| str_of(m, "id") == trimmed);
        }
        models.iter().find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true)).or(models.first())
    }

    fn thinking(&self, provider: &str, model: &str) -> Vec<Value> {
        let models = self.models(provider);
        Self::effective_model(&models, model).map(|m| arr(m, "thinkingOptions").to_vec()).unwrap_or_default()
    }

    fn default_model_id(models: &[Value]) -> String {
        models
            .iter()
            .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))
            .or(models.first())
            .map(|m| str_of(m, "id").to_string())
            .unwrap_or_default()
    }

    fn default_thinking_id(model: Option<&Value>) -> String {
        let Some(model) = model else { return String::new() };
        if let Some(Value::String(declared)) = model.get("defaultThinkingOptionId") {
            return declared.clone();
        }
        let options = arr(model, "thinkingOptions");
        options
            .iter()
            .find(|o| o.get("isDefault").and_then(Value::as_bool) == Some(true))
            .or(options.first())
            .map(|o| str_of(o, "id").to_string())
            .unwrap_or_default()
    }

    fn derive(&mut self) {
        let entry = self.entry(&self.provider).cloned();
        let models = self.models(&self.provider);
        let modes = self.modes(&self.provider);
        if self.model_id.is_empty() {
            self.model_id = Self::default_model_id(&models);
        }
        if self.mode_id.is_empty() {
            let declared = entry.as_ref().map(|e| str_of(e, "defaultModeId").to_string()).unwrap_or_default();
            self.mode_id = if declared.is_empty() {
                modes.first().map(|m| str_of(m, "id").to_string()).unwrap_or_default()
            } else {
                declared
            };
        }
        if self.thinking_id.is_empty() {
            self.thinking_id = Self::default_thinking_id(Self::effective_model(&models, &self.model_id));
        }
        let thinking = self.thinking(&self.provider, &self.model_id);

        self.feature_request = if self.provider.is_empty() {
            None
        } else {
            let mut request = Map::new();
            request.insert("provider".into(), Value::String(self.provider.clone()));
            if !self.model_id.is_empty() {
                request.insert("model".into(), Value::String(self.model_id.clone()));
            }
            if !self.mode_id.is_empty() {
                request.insert("modeId".into(), Value::String(self.mode_id.clone()));
            }
            if !self.thinking_id.is_empty() {
                request.insert("thinkingOptionId".into(), Value::String(self.thinking_id.clone()));
            }
            Some(Value::Object(request))
        };
        self.feature_key = self.feature_request.as_ref().map(|_| format!("{}|{}|{}|{}", self.provider, self.model_id, self.mode_id, self.thinking_id));
        let current = self.feature_key.is_some() && self.feature_key == self.resolved_feature_key;
        self.features = if current {
            self.resolved_features
                .iter()
                .map(|feature| {
                    let id = str_of(feature, "id");
                    match self.feature_values.get(id) {
                        Some(value) => {
                            let mut f = feature.clone();
                            f["value"] = value.clone();
                            f
                        }
                        None => feature.clone(),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        self.feature_resolution = match (&self.feature_key, current) {
            (None, _) => Resolution::Idle,
            (Some(_), true) => Resolution::Complete,
            (Some(_), false) => Resolution::Pending,
        };

        self.provider_options = self
            .entries
            .iter()
            .filter(|e| e.get("enabled").and_then(Value::as_bool) == Some(true))
            .map(|e| Opt {
                id: str_of(e, "provider").to_string(),
                label: crate::features::settings::json::str_or(e, "label", str_of(e, "provider")).to_string(),
                description: e.get("description").and_then(Value::as_str).map(String::from),
            })
            .collect();
        self.model_options = models
            .iter()
            .map(|m| Opt {
                id: str_of(m, "id").to_string(),
                label: crate::features::settings::json::str_or(m, "label", str_of(m, "id")).to_string(),
                description: Some(crate::features::settings::json::str_or(m, "description", str_of(m, "id")).to_string()),
            })
            .collect();
        self.mode_options = modes
            .iter()
            .map(|m| Opt { id: str_of(m, "id").to_string(), label: mode_label(m), description: m.get("description").and_then(Value::as_str).map(String::from) })
            .collect();
        self.thinking_options = thinking
            .iter()
            .map(|o| Opt { id: str_of(o, "id").to_string(), label: thinking_label(o), description: o.get("description").and_then(Value::as_str).map(String::from) })
            .collect();

        let provider_label = entry.as_ref().and_then(|e| e.get("label")).and_then(Value::as_str).map(String::from);
        self.provider_display = refine(self.provider_display.take(), &self.provider, provider_label);
        let model_label = models.iter().find(|m| str_of(m, "id") == self.model_id).and_then(|m| m.get("label")).and_then(Value::as_str).map(String::from);
        self.model_display = refine(self.model_display.take(), &self.model_id, model_label);
        let mode_label_value = modes.iter().find(|m| str_of(m, "id") == self.mode_id).map(mode_label);
        self.mode_display = refine(self.mode_display.take(), &self.mode_id, mode_label_value);
        let thinking_value = thinking.iter().find(|o| str_of(o, "id") == self.thinking_id).map(thinking_label);
        self.thinking_display = refine(self.thinking_display.take(), &self.thinking_id, thinking_value);

        let has_provider = !self.provider.is_empty();
        self.show_model = has_provider && (!models.is_empty() || !self.model_id.is_empty());
        self.show_mode = has_provider && (!modes.is_empty() || !self.mode_id.is_empty());
        self.show_thinking = has_provider && (!thinking.is_empty() || !self.thinking_id.is_empty());
        self.show_features = has_provider && !self.features.is_empty();
        self.can_submit = !self.name.trim().is_empty() && has_provider && !self.submitting;
        let notes = self.notes.trim().to_string();
        self.submit_value = if self.can_submit {
            let mut value = Map::new();
            value.insert("name".into(), Value::String(self.name.trim().to_string()));
            if !self.icon.is_empty() {
                value.insert("icon".into(), Value::String(self.icon.clone()));
            }
            if !self.color.is_empty() {
                value.insert("color".into(), Value::String(self.color.clone()));
            }
            value.insert("provider".into(), Value::String(self.provider.clone()));
            if !self.model_id.is_empty() {
                value.insert("model".into(), Value::String(self.model_id.clone()));
            }
            if !self.mode_id.is_empty() {
                value.insert("modeId".into(), Value::String(self.mode_id.clone()));
            }
            if !self.thinking_id.is_empty() {
                value.insert("thinkingOptionId".into(), Value::String(self.thinking_id.clone()));
            }
            if !self.feature_values.is_empty() {
                value.insert("featureValues".into(), Value::Object(self.feature_values.clone()));
            }
            if !notes.is_empty() {
                value.insert("notes".into(), Value::String(notes));
            }
            Some(Value::Object(value))
        } else {
            None
        };
    }

    pub fn apply_catalog(&mut self, entries: Vec<Value>) {
        self.entries = entries;
        self.catalog = Resolution::Complete;
        self.derive();
    }

    /// Stale keys are ignored; values for features no longer listed drop.
    pub fn apply_features(&mut self, key: &str, features: Vec<Value>) {
        if self.feature_key.as_deref() != Some(key) {
            return;
        }
        let allowed: Vec<String> = features.iter().map(|f| str_of(f, "id").to_string()).collect();
        self.feature_values.retain(|id, _| allowed.contains(id));
        self.resolved_feature_key = Some(key.to_string());
        self.resolved_features = features;
        self.derive();
    }

    pub fn apply_features_unavailable(&mut self, key: &str) {
        if self.feature_key.as_deref() != Some(key) {
            return;
        }
        self.resolved_feature_key = Some(key.to_string());
        self.resolved_features = Vec::new();
        self.derive();
    }

    pub fn set_name(&mut self, value: &str) {
        self.name = value.to_string();
        self.derive();
    }

    pub fn set_notes(&mut self, value: &str) {
        self.notes = value.to_string();
        self.derive();
    }

    pub fn set_appearance(&mut self, icon: &str, color: &str) {
        self.icon = icon.to_string();
        self.color = color.to_string();
        self.derive();
    }

    /// Everything below the provider is provider-scoped.
    pub fn set_provider(&mut self, id: &str, display: Option<Display>) {
        if self.provider == id {
            return;
        }
        self.provider = id.to_string();
        self.provider_display = display;
        self.model_id.clear();
        self.model_display = None;
        self.mode_id.clear();
        self.mode_display = None;
        self.thinking_id.clear();
        self.thinking_display = None;
        self.feature_values.clear();
        self.derive();
    }

    /// A thinking level the new model does not offer has to go.
    pub fn set_model(&mut self, id: &str, display: Option<Display>) {
        if self.model_id == id {
            return;
        }
        let keeps = !self.thinking_id.is_empty() && self.thinking(&self.provider, id).iter().any(|o| str_of(o, "id") == self.thinking_id);
        self.model_id = id.to_string();
        self.model_display = display;
        if !keeps {
            self.thinking_id.clear();
            self.thinking_display = None;
        }
        self.derive();
    }

    pub fn set_mode(&mut self, id: &str, display: Option<Display>) {
        self.mode_id = id.to_string();
        self.mode_display = display;
        self.derive();
    }

    pub fn set_thinking(&mut self, id: &str, display: Option<Display>) {
        self.thinking_id = id.to_string();
        self.thinking_display = display;
        self.derive();
    }

    pub fn set_feature(&mut self, id: &str, value: Value) {
        self.feature_values.insert(id.to_string(), value);
        self.derive();
    }

    pub fn set_submitting(&mut self, value: bool) {
        self.submitting = value;
        self.derive();
    }

    pub fn set_submit_error(&mut self, value: Option<String>) {
        self.submit_error = value;
    }
}

/// Original `AgentProfilesSection.handleSave`: an edited profile is replaced,
/// not merged (`{id, cliDefault, ...value}`); a new one is appended.
pub fn apply_profile_save(current: &[Value], record: &Value, editing: bool) -> Vec<Value> {
    let id = record.get("id").cloned().unwrap_or(Value::Null);
    let mut value = as_obj(record).clone();
    value.remove("id");
    value.remove("cliDefault");
    if !editing {
        let mut next = current.to_vec();
        let mut entry = Map::new();
        entry.insert("id".into(), id);
        entry.extend(value);
        next.push(Value::Object(entry));
        return next;
    }
    current
        .iter()
        .map(|entry| {
            if entry.get("id") == Some(&id) {
                let mut replaced = Map::new();
                replaced.insert("id".into(), id.clone());
                if let Some(cli) = entry.get("cliDefault").filter(|v| !v.is_null()) {
                    replaced.insert("cliDefault".into(), cli.clone());
                }
                replaced.extend(value.clone());
                Value::Object(replaced)
            } else {
                entry.clone()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Vec<Value> {
        vec![json!({
            "provider": "codex", "label": "Codex", "enabled": true, "defaultModeId": "auto",
            "modes": [{"id": "read-only"}, {"id": "auto"}],
            "models": [
                {"id": "a", "label": "A", "thinkingOptions": [{"id": "low"}, {"id": "high", "isDefault": true}]},
                {"id": "b", "label": "B", "isDefault": true, "thinkingOptions": [{"id": "low"}]},
                {"id": "hidden", "isSelectable": false}
            ]
        })]
    }

    #[test]
    fn create_form_seeds_catalog_defaults_and_prunes_thinking_on_model_change() {
        let mut form = ProfileForm::new(None, Some(&json!({"provider": "codex", "name": "Review"})));
        form.apply_catalog(catalog());
        assert_eq!((form.model_id.as_str(), form.mode_id.as_str(), form.thinking_id.as_str()), ("b", "auto", "low"));
        form.set_model("a", None);
        // `low` exists on model a too, so it stays.
        assert_eq!(form.thinking_id, "low");
        form.set_thinking("high", None);
        form.set_model("b", None);
        assert_eq!(form.thinking_id, "low", "high is not offered by b; the default seeds instead");
        assert_eq!(form.model_options.len(), 2, "non-selectable models are hidden");
    }

    #[test]
    fn stale_feature_results_are_ignored_and_unknown_values_dropped() {
        let mut form = ProfileForm::new(Some(&json!({"name": "x", "provider": "codex", "featureValues": {"gone": true, "fast": true}})), None);
        form.apply_catalog(catalog());
        let key = form.feature_key.clone().unwrap();
        form.apply_features("other|key", vec![json!({"id": "fast", "type": "toggle"})]);
        assert!(form.features.is_empty());
        form.apply_features(&key, vec![json!({"id": "fast", "type": "toggle"})]);
        assert_eq!(form.feature_values.keys().collect::<Vec<_>>(), vec!["fast"]);
        assert_eq!(form.features[0]["value"], json!(true));
    }

    #[test]
    fn edit_save_replaces_record_and_keeps_cli_default() {
        let current = vec![json!({"id": "p1", "cliDefault": true, "name": "Old", "notes": "drop me"}), json!({"id": "p2", "name": "Other"})];
        let next = apply_profile_save(&current, &json!({"id": "p1", "name": "New", "provider": "codex", "cliDefault": false}), true);
        assert_eq!(next[0], json!({"id": "p1", "cliDefault": true, "name": "New", "provider": "codex"}));
        assert_eq!(next[1], current[1]);
    }
}
