//! Pure projections of `agent_controls.dart` (AgentControlsModel, label formatting, toolbar
//! density) over the provider snapshot shared by the toolbar and the model sheet.

use super::json::*;
use serde_json::{json, Value};

pub struct ControlsModel<'a> {
    pub providers: &'a [Value],
    pub config: &'a Value,
    pub modes: &'a [Value],
    pub features: &'a [Value],
}

pub fn models_of(provider: &Value) -> Vec<Value> {
    list_ref(get(provider, "models"))
        .iter()
        .filter(|model| model.is_object() && get(model, "isSelectable") != &Value::Bool(false))
        .cloned()
        .collect()
}

pub fn model_label(model: &Value) -> String {
    s_or(get(model, "label"), s(get(model, "id"))).to_owned()
}

/// Localized fallbacks the model needs; provided by the caller's translation.
pub struct ModelLabels<'a> {
    pub select_model: &'a str,
    pub loading: &'a str,
    pub error: &'a str,
    pub thinking_unknown: &'a str,
    pub extra_high: &'a str,
}

impl<'a> ControlsModel<'a> {
    pub fn provider(&self) -> Value {
        let provider = get(self.config, "provider");
        self.providers
            .iter()
            .find(|entry| get(entry, "provider") == provider && !provider.is_null())
            .cloned()
            .unwrap_or_else(|| json!({}))
    }

    /// agent-controls/utils.ts pickSelectedModel.
    pub fn selected_model(&self) -> Option<Value> {
        let models = models_of(&self.provider());
        let id = s(get(self.config, "model")).trim().to_owned();
        if id.is_empty() {
            return models
                .iter()
                .find(|m| get(m, "isDefault") == &Value::Bool(true))
                .or_else(|| models.first())
                .cloned();
        }
        models
            .iter()
            .find(|m| s(get(m, "id")) == id)
            .or_else(|| models.iter().find(|m| list_ref(get(m, "aliases")).iter().any(|a| a.as_str() == Some(id.as_str()))))
            .cloned()
    }

    /// provider-selection.ts resolveSelectedModelLabel.
    pub fn selected_model_label(&self, labels: &ModelLabels) -> String {
        let provider = self.provider();
        if s(get(self.config, "provider")).trim().is_empty() || !non_empty_obj(&provider) {
            return labels.select_model.to_owned();
        }
        let models = models_of(&provider);
        if models.is_empty() && s(get(&provider, "status")) == "loading" {
            return labels.loading.to_owned();
        }
        if models.is_empty() && !get(&provider, "error").is_null() {
            return labels.error.to_owned();
        }
        let selected_id = s(get(self.config, "model")).trim().to_owned();
        let exact = models.iter().find(|m| get(m, "id") == get(self.config, "model") && !get(self.config, "model").is_null());
        if exact.is_none() && !selected_id.is_empty() {
            return selected_id;
        }
        let model = exact
            .or_else(|| models.iter().find(|m| get(m, "isDefault") == &Value::Bool(true)))
            .or_else(|| models.first());
        model.map(model_label).unwrap_or_else(|| labels.select_model.to_owned())
    }

    pub fn thinking_options(&self) -> Vec<Value> {
        self.selected_model().map(|m| maps(get(&m, "thinkingOptions"))).unwrap_or_default()
    }

    pub fn thinking_label(&self, labels: &ModelLabels) -> String {
        let options = self.thinking_options();
        let selected = options
            .iter()
            .find(|o| get(o, "id") == get(self.config, "thinkingOptionId"))
            .or_else(|| options.first());
        match selected {
            None => labels.thinking_unknown.to_owned(),
            Some(option) => format_thinking_option_label(option, labels.extra_high),
        }
    }

