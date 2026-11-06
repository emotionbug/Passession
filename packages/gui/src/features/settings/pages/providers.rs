//! Original `providers-section.tsx` / Flutter `provider_catalog.dart`: the
//! host's provider rows, the ACP "Add provider" catalog, the provider sheet
//! (discovered/custom models, configure, add model, diagnostic, refresh).

use makepad_widgets::*;
use serde_json::{Value, json};

use super::View;
use super::daemon::{self, data};
use crate::features::settings::host::HostView;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{str_of, str_or};
use crate::features::settings::model::match_score::rank;
use crate::features::settings::model::provider_catalog::{
    DiscoveredCache, Tone, acp_install_patch, additional_models, available_catalog, config_owned, provider_status, resolve_discovered,
    selectable_models,
};
use crate::features::settings::model::time::relative;
use crate::features::settings::rpc::{Env, handler};
use crate::features::settings::state::{Settings, SheetKind};
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::field::FieldSpec;
use crate::features::settings::ui::kit::{BSize, Ctl, Ui, Variant, button, row, switch};
use crate::features::settings::ui::overlay::{Menu, SheetHead, item};

#[derive(Default)]
pub struct ProvidersUi {
    pub pending: Option<String>,
    pub removing: Option<String>,
    pub installing: Option<String>,
}

fn pt(key: &str) -> String {
    t(&format!("settings.providers.{key}"))
}

fn catalog_field(host: &str) -> String {
    format!("providers.{host}.catalogSearch")
}

// ------------------------------------------------------------ list

fn toggle(s: &mut Settings, env: &mut Env, host: &str, id: String, enabled: bool) {
    s.providers_ui.entry(host.to_string()).or_default().pending = Some(id.clone());
    let host_id = host.to_string();
    daemon::patch(s, env, host, json!({"providers": { id.clone(): {"enabled": enabled}}}), move |s, env, result| {
        let ui = s.providers_ui.entry(host_id).or_default();
        if ui.pending.as_deref() == Some(id.as_str()) {
            ui.pending = None;
        }
        if let Err(error) = result {
            env.error(&error);
        }
    });
}

fn remove(s: &mut Settings, host: &str, id: String, label: String) {
    if s.providers_ui.get(host).and_then(|u| u.removing.clone()).is_some() {
        return;
    }
    let host_id = host.to_string();
    let go = handler(move |s, env| {
        s.providers_ui.entry(host_id.clone()).or_default().removing = Some(id.clone());
        let host2 = host_id.clone();
        let id2 = id.clone();
        daemon::patch(s, env, &host_id, json!({"removeProviders": [id.clone()]}), move |s, env, result| {
            let ui = s.providers_ui.entry(host2).or_default();
            if ui.removing.as_deref() == Some(id2.as_str()) {
                ui.removing = None;
            }
            if let Err(error) = result {
                env.error(&error);
            }
        });
    });
    s.confirm(
        ta("settings.providers.remove.confirmTitle", &[("name", &label)]),
        pt("remove.confirmMessage"),
        Some(pt("remove.confirm")),
        true,
        go,
    );
}

fn install(s: &mut Settings, env: &mut Env, host: &str, entry: Value) {
    if s.providers_ui.get(host).and_then(|u| u.installing.clone()).is_some() {
        return;
    }
    let supports_hoplite = env.host(host).map(|h| h.feature("hopliteProvider")).unwrap_or(false);
    let patch = match acp_install_patch(&entry, supports_hoplite) {
        Ok(patch) => patch,
        Err(message) => return env.toast(message, true),
    };
    let id = str_of(&entry, "id").to_string();
    s.providers_ui.entry(host.to_string()).or_default().installing = Some(id.clone());
    let host_id = host.to_string();
    daemon::patch(s, env, host, patch, move |s, env, result| {
        let finish = |s: &mut Settings, env: &mut Env, result: Result<(), crate::features::settings::rpc::ReqError>, host: &str, id: &str| {
            let ui = s.providers_ui.entry(host.to_string()).or_default();
            if ui.installing.as_deref() == Some(id) {
                ui.installing = None;
            }
            if let Err(error) = result {
                env.error(&error);
            }
        };
        match result {
            Err(error) => finish(s, env, Err(error), &host_id, &id),
            Ok(()) => {
                let host2 = host_id.clone();
                let id2 = id.clone();
                daemon::refresh_providers(s, env, &host_id, vec![id.clone()], move |s, env, result| finish(s, env, result, &host2, &id2));
            }
        }
    });
}

