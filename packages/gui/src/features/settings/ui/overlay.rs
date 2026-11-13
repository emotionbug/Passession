//! Overlay surfaces: anchored menus (`showPaseoMenu` / `PaseoSelectField`),
//! adaptive sheets (`showAdaptiveSheet` + `SheetContents`) and the generic
//! form / report / confirm sheets. Wide layouts get a centered 12px-radius
//! dialog over a 55% scrim; compact layouts a bottom sheet.


use makepad_widgets::*;

use super::draw::{Font, Icon};
use super::field::FieldSpec;
use super::kit::{BSize, Ctl, Lead, Ui, Variant, button};
use crate::features::settings::i18n::t;
use crate::features::settings::model::forms::{FormSheet, Kind};
use crate::features::settings::model::match_score::rank;
use crate::features::settings::rpc::{Handler, handler};
use crate::features::settings::state::{Settings, Tip};

pub struct MenuItem {
    pub label: String,
    pub description: Option<String>,
    pub hint: Option<String>,
    pub lead: Option<Lead>,
    pub enabled: bool,
    pub destructive: bool,
    pub selected: bool,
    pub separator_before: bool,
    pub on: Option<Handler>,
}

pub fn item(label: impl Into<String>, on: Handler) -> MenuItem {
    MenuItem {
        label: label.into(),
        description: None,
        hint: None,
        lead: None,
        enabled: true,
        destructive: false,
        selected: false,
        separator_before: false,
        on: Some(on),
    }
}

impl MenuItem {
    pub fn description(mut self, text: Option<String>) -> Self {
        self.description = text.filter(|t| !t.is_empty());
        self
    }
    pub fn hint(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.hint = if text.is_empty() { None } else { Some(text) };
        self
    }
    pub fn lead(mut self, lead: Lead) -> Self {
        self.lead = Some(lead);
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn separator(mut self, before: bool) -> Self {
        self.separator_before = before;
        self
    }
}

pub struct Menu {
    pub id: u64,
    pub title: String,
    pub anchor: Rect,
    pub width: f64,
    pub align_end: bool,
    pub items: Vec<MenuItem>,
    pub searchable: bool,
    pub search_placeholder: String,
    pub empty_text: String,
    pub scroll: f64,
    pub style: MenuStyle,
    /// `showPaseoMenu(compactSheet:)`: compact layouts show a bottom sheet.
    pub compact_sheet: bool,
}

/// Which Flutter menu a [`Menu`] reproduces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuStyle {
    /// `showPaseoMenu` (dropdown triggers, `SettingsMenuButton`, host picker).
    Menu,
    /// `showPaseoSelect` (`PaseoSelectField`).
    Select,
}

impl Menu {
    pub fn new(s: &mut Settings, title: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            id: s.next_id(),
            title: title.into(),
            anchor: s.anchor,
            width: 220.0,
            align_end: true,
            items,
            searchable: false,
            search_placeholder: String::new(),
            empty_text: String::new(),
            scroll: 0.0,
            style: MenuStyle::Menu,
            compact_sheet: true,
        }
    }
    /// `PaseoSelectField` picker (see [`MenuStyle::Select`]).
    pub fn select(mut self) -> Self {
        self.style = MenuStyle::Select;
        self
    }
    /// `compactSheet: false`: a popover on compact layouts too.
    pub fn popover(mut self) -> Self {
        self.compact_sheet = false;
        self
    }
    pub fn width(mut self, width: f64) -> Self {
        self.width = width;
        self
    }
    pub fn align_start(mut self) -> Self {
        self.align_end = false;
        self
    }
    pub fn searchable(mut self, on: bool, placeholder: String) -> Self {
        self.searchable = on;
        self.search_placeholder = placeholder;
        self
    }
    pub fn empty(mut self, text: String) -> Self {
        self.empty_text = text;
        self
    }
}

pub struct Confirm {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub destructive: bool,
    pub on_confirm: Handler,
    /// Runs when the dialog closes without confirming (cancel, scrim, close, Escape).
    pub on_decline: Option<Handler>,
}

/// What a sheet kind shows around its body.
pub struct SheetHead {
    pub title: String,
    pub header_actions: Vec<Ctl>,
    pub actions: Vec<Ctl>,
    pub stretch_actions: bool,
    /// Busy sheets ignore the scrim tap.
    pub dismissible: bool,
    /// `SheetContents(header:)` search row: field id and placeholder.
    pub search: Option<(String, String)>,
    /// Muted 12px text leading the footer row (provider sheet "Updated …").
    pub footer_meta: Option<String>,
    /// Compact layouts stack the footer: the meta line over full-width buttons, 8px apart.
    pub stack_compact: bool,
}

impl SheetHead {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            header_actions: Vec::new(),
            actions: Vec::new(),
            stretch_actions: false,
            dismissible: true,
            search: None,
            footer_meta: None,
            stack_compact: false,
        }
    }
}

/// Footer height (rule, 12px insets and the action row or stacked column).
fn footer_h(ui: &mut Ui, head: &SheetHead) -> f64 {
    if head.actions.is_empty() {
        return 0.0;
    }
    let heights: Vec<f64> = head.actions.iter().map(|a| ui.measure_ctl(a).y).collect();
    let meta_h = head.footer_meta.as_ref().filter(|m| !m.is_empty()).map(|m| ui.measure(Font::Regular, 12.0, m, None).y);
    let body = if ui.compact && head.stack_compact {
        let items = heights.len() + usize::from(meta_h.is_some());
        heights.iter().sum::<f64>() + meta_h.unwrap_or(0.0) + 8.0 * (items.saturating_sub(1)) as f64
    } else {
        heights.iter().copied().fold(meta_h.unwrap_or(0.0), f64::max)
    };
    1.0 + 12.0 + body + 12.0
}

fn close_menu() -> Handler {
    handler(|s, _| s.menu = None)
}

/// Visible rows of `menu` for the current search query (ranked when searching).
fn visible_items(ui: &mut Ui, menu: &Menu) -> (Vec<usize>, bool) {
    let query = if menu.searchable { ui.fields.text(&format!("menu.{}.search", menu.id)) } else { String::new() };
    if query.trim().is_empty() {
        return ((0..menu.items.len()).collect(), false);
    }
    let indices: Vec<usize> = (0..menu.items.len()).collect();
    let ranked = rank(&indices, &query, |i| {
        let item = &menu.items[*i];
        vec![item.label.clone(), item.description.clone().unwrap_or_default()]
    });
    (ranked, true)
}

/// Flutter popover `BoxShadow(offset: (0, 4), blurRadius: blur)`. There is
/// no blur primitive: four stacked rounded boxes, each a quarter of the
/// shadow alpha and `blur / 4` wider, approximate the falloff.
fn popover_shadow(ui: &mut Ui, rect: Rect, color: Vec4, blur: f64) {
    let layer = vec4(color.x, color.y, color.z, color.w / 4.0);
    for i in 1..=4 {
        let grow = blur * i as f64 / 8.0;
        let shadow = Rect { pos: dvec2(rect.pos.x - grow, rect.pos.y + 4.0 - grow), size: dvec2(rect.size.x + grow * 2.0, rect.size.y + grow * 2.0) };
        ui.rect(shadow, layer, None, 8.0 + grow);
    }
}

