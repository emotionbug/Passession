//! The model/agent-controls sheet (Flutter `_AgentControlsSheet` + `_ModelListSheet`):
//! profiles, a quick switch over favorite or recent models, thinking, mode and feature pages,
//! and the full model list (Favorites, Recent, then every provider as a collapsible group,
//! searchable across providers). Rows are rebuilt from the controller on every draw so the
//! open sheet follows the live provider catalog, draft features and saved picks.

use crate::features::composer::controller::Controller;
use crate::features::composer::controls_model::*;
use crate::features::composer::i18n::{t, t_args};
use crate::features::composer::json::*;
use crate::features::composer::model_picks::{
    Availability, Group, ModelPicks, ModelRef, Pick, ProviderState, QuickKind, quick_switch,
    search, sections, starts_collapsed,
};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Root,
    /// The full model list.
    Models,
    Thinking,
    Mode,
    Feature,
}

#[derive(Clone, Debug)]
pub struct SheetState {
    pub pages: Vec<Page>,
    pub feature: Option<Value>,
    pub can_switch: bool,
    /// The selection shown while a running agent applies the change (Flutter sheet-local
    /// `config`); drafts read their persisted config directly.
    pub config: Option<Value>,
    pub rows: Vec<Row>,
    /// Provider groups the user expanded (`true`) or collapsed (`false`) in this sheet;
    /// others follow [`starts_collapsed`].
    pub expanded: HashMap<String, bool>,
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
    SelectModel {
        provider: String,
        model: String,
    },
    ToggleFavorite {
        provider: String,
        model: String,
    },
    /// Expand (`true`) or collapse a provider group of the full list.
    ToggleGroup {
        provider: String,
        expand: bool,
    },
    ProviderSettings(String),
    OpenModels,
    Push(Page),
    OpenFeature(Value),
    Choose(String),
    /// `{provider, model, name}` seed of a new profile.
    CreateProfileFromModel(Value),
    EditProfile(Value),
}

/// A trailing icon button of a row (favorite star, profile create/edit, provider settings).
#[derive(Clone, Debug, PartialEq)]
pub struct Trailing {
    pub icon: &'static str,
    pub action: RowAction,
    /// Tooltip and accessible name.
    pub label: String,
    /// Drawn in the highlight tint (a pinned favorite).
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub heading: bool,
    pub divider: bool,
    pub leading: Leading,
    pub label: String,
    pub label_muted: bool,
    /// Medium weight label (provider group headers).
    pub strong: bool,
    /// Shown but not selectable (unavailable model or provider): muted, no row hit.
    pub disabled: bool,
    pub description: String,
    /// Settings-row value (`_SheetRow`) or provider-row trailing text.
    pub value: String,
    pub chevron: bool,
    /// A collapsible group header: `Some(expanded)` draws a down/right chevron.
    pub expanded: Option<bool>,
    pub loading: bool,
    pub warning: bool,
    pub selected: bool,
    /// Reserve the check column even when unselected, so trailing buttons line up.
    pub check_slot: bool,
    pub action: Option<RowAction>,
    /// Trailing icon buttons, rightmost first.
    pub trailing: Vec<Trailing>,
    /// `section:provider/model` of a model row (empty otherwise): finds the same row again
    /// after a favorite toggle reshuffles the sections above it.
    pub key: String,
}

impl Row {
    fn new(label: impl Into<String>) -> Self {
        Self {
            heading: false,
            divider: false,
            leading: Leading::None,
            label: label.into(),
            label_muted: false,
            strong: false,
            disabled: false,
            description: String::new(),
            value: String::new(),
            chevron: false,
            expanded: None,
            loading: false,
            warning: false,
            selected: false,
            check_slot: false,
            action: None,
            trailing: Vec::new(),
            key: String::new(),
        }
    }

    fn heading(label: impl Into<String>) -> Self {
        Self {
            heading: true,
            ..Self::new(label)
        }
    }