fn status_ctl(ui: &mut Ui, entry: &Value, compact: bool) -> Ctl {
    let pal = ui.pal;
    let enabled = entry.get("enabled").and_then(Value::as_bool) != Some(false);
    let (tone, key, count) = provider_status(str_of(entry, "status"), enabled, selectable_models(entry).len());
    let dot = match tone {
        Tone::Success => Ctl::Dot(pal.success, 8.0),
        Tone::Warning => Ctl::Dot(pal.warning, 8.0),
        Tone::Danger => Ctl::Dot(pal.danger, 8.0),
        Tone::Muted => Ctl::Dot(pal.muted, 8.0),
        Tone::Loading => Ctl::Spinner(10.0),
    };
    let mut items = vec![dot];
    if !compact {
        let muted = |text: String| Ctl::Label { text, color: pal.muted, size: 14.0, font: Font::Regular, max_w: 200.0 };
        items.push(muted(t(key)));
        if let Some(count) = count {
            items.push(muted("·".into()));
            items.push(muted(if count == 1 { pt("models.one") } else { ta("settings.providers.models.many", &[("count", &count.to_string())]) }));
        }
    }
    Ctl::Row(items, 6.0)
}

pub fn draw_providers(ui: &mut Ui, s: &Settings, _v: &View, host: &HostView) {
    let connected = host.connected();
    let d = data(s, &host.server_id);
    let loading = d.map(|d| d.loading).unwrap_or(true);
    if let Some(error) = d.and_then(|d| d.error.clone()) {
        let id = host.server_id.clone();
        let retry = button(format!("providers.{id}.retry"), t("common.actions.retry"), Some(handler(move |s, env| {
            if let Some(host) = env.host(&id) {
                daemon::load(s, env, &host);
            }
        })))
        .ctl();
        ui.alert(false, &error.text(), None, Some(retry));
        ui.gap(24.0);
        return;
    }
    let pal = ui.pal;
    let compact = ui.compact;
    let pui = s.providers_ui.get(&host.server_id);
    let supports_removal = host.feature("providerRemoval");
    ui.section_title(&pt("title"), None, None);
    let entries: Vec<Value> = d.map(|d| d.providers.clone()).unwrap_or_default();
    if !connected {
        ui.empty_card(&pt("unavailable"), None, false);
    } else if loading {
        ui.empty_card(&pt("loading"), None, false);
    } else if !entries.is_empty() {
        ui.card_begin();
        let config = d.map(|d| d.config.clone()).unwrap_or(Value::Null);
        for entry in &entries {
            let id = str_of(entry, "provider").to_string();
            let label = str_or(entry, "label", &id).to_string();
            let enabled = entry.get("enabled").and_then(Value::as_bool) != Some(false);
            let status = str_of(entry, "status");
            let raw_error = str_of(entry, "error").trim().to_string();
            let error = (enabled && status == "error" && !raw_error.is_empty() && !compact).then_some(raw_error);
            let toggling = pui.and_then(|u| u.pending.as_deref()) == Some(id.as_str());
            let removing = pui.and_then(|u| u.removing.as_deref()) == Some(id.as_str());
            let can_remove = supports_removal && str_of(entry, "source") == "custom" && config_owned(&config, &id);
            let host_id = host.server_id.clone();
            let toggle_id = id.clone();
            let toggle_act = (!toggling && !removing).then(|| handler(move |s, env| toggle(s, env, &host_id, toggle_id.clone(), !enabled)));
            let mut controls = vec![switch(format!("providers.{}.{id}.enabled", host.server_id), enabled, toggle_act)];
            if can_remove {
                let host_id = host.server_id.clone();
                let rid = id.clone();
                let rlabel = label.clone();
                let open = (!removing).then(|| {
                    handler(move |s, _| {
                        let host_id = host_id.clone();
                        let rid = rid.clone();
                        let menu_title = ta("settings.providers.actions.menu", &[("name", &rlabel)]);
                        let rlabel = rlabel.clone();
                        let items = vec![item(pt("actions.remove"), handler(move |s, _| remove(s, &host_id, rid.clone(), rlabel.clone())))
                            .lead(crate::features::settings::ui::kit::Lead::Icon(Icon::Trash, None))
                            .destructive()];
                        let menu = Menu::new(s, menu_title, items);
                        s.open_menu(menu);
                    })
                });
                controls.push(Ctl::IconButton { key: format!("providers.{}.{id}.menu", host.server_id), icon: Icon::MoreHorizontal, act: open, destructive: false, size: 32.0, tip: None });
            } else {
                controls.push(Ctl::Gap(32.0));
            }
            let (hovered, _) = ui.state(&format!("providers.{}.{id}.row", host.server_id));
            let chevron = Ctl::Glyph(Icon::ChevronRight, 14.0, if hovered { pal.foreground } else { pal.muted });
            let svg = entry.get("iconSvg").and_then(Value::as_str).map(String::from).filter(|s| !s.is_empty());
            let lead = Ctl::Row(vec![chevron, Ctl::Provider(id.clone(), svg, 16.0, pal.foreground)], 8.0);
            let mut title = row(label.clone()).one_line().leading(lead).inline().error(error).control(Ctl::Row(controls, 4.0));
            let mut accessory = Vec::new();
            if !compact {
                accessory.push(Ctl::Label { text: "·".into(), color: pal.muted, size: 14.0, font: Font::Regular, max_w: 20.0 });
            }
            accessory.push(status_ctl(ui, entry, compact));
            title = title.accessory(Ctl::Row(accessory, 8.0));
            let host_id = host.server_id.clone();
            let open_id = id.clone();
            title = title.tap(format!("providers.{}.{id}.row", host.server_id), handler(move |s, _| open_provider_sheet(s, &host_id, &open_id)));
            ui.row(title);
        }
        ui.card_end();
    }
    ui.gap(16.0);
    if !connected {
        return;
    }
    ui.gap(16.0);
    ui.section_title(&pt("addProvider"), None, None);
    draw_catalog(ui, s, host, &entries);
    ui.gap(24.0);
}

