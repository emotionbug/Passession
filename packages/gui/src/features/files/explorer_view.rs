//! The explorer tree view (Flutter `RemoteDirectoryBrowser.build`): the 36px sidebar toolbar
//! (sort trigger, owner actions, hidden-files toggle, refresh, optional navigation menu), the
//! optional remote location field and "Use this directory" button (picker), the stale-error
//! line, and the lazily drawn flattened rows (only rows in the viewport are laid out).

use makepad_widgets::*;

use crate::common::clock;
use crate::common::paint::{Font, HitMap};
use crate::theme::{Palette, Typography, fade, with_alpha};

use super::editor::field::{FieldEvent, TextField};
use super::explorer::{Entry, ExplorerState};
use super::i18n::{l, t};
use super::images;
use super::kit::{Kit, rect, spec};
use super::ui::{self, Interaction, TOOLBAR_HEIGHT};

/// A toolbar action supplied by the owner (new file / new folder).
#[derive(Clone, Debug)]
pub struct ToolbarAction {
    pub id: &'static str,
    pub icon: &'static str,
    pub tooltip: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Hot {
    Sort,
    Action(&'static str),
    Hidden,
    Refresh,
    Navigation,
    UseDirectory,
    Location,
    Row(usize),
    Back,
    Retry,
    Background,
    NavItem(&'static str),
}

/// What the owner handles.
#[derive(Clone, Debug)]
pub enum ExplorerEvent {
    None,
    Action(&'static str),
    FileSelected(Entry),
    /// Right click (with the window position) or long press (None) on a row.
    EntryMenu { entry: Entry, at: Option<DVec2>, row: Rect },
    BackgroundMenu { at: Option<DVec2>, anchor: Rect },
    DirectorySelected(String),
}

pub struct ExplorerView {
    hits: HitMap<Hot>,
    hovered: Option<Hot>,
    pressed: Option<Hot>,
    scroll_y: f64,
    list_rect: Rect,
    row_height: f64,
    thumb_drag: Option<(f64, f64)>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub location: TextField,
    pub location_focused: bool,
    /// The picker's in-view navigation menu (Back / Forward / Parent / Go to directory).
    nav_open: bool,
    nav_anchor: Rect,
    /// Visible row rects by index, for anchoring menus.
    row_rects: Vec<(usize, Rect)>,
    pub sidebar: bool,
}

impl Default for ExplorerView {
    fn default() -> Self {
        ExplorerView {
            hits: HitMap::default(),
            hovered: None,
            pressed: None,
            scroll_y: 0.0,
            list_rect: Rect::default(),
            row_height: 28.0,
            thumb_drag: None,
            touch_scroll: Default::default(),
            location: TextField::new("."),
            location_focused: false,
            nav_open: false,
            nav_anchor: Rect::default(),
            row_rects: Vec::new(),
            sidebar: true,
        }
    }
}

pub struct ExplorerLook<'a> {
    pub palette: Palette,
    pub typography: Typography,
    pub compact: bool,
    pub actions: &'a [ToolbarAction],
    pub navigation_menu: bool,
    pub picker: bool,
    pub busy: bool,
}

impl ExplorerView {
    fn state_of(&self, hot: &Hot, enabled: bool, selected: bool) -> Interaction {
        Interaction {
            hovered: self.hovered.as_ref() == Some(hot),
            pressed: self.pressed.as_ref() == Some(hot),
            enabled,
            selected,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, state: &ExplorerState, look: &ExplorerLook, ime: (Area, DVec2)) {
        let p = &look.palette;
        let ty = &look.typography;
        self.hits.clear();
        self.row_rects.clear();
        kit.fill(cx, area, if self.sidebar { p.sidebar } else { p.surface0 }, 0.0);
        // Toolbar.
        let bar = rect(area.pos.x, area.pos.y, area.size.x, TOOLBAR_HEIGHT);
        ui::toolbar(cx, kit, bar, self.sidebar, p);
        let small = ty.ui_size(12.0);
        let sort_label = t(&format!("workspace.fileExplorer.sort.{}", state.sort().id()));
        let sort_w = kit.measure(cx, spec(Font::Regular, small, p.muted), &sort_label).x + 4.0 + 12.0 + 8.0;
        let sort = rect(bar.pos.x + 8.0, bar.pos.y + (TOOLBAR_HEIGHT - 24.0) / 2.0, sort_w, 24.0);
        let sort_state = self.state_of(&Hot::Sort, true, false);
        if sort_state.hovered || sort_state.pressed {
            kit.fill(cx, sort, ui::sidebar_hover(p), 4.0);
        }
        kit.text_in(cx, rect(sort.pos.x + 4.0, sort.pos.y, sort.size.x - 20.0, sort.size.y), spec(Font::Regular, small, p.muted), &sort_label, 0.0);
        kit.icon(cx, rect(sort.pos.x + sort.size.x - 16.0, sort.pos.y, 12.0, sort.size.y), "chevron-down", 12.0, p.muted);
        self.hits.push(sort, Hot::Sort);
        // Trailing controls, right to left, 1px apart.
        let size = ui::toolbar_button_size(look.compact);
        let mut x = bar.pos.x + bar.size.x - ui::toolbar_trailing_padding(look.compact);
        let y = bar.pos.y + (TOOLBAR_HEIGHT - size) / 2.0;
        let place = |x: &mut f64| {
            *x -= size;
            let r = rect(*x, y, size, size);
            *x -= 1.0;
            r
        };
        if look.navigation_menu {
            let r = place(&mut x);
            let st = self.state_of(&Hot::Navigation, true, self.nav_open);
            ui::icon_button(cx, kit, r, "ellipsis", st, look.compact, p);
            self.hits.push(r, Hot::Navigation);
            self.nav_anchor = r;
        }
        let refresh = place(&mut x);
        if state.refreshing() {
            ui::spinner(cx, kit, refresh, ui::toolbar_icon_size(look.compact), p.extra_muted, clock::phase(900));
            clock::register(cx, ime.0);
        } else {
            let st = self.state_of(&Hot::Refresh, true, false);
            ui::icon_button(cx, kit, refresh, "rotate-cw", st, look.compact, p);
        }
        self.hits.push(refresh, Hot::Refresh);
        let hidden = place(&mut x);
        let st = self.state_of(&Hot::Hidden, true, !state.show_hidden());
        ui::icon_button(cx, kit, hidden, if state.show_hidden() { "eye" } else { "eye-off" }, st, look.compact, p);
        self.hits.push(hidden, Hot::Hidden);
        for action in look.actions.iter().rev() {
            let r = place(&mut x);
            let hot = Hot::Action(action.id);
            let st = self.state_of(&hot, action.enabled && !look.busy, false);
            ui::icon_button(cx, kit, r, action.icon, st, look.compact, p);
            self.hits.push(r, hot);
        }
        let mut top = bar.pos.y + TOOLBAR_HEIGHT;
        // Remote location field (picker, or the navigation menu's "Go to directory…").
        if state.show_location || state.directories_only {
            let h = ui::ButtonSize::Sm.height();
            let r = rect(area.pos.x + 8.0, top + 8.0, area.size.x - 16.0, h);
            self.location.focused = self.location_focused;
            self.location.draw(cx, kit, r, l("Remote directory"), None, ty.ui_size(12.0), p, ime.0);
            self.hits.push(r, Hot::Location);
            top += 8.0 + h;
        }
        if look.picker {
            let label = l("Use this directory");
            let h = ui::ButtonSize::Sm.height();
            let w = ui::button_width(cx, kit, label, Some("check"), ui::ButtonSize::Sm, ty);
            let r = rect(area.pos.x + 8.0, top + 8.0, w, h);
            let st = self.state_of(&Hot::UseDirectory, true, false);
            ui::button(cx, kit, r, label, Some("check"), ui::ButtonVariant::Outline, ui::ButtonSize::Sm, st, p, ty);
            self.hits.push(r, Hot::UseDirectory);
            top += 8.0 + h;
        }
        if let Some(error) = state.stale_root_error() {
            let width = area.size.x - 24.0;
            let h = kit.paragraph_height(cx, width, spec(Font::Regular, small, p.destructive), error);
            kit.paragraph(cx, dvec2(area.pos.x + 12.0, top + 8.0), width, spec(Font::Regular, small, p.destructive), error);
            top += 8.0 + h;
        }
        let list = rect(area.pos.x, top, area.size.x, (area.pos.y + area.size.y - top).max(0.0));
        self.list_rect = list;
        self.hits.push(list, Hot::Background);
        let base = ty.ui_size(14.0);
        let muted = spec(Font::Regular, base, p.muted);
        if let Some(error) = state.root_error() {
            // Destructive base text over outline pill actions.
            let width = (list.size.x - 32.0).max(1.0);
            let text_h = kit.paragraph_height(cx, width, spec(Font::Regular, base, p.destructive), error);
            let pill_h = base * 1.4 + 8.0;
            let total = text_h + 8.0 + pill_h;
            let mut y0 = list.pos.y + (list.size.y - total) / 2.0;
            let text_w = kit.measure(cx, spec(Font::Regular, base, p.destructive), error).x.min(width);
            kit.paragraph(cx, dvec2(list.pos.x + (list.size.x - text_w) / 2.0, y0), width, spec(Font::Regular, base, p.destructive), error);
            y0 += text_h + 8.0;
            let mut pills: Vec<(Hot, String)> = Vec::new();
            if state.can_go_back() {
                pills.push((Hot::Back, t("workspace.fileExplorer.actions.back")));
            }
            pills.push((Hot::Retry, t("workspace.fileExplorer.actions.retry")));
            let widths: Vec<f64> = pills.iter().map(|(_, label)| kit.measure(cx, spec(Font::Medium, base, p.muted), label).x + 24.0).collect();
            let total_w = widths.iter().sum::<f64>() + 8.0 * (pills.len() - 1) as f64;
            let mut px = list.pos.x + (list.size.x - total_w) / 2.0;
            for ((hot, label), w) in pills.into_iter().zip(widths) {
                let r = rect(px, y0, w, pill_h);
                kit.outline(cx, r, Vec4::default(), p.border, 1.0, pill_h / 2.0);
                kit.text_in(cx, r, spec(Font::Medium, base, p.muted), &label, 0.5);
                self.hits.push(r, hot);
                px += w + 8.0;
            }
        } else if state.initial_loading() {
            let label = t("workspace.fileExplorer.states.loading");
            let cy = list.pos.y + list.size.y / 2.0;
            ui::spinner(cx, kit, rect(list.pos.x + list.size.x / 2.0 - 10.0, cy - 24.0, 20.0, 20.0), 20.0, p.muted, clock::phase(900));
            clock::register(cx, ime.0);
            kit.text_in(cx, rect(list.pos.x, cy + 4.0, list.size.x, base * 1.4), muted, &label, 0.5);
        } else if state.rows().is_empty() {
            let label = t(if state.show_hidden() { "workspace.fileExplorer.empty.noFiles" } else { "workspace.fileExplorer.empty.noVisibleFiles" });
            kit.text_in(cx, rect(list.pos.x, list.pos.y + list.size.y / 2.0 - base, list.size.x, base * 2.0), muted, &label, 0.5);
        } else {
            self.draw_rows(cx, kit, list, state, look, ime.0);
        }
        if self.nav_open {
            self.draw_nav_menu(cx, kit, state, look);
        }
    }

    fn draw_rows(&mut self, cx: &mut Cx2d, kit: &mut Kit, list: Rect, state: &ExplorerState, look: &ExplorerLook, area: Area) {
        let p = &look.palette;
        let ty = &look.typography;
        let base = ty.ui_size(14.0);
        let row_h = (base * 1.2 + 12.0).max(28.0).round();
        self.row_height = row_h;
        let rows = state.rows();
        let content_h = 8.0 + rows.len() as f64 * row_h + 16.0;
        let max = (content_h - list.size.y).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max);
        cx.begin_turtle(Walk::abs_rect(list), Layout::default());
        self.hits.set_clip(Some(list));
        let first = ((self.scroll_y - 8.0) / row_h).floor().max(0.0) as usize;
        let last = (first + (list.size.y / row_h).ceil() as usize + 2).min(rows.len());
        let name_color_rest = fade(p.foreground, 0.76);
        let mut needs_clock = false;
        for (index, row) in rows.iter().enumerate().take(last).skip(first) {
            let y = list.pos.y + 8.0 + index as f64 * row_h - self.scroll_y;
            let r = rect(list.pos.x, y, list.size.x, row_h);
            let hot = Hot::Row(index);
            let hovered = self.hovered.as_ref() == Some(&hot);
            let pressed = self.pressed.as_ref() == Some(&hot);
            let selected = state.selected.as_deref() == Some(row.entry.path.as_str());
            if hovered || pressed || selected {
                kit.fill(cx, r, ui::sidebar_hover(p), 0.0);
            }
            let x = r.pos.x + ui::tree_row_padding_left(row.depth);
            let slot = rect(x, y + (row_h - 16.0) / 2.0, 16.0, 16.0);
            if row.entry.is_directory() {
                if state.is_loading(&row.entry.path) {
                    ui::spinner(cx, kit, slot, 14.0, p.muted, clock::phase(900));
                    needs_clock = true;
                } else {
                    ui::chevron(cx, kit, slot, state.is_expanded(&row.entry.path), p);
                }
            } else if let Some(texture) = images::file_icon(&row.entry.name) {
                kit.texture(cx, slot, &texture, 0.0, 1.0);
            }
            let name_x = x + 16.0 + 8.0;
            let name_color = if hovered { p.foreground } else { name_color_rest };
            kit.text_in(cx, rect(name_x, y, r.pos.x + r.size.x - ui::TRAILING_GLYPH_RAIL - name_x, row_h), spec(Font::Regular, base, name_color), &row.entry.name, 0.0);
            self.hits.push(r, hot);
            self.row_rects.push((index, r));
        }
        if needs_clock {
            clock::register(cx, area);
        }
        self.hits.set_clip(None);
        if max > 0.0 {
            let thumb_h = (list.size.y * list.size.y / content_h).max(24.0);
            let thumb_y = list.pos.y + (list.size.y - thumb_h) * (self.scroll_y / max);
            kit.fill(cx, rect(list.pos.x + list.size.x - 6.0, thumb_y, 4.0, thumb_h), with_alpha(p.muted, 0.35), 2.0);
        }
        cx.end_turtle();
    }

    /// The picker's navigation popover (200px, under the "…" button, aligned to its end).
    fn draw_nav_menu(&mut self, cx: &mut Cx2d, kit: &mut Kit, state: &ExplorerState, look: &ExplorerLook) {
        let p = &look.palette;
        let ty = &look.typography;
        let items: [(&'static str, &'static str, &str, bool); 4] = [
            ("back", "arrow-left", l("Back"), state.can_go_back()),
            ("forward", "arrow-right", l("Forward"), state.can_go_forward()),
            ("parent", "corner-left-up", l("Parent directory"), true),
            ("location", "folder-search", l("Go to directory…"), true),
        ];
        let item_h = 32.0;
        let w = 200.0;
        let h = item_h * 4.0 + 9.0 + 8.0;
        let x = self.nav_anchor.pos.x + self.nav_anchor.size.x - w;
        let y = self.nav_anchor.pos.y + self.nav_anchor.size.y + 4.0;
        let card = rect(x, y, w, h);
        ui::card(cx, kit, card, p, 8.0);
        let mut iy = y + 4.0;
        for (index, (id, icon, label, enabled)) in items.into_iter().enumerate() {
            if index == 3 {
                kit.hline(cx, x + 1.0, iy + 4.0, w - 2.0, p.border);
                iy += 9.0;
            }
            let r = rect(x + 4.0, iy, w - 8.0, item_h);
            let hot = Hot::NavItem(id);
            if enabled && self.hovered.as_ref() == Some(&hot) {
                kit.fill(cx, r, p.interaction_highlight, 6.0);
            }
            let color = if enabled { p.foreground } else { fade(p.foreground, 0.5) };
            kit.icon(cx, rect(r.pos.x + 8.0, r.pos.y, 16.0, item_h), icon, 16.0, if enabled { p.muted } else { fade(p.muted, 0.5) });
            kit.text_in(cx, rect(r.pos.x + 32.0, r.pos.y, r.size.x - 40.0, item_h), spec(Font::Regular, ty.ui_size(14.0), color), label, 0.0);
            if enabled {
                self.hits.push(r, hot);
            }
            iy += item_h;
        }
    }

    pub fn row_rect(&self, index: usize) -> Option<Rect> {
        self.row_rects.iter().find(|(i, _)| *i == index).map(|(_, r)| *r)
    }

    pub fn animate_scroll(&mut self, cx: &mut Cx, event: &Event, state: &ExplorerState, enabled: bool) -> bool {
        if !enabled || self.nav_open {
            self.touch_scroll.reset();
            return false;
        }
        let Some(delta) = self.touch_scroll.frame(cx, event) else { return false };
        let max = 8.0 + state.rows().len() as f64 * self.row_height + 16.0 - self.list_rect.size.y;
        self.touch_scroll.apply_delta(&mut self.scroll_y, delta, max)
    }

    /// Pointer and keyboard input on the owner's area. `changed` tells the owner to redraw.
    pub fn handle_hit(&mut self, cx: &mut Cx, kit: &mut Kit, hit: &Hit, state: &mut ExplorerState, look: &ExplorerLook, changed: &mut bool) -> ExplorerEvent {
        match hit {
            Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                let hot = self.hits.at(fe.abs);
                if hot != self.hovered {
                    self.hovered = hot;
                    *changed = true;
                }
                let hand = matches!(self.hovered, Some(Hot::Row(_) | Hot::Sort | Hot::Action(_) | Hot::Hidden | Hot::Refresh | Hot::Navigation | Hot::UseDirectory | Hot::Back | Hot::Retry | Hot::NavItem(_)));
                cx.set_cursor(if hand { MouseCursor::Hand } else if self.hovered == Some(Hot::Location) { MouseCursor::Text } else { MouseCursor::Default });
            }
            Hit::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    *changed = true;
                }
            }
            Hit::FingerScroll(fe) => {
                self.touch_scroll.reset();
                if self.list_rect.contains(fe.abs) {
                    self.scroll_y += fe.scroll.y;
                    *changed = true;
                }
            }
            Hit::FingerDown(fe) => {
                self.touch_scroll.reset();
                let hot = self.hits.at(fe.abs);
                if self.nav_open && !matches!(hot, Some(Hot::NavItem(_) | Hot::Navigation)) {
                    self.nav_open = false;
                    *changed = true;
                    return ExplorerEvent::None;
                }
                let secondary = fe.device.mouse_button().is_some_and(|b| b.is_secondary());
                self.location_focused = hot == Some(Hot::Location);
                if self.location_focused {
                    let event = self.location.handle_hit(cx, kit, hit);
                    *changed = true;
                    let _ = event;
                    return ExplorerEvent::None;
                }
                if secondary {
                    if let Some(Hot::Row(index)) = hot
                        && let Some(entry) = state.rows().get(index).map(|row| row.entry.clone()) {
                            state.selected = Some(entry.path.clone());
                            *changed = true;
                            return ExplorerEvent::EntryMenu { entry, at: Some(fe.abs), row: self.row_rect(index).unwrap_or_default() };
                        }
                    if hot == Some(Hot::Background) {
                        return ExplorerEvent::BackgroundMenu { at: Some(fe.abs), anchor: rect(fe.abs.x, fe.abs.y, 0.0, 0.0) };
                    }
                    return ExplorerEvent::None;
                }
                if let Some(list) = Some(self.list_rect) {
                    let content_h = 8.0 + state.rows().len() as f64 * self.row_height + 16.0;
                    if content_h > list.size.y && fe.abs.x > list.pos.x + list.size.x - 10.0 && list.contains(fe.abs) {
                        self.thumb_drag = Some((fe.abs.y, self.scroll_y));
                        return ExplorerEvent::None;
                    }
                }
                self.pressed = hot;
                if self.list_rect.contains(fe.abs) {
                    self.touch_scroll.handle_hit(hit);
                }
                *changed = true;
            }
            Hit::FingerMove(fe) => {
                if let Some((start, scroll)) = self.thumb_drag {
                    let content_h = 8.0 + state.rows().len() as f64 * self.row_height + 16.0;
                    self.scroll_y = scroll + (fe.abs.y - start) * content_h / self.list_rect.size.y.max(1.0);
                    *changed = true;
                } else if let Some(dy) = self.touch_scroll.handle_hit(hit) {
                    if self.list_rect.contains(fe.abs_start) {
                        let max = 8.0 + state.rows().len() as f64 * self.row_height + 16.0 - self.list_rect.size.y;
                        self.touch_scroll.apply_delta(&mut self.scroll_y, dy, max);
                        self.pressed = None;
                        *changed = true;
                    }
                } else if self.location_focused {
                    self.location.handle_hit(cx, kit, hit);
                    *changed = true;
                }
            }
            Hit::FingerLongPress(fe) => {
                self.touch_scroll.reset();
                let hot = self.hits.at(fe.abs);
                self.pressed = None;
                *changed = true;
                match hot {
                    Some(Hot::Row(index)) => {
                        let Some(row) = state.rows().get(index).cloned() else { return ExplorerEvent::None };
                        state.selected = Some(row.entry.path.clone());
                        if look.picker {
                            // Long press selects a directory in the picker.
                            if row.entry.is_directory() {
                                return ExplorerEvent::DirectorySelected(state.absolute(&row.entry.path));
                            }
                            return ExplorerEvent::None;
                        }
                        return ExplorerEvent::EntryMenu { entry: row.entry, at: None, row: self.row_rect(index).unwrap_or_default() };
                    }
                    Some(Hot::Background) if !look.picker => {
                        return ExplorerEvent::BackgroundMenu { at: None, anchor: rect(fe.abs.x, fe.abs.y, 0.0, 0.0) };
                    }
                    _ => {}
                }
            }
            Hit::FingerUp(fe) => {
                self.touch_scroll.release(fe.time);
                self.touch_scroll.schedule(cx);
                self.thumb_drag = None;
                let pressed = self.pressed.take();
                *changed = true;
                if self.location_focused {
                    self.location.handle_hit(cx, kit, hit);
                    return ExplorerEvent::None;
                }
                if !(fe.is_over && fe.was_tap()) || fe.has_long_press_occurred {
                    return ExplorerEvent::None;
                }
                let hot = self.hits.at(fe.abs);
                if hot != pressed {
                    return ExplorerEvent::None;
                }
                return self.activate(hot, state, look);
            }
            Hit::KeyDown(_) | Hit::TextInput(_) | Hit::TextIme(_) | Hit::TextCopy(_) | Hit::TextCut(_) if self.location_focused => {
                *changed = true;
                match self.location.handle_hit(cx, kit, hit) {
                    FieldEvent::Submitted { .. } => {
                        let path = self.location.text().trim().to_owned();
                        state.navigate(&path, true);
                    }
                    FieldEvent::Escaped => self.location_focused = false,
                    _ => {}
                }
            }
            Hit::KeyFocusLost(_) => {
                if self.location_focused {
                    self.location_focused = false;
                    *changed = true;
                }
            }
            _ => {}
        }
        ExplorerEvent::None
    }