/// Compact bottom-sheet surface shared by both menu styles: the 55% barrier
/// over `screen`, the top-rounded surface0 panel at the bottom of `view` and
/// its 1px rule; returns the panel rect.
fn bottom_sheet(ui: &mut Ui, screen: Rect, view: Rect, h: f64, radius: f64, border: bool) -> Rect {
    let pal = ui.pal;
    let rect = Rect { pos: dvec2(view.pos.x, view.pos.y + view.size.y - h), size: dvec2(view.size.x, h) };
    ui.rect(screen, vec4(0.0, 0.0, 0.0, 0x73 as f32 / 255.0), None, 0.0);
    if border {
        ui.rect_top(rect, pal.border, radius);
        let inner = Rect { pos: dvec2(rect.pos.x + 1.0, rect.pos.y + 1.0), size: dvec2(rect.size.x - 2.0, rect.size.y - 1.0) };
        ui.rect_top(inner, pal.surface0, (radius - 1.0).max(0.0));
    } else {
        ui.rect_top(rect, pal.surface0, radius);
    }
    surface_below(ui, rect, screen, pal.surface0);
    rect
}

/// Continues a bottom-anchored surface under the keyboard and system bars
/// below the viewport, so that strip shows the sheet rather than the barrier
/// and taps on it stay with the sheet.
fn surface_below(ui: &mut Ui, rect: Rect, screen: Rect, color: Vec4) {
    let top = rect.pos.y + rect.size.y;
    let gap = screen.pos.y + screen.size.y - top;
    if gap > 0.0 {
        let below = Rect { pos: dvec2(rect.pos.x, top), size: dvec2(rect.size.x, gap) };
        ui.rect(below, color, None, 0.0);
        ui.zone("sheet.below", below, Some(handler(|_, _| {})));
    }
}

/// Draws the open menu over everything else, laid out inside `view` (the
/// window less safe areas and the keyboard); returns the scrollable list
/// rect and its content height.
pub fn draw_menu(ui: &mut Ui, menu: &Menu, screen: Rect, view: Rect) -> (Rect, f64) {
    ui.zone(&format!("menu.{}.backdrop", menu.id), screen, Some(close_menu()));
    match menu.style {
        MenuStyle::Menu => draw_paseo_menu(ui, menu, screen, view),
        MenuStyle::Select => draw_select(ui, menu, screen, view),
    }
}

/// Flutter `showPaseoMenu` + `PaseoMenuItem` (original `MenuSurface` /
/// `MenuItem`): a surface1 popover with a borderAccent rule, 8px radius,
/// 4px page inset and 28px rows (40px on compact), each inset 4px with
/// 8/4 padding, a 6px surface2 hover, a 16px leading slot, 14px label on an
/// 18px line (two lines at most), trailing muted hint and a 16px check.
/// Compact layouts with `compact_sheet` get a bottom sheet with a drag
/// handle and the title header instead.
fn draw_paseo_menu(ui: &mut Ui, menu: &Menu, screen: Rect, view: Rect) -> (Rect, f64) {
    let pal = ui.pal;
    let sheet = menu.compact_sheet && ui.compact;
    let row_min = if ui.compact { 40.0 } else { 28.0 };
    let width = if sheet { view.size.x } else { menu.width.min(view.size.x - 16.0) };
    let (visible, _) = visible_items(ui, menu);
    // Row geometry: 4px margin + 8px padding on each side.
    let content_x = 4.0 + 8.0;
    let content_w = width - content_x * 2.0;
    let mut rows = Vec::with_capacity(visible.len());
    for &i in &visible {
        let item = &menu.items[i];
        let hint_w = item.hint.as_ref().map(|h| ui.measure(Font::Regular, 12.0, h, None).x + 8.0).unwrap_or(0.0);
        let lead_w = if item.lead.is_some() { 24.0 } else { 0.0 };
        let check_w = if item.selected { 24.0 } else { 0.0 };
        let label_w = (content_w - lead_w - hint_w - check_w).max(1.0);
        let text_h = ui.measure(Font::Regular, 14.0, &item.label, Some(label_w)).y;
        let h = (text_h + 8.0 + 2.0).max(row_min);
        rows.push((h, label_w, text_h));
    }
    let list_h: f64 = 8.0 + visible.iter().zip(&rows).map(|(&i, (h, ..))| h + if menu.items[i].separator_before { 9.0 } else { 0.0 }).sum::<f64>();
    let header_h = if sheet { 24.0 + if menu.title.is_empty() { 0.0 } else { 16.0 + ui.measure(Font::Medium, 14.0, &menu.title, None).y } } else { 0.0 };
    let (rect, list) = if sheet {
        let max_h = view.size.y * 0.8;
        let total = (header_h + list_h + 4.0).min(max_h);
        let rect = bottom_sheet(ui, screen, view, total, 12.0, true);
        // Drag handle: 36x4 surface2, 10px above and below.
        ui.rect(Rect { pos: dvec2(rect.pos.x + (rect.size.x - 36.0) / 2.0, rect.pos.y + 10.0), size: dvec2(36.0, 4.0) }, pal.surface2, None, 2.0);
        if !menu.title.is_empty() {
            ui.text_line(Font::Medium, 14.0, pal.foreground, dvec2(rect.pos.x + 12.0, rect.pos.y + 24.0 + 4.0), rect.size.x - 24.0, &menu.title);
        }
        let list = Rect { pos: dvec2(rect.pos.x, rect.pos.y + header_h), size: dvec2(rect.size.x, (total - header_h - 4.0).max(0.0)) };
        (rect, list)
    } else {
        let total = list_h.min(view.size.y - 16.0);
        let a = menu.anchor;
        let x = if menu.align_end { a.pos.x + a.size.x - width } else { a.pos.x };
        let below = a.pos.y + a.size.y + 4.0;
        let y = if below + total <= view.pos.y + view.size.y - 8.0 { below } else { a.pos.y - 4.0 - total };
        let x = x.clamp(view.pos.x + 8.0, (view.pos.x + view.size.x - width - 8.0).max(view.pos.x + 8.0));
        let y = y.clamp(view.pos.y + 8.0, (view.pos.y + view.size.y - total - 8.0).max(view.pos.y + 8.0));
        let rect = Rect { pos: dvec2(x, y), size: dvec2(width, total) };
        let (shadow, blur) = if pal.dark { (0x33, 8.0) } else { (0x0a, 16.0) };
        popover_shadow(ui, rect, vec4(0.0, 0.0, 0.0, shadow as f32 / 255.0), blur);
        ui.rect(rect, pal.surface1, Some(pal.border_accent), 8.0);
        (rect, rect)
    };
    ui.zone(&format!("menu.{}.panel", menu.id), rect, Some(handler(|_, _| {})));
    ui.cx.begin_turtle(Walk::fixed(list.size.x, list.size.y).with_abs_pos(list.pos), Layout::flow_overlay().with_clip(true, true));
    ui.set_clip(Some(list));
    let scroll = menu.scroll.clamp(0.0, (list_h - list.size.y).max(0.0));
    let mut y = list.pos.y + 4.0 - scroll;
    for (row, &i) in visible.iter().enumerate() {
        let item = &menu.items[i];
        if item.separator_before {
            ui.rect(Rect { pos: dvec2(list.pos.x, y + 4.0), size: dvec2(list.size.x, 1.0) }, pal.border_accent, None, 0.0);
            y += 9.0;
        }
        let (h, label_w, text_h) = rows[row];
        let item_rect = Rect { pos: dvec2(list.pos.x + 4.0, y), size: dvec2(list.size.x - 8.0, h) };
        let k = format!("menu.{}.item.{i}", menu.id);
        let (hovered, pressed) = ui.state(&k);
        if item.enabled && (hovered || pressed) {
            ui.rect(item_rect, pal.surface2, None, 6.0);
        }
        let fade = if item.enabled { 1.0 } else { 0.5 };
        let fg = super::theme::alpha(if item.destructive { pal.danger } else { pal.foreground }, fade);
        let muted = super::theme::alpha(pal.muted, fade);
        let mut x = item_rect.pos.x + 8.0;
        if let Some(lead) = item.lead.clone() {
            ui.lead(lead, dvec2(x, y + (h - 16.0) / 2.0), 16.0);
            x += 24.0;
        }
        let ty = y + (h - text_h) / 2.0;
        ui.text(Font::Regular, 14.0, fg, dvec2(x, ty), Some(label_w), &item.label);
        let mut right = item_rect.pos.x + item_rect.size.x - 8.0;
        if item.selected {
            right -= 16.0;
            ui.icon(Icon::Check, dvec2(right, y + (h - 16.0) / 2.0), 16.0, muted);
            right -= 8.0;
        }
        if let Some(hint) = &item.hint {
            let hs = ui.measure(Font::Regular, 12.0, hint, None);
            ui.text(Font::Regular, 12.0, muted, dvec2(right - hs.x, y + (h - hs.y) / 2.0), None, hint);
        }
        if item.enabled
            && let Some(on) = item.on.clone()
        {
            let run = handler(move |s, env| {
                s.menu = None;
                on(s, env);
            });
            ui.zone(&k, item_rect, Some(run));
        }
        y += h;
    }
    ui.set_clip(None);
    ui.cx.end_turtle();
    (list, list_h)
}