/// Original `ProviderCatalogList`: search rail over wrapping 300px tiles (≤4 columns).
fn draw_catalog(ui: &mut Ui, s: &Settings, host: &HostView, entries: &[Value]) {
    let pal = ui.pal;
    let field = catalog_field(&host.server_id);
    let query = ui.fields.text(&field);
    let w = ui.w.min(420.0);
    let spec = FieldSpec::new(field, "").placeholder(t("providerCatalog.search"));
    ui.place(Ctl::Field { spec, w, h: 34.0 });
    ui.gap(12.0);
    let installed: Vec<String> = entries.iter().map(|e| str_of(e, "provider").to_string()).collect();
    let list = available_catalog(&installed, &query);
    if list.is_empty() {
        ui.empty_card(&t("providerCatalog.noProviders"), None, false);
        return;
    }
    let installing = s.providers_ui.get(&host.server_id).and_then(|u| u.installing.clone());
    let gap = 12.0;
    let columns = (((ui.w + gap) / (300.0 + gap)).floor() as usize).clamp(1, 4);
    let tile_w = if columns == 1 { ui.w } else { ((ui.w - gap * (columns as f64 - 1.0)) / columns as f64).floor() };
    let tile_h = 62.0;
    let (x0, y0) = (ui.x, ui.y);
    for (i, entry) in list.iter().enumerate() {
        let col = i % columns;
        let rown = i / columns;
        let x = x0 + col as f64 * (tile_w + gap);
        let y = y0 + rown as f64 * (tile_h + gap);
        ui.rect(Rect { pos: dvec2(x, y), size: dvec2(tile_w, tile_h) }, pal.surface1, Some(pal.border), 8.0);
        ui.rect(Rect { pos: dvec2(x + 12.0, y + 13.0), size: dvec2(36.0, 36.0) }, pal.surface2, None, 6.0);
        let id = str_of(entry, "id").to_string();
        if crate::common::provider_icon::svg_for(&id, None).is_some() {
            ui.provider_glyph(&id, None, dvec2(x + 18.0, y + 19.0), 24.0, pal.foreground);
        } else {
            ui.icon(Icon::PackagePlus, dvec2(x + 20.0, y + 21.0), 20.0, pal.foreground);
        }
        let busy = installing.as_deref() == Some(id.as_str());
        let host_id = host.server_id.clone();
        let tile_entry = entry.clone();
        let add_label = t(if busy { "providerCatalog.actions.adding" } else { "providerCatalog.actions.add" });
        let add = button(format!("catalog.{}.{id}", host.server_id), add_label, (!busy && installing.is_none()).then(|| handler(move |s, env| install(s, env, &host_id, tile_entry.clone()))))
            .size(BSize::Xs)
            .loading(busy)
            .ctl();
        let add_size = ui.measure_ctl(&add);
        let text_x = x + 60.0;
        let text_w = (tile_w - 60.0 - 12.0 - add_size.x - 12.0).max(40.0);
        let title = str_or(entry, "title", &id).to_string();
        ui.text_line(Font::Regular, 14.0, pal.foreground, dvec2(text_x, y + 12.0), text_w, &title);
        let mut meta = vec![Ctl::Badge { label: t("providerCatalog.types.acp"), tone: None, xs: true, dot: None, icon: None }];
        let link = str_of(entry, "installLink");
        if !link.is_empty() {
            meta.push(Ctl::Link { key: format!("catalog.{}.{id}.link", host.server_id), label: t("providerCatalog.actions.installInstructions"), url: link.to_string(), leading: false });
        }
        ui.draw_ctl(Ctl::Row(meta, 8.0), dvec2(text_x, y + 34.0));
        ui.draw_ctl(add, dvec2(x + tile_w - 12.0 - add_size.x, y + (tile_h - add_size.y) / 2.0));
    }
    let rows = list.len().div_ceil(columns);
    ui.y = y0 + rows as f64 * (tile_h + gap) - gap;
}

