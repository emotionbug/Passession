//! Model selector favorites and recents, and the pure projection the model sheet draws from
//! them: the root page's quick switch, the full list's Favorites / Recent / provider groups,
//! and cross-provider search.
//!
//! Both lists persist in the client preference [`MODEL_PICKS_KEY`], per host:
//! `{ "<hostId>": { "favorites": [ref], "recents": [ref] } }` with each `ref`
//! `{ "provider": "<id>", "model": "<id>" }`. Favorites survive a model leaving the catalog
//! (they show as unavailable until unpinned); recents only ever show what can be picked now.

use super::controls_model::{model_label, models_of};
use super::json::*;
use serde_json::{Value, json};
use std::collections::HashSet;

/// Client preference holding the per-host favorites and recents.
pub const MODEL_PICKS_KEY: &str = "modelSelectorPicks";
/// Recent rows shown after favorites are removed from them.
pub const RECENT_SHOWN: usize = 5;
/// Recents kept on disk, so pinning a few still leaves a full recent list.
const RECENT_KEPT: usize = 10;
/// Rows of the root page's quick switch.
pub const QUICK_MAX: usize = 6;
/// Provider groups longer than this start collapsed in the full list unless they hold the
/// current selection.
pub const COLLAPSE_AT: usize = 8;

/// One model of one provider, as the preference stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn parse(value: &Value) -> Option<Self> {
        let provider = s(get(value, "provider")).trim();
        let model = s(get(value, "model")).trim();
        (!provider.is_empty() && !model.is_empty()).then(|| Self::new(provider, model))
    }

    fn to_value(&self) -> Value {
        json!({"provider": self.provider, "model": self.model})
    }
}

/// One host's favorites (pin order) and recents (most recent first).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelPicks {
    pub favorites: Vec<ModelRef>,
    pub recents: Vec<ModelRef>,
}

impl ModelPicks {
    /// `host`'s picks from the whole preference value; malformed entries are skipped.
    pub fn read(preference: &Value, host: &str) -> Self {
        let entry = get(preference, host);
        let refs = |key: &str| {
            let mut out: Vec<ModelRef> = Vec::new();
            for r in list_ref(get(entry, key)).iter().filter_map(ModelRef::parse) {
                if !out.contains(&r) {
                    out.push(r);
                }
            }
            out
        };
        Self {
            favorites: refs("favorites"),
            recents: refs("recents"),
        }
    }

    pub fn is_favorite(&self, provider: &str, model: &str) -> bool {
        self.favorites
            .iter()
            .any(|r| r.provider == provider && r.model == model)
    }

    /// The whole preference value with this host's entry replaced; other hosts are kept.
    fn write(&self, preference: &Value, host: &str) -> Value {
        let mut whole = obj(preference);
        whole.insert(
            host.to_owned(),
            json!({
                "favorites": self.favorites.iter().map(ModelRef::to_value).collect::<Vec<_>>(),
                "recents": self.recents.iter().map(ModelRef::to_value).collect::<Vec<_>>(),
            }),
        );
        Value::Object(whole)
    }

    /// These picks with `current`, the model in use, counted as the most recent one when no
    /// recent lists it yet: once a model is in use the quick switch is never empty.
    pub fn seeded(mut self, current: Option<&ModelRef>) -> Self {
        if let Some(current) = current
            && !self.recents.contains(current)
        {
            self.recents.insert(0, current.clone());
        }
        self
    }
}

/// The preference value with `model` pinned on `host`, or unpinned when it already was.
pub fn toggle_favorite(preference: &Value, host: &str, model: &ModelRef) -> Value {
    let mut picks = ModelPicks::read(preference, host);
    match picks.favorites.iter().position(|r| r == model) {
        Some(index) => {
            picks.favorites.remove(index);
        }
        None => picks.favorites.push(model.clone()),
    }
    picks.write(preference, host)
}

/// The preference value with `model` moved to the front of `host`'s recents. `previous`, the
/// model in use before this choice, is kept behind it when it was not listed yet, so the
/// model a session started with stays one tap away.
pub fn record_recent(
    preference: &Value,
    host: &str,
    previous: Option<&ModelRef>,
    model: &ModelRef,
) -> Value {
    let mut picks = ModelPicks::read(preference, host).seeded(previous);
    picks.recents.retain(|r| r != model);
    picks.recents.insert(0, model.clone());
    picks.recents.truncate(RECENT_KEPT);
    picks.write(preference, host)
}

/// What a provider entry can offer right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderState {
    Ready,
    Loading,
    Failed,
    Disabled,
}