/// Flutter `showPaseoSelect` + `_SelectList` / `_SelectItem` (original
/// `SelectField` popover): at least 220px or the trigger's width, opening
/// up when fewer than 240px remain below, 120–400px tall, a surface0 panel
/// with a divider rule and 8px radius; the optional surface1 search row
/// (16px glyph, bare 14px field, 12px vertical padding, divider below);
/// 36px rows with 12/8 padding, a surface1 hover, a 16px leading slot, the
/// 14px label and the muted 12px description on one ellipsized line, a 16px
/// check, 55% opacity when disabled. Compact layouts get a bottom sheet with
/// the Material drag handle and the medium title.
fn draw_select(ui: &mut Ui, menu: &Menu, screen: Rect, view: Rect) -> (Rect, f64) {
    let pal = ui.pal;
    let sheet = ui.compact;
    let (visible, searching) = visible_items(ui, menu);
    let a = menu.anchor;
    let width = if sheet { view.size.x } else { a.size.x.max(220.0).min(view.size.x - 16.0) };
    let separators = |i: usize| menu.items[i].separator_before && !searching;
    let row_h = visible.iter().map(|&i| ui.measure(Font::Regular, 14.0, &menu.items[i].label, None).y + 12.0).fold(36.0_f64, f64::max);
    let rows_h: f64 = visible.iter().map(|&i| row_h + if sheet { 4.0 } else { 0.0 } + if separators(i) { 9.0 } else { 0.0 }).sum();
    let list_h = if visible.is_empty() { 8.0 + ui.measure(Font::Regular, 14.0, &menu.empty_text, Some(width - 32.0)).y + 8.0 } else { rows_h + if sheet { 4.0 + 32.0 } else { 8.0 } };
    let search_field_h = 20.0_f64.max(ui.measure(Font::Regular, 14.0, "Ag", None).y);
    let search_h = if menu.searchable { search_field_h + 25.0 } else { 0.0 };
    let (rect, top) = if sheet {
        // Material drag handle area (48px) then the title (8px below).
        let title_h = ui.measure(Font::Medium, 14.0, &menu.title, None).y + 8.0;
        let total = (48.0 + title_h + search_h + list_h).min(view.size.y * 0.9);
        let rect = bottom_sheet(ui, screen, view, total, 16.0, false);
        ui.rect(Rect { pos: dvec2(rect.pos.x + (rect.size.x - 32.0) / 2.0, rect.pos.y + 22.0), size: dvec2(32.0, 4.0) }, super::theme::alpha(pal.muted, 0.4), None, 2.0);
        ui.text_line(Font::Medium, 14.0, pal.foreground, dvec2(rect.pos.x + 24.0, rect.pos.y + 48.0), rect.size.x - 48.0, &menu.title);
        (rect, rect.pos.y + 48.0 + title_h)
    } else {
        let below = view.pos.y + view.size.y - (a.pos.y + a.size.y) - 8.0;
        let above = a.pos.y - view.pos.y;
        let open_up = below < 240.0 && above > below;
        // At least 120px when the viewport has it; never past its edge.
        let room = if open_up { above - 12.0 } else { below - 4.0 };
        let max_h = room.clamp(120.0_f64.min(room.max(0.0)), 400.0);
        let total = (search_h + list_h).min(max_h);
        let left = if a.pos.x + width > view.pos.x + view.size.x - 8.0 { (view.pos.x + view.size.x - 8.0 - width).max(view.pos.x + 8.0) } else { a.pos.x };
        let y = if open_up { a.pos.y - 4.0 - total } else { a.pos.y + a.size.y + 4.0 };
        let rect = Rect { pos: dvec2(left, y), size: dvec2(width, total) };
        popover_shadow(ui, rect, vec4(0.0, 0.0, 0.0, 0x33 as f32 / 255.0), 8.0);
        ui.rect(rect, pal.surface0, Some(pal.border), 8.0);
        (rect, rect.pos.y)
    };
    ui.zone(&format!("menu.{}.panel", menu.id), rect, Some(handler(|_, _| {})));
    let inset = if sheet { 4.0 } else { 0.0 };
    let mut y = top;
    if menu.searchable {
        // Inside the popover's 1px rule on desktop.
        let edge = if sheet { inset } else { 1.0 };
        let row = Rect { pos: dvec2(rect.pos.x + edge, y + if sheet { 0.0 } else { 1.0 }), size: dvec2(rect.size.x - edge * 2.0, search_h - if sheet { 1.0 } else { 2.0 }) };
        // Desktop popovers keep the 8px panel radius at the top edge.
        if sheet {
            ui.rect(row, pal.surface1, None, 0.0);
        } else {
            ui.rect_top(row, pal.surface1, 7.0);
        }
        ui.rect(Rect { pos: dvec2(row.pos.x, y + search_h - 1.0), size: dvec2(row.size.x, 1.0) }, pal.border, None, 0.0);
        ui.icon(Icon::Search, dvec2(row.pos.x + 12.0, y + (search_h - 1.0 - 16.0) / 2.0), 16.0, pal.muted);
        let field_x = row.pos.x + 12.0 + 16.0 + 8.0;
        let mut spec = FieldSpec::new(format!("menu.{}.search", menu.id), "").placeholder(menu.search_placeholder.clone()).bare();
        if !sheet {
            spec = spec.autofocus();
        }
        ui.draw_ctl(Ctl::Field { spec, w: (row.pos.x + row.size.x - 12.0 - field_x).max(1.0), h: search_field_h }, dvec2(field_x, y + 12.0));
        y += search_h;
    }
    let list = Rect { pos: dvec2(rect.pos.x, y), size: dvec2(rect.size.x, (rect.pos.y + rect.size.y - y).max(0.0)) };
    ui.cx.begin_turtle(Walk::fixed(list.size.x, list.size.y).with_abs_pos(list.pos), Layout::flow_overlay().with_clip(true, true));
    ui.set_clip(Some(list));
    let scroll = menu.scroll.clamp(0.0, (list_h - list.size.y).max(0.0));
    let mut y = y - scroll;
    if visible.is_empty() {
        let text = if menu.empty_text.is_empty() { t("common.empty.noOptions") } else { menu.empty_text.clone() };
        ui.text(Font::Regular, 14.0, pal.muted, dvec2(list.pos.x + 12.0, y + 8.0), Some(list.size.x - 24.0), &text);
    } else {
        // ListView padding: 4px on top (sheet and popover alike).
        y += 4.0;
    }
    let (row_x, row_w) = if sheet { (list.pos.x + 8.0 + 4.0, list.size.x - 16.0 - 8.0) } else { (list.pos.x, list.size.x) };
    for &i in &visible {
        let item = &menu.items[i];
        if separators(i) {
            ui.rect(Rect { pos: dvec2(list.pos.x + if sheet { 8.0 } else { 0.0 }, y + 4.0), size: dvec2(list.size.x - if sheet { 16.0 } else { 0.0 }, 1.0) }, pal.border, None, 0.0);
            y += 9.0;
        }
        let item_rect = Rect { pos: dvec2(row_x, y), size: dvec2(row_w, row_h) };
        let k = format!("menu.{}.item.{i}", menu.id);
        let (hovered, pressed) = ui.state(&k);
        if item.enabled && (hovered || pressed) {
            ui.rect(item_rect, pal.surface1, None, 0.0);
        }
        let fade = if item.enabled { 1.0 } else { 0.55 };
        let fg = super::theme::alpha(pal.foreground, fade);
        let muted = super::theme::alpha(pal.muted, fade);
        let mut x = item_rect.pos.x + 12.0;
        if let Some(lead) = item.lead.clone() {
            ui.lead(lead, dvec2(x, y + (row_h - 16.0) / 2.0), 16.0);
            x += 24.0;
        }
        let right = item_rect.pos.x + item_rect.size.x - 12.0 - if item.selected { 16.0 + 8.0 } else { 0.0 };
        let avail = (right - x).max(1.0);
        // Label then description on one baseline-aligned line, both ellipsized.
        let ls = ui.measure(Font::Regular, 14.0, &item.label, None);
        let label_w = ls.x.min(avail);
        ui.text_line(Font::Regular, 14.0, fg, dvec2(x, y + (row_h - ls.y) / 2.0), label_w, &item.label);
        if let Some(d) = &item.description {
            let dx = x + label_w + 8.0;
            if right - dx > 8.0 {
                let ds = ui.measure(Font::Regular, 12.0, d, None);
                // Align the 12px baseline with the 14px label's.
                let dy = y + (row_h - ls.y) / 2.0 + (ls.y - ds.y) * 0.8;
                ui.text_line(Font::Regular, 12.0, muted, dvec2(dx, dy), right - dx, d);
            }
        }
        if item.selected {
            ui.icon(Icon::Check, dvec2(item_rect.pos.x + item_rect.size.x - 12.0 - 16.0, y + (row_h - 16.0) / 2.0), 16.0, muted);
        }
        if item.enabled
            && let Some(on) = item.on.clone()
        {
            let run = handler(move |s, env| {
                s.menu = None;
                on(s, env);
            });
            ui.zone(&k, item_rect, Some(run));
        }
        y += row_h + if sheet { 4.0 } else { 0.0 };
    }
    ui.set_clip(None);
    ui.cx.end_turtle();
    (list, list_h)
}

