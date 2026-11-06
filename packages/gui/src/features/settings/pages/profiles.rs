//! Agent profiles (`AgentProfilesSection`, `agent-profile-edit-modal.tsx`) and
//! metadata generation (`metadata-generation-page.tsx`), ported from Flutter
//! `profile_settings.dart`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use makepad_widgets::*;
use serde_json::{Value, json};

use super::View;
use super::daemon::{self, data};
use crate::features::settings::host::HostView;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, get, obj, str_of, str_or};
use crate::features::settings::model::profile_form::{Display, Opt, ProfileForm, Resolution, apply_profile_save, generate_profile_id, mode_label, thinking_label};
use crate::features::settings::model::provider_catalog::provider_entries;
use crate::features::settings::rpc::{Env, Handler, ReqError, handler};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon, PROFILE_ICONS, profile_icon};
use crate::features::settings::ui::field::FieldSpec;
use crate::features::settings::ui::kit::{BSize, Ctl, Lead, Ui, Variant, button, danger_icon_button, icon_button, row, switch};
use crate::features::settings::ui::overlay::{Menu, MenuItem, SheetHead, item};
use crate::features::settings::ui::theme::{IDENTITY_COLORS, alpha, identity_foreground};

fn ap(key: &str) -> String {
    t(&format!("settings.host.agentProfiles.{key}"))
}

/// Original `useAgentProfiles().isSupported`.
pub fn supports_profiles(host: &HostView) -> bool {
    host.feature("agentProfiles") && host.feature("agentConfigApply")
}

fn glyph(icon: &str, color: &str, dark: bool, muted: Vec4, size: f64) -> Ctl {
    Ctl::Glyph(profile_icon(icon), size, identity_foreground(color, dark).unwrap_or(muted))
}

/// Original `buildAgentProfileTags`.
fn summary(providers: &[Value], profile: &Value) -> String {
    let provider_id = str_of(profile, "provider");
    let empty = json!({});
    let entry = providers.iter().find(|e| str_of(e, "provider") == provider_id).unwrap_or(&empty);
    let mut tags = vec![str_or(entry, "label", provider_id).to_string()];
    let model = str_of(profile, "model").trim();
    if !model.is_empty() {
        let found = arr(entry, "models").iter().find(|m| str_of(m, "id") == model);
        tags.push(found.map(|m| str_or(m, "label", model).to_string()).unwrap_or_else(|| model.to_string()));
    }
    let mode = str_of(profile, "modeId").trim();
    if !mode.is_empty() {
        let found = arr(entry, "modes").iter().find(|m| str_of(m, "id") == mode);
        tags.push(found.map(mode_label).unwrap_or_else(|| mode.to_string()));
    }
    let thinking = str_of(profile, "thinkingOptionId").trim();
    if !thinking.is_empty() {
        tags.push(thinking_label(&json!({"id": thinking})));
    }
    let features = obj(profile, "featureValues").len();
    if features > 0 {
        let key = if features == 1 { "settings.host.agentProfiles.featureCountOne" } else { "settings.host.agentProfiles.featureCount" };
        tags.push(ta(key, &[("count", &features.to_string())]));
    }
    tags.into_iter().filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" · ")
}

fn save_profiles(s: &mut Settings, env: &mut Env, host: &str, next: Vec<Value>) {
    daemon::patch(s, env, host, json!({"agentProfiles": next}), |_, env, result| {
        if let Err(error) = result {
            env.error(&error);
        }
    });
}