pub fn provider_state(provider: &Value) -> ProviderState {
    if get(provider, "enabled") == &Value::Bool(false) {
        return ProviderState::Disabled;
    }
    if !models_of(provider).is_empty() {
        return ProviderState::Ready;
    }
    let status = s(get(provider, "status"));
    if status == "loading" && get(provider, "error").is_null() {
        ProviderState::Loading
    } else if !get(provider, "error").is_null() || matches!(status, "error" | "unavailable") {
        ProviderState::Failed
    } else {
        ProviderState::Ready
    }
}

/// Whether a remembered model can be picked now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Ready,
    /// Its provider is still listing models.
    Loading,
    /// The provider is disabled, failed, gone, or no longer lists the model.
    Unavailable,
}

/// A model row of the sheet: the reference, its catalog entries when present. The entries are
/// borrowed from the provider catalog, which a model list reads once per row on every draw.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick<'a> {
    pub r: ModelRef,
    pub provider: Option<&'a Value>,
    pub model: Option<&'a Value>,
    pub availability: Availability,
}

impl<'a> Pick<'a> {
    /// A catalog model of a ready provider.
    pub fn ready(provider: &'a Value, model: &'a Value) -> Self {
        Self {
            r: ModelRef::new(owned(get(provider, "provider")), owned(get(model, "id"))),
            provider: Some(provider),
            model: Some(model),
            availability: Availability::Ready,
        }
    }

    pub fn selectable(&self) -> bool {
        self.availability == Availability::Ready
    }

    pub fn label(&self) -> String {
        self.model
            .map(model_label)
            .unwrap_or_else(|| self.r.model.clone())
    }

    pub fn provider_label(&self) -> &str {
        self.provider
            .map(|p| s_or(get(p, "label"), &self.r.provider))
            .unwrap_or(&self.r.provider)
    }

    pub fn provider_icon(&self) -> String {
        self.provider
            .map(|p| owned(get(p, "iconSvg")))
            .unwrap_or_default()
    }

    /// Case-insensitive match of a lowercased query on label, id, provider and description.
    fn matches(&self, query: &str) -> bool {
        let description = self.model.map_or("", |m| s(get(m, "description")));
        [
            self.label().as_str(),
            self.r.model.as_str(),
            self.provider_label(),
            description,
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(query))
    }
}

fn find_provider<'a>(providers: &'a [Value], id: &str) -> Option<(usize, &'a Value)> {
    providers
        .iter()
        .enumerate()
        .find(|(_, p)| s(get(p, "provider")) == id)
}

fn resolve<'a>(providers: &'a [Value], r: &ModelRef) -> Pick<'a> {
    let mut pick = Pick {
        r: r.clone(),
        provider: None,
        model: None,
        availability: Availability::Unavailable,
    };
    let Some((_, provider)) = find_provider(providers, &r.provider) else {
        return pick;
    };
    pick.provider = Some(provider);
    match provider_state(provider) {
        ProviderState::Loading => pick.availability = Availability::Loading,
        ProviderState::Ready => {
            if let Some(model) = models_of(provider)
                .into_iter()
                .find(|m| s(get(m, "id")) == r.model)
            {
                pick.model = Some(model);
                pick.availability = Availability::Ready;
            }
        }
        ProviderState::Failed | ProviderState::Disabled => {}
    }
    pick
}

/// Catalog position of a pick: provider order, then model order; unresolved picks after.
fn rank(providers: &[Value], pick: &Pick) -> (usize, usize) {
    let Some((provider_index, provider)) = find_provider(providers, &pick.r.provider) else {
        return (usize::MAX, usize::MAX);
    };
    if pick.availability == Availability::Unavailable {
        return (usize::MAX, usize::MAX);
    }
    let model_index = models_of(provider)
        .iter()
        .position(|m| s(get(m, "id")) == pick.r.model)
        .unwrap_or(usize::MAX);
    (provider_index, model_index)
}

/// One provider of the full list.
#[derive(Clone, Debug, PartialEq)]
pub struct Group<'a> {
    pub provider: &'a Value,
    pub state: ProviderState,
    pub models: Vec<&'a Value>,
}

impl Group<'_> {
    pub fn id(&self) -> &str {
        s(get(self.provider, "provider"))
    }

    pub fn label(&self) -> &str {
        s_or(get(self.provider, "label"), self.id())
    }
}

/// The full list's sections. `favorites` keeps unavailable pins (flagged) after the
/// available ones in catalog order; `recents` holds only selectable, unpinned models.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sections<'a> {
    pub favorites: Vec<Pick<'a>>,
    pub recents: Vec<Pick<'a>>,
    pub groups: Vec<Group<'a>>,
}