/// Size of a sheet's header actions (8px apart, as in `SheetContents`).
fn header_actions_size(ui: &mut Ui, head: &SheetHead) -> DVec2 {
    head.header_actions.iter().fold(DVec2::default(), |mut total, action| {
        let size = ui.measure_ctl(action);
        total.x += size.x + 8.0;
        total.y = total.y.max(size.y);
        total
    })
}

/// Draws the frame of one sheet and positions `ui` for its body; returns the
/// body clip rect. `content_h` is the body height measured last frame.
///
/// Flutter `showAdaptiveSheet` + `SheetContents`: wide layouts get a
/// `Dialog` (surface1, 12px radius, surface2 rule, 24px inset, at most 85%
/// of the window) over a 55% barrier; compact layouts a surface0 bottom
/// sheet with 16px top corners and the Material drag handle over the
/// default 54% barrier. Header: 24/16/16/16 insets, the 14px medium title
/// (wrapped to its measured height), header actions and a 32px muted close glyph, a
/// surface2 rule below. Footer: a surface2 rule, 24/12 insets, actions
/// 8px apart at the end (or sharing the row).
/// The barrier covers `screen`; the sheet lies inside `view` (the window less
/// safe areas and the keyboard). The body yields height before the header and
/// footer; a view too short for even those keeps the footer's actions on screen.
pub fn sheet_frame(ui: &mut Ui, screen: Rect, view: Rect, id: u64, max_w: f64, head: &SheetHead, content_h: f64, busy_close: bool) -> (Rect, Rect) {
    let pal = ui.pal;
    let compact = ui.compact;
    let barrier = if compact { 0x8a000000 } else { 0x8c000000 };
    ui.rect(screen, super::theme::argb(barrier), None, 0.0);
    let dismiss = if head.dismissible && !busy_close {
        Some(handler(move |s, env| s.close_sheet(env, id)))
    } else {
        Some(handler(|_, _| {}))
    };
    ui.zone(&format!("sheet.{id}.backdrop"), screen, dismiss);
    let width = if compact { view.size.x } else { max_w.min(view.size.x - 48.0) };
    // Material bottom-sheet drag handle area.
    let handle_h = if compact { 48.0 } else { 0.0 };
    let header_actions = header_actions_size(ui, head);
    let title_w = (width - 24.0 - 16.0 - 32.0 - 8.0 - header_actions.x).max(40.0);
    let title_h = ui.measure(Font::Medium, 14.0, &head.title, Some(title_w)).y;
    let row_h = title_h.max(header_actions.y).max(32.0);
    let search_field_h = 20.0_f64.max(ui.measure(Font::Regular, 14.0, "Ag", None).y);
    let search_h = if head.search.is_some() { search_field_h + 16.0 + 12.0 } else { 0.0 };
    let header_h = handle_h + 16.0 + row_h + 16.0 + search_h + 1.0;
    let actions_h = footer_h(ui, head);
    let max_h = if compact { view.size.y * 0.9 } else { (view.size.y * 0.85).min(view.size.y - 48.0) };
    // `content_h` already includes the 24px body padding on both sides. The
    // body keeps up to 80px only while the view has room for it.
    let room = (view.size.y - header_h - actions_h).max(0.0);
    let body_h = content_h.min((max_h - header_h - actions_h).max(room.min(80.0)));
    let total = header_h + body_h + actions_h;
    let pos = if compact {
        dvec2(view.pos.x, view.pos.y + view.size.y - total)
    } else {
        // Centred; an overflowing sheet aligns its footer with the view's bottom.
        let slack = view.size.y - total;
        dvec2(view.pos.x + (view.size.x - width) / 2.0, view.pos.y + slack.min(slack / 2.0))
    };
    let rect = Rect { pos, size: dvec2(width, total) };
    if compact {
        ui.rect_top(rect, pal.surface0, 16.0);
        surface_below(ui, rect, screen, pal.surface0);
        ui.rect(Rect { pos: dvec2(pos.x + (width - 32.0) / 2.0, pos.y + 22.0), size: dvec2(32.0, 4.0) }, super::theme::alpha(pal.muted, 0.4), None, 2.0);
    } else {
        ui.rect(rect, pal.surface1, Some(pal.surface2), 12.0);
    }
    ui.zone(&format!("sheet.{id}.panel"), rect, Some(handler(|_, _| {})));
    let row_top = pos.y + handle_h + 16.0;
    ui.text(Font::Medium, 14.0, pal.foreground, dvec2(pos.x + 24.0, row_top + (row_h - title_h) / 2.0), Some(title_w), &head.title);
    // Close: `IconButton` with 8px padding, a 16px X and the 8px-radius hover.
    let close_rect = Rect { pos: dvec2(pos.x + width - 16.0 - 32.0, row_top + (row_h - 32.0) / 2.0), size: dvec2(32.0, 32.0) };
    let close_key = format!("sheet.{id}.close");
    let (hovered, pressed) = ui.state(&close_key);
    if hovered || pressed {
        ui.rect(close_rect, pal.interaction, None, 8.0);
    }
    ui.icon(Icon::X, close_rect.pos + dvec2(8.0, 8.0), 16.0, pal.muted);
    let close: Handler = if busy_close { handler(|_, _| {}) } else { handler(move |s, env| s.close_sheet(env, id)) };
    ui.zone(&close_key, close_rect, Some(close));
    ui.set_last_tip_pub(&close_key, t("common.actions.close"));
    if let Some((field, placeholder)) = &head.search {
        let y = row_top + row_h + 16.0;
        ui.icon(Icon::Search, dvec2(pos.x + 24.0, y + search_field_h / 2.0), 16.0, pal.muted);
        let fx = pos.x + 24.0 + 16.0 + 8.0;
        let spec = FieldSpec::new(field.clone(), "").placeholder(placeholder.clone()).bare();
        ui.draw_ctl(Ctl::Field { spec, w: (pos.x + width - 24.0 - fx).max(1.0), h: search_field_h }, dvec2(fx, y + 8.0));
    }
    let rule_y = pos.y + header_h - 1.0;
    let rule_x = if compact { pos.x } else { pos.x + 1.0 };
    let rule_w = if compact { width } else { width - 2.0 };
    ui.rect(Rect { pos: dvec2(rule_x, rule_y), size: dvec2(rule_w, 1.0) }, pal.surface2, None, 0.0);
    if actions_h > 0.0 {
        ui.rect(Rect { pos: dvec2(rule_x, pos.y + header_h + body_h), size: dvec2(rule_w, 1.0) }, pal.surface2, None, 0.0);
    }
    (rect, Rect { pos: dvec2(pos.x, pos.y + header_h), size: dvec2(width, body_h) })
}

