//! Original `keyboard-shortcuts-section.tsx` (Flutter
//! `KeyboardShortcutSettings`): one section per help section, effective combo
//! badges, Bind/Rebind/Clear/Reset menu, inline capture and Reset all.

use makepad_widgets::*;
use serde_json::Value;

use super::View;
use crate::features::settings::i18n::{t, ta};
use crate::features::settings::model::shortcuts::{self, Binding, CaptureKey, SECTIONS};
use crate::features::settings::platform::MOBILE;
use crate::features::settings::rpc::{Env, handler};
use crate::features::settings::state::Settings;
use crate::features::settings::ui::draw::{Font, Icon};
use crate::features::settings::ui::kit::{BSize, Ctl, Lead, Ui, Variant, button};
use crate::features::settings::ui::overlay::{Menu, item};

#[derive(Default)]
pub struct ShortcutsState {
    pub bindings: Option<Vec<Binding>>,
    pub capturing: Option<String>,
    pub steps: Vec<String>,
    /// The panel takes key focus on its next event when set.
    pub want_focus: bool,
    pub focus_scope_active: bool,
}

/// A null combo drops the override (default restored); `""` unassigns.
fn persist(s: &mut Settings, env: &mut Env, id: &str, combo: Option<String>) {
    let prefs = s.prefs(&env.preferences());
    let mut overrides = shortcuts::overrides(&prefs);
    match combo {
        Some(combo) => {
            overrides.insert(id.to_string(), Value::String(combo));
        }
        None => {
            overrides.remove(id);
        }
    }
    s.save_pref(env, "keyboardShortcuts", Some(Value::Object(overrides)));
}

fn binding(s: &Settings, id: &str) -> Option<Binding> {
    s.shortcuts.bindings.as_ref()?.iter().find(|b| b.id == id).cloned()
}

/// Panel hook: a key press while a row captures.
pub fn on_key(s: &mut Settings, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let Some(id) = s.shortcuts.capturing.clone() else { return false };
    let Some(binding) = binding(s, &id) else { return false };
    match shortcuts::capture_key(&binding, code, modifiers) {
        CaptureKey::Ignore => {}
        CaptureKey::RemoveLast => {
            s.shortcuts.steps.pop();
        }
        CaptureKey::Step(step) => s.shortcuts.steps.push(step),
    }
    true
}

pub fn cancel_capture(s: &mut Settings) {
    s.shortcuts.capturing = None;
    s.shortcuts.steps.clear();
    s.shortcuts.want_focus = false;
}