// ------------------------------------------------------------ provider sheet

pub struct ProviderSheet {
    pub host: String,
    pub provider: String,
    pub deleting: Option<String>,
    pub refreshing: bool,
    pub cache: Option<DiscoveredCache>,
}

pub fn open_provider_sheet(s: &mut Settings, host: &str, provider: &str) {
    let id = s.next_id();
    s.push_sheet(id, SheetKind::Provider(ProviderSheet { host: host.into(), provider: provider.into(), deleting: None, refreshing: false, cache: None }), 600.0);
}

fn provider_entry(s: &Settings, host: &str, provider: &str) -> Value {
    data(s, host).and_then(|d| d.providers.iter().find(|e| str_of(e, "provider") == provider).cloned()).unwrap_or_else(|| json!({}))
}

fn refresh_sheet(s: &mut Settings, env: &mut Env, id: u64) {
    let Some(SheetKind::Provider(p)) = s.sheet_mut(id) else { return };
    if p.refreshing {
        return;
    }
    p.refreshing = true;
    let (host, provider) = (p.host.clone(), p.provider.clone());
    daemon::refresh_providers(s, env, &host, vec![provider], move |s, env, result| {
        if let Some(SheetKind::Provider(p)) = s.sheet_mut(id) {
            p.refreshing = false;
        }
        if let Err(error) = result {
            env.error(&error);
        }
    });
}

fn delete_model(s: &mut Settings, env: &mut Env, id: u64, model: String) {
    let Some(SheetKind::Provider(p)) = s.sheet_mut(id) else { return };
    p.deleting = Some(model.clone());
    let (host, provider) = (p.host.clone(), p.provider.clone());
    let config = data(s, &host).map(|d| d.config.clone()).unwrap_or(Value::Null);
    let next: Vec<Value> = additional_models(&config, &provider).into_iter().filter(|m| str_of(m, "id") != model).collect();
    let host2 = host.clone();
    let provider2 = provider.clone();
    daemon::patch(s, env, &host, json!({"providers": { provider.clone(): {"additionalModels": next}}}), move |s, env, result| {
        let done = move |s: &mut Settings, env: &mut Env, result: Result<(), crate::features::settings::rpc::ReqError>| {
            if let Some(SheetKind::Provider(p)) = s.sheet_mut(id)
                && p.deleting.as_deref() == Some(model.as_str())
            {
                p.deleting = None;
            }
            if let Err(error) = result {
                env.error(&error);
            }
        };
        match result {
            Err(error) => done(s, env, Err(error)),
            Ok(()) => daemon::refresh_providers(s, env, &host2, vec![provider2], done),
        }
    });
}