    pub fn selected_mode(&self) -> Option<&'a Value> {
        self.modes
            .iter()
            .find(|m| get(m, "id") == get(self.config, "modeId"))
            .or_else(|| self.modes.first())
    }

    pub fn feature_value(&self, feature: &Value) -> Value {
        let id = s(get(feature, "id"));
        match get(get(self.config, "featureValues"), id) {
            Value::Null => get(feature, "value").clone(),
            value => value.clone(),
        }
    }

    pub fn with_model(&self, provider_id: &str, model_id: &str) -> Value {
        let mut next = obj(self.config);
        next.insert("provider".into(), json!(provider_id));
        next.insert("model".into(), json!(model_id));
        if s(get(self.config, "provider")) != provider_id {
            next.remove("modeId");
            next.remove("thinkingOptionId");
            next.remove("featureValues");
        } else if s(get(self.config, "model")) != model_id {
            next.remove("thinkingOptionId");
        }
        Value::Object(next)
    }

    pub fn with_thinking(&self, id: &str) -> Value {
        merged(self.config, &json!({"thinkingOptionId": id}))
    }

    pub fn with_mode(&self, id: &str) -> Value {
        merged(self.config, &json!({"modeId": id}))
    }

    pub fn with_feature(&self, id: &str, value: Value) -> Value {
        let mut features = obj(get(self.config, "featureValues"));
        features.insert(id.into(), value);
        merged(self.config, &json!({"featureValues": features}))
    }
}

/// model-sheet.tsx shortModelLabel.
pub fn short_model_label(label: &str) -> String {
    match label.rfind('/') {
        Some(index) => label[index + 1..].to_owned(),
        None => label.to_owned(),
    }
}

fn sentence_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
    }
}

fn split_compact_label(value: &str, split_hyphen: bool) -> String {
    // replaceAll(splitHyphen ? [_-]+ : _+, ' ')
    let mut spaced = String::with_capacity(value.len());
    let mut in_run = false;
    for ch in value.chars() {
        let separator = ch == '_' || (split_hyphen && ch == '-');
        if separator {
            if !in_run {
                spaced.push(' ');
            }
            in_run = true;
        } else {
            spaced.push(ch);
            in_run = false;
        }
    }
    // ([a-z])([A-Z]) → '$1 $2', then ([A-Z]+)([A-Z][a-z]) → '$1 $2'
    let chars: Vec<char> = spaced.chars().collect();
    let mut out = String::with_capacity(spaced.len() + 8);
    for (index, &ch) in chars.iter().enumerate() {
        if index > 0 {
            let prev = chars[index - 1];
            let next = chars.get(index + 1).copied();
            let camel = prev.is_ascii_lowercase() && ch.is_ascii_uppercase();
            let acronym = prev.is_ascii_uppercase() && ch.is_ascii_uppercase() && next.is_some_and(|n| n.is_ascii_lowercase());
            if camel || acronym {
                out.push(' ');
            }
        }
        out.push(ch);
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// agent-controls/labels.ts formatAgentModeLabel.
pub fn format_agent_mode_label(mode: &Value) -> String {
    let raw = s_or(get(mode, "label"), s(get(mode, "id"))).trim().to_owned();
    sentence_case(&split_compact_label(&raw, get(mode, "label").is_null()))
}

/// agent-controls/labels.ts formatThinkingOptionLabel.
pub fn format_thinking_option_label(option: &Value, extra_high: &str) -> String {
    let raw = s_or(get(option, "label"), s(get(option, "id"))).trim().to_owned();
    let compact: String = raw
        .chars()
        .filter(|c| !(c.is_whitespace() || *c == '_' || *c == '-'))
        .collect::<String>()
        .to_lowercase();
    if compact == "xhigh" || compact == "extrahigh" {
        return extra_high.to_owned();
    }
    sentence_case(&split_compact_label(&raw, true))
}

/// agent-controls/icons.ts MODE_ICONS: the lucide glyph name for a mode, None when unknown.
pub fn mode_option_icon(mode: Option<&Value>) -> Option<&'static str> {
    match mode.map(|m| s(get(m, "icon"))) {
        Some("Bot") => Some("bot"),
        Some("Shield") => Some("shield"),
        Some("ShieldAlert") => Some("shield-alert"),
        Some("ShieldCheck") => Some("shield-check"),
        Some("ShieldEllipsis") => Some("shield-ellipsis"),
        Some("ShieldOff") => Some("shield-off"),
        Some("ShieldPlus") => Some("shield-plus"),
        Some("ShieldQuestionMark") => Some("shield-question-mark"),
        _ => None,
    }
}

pub fn mode_icon(mode: Option<&Value>) -> &'static str {
    mode_option_icon(mode).unwrap_or("bot")
}