    fn activate(&mut self, hot: Option<Hot>, state: &mut ExplorerState, look: &ExplorerLook) -> ExplorerEvent {
        match hot {
            Some(Hot::Sort) => state.cycle_sort(),
            Some(Hot::Hidden) => state.toggle_hidden(),
            Some(Hot::Refresh) if !state.refreshing() => state.refresh(),
            Some(Hot::Action(id)) => {
                if !look.busy && look.actions.iter().any(|a| a.id == id && a.enabled) {
                    return ExplorerEvent::Action(id);
                }
            }
            Some(Hot::Navigation) => self.nav_open = !self.nav_open,
            Some(Hot::NavItem(id)) => {
                self.nav_open = false;
                match id {
                    "back" => state.back(),
                    "forward" => state.forward(),
                    "parent" => state.parent(),
                    _ => {
                        state.show_location = !state.show_location;
                        self.location.set_text(state.path());
                    }
                }
            }
            Some(Hot::UseDirectory) => return ExplorerEvent::DirectorySelected(state.absolute_path()),
            Some(Hot::Back) => state.back(),
            Some(Hot::Retry) => {
                let path = state.path().to_owned();
                state.reload(&path);
            }
            Some(Hot::Row(index)) => {
                let Some(row) = state.rows().get(index).cloned() else { return ExplorerEvent::None };
                if row.entry.is_directory() {
                    state.toggle(&row.entry.path);
                } else {
                    state.selected = Some(row.entry.path.clone());
                    return ExplorerEvent::FileSelected(row.entry);
                }
            }
            _ => {}
        }
        ExplorerEvent::None
    }

    /// Keeps the location field in step with navigation.
    pub fn sync_location(&mut self, state: &ExplorerState) {
        if !self.location_focused && self.location.text() != state.location_text {
            self.location.set_text(&state.location_text);
        }
    }
}