/// Whether `provider` belongs to the sheet: `scope` is the one provider a running agent may
/// still use, `None` when any provider can be chosen.
fn in_scope(scope: Option<&str>, provider: &str) -> bool {
    scope.is_none_or(|only| only == provider)
}

pub fn sections<'a>(
    providers: &'a [Value],
    picks: &ModelPicks,
    scope: Option<&str>,
) -> Sections<'a> {
    let groups = providers
        .iter()
        .filter(|p| in_scope(scope, s(get(p, "provider"))))
        .map(|provider| {
            let state = provider_state(provider);
            Group {
                provider,
                state,
                models: if state == ProviderState::Ready {
                    models_of(provider)
                } else {
                    Vec::new()
                },
            }
        })
        .collect();
    let mut favorites: Vec<Pick> = picks
        .favorites
        .iter()
        .filter(|r| in_scope(scope, &r.provider))
        .map(|r| resolve(providers, r))
        .collect();
    favorites.sort_by_key(|pick| rank(providers, pick));
    let recents = picks
        .recents
        .iter()
        .filter(|r| in_scope(scope, &r.provider) && !picks.favorites.contains(r))
        .map(|r| resolve(providers, r))
        .filter(Pick::selectable)
        .take(RECENT_SHOWN)
        .collect();
    Sections {
        favorites,
        recents,
        groups,
    }
}

/// Which list the root page's quick switch shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuickKind {
    Favorites,
    Recents,
}

/// The root page's one-tap rows: selectable favorites, or recents when no favorite can be
/// picked; `None` when there is nothing to offer.
pub fn quick_switch<'a>(sections: &Sections<'a>) -> Option<(QuickKind, Vec<Pick<'a>>)> {
    let favorites: Vec<Pick> = sections
        .favorites
        .iter()
        .filter(|p| p.selectable())
        .take(QUICK_MAX)
        .cloned()
        .collect();
    if !favorites.is_empty() {
        return Some((QuickKind::Favorites, favorites));
    }
    (!sections.recents.is_empty()).then(|| (QuickKind::Recents, sections.recents.clone()))
}

/// Cross-provider search: selectable matches, favorites first, then recents, then the rest
/// in catalog order, each model once. `query` is trimmed and compared case-insensitively.
pub fn search<'a>(
    providers: &'a [Value],
    picks: &ModelPicks,
    scope: Option<&str>,
    query: &str,
) -> Vec<Pick<'a>> {
    let query = query.trim().to_lowercase();
    let sections = sections(providers, picks, scope);
    let mut out: Vec<Pick> = Vec::new();
    let mut seen = HashSet::new();
    let catalog = sections.groups.iter().flat_map(|group| {
        group
            .models
            .iter()
            .map(|model| Pick::ready(group.provider, *model))
    });
    let candidates = sections
        .favorites
        .iter()
        .filter(|pick| pick.selectable())
        .cloned()
        .chain(sections.recents.iter().cloned())
        .chain(catalog);
    for pick in candidates {
        if pick.matches(&query) && seen.insert(pick.r.clone()) {
            out.push(pick);
        }
    }
    out
}