/// agent-controls/icons.ts FEATURE_ICONS.
pub fn feature_icon(feature: &Value) -> &'static str {
    match s(get(feature, "icon")) {
        "list-todo" => "list-todo",
        "shield-check" => "shield-check",
        "zap" => "zap",
        _ => "settings-2",
    }
}

/// The tint of an active feature glyph (RGBA), None for the muted default.
pub fn feature_icon_tint(feature: &Value, value: &Value) -> Option<[f32; 4]> {
    let active = if s(get(feature, "type")) == "toggle" {
        value == &Value::Bool(true)
    } else {
        maps(get(feature, "options"))
            .iter()
            .filter(|o| get(o, "id") == value)
            .any(|o| get(o, "isDefault") != &Value::Bool(true))
    };
    if !active {
        return None;
    }
    let hex: u32 = match s(get(feature, "id")) {
        "fast_mode" | "service_tier" => 0xfacc15,
        "auto_accept" => 0x4ade80,
        "plan_mode" => 0x60a5fa,
        _ => return None,
    };
    Some([
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
        1.0,
    ])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Full,
    Condensed,
    Tight,
}

#[derive(Clone, Copy, Debug)]
pub struct Presentation {
    pub show_carets: bool,
    pub show_thinking_label: bool,
    pub show_mode_label: bool,
    pub aggregate_features: bool,
}

/// layout.ts resolveComposerControlDensity with 12px hysteresis.
pub fn resolve_density(
    available_width: f64,
    current: Density,
    has_model: bool,
    has_thinking: bool,
    has_mode: bool,
    features: &[Value],
    font_scale: f64,
) -> Density {
    const HYSTERESIS: f64 = 12.0;
    let scale = if font_scale.is_finite() { font_scale.max(1.0) } else { 1.0 };
    let sum = |widths: &[f64]| {
        if widths.is_empty() { 0.0 } else { widths.iter().sum::<f64>() + (widths.len() - 1) as f64 * 4.0 }
    };
    let mut full_parts = Vec::new();
    if has_model {
        full_parts.push(50.0 + 70.0 * scale);
    }
    if has_thinking {
        full_parts.push(54.0 + 48.0 * scale);
    }
    if has_mode {
        full_parts.push(54.0 + 96.0 * scale);
    }
    for feature in features {
        full_parts.push(if s(get(feature, "type")) == "toggle" {
            28.0
        } else {
            28.0 + 4.0 + 16.0 + s(get(feature, "label")).chars().count() as f64 * 7.0 * scale
        });
    }
    let full = sum(&full_parts);
    let mut condensed_parts = Vec::new();
    if has_model {
        condensed_parts.push(36.0 + 60.0 * scale);
    }
    if has_thinking {
        condensed_parts.push(28.0);
    }
    if has_mode {
        condensed_parts.push(36.0 + 96.0 * scale);
    }
    if !features.is_empty() {
        condensed_parts.push(28.0);
    }
    let condensed = sum(&condensed_parts);
    let width = available_width;
    match current {
        Density::Full => {
            if width >= full - HYSTERESIS {
                Density::Full
            } else if width >= condensed {
                Density::Condensed
            } else {
                Density::Tight
            }
        }
        Density::Condensed => {
            if width >= full + HYSTERESIS {
                Density::Full
            } else if width < condensed - HYSTERESIS {
                Density::Tight
            } else {
                Density::Condensed
            }
        }
        Density::Tight => {
            if width >= full + HYSTERESIS {
                Density::Full
            } else if width >= condensed + HYSTERESIS {
                Density::Condensed
            } else {
                Density::Tight
            }
        }
    }
}

pub fn presentation(density: Density) -> Presentation {
    match density {
        Density::Full => Presentation { show_carets: true, show_thinking_label: true, show_mode_label: true, aggregate_features: false },
        Density::Condensed => Presentation { show_carets: false, show_thinking_label: false, show_mode_label: true, aggregate_features: true },
        Density::Tight => Presentation { show_carets: false, show_thinking_label: false, show_mode_label: false, aggregate_features: true },
    }
}
