//! The model/agent-controls sheet (Flutter `_AgentControlsSheet` + `_ModelListSheet`):
//! profiles, providers, models (search across providers), thinking, mode and feature pages.
//! Rows are rebuilt from the controller on every draw so the open sheet follows the live
//! provider catalog and draft features.

use crate::features::composer::controller::Controller;
use crate::features::composer::controls_model::*;
use crate::features::composer::i18n::{t, t_args};
use crate::features::composer::json::*;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Root,
    Providers,
    Models,
    Thinking,
    Mode,
    Feature,
}

#[derive(Clone, Debug)]
pub struct SheetState {
    pub pages: Vec<Page>,
    pub drill: Option<String>,
    pub feature: Option<Value>,
    pub can_switch: bool,
    /// The selection shown while a running agent applies the change (Flutter sheet-local
    /// `config`); drafts read their persisted config directly.
    pub config: Option<Value>,
    pub rows: Vec<Row>,
    /// Compact single-provider model browser stacked over this sheet (`_ModelListSheet`).
    pub stacked: Option<String>,
    pub stacked_rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Leading {
    None,
    Icon(&'static str, Option<[f32; 4]>),
    Provider(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowAction {
    ApplyProfile(Value),
    OpenProfiles,
    Drill(String),
    SelectModel { provider: String, model: String },
    OpenModels,
    Push(Page),
    OpenFeature(Value),
    Choose(String),
    /// `{provider, model, name}` seed of a new profile.
    CreateProfileFromModel(Value),
    EditProfile(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub heading: bool,
    pub divider: bool,
    pub leading: Leading,
    pub label: String,
    pub label_muted: bool,
    pub description: String,
    /// Settings-row value (`_SheetRow`) or provider-row trailing text.
    pub value: String,
    pub chevron: bool,
    pub loading: bool,
    pub warning: bool,
    pub selected: bool,
    pub action: Option<RowAction>,
    /// The row's trailing icon button (profile create/edit).
    pub secondary: Option<(&'static str, RowAction, String)>,
    pub error: bool,
}

impl Row {
    fn new(label: impl Into<String>) -> Self {
        Self {
            heading: false,
            divider: false,
            leading: Leading::None,
            label: label.into(),
            label_muted: false,
            description: String::new(),
            value: String::new(),
            chevron: false,
            loading: false,
            warning: false,
            selected: false,
            action: None,
            secondary: None,
            error: false,
        }
    }

    fn heading(label: impl Into<String>) -> Self {
        Self { heading: true, ..Self::new(label) }
    }

    fn divider() -> Self {
        Self { divider: true, ..Self::new("") }
    }
}

pub struct SheetView {
    pub title: String,
    pub back: bool,
    pub leading_provider: Option<(String, String)>,
    pub gear: Option<String>,
    pub searchable: bool,
    pub placeholder: String,
    pub empty: Option<(&'static str, String)>,
    pub loading: bool,
}

fn labels() -> (String, String, String, String, String) {
    (
        t("modelSelector.selectModel"),
        t("modelSelector.loading"),
        t("modelSelector.error"),
        t("agentControls.thinking.unknown"),
        t("agentControls.thinking.extraHigh"),
    )
}

impl SheetState {
    pub fn new(can_switch: bool) -> Self {
        Self { pages: vec![Page::Root], drill: None, feature: None, can_switch, config: None, rows: Vec::new(), stacked: None, stacked_rows: Vec::new() }
    }

    pub fn page(&self) -> Page {
        *self.pages.last().unwrap_or(&Page::Root)
    }

    pub fn push(&mut self, page: Page) {
        self.pages.push(page);
    }

    pub fn pop(&mut self) {
        if self.pages.len() > 1 {
            self.pages.pop();
        }
        if matches!(self.page(), Page::Providers | Page::Root) {
            self.drill = None;
        }
    }

    pub fn config(&self, ctl: &Controller) -> Value {
        if ctl.is_draft() {
            return ctl.config();
        }
        self.config.clone().unwrap_or_else(|| ctl.config())
    }

    fn modes(&self, ctl: &Controller, config: &Value) -> Vec<Value> {
        if !self.can_switch {
            return ctl.modes();
        }
        crate::features::composer::agent_form::provider_entry(&ctl.providers, s(get(config, "provider")))
            .map(|entry| maps(get(entry, "modes")))
            .unwrap_or_else(|| ctl.modes())
    }

    fn available_providers(&self, ctl: &Controller, config: &Value) -> Vec<Value> {
        if self.can_switch {
            return ctl.providers.clone();
        }
        ctl.providers
            .iter()
            .find(|p| get(p, "provider") == get(config, "provider"))
            .or_else(|| ctl.providers.first())
            .cloned()
            .into_iter()
            .collect()
    }

    /// Rebuild rows for the current page and query; returns the header/view facts.
    pub fn build(&mut self, ctl: &Controller, compact: bool, query: &str, profiles_supported: bool) -> SheetView {
        let config = self.config(ctl);
        let features = ctl.features();
        let modes = self.modes(ctl, &config);
        let providers = ctl.providers.clone();
        let model = ControlsModel { providers: &providers, config: &config, modes: &modes, features: &features };
        let (select_model, loading_label, error_label, unknown, extra_high) = labels();
        let model_labels = ModelLabels {
            select_model: &select_model,
            loading: &loading_label,
            error: &error_label,
            thinking_unknown: &unknown,
            extra_high: &extra_high,
        };
        let page = self.page();
        let drilled = self
            .drill
            .as_ref()
            .and_then(|id| providers.iter().find(|p| s(get(p, "provider")) == id))
            .cloned();
        let available = self.available_providers(ctl, &config);
        let query = query.trim().to_lowercase();
        let searchable = matches!(page, Page::Root | Page::Providers | Page::Models);
        let mut view = SheetView {
            title: match page {
                Page::Root => t("modelSelector.selectModel"),
                Page::Providers => t("modelSelector.title"),
                Page::Models => drilled
                    .as_ref()
                    .map(|p| s_or(get(p, "label"), s(get(p, "provider"))).to_owned())
                    .unwrap_or_else(|| self.drill.clone().unwrap_or_default()),
                Page::Thinking => t("agentControls.thinking.title"),
                Page::Mode => t("agentControls.mode.title"),
                Page::Feature => self.feature.as_ref().map(|f| owned(get(f, "label"))).unwrap_or_default(),
            },
            back: self.pages.len() > 1,
            leading_provider: (page == Page::Models).then(|| {
                let svg = drilled.as_ref().map(|p| owned(get(p, "iconSvg"))).unwrap_or_default();
                (self.drill.clone().unwrap_or_default(), svg)
            }),
            gear: (page == Page::Models).then(|| self.drill.clone().unwrap_or_default()),
            searchable,
            placeholder: if page == Page::Models || !self.can_switch {
                t("modelSelector.searchPlaceholder")
            } else {
                t("modelSelector.searchAllPlaceholder")
            },
            empty: None,
            loading: false,
        };
        let mut rows = Vec::new();
        if searchable && !query.is_empty() {
            let only = if page == Page::Models { drilled.clone() } else { None };
            let scope = match &only {
                Some(p) => vec![p.clone()],
                None => available.clone(),
            };
            for provider in &scope {
                let provider_label = s_or(get(provider, "label"), s(get(provider, "provider"))).to_owned();
                let matches: Vec<Value> = models_of(provider)
                    .into_iter()
                    .filter(|row| {
                        [model_label(row), owned(get(row, "id")), provider_label.clone(), owned(get(row, "description"))]
                            .iter()
                            .any(|field| field.to_lowercase().contains(&query))
                    })
                    .collect();
                let qualified = only.is_none() && available.len() > 1;
                self.model_rows(&mut rows, ctl, &model, provider, &matches, RowFlags { qualified, profiles_supported });
            }
            if rows.is_empty() {
                view.empty = Some(("search", t_args("modelSelector.noMatchesForQuery", &[("query", query.as_str())])));
            }
            self.rows = rows;
            return view;
        }
        match page {
            Page::Root => {
                self.profile_rows(&mut rows, ctl, &config, profiles_supported);
                if compact {
                    rows.push(Row::divider());
                    self.settings_rows(&mut rows, &model, &model_labels, &features, true);
                } else {
                    if !ctl.profiles.is_empty() {
                        rows.push(Row::heading(t("modelSelector.providers")));
                    }
                    if available.len() == 1 {
                        let provider = available[0].clone();
                        let models = models_of(&provider);
                        self.model_rows(&mut rows, ctl, &model, &provider, &models, RowFlags { qualified: false, profiles_supported });
                    } else {
                        self.provider_rows(&mut rows, &available);
                    }
                    let mut settings = Vec::new();
                    self.settings_rows(&mut settings, &model, &model_labels, &features, false);
                    if !settings.is_empty() {
                        rows.push(Row::divider());
                        rows.extend(settings);
                    }
                }
            }
            Page::Providers => self.provider_rows(&mut rows, &available),
            Page::Models => {
                let provider = drilled.clone().unwrap_or_default();
                let models = models_of(&provider);
                if models.is_empty() {
                    if get(&provider, "error").is_null() && s(get(&provider, "status")) == "loading" {
                        view.loading = true;
                    } else if get(&provider, "error").is_null() {
                        view.empty = Some(("search", t("modelSelector.noMatches")));
                    } else {
                        view.empty = Some(("triangle-alert", display(get(&provider, "error"))));
                    }
                }
                self.model_rows(&mut rows, ctl, &model, &provider, &models, RowFlags { qualified: false, profiles_supported });
            }
            Page::Thinking => {
                let options = model.thinking_options();
                let selected = s_or(get(&config, "thinkingOptionId"), options.first().map(|o| s(get(o, "id"))).unwrap_or("")).to_owned();
                for option in options {
                    let mut row = Row::new(format_thinking_option_label(&option, &extra_high));
                    row.selected = s(get(&option, "id")) == selected;
                    row.action = Some(RowAction::Choose(owned(get(&option, "id"))));
                    rows.push(row);
                }
            }
            Page::Mode => {
                let selected = model.selected_mode().map(|m| owned(get(m, "id"))).unwrap_or_default();
                for mode in &modes {
                    let mut row = Row::new(format_agent_mode_label(mode));
                    if let Some(icon) = mode_option_icon(Some(mode)) {
                        row.leading = Leading::Icon(icon, None);
                    }
                    row.selected = s(get(mode, "id")) == selected;
                    row.action = Some(RowAction::Choose(owned(get(mode, "id"))));
                    rows.push(row);
                }
            }
            Page::Feature => {
                let feature = self.feature.clone().unwrap_or_default();
                let current = display(&model.feature_value(&feature));
                if s(get(&feature, "type")) == "toggle" {
                    for (id, label) in [("true", t("agentControls.features.on")), ("false", t("agentControls.features.off"))] {
                        let mut row = Row::new(label);
                        row.selected = current == id;
                        row.action = Some(RowAction::Choose(id.to_owned()));
                        rows.push(row);
                    }
                } else {
                    for option in maps(get(&feature, "options")) {
                        let mut row = Row::new(s_or(get(&option, "label"), s(get(&option, "id"))).to_owned());
                        row.selected = display(get(&option, "id")) == current;
                        row.action = Some(RowAction::Choose(display(get(&option, "id"))));
                        rows.push(row);
                    }
                }
            }
        }
        self.rows = rows;
        view
    }

    /// model-browser.tsx AgentProfilesPickerContent.
    fn profile_rows(&self, rows: &mut Vec<Row>, ctl: &Controller, config: &Value, profiles_supported: bool) {
        let profiles: Vec<Value> = ctl
            .profiles
            .iter()
            .filter(|p| self.can_switch || get(p, "provider") == get(config, "provider"))
            .cloned()
            .collect();
        if profiles.is_empty() && profiles_supported {
            let mut row = Row::new(t("modelSelector.createProfile"));
            row.leading = Leading::Icon("plus", None);
            row.label_muted = true;
            row.action = Some(RowAction::OpenProfiles);
            rows.push(row);
            return;
        }
        if profiles.is_empty() {
            return;
        }
        let mut heading = Row::heading(t("modelSelector.profiles"));
        if profiles_supported {
            heading.secondary = Some(("pencil", RowAction::OpenProfiles, t("modelSelector.editProfilesLabel")));
        }
        rows.push(heading);
        for profile in profiles {
            let mut row = Row::new(s_or(get(&profile, "name"), s(get(&profile, "id"))).to_owned());
            row.leading = Leading::Provider(owned(get(&profile, "provider")), String::new());
            row.description = owned(get(&profile, "model"));
            row.action = Some(RowAction::ApplyProfile(profile));
            rows.push(row);
        }
    }

    /// model-browser.tsx GroupedProviderRows.
    fn provider_rows(&self, rows: &mut Vec<Row>, providers: &[Value]) {
        for (index, provider) in providers.iter().enumerate() {
            if index > 0 {
                rows.push(Row::divider());
            }
            let models = models_of(provider);
            let loading = models.is_empty() && s(get(provider, "status")) == "loading";
            let failed = models.is_empty() && !get(provider, "error").is_null();
            let mut row = Row::new(s_or(get(provider, "label"), s(get(provider, "provider"))).to_owned());
            row.leading = Leading::Provider(owned(get(provider, "provider")), owned(get(provider, "iconSvg")));
            row.loading = loading;
            row.warning = failed;
            row.value = if loading {
                t("modelSelector.loadingShort")
            } else if failed {
                t("modelSelector.error")
            } else {
                let count = models.len().to_string();
                t_args(
                    if models.len() == 1 { "modelSelector.modelCount" } else { "modelSelector.modelCountPlural" },
                    &[("count", count.as_str())],
                )
            };
            row.chevron = true;
            row.action = Some(RowAction::Drill(owned(get(provider, "provider"))));
            rows.push(row);
        }
    }

    fn model_rows(
        &self,
        rows: &mut Vec<Row>,
        ctl: &Controller,
        model: &ControlsModel,
        provider: &Value,
        models: &[Value],
        flags: RowFlags,
    ) {
        let RowFlags { qualified, profiles_supported } = flags;
        let provider_id = owned(get(provider, "provider"));
        let provider_label = s_or(get(provider, "label"), &provider_id).to_owned();
        let selected_id = model.selected_model().map(|m| owned(get(&m, "id")));
        for entry in models {
            let mut row = Row::new(model_label(entry));
            row.leading = Leading::Provider(provider_id.clone(), owned(get(provider, "iconSvg")));
            let model_id = owned(get(entry, "id"));
            row.description = if qualified { format!("{provider_label} · {model_id}") } else { model_id.clone() };
            if let Some(description) = get(entry, "description").as_str().filter(|s| !s.is_empty() && *s != model_id.as_str()) {
                row.description.push_str(" · ");
                row.description.push_str(description);
            }
            row.selected = s(get(model.config, "provider")) == provider_id && selected_id.as_deref() == Some(s(get(entry, "id")));
            row.action = Some(RowAction::SelectModel { provider: provider_id.clone(), model: model_id.clone() });
            // model-browser.tsx SelectableModelRow profileAction.
            if profiles_supported && !model_id.is_empty() {
                let profiled: Vec<&Value> = ctl
                    .profiles
                    .iter()
                    .filter(|p| s(get(p, "provider")) == provider_id && s(get(p, "model")) == model_id)
                    .collect();
                row.secondary = match profiled.len() {
                    0 => Some((
                        "plus",
                        RowAction::CreateProfileFromModel(serde_json::json!({
                            "provider": provider_id,
                            "model": model_id,
                            "name": model_label(entry),
                        })),
                        t("modelSelector.createProfileFromModel"),
                    )),
                    1 => Some((
                        "pencil",
                        RowAction::EditProfile(profiled[0].clone()),
                        t_args("modelSelector.editProfileLabel", &[("name", s(get(profiled[0], "name")))]),
                    )),
                    n => {
                        let count = n.to_string();
                        Some(("pencil", RowAction::OpenProfiles, t_args("modelSelector.editProfilesCount", &[("count", count.as_str())])))
                    }
                };
            }
            rows.push(row);
        }
    }

    fn settings_rows(&self, rows: &mut Vec<Row>, model: &ControlsModel, labels: &ModelLabels, features: &[Value], include_model: bool) {
        let provider = owned(get(model.config, "provider"));
        if include_model {
            let mut row = Row::new(t("modelSelector.model"));
            row.leading = Leading::Provider(provider.clone(), owned(get(&model.provider(), "iconSvg")));
            row.value = model.selected_model_label(labels);
            row.chevron = true;
            row.action = Some(RowAction::OpenModels);
            rows.push(row);
        }
        if !model.thinking_options().is_empty() {
            let mut row = Row::new(t("agentControls.thinking.title"));
            row.leading = Leading::Icon("brain", None);
            row.value = model.thinking_label(labels);
            row.chevron = true;
            row.action = Some(RowAction::Push(Page::Thinking));
            rows.push(row);
        }
        if !model.modes.is_empty() {
            let mut row = Row::new(t("agentControls.mode.title"));
            row.leading = Leading::Icon(mode_icon(model.selected_mode()), None);
            row.value = format_agent_mode_label(model.selected_mode().unwrap_or(&Value::Null));
            row.chevron = true;
            row.action = Some(RowAction::Push(Page::Mode));
            rows.push(row);
        }
        for feature in features {
            let kind = s(get(feature, "type"));
            if kind != "toggle" && kind != "select" {
                continue;
            }
            let value = model.feature_value(feature);
            let mut row = Row::new(s_or(get(feature, "label"), s(get(feature, "id"))).to_owned());
            row.leading = Leading::Icon(feature_icon(feature), feature_icon_tint(feature, &value));
            row.value = if kind == "toggle" {
                if value == Value::Bool(true) { t("agentControls.features.on") } else { t("agentControls.features.off") }
            } else {
                maps(get(feature, "options"))
                    .iter()
                    .find(|o| get(o, "id") == &value)
                    .map(|o| owned(get(o, "label")))
                    .unwrap_or_else(|| owned(get(feature, "label")))
            };
            row.chevron = true;
            row.action = Some(RowAction::OpenFeature(feature.clone()));
            rows.push(row);
        }
    }
}

/// Model row presentation: provider-qualified descriptions (cross-provider search) and the
/// profile create/edit trailing action.
#[derive(Clone, Copy)]
struct RowFlags {
    qualified: bool,
    profiles_supported: bool,
}