/// Whether a provider group starts collapsed: only long groups among several, and never the
/// one holding the current selection.
pub fn starts_collapsed(group_len: usize, holds_selection: bool, group_count: usize) -> bool {
    group_count > 1 && group_len > COLLAPSE_AT && !holds_selection
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, models: &[&str]) -> Value {
        json!({
            "provider": id,
            "label": id.to_uppercase(),
            "status": "ready",
            "models": models
                .iter()
                .map(|m| json!({"id": m, "label": format!("{m} label")}))
                .collect::<Vec<_>>(),
        })
    }

    fn picks(favorites: &[(&str, &str)], recents: &[(&str, &str)]) -> ModelPicks {
        let refs = |list: &[(&str, &str)]| {
            list.iter()
                .map(|(p, m)| ModelRef::new(*p, *m))
                .collect::<Vec<_>>()
        };
        ModelPicks {
            favorites: refs(favorites),
            recents: refs(recents),
        }
    }

    fn ids(list: &[Pick]) -> Vec<String> {
        list.iter()
            .map(|p| format!("{}/{}", p.r.provider, p.r.model))
            .collect()
    }

    #[test]
    fn favorites_toggle_and_persist_per_host() {
        let opus = ModelRef::new("claude", "opus");
        let pref = toggle_favorite(&Value::Null, "host-a", &opus);
        let pref = toggle_favorite(&pref, "host-b", &ModelRef::new("codex", "gpt"));
        assert!(ModelPicks::read(&pref, "host-a").is_favorite("claude", "opus"));
        assert!(!ModelPicks::read(&pref, "host-b").is_favorite("claude", "opus"));
        let pref = toggle_favorite(&pref, "host-a", &opus);
        assert!(ModelPicks::read(&pref, "host-a").favorites.is_empty());
        assert_eq!(ModelPicks::read(&pref, "host-b").favorites.len(), 1);
    }

    #[test]
    fn malformed_and_duplicate_entries_are_skipped() {
        let pref = json!({"h": {"favorites": [
            {"provider": "a", "model": "x"},
            {"provider": "a", "model": "x"},
            {"provider": "", "model": "y"},
            "junk",
        ]}});
        assert_eq!(
            ModelPicks::read(&pref, "h").favorites,
            vec![ModelRef::new("a", "x")]
        );
        assert_eq!(ModelPicks::read(&json!("bad"), "h"), ModelPicks::default());
    }

    #[test]
    fn recents_move_to_front_dedupe_and_cap() {
        let mut pref = Value::Null;
        for index in 0..12 {
            let model = ModelRef::new("p", format!("m{index}"));
            pref = record_recent(&pref, "h", None, &model);
        }
        pref = record_recent(&pref, "h", None, &ModelRef::new("p", "m5"));
        let recents = ModelPicks::read(&pref, "h").recents;
        assert_eq!(recents.len(), RECENT_KEPT);
        assert_eq!(recents[0].model, "m5");
        assert_eq!(recents[1].model, "m11");
        assert_eq!(recents.iter().filter(|r| r.model == "m5").count(), 1);
    }

    #[test]
    fn current_model_seeds_recents_and_survives_the_next_choice() {
        let providers = [provider("a", &["1", "2", "3"])];
        let current = ModelRef::new("a", "1");
        // First use: nothing saved, the model in use still fills the quick switch.
        let s = sections(
            &providers,
            &ModelPicks::default().seeded(Some(&current)),
            None,
        );
        let (kind, rows) = quick_switch(&s).unwrap();
        assert_eq!(kind, QuickKind::Recents);
        assert_eq!(ids(&rows), ["a/1"]);
        // Seeding never reorders an already listed model and still yields to favorites.
        let p = picks(&[("a", "1")], &[("a", "2")]).seeded(Some(&current));
        assert_eq!(ids(&sections(&providers, &p, None).recents), ["a/2"]);
        let p = picks(&[], &[("a", "2"), ("a", "1")]).seeded(Some(&current));
        assert_eq!(ids(&sections(&providers, &p, None).recents), ["a/2", "a/1"]);
        // Choosing another model keeps the one the session started with behind it.
        let pref = record_recent(&Value::Null, "h", Some(&current), &ModelRef::new("a", "3"));
        let recents = ModelPicks::read(&pref, "h").recents;
        assert_eq!(recents, [ModelRef::new("a", "3"), current.clone()]);
        let pref = record_recent(&pref, "h", Some(&ModelRef::new("a", "3")), &current);
        assert_eq!(
            ModelPicks::read(&pref, "h").recents,
            [current, ModelRef::new("a", "3")]
        );
    }

    #[test]
    fn favorites_follow_catalog_order_with_unavailable_last() {
        let providers = [provider("a", &["a1", "a2"]), provider("b", &["b1"])];
        let p = picks(
            &[
                ("b", "b1"),
                ("gone", "x"),
                ("a", "a2"),
                ("a", "removed"),
                ("a", "a1"),
            ],
            &[],
        );
        let sections = sections(&providers, &p, None);
        assert_eq!(
            ids(&sections.favorites),
            ["a/a1", "a/a2", "b/b1", "gone/x", "a/removed"]
        );
        let flags: Vec<bool> = sections.favorites.iter().map(Pick::selectable).collect();
        assert_eq!(flags, [true, true, true, false, false]);
    }

    #[test]
    fn recents_dedupe_against_favorites_drop_unavailable_and_cap() {
        let providers = [provider("a", &["1", "2", "3", "4", "5", "6", "7", "8"])];
        let recents: Vec<(&str, &str)> = ["2", "gone", "1", "3", "4", "5", "6", "7"]
            .iter()
            .map(|m| ("a", *m))
            .collect();
        let p = picks(&[("a", "2")], &recents);
        let sections = sections(&providers, &p, None);
        assert_eq!(ids(&sections.recents), ["a/1", "a/3", "a/4", "a/5", "a/6"]);
    }

    #[test]
    fn loading_and_failed_providers_flag_their_favorites() {
        let loading = json!({"provider": "l", "status": "loading", "models": []});
        let failed = json!({"provider": "f", "status": "error", "error": "boom", "models": []});
        let disabled = json!({"provider": "d", "enabled": false, "status": "ready",
            "models": [{"id": "x"}]});
        let providers = [loading, failed, disabled];
        let p = picks(&[("l", "x"), ("f", "x"), ("d", "x")], &[("l", "y")]);
        let s = sections(&providers, &p, None);
        let states: Vec<Availability> = s.favorites.iter().map(|p| p.availability).collect();
        assert_eq!(
            states,
            [
                Availability::Loading,
                Availability::Unavailable,
                Availability::Unavailable
            ]
        );
        assert!(s.recents.is_empty());
        let groups: Vec<ProviderState> = s.groups.iter().map(|g| g.state).collect();
        assert_eq!(
            groups,
            [
                ProviderState::Loading,
                ProviderState::Failed,
                ProviderState::Disabled
            ]
        );
        assert!(s.groups.iter().all(|g| g.models.is_empty()));
        assert_eq!(quick_switch(&s), None);
    }

    #[test]
    fn quick_switch_prefers_selectable_favorites_then_recents() {
        let providers = [provider("a", &["1", "2", "3", "4", "5", "6", "7", "8"])];
        let all: Vec<(&str, &str)> = ["1", "2", "3", "4", "5", "6", "7", "8"]
            .iter()
            .map(|m| ("a", *m))
            .collect();
        let s = sections(&providers, &picks(&all, &[]), None);
        let (kind, rows) = quick_switch(&s).unwrap();
        assert_eq!(kind, QuickKind::Favorites);
        assert_eq!(rows.len(), QUICK_MAX);

        let s = sections(&providers, &picks(&[("a", "gone")], &[("a", "3")]), None);
        let (kind, rows) = quick_switch(&s).unwrap();
        assert_eq!(kind, QuickKind::Recents);
        assert_eq!(ids(&rows), ["a/3"]);
    }

    #[test]
    fn scope_limits_every_section_to_one_provider() {
        let providers = [provider("a", &["1"]), provider("b", &["1"])];
        let p = picks(&[("a", "1"), ("b", "1")], &[("b", "1"), ("a", "1")]);
        let s = sections(&providers, &picks(&[("b", "1")], &[("a", "1")]), Some("a"));
        assert!(s.favorites.is_empty());
        assert_eq!(ids(&s.recents), ["a/1"]);
        assert_eq!(s.groups.len(), 1);
        assert_eq!(ids(&search(&providers, &p, Some("b"), "1")), ["b/1"]);
    }

    #[test]
    fn search_spans_sections_once_with_favorites_and_recents_first() {
        let providers = [
            provider("claude", &["opus", "sonnet", "haiku"]),
            provider("codex", &["gpt-opus", "mini"]),
        ];
        let p = picks(
            &[("codex", "gpt-opus")],
            &[("claude", "sonnet"), ("claude", "opus")],
        );
        assert_eq!(
            ids(&search(&providers, &p, None, "  OPUS ")),
            ["codex/gpt-opus", "claude/opus"]
        );
        // Provider labels match too; favorites and recents lead, nothing repeats.
        assert_eq!(
            ids(&search(&providers, &p, None, "claude")),
            ["claude/sonnet", "claude/opus", "claude/haiku"]
        );
        assert!(search(&providers, &p, None, "nothing").is_empty());
    }

    #[test]
    fn search_skips_unavailable_models() {
        let providers = [
            provider("a", &["opus"]),
            json!({"provider": "b", "status": "error", "error": "x", "models": []}),
            json!({"provider": "c", "enabled": false, "status": "ready",
                "models": [{"id": "opus"}]}),
        ];
        let p = picks(&[("b", "opus"), ("a", "gone-opus")], &[]);
        assert_eq!(ids(&search(&providers, &p, None, "opus")), ["a/opus"]);
    }

    #[test]
    fn long_groups_collapse_unless_selected_or_alone() {
        assert!(starts_collapsed(COLLAPSE_AT + 1, false, 2));
        assert!(!starts_collapsed(COLLAPSE_AT + 1, true, 2));
        assert!(!starts_collapsed(COLLAPSE_AT + 1, false, 1));
        assert!(!starts_collapsed(COLLAPSE_AT, false, 3));
    }
}
