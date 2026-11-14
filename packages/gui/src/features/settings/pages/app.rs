//! App preference pages from `app_preferences.dart`: General, Chat, Sidebar
//! and Files. Every write saves the whole preferences object.

use std::rc::Rc;

use serde_json::{Value, json};

use super::View;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::json::{arr, obj, str_of};
use crate::features::settings::model::preferences::{file_panel_rows, language_preference, nav_rows, selector_label, sidebar_nav_preferences};
use crate::features::settings::platform::{DESKTOP, MOBILE};
use crate::features::settings::rpc::{Env, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::Icon;
use crate::features::settings::ui::kit::{BSize, Ctl, Lead, Ui, Variant, button, row, switch};
use crate::features::settings::ui::overlay::{Menu, item};

pub type Pick = Rc<dyn Fn(&mut Settings, &mut Env, String)>;

/// A `SettingsDropdown` trigger opening an end-aligned menu of `options`.
pub fn select_ctl(key: &str, title: String, value: &str, options: Vec<(String, String)>, width: f64, enabled: bool, on_pick: Pick) -> Ctl {
    select_ctl_lead(key, title, value, options, Vec::new(), width, enabled, on_pick)
}

#[allow(clippy::too_many_arguments)]
pub fn select_ctl_lead(
    key: &str,
    title: String,
    value: &str,
    options: Vec<(String, String)>,
    leads: Vec<(String, Lead, bool)>,
    width: f64,
    enabled: bool,
    on_pick: Pick,
) -> Ctl {
    let label = options.iter().find(|(v, _)| v == value).map(|(_, l)| l.clone()).unwrap_or_else(|| value.to_string());
    let current_lead = leads.iter().find(|(v, _, _)| v == value).map(|(_, l, _)| l.clone());
    let current = value.to_string();
    let act = enabled.then(|| {
        handler(move |s, _| {
            let items = options
                .iter()
                .map(|(v, l)| {
                    let pick = on_pick.clone();
                    let picked = v.clone();
                    let mut entry = item(l.clone(), handler(move |s, env| pick(s, env, picked.clone()))).selected(*v == current);
                    if let Some((_, lead, separator)) = leads.iter().find(|(id, _, _)| id == v) {
                        entry = entry.lead(lead.clone()).separator(*separator);
                    }
                    entry
                })
                .collect();
            let menu = Menu::new(s, title.clone(), items).width(width).popover();
            s.open_menu(menu);
        })
    });
    Ctl::Dropdown { key: key.to_string(), label, lead: current_lead, act, max_w: 220.0 }
}

pub fn pref_pick(key: &'static str) -> Pick {
    Rc::new(move |s: &mut Settings, env: &mut Env, value: String| s.save_pref(env, key, Some(Value::String(value))))
}

fn pref_switch(ui: &mut Ui, v: &View, key: &'static str, label: String, hint: Option<String>, fallback: bool) {
    let on = v.prefs.get(key).and_then(Value::as_bool).unwrap_or(fallback);
    let act = handler(move |s, env| s.save_pref(env, key, Some(Value::Bool(!on))));
    ui.row(row(label).hint_opt(hint).control(switch(format!("pref.{key}"), on, Some(act))));
}

fn card_section(ui: &mut Ui, title: &str, body: impl FnOnce(&mut Ui)) {
    ui.section_title(title, None, None);
    ui.card_begin();
    body(ui);
    ui.card_end();
    ui.gap(24.0);
}

pub fn draw_app_section(ui: &mut Ui, s: &Settings, v: &View, section: &str) {
    match section {
        "sidebar" => draw_sidebar(ui, s, v),
        "chat" => draw_chat(ui, v),
        "files" => draw_files(ui, v),
        _ => draw_general(ui, v),
    }
}

fn draw_general(ui: &mut Ui, v: &View) {
    let language = language_preference(v.prefs.get("language").unwrap_or(&Value::Null));
    card_section(ui, &t("settings.general.title"), |ui| {
        let options = ["system", "en", "ko"].iter().map(|o| (o.to_string(), t(&format!("settings.general.language.options.{o}")))).collect();
        let ctl = select_ctl("pref.language", t("settings.general.language.label"), language, options, 300.0, true, pref_pick("language"));
        ui.row(row(t("settings.general.language.label")).hint(t("settings.general.language.description")).control(ctl));
    });
    let send = match v.prefs.get("sendBehavior").and_then(Value::as_str) {
        Some(value @ ("interrupt" | "steer" | "queue")) => value.to_string(),
        _ => "steer".to_string(),
    };
    card_section(ui, &t("settings.general.sending"), |ui| {
        let options = ["interrupt", "steer", "queue"].iter().map(|o| (o.to_string(), t(&format!("settings.general.defaultSend.options.{o}")))).collect();
        let ctl = select_ctl("pref.sendBehavior", t("settings.general.defaultSend.label"), &send, options, 220.0, true, pref_pick("sendBehavior"));
        ui.row(row(t("settings.general.defaultSend.label")).hint(t(&format!("settings.general.defaultSend.descriptions.{send}"))).control(ctl));
    });
    card_section(ui, &t("settings.sections.terminal"), |ui| draw_scrollback(ui, v));
    if DESKTOP {
        let open = v.prefs.get("openInSidePane").cloned().unwrap_or_else(|| json!({}));
        let destinations = vec![
            ("main".to_string(), t("settings.layout.openInSidePane.destinations.main")),
            ("side".to_string(), t("settings.layout.openInSidePane.destinations.side")),
        ];
        card_section(ui, &t("settings.layout.openInSidePane.title"), |ui| {
            for source in ["explorerFiles", "diffs", "chatFiles", "diffFiles", "subagents"] {
                let value = if open.get(source).and_then(Value::as_bool) == Some(true) { "side" } else { "main" };
                let label = t(&format!("settings.layout.openInSidePane.sources.{source}.label"));
                let base = open.clone();
                let pick: Pick = Rc::new(move |s, env, picked| {
                    let mut next = base.as_object().cloned().unwrap_or_default();
                    next.insert(source.into(), Value::Bool(picked == "side"));
                    s.save_pref(env, "openInSidePane", Some(Value::Object(next)));
                });
                let ctl = select_ctl(&format!("pref.openInSidePane.{source}"), label.clone(), value, destinations.clone(), 220.0, true, pick);
                ui.row(row(label).control(ctl));
            }
            let location = match v.prefs.get("pullRequestOpenLocation").and_then(Value::as_str) {
                Some(value) if !value.is_empty() => value.to_string(),
                _ => "explorer".into(),
            };
            let mut options = destinations.clone();
            options.push(("explorer".into(), t("settings.layout.openInSidePane.destinations.explorer")));
            let label = t("settings.layout.openInSidePane.sources.pullRequests.label");
            let ctl = select_ctl("pref.pullRequestOpenLocation", label.clone(), &location, options, 220.0, true, pref_pick("pullRequestOpenLocation"));
            ui.row(row(label).control(ctl));
        });
    }
}

/// Scrollback presets; a stored value outside them stays selectable.
const SCROLLBACK_PRESETS: [usize; 6] = [1_000, 5_000, 10_000, 50_000, 100_000, 1_000_000];

/// `1000000` → `1,000,000`.
fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `settings.terminalScrollbackLines` (read like the terminal does).
fn draw_scrollback(ui: &mut Ui, v: &View) {
    let current = crate::features::terminal::session::scrollback_preference(&v.prefs);
    let mut values = SCROLLBACK_PRESETS.to_vec();
    if !values.contains(&current) {
        values.push(current);
        values.sort_unstable();
    }
    let options = values.iter().map(|n| (n.to_string(), ta("settingsNative.scrollbackLines", &[("count", &grouped(*n))]))).collect();
    let pick: Pick = Rc::new(|s, env, picked| {
        if let Ok(lines) = picked.parse::<u64>() {
            s.save_pref(env, "terminalScrollbackLines", Some(Value::from(lines)));
        }
    });
    let label = t("settings.general.terminalScrollback.label");
    let ctl = select_ctl("pref.terminalScrollbackLines", label.clone(), &current.to_string(), options, 220.0, true, pick);
    ui.row(row(label).hint(t("settings.general.terminalScrollback.description")).control(ctl));
}

fn draw_chat(ui: &mut Ui, v: &View) {
    card_section(ui, &t("settings.appearance.detailLevel.title"), |ui| {
        pref_switch(ui, v, "autoExpandReasoning", t("settings.general.autoExpandReasoning.label"), Some(t("settings.general.autoExpandReasoning.description")), false);
        let detail = if str_of(&v.prefs, "toolCallDetailLevel") == "overview" { "overview" } else { "detailed" };
        let options = ["detailed", "overview"].iter().map(|o| (o.to_string(), t(&format!("settings.general.toolCallDetail.options.{o}")))).collect();
        let ctl = select_ctl("pref.toolCallDetailLevel", t("settings.general.toolCallDetail.label"), detail, options, 220.0, true, pref_pick("toolCallDetailLevel"));
        ui.row(row(t("settings.general.toolCallDetail.label")).hint(t("settings.general.toolCallDetail.description")).control(ctl));
        if !MOBILE {
            let on = v.prefs.get("chatOutlineEnabled").and_then(Value::as_bool) != Some(false);
            let act = handler(move |s, env| s.save_pref(env, "chatOutlineEnabled", Some(Value::Bool(!on))));
            ui.row(row(t("settings.appearance.chatOutline.title")).hint(t("settings.appearance.chatOutline.description")).control(switch("pref.chatOutlineEnabled", on, Some(act))));
        }
    });
}

fn nav_label(key: &str) -> String {
    t(match key {
        "new-workspace" => "sessionNavigation.newSession",
        "history" => "sidebar.sections.sessions",
        "search" => "sidebar.sections.search",
        "schedules" => "sidebar.sections.schedules",
        _ => "sidebar.footer.usage",
    })
}

fn nav_icon(key: &str) -> Icon {
    match key {
        "new-workspace" => Icon::Plus,
        "history" => Icon::History,
        "search" => Icon::Search,
        "schedules" => Icon::CalendarClock,
        _ => Icon::Gauge,
    }
}

fn nav_card(ui: &mut Ui, s: &Settings, v: &View, pref_key: &'static str, defaults: &'static [&'static str], title: String, info: String) {
    let _ = s;
    let stored: Vec<Value> = arr(&v.prefs, pref_key).to_vec();
    let rows = nav_rows(&stored, defaults);
    let info_key = format!("pref.{pref_key}.info");
    ui.section_title(&title, Some((info_key, info.clone())), None);
    ui.card_begin();
    let muted = ui.pal.muted;
    for (i, entry) in rows.iter().enumerate() {
        let id = str_of(entry, "key").to_string();
        let visible = entry.get("visible").and_then(Value::as_bool) != Some(false);
        let persist = |next: Vec<Value>, stored: Vec<Value>| {
            handler(move |s, env| s.save_pref(env, pref_key, Some(Value::Array(sidebar_nav_preferences(&next, &stored)))))
        };
        let up = (i > 0).then(|| {
            let mut next = rows.clone();
            let moved = next.remove(i);
            next.insert(i - 1, moved);
            persist(next, stored.clone())
        });
        let down = (i + 1 < rows.len()).then(|| {
            let mut next = rows.clone();
            let moved = next.remove(i);
            next.insert(i + 1, moved);
            persist(next, stored.clone())
        });
        let mut toggled = rows.clone();
        toggled[i]["visible"] = Value::Bool(!visible);
        let toggle = persist(toggled, stored.clone());
        let controls = Ctl::Row(
            vec![
                Ctl::IconButton { key: format!("pref.{pref_key}.{id}.up"), icon: Icon::ArrowUp, act: up, destructive: false, size: 32.0, tip: None },
                Ctl::IconButton { key: format!("pref.{pref_key}.{id}.down"), icon: Icon::ArrowDown, act: down, destructive: false, size: 32.0, tip: None },
                switch(format!("pref.{pref_key}.{id}.visible"), visible, Some(toggle)),
            ],
            8.0,
        );
        ui.row(row(nav_label(&id)).one_line().leading(Ctl::Glyph(nav_icon(&id), 16.0, muted)).control(controls));
    }
    ui.card_end();
    ui.gap(24.0);
}

fn draw_sidebar(ui: &mut Ui, s: &Settings, v: &View) {
    nav_card(
        ui,
        s,
        v,
        "sidebarNavItems",
        &["new-workspace", "history", "search", "schedules"],
        t("settings.appearance.sidebar.header.title"),
        t("settings.appearance.sidebar.header.description"),
    );
    nav_card(ui, s, v, "sidebarFooterItems", &["usage"], t("settings.appearance.sidebar.footer.title"), t("settings.appearance.sidebar.footer.description"));
    let row_items = obj(&v.prefs, "sidebarRowItems").clone();
    card_section(ui, &t("sidebar.display.heading"), |ui| {
        let title_source = if str_of(&v.prefs, "workspaceTitleSource") == "branch" { "branch" } else { "title" };
        let options = vec![("title".into(), t("sidebar.display.titleSource.title")), ("branch".into(), t("sidebar.display.titleSource.branch"))];
        let ctl = select_ctl("pref.workspaceTitleSource", t("sidebar.display.titleSource.label"), title_source, options, 220.0, true, pref_pick("workspaceTitleSource"));
        ui.row(row(t("sidebar.display.titleSource.label")).control(ctl));
        for entry in ["branch", "project", "host", "changeRequest", "services", "labels"] {
            let on = row_items.get(entry).and_then(Value::as_bool) != Some(false);
            let base = row_items.clone();
            let act = handler(move |s, env| {
                let mut next = base.clone();
                next.insert(entry.into(), Value::Bool(!on));
                s.save_pref(env, "sidebarRowItems", Some(Value::Object(next)));
            });
            ui.row(row(t(&format!("sidebar.display.show.{entry}"))).control(switch(format!("pref.sidebarRowItems.{entry}"), on, Some(act))));
        }
        let checks = match v.prefs.get("sidebarChecksDisplay").and_then(Value::as_str) {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => "iconAndText".into(),
        };
        let options = ["iconAndText", "icon", "none"].iter().map(|o| (o.to_string(), t(&format!("sidebar.display.checks.{o}")))).collect();
        let ctl = select_ctl("pref.sidebarChecksDisplay", t("sidebar.display.show.checks"), &checks, options, 220.0, true, pref_pick("sidebarChecksDisplay"));
        ui.row(row(t("sidebar.display.show.checks")).control(ctl));
        let trailing = match v.prefs.get("sidebarWorkspaceTrailing").and_then(Value::as_str) {
            Some(value) if !value.is_empty() => value.to_string(),
            _ => "timestamp".into(),
        };
        let options = vec![
            ("timestamp".into(), t("sidebar.display.show.timestamp")),
            ("diff".into(), t("sidebar.display.show.diff")),
            ("none".into(), t("sidebar.display.checks.none")),
        ];
        let ctl = select_ctl("pref.sidebarWorkspaceTrailing", t("sidebar.display.show.timestamp"), &trailing, options, 220.0, true, pref_pick("sidebarWorkspaceTrailing"));
        ui.row(row(t("sidebar.display.show.timestamp")).control(ctl));
    });
}

fn panel_label(id: &str) -> String {
    let name = id.strip_prefix("builtin:").unwrap_or(id);
    ta(
        "panels.file.filePanels.option",
        &[("title", &t(&format!("panels.file.filePanels.builtin.{name}"))), ("owner", &t("panels.file.filePanels.builtin.owner"))],
    )
}

fn draw_files(ui: &mut Ui, v: &View) {
    let associations = obj(&v.prefs, "filePanelAssociations").clone();
    let rows = file_panel_rows(&associations);
    card_section(ui, &t("settings.files.title"), |ui| {
        if rows.is_empty() {
            let muted = ui.pal.muted;
            ui.card_row_separator();
            let w = ui.w - 32.0;
            let text = t("settings.files.empty");
            let size = ui.measure(crate::features::settings::ui::draw::Font::Regular, 14.0, &text, Some(w));
            let (x, y) = (ui.x + (ui.w - size.x) / 2.0, ui.y + 16.0);
            ui.text(crate::features::settings::ui::draw::Font::Regular, 14.0, muted, makepad_widgets::dvec2(x, y), Some(w), &text);
            ui.gap(size.y + 32.0);
        }
        for (key, candidates, associated) in rows {
            let label = selector_label(&key);
            let current = candidates.iter().find(|c| Some(c.id) == associated.as_deref());
            let value_label = match (&associated, current) {
                (None, _) => t("settings.files.askEveryTime"),
                (Some(_), None) => t("settings.files.unavailable"),
                (Some(_), Some(panel)) => panel_label(panel.id),
            };
            let mut options: Vec<(String, String)> = Vec::new();
            if current.is_none() {
                options.push((String::new(), value_label.clone()));
            }
            for panel in &candidates {
                options.push((panel.id.to_string(), panel_label(panel.id)));
            }
            let base = associations.clone();
            let selector = key.clone();
            let pick: Pick = Rc::new(move |s, env, id| {
                if id.is_empty() {
                    return;
                }
                let mut next = base.clone();
                next.insert(selector.clone(), Value::String(id));
                s.save_pref(env, "filePanelAssociations", Some(Value::Object(next)));
            });
            let value = current.map(|c| c.id.to_string()).unwrap_or_default();
            let dropdown = select_ctl(&format!("pref.files.{key}"), format!("{label}: {value_label}"), &value, options, 260.0, !candidates.is_empty(), pick);
            let base = associations.clone();
            let forget = key.clone();
            let ask_again = associated.is_some().then(|| {
                handler(move |s, env| {
                    let mut next = base.clone();
                    next.remove(&forget);
                    s.save_pref(env, "filePanelAssociations", Some(Value::Object(next)));
                })
            });
            let again = button(format!("pref.files.{key}.again"), t("settings.files.askAgain"), ask_again).size(BSize::Sm).variant(Variant::Outline).ctl();
            ui.row(row(label).control(Ctl::Row(vec![dropdown, again], 8.0)));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::grouped;

    #[test]
    fn groups_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(50_000), "50,000");
        assert_eq!(grouped(1_000_000), "1,000,000");
    }
}