/// Draws header actions (left of the close button) and footer actions.
pub fn sheet_controls(ui: &mut Ui, rect: Rect, mut head: SheetHead) {
    let handle_h = if ui.compact { 48.0 } else { 0.0 };
    let header_actions = header_actions_size(ui, &head);
    let title_w = (rect.size.x - 24.0 - 16.0 - 32.0 - 8.0 - header_actions.x).max(40.0);
    let title_h = ui.measure(Font::Medium, 14.0, &head.title, Some(title_w)).y;
    let row_top = rect.pos.y + handle_h + 16.0;
    let row_h = title_h.max(header_actions.y).max(32.0);
    let footer = footer_h(ui, &head);
    let mut x = rect.pos.x + rect.size.x - 16.0 - 32.0;
    for action in std::mem::take(&mut head.header_actions).into_iter().rev() {
        let s = ui.measure_ctl(&action);
        x -= s.x + 8.0;
        ui.draw_ctl(action, dvec2(x, row_top + (row_h - s.y) / 2.0));
    }
    if head.actions.is_empty() {
        return;
    }
    let pal = ui.pal;
    let meta = head.footer_meta.clone().filter(|m| !m.is_empty());
    if ui.compact && head.stack_compact {
        let mut y = rect.pos.y + rect.size.y - footer + 1.0 + 12.0;
        if let Some(meta) = &meta {
            let s = ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(rect.pos.x + 24.0, y), rect.size.x - 48.0, meta);
            y += s.y + 8.0;
        }
        for action in head.actions {
            let h = ui.measure_ctl(&action).y;
            let action = match action {
                Ctl::Button(mut b) => {
                    b.expand = true;
                    Ctl::Button(b)
                }
                other => other,
            };
            draw_expanded(ui, action, dvec2(rect.pos.x + 24.0, y), rect.size.x - 48.0);
            y += h + 8.0;
        }
        return;
    }
    let sizes: Vec<DVec2> = head.actions.iter().map(|a| ui.measure_ctl(a)).collect();
    let row_h = sizes.iter().map(|s| s.y).fold(0.0, f64::max);
    let y = rect.pos.y + rect.size.y - 12.0 - row_h;
    if let Some(meta) = &meta {
        let buttons_w: f64 = sizes.iter().map(|s| s.x + 8.0).sum();
        let ms = ui.measure(Font::Regular, 12.0, meta, None);
        ui.text_line(Font::Regular, 12.0, pal.muted, dvec2(rect.pos.x + 24.0, y + (row_h - ms.y) / 2.0), (rect.size.x - 48.0 - buttons_w).max(0.0), meta);
    }
    if head.stretch_actions {
        let n = head.actions.len() as f64;
        let w = (rect.size.x - 48.0 - 8.0 * (n - 1.0)) / n;
        for (i, action) in head.actions.into_iter().enumerate() {
            let x = rect.pos.x + 24.0 + i as f64 * (w + 8.0);
            let action = match action {
                Ctl::Button(mut b) => {
                    b.expand = true;
                    Ctl::Button(b)
                }
                other => other,
            };
            // Expanded buttons fill their slot.
            draw_expanded(ui, action, dvec2(x, y), w);
        }
    } else {
        let mut x = rect.pos.x + rect.size.x - 24.0;
        for (action, size) in head.actions.into_iter().rev().zip(sizes.iter().rev()) {
            x -= size.x;
            ui.draw_ctl(action, dvec2(x, y + (row_h - size.y) / 2.0));
            x -= 8.0;
        }
    }
}