pub fn provider_sheet_head(s: &Settings, v: &View, id: u64, p: &ProviderSheet) -> SheetHead {
    let entry = provider_entry(s, &p.host, &p.provider);
    let mut head = SheetHead::new(str_or(&entry, "label", &p.provider).to_string());
    head.search = Some((format!("sheet.{id}.search"), pt("models.searchPlaceholder")));
    let fetched = str_of(&entry, "fetchedAt");
    head.footer_meta = Some(relative((!fetched.is_empty()).then_some(fetched), true).map(|time| ta("settings.providers.models.updated", &[("time", &time)])).unwrap_or_default());
    head.stack_compact = true;
    let refreshing = p.refreshing || str_of(&entry, "status") == "loading";
    // Flutter `onConfigure` is offered only when the connection may manage the daemon.
    let manage = s.daemon.contains_key(&p.host) && v.host(&p.host).map(HostView::can_manage).unwrap_or(false);
    let host = p.host.clone();
    let configure_entry = if entry.as_object().map(|m| m.is_empty()).unwrap_or(true) { json!({"provider": p.provider}) } else { entry.clone() };
    let can_configure = manage;
    if can_configure {
        head.actions.push(
            // Flutter's label is the untranslated "Configure".
            button(format!("sheet.{id}.configure"), "Configure", Some(handler(move |s, env| daemon::edit_provider_config(s, env, &host, &configure_entry))))
                .icon(Icon::Settings2)
                .variant(Variant::Secondary)
                .ctl(),
        );
    }
    let (host, provider) = (p.host.clone(), p.provider.clone());
    head.actions.push(
        button(format!("sheet.{id}.add"), pt("models.addModel"), Some(handler(move |s, _| {
            let sheet = s.next_id();
            s.push_sheet(sheet, SheetKind::AddModel(AddModelSheet { host: host.clone(), provider: provider.clone(), error: None, saving: false }), 420.0);
        })))
        .icon(Icon::Plus)
        .variant(Variant::Secondary)
        .ctl(),
    );
    let (host, provider) = (p.host.clone(), p.provider.clone());
    head.actions.push(
        button(format!("sheet.{id}.diagnostic"), pt("diagnostic.button"), Some(handler(move |s, env| open_diagnostic(s, env, &host, &provider))))
            .icon(Icon::FileText)
            .variant(Variant::Secondary)
            .ctl(),
    );
    let label = pt(if refreshing { "diagnostic.refreshing" } else { "diagnostic.refresh" });
    let mut refresh = button(format!("sheet.{id}.refresh"), label, (!refreshing).then(|| handler(move |s, env| refresh_sheet(s, env, id)))).variant(Variant::Primary);
    if !refreshing {
        refresh = refresh.icon(Icon::RotateCw);
    }
    head.actions.push(refresh.ctl());
    head
}

fn model_section(ui: &mut Ui, title: &str, models: &[Value], deleting: Option<&str>, sheet: Option<u64>) {
    let pal = ui.pal;
    let count = models.len().to_string();
    let cw = ui.measure(Font::Regular, 12.0, &count, None).x;
    ui.text(Font::Regular, 12.0, pal.muted, dvec2(ui.x + 4.0, ui.y), Some(ui.w - cw - 16.0), title);
    ui.text(Font::Regular, 12.0, pal.muted, dvec2(ui.x + ui.w - cw, ui.y), None, &count);
    ui.gap(16.0 + 8.0);
    ui.card_begin();
    for model in models {
        let mid = str_of(model, "id").to_string();
        let label = str_or(model, "label", &mid).to_string();
        let description = str_of(model, "description").to_string();
        ui.hline(ui.x, ui.y, ui.w);
        ui.y += 1.0;
        let (x, y, w) = (ui.x, ui.y, ui.w);
        let h = 36.0;
        let mut cx = x + 16.0;
        let third = (w - 32.0) / 3.0;
        cx += ui.text_line(Font::Regular, 14.0, pal.foreground, dvec2(cx, y + 9.0), third, &label).x + 12.0;
        cx += ui.text_line(Font::Code, 12.0, pal.muted, dvec2(cx, y + 11.0), third, &mid).x + 12.0;
        match sheet {
            None => {
                if !description.is_empty() {
                    ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(cx, y + 11.0), (x + w - 16.0 - cx).max(0.0), &description);
                }
            }
            Some(sheet) => {
                let busy = deleting == Some(mid.as_str());
                let del_id = mid.clone();
                let act = (!busy).then(|| handler(move |s, env| delete_model(s, env, sheet, del_id.clone())));
                ui.draw_ctl(
                    Ctl::IconButton { key: format!("sheet.{sheet}.delete.{mid}"), icon: Icon::Trash, act, destructive: true, size: 28.0, tip: None },
                    dvec2(x + w - 16.0 - 28.0, y + 4.0),
                );
            }
        }
        ui.y += h;
    }
    ui.card_end();
    ui.gap(16.0);
}