pub fn draw_profiles(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    let connected = host.connected();
    if !connected || !supports_profiles(host) {
        ui.section_title(&ap("sectionTitle"), None, None);
        ui.empty_card(&ap(if connected { "unsupported" } else { "unavailable" }), None, false);
        ui.gap(24.0);
        return;
    }
    let pal = ui.pal;
    let id = host.server_id.clone();
    let d = data(s, &id);
    let profiles: Vec<Value> = d.map(|d| arr(&d.config, "agentProfiles").to_vec()).unwrap_or_default();
    let providers: Vec<Value> = d.map(|d| d.providers.clone()).unwrap_or_default();
    let add_id = id.clone();
    let add = handler(move |s, env| open_profile(s, env, &add_id, None, None));
    ui.section_title(&ap("sectionTitle"), None, Some(icon_button(format!("profiles.{id}.add"), Icon::Plus, Some(add.clone())).tipped(ap("addProfileTitle"))));
    if profiles.is_empty() {
        let action = button(format!("profiles.{id}.new"), ap("newProfile"), Some(add)).icon(Icon::Plus).variant(Variant::Secondary).ctl();
        ui.empty_card(&ap("emptyState"), Some(action), true);
        ui.gap(24.0);
        return;
    }
    ui.card_begin();
    for (i, profile) in profiles.iter().enumerate() {
        let pid = str_of(profile, "id").to_string();
        let cli_default = profile.get("cliDefault").and_then(Value::as_bool) == Some(true);
        let mk = |next: Vec<Value>| {
            let host = id.clone();
            handler(move |s, env| save_profiles(s, env, &host, next.clone()))
        };
        let toggled: Vec<Value> = profiles
            .iter()
            .map(|p| {
                let mut p = p.clone();
                p["cliDefault"] = Value::Bool(str_of(&p, "id") == pid && !cli_default);
                p
            })
            .collect();
        let cli = button(format!("profiles.{id}.{pid}.cli"), ap(if cli_default { "cliDefault" } else { "useAsCliDefault" }), Some(mk(toggled)))
            .variant(if cli_default { Variant::Secondary } else { Variant::Ghost })
            .size(if ui.compact { BSize::Md } else { BSize::Sm })
            .ctl();
        let up = (i > 0).then(|| {
            let mut next = profiles.clone();
            next.swap(i - 1, i);
            mk(next)
        });
        let down = (i + 1 < profiles.len()).then(|| {
            let mut next = profiles.clone();
            next.swap(i, i + 1);
            mk(next)
        });
        let edit_id = id.clone();
        let edit_profile = profile.clone();
        let edit = handler(move |s, env| open_profile(s, env, &edit_id, Some(edit_profile.clone()), None));
        let name = str_of(profile, "name").to_string();
        let remaining: Vec<Value> = profiles.iter().filter(|p| str_of(p, "id") != pid).cloned().collect();
        let remove_host = id.clone();
        let remove = handler(move |s, _| {
            let host = remove_host.clone();
            let remaining = remaining.clone();
            let go = handler(move |s, env| save_profiles(s, env, &host, remaining.clone()));
            s.confirm(ap("removeConfirmTitle"), ta("settings.host.agentProfiles.removeConfirmMessage", &[("name", &name)]), Some(ap("remove")), true, go);
        });
        let mut actions = vec![
            icon_button(format!("profiles.{id}.{pid}.up"), Icon::ArrowUp, up).tipped(ap("moveUp")),
            icon_button(format!("profiles.{id}.{pid}.down"), Icon::ArrowDown, down).tipped(ap("moveDown")),
            icon_button(format!("profiles.{id}.{pid}.edit"), Icon::Pencil, Some(edit)).tipped(ap("editProfile")),
            danger_icon_button(format!("profiles.{id}.{pid}.remove"), Icon::Trash, Some(remove)).tipped(ap("remove")),
        ];
        if ui.compact {
            for action in &mut actions {
                if let Ctl::IconButton { size, .. } = action {
                    *size = 48.0;
                }
            }
        }
        let actions = Ctl::Row(actions, 0.0);
        let notes = str_of(profile, "notes").to_string();
        let lead = glyph(str_of(profile, "icon"), str_of(profile, "color"), pal.dark, pal.muted, 16.0);
        let tags = summary(&providers, profile);
        if ui.compact {
            // The identity and metadata need the card width, not the space
            // left over after the CLI action and four icon buttons.
            let hint = if notes.is_empty() { tags } else if tags.is_empty() { notes } else { format!("{tags}\n{notes}") };
            ui.row(row(str_of(profile, "name")).leading(lead).hint(hint).padding(12.0));
            let controls = Ctl::Col(vec![cli, actions], 8.0, false);
            let size = ui.draw_ctl(controls, dvec2(ui.x + 12.0, ui.y));
            ui.gap(size.y + 12.0);
        } else {
            let controls = Ctl::Row(vec![cli, actions], 0.0);
            let mut r = row(str_of(profile, "name").to_string())
                .one_line()
                .leading(lead)
                .accessory(Ctl::Label { text: tags, color: pal.muted, size: 12.0, font: Font::Regular, max_w: 260.0 })
                .inline()
                .padding(12.0)
                .control(controls);
            if !notes.is_empty() {
                r = r.hint(notes);
            }
            ui.row(r);
        }
    }
    ui.card_end();
    ui.gap(24.0);
}

// ------------------------------------------------------------ profile editor

pub struct ProfileSheet {
    pub host: String,
    pub form: ProfileForm,
    pub original: Option<Value>,
    pub feature_cache: HashMap<String, (Instant, Option<Vec<Value>>)>,
    pub requested_key: Option<String>,
    pub catalog_requested: Option<bool>,
}

/// Opens the profile editor sheet: `profile` edits, else `seed`
/// (`{provider, model|modelId, name}`) pre-fills a new profile.
pub fn open_editor(s: &mut Settings, env: &mut Env, host: &str, profile: Option<Value>, seed: Option<Value>) {
    open_profile(s, env, host, profile, seed);
}

fn open_profile(s: &mut Settings, env: &mut Env, host: &str, original: Option<Value>, seed: Option<Value>) {
    let id = s.next_id();
    let form = ProfileForm::new(original.as_ref(), seed.as_ref());
    let sheet = ProfileSheet { host: host.to_string(), form, original, feature_cache: HashMap::new(), requested_key: None, catalog_requested: None };
    s.push_sheet(id, SheetKind::Profile(Box::new(sheet)), 520.0);
    load_catalog(s, env, id);
}

fn profile_sheet(s: &mut Settings, id: u64) -> Option<&mut ProfileSheet> {
    match s.sheet_mut(id) {
        Some(SheetKind::Profile(p)) => Some(&mut **p),
        _ => None,
    }
}

thread_local! {
    /// Host-wide provider snapshots shared by every settings panel (the page
    /// and embedded editors), as Flutter's editor read the host's snapshot.
    static CATALOGS: std::cell::RefCell<HashMap<String, Vec<Value>>> = std::cell::RefCell::new(HashMap::new());
}