/// A button stretched to `w` (Flutter `expand: true`).
pub fn draw_expanded(ui: &mut Ui, action: Ctl, pos: DVec2, w: f64) {
    match action {
        Ctl::Button(b) => {
            let natural = ui.measure_ctl(&Ctl::Button(Button2::copy(&b))).x;
            let pad = ((w - natural) / 2.0).max(0.0);
            let h = ui.measure_ctl(&Ctl::Button(Button2::copy(&b))).y;
            let (hovered, _) = ui.state(&b.key);
            let enabled = b.enabled && !b.loading && b.act.is_some();
            let pal = ui.pal;
            let (bg, border) = match b.variant {
                Variant::Primary => (pal.accent, Some(pal.accent)),
                Variant::Secondary => (pal.surface3, Some(pal.surface3)),
                Variant::Outline => (if hovered && enabled { pal.interaction } else { pal.transparent }, Some(pal.border_accent)),
                Variant::Ghost => (if hovered && enabled { pal.interaction } else { pal.transparent }, None),
            };
            let fade = if enabled { 1.0 } else { 0.5 };
            let radius = if b.size == BSize::Md { 16.0 } else { 12.0 };
            ui.rect(Rect { pos, size: dvec2(w, h) }, super::theme::alpha(bg, fade), border.map(|c| super::theme::alpha(c, fade)), radius);
            let key = b.key.clone();
            let act = b.act.clone();
            // Draw the content centered over the stretched background without
            // its own background, then make the whole slot tappable.
            let mut content = b;
            content.act = None;
            content.enabled = enabled;
            draw_button_content(ui, &content, dvec2(pos.x + pad, pos.y), natural, h);
            if enabled {
                ui.zone(&key, Rect { pos, size: dvec2(w, h) }, act);
            }
        }
        other => {
            ui.draw_ctl(other, pos);
        }
    }
}

struct Button2;
impl Button2 {
    fn copy(b: &super::kit::Button) -> super::kit::Button {
        super::kit::Button {
            key: b.key.clone(),
            label: b.label.clone(),
            icon: b.icon,
            variant: b.variant,
            size: b.size,
            enabled: b.enabled,
            loading: b.loading,
            expand: b.expand,
            danger_text: b.danger_text,
            act: None,
            tip: b.tip.clone(),
        }
    }
}

fn draw_button_content(ui: &mut Ui, b: &super::kit::Button, pos: DVec2, w: f64, h: f64) {
    let pal = ui.pal;
    let fg = match b.variant {
        Variant::Primary => pal.on_accent,
        Variant::Ghost => pal.muted,
        _ => pal.foreground,
    };
    let fg = super::theme::alpha(if b.danger_text { pal.danger } else { fg }, if b.enabled { 1.0 } else { 0.5 });
    let font = if b.size == BSize::Xs { 12.0 } else { 14.0 };
    let icon = if b.size == BSize::Xs { 12.0 } else { 14.0 };
    let tw = if b.label.is_empty() { 0.0 } else { ui.measure(Font::Regular, font, &b.label, None).x };
    let has_icon = b.icon.is_some() || b.loading;
    let content = tw + if has_icon { icon } else { 0.0 } + if has_icon && tw > 0.0 { 8.0 } else { 0.0 };
    let mut x = pos.x + (w - content) / 2.0;
    if has_icon {
        let glyph = if b.loading { Icon::Spinner } else { b.icon.unwrap_or(Icon::Circle) };
        ui.icon(glyph, dvec2(x, pos.y + (h - icon) / 2.0), icon, fg);
        x += icon + 8.0;
    }
    if tw > 0.0 {
        let th = ui.measure(Font::Regular, font, &b.label, None).y;
        ui.text(Font::Regular, font, fg, dvec2(x, pos.y + (h - th) / 2.0), None, &b.label);
    }
}

// ------------------------------------------------------------ generic sheets