pub fn provider_sheet_body(ui: &mut Ui, s: &Settings, _v: &View, id: u64, p: &ProviderSheet) {
    let pal = ui.pal;
    let entry = provider_entry(s, &p.host, &p.provider);
    let config = data(s, &p.host).map(|d| d.config.clone()).unwrap_or(Value::Null);
    let additional = additional_models(&config, &p.provider);
    let status = str_of(&entry, "status");
    let snapshot_refreshing = status == "loading";
    // `resolve_discovered` updates the cache; the sheet copy is refreshed in `tick`.
    let mut cache = p.cache.clone();
    let discovered = resolve_discovered(&p.host, &p.provider, &entry, snapshot_refreshing, &mut cache);
    let raw_error = if status == "error" { str_or(&entry, "error", &pt("diagnostic.unknownError")).to_string() } else { String::new() };
    let error = (!raw_error.is_empty()).then_some(raw_error);
    let refreshing = p.refreshing || snapshot_refreshing;
    let query = ui.fields.text(&format!("sheet.{id}.search"));
    let none = discovered.is_empty() && additional.is_empty();
    let empty = |ui: &mut Ui, text: &str| {
        let size = ui.measure(Font::Regular, 14.0, text, Some(ui.w - 32.0));
        ui.text(Font::Regular, 14.0, pal.muted, dvec2(ui.x + (ui.w - size.x) / 2.0, ui.y + 32.0), Some(ui.w - 32.0), text);
        ui.y += size.y + 64.0;
    };
    if none && snapshot_refreshing {
        ui.draw_ctl(Ctl::Spinner(20.0), dvec2(ui.x + (ui.w - 20.0) / 2.0, ui.y + 32.0));
        ui.y += 64.0;
        empty(ui, &pt("models.loading"));
        return;
    }
    if none
        && let Some(error) = error
    {
        ui.draw_ctl(Ctl::Glyph(Icon::TriangleAlert, 16.0, pal.muted), dvec2(ui.x + (ui.w - 16.0) / 2.0, ui.y + 32.0));
        ui.y += 56.0;
        empty(ui, &error);
        let label = pt(if refreshing { "models.retrying" } else { "models.retry" });
        let retry = button(format!("sheet.{id}.retry"), label, (!refreshing).then(|| handler(move |s, env| refresh_sheet(s, env, id)))).variant(Variant::Primary).ctl();
        let size = ui.measure_ctl(&retry);
        ui.draw_ctl(retry, dvec2(ui.x + (ui.w - size.x) / 2.0, ui.y));
        ui.y += size.y + 32.0;
        return;
    }
    let ranked_discovered = rank(&discovered, &query, |m| vec![str_of(m, "label").to_string(), str_of(m, "id").to_string(), str_of(m, "description").to_string()]);
    let ranked_custom = rank(&additional, &query, |m| vec![str_of(m, "label").to_string(), str_of(m, "id").to_string()]);
    if ranked_discovered.is_empty() && ranked_custom.is_empty() && !query.trim().is_empty() {
        empty(ui, &pt("models.noSearchMatches"));
        return;
    }
    if none {
        empty(ui, &pt("models.noneDetected"));
        return;
    }
    if !ranked_discovered.is_empty() {
        model_section(ui, &pt("models.discovered"), &ranked_discovered, None, None);
    }
    if !ranked_custom.is_empty() {
        model_section(ui, &pt("models.custom"), &ranked_custom, p.deleting.as_deref(), Some(id));
    }
}