    fn divider() -> Self {
        Self {
            divider: true,
            ..Self::new("")
        }
    }
}

pub struct SheetView {
    pub title: String,
    pub back: bool,
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

/// Model row presentation: provider-qualified descriptions (several providers in view) and
/// the profile create/edit trailing action.
#[derive(Clone, Copy)]
struct RowFlags {
    qualified: bool,
    profiles: bool,
    /// Names the section in [`Row::key`].
    section: &'static str,
}

impl SheetState {
    pub fn new(can_switch: bool) -> Self {
        Self {
            pages: vec![Page::Root],
            feature: None,
            can_switch,
            config: None,
            rows: Vec::new(),
            expanded: HashMap::new(),
        }
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
        crate::features::composer::agent_form::provider_entry(
            &ctl.providers,
            s(get(config, "provider")),
        )
        .map(|entry| maps(get(entry, "modes")))
        .unwrap_or_else(|| ctl.modes())
    }

    /// The one provider a running agent may use (its own, else the first listed); `None`
    /// when any provider can be chosen.
    fn scope(&self, ctl: &Controller, config: &Value) -> Option<String> {
        if self.can_switch {
            return None;
        }
        ctl.providers
            .iter()
            .find(|p| get(p, "provider") == get(config, "provider"))
            .or_else(|| ctl.providers.first())
            .map(|p| owned(get(p, "provider")))
    }