pub fn form_head(form: &FormSheet) -> SheetHead {
    let id = form.id;
    let busy = form.busy;
    let mut head = SheetHead::new(form.title.clone());
    head.stretch_actions = true;
    head.dismissible = !busy;
    head.actions.push(button(format!("sheet.{id}.cancel"), t("common.actions.cancel"), (!busy).then(|| handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    let save = handler(move |s, env| save_form(s, env, id));
    let label = if busy { t("settingsNative.saving") } else { t("common.actions.save") };
    head.actions.push(button(format!("sheet.{id}.save"), label, (!busy).then_some(save)).variant(Variant::Primary).loading(busy).ctl());
    head
}

fn save_form(s: &mut Settings, env: &mut crate::features::settings::rpc::Env, id: u64) {
    let fields = &*env.fields;
    let collected = match s.sheet_mut(id) {
        Some(crate::features::settings::state::SheetKind::Form(form)) => {
            if form.busy {
                return;
            }
            let values = form.collect(|field_id| fields.text(field_id));
            if values.is_some() {
                form.busy = true;
                form.error = None;
            }
            values.map(|v| (v, form.on_save.clone()))
        }
        _ => None,
    };
    if let Some((values, on_save)) = collected {
        on_save(s, env, id, values);
    }
}

pub fn form_body(ui: &mut Ui, form: &FormSheet) {
    let pal = ui.pal;
    if let Some(description) = &form.description {
        ui.para(description, 14.0, pal.muted);
        ui.gap(16.0);
    }
    for field in &form.fields {
        let id = form.field_id(&field.key);
        let error = form.errors.get(&field.key).map(String::as_str);
        match &field.kind {
            Kind::Bool => {
                let on = form.bools.get(&field.key).copied().unwrap_or(false);
                let key = field.key.clone();
                let sheet = form.id;
                let toggle = (!form.busy).then(|| {
                    handler(move |s, _| {
                        if let Some(crate::features::settings::state::SheetKind::Form(form)) = s.sheet_mut(sheet) {
                            let current = form.bools.get(&key).copied().unwrap_or(false);
                            form.bools.insert(key.clone(), !current);
                        }
                    })
                });
                let lw = ui.w - 48.0;
                let ls = ui.measure(Font::Regular, 14.0, &field.label, Some(lw));
                let y = ui.y;
                ui.text(Font::Regular, 14.0, pal.foreground, dvec2(ui.x, y + (ls.y.max(20.0) - ls.y) / 2.0), Some(lw), &field.label);
                let sx = ui.x + ui.w - 36.0;
                ui.draw_ctl(super::kit::switch(format!("{id}.switch"), on, toggle), dvec2(sx, y + (ls.y.max(20.0) - 20.0) / 2.0));
                ui.y += ls.y.max(20.0);
            }
            Kind::Text { multiline, secret } => {
                let initial = crate::features::settings::json::scalar_text(&field.value);
                let mut spec = FieldSpec::new(id.clone(), initial.clone()).password(*secret).enabled(!form.busy);
                if *multiline {
                    spec = spec.multiline();
                }
                let text = if ui.fields.exists(&id) { ui.fields.text(&id) } else { initial };
                let h = if *multiline { ui.multiline_height(&text, 84.0) } else { 40.0 };
                ui.form_field(Some(&field.label), None, error, spec, h);
            }
        }
        ui.gap(16.0);
    }
    if let Some(error) = &form.error {
        ui.selectable_para(&format!("form.{}.error", form.id), error, 12.0, pal.danger);
    }
}

pub fn report_head(id: u64, title: &str, text: &str) -> SheetHead {
    let mut head = SheetHead::new(title.to_string());
    let copy = text.to_string();
    head.actions.push(
        button(format!("sheet.{id}.copy"), t("common.actions.copy"), Some(handler(move |_, env| env.copy(&copy, None))))
            .icon(Icon::Copy)
            .ctl(),
    );
    head.actions.push(button(format!("sheet.{id}.closebtn"), t("common.actions.close"), Some(handler(move |s, env| s.close_sheet(env, id)))).variant(Variant::Secondary).ctl());
    head
}

/// Flutter `showSettingsReport`: the report is a `SelectableText` in the code style.
pub fn report_body(ui: &mut Ui, id: u64, text: &str) {
    let fg = ui.pal.foreground;
    let s = ui.selectable(&format!("report.{id}"), Font::Code, 12.0, fg, dvec2(ui.x, ui.y), Some(ui.w), text);
    ui.y += s.y;
}

/// Flutter Material `Tooltip` (no tooltip theme override): grey[700] at 90%
/// with white text on light themes, white at 90% with black text on dark,
/// 4px radius; desktop 24px min height, 8px side padding and 12px text
/// (touch: 32px, 16px, 14px); centred 24px below the anchor's centre, or
/// above when it would leave the window.
pub fn draw_tip(ui: &mut Ui, tip: &Tip, screen: Rect) {
    let desktop = !cfg!(any(target_os = "android", target_os = "ios"));
    let (min_h, pad_x, size) = if desktop { (24.0, 8.0, 12.0) } else { (32.0, 16.0, 14.0) };
    let max_text_w = (screen.size.x - pad_x * 2.0).max(0.0);
    let text_size = ui.measure(Font::Regular, size, &tip.text, Some(max_text_w));
    let w = text_size.x + pad_x * 2.0;
    let h = (text_size.y + 8.0).max(min_h);
    let center = dvec2(tip.anchor.pos.x + tip.anchor.size.x / 2.0, tip.anchor.pos.y + tip.anchor.size.y / 2.0);
    let below = center.y + 24.0;
    let y = if below + h <= screen.pos.y + screen.size.y { below } else { center.y - 24.0 - h };
    let x = (center.x - w / 2.0).clamp(screen.pos.x, (screen.pos.x + screen.size.x - w).max(screen.pos.x));
    let (fill, color) = if ui.pal.dark {
        (vec4(1.0, 1.0, 1.0, 0.9), vec4(0.0, 0.0, 0.0, 1.0))
    } else {
        (vec4(97.0 / 255.0, 97.0 / 255.0, 97.0 / 255.0, 0.9), vec4(1.0, 1.0, 1.0, 1.0))
    };
    ui.rect(Rect { pos: dvec2(x, y), size: dvec2(w, h) }, fill, None, 4.0);
    ui.text(Font::Regular, size, color, dvec2(x + pad_x, y + (h - text_size.y) / 2.0), Some(text_size.x + 1.0), &tip.text);
}

/// Flutter `confirmAction`: `showAdaptiveDialog` + `AlertDialog.adaptive`
/// over the default 54% barrier, dismissible by a barrier tap. The barrier
/// covers `screen`; the dialog centres in `view`.
pub fn draw_alert(ui: &mut Ui, screen: Rect, view: Rect, id: u64, confirm: &Confirm) {
    ui.rect(screen, super::theme::argb(0x8a000000), None, 0.0);
    ui.zone(&format!("sheet.{id}.backdrop"), screen, Some(handler(move |s, env| s.close_sheet(env, id))));
    let cancel_label = t("common.actions.cancel");
    let confirm_label = confirm.confirm_label.clone();
    let on = confirm.on_confirm.clone();
    let run = handler(move |s, env| {
        // Confirming is not declining.
        if let Some(crate::features::settings::state::SheetKind::Confirm(confirm)) = s.sheet_mut(id) {
            confirm.on_decline = None;
        }
        s.close_sheet(env, id);
        on(s, env);
    });
    let cancel = handler(move |s, env| s.close_sheet(env, id));
    let actions = AlertActions { cancel: (cancel_label, cancel), confirm: (confirm_label, run) };
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        draw_cupertino_alert(ui, view, id, confirm, actions);
    } else {
        draw_material_alert(ui, view, id, confirm, actions);
    }
}

/// The two alert buttons: label and action.
struct AlertActions {
    cancel: (String, Handler),
    confirm: (String, Handler),
}

/// Material 3 `AlertDialog` with the Paseo theme: `dialogTheme` surface
/// (surface2 dark, surface0 light, 12px radius), 280–560px wide inside the
/// 40/24 dialog insets, 24px padding, the 20px semibold title, 16px gap, the
/// 14px `onSurfaceVariant` (muted) message, 24px gap and end-aligned
/// `TextButton`s 8px apart (muted 14px labels that turn foreground on hover,
/// 12px horizontal padding, 32px tall; 44/16 on compact).
fn draw_material_alert(ui: &mut Ui, screen: Rect, id: u64, confirm: &Confirm, actions: AlertActions) {
    let AlertActions { cancel: (cancel_label, cancel), confirm: (confirm_label, run) } = actions;
    let pal = ui.pal;
    let compact = ui.compact;
    let (mut button_h, button_pad): (f64, f64) = if compact { (44.0, 16.0) } else { (32.0, 12.0) };
    let labels = [cancel_label.as_str(), confirm_label.as_str()];
    let label_w: Vec<f64> = labels.iter().map(|label| {
        let size = ui.measure(Font::Regular, 14.0, label, None);
        button_h = button_h.max(size.y + 8.0);
        size.x
    }).collect();
    let actions_w: f64 = label_w.iter().map(|w| w + button_pad * 2.0).sum::<f64>() + 8.0;
    let max_w = (screen.size.x - 80.0).clamp(0.0, 560.0);
    let title_natural = ui.measure(Font::Medium, 20.0, &confirm.title, None).x;
    let message_natural = ui.measure(Font::Regular, 14.0, &confirm.message, None).x;
    let width = (title_natural.max(message_natural).max(actions_w) + 48.0).clamp(280.0_f64.min(max_w), max_w);
    let inner = width - 48.0;
    let title_h = ui.measure(Font::Medium, 20.0, &confirm.title, Some(inner)).y;
    let message_h = ui.measure(Font::Regular, 14.0, &confirm.message, Some(inner)).y;
    let height = 24.0 + title_h + 16.0 + message_h + 24.0 + button_h + 24.0;
    let pos = dvec2(screen.pos.x + (screen.size.x - width) / 2.0, screen.pos.y + ((screen.size.y - height) / 2.0).max(24.0));
    let rect = Rect { pos, size: dvec2(width, height) };
    // Material elevation 6 shadow.
    popover_shadow(ui, rect, vec4(0.0, 0.0, 0.0, 0.2), 12.0);
    ui.rect(rect, if pal.dark { pal.surface2 } else { pal.surface0 }, None, 12.0);
    ui.zone(&format!("sheet.{id}.panel"), rect, Some(handler(|_, _| {})));
    ui.text(Font::Medium, 20.0, pal.foreground, dvec2(pos.x + 24.0, pos.y + 24.0), Some(inner), &confirm.title);
    ui.text(Font::Regular, 14.0, pal.muted, dvec2(pos.x + 24.0, pos.y + 24.0 + title_h + 16.0), Some(inner), &confirm.message);
    let y = pos.y + height - 24.0 - button_h;
    let mut x = pos.x + width - 24.0;
    let acts = [(1usize, run, "confirm"), (0usize, cancel, "cancel")];
    for (i, act, name) in acts {
        let w = label_w[i] + button_pad * 2.0;
        x -= w;
        let key = format!("sheet.{id}.{name}");
        let (hovered, pressed) = ui.state(&key);
        let color = if hovered || pressed { pal.foreground } else { pal.muted };
        let ls = ui.measure(Font::Regular, 14.0, labels[i], None);
        ui.text(Font::Regular, 14.0, color, dvec2(x + button_pad, y + (button_h - ls.y) / 2.0), None, labels[i]);
        ui.zone(&key, Rect { pos: dvec2(x, y), size: dvec2(w, button_h) }, Some(act));
        x -= 8.0;
    }
}

/// `CupertinoAlertDialog`: 270px, 14px radius, centred 17px semibold title
/// and 13px message (20px sides, 19px top, 20px bottom), then a hairline
/// and two 44px actions split by a vertical hairline; the default action in
/// system blue, a destructive one in system red.
fn draw_cupertino_alert(ui: &mut Ui, screen: Rect, id: u64, confirm: &Confirm, actions: AlertActions) {
    let AlertActions { cancel: (cancel_label, cancel), confirm: (confirm_label, run) } = actions;
    let pal = ui.pal;
    let width = 270.0;
    let inner = width - 40.0;
    let title_h = ui.measure(Font::Medium, 17.0, &confirm.title, Some(inner)).y;
    let message_h = ui.measure(Font::Regular, 13.0, &confirm.message, Some(inner)).y;
    let body_h = 19.0 + title_h + 2.0 + message_h + 20.0;
    let height = body_h + 44.0;
    let pos = dvec2(screen.pos.x + (screen.size.x - width) / 2.0, screen.pos.y + (screen.size.y - height) / 2.0);
    let rect = Rect { pos, size: dvec2(width, height) };
    let (fill, rule) = if pal.dark {
        (vec4(0x2c as f32 / 255.0, 0x2c as f32 / 255.0, 0x2e as f32 / 255.0, 0.95), vec4(0.33, 0.33, 0.35, 0.6))
    } else {
        (vec4(0xf2 as f32 / 255.0, 0xf2 as f32 / 255.0, 0xf7 as f32 / 255.0, 0.95), vec4(0.24, 0.24, 0.26, 0.29))
    };
    ui.rect(rect, fill, None, 14.0);
    ui.zone(&format!("sheet.{id}.panel"), rect, Some(handler(|_, _| {})));
    let centered = |ui: &mut Ui, font: Font, size: f64, text: &str, y: f64| {
        let s = ui.measure(font, size, text, Some(inner));
        ui.text(font, size, pal.foreground, dvec2(pos.x + (width - s.x) / 2.0, y), Some(inner), text);
    };
    centered(ui, Font::Medium, 17.0, &confirm.title, pos.y + 19.0);
    centered(ui, Font::Regular, 13.0, &confirm.message, pos.y + 19.0 + title_h + 2.0);
    let y = pos.y + body_h;
    ui.rect(Rect { pos: dvec2(pos.x, y), size: dvec2(width, 0.5) }, rule, None, 0.0);
    ui.rect(Rect { pos: dvec2(pos.x + width / 2.0, y), size: dvec2(0.5, 44.0) }, rule, None, 0.0);
    let blue = vec4(0.0, 122.0 / 255.0, 1.0, 1.0);
    let red = vec4(1.0, 59.0 / 255.0, 48.0 / 255.0, 1.0);
    let half = width / 2.0;
    for (i, (label, act, color, name)) in [(cancel_label.as_str(), cancel, blue, "cancel"), (confirm_label.as_str(), run, if confirm.destructive { red } else { blue }, "confirm")].into_iter().enumerate() {
        let x = pos.x + half * i as f64;
        let key = format!("sheet.{id}.{name}");
        let (hovered, pressed) = ui.state(&key);
        if hovered || pressed {
            ui.rect(Rect { pos: dvec2(x, y + 0.5), size: dvec2(half, 43.5) }, super::theme::alpha(rule, 0.5), None, 0.0);
        }
        let s = ui.measure(Font::Regular, 17.0, label, None);
        ui.text(Font::Regular, 17.0, color, dvec2(x + (half - s.x) / 2.0, y + (44.0 - s.y) / 2.0), None, label);
        ui.zone(&key, Rect { pos: dvec2(x, y), size: dvec2(half, 44.0) }, Some(act));
    }
}