/// Keeps each open provider sheet's discovered-model cache current.
pub fn tick(s: &mut Settings) {
    let snapshots: Vec<(u64, Value)> = s
        .sheets
        .iter()
        .filter_map(|sheet| match &sheet.kind {
            SheetKind::Provider(p) => Some((sheet.id, provider_entry(s, &p.host, &p.provider))),
            _ => None,
        })
        .collect();
    for (id, entry) in snapshots {
        if let Some(SheetKind::Provider(p)) = s.sheet_mut(id) {
            let refreshing = str_of(&entry, "status") == "loading";
            let mut cache = p.cache.take();
            resolve_discovered(&p.host.clone(), &p.provider.clone(), &entry, refreshing, &mut cache);
            p.cache = cache;
        }
    }
}

// ------------------------------------------------------------ add custom model

pub struct AddModelSheet {
    pub host: String,
    pub provider: String,
    pub error: Option<String>,
    pub saving: bool,
}

fn add_model(s: &mut Settings, env: &mut Env, id: u64) {
    let model = env.fields.text(&format!("sheet.{id}.model")).trim().to_string();
    let Some(SheetKind::AddModel(p)) = s.sheet_mut(id) else { return };
    if p.saving || model.is_empty() {
        return;
    }
    let (host, provider) = (p.host.clone(), p.provider.clone());
    let config = data(s, &host).map(|d| d.config.clone()).unwrap_or(Value::Null);
    let mut models = additional_models(&config, &provider);
    if models.iter().any(|m| str_of(m, "id") == model) {
        return;
    }
    if let Some(SheetKind::AddModel(p)) = s.sheet_mut(id) {
        p.saving = true;
        p.error = None;
    }
    models.push(json!({"id": model, "label": model}));
    let host2 = host.clone();
    let provider2 = provider.clone();
    daemon::patch(s, env, &host, json!({"providers": { provider.clone(): {"additionalModels": models}}}), move |s, env, result| {
        let done = move |s: &mut Settings, env: &mut Env, result: Result<(), crate::features::settings::rpc::ReqError>| match result {
            Ok(()) => s.close_sheet(env, id),
            Err(error) => {
                if let Some(SheetKind::AddModel(p)) = s.sheet_mut(id) {
                    p.saving = false;
                    let message = error.text();
                    p.error = Some(if message.is_empty() { pt("models.failedToSave") } else { message });
                }
            }
        };
        match result {
            Err(error) => done(s, env, Err(error)),
            Ok(()) => daemon::refresh_providers(s, env, &host2, vec![provider2], done),
        }
    });
}

pub fn add_model_head(_id: u64, p: &AddModelSheet) -> SheetHead {
    let mut head = SheetHead::new(pt("models.addCustomTitle"));
    head.dismissible = !p.saving;
    head
}