pub fn draw_shortcuts(ui: &mut Ui, s: &Settings, v: &View) {
    if MOBILE {
        ui.section_title(&t("settings.sections.shortcuts"), None, None);
        ui.empty_card(&t("settings.shortcuts.unavailableOnMobile"), None, false);
        return;
    }
    let catalog;
    let bindings: &[Binding] = match &s.shortcuts.bindings {
        Some(list) => list,
        None => {
            catalog = shortcuts::catalog();
            &catalog
        }
    };
    let overrides = shortcuts::overrides(&v.prefs);
    let pal = ui.pal;
    let mut first = true;
    for (section, key) in SECTIONS {
        let rows = shortcuts::section_rows(bindings, section);
        if rows.is_empty() {
            continue;
        }
        let trailing = (first && !overrides.is_empty()).then(|| {
            let reset = handler(|s, env| {
                cancel_capture(s);
                s.save_pref(env, "keyboardShortcuts", Some(Value::Object(Default::default())));
            });
            button("shortcuts.resetAll", t("settings.shortcuts.actions.resetAll"), Some(reset)).variant(Variant::Ghost).size(BSize::Sm).ctl()
        });
        first = false;
        ui.section_title(&t(&format!("settings.shortcuts.sections.{key}")), None, trailing);
        ui.card_begin();
        for binding in rows {
            ui.card_row_separator();
            let capturing = s.shortcuts.capturing.as_deref() == Some(binding.id.as_str());
            let combo = shortcuts::effective_combo(&overrides, binding);
            let has_override = overrides.contains_key(&binding.id);
            let complete = shortcuts::capture_complete(binding, &s.shortcuts.steps);
            let keys: Ctl = if capturing {
                if s.shortcuts.steps.is_empty() {
                    Ctl::Label { text: t("settings.shortcuts.capturePrompt"), color: pal.muted, size: 14.0, font: Font::Regular, max_w: 220.0 }
                } else {
                    Ctl::Keys(s.shortcuts.steps.iter().map(|step| shortcuts::format_step(step)).collect())
                }
            } else if combo.is_empty() {
                Ctl::Label { text: t("settings.shortcuts.unassigned"), color: pal.muted, size: 14.0, font: Font::Regular, max_w: 220.0 }
            } else {
                Ctl::Keys(shortcuts::combo_steps(&combo).iter().map(|step| shortcuts::format_step(step)).collect())
            };
            let mut key_col = vec![keys];
            if capturing && !s.shortcuts.steps.is_empty() && !complete {
                key_col.push(Ctl::Label { text: t("settings.shortcuts.captureIndexPrompt"), color: pal.muted, size: 14.0, font: Font::Regular, max_w: 220.0 });
            }
            let mut actions: Vec<Ctl> = Vec::new();
            let id = binding.id.clone();
            if capturing {
                if !s.shortcuts.steps.is_empty() {
                    let save_id = id.clone();
                    let done = complete.then(|| {
                        handler(move |s, env| {
                            let combo = s.shortcuts.steps.join(" ");
                            cancel_capture(s);
                            persist(s, env, &save_id, Some(combo));
                        })
                    });
                    actions.push(button(format!("shortcut.{id}.done"), t("settings.shortcuts.actions.done"), done).variant(Variant::Ghost).ctl());
                }
                actions.push(button(format!("shortcut.{id}.cancel"), t("settings.shortcuts.actions.cancel"), Some(handler(|s, _| cancel_capture(s)))).variant(Variant::Ghost).ctl());
            } else {
                let has_default = !binding.combo.is_empty();
                let label = binding.label.clone();
                let empty_combo = combo.is_empty();
                let menu = handler(move |s, _| {
                    let bind_id = id.clone();
                    let mut items = vec![item(
                        t(if empty_combo { "settings.shortcuts.actions.bind" } else { "settings.shortcuts.actions.rebind" }),
                        handler(move |s, _| {
                            s.shortcuts.capturing = Some(bind_id.clone());
                            s.shortcuts.steps.clear();
                            s.shortcuts.want_focus = true;
                        }),
                    )
                    .lead(Lead::Icon(Icon::Pencil, None))];
                    if !empty_combo {
                        let clear_id = id.clone();
                        items.push(item(t("settings.shortcuts.actions.clear"), handler(move |s, env| persist(s, env, &clear_id, Some(String::new())))).lead(Lead::Icon(Icon::X, None)));
                    }
                    if has_override && has_default {
                        let reset_id = id.clone();
                        items.push(item(t("settings.shortcuts.actions.reset"), handler(move |s, env| persist(s, env, &reset_id, None))).lead(Lead::Icon(Icon::Undo, None)));
                    }
                    let menu = Menu::new(s, ta("settings.shortcuts.actions.menu", &[("name", &label)]), items);
                    s.open_menu(menu);
                });
                actions.push(Ctl::IconButton { key: format!("shortcut.{}.menu", binding.id), icon: Icon::MoreHorizontal, act: Some(menu), destructive: false, size: 32.0, tip: None });
            }
            // Row: label | keys (end-aligned) | actions, 12px padding.
            let key_ctl = Ctl::Col(key_col, 2.0, true);
            let key_size = ui.measure_ctl(&key_ctl);
            let action_ctl = Ctl::Row(actions, 4.0);
            let action_size = ui.measure_ctl(&action_ctl);
            let label_w = (ui.w - 24.0 - key_size.x - action_size.x - 16.0).max(80.0);
            let label_size = ui.measure(Font::Regular, 14.0, &binding.label, Some(label_w));
            let h = label_size.y.max(key_size.y).max(action_size.y) + 24.0;
            let (x, y, w) = (ui.x, ui.y, ui.w);
            if capturing {
                ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, h) }, pal.surface2, None, 0.0);
            }
            ui.text(Font::Regular, 14.0, pal.foreground, dvec2(x + 12.0, y + (h - label_size.y) / 2.0), Some(label_w), &binding.label);
            let ax = x + w - 12.0 - action_size.x;
            ui.draw_ctl(action_ctl, dvec2(ax, y + (h - action_size.y) / 2.0));
            ui.draw_ctl(key_ctl, dvec2(ax - 8.0 - key_size.x, y + (h - key_size.y) / 2.0));
            ui.y += h;
        }
        ui.card_end();
        ui.gap(24.0);
    }
}

/// Loads the catalog once the page is shown.
pub fn ensure(s: &mut Settings) {
    if s.shortcuts.bindings.is_none() {
        s.shortcuts.bindings = Some(shortcuts::catalog());
    }
}