    /// Rebuild rows for the current page and query; returns the header/view facts.
    pub fn build(
        &mut self,
        ctl: &Controller,
        query: &str,
        profiles_supported: bool,
        picks: &ModelPicks,
    ) -> SheetView {
        let config = self.config(ctl);
        let features = ctl.features();
        let modes = self.modes(ctl, &config);
        let providers: &[Value] = &ctl.providers;
        let model = ControlsModel {
            providers,
            config: &config,
            modes: &modes,
            features: &features,
        };
        let (select_model, loading_label, error_label, unknown, extra_high) = labels();
        let model_labels = ModelLabels {
            select_model: &select_model,
            loading: &loading_label,
            error: &error_label,
            thinking_unknown: &unknown,
            extra_high: &extra_high,
        };
        let page = self.page();
        let scope = self.scope(ctl, &config);
        let scope = scope.as_deref();
        let qualified = scope.is_none() && providers.len() > 1;
        // The model in use counts as recent, so the quick switch is never empty.
        let current = model
            .selected_model()
            .map(|m| ModelRef::new(owned(get(&config, "provider")), owned(get(m, "id"))));
        let seeded = picks.clone().seeded(current.as_ref());
        let picks = &seeded;
        let rc = RowCtx {
            ctl,
            picks,
            selected: current,
        };
        let query = query.trim().to_lowercase();
        let searchable = matches!(page, Page::Root | Page::Models);
        let mut view = SheetView {
            title: match page {
                Page::Root => t("modelSelector.selectModel"),
                Page::Models => t("modelSelector.model"),
                Page::Thinking => t("agentControls.thinking.title"),
                Page::Mode => t("agentControls.mode.title"),
                Page::Feature => self
                    .feature
                    .as_ref()
                    .map(|f| owned(get(f, "label")))
                    .unwrap_or_default(),
            },
            back: self.pages.len() > 1,
            searchable,
            placeholder: if scope.is_some() {
                t("modelSelector.searchPlaceholder")
            } else {
                t("modelSelector.searchAllPlaceholder")
            },
            empty: None,
            loading: false,
        };
        let mut rows = Vec::new();
        if searchable && !query.is_empty() {
            let flags = RowFlags {
                qualified,
                profiles: profiles_supported,
                section: "search",
            };
            for pick in search(providers, picks, scope, &query) {
                rows.push(model_row(&rc, &pick, flags));
            }
            if rows.is_empty() {
                view.empty = Some((
                    "search",
                    t_args(
                        "modelSelector.noMatchesForQuery",
                        &[("query", query.as_str())],
                    ),
                ));
            }
            self.rows = rows;
            return view;
        }
        match page {
            Page::Root => {
                let profiles = self.profile_rows(&mut rows, ctl, &config, profiles_supported);
                let sections = sections(providers, picks, scope);
                if let Some((kind, quick)) = quick_switch(&sections) {
                    if !rows.is_empty() {
                        rows.push(Row::divider());
                    }
                    rows.push(Row::heading(match kind {
                        QuickKind::Favorites => t("modelSelector.favorites"),
                        QuickKind::Recents => t("modelSelector.recent"),
                    }));
                    let flags = RowFlags {
                        qualified,
                        profiles: false,
                        section: "quick",
                    };
                    for pick in &quick {
                        rows.push(model_row(&rc, pick, flags));
                    }
                }
                if !rows.is_empty() {
                    rows.push(Row::divider());
                }
                settings_rows(&mut rows, &model, &model_labels, &features);
                // With no profiles yet, creating one stays offered below the controls.
                if profiles == 0 && profiles_supported {
                    rows.push(Row::divider());
                    let mut row = Row::new(t("modelSelector.createProfile"));
                    row.leading = Leading::Icon("plus", None);
                    row.label_muted = true;
                    row.action = Some(RowAction::OpenProfiles);
                    rows.push(row);
                }
            }
            Page::Models => {
                let sections = sections(providers, picks, scope);
                if sections.groups.is_empty() {
                    if ctl.providers_loaded {
                        view.empty = Some(("bot", t("modelSelector.noProviders")));
                    } else {
                        view.loading = true;
                    }
                }
                let pinned = |section| RowFlags {
                    qualified,
                    profiles: profiles_supported,
                    section,
                };
                if !sections.favorites.is_empty() {
                    rows.push(Row::heading(t("modelSelector.favorites")));
                    for pick in &sections.favorites {
                        rows.push(model_row(&rc, pick, pinned("favorites")));
                    }
                }
                if !sections.recents.is_empty() {
                    rows.push(Row::heading(t("modelSelector.recent")));
                    for pick in &sections.recents {
                        rows.push(model_row(&rc, pick, pinned("recent")));
                    }
                }
                if !rows.is_empty() && !sections.groups.is_empty() {
                    rows.push(Row::heading(t("modelSelector.allModels")));
                }
                let count = sections.groups.len();
                for group in &sections.groups {
                    self.group_rows(&mut rows, &rc, group, count, profiles_supported);
                }
            }
            Page::Thinking => {
                let options = model.thinking_options();
                let selected = s_or(
                    get(&config, "thinkingOptionId"),
                    options.first().map(|o| s(get(o, "id"))).unwrap_or(""),
                )
                .to_owned();
                for option in options {
                    let mut row = Row::new(format_thinking_option_label(&option, &extra_high));
                    row.selected = s(get(&option, "id")) == selected;
                    row.action = Some(RowAction::Choose(owned(get(&option, "id"))));
                    rows.push(row);
                }
            }
            Page::Mode => {
                let selected = model
                    .selected_mode()
                    .map(|m| owned(get(m, "id")))
                    .unwrap_or_default();
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
                    for (id, label) in [
                        ("true", t("agentControls.features.on")),
                        ("false", t("agentControls.features.off")),
                    ] {
                        let mut row = Row::new(label);
                        row.selected = current == id;
                        row.action = Some(RowAction::Choose(id.to_owned()));
                        rows.push(row);
                    }
                } else {
                    for option in maps(get(&feature, "options")) {
                        let mut row =
                            Row::new(s_or(get(&option, "label"), s(get(&option, "id"))).to_owned());
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

    /// model-browser.tsx AgentProfilesPickerContent; returns how many profiles it listed.
    fn profile_rows(
        &self,
        rows: &mut Vec<Row>,
        ctl: &Controller,
        config: &Value,
        profiles_supported: bool,
    ) -> usize {
        let profiles: Vec<Value> = ctl
            .profiles
            .iter()
            .filter(|p| self.can_switch || get(p, "provider") == get(config, "provider"))
            .cloned()
            .collect();
        if profiles.is_empty() {
            return 0;
        }
        let count = profiles.len();
        let mut heading = Row::heading(t("modelSelector.profiles"));
        if profiles_supported {
            heading.trailing.push(Trailing {
                icon: "pencil",
                action: RowAction::OpenProfiles,
                label: t("modelSelector.editProfilesLabel"),
                active: false,
            });
        }
        rows.push(heading);
        for profile in profiles {
            let mut row = Row::new(s_or(get(&profile, "name"), s(get(&profile, "id"))).to_owned());
            row.leading = Leading::Provider(owned(get(&profile, "provider")), String::new());
            row.description = owned(get(&profile, "model"));
            row.action = Some(RowAction::ApplyProfile(profile));
            rows.push(row);
        }
        count
    }

    /// One provider of the full list: its header (count or state, collapse chevron, settings)
    /// and, while expanded, its models.
    fn group_rows(
        &self,
        rows: &mut Vec<Row>,
        rc: &RowCtx,
        group: &Group,
        group_count: usize,
        profiles_supported: bool,
    ) {
        let id = group.id().to_owned();
        let label = group.label().to_owned();
        let mut header = Row::new(label.clone());
        header.strong = true;
        header.leading = Leading::Provider(id.clone(), owned(get(group.provider, "iconSvg")));
        let mut expanded = true;
        match group.state {
            ProviderState::Ready => {
                let count = group.models.len().to_string();
                header.value = t_args(
                    if group.models.len() == 1 {
                        "modelSelector.modelCount"
                    } else {
                        "modelSelector.modelCountPlural"
                    },
                    &[("count", count.as_str())],
                );
                let holds_selection = rc.selected.as_ref().is_some_and(|r| r.provider == id);
                let collapsed_default =
                    starts_collapsed(group.models.len(), holds_selection, group_count);
                if group_count > 1 && group.models.len() > 1 {
                    expanded = self
                        .expanded
                        .get(&id)
                        .copied()
                        .unwrap_or(!collapsed_default);
                    header.expanded = Some(expanded);
                    header.action = Some(RowAction::ToggleGroup {
                        provider: id.clone(),
                        expand: !expanded,
                    });
                }
            }
            ProviderState::Loading => {
                header.loading = true;
                header.value = t("modelSelector.loadingShort");
            }
            ProviderState::Failed => {
                header.warning = true;
                header.value = t("modelSelector.error");
            }
            ProviderState::Disabled => {
                header.label_muted = true;
                header.value = t("modelSelector.disabled");
            }
        }
        header.trailing.push(Trailing {
            icon: "settings",
            action: RowAction::ProviderSettings(id.clone()),
            label: t_args(
                "modelSelector.openProviderSettings",
                &[("provider", label.as_str())],
            ),
            active: false,
        });
        rows.push(header);
        if group.state == ProviderState::Failed {
            // The daemon's reason stays secondary to a named condition.
            let mut row = Row::new(t("modelSelector.providerFailed"));
            row.disabled = true;
            row.leading = Leading::Icon("triangle-alert", None);
            row.description = display(get(group.provider, "error"));
            rows.push(row);
        }
        if !expanded {
            return;
        }
        let flags = RowFlags {
            qualified: false,
            profiles: profiles_supported,
            section: "group",
        };
        for entry in &group.models {
            rows.push(model_row(rc, &Pick::ready(group.provider, entry), flags));
        }
    }
}

/// The root page's control rows: model (opens the full list), thinking, mode, features.
fn settings_rows(
    rows: &mut Vec<Row>,
    model: &ControlsModel,
    labels: &ModelLabels,
    features: &[Value],
) {
    let provider = owned(get(model.config, "provider"));
    let mut row = Row::new(t("modelSelector.model"));
    row.leading = Leading::Provider(
        provider,
        model
            .provider()
            .map(|entry| owned(get(entry, "iconSvg")))
            .unwrap_or_default(),
    );
    row.value = model.selected_model_label(labels);
    row.chevron = true;
    row.action = Some(RowAction::OpenModels);
    rows.push(row);
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
            if value == Value::Bool(true) {
                t("agentControls.features.on")
            } else {
                t("agentControls.features.off")
            }
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

/// What every model row of one build reads: the controller (profiles), the saved picks and
/// the model in use.
struct RowCtx<'a> {
    ctl: &'a Controller,
    picks: &'a ModelPicks,
    selected: Option<ModelRef>,
}

fn model_row(rc: &RowCtx, pick: &Pick, flags: RowFlags) -> Row {
    let provider_id = pick.r.provider.clone();
    let model_id = pick.r.model.clone();
    let mut row = Row::new(pick.label());
    row.leading = Leading::Provider(provider_id.clone(), pick.provider_icon());
    row.check_slot = true;
    row.key = format!("{}:{provider_id}/{model_id}", flags.section);
    let qualifier = if flags.qualified {
        format!("{} · ", pick.provider_label())
    } else {
        String::new()
    };
    match pick.availability {
        Availability::Ready => {
            row.description = format!("{qualifier}{model_id}");
            if let Some(description) = pick
                .model
                .as_ref()
                .and_then(|m| get(m, "description").as_str())
                .filter(|s| !s.is_empty() && *s != model_id.as_str())
            {
                row.description.push_str(" · ");
                row.description.push_str(description);
            }
            row.selected = rc.selected.as_ref() == Some(&pick.r);
            row.action = Some(RowAction::SelectModel {
                provider: provider_id.clone(),
                model: model_id.clone(),
            });
        }
        Availability::Loading => {
            row.disabled = true;
            row.loading = true;
            row.description = format!("{qualifier}{model_id}");
            row.value = t("modelSelector.loadingShort");
        }
        Availability::Unavailable => {
            row.disabled = true;
            row.description = format!("{qualifier}{}", t("modelSelector.unavailable"));
        }
    }
    let favorite = rc.picks.is_favorite(&provider_id, &model_id);
    row.trailing.push(Trailing {
        icon: "star",
        action: RowAction::ToggleFavorite {
            provider: provider_id.clone(),
            model: model_id.clone(),
        },
        label: t(if favorite {
            "modelSelector.removeFavorite"
        } else {
            "modelSelector.addFavorite"
        }),
        active: favorite,
    });
    // model-browser.tsx SelectableModelRow profileAction.
    if flags.profiles && pick.selectable() && !model_id.is_empty() {
        let profiled: Vec<&Value> = rc
            .ctl
            .profiles
            .iter()
            .filter(|p| s(get(p, "provider")) == provider_id && s(get(p, "model")) == model_id)
            .collect();
        row.trailing.push(match profiled.len() {
            0 => Trailing {
                icon: "plus",
                action: RowAction::CreateProfileFromModel(serde_json::json!({
                    "provider": provider_id,
                    "model": model_id,
                    "name": pick.label(),
                })),
                label: t("modelSelector.createProfileFromModel"),
                active: false,
            },
            1 => Trailing {
                icon: "pencil",
                action: RowAction::EditProfile(profiled[0].clone()),
                label: t_args(
                    "modelSelector.editProfileLabel",
                    &[("name", s(get(profiled[0], "name")))],
                ),
                active: false,
            },
            n => {
                let count = n.to_string();
                Trailing {
                    icon: "pencil",
                    action: RowAction::OpenProfiles,
                    label: t_args(
                        "modelSelector.editProfilesCount",
                        &[("count", count.as_str())],
                    ),
                    active: false,
                }
            }
        });
    }
    row
}