pub fn add_model_body(ui: &mut Ui, s: &Settings, id: u64, p: &AddModelSheet) {
    let pal = ui.pal;
    ui.para(&pt("models.modelId"), 14.0, pal.foreground);
    ui.gap(12.0);
    let field = format!("sheet.{id}.model");
    let text = ui.fields.text(&field).trim().to_string();
    let config = data(s, &p.host).map(|d| d.config.clone()).unwrap_or(Value::Null);
    let can_add = !text.is_empty() && !additional_models(&config, &p.provider).iter().any(|m| str_of(m, "id") == text) && !p.saving;
    let submit = handler(move |s, env| add_model(s, env, id));
    let spec = FieldSpec::new(field, "").placeholder(pt("models.modelIdPlaceholder")).enabled(!p.saving).autofocus().on_submit(submit.clone());
    let w = ui.w;
    ui.place(Ctl::Field { spec, w, h: 40.0 });
    if let Some(error) = &p.error {
        ui.gap(12.0);
        ui.para(error, 12.0, pal.danger);
    }
    ui.gap(12.0);
    let controls = Ctl::Row(
        vec![
            button(format!("sheet.{id}.cancel"), t("common.actions.cancel"), (!p.saving).then(|| handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl(),
            button(format!("sheet.{id}.add"), pt(if p.saving { "models.adding" } else { "models.add" }), can_add.then_some(submit)).variant(Variant::Primary).ctl(),
        ],
        8.0,
    );
    let size = ui.measure_ctl(&controls);
    ui.draw_ctl(controls, dvec2(ui.x + ui.w - size.x, ui.y));
    ui.y += size.y;
}

// ------------------------------------------------------------ diagnostic

pub struct DiagnosticSheet {
    pub host: String,
    pub provider: String,
    pub text: Option<String>,
    pub loading: bool,
}

fn open_diagnostic(s: &mut Settings, env: &mut Env, host: &str, provider: &str) {
    let id = s.next_id();
    s.push_sheet(id, SheetKind::Diagnostic(DiagnosticSheet { host: host.into(), provider: provider.into(), text: None, loading: false }), 600.0);
    fetch_diagnostic(s, env, id);
}

fn fetch_diagnostic(s: &mut Settings, env: &mut Env, id: u64) {
    let Some(SheetKind::Diagnostic(p)) = s.sheet_mut(id) else { return };
    p.loading = true;
    let (host, provider) = (p.host.clone(), p.provider.clone());
    s.request(env, &host, "provider_diagnostic_request", json!({"provider": provider}), "provider_diagnostic_response", move |s, _, result| {
        if let Some(SheetKind::Diagnostic(p)) = s.sheet_mut(id) {
            p.loading = false;
            p.text = Some(match result {
                Ok(r) => str_of(&r, "diagnostic").to_string(),
                Err(error) => {
                    let message = error.text();
                    if message.is_empty() { pt("diagnostic.failedToFetch") } else { message }
                }
            });
        }
    });
}

pub fn diagnostic_head(id: u64, p: &DiagnosticSheet) -> SheetHead {
    let mut head = SheetHead::new(pt("diagnostic.title"));
    let text = p.text.clone().filter(|t| !t.is_empty());
    let copy = text.map(|text| handler(move |_, env| env.copy(&text, Some(t("settings.providers.diagnostic.copyLabel")))));
    head.header_actions.push(Ctl::IconButton { key: format!("sheet.{id}.copy"), icon: Icon::Copy, act: copy, destructive: false, size: 28.0, tip: None });
    let refresh = (!p.loading).then(|| handler(move |s, env| fetch_diagnostic(s, env, id)));
    head.header_actions.push(Ctl::IconButton { key: format!("sheet.{id}.refresh"), icon: if p.loading { Icon::Spinner } else { Icon::RotateCw }, act: refresh, destructive: false, size: 28.0, tip: None });
    head
}

pub fn diagnostic_body(ui: &mut Ui, p: &DiagnosticSheet) {
    let pal = ui.pal;
    match (&p.text, p.loading) {
        (None, true) => {
            ui.card_begin();
            ui.card_row_separator();
            ui.draw_ctl(Ctl::Row(vec![Ctl::Spinner(16.0), crate::features::settings::ui::kit::label(pt("diagnostic.running"), pal.muted)], 8.0), dvec2(ui.x + 16.0, ui.y + 16.0));
            ui.y += 52.0;
            ui.card_end();
        }
        (Some(text), _) if !text.is_empty() => {
            let size = ui.measure(Font::Code, 12.0, text, Some(ui.w - 32.0));
            let (x, y, w) = (ui.x, ui.y, ui.w);
            ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, size.y + 24.0) }, pal.surface1, Some(pal.border), 8.0);
            ui.selectable(&format!("diagnostic.{}.{}", p.host, p.provider), Font::Code, 12.0, pal.foreground, dvec2(x + 16.0, y + 12.0), Some(w - 32.0), text);
            ui.y += size.y + 24.0;
        }
        _ => {
            ui.card_begin();
            ui.card_row_separator();
            ui.draw_ctl(crate::features::settings::ui::kit::label(pt("diagnostic.none"), pal.muted), dvec2(ui.x + 16.0, ui.y + 16.0));
            ui.y += 52.0;
            ui.card_end();
        }
    }
}