/// Records `host`'s latest provider snapshot entries and applies them to
/// every open profile editor of that host.
pub fn remember_catalog(s: &mut Settings, env: &mut Env, host: &str, entries: Vec<Value>) {
    CATALOGS.with(|c| c.borrow_mut().insert(host.to_string(), entries.clone()));
    let ids: Vec<u64> = s.sheets.iter().filter(|sheet| matches!(&sheet.kind, SheetKind::Profile(p) if p.host == host)).map(|sheet| sheet.id).collect();
    for id in ids {
        if let Some(p) = profile_sheet(s, id) {
            p.form.apply_catalog(entries.clone());
        }
        load_features(s, env, id);
    }
}

/// A `providers_snapshot_update` for `host`: the editors follow the live
/// snapshot; an update without entries re-reads it.
pub fn on_snapshot_update(s: &mut Settings, env: &mut Env, host: &str, payload: &Value) {
    if payload.get("entries").is_some() || payload.get("compactSnapshot").is_some() {
        remember_catalog(s, env, host, provider_entries(payload));
        return;
    }
    let ids: Vec<u64> = s.sheets.iter().filter(|sheet| matches!(&sheet.kind, SheetKind::Profile(p) if p.host == host)).map(|sheet| sheet.id).collect();
    for id in ids {
        load_catalog(s, env, id);
    }
}

/// Original `useAgentProfileFormCatalog`: host-wide snapshot; without one the
/// provider select stays loading (retried when the host reconnects).
fn load_catalog(s: &mut Settings, env: &mut Env, id: u64) {
    let Some(p) = profile_sheet(s, id) else { return };
    let host = p.host.clone();
    if let Some(entries) = CATALOGS.with(|c| c.borrow().get(&host).cloned()) {
        p.form.apply_catalog(entries);
        load_features(s, env, id);
    }
    let Some(p) = profile_sheet(s, id) else { return };
    let connected = env.host(&host).map(|h| h.connected()).unwrap_or(false);
    p.catalog_requested = Some(connected);
    if !connected {
        return;
    }
    let snapshot_host = host.clone();
    s.request(env, &host, "get_providers_snapshot_request", json!({}), "get_providers_snapshot_response", move |s, env, result| {
        if let Ok(snapshot) = result {
            remember_catalog(s, env, &snapshot_host, provider_entries(&snapshot));
        }
    });
}

/// Original `useAgentProfileFormFeatures`: one request per key, cached for
/// five minutes, never retried; an error resolves the key to no features.
fn load_features(s: &mut Settings, env: &mut Env, id: u64) {
    let Some(p) = profile_sheet(s, id) else { return };
    let (Some(key), Some(request)) = (p.form.feature_key.clone(), p.form.feature_request.clone()) else { return };
    if p.requested_key.as_deref() == Some(key.as_str()) {
        return;
    }
    let host = p.host.clone();
    if !env.host(&host).map(|h| h.connected()).unwrap_or(false) {
        return;
    }
    p.requested_key = Some(key.clone());
    if let Some((at, cached)) = p.feature_cache.get(&key).cloned()
        && at.elapsed() < Duration::from_secs(300)
    {
        match cached {
            Some(features) => p.form.apply_features(&key, features),
            None => p.form.apply_features_unavailable(&key),
        }
        return;
    }
    let mut draft = request;
    draft["cwd"] = Value::String("~".into());
    s.request(env, &host, "list_provider_features_request", json!({"draftConfig": draft}), "list_provider_features_response", move |s, env, result| {
        let Some(p) = profile_sheet(s, id) else { return };
        match result {
            Ok(r) => {
                let features = arr(&r, "features").to_vec();
                p.feature_cache.insert(key.clone(), (Instant::now(), Some(features.clone())));
                p.form.apply_features(&key, features);
            }
            Err(_) => {
                p.feature_cache.insert(key.clone(), (Instant::now(), None));
                p.form.apply_features_unavailable(&key);
            }
        }
        load_features(s, env, id);
    });
}

/// Sheet hook after every change: the next feature key loads.
fn changed(s: &mut Settings, env: &mut Env, id: u64) {
    load_features(s, env, id);
}

/// Field edits: open profile sheets mirror their name/notes into the form so
/// Save enables as soon as a name is typed (Flutter `onChanged: model.setName`).
pub fn sync_fields(s: &mut Settings, env: &mut Env) {
    let ids: Vec<u64> = s.sheets.iter().filter(|sh| matches!(sh.kind, SheetKind::Profile(_))).map(|sh| sh.id).collect();
    for id in ids {
        let name_id = format!("sheet.{id}.name");
        let notes_id = format!("sheet.{id}.notes");
        let name = env.fields.exists(&name_id).then(|| env.fields.text(&name_id));
        let notes = env.fields.exists(&notes_id).then(|| env.fields.text(&notes_id));
        if let Some(p) = profile_sheet(s, id) {
            if let Some(name) = name.filter(|n| *n != p.form.name) {
                p.form.set_name(&name);
            }
            if let Some(notes) = notes.filter(|n| *n != p.form.notes) {
                p.form.set_notes(&notes);
            }
        }
    }
}

/// Connection returned: retry an unresolved catalog and re-run feature loads.
pub fn on_reconnect(s: &mut Settings, env: &mut Env, host: &str) {
    let ids: Vec<u64> = s.sheets.iter().filter_map(|sh| match &sh.kind {
        SheetKind::Profile(p) if p.host == host => Some(sh.id),
        _ => None,
    }).collect();
    for id in ids {
        let unresolved = profile_sheet(s, id).map(|p| p.form.catalog != Resolution::Complete).unwrap_or(false);
        if unresolved {
            load_catalog(s, env, id);
        }
        if let Some(p) = profile_sheet(s, id) {
            p.requested_key = None;
        }
        load_features(s, env, id);
    }
}

fn save_profile(s: &mut Settings, env: &mut Env, id: u64) {
    let name = env.fields.text(&format!("sheet.{id}.name"));
    let notes = env.fields.text(&format!("sheet.{id}.notes"));
    let Some(p) = profile_sheet(s, id) else { return };
    p.form.set_name(&name);
    p.form.set_notes(&notes);
    let Some(value) = p.form.submit_value.clone() else { return };
    p.form.set_submit_error(None);
    p.form.set_submitting(true);
    let editing = p.original.is_some();
    let record_id = p.original.as_ref().and_then(|o| o.get("id").cloned()).unwrap_or_else(|| Value::String(generate_profile_id()));
    let mut record = serde_json::Map::new();
    record.insert("id".into(), record_id.clone());
    record.extend(value.as_object().cloned().unwrap_or_default());
    let record = Value::Object(record);
    let host = p.host.clone();
    let host2 = host.clone();
    let fail = move |s: &mut Settings, error: ReqError| {
        if let Some(p) = profile_sheet(s, id) {
            p.form.set_submitting(false);
            p.form.set_submit_error(Some(error.text()));
        }
    };
    // Re-read first so a concurrent host edit is not overwritten.
    s.request(env, &host, "get_daemon_config_request", json!({}), "get_daemon_config_response", move |s, env, fresh| {
        let fresh = match fresh {
            Ok(fresh) => fresh,
            Err(error) => return fail(s, error),
        };
        let profiles = arr(get(&fresh, "config"), "agentProfiles").to_vec();
        if editing && !profiles.iter().any(|p| p.get("id") == Some(&record_id)) {
            return fail(s, ReqError::msg(t("settingsNative.profileRemoved")));
        }
        let next = apply_profile_save(&profiles, &record, editing);
        let saved_host = host2.clone();
        daemon::patch(s, env, &host2, json!({"agentProfiles": next}), move |s, env, result| match result {
            Ok(()) => {
                s.saved_profile_host = Some(saved_host);
                s.close_sheet(env, id);
            }
            Err(error) => fail(s, error),
        });
    });
}

pub fn profile_head(id: u64, p: &ProfileSheet) -> SheetHead {
    let mut head = SheetHead::new(ap(if p.original.is_none() { "addProfileTitle" } else { "editProfileTitle" }));
    head.stretch_actions = true;
    let submitting = p.form.submitting;
    head.dismissible = !submitting;
    head.actions.push(button(format!("sheet.{id}.cancel"), t("common.actions.cancel"), (!submitting).then(|| handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    head.actions.push(
        button(format!("sheet.{id}.save"), ap(if submitting { "saving" } else { "save" }), p.form.can_submit.then(|| handler(move |s, env| save_profile(s, env, id))))
            .variant(Variant::Primary)
            .ctl(),
    );
    head
}

type Setter = fn(&mut ProfileForm, &str, Option<Display>);

/// One provider/model/mode/thinking dropdown of the profile editor.
struct SelectField<'f> {
    key: &'static str,
    label: String,
    value: &'f str,
    display: Option<&'f Display>,
    options: &'f [Opt],
    placeholder: String,
    empty_text: String,
    loading: bool,
    setter: Setter,
}

/// Flutter `PaseoFormField` + `PaseoSelectField` (md): the muted label, then
/// a full-width surface2 field (44px, 8px radius, 16px padding, borderAccent
/// rule on hover), the selected option's label (else the muted placeholder),
/// a 14px spinner while `loading` and a muted 16px chevron.
/// A value the catalog cannot describe (yet) reads as itself, muted.
fn select(ui: &mut Ui, id: u64, enabled: bool, field: SelectField) {
    let SelectField { key, label, value, display, options, placeholder, empty_text, loading, setter } = field;
    let pal = ui.pal;
    ui.para(&label, 14.0, pal.muted);
    ui.gap(8.0);
    let selected = options.iter().find(|o| !value.is_empty() && o.id == value).map(|o| o.label.clone());
    let (shown, muted) = match selected {
        Some(label) => (label, false),
        None => (display.map(|d| d.label.clone()).unwrap_or(placeholder), true),
    };
    let opts: Vec<Opt> = options.to_vec();
    let current = value.to_string();
    let title = label.clone();
    // Flutter keeps the field openable while loading (an empty list reads `emptyText`).
    let act = enabled.then(|| {
        handler(move |s, _| {
            let items: Vec<MenuItem> = opts
                .iter()
                .map(|o| {
                    let pick = o.clone();
                    item(o.label.clone(), handler(move |s, env| {
                        if let Some(p) = profile_sheet(s, id) {
                            let display = (!pick.id.is_empty()).then(|| Display { label: pick.label.clone(), description: pick.description.clone() });
                            setter(&mut p.form, &pick.id, display);
                        }
                        changed(s, env, id);
                    }))
                    .description(o.description.clone())
                    .selected(o.id == current)
                })
                .collect();
            let searchable = items.len() > 6;
            // Flutter passes no `searchPlaceholder`: the select default "Search…".
            let menu = Menu::new(s, title.clone(), items).select().searchable(searchable, "Search…".to_string()).empty(empty_text.clone());
            s.open_menu(menu);
        })
    });
    let zone_key = format!("sheet.{id}.{key}");
    let rect = Rect { pos: dvec2(ui.x, ui.y), size: dvec2(ui.w, 44.0) };
    let (hovered, _) = ui.state(&zone_key);
    let interactive = act.is_some();
    let border = if hovered && interactive { Some(pal.border_accent) } else { None };
    // Disabled (not loading) fields fade to 50%.
    let fade = if !interactive && !loading { 0.5 } else { 1.0 };
    ui.rect(rect, alpha(pal.surface2, fade), border, 8.0);
    let mut right = rect.pos.x + rect.size.x - 16.0 - 16.0;
    ui.icon(Icon::ChevronDown, dvec2(right, rect.pos.y + 14.0), 16.0, alpha(pal.muted, fade));
    if loading {
        right -= 8.0 + 14.0;
        ui.icon(Icon::Spinner, dvec2(right, rect.pos.y + 15.0), 14.0, pal.muted);
    }
    let color = if muted { pal.muted } else { pal.foreground };
    let line = ui.measure(Font::Regular, 14.0, "Ag", None).y;
    ui.text_line(Font::Regular, 14.0, alpha(color, fade), dvec2(rect.pos.x + 16.0, rect.pos.y + (44.0 - line) / 2.0), (right - 8.0 - rect.pos.x - 16.0).max(0.0), &shown);
    if interactive {
        ui.zone(&zone_key, rect, act);
    }
    ui.y += 44.0;
    ui.gap(16.0);
}

fn pick_appearance(s: &mut Settings, id: u64) {
    let Some(p) = profile_sheet(s, id) else { return };
    let color = p.form.color.clone();
    let current = p.form.icon.clone();
    let dark = crate::features::settings::ui::theme::palette().dark;
    let mut items = vec![item(ap("noIcon"), handler(move |s, _| pick_color(s, id, String::new()))).lead(Lead::Icon(Icon::Star, identity_foreground(&color, dark))).selected(current.is_empty())];
    for (name, icon) in PROFILE_ICONS {
        let chosen = name.to_string();
        items.push(
            item(name.to_string(), handler(move |s, _| pick_color(s, id, chosen.clone())))
                .lead(Lead::Icon(*icon, identity_foreground(&color, dark)))
                .selected(current == *name),
        );
    }
    let menu = Menu::new(s, ap("iconLabel"), items).select().searchable(true, "Search…".to_string());
    s.open_menu(menu);
}

/// Second step of the appearance picker: the identity colour for the chosen icon.
fn pick_color(s: &mut Settings, id: u64, icon: String) {
    let Some(p) = profile_sheet(s, id) else { return };
    let current = if p.form.color.is_empty() { "none".to_string() } else { p.form.color.clone() };
    let dark = crate::features::settings::ui::theme::palette().dark;
    let glyph = profile_icon(&icon);
    let mut items = Vec::new();
    for name in std::iter::once("none").chain(IDENTITY_COLORS) {
        let picked = name.to_string();
        let chosen_icon = icon.clone();
        let label = if name == "none" { ap("noIcon") } else { name.to_string() };
        items.push(
            item(label, handler(move |s, env| {
                if let Some(p) = profile_sheet(s, id) {
                    p.form.set_appearance(&chosen_icon, if picked == "none" { "" } else { &picked });
                }
                changed(s, env, id);
            }))
            .lead(Lead::Icon(glyph, identity_foreground(name, dark)))
            .selected(current == name),
        );
    }
    // Dismissing the colour step keeps the icon with the previous colour.
    let keep_icon = icon.clone();
    if let Some(p) = profile_sheet(s, id) {
        let color = p.form.color.clone();
        p.form.set_appearance(&keep_icon, &color);
    }
    let menu = Menu::new(s, ap("iconLabel"), items).select();
    s.open_menu(menu);
}

pub fn profile_body(ui: &mut Ui, id: u64, p: &ProfileSheet) {
    let pal = ui.pal;
    let f = &p.form;
    let enabled = !f.submitting;
    // Icon trigger beside the name field.
    let (x0, y0, w0) = (ui.x, ui.y, ui.w);
    ui.para(&ap("iconLabel"), 14.0, pal.muted);
    let label_h = ui.y - y0;
    ui.y = y0;
    ui.x = x0 + 64.0;
    ui.para(&ap("nameLabel"), 14.0, pal.muted);
    ui.x = x0;
    let fy = y0 + label_h + 8.0;
    let trigger = Rect { pos: dvec2(x0, fy), size: dvec2(56.0, 40.0) };
    let (hovered, _) = ui.state(&format!("sheet.{id}.appearance"));
    ui.rect(trigger, pal.surface2, Some(if hovered && enabled { pal.border_accent } else { pal.surface2 }), 8.0);
    let color = identity_foreground(&f.color, pal.dark).unwrap_or(pal.muted);
    ui.icon(profile_icon(&f.icon), dvec2(x0 + 10.0, fy + 12.0), 16.0, color);
    ui.icon(Icon::ChevronDown, dvec2(x0 + 30.0, fy + 12.0), 16.0, pal.muted);
    if enabled {
        ui.zone(&format!("sheet.{id}.appearance"), trigger, Some(handler(move |s, _| pick_appearance(s, id))));
    }
    let name_id = format!("sheet.{id}.name");
    let name_changed: Handler = handler(move |s, env| {
        let text = env.fields.text(&format!("sheet.{id}.name"));
        if let Some(p) = profile_sheet(s, id) {
            p.form.set_name(&text);
        }
    });
    let spec = FieldSpec::new(name_id, f.name.clone()).placeholder(ap("namePlaceholder")).enabled(enabled).autofocus().on_blur(name_changed);
    ui.draw_ctl(Ctl::Field { spec, w: w0 - 64.0, h: 40.0 }, dvec2(x0 + 64.0, fy));
    ui.y = fy + 40.0 + 16.0;

    let loading = f.catalog != Resolution::Complete;
    select(ui, id, enabled, SelectField { key: "provider", label: ap("providerLabel"), value: &f.provider, display: f.provider_display.as_ref(), options: &f.provider_options, placeholder: ap("providerPlaceholder"), empty_text: ap("noProviders"), loading, setter: ProfileForm::set_provider });
    if f.show_model {
        select(ui, id, enabled, SelectField { key: "model", label: ap("modelLabel"), value: &f.model_id, display: f.model_display.as_ref(), options: &f.model_options, placeholder: ap("modelLabel"), empty_text: ap("noModels"), loading: false, setter: ProfileForm::set_model });
    }
    if f.show_mode {
        select(ui, id, enabled, SelectField { key: "mode", label: ap("modeLabel"), value: &f.mode_id, display: f.mode_display.as_ref(), options: &f.mode_options, placeholder: ap("modeLabel"), empty_text: ap("noModes"), loading: false, setter: ProfileForm::set_mode });
    }
    if f.show_thinking {
        select(ui, id, enabled, SelectField { key: "thinking", label: ap("thinkingLabel"), value: &f.thinking_id, display: f.thinking_display.as_ref(), options: &f.thinking_options, placeholder: ap("thinkingLabel"), empty_text: ap("noThinkingOptions"), loading: false, setter: ProfileForm::set_thinking });
    }
    if f.show_features {
        ui.para(&ap("featuresLabel"), 14.0, pal.muted);
        ui.gap(8.0);
        ui.card_begin();
        for feature in &f.features {
            draw_feature(ui, id, feature, enabled);
        }
        ui.card_end();
        ui.gap(16.0);
    }
    let notes_id = format!("sheet.{id}.notes");
    let notes_text = if ui.fields.exists(&notes_id) { ui.fields.text(&notes_id) } else { f.notes.clone() };
    let h = ui.multiline_height(&notes_text, 88.0);
    let spec = FieldSpec::new(notes_id, f.notes.clone()).multiline().placeholder(ap("notesPlaceholder")).enabled(enabled);
    ui.form_field(Some(&ap("notesLabel")), Some(&ap("notesHint")), None, spec, h);
    if let Some(error) = &f.submit_error {
        ui.gap(16.0);
        ui.selectable_para(&format!("profile.{id}.error"), error, 14.0, pal.danger);
    }
}

/// Original `AgentProfileFeatureRow`: a select or a switch per feature.
fn draw_feature(ui: &mut Ui, id: u64, feature: &Value, enabled: bool) {
    let fid = str_of(feature, "id").to_string();
    let label = str_of(feature, "label").to_string();
    let description = Some(str_of(feature, "description").to_string()).filter(|d| !d.is_empty());
    if str_of(feature, "type") == "select" {
        let options: Vec<Value> = arr(feature, "options").to_vec();
        let value = feature.get("value").and_then(Value::as_str).unwrap_or("").to_string();
        let shown = options.iter().find(|o| str_of(o, "id") == value).map(|o| str_of(o, "label").to_string()).unwrap_or_else(|| if value.is_empty() { label.clone() } else { value.clone() });
        let title = label.clone();
        let act = enabled.then(|| {
            let fid = fid.clone();
            handler(move |s, _| {
                let items = options
                    .iter()
                    .map(|o| {
                        let pick = str_of(o, "id").to_string();
                        let fid = fid.clone();
                        item(str_of(o, "label").to_string(), handler(move |s, env| {
                            if let Some(p) = profile_sheet(s, id) {
                                p.form.set_feature(&fid, Value::String(pick.clone()));
                            }
                            changed(s, env, id);
                        }))
                        .description(o.get("description").and_then(Value::as_str).map(String::from))
                        .selected(str_of(o, "id") == value)
                    })
                    .collect();
                let menu = Menu::new(s, title.clone(), items).select();
                s.open_menu(menu);
            })
        });
        ui.row(row(label).hint_opt(description).padding(12.0).control(Ctl::Dropdown { key: format!("sheet.{id}.feature.{fid}"), label: shown, lead: None, act, max_w: 200.0 }));
    } else {
        let on = feature.get("value").and_then(Value::as_bool) == Some(true);
        let act = enabled.then(|| {
            let fid = fid.clone();
            handler(move |s, env| {
                if let Some(p) = profile_sheet(s, id) {
                    p.form.set_feature(&fid, Value::Bool(!on));
                }
                changed(s, env, id);
            })
        });
        ui.row(row(label).hint_opt(description).padding(12.0).control(switch(format!("sheet.{id}.feature.{fid}"), on, act)));
    }
}

// ------------------------------------------------------------ metadata generation

#[derive(Default)]
pub struct MetadataState {
    pub entries: Vec<Value>,
    pub catalog_loading: bool,
    pub catalog_error: Option<String>,
    pub requested: bool,
    pub saving: bool,
    /// `None` follows the saved mode.
    pub draft_automatic: Option<bool>,
    /// The saved provider/model the draft was made against.
    pub draft_for: Option<(String, String)>,
}

pub fn reset(s: &mut Settings, host: &str) {
    if let Some(m) = s.metadata.get_mut(host) {
        m.requested = false;
    }
}

pub fn ensure_metadata(s: &mut Settings, env: &mut Env, host: &HostView) {
    if !host.connected() || s.metadata.get(&host.server_id).map(|m| m.requested).unwrap_or(false) {
        return;
    }
    let state = s.metadata.entry(host.server_id.clone()).or_default();
    state.requested = true;
    state.catalog_loading = true;
    state.catalog_error = None;
    let id = host.server_id.clone();
    s.request(env, &host.server_id, "get_providers_snapshot_request", json!({}), "get_providers_snapshot_response", move |s, _, result| {
        let state = s.metadata.entry(id).or_default();
        state.catalog_loading = false;
        match result {
            Ok(snapshot) => state.entries = provider_entries(&snapshot),
            Err(error) => state.catalog_error = Some(error.text()),
        }
    });
}

fn configured(s: &Settings, host: &str) -> Vec<Value> {
    data(s, host).map(|d| arr(get(&d.config, "metadataGeneration"), "providers").to_vec()).unwrap_or_default()
}

fn save_metadata(s: &mut Settings, env: &mut Env, host: &str, providers: Vec<Value>) {
    s.metadata.entry(host.to_string()).or_default().saving = true;
    let id = host.to_string();
    daemon::patch(s, env, host, json!({"metadataGeneration": {"providers": providers}}), move |s, env, result| {
        let state = s.metadata.entry(id).or_default();
        state.saving = false;
        if let Err(error) = result {
            state.draft_automatic = None;
            env.toast(format!("{}: {}", t("settings.metadataGeneration.saveError"), error.text()), true);
        }
    });
}

/// Original `buildSelectableProviderSelectorProviders` selection kind.
fn selection_kind(entry: &Value, connected: bool) -> &'static str {
    if !arr(entry, "models").is_empty() || str_of(entry, "status") == "ready" {
        return "models";
    }
    if str_of(entry, "status") == "loading" {
        return if connected { "loading" } else { "error" };
    }
    "error"
}

fn mt(key: &str) -> String {
    t(&format!("settings.metadataGeneration.{key}"))
}

pub fn draw_metadata_page(ui: &mut Ui, s: &Settings, host: &HostView) {
    if !host.connected() || !host.can_manage() {
        return;
    }
    if data(s, &host.server_id).map(|d| d.loading).unwrap_or(true) {
        ui.draw_ctl(Ctl::Spinner(24.0), dvec2(ui.x + (ui.w - 24.0) / 2.0, ui.y + 78.0));
        ui.y += 180.0;
        return;
    }
    let id = host.server_id.clone();
    let configured = configured(s, &id);
    let first = configured.first().cloned();
    let saved = first.as_ref().map(|f| (str_of(f, "provider").to_string(), str_of(f, "model").to_string()));
    let default = MetadataState::default();
    let state = s.metadata.get(&id).unwrap_or(&default);
    // The draft resets whenever the saved provider or model changes.
    let draft = if state.draft_for == saved { state.draft_automatic } else { None };
    let automatic = draft.unwrap_or(first.is_none());
    let info = mt("description");
    let docs = Ctl::Link { key: format!("metadata.{id}.docs"), label: mt("docs"), url: "https://paseo.sh/docs/metadata-generation".into(), leading: false };
    ui.section_title(&mt("title"), Some((format!("metadata.{id}.info"), info)), Some(docs));
    ui.card_begin();
    let seg = |value: bool| {
        let host = id.clone();
        let saved = saved.clone();
        let act = (automatic != value).then(|| {
            handler(move |s, env| {
                let state = s.metadata.entry(host.clone()).or_default();
                state.draft_automatic = Some(value);
                state.draft_for = saved.clone();
                if value {
                    save_metadata(s, env, &host, Vec::new());
                }
            })
        });
        (mt(if value { "automatic" } else { "preferred" }), automatic == value, act)
    };
    let control = Ctl::Segmented { key: format!("metadata.{id}.mode"), items: vec![seg(true), seg(false)] };
    ui.row(row(mt("selection")).hint(mt(if automatic { "automaticHint" } else { "preferredHint" })).control(control));
    if !automatic {
        let connected = host.connected();
        let provider_id = saved.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        let model_id = saved.as_ref().map(|s| s.1.clone()).unwrap_or_default();
        let label = selected_label(state, &provider_id, &model_id, connected);
        let entries = state.entries.clone();
        let loading = state.catalog_loading;
        let empty_text = if loading {
            t("modelSelector.loadingSelector")
        } else {
            state.catalog_error.clone().unwrap_or_else(|| t("providerSelection.readiness.noProviders"))
        };
        let host = id.clone();
        let configured = configured.clone();
        let saved2 = saved.clone();
        let act = (!state.saving).then(|| {
            handler(move |s, _| {
                let mut items = Vec::new();
                for entry in entries.iter().filter(|e| e.get("enabled").and_then(Value::as_bool) == Some(true)) {
                    let pid = str_of(entry, "provider").to_string();
                    let plabel = str_or(entry, "label", &pid).to_string();
                    let kind = selection_kind(entry, connected);
                    let lead = Lead::Provider(pid.clone());
                    if kind != "models" {
                        let message = str_of(entry, "error");
                        let description = if kind == "loading" {
                            t("providerSelection.loading")
                        } else if !message.is_empty() {
                            message.to_string()
                        } else {
                            t(if str_of(entry, "status") == "unavailable" { "providerSelection.unavailable" } else { "providerSelection.unknownError" })
                        };
                        let sep = !items.is_empty();
                        items.push(item(plabel, handler(|_, _| {})).description(Some(description)).lead(lead).enabled(false).separator(sep));
                        continue;
                    }
                    let mut first_model = true;
                    for model in arr(entry, "models").iter().filter(|m| m.get("isSelectable").and_then(Value::as_bool) != Some(false)) {
                        let mid = str_of(model, "id").to_string();
                        let sep = first_model && !items.is_empty();
                        first_model = false;
                        let host = host.clone();
                        let configured = configured.clone();
                        let pick_pid = pid.clone();
                        let pick_mid = mid.clone();
                        let saved = saved2.clone();
                        let selected = saved2.as_ref() == Some(&(pid.clone(), mid.clone()));
                        items.push(
                            item(str_or(model, "label", &mid).to_string(), handler(move |s, env| {
                                let state = s.metadata.entry(host.clone()).or_default();
                                state.draft_automatic = Some(false);
                                state.draft_for = saved.clone();
                                // The pick leads, later fallbacks stay; an empty model is left out.
                                let mut lead = serde_json::Map::new();
                                lead.insert("provider".into(), Value::String(pick_pid.clone()));
                                if !pick_mid.is_empty() {
                                    lead.insert("model".into(), Value::String(pick_mid.clone()));
                                }
                                let mut next = vec![Value::Object(lead)];
                                next.extend(configured.iter().skip(1).cloned());
                                save_metadata(s, env, &host, next);
                            }))
                            .description(Some(format!("{plabel} · {mid}")))
                            .lead(lead.clone())
                            .selected(selected)
                            .separator(sep),
                        );
                    }
                }
                let menu = Menu::new(s, t("providerSelection.selectModel"), items)
                    .select()
                    .searchable(true, t("modelSelector.searchAllPlaceholder"))
                    .empty(empty_text.clone());
                s.open_menu(menu);
            })
        });
        let _ = loading;
        let lead = (!provider_id.is_empty()).then(|| Lead::Provider(provider_id.clone()));
        ui.row(row(mt("model")).hint(mt("modelHint")).control(Ctl::Dropdown { key: format!("metadata.{id}.model"), label, lead, act, max_w: 220.0 }));
    }
    ui.card_end();
    ui.gap(24.0);
}

/// Original `resolveSelectedModelLabel`.
fn selected_label(state: &MetadataState, provider_id: &str, model_id: &str, connected: bool) -> String {
    let Some(entry) = state.entries.iter().find(|e| e.get("enabled").and_then(Value::as_bool) == Some(true) && str_of(e, "provider") == provider_id) else {
        return t(if state.catalog_loading { "providerSelection.loading" } else { "providerSelection.selectModel" });
    };
    match selection_kind(entry, connected) {
        "loading" => return t("providerSelection.loading"),
        "error" => return t("providerSelection.error"),
        _ => {}
    }
    let models: Vec<&Value> = arr(entry, "models").iter().filter(|m| m.get("isSelectable").and_then(Value::as_bool) != Some(false)).collect();
    let found = models.iter().find(|m| str_of(m, "id") == model_id);
    if found.is_none() && !model_id.is_empty() {
        return model_id.to_string();
    }
    let row = found.copied().or_else(|| models.iter().find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true)).copied()).or(models.first().copied());
    match row {
        Some(m) => str_or(m, "label", str_of(m, "id")).to_string(),
        None => t("providerSelection.selectModel"),
    }
}
