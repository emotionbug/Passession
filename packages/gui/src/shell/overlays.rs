//! Shell-drawn overlays: anchored menus / compact menu sheets (`showPaseoMenu`), confirm and
//! text prompt dialogs, adaptive sheets (`showAdaptiveSheet` + `SheetContents`), toasts
//! (`showToast`) and tooltips. Drawn into a last-overlay draw list; while a modal is open the
//! sweep lock keeps the content below from reacting.

use makepad_widgets::*;
use serde_json::{Value, json};

use crate::common::overlay::{MenuEntry, OverlayKind, OverlayRequest, SelectOption, Tip};
use crate::common::paint::{Font, HitMap, TextSpec, inset, rect};
use crate::common::widgets::PTextFieldWidgetRefExt;
use crate::common::{UiContext, tr};
use crate::shell::sheets::{Sheet, SheetFlow, SheetOps, SheetStyle};
use crate::shell::ui::{Pointer, Press, RowLead, Ui, Variant};
use crate::shell::PaseoShell;
use crate::theme::{self, ControlSize, with_alpha};

pub struct OpenMenu {
    pub id: u64,
    pub owner: String,
    pub title: Option<String>,
    pub anchor: Rect,
    /// Page stack: root entries, then opened submenus (title, entries).
    pub pages: Vec<(Option<String>, Vec<MenuEntry>)>,
    pub width: f64,
    pub align_end: bool,
    pub sheet: bool,
    pub scroll: f64,
}

/// An open `showPaseoSelect` (original `Combobox`).
pub struct OpenSelect {
    pub id: u64,
    pub owner: String,
    pub title: String,
    pub anchor: Rect,
    pub options: Vec<SelectOption>,
    pub value: Option<String>,
    /// The search row's field (`searchable`).
    pub search: Option<WidgetRef>,
    pub empty_text: String,
    pub min_width: f64,
    /// Trimmed, lowercased search text.
    pub query: String,
    pub scroll: f64,
    pub focused: bool,
}

impl OpenSelect {
    /// Indices of the options the query keeps, in order.
    pub fn visible(&self) -> Vec<usize> {
        (0..self.options.len()).filter(|&i| self.options[i].matches(&self.query)).collect()
    }
}

pub enum Modal {
    Menu(OpenMenu),
    Select(OpenSelect),
    Confirm { id: u64, owner: String, title: String, message: String, confirm_label: String, destructive: bool },
    Prompt {
        id: u64,
        owner: String,
        title: String,
        message: Option<String>,
        field: WidgetRef,
        confirm_label: String,
        focused: bool,
        multiline: bool,
    },
    Sheet { id: u64, sheet: Box<dyn Sheet>, scroll: f64, content_height: f64, viewport: Rect, motion: SheetMotion },
}

/// Draggable extent snap points (`minChildSize .3`, `snapSizes [.6]`, `maxChildSize .9`).
const DRAG_SNAPS: [f64; 3] = [0.3, 0.6, 0.9];

/// Presentation state of a sheet: the draggable extent and its animation, the fade-in clock
/// and an active vertical drag. Animations run on next-frame redraws only while moving.
#[derive(Clone, Copy, Debug, Default)]
pub struct SheetMotion {
    /// Draggable: fraction of the window height shown.
    pub extent: f64,
    from: f64,
    to: f64,
    /// Animation start (seconds since app start); `None` when settled.
    start: Option<f64>,
    duration: f64,
    /// FadeBottom: when the panel appeared.
    opened: Option<f64>,
    /// The window height the extent is a fraction of.
    window_h: f64,
    /// The sheet surface last frame (window coordinates), for drags.
    frame: Rect,
    /// Active vertical drag: last pointer y and total travel.
    drag: Option<f64>,
    travel: f64,
}

impl SheetMotion {
    fn animate(&mut self, now: f64, to: f64, duration: f64) {
        self.from = self.extent;
        self.to = to;
        self.start = Some(now);
        self.duration = duration;
    }

    /// Advances the extent animation (ease-out cubic); `true` while still moving.
    fn step(&mut self, now: f64) -> bool {
        let Some(start) = self.start else { return false };
        let t = ((now - start) / self.duration).clamp(0.0, 1.0);
        let eased = 1.0 - (1.0 - t).powi(3);
        self.extent = self.from + (self.to - self.from) * eased;
        if t >= 1.0 {
            self.start = None;
            return false;
        }
        true
    }

    /// The nearest snap point for a released drag.
    fn snap(extent: f64) -> f64 {
        DRAG_SNAPS.iter().copied().fold(DRAG_SNAPS[0], |best, s| if (s - extent).abs() < (best - extent).abs() { s } else { best })
    }
}

impl Modal {
    fn id(&self) -> u64 {
        match self {
            Modal::Menu(m) => m.id,
            Modal::Select(s) => s.id,
            Modal::Confirm { id, .. } | Modal::Prompt { id, .. } | Modal::Sheet { id, .. } => *id,
        }
    }
}

pub struct Toast {
    pub message: String,
    pub error: bool,
    pub timer: Timer,
}

#[derive(Default)]
pub struct Overlays {
    pub stack: Vec<Modal>,
    pub hits: HitMap<String>,
    pub pointer: Pointer,
    pub area: Area,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    pub toast: Option<Toast>,
    pub tooltip: Option<Tip>,
    pub tooltip_pending: Option<Tip>,
    pub tooltip_timer: Timer,
    pub tooltips: std::collections::HashMap<String, String>,
    pub locked: bool,
    /// The area the layer's sweep lock was last taken on.
    pub locked_area: Area,
    /// Next frame while a sheet draws a spinner.
    pub spin: NextFrame,
    next_sheet: u64,
}

impl Overlays {
    pub fn open(&mut self, cx: &mut Cx, request: OverlayRequest, field_ptr: Option<LivePtr>) {
        let modal = match request.kind {
            OverlayKind::Menu { title, anchor, entries, width, align_end, sheet } => Modal::Menu(OpenMenu {
                id: request.id,
                owner: request.owner,
                title: title.clone(),
                anchor,
                pages: vec![(title, entries)],
                width,
                align_end,
                sheet,
                scroll: 0.0,
            }),
            OverlayKind::Confirm { title, message, confirm_label, destructive } => {
                Modal::Confirm { id: request.id, owner: request.owner, title, message, confirm_label, destructive }
            }
            OverlayKind::Prompt { title, message, initial, placeholder, confirm_label, multiline } => {
                let field = WidgetRef::new_from_ptr(cx, field_ptr);
                let text = field.as_ptext_field();
                field.set_text(cx, &initial);
                text.set_placeholder(cx, &placeholder);
                text.set_multiline(cx, multiline);
                Modal::Prompt { id: request.id, owner: request.owner, title, message, field, confirm_label, focused: false, multiline }
            }
            OverlayKind::Select { title, anchor, options, value, searchable, search_placeholder, empty_text, min_width } => {
                let search = searchable.then(|| {
                    let field = WidgetRef::new_from_ptr(cx, field_ptr);
                    let text = field.as_ptext_field();
                    text.set_plain(cx);
                    text.set_placeholder(cx, search_placeholder.as_deref().unwrap_or("Search…"));
                    field
                });
                Modal::Select(OpenSelect {
                    id: request.id,
                    owner: request.owner,
                    title,
                    anchor,
                    options,
                    value,
                    search,
                    empty_text,
                    min_width,
                    query: String::new(),
                    scroll: 0.0,
                    focused: false,
                })
            }
        };
        self.stack.push(modal);
        self.tooltip = None;
        self.tooltip_pending = None;
    }

    pub fn open_sheet(&mut self, sheet: Box<dyn Sheet>) -> u64 {
        self.next_sheet += 1;
        let id = 1_000_000 + self.next_sheet;
        self.stack.push(Modal::Sheet { id, sheet, scroll: 0.0, content_height: 0.0, viewport: Rect::default(), motion: SheetMotion::default() });
        self.tooltip = None;
        self.tooltip_pending = None;
        id
    }

    /// Replaces the open menu's entries (display preferences rebuilt after a toggle).
    pub fn update_menu(&mut self, id: u64, entries: Vec<MenuEntry>) {
        for modal in &mut self.stack {
            if let Modal::Menu(menu) = modal
                && menu.id == id
            {
                let depth = menu.pages.len();
                menu.pages[0].1 = entries.clone();
                // Re-resolve open sub pages by label.
                for level in 1..depth {
                    let title = menu.pages[level].0.clone();
                    let parent = menu.pages[level - 1].1.clone();
                    if let Some(MenuEntry::Submenu { entries, .. }) =
                        parent.iter().find(|e| matches!(e, MenuEntry::Submenu { label, .. } if Some(label) == title.as_ref()))
                    {
                        menu.pages[level].1 = entries.clone();
                    }
                }
            }
        }
    }

    pub fn is_modal(&self) -> bool {
        !self.stack.is_empty()
    }

    pub fn sheet_mut(&mut self, id: u64) -> Option<&mut Box<dyn Sheet>> {
        self.stack.iter_mut().find_map(|m| match m {
            Modal::Sheet { id: sid, sheet, .. } if *sid == id => Some(sheet),
            _ => None,
        })
    }

    pub fn close(&mut self, id: u64) {
        self.stack.retain(|m| m.id() != id);
    }
}

pub fn toast(overlays: &mut Overlays, cx: &mut Cx, message: String, error: bool) {
    let text = message.trim().to_string();
    if text.is_empty() {
        return;
    }
    if let Some(previous) = overlays.toast.take() {
        cx.stop_timer(previous.timer);
    }
    // Original durations: 2200ms, errors 3200ms.
    let timer = cx.start_timeout(if error { 3.2 } else { 2.2 });
    overlays.toast = Some(Toast { message: text, error, timer });
    cx.redraw_all();
}

pub fn handle_timers(overlays: &mut Overlays, cx: &mut Cx, event: &Event) {
    if let Some(toast) = &overlays.toast
        && toast.timer.is_event(event).is_some()
    {
        overlays.toast = None;
        cx.redraw_all();
    }
    if overlays.tooltip_timer.is_event(event).is_some() {
        overlays.tooltip = overlays.tooltip_pending.take();
        cx.redraw_all();
    }
    if overlays.spin.is_event(event).is_some() {
        overlays.area.redraw(cx);
    }
}

/// Chrome hover → tooltip anchored to the hovered control after the theme's 500ms
/// (`TooltipThemeData.waitDuration`).
pub fn hover_tooltip(shell: &mut PaseoShell, cx: &mut Cx, hover: Option<String>, abs: DVec2) {
    let tip = hover.and_then(|id| {
        let text = shell.data.tooltips.get(&id).cloned()?;
        let anchor = shell.data.hits.rect_of(&id).unwrap_or(Rect { pos: abs, size: DVec2::default() });
        Some(Tip { text, anchor, wait: 0.5 })
    });
    show_tooltip(&mut shell.data.overlays, cx, tip);
}

/// Shows `tip` after its wait, or hides the tooltip (`None`). Feature roots reach this through
/// `common::overlay::{tooltip, tooltip_at}`.
pub fn show_tooltip(overlays: &mut Overlays, cx: &mut Cx, tip: Option<Tip>) {
    let tip = tip.filter(|t| !t.text.is_empty());
    if overlays.tooltip_pending.is_some() && overlays.tooltip_pending.as_ref().map(|t| &t.text) == tip.as_ref().map(|t| &t.text) {
        return;
    }
    cx.stop_timer(overlays.tooltip_timer);
    let had = overlays.tooltip.take().is_some();
    overlays.tooltip_pending = None;
    if let Some(tip) = tip {
        overlays.tooltip_timer = cx.start_timeout(tip.wait.max(0.0));
        overlays.tooltip_pending = Some(tip);
    }
    if had {
        cx.redraw_all();
    }
}

// ---------------------------------------------------------------------------------------------
// Geometry

fn menu_row_height(compact: bool) -> f64 {
    if compact { 40.0 } else { 28.0 }
}

fn entries_height(entries: &[MenuEntry], compact: bool) -> f64 {
    let mut h = 8.0;
    for entry in entries {
        h += match entry {
            MenuEntry::Separator => 9.0,
            MenuEntry::Label(_) => 28.0,
            MenuEntry::Item(item) => menu_row_height(compact) + if item.description.is_some() { 16.0 } else { 0.0 },
            MenuEntry::Submenu { .. } => menu_row_height(compact),
        };
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Drawing

fn overlay_ui(shell: &mut PaseoShell) -> Ui<'_> {
    let compact = shell.data.compact;
    let overlays = &mut shell.data.overlays;
    Ui {
        painter: &mut shell.overlay_painter,
        draw_icon: &mut shell.overlay_icon,
        hits: &mut overlays.hits,
        tooltips: &mut overlays.tooltips,
        hover: overlays.pointer.hover.as_deref(),
        pressed: overlays.pointer.pressed.as_deref(),
        p: theme::current_palette(),
        t: theme::current_typography(),
        compact,
    }
}

pub fn draw(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, window: Rect) {
    shell.overlay_list.begin_overlay_last(cx);
    cx.begin_turtle(Walk::abs_rect(window), Layout::default());
    shell.data.overlays.hits.clear();
    shell.data.overlays.tooltips.clear();
    // Full-window capture area for the overlay layer.
    shell.data.overlays.area = shell.overlay_painter.capture_box(cx, window, Vec4::default(), Vec4::default(), 0.0, 0.0);

    crate::shell::chrome::draw_compact_explorer(shell, cx, scope, window);

    let count = shell.data.overlays.stack.len();
    for index in 0..count {
        let top = index + 1 == count;
        let p = theme::current_palette();
        // Each modal draws above the one below it, text included.
        new_layer(shell, cx);
        // Scrim: menus and select popovers are transparent; dialogs and sheets dim the window.
        let scrim = match &shell.data.overlays.stack[index] {
            Modal::Menu(menu) => {
                if shell.data.compact && menu.sheet { p.scrim } else { Vec4::default() }
            }
            Modal::Select(_) => {
                if shell.data.compact { p.scrim } else { Vec4::default() }
            }
            Modal::Sheet { sheet, motion, .. } => match sheet.style() {
                SheetStyle::FullScreen => Vec4::default(),
                // `barrierColor: 0x59000000`, fading in with the panel.
                SheetStyle::FadeBottom => with_alpha(theme::argb(0x59000000), 0.35 * fade_progress(motion, cx.seconds_since_app_start()) as f32),
                _ => argb_scrim(shell.data.compact),
            },
            _ => argb_scrim(shell.data.compact),
        };
        shell.overlay_painter.fill(cx, window, scrim, 0.0);
        if top {
            shell.data.overlays.hits.push(window, "overlay:backdrop".into());
        }
        draw_modal(shell, cx, scope, index, window, top);
    }
    // Standalone tool-call sheets (timeline `showToolCallSheet`) above the modal stack.
    crate::features::timeline::draw_tool_call_sheets(cx, &shell.data.window_id, window);
    new_layer(shell, cx);
    draw_toast(shell, cx, window);
    draw_tooltip(shell, cx, window);
    cx.end_turtle();
    shell.overlay_list.end(cx);
    shell.data.overlays.locked = shell.data.overlays.is_modal() || (shell.data.compact && shell.data.chrome.explorer_position > 0.0);
}

fn argb_scrim(compact: bool) -> Vec4 {
    theme::argb(if compact { 0x73000000 } else { 0x8c000000 })
}

fn new_layer(shell: &mut PaseoShell, cx: &mut Cx2d) {
    shell.overlay_painter.new_layer(cx);
    shell.overlay_icon.new_draw_call(cx);
}

fn draw_modal(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, index: usize, window: Rect, top: bool) {
    let compact = shell.data.compact;
    let mut modal = std::mem::replace(&mut shell.data.overlays.stack[index], Modal::Confirm {
        id: 0,
        owner: String::new(),
        title: String::new(),
        message: String::new(),
        confirm_label: String::new(),
        destructive: false,
    });
    match &mut modal {
        Modal::Menu(menu) => draw_menu(shell, cx, menu, window, compact, top),
        Modal::Confirm { title, message, confirm_label, destructive, .. } => {
            draw_confirm(shell, cx, window, title, message, confirm_label, *destructive, top);
        }
        Modal::Prompt { title, message, field, confirm_label, focused, multiline, .. } => {
            draw_prompt(shell, cx, scope, window, title, message.as_deref(), field, confirm_label, *multiline, top);
            if !*focused {
                *focused = true;
                field.as_ptext_field().set_key_focus(cx);
                field.as_ptext_field().select_all(cx);
            }
        }
        Modal::Select(select) => {
            draw_select(shell, cx, scope, select, window, compact, top);
            // Original: the search field autofocuses on wide layouts only.
            if !select.focused {
                select.focused = true;
                if let Some(search) = select.search.as_ref().filter(|_| !compact) {
                    search.as_ptext_field().set_key_focus(cx);
                }
            }
        }
        Modal::Sheet { sheet, scroll, content_height, viewport, motion, .. } => {
            let state = scope.data.get::<UiContext>().map(|ui| ui.state.clone()).unwrap_or_default();
            sheet.sync(shell, &state);
            draw_sheet(shell, cx, scope, window, sheet.as_mut(), scroll, content_height, viewport, motion, top);
        }
    }
    shell.data.overlays.stack[index] = modal;
}

fn menu_rect(menu: &OpenMenu, window: Rect, height: f64) -> Rect {
    let width = menu.width.min(window.size.x - 16.0);
    let a = menu.anchor;
    let x = if menu.align_end { a.pos.x + a.size.x - width } else { a.pos.x };
    let below = a.pos.y + a.size.y + 4.0;
    let h = height.min(window.size.y - 16.0);
    let y = if below + h <= window.pos.y + window.size.y - 8.0 { below } else { a.pos.y - 4.0 - h };
    rect(
        x.clamp(window.pos.x + 8.0, (window.pos.x + window.size.x - width - 8.0).max(window.pos.x + 8.0)),
        y.clamp(window.pos.y + 8.0, (window.pos.y + window.size.y - h - 8.0).max(window.pos.y + 8.0)),
        width,
        h,
    )
}

fn draw_menu(shell: &mut PaseoShell, cx: &mut Cx2d, menu: &mut OpenMenu, window: Rect, compact: bool, top: bool) {
    let sheet = compact && menu.sheet;
    let depth = menu.pages.len();
    let (page_title, entries) = menu.pages[depth - 1].clone();
    let header = sheet || depth > 1;
    let header_h = if header { if sheet { 40.0 } else { 44.0 } } else { 0.0 };
    let content_h = entries_height(&entries, compact);
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let surface = if sheet {
        // Keep the background edge-to-edge, but reserve the render surface's remaining
        // system-bar overlap below the menu content (already-consumed insets are zero).
        let insets = crate::shell::platform::safe_insets();
        let h = (content_h + header_h + 24.0 + insets.bottom)
            .min(window.size.y * 0.8)
            .min((window.size.y - insets.top).max(0.0));
        let r = rect(window.pos.x, window.pos.y + window.size.y - h, window.size.x, h);
        ui.painter.draw_box.paint_bordered(cx, rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 12.0), p.surface0, p.border, 1.0, 12.0);
        ui.sheet_handle(cx, r.pos.x + r.size.x * 0.5, r.pos.y);
        rect(r.pos.x, r.pos.y + 24.0, r.size.x, (r.size.y - 24.0 - insets.bottom).max(0.0))
    } else {
        let r = menu_rect(menu, window, content_h + header_h);
        // md shadow then the surface1 popover with a borderAccent rule.
        let shadow = if p.is_dark { theme::argb(0x33000000) } else { theme::argb(0x0a000000) };
        ui.fill(cx, rect(r.pos.x - 2.0, r.pos.y + 2.0, r.size.x + 4.0, r.size.y + 6.0), shadow, 10.0);
        ui.painter.outline(cx, r, p.surface1, p.border_accent, 1.0, 8.0);
        r
    };
    if top {
        ui.hit(surface, "menu:surface");
    }
    let mut y = surface.pos.y;
    if header {
        let title = page_title.clone().or(menu.title.clone()).unwrap_or_default();
        let mut x = surface.pos.x + 12.0;
        if depth > 1 {
            let back = rect(x, y + 4.0, 24.0, 24.0);
            ui.icon(cx, back, "chevron-left", 18.0, p.muted);
            if top {
                ui.hit(back, "menu:back");
            }
            x += 32.0;
        }
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        ui.text(cx, rect(x, y + 4.0, surface.size.x - (x - surface.pos.x) - 12.0, 24.0), spec, &title);
        y += header_h - 8.0;
    }
    // The header stays fixed while tall menus scroll within their visible, touchable body.
    let viewport = rect(surface.pos.x, y, surface.size.x, (surface.pos.y + surface.size.y - y).max(0.0));
    menu.scroll = menu.scroll.clamp(0.0, (content_h - viewport.size.y).max(0.0));
    cx.begin_turtle(Walk::abs_rect(viewport), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    ui.hits.set_clip(Some(viewport));
    y += 4.0 - menu.scroll;
    let row_h = menu_row_height(compact);
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            MenuEntry::Separator => {
                ui.menu_separator(cx, surface.pos.x, y, surface.size.x);
                y += 9.0;
            }
            MenuEntry::Label(text) => {
                let spec = ui.spec(Font::Regular, 12.0, p.muted);
                ui.text(cx, rect(surface.pos.x + 12.0, y + 8.0, surface.size.x - 24.0, 16.0), spec, text);
                y += 28.0;
            }
            MenuEntry::Item(item) => {
                let h = row_h + if item.description.is_some() { 16.0 } else { 0.0 };
                let id = if top { format!("menu:item:{i}") } else { String::new() };
                let lead = match (&item.svg, &item.icon, item.dot) {
                    (_, _, Some(color)) => RowLead::Dot(color),
                    (Some(svg), _, None) => RowLead::Svg(svg),
                    (None, Some(icon), None) => RowLead::Glyph(icon),
                    (None, None, None) => RowLead::None,
                };
                ui.menu_row(
                    cx,
                    rect(surface.pos.x, y, surface.size.x, row_h),
                    &id,
                    &item.label,
                    lead,
                    item.hint.as_deref(),
                    item.enabled && top,
                    item.muted,
                    item.destructive,
                    item.checked,
                    false,
                );
                if let Some(description) = &item.description {
                    let spec = ui.spec(Font::Regular, 12.0, p.muted);
                    ui.text(cx, rect(surface.pos.x + 36.0, y + row_h - 4.0, surface.size.x - 48.0, 16.0), spec, description);
                }
                y += h;
            }
            MenuEntry::Submenu { label, icon, value, indicator, .. } => {
                let r = rect(surface.pos.x, y, surface.size.x, row_h);
                let id = format!("menu:sub:{i}");
                let lead = icon.as_deref().map_or(RowLead::None, RowLead::Glyph);
                ui.menu_row(cx, r, &id, label, lead, None, top, false, false, false, false);
                let mut right = r.pos.x + r.size.x - 12.0;
                ui.icon(cx, rect(right - 14.0, r.pos.y + (r.size.y - 14.0) * 0.5, 14.0, 14.0), "chevron-right", 14.0, p.muted);
                right -= 20.0;
                if *indicator {
                    ui.dot(cx, dvec2(right - 3.0, r.pos.y + r.size.y * 0.5), 6.0, p.accent);
                    right -= 12.0;
                }
                if let Some(value) = value {
                    let spec = ui.spec(Font::Regular, 14.0, p.muted);
                    ui.text_right(cx, rect(r.pos.x + r.size.x * 0.5, r.pos.y, right - r.pos.x - r.size.x * 0.5, r.size.y), spec, value);
                }
                y += row_h;
            }
        }
    }
    ui.hits.set_clip(None);
    cx.end_turtle();
}

/// `_SelectList` row height (36px min) and the sheet's row margin below each row.
const SELECT_ROW_H: f64 = 36.0;

fn select_list_height(select: &OpenSelect, visible: &[usize], sheet: bool) -> f64 {
    if visible.is_empty() {
        return 8.0 + 20.0 + 8.0;
    }
    let mut h = if sheet { 4.0 + 32.0 } else { 8.0 };
    for &i in visible {
        if select.options[i].separator_before && select.query.is_empty() {
            h += 9.0;
        }
        h += SELECT_ROW_H + if sheet { 4.0 } else { 0.0 };
    }
    h
}

/// Original `showPaseoSelect`: anchored surface0 popover (8px radius, divider border, md
/// shadow, 400px max) on wide layouts; a surface0 bottom sheet with a drag handle and the
/// medium title on compact ones. The optional search row filters label and description.
fn draw_select(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, select: &mut OpenSelect, window: Rect, sheet: bool, top: bool) {
    let visible = select.visible();
    let search_h = if select.search.is_some() { 45.0 } else { 0.0 };
    let list_h = select_list_height(select, &visible, sheet);
    let field_rect = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        let (surface, mut y) = if sheet {
            let title_h = 28.0;
            let h = (48.0 + title_h + search_h + list_h).min(window.size.y - 24.0);
            let r = rect(window.pos.x, window.pos.y + window.size.y - h, window.size.x, h);
            ui.painter.draw_box.paint(cx, rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 16.0), p.surface0, 16.0);
            // Material 3 drag handle: 32x4 in a 48px band, onSurfaceVariant at 40%.
            ui.fill(cx, rect(r.pos.x + r.size.x * 0.5 - 16.0, r.pos.y + 22.0, 32.0, 4.0), with_alpha(p.muted, 0.4), 2.0);
            let spec = ui.spec(Font::Medium, 14.0, p.foreground);
            ui.text(cx, rect(r.pos.x + 24.0, r.pos.y + 48.0, r.size.x - 48.0, 20.0), spec, &select.title);
            (r, r.pos.y + 48.0 + title_h)
        } else {
            let a = select.anchor;
            let width = a.size.x.max(select.min_width).min(window.size.x - 16.0);
            let bottom = window.pos.y + window.size.y;
            let below = bottom - (a.pos.y + a.size.y) - 8.0;
            let above = a.pos.y - window.pos.y;
            let open_up = below < 240.0 && above > below;
            let left = if a.pos.x + width > window.pos.x + window.size.x - 8.0 {
                (window.pos.x + window.size.x - 8.0 - width).max(window.pos.x + 8.0)
            } else {
                a.pos.x
            };
            let max_h = (if open_up { above - 12.0 } else { below - 4.0 }).clamp(120.0, 400.0);
            let h = (search_h + list_h).min(max_h);
            let y = if open_up { a.pos.y - 4.0 - h } else { a.pos.y + a.size.y + 4.0 };
            let r = rect(left, y, width, h);
            // BoxShadow(0x33000000, offset (0, 4), blur 8), approximated by two soft layers.
            ui.fill(cx, rect(r.pos.x - 2.0, r.pos.y + 2.0, r.size.x + 4.0, r.size.y + 6.0), theme::argb(0x14000000), 10.0);
            ui.fill(cx, rect(r.pos.x, r.pos.y + 3.0, r.size.x, r.size.y + 2.0), theme::argb(0x1f000000), 9.0);
            ui.painter.outline(cx, r, p.surface0, p.border, 1.0, 8.0);
            (r, r.pos.y)
        };
        if top {
            ui.hit(surface, "select:surface");
        }
        let mut field_rect = None;
        if select.search.is_some() {
            let inset_x = if sheet { 4.0 } else { 0.0 };
            let row = rect(surface.pos.x + inset_x, y, surface.size.x - inset_x * 2.0, search_h);
            // The popover clips its children to the 8px corners: round the top, square the rest.
            if sheet {
                ui.fill(cx, rect(row.pos.x, row.pos.y, row.size.x, row.size.y - 1.0), p.surface1, 0.0);
            } else {
                let inner = rect(row.pos.x + 1.0, row.pos.y + 1.0, row.size.x - 2.0, row.size.y - 2.0);
                ui.fill(cx, rect(inner.pos.x, inner.pos.y, inner.size.x, 16.0), p.surface1, 7.0);
                ui.fill(cx, rect(inner.pos.x, inner.pos.y + 8.0, inner.size.x, inner.size.y - 8.0), p.surface1, 0.0);
            }
            ui.painter.hline(cx, row.pos.x, row.pos.y + row.size.y - 1.0, row.size.x, p.border);
            ui.icon(cx, rect(row.pos.x + 12.0, row.pos.y + (44.0 - 16.0) * 0.5, 16.0, 16.0), "search", 16.0, p.muted);
            field_rect = Some(rect(row.pos.x + 12.0 + 16.0 + 8.0, row.pos.y, row.size.x - 12.0 - 16.0 - 8.0 - 12.0, 44.0));
            y += search_h;
        }
        let viewport = rect(surface.pos.x, y, surface.size.x, (surface.pos.y + surface.size.y - y).max(0.0));
        select.scroll = select.scroll.min((list_h - viewport.size.y).max(0.0));
        cx.begin_turtle(Walk::abs_rect(viewport), Layout::default());
        let mut ry = viewport.pos.y - select.scroll;
        if visible.is_empty() {
            let spec = ui.spec(Font::Regular, 14.0, p.muted);
            ui.text(cx, rect(viewport.pos.x + 12.0, ry + 8.0, viewport.size.x - 24.0, 20.0), spec, &select.empty_text);
        } else {
            ry += 4.0;
            let (mx, mb) = if sheet { (8.0 + 4.0, 4.0) } else { (0.0, 0.0) };
            for &i in &visible {
                let option = &select.options[i];
                if option.separator_before && select.query.is_empty() {
                    ui.painter.hline(cx, viewport.pos.x + mx - if sheet { 4.0 } else { 0.0 }, ry + 4.0, viewport.size.x - (mx - if sheet { 4.0 } else { 0.0 }) * 2.0, p.border);
                    ry += 9.0;
                }
                let r = rect(viewport.pos.x + mx, ry, viewport.size.x - mx * 2.0, SELECT_ROW_H);
                let id = format!("select:item:{i}");
                draw_select_row(&mut ui, cx, r, &id, option, select.value.as_deref() == Some(option.value.as_str()));
                if top && option.enabled {
                    let clipped = clip_rect(r, viewport);
                    if clipped.size.y > 0.0 {
                        ui.hit(clipped, id);
                    }
                }
                ry += SELECT_ROW_H + mb;
            }
        }
        cx.end_turtle();
        field_rect
    };
    if let (Some(field), Some(r)) = (select.search.as_ref(), field_rect) {
        cx.begin_turtle(Walk::abs_rect(r), Layout::default());
        field.draw_all(cx, scope);
        cx.end_turtle();
    }
}

/// `_SelectItem`: 12/8 padding, surface1 while hovered or pressed, 16px leading slot, 14px
/// label with a muted 12px description after it, trailing slot and a muted check when chosen,
/// 55% opacity when disabled.
fn draw_select_row(ui: &mut Ui, cx: &mut Cx2d, r: Rect, id: &str, option: &SelectOption, selected: bool) {
    let p = ui.p;
    let alpha = if option.enabled { 1.0 } else { 0.55 };
    if option.enabled && (ui.is_hover(id) || ui.is_pressed(id)) {
        ui.fill(cx, r, p.surface1, 0.0);
    }
    let mut x = r.pos.x + 12.0;
    let mut right = r.pos.x + r.size.x - 12.0;
    let mid = r.pos.y + r.size.y * 0.5;
    if let Some(icon) = &option.icon {
        ui.icon(cx, rect(x, mid - 8.0, 16.0, 16.0), icon, option.icon_size, with_alpha(p.muted, alpha));
        x += 16.0 + 8.0;
    } else if let Some(dot) = option.dot {
        ui.dot(cx, dvec2(x + 8.0, mid), 8.0, with_alpha(dot, dot.w * alpha));
        x += 16.0 + 8.0;
    }
    if selected {
        ui.icon(cx, rect(right - 16.0, mid - 8.0, 16.0, 16.0), "check", 16.0, with_alpha(p.muted, alpha));
        right -= 16.0 + 8.0;
    }
    if let Some(trailing) = &option.trailing {
        let spec = ui.spec(Font::Regular, 12.0, with_alpha(p.muted, alpha));
        let w = ui.measure(cx, spec, trailing);
        ui.text_right(cx, rect(right - w, r.pos.y, w, r.size.y), spec, trailing);
        right -= w + 8.0;
    }
    let label_spec = ui.spec(Font::Regular, 14.0, with_alpha(p.foreground, alpha));
    let avail = (right - x).max(0.0);
    let label_w = ui.measure(cx, label_spec, &option.label).min(avail);
    ui.text(cx, rect(x, r.pos.y, label_w, r.size.y), label_spec, &option.label);
    if let Some(description) = &option.description {
        let dx = x + label_w + 8.0;
        if right - dx > 8.0 {
            let spec = ui.spec(Font::Regular, 12.0, with_alpha(p.muted, alpha));
            ui.text(cx, rect(dx, r.pos.y + 1.0, right - dx, r.size.y), spec, description);
        }
    }
}

fn clip_rect(r: Rect, clip: Rect) -> Rect {
    let y0 = r.pos.y.max(clip.pos.y);
    let y1 = (r.pos.y + r.size.y).min(clip.pos.y + clip.size.y);
    rect(r.pos.x, y0, r.size.x, (y1 - y0).max(0.0))
}

/// `SheetContents`-style chrome around a dialog body; returns the body rect. `back` shows the
/// leading back chevron (`onBack`), whose click reaches the sheet as `sheet:back`.
#[allow(clippy::too_many_arguments)]
fn dialog_frame(ui: &mut Ui, cx: &mut Cx2d, window: Rect, width: f64, height: f64, title: &str, compact: bool, top: bool, back: bool) -> Rect {
    let p = ui.p;
    let frame = if compact {
        let h = (height + 57.0 + 24.0).min(window.size.y * 0.9);
        let r = rect(window.pos.x, window.pos.y + window.size.y - h, window.size.x, h);
        ui.painter.draw_box.paint(cx, rect(r.pos.x, r.pos.y, r.size.x, r.size.y + 16.0), p.surface0, 16.0);
        ui.sheet_handle(cx, r.pos.x + r.size.x * 0.5, r.pos.y - 4.0);
        rect(r.pos.x, r.pos.y + 16.0, r.size.x, r.size.y - 16.0)
    } else {
        let w = width.min(window.size.x - 48.0);
        let h = (height + 57.0).min(window.size.y * 0.85);
        let r = rect(window.pos.x + (window.size.x - w) * 0.5, window.pos.y + (window.size.y - h) * 0.5, w, h);
        ui.painter.outline(cx, r, p.surface1, p.surface2, 1.0, 12.0);
        r
    };
    if top {
        ui.hit(frame, "dialog:surface");
    }
    let spec = ui.spec(Font::Medium, 14.0, p.foreground);
    let mut title_x = frame.pos.x + 24.0;
    if back {
        let r = rect(frame.pos.x + 12.0, frame.pos.y + 12.0, 32.0, 32.0);
        ui.icon_button(cx, r, if top { "sheet:back" } else { "" }, "chevron-left", 16.0, 8.0, if compact { "" } else { "Back" });
        title_x = r.pos.x + 32.0 + 4.0;
    }
    ui.text(cx, rect(title_x, frame.pos.y + 16.0, frame.pos.x + frame.size.x - 56.0 - title_x, 24.0), spec, title);
    let close = rect(frame.pos.x + frame.size.x - 16.0 - 32.0, frame.pos.y + 12.0, 32.0, 32.0);
    ui.icon_button(cx, close, if top { "dialog:close" } else { "" }, "x", 16.0, 8.0, if compact { "" } else { "Close" });
    ui.painter.hline(cx, frame.pos.x, frame.pos.y + 56.0, frame.size.x, p.surface2);
    rect(frame.pos.x, frame.pos.y + 57.0, frame.size.x, frame.size.y - 57.0)
}

#[allow(clippy::too_many_arguments)]
fn draw_confirm(shell: &mut PaseoShell, cx: &mut Cx2d, window: Rect, title: &str, message: &str, confirm_label: &str, destructive: bool, top: bool) {
    let compact = shell.data.compact;
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 14.0, p.muted);
    let width: f64 = 420.0;
    let text_w = (width.min(window.size.x - 48.0)) - 48.0;
    let text_h = ui.painter.paragraph_height(cx, if compact { window.size.x - 48.0 } else { text_w }, spec, message);
    let body = dialog_frame(&mut ui, cx, window, width, text_h + 24.0 + 56.0, title, compact, top, false);
    ui.painter.paragraph(cx, dvec2(body.pos.x + 24.0, body.pos.y + 16.0), body.size.x - 48.0, spec, message);
    let y = body.pos.y + body.size.y - 48.0;
    let cancel = tr("common.actions.cancel");
    let cw = ui.button_width(cx, &cancel, None, ControlSize::Sm);
    let ow = ui.button_width(cx, confirm_label, None, ControlSize::Sm);
    let right = body.pos.x + body.size.x - 24.0;
    ui.button(cx, rect(right - ow, y, ow, 32.0), if top { "confirm:ok" } else { "" }, confirm_label, None, if destructive { Variant::Destructive } else { Variant::Primary }, ControlSize::Sm, top);
    ui.button(cx, rect(right - ow - 8.0 - cw, y, cw, 32.0), if top { "confirm:cancel" } else { "" }, &cancel, None, Variant::Ghost, ControlSize::Sm, top);
}

#[allow(clippy::too_many_arguments)]
fn draw_prompt(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    title: &str,
    message: Option<&str>,
    field: &WidgetRef,
    confirm_label: &str,
    multiline: bool,
    top: bool,
) {
    let compact = shell.data.compact;
    let (field_rect, scroll) = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        let spec = ui.spec(Font::Regular, 14.0, p.muted);
        let message_h = message.map_or(0.0, |m| ui.painter.paragraph_height(cx, 432.0, spec, m) + 12.0);
        // `minLines: 4, maxLines: 12`: the field grows with its wrapped text and scrolls past
        // twelve lines, keeping the end (where typing happens) in view.
        let (field_h, scroll) = if multiline {
            let text_spec = ui.spec(Font::Regular, 14.0, p.foreground);
            let line_h = ui.painter.line_height(cx, text_spec);
            let width = (480.0f64.min(window.size.x - 48.0) - 48.0 - 24.0).max(40.0);
            let text = field.text();
            let lines = (ui.painter.paragraph_height(cx, width, text_spec, if text.is_empty() { " " } else { &text }) / line_h).ceil().max(1.0);
            let visible = lines.clamp(4.0, 12.0);
            (visible * line_h + 12.0, (lines - visible) * line_h)
        } else {
            (40.0, 0.0)
        };
        let body = dialog_frame(&mut ui, cx, window, 480.0, message_h + 16.0 + field_h + 4.0 + 72.0, title, compact, top, false);
        let mut y = body.pos.y + 16.0;
        if let Some(message) = message {
            ui.painter.paragraph(cx, dvec2(body.pos.x + 24.0, y), body.size.x - 48.0, spec, message);
            y += message_h;
        }
        let field_rect = rect(body.pos.x + 24.0, y, body.size.x - 48.0, field_h);
        let by = body.pos.y + body.size.y - 48.0;
        let cancel = tr("common.actions.cancel");
        let cw = ui.button_width(cx, &cancel, None, ControlSize::Sm);
        let ow = ui.button_width(cx, confirm_label, None, ControlSize::Sm);
        let right = body.pos.x + body.size.x - 24.0;
        ui.button(cx, rect(right - ow, by, ow, 32.0), if top { "prompt:ok" } else { "" }, confirm_label, None, Variant::Primary, ControlSize::Sm, top);
        ui.button(cx, rect(right - ow - 8.0 - cw, by, cw, 32.0), if top { "prompt:cancel" } else { "" }, &cancel, None, Variant::Ghost, ControlSize::Sm, top);
        (field_rect, scroll)
    };
    cx.begin_turtle(Walk::abs_rect(field_rect), Layout { scroll: dvec2(0.0, scroll), ..Layout::default() });
    field.draw_all(cx, scope);
    cx.end_turtle();
}

#[allow(clippy::too_many_arguments)]
fn draw_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    scroll: &mut f64,
    content_height: &mut f64,
    viewport: &mut Rect,
    motion: &mut SheetMotion,
    top: bool,
) {
    let compact = shell.data.compact;
    let title = sheet.title();
    match sheet.style() {
        SheetStyle::FullScreen => {
            draw_full_screen_sheet(shell, cx, scope, window, sheet, &title, viewport, top);
            return;
        }
        SheetStyle::Draggable { material_handle } if compact => {
            draw_draggable_sheet(shell, cx, scope, window, sheet, &title, material_handle, scroll, content_height, viewport, motion, top);
            return;
        }
        SheetStyle::FadeBottom => {
            draw_fade_sheet(shell, cx, scope, window, sheet, content_height, viewport, motion, top);
            return;
        }
        _ => {}
    }
    let width = sheet.max_width();
    let back = sheet.can_go_back();
    let estimated = if *content_height > 0.0 { *content_height } else { 240.0 };
    let body = {
        let mut ui = overlay_ui(shell);
        dialog_frame(&mut ui, cx, window, width, estimated + 48.0, &title, compact, top, back)
    };
    let inner = rect(body.pos.x, body.pos.y, body.size.x, body.size.y);
    *viewport = inner;
    let max = (*content_height + 48.0 - inner.size.y).max(0.0);
    *scroll = scroll.clamp(0.0, max);
    cx.begin_turtle(Walk::abs_rect(inner), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    let height = {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(inner));
        let h = sheet.draw(cx, &mut ui, scope, inner.pos.x + 24.0, inner.pos.y + 24.0 - *scroll, inner.size.x - 48.0, top);
        ui.hits.set_clip(None);
        h
    };
    cx.end_turtle();
    if crate::shell::sheets::take_spinning() {
        shell.data.overlays.spin = cx.new_next_frame();
    }
    if (height - *content_height).abs() > 0.5 {
        *content_height = height;
        shell.redraw(cx);
    }
}

/// How far a fading sheet has faded in (200ms `FadeTransition`).
fn fade_progress(motion: &SheetMotion, now: f64) -> f64 {
    motion.opened.map_or(0.0, |opened| ((now - opened) / 0.2).clamp(0.0, 1.0))
}

/// `SheetStyle::Draggable` on compact layouts.
#[allow(clippy::too_many_arguments)]
fn draw_draggable_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    title: &str,
    material_handle: bool,
    scroll: &mut f64,
    content_height: &mut f64,
    viewport: &mut Rect,
    motion: &mut SheetMotion,
    top: bool,
) {
    let now = cx.seconds_since_app_start();
    if motion.extent == 0.0 && motion.start.is_none() {
        // `showModalBottomSheet` enter: slide up to the initial .6 over 250ms.
        motion.animate(now, 0.6, 0.25);
    }
    if motion.step(now) {
        shell.data.overlays.spin = cx.new_next_frame();
    }
    let insets = crate::shell::platform::safe_insets();
    // `useSafeArea`: never under the top inset.
    let max_h = window.size.y - insets.top;
    let h = (window.size.y * motion.extent).min(max_h).max(0.0);
    let frame = rect(window.pos.x, window.pos.y + window.size.y - h, window.size.x, h);
    motion.frame = frame;
    motion.window_h = window.size.y;
    let body = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        ui.fill(cx, rect(frame.pos.x, frame.pos.y, frame.size.x, frame.size.y + 16.0), p.surface0, 16.0);
        if top {
            ui.hit(frame, "dialog:surface");
        }
        let cxm = frame.pos.x + frame.size.x * 0.5;
        let band = if material_handle {
            // Material 3 drag handle: 32x4 in a 48px band, onSurfaceVariant at 40%.
            ui.fill(cx, rect(cxm - 16.0, frame.pos.y + 22.0, 32.0, 4.0), with_alpha(p.muted, 0.4), 2.0);
            48.0
        } else {
            // The sheet's own 36x4 surface2 handle with 10px margins.
            ui.fill(cx, rect(cxm - 18.0, frame.pos.y + 10.0, 36.0, 4.0), p.surface2, 2.0);
            24.0
        };
        let spec = ui.spec(Font::Medium, 14.0, p.foreground);
        ui.text(cx, rect(frame.pos.x + 24.0, frame.pos.y + band, frame.size.x - 48.0, 20.0), spec, title);
        let y = frame.pos.y + band + 20.0 + 8.0;
        let footer_h = sheet.footer_height();
        let footer_band = if footer_h > 0.0 { 1.0 + 12.0 + footer_h + 12.0 + insets.bottom } else { insets.bottom };
        let bottom = frame.pos.y + frame.size.y - footer_band;
        if footer_h > 0.0 {
            ui.painter.hline(cx, frame.pos.x, bottom, frame.size.x, p.border);
            sheet.draw_footer(cx, &mut ui, frame.pos.x + 24.0, bottom + 1.0 + 12.0, frame.size.x - 48.0, top);
        }
        rect(frame.pos.x, y, frame.size.x, (bottom - y).max(0.0))
    };
    *viewport = body;
    // List padding (8, 4, 8, 32).
    let max = (*content_height + 4.0 + 32.0 - body.size.y).max(0.0);
    *scroll = scroll.clamp(0.0, max);
    cx.begin_turtle(Walk::abs_rect(body), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    let height = {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(body));
        let h = sheet.draw(cx, &mut ui, scope, body.pos.x + 8.0, body.pos.y + 4.0 - *scroll, body.size.x - 16.0, top);
        ui.hits.set_clip(None);
        h
    };
    cx.end_turtle();
    if crate::shell::sheets::take_spinning() {
        shell.data.overlays.spin = cx.new_next_frame();
    }
    if (height - *content_height).abs() > 0.5 {
        *content_height = height;
        shell.redraw(cx);
    }
}

/// `SheetStyle::FadeBottom`: a full-width surface2 panel at the bottom, fading in.
#[allow(clippy::too_many_arguments)]
fn draw_fade_sheet(
    shell: &mut PaseoShell,
    cx: &mut Cx2d,
    scope: &mut Scope,
    window: Rect,
    sheet: &mut dyn Sheet,
    content_height: &mut f64,
    viewport: &mut Rect,
    motion: &mut SheetMotion,
    top: bool,
) {
    let now = cx.seconds_since_app_start();
    let opened = *motion.opened.get_or_insert(now);
    let t = ((now - opened) / 0.2).clamp(0.0, 1.0);
    if t < 1.0 {
        shell.data.overlays.spin = cx.new_next_frame();
    }
    let insets = crate::shell::platform::safe_insets();
    let bottom = if insets.bottom > 24.0 { insets.bottom } else { 24.0 };
    let estimated = if *content_height > 0.0 { *content_height } else { 72.0 };
    let h = 16.0 + 4.0 + 16.0 + estimated + bottom;
    let frame = rect(window.pos.x, window.pos.y + window.size.y - h, window.size.x, h);
    motion.frame = frame;
    shell.overlay_painter.set_alpha(t as f32);
    {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        ui.fill(cx, rect(frame.pos.x, frame.pos.y, frame.size.x, frame.size.y + 16.0), p.surface2, 16.0);
        if top {
            ui.hit(frame, "dialog:surface");
        }
        ui.fill(cx, rect(frame.pos.x + frame.size.x * 0.5 - 20.0, frame.pos.y + 16.0, 40.0, 4.0), with_alpha(p.muted, 0.3), 2.0);
    }
    let body = rect(frame.pos.x + 24.0, frame.pos.y + 16.0 + 4.0 + 16.0, frame.size.x - 48.0, estimated);
    *viewport = body;
    let height = {
        let mut ui = overlay_ui(shell);
        sheet.draw(cx, &mut ui, scope, body.pos.x, body.pos.y, body.size.x, top)
    };
    shell.overlay_painter.set_alpha(1.0);
    if (height - *content_height).abs() > 0.5 {
        *content_height = height;
        shell.redraw(cx);
    }
}

/// A pushed full-screen route (`Scaffold(backgroundColor: surface0)`): the original
/// `BackHeader` (safe area + 8px, 12px sides, 56px row, 32px round back island with a 16px
/// muted arrow, lg medium title 8px after it), then the body with 24px sides and bottom.
#[allow(clippy::too_many_arguments)]
fn draw_full_screen_sheet(shell: &mut PaseoShell, cx: &mut Cx2d, scope: &mut Scope, window: Rect, sheet: &mut dyn Sheet, title: &str, viewport: &mut Rect, top: bool) {
    let insets = crate::shell::platform::safe_insets();
    let body = {
        let mut ui = overlay_ui(shell);
        let p = ui.p;
        ui.fill(cx, window, p.surface0, 0.0);
        if top {
            ui.hit(window, "dialog:surface");
        }
        let row_y = window.pos.y + insets.top + 8.0;
        let island = rect(window.pos.x + 12.0 + insets.left, row_y + 12.0, 32.0, 32.0);
        let id = if top { "sheet:close" } else { "" };
        if top && (ui.is_hover(id) || ui.is_pressed(id)) {
            ui.fill(cx, island, p.interaction_highlight, 16.0);
        }
        ui.icon(cx, island, "arrow-left", 16.0, p.muted);
        if top {
            ui.hit(rect(island.pos.x, row_y + 4.0, 32.0, 48.0), id);
        }
        let spec = ui.spec(Font::Medium, 16.0, p.foreground);
        let tx = island.pos.x + 32.0 + 8.0;
        ui.text(cx, rect(tx, row_y, window.pos.x + window.size.x - 12.0 - insets.right - tx, 56.0), spec, title);
        let top_y = row_y + 56.0;
        rect(window.pos.x + 24.0, top_y, window.size.x - 48.0, (window.pos.y + window.size.y - insets.bottom - 24.0 - top_y).max(0.0))
    };
    *viewport = body;
    sheet.set_body_height(body.size.y);
    cx.begin_turtle(Walk::abs_rect(body), Layout { clip_x: true, clip_y: true, ..Layout::default() });
    {
        let mut ui = overlay_ui(shell);
        ui.hits.set_clip(Some(body));
        sheet.draw(cx, &mut ui, scope, body.pos.x, body.pos.y, body.size.x, top);
        ui.hits.set_clip(None);
    }
    cx.end_turtle();
    if crate::shell::sheets::take_spinning() {
        shell.data.overlays.spin = cx.new_next_frame();
    }
}

fn draw_toast(shell: &mut PaseoShell, cx: &mut Cx2d, window: Rect) {
    let Some((message, error)) = shell.data.overlays.toast.as_ref().map(|t| (t.message.clone(), t.error)) else { return };
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 14.0, p.foreground);
    // Original toast-host pill: 92% wide up to 480, under the header, surface0 + border.
    let w = (window.size.x * 0.92).min(480.0);
    let text_w = w - 24.0 - 28.0;
    let h = ui.painter.paragraph_height(cx, text_w, spec, &message).max(20.0) + 20.0;
    let top = window.pos.y + if ui.compact { 72.0 } else { 44.0 };
    let r = rect(window.pos.x + (window.size.x - w) * 0.5, top, w, h);
    let border = if error { p.destructive } else { p.border };
    ui.painter.outline(cx, r, p.surface0, border, 1.0, 16.0);
    let icon = if error { "circle-alert" } else { "info" };
    ui.icon(cx, rect(r.pos.x + 12.0, r.pos.y + (h - 16.0) * 0.5, 16.0, 16.0), icon, 16.0, if error { p.destructive } else { p.muted });
    ui.painter.paragraph(cx, dvec2(r.pos.x + 36.0, r.pos.y + 10.0), text_w, spec, &message);
}

fn draw_tooltip(shell: &mut PaseoShell, cx: &mut Cx2d, window: Rect) {
    let Some(tip) = shell.data.overlays.tooltip.clone() else { return };
    let mut ui = overlay_ui(shell);
    let p = ui.p;
    let spec = ui.spec(Font::Regular, 12.0, p.surface0);
    let w = ui.measure(cx, spec, &tip.text) + 16.0;
    let h = 24.0;
    // Flutter `Tooltip`: centred on the target, `verticalOffset` 24 from its centre, below
    // unless that overflows the window, then above.
    let a = tip.anchor;
    let cx0 = a.pos.x + a.size.x * 0.5;
    let cy = a.pos.y + a.size.y * 0.5;
    let below = cy + 24.0;
    let y = if below + h > window.pos.y + window.size.y - 4.0 { cy - 24.0 - h } else { below };
    let x = (cx0 - w * 0.5).clamp(window.pos.x + 4.0, (window.pos.x + window.size.x - w - 4.0).max(window.pos.x + 4.0));
    let r = rect(x, y, w, h);
    ui.fill(cx, r, with_alpha(p.foreground, 0.9), 4.0);
    ui.text_centered(cx, r, spec, &tip.text);
}

// ---------------------------------------------------------------------------------------------
// Events

/// Returns true while a modal overlay owns input.
pub fn handle_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, scope: &mut Scope) -> bool {
    let area = shell.data.overlays.area;
    // The layer's capture instance can change between frames (window resize, a new draw
    // list); a lock taken on an older area would block everything, so release it.
    let stale = std::mem::replace(&mut shell.data.overlays.locked_area, area);
    if stale != area {
        cx.sweep_unlock(stale);
    }
    let explorer_open = shell.data.compact && shell.data.chrome.explorer_position > 0.0;
    if !shell.data.overlays.is_modal() && !explorer_open {
        // The last modal closed: release the layer's sweep lock, or every hover and click
        // below stays blocked.
        cx.sweep_unlock(area);
        return false;
    }
    // Escape / back dismisses the top overlay.
    let dismiss = matches!(event, Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, .. })) || event.back_pressed();
    if let Some(top) = shell.data.overlays.stack.len().checked_sub(1) {
        // Text fields and sheets receive events first (unlocked).
        cx.sweep_unlock(area);
        match &mut shell.data.overlays.stack[top] {
            Modal::Prompt { field, id, owner, .. } => {
                let field = field.clone();
                let (id, owner) = (*id, owner.clone());
                field.handle_event(cx, event, scope);
                if let Event::Actions(actions) = event
                    && let Some(text) = field.as_ptext_field().returned(actions)
                {
                    shell.data.overlays.stack.pop();
                    answer(shell, cx, scope, &owner, id, json!({ "text": text }));
                    cx.sweep_lock(area);
                    return true;
                }
            }
            Modal::Select(select) => {
                if let Some(search) = select.search.clone() {
                    search.handle_event(cx, event, scope);
                    if let Event::Actions(actions) = event
                        && let Some(text) = search.as_ptext_field().changed(actions)
                    {
                        select.query = text.trim().to_lowercase();
                        select.scroll = 0.0;
                        shell.redraw(cx);
                    }
                }
            }
            Modal::Sheet { .. } => {
                if let Some(flow) = sheet_event(shell, cx, event, scope, top) {
                    apply_flow(shell, cx, scope, top, flow);
                }
            }
            _ => {}
        }
        cx.sweep_lock(area);
        if dismiss {
            dismiss_top(shell, cx, scope);
            if let Event::BackPressed { handled } = event {
                handled.set(true);
            }
            return true;
        }
    } else if explorer_open && dismiss {
        shell.data.layout.show_compact_explorer(false);
        if let Event::BackPressed { handled } = event {
            handled.set(true);
        }
        shell.redraw(cx);
        return true;
    }
    // While a modal is open the layer holds the sweep lock on `area`, so pointer events only
    // reach it through its own sweep area. Resolve the hit once (moves mark the event handled)
    // and share it.
    let swept = event.hits_with_sweep_area(cx, area, area);
    if let Hit::FingerScroll(e) = &swept {
        scroll_top(shell, e.scroll.y);
        shell.redraw(cx);
    }
    // Compact draggable sheets already combine resizing and scrolling below.
    let draggable = shell.data.overlays.stack.last().is_some_and(|top| {
        matches!(top, Modal::Sheet { sheet, motion, .. }
            if shell.data.compact && matches!(sheet.style(), SheetStyle::Draggable { .. }) && motion.window_h > 0.0)
    });
    if !draggable {
        if let Some(dy) = shell.data.overlays.touch_scroll.handle_hit(&swept) {
            scroll_top(shell, dy);
            shell.redraw(cx);
        }
    } else {
        shell.data.overlays.touch_scroll.reset();
    }
    if drag_sheet(shell, cx, scope, &swept) {
        return true;
    }
    let (redraw, press) = {
        let overlays = &mut shell.data.overlays;
        let hits = overlays.hits.clone();
        overlays.pointer.handle_hit(cx, swept, &hits)
    };
    if redraw {
        shell.redraw(cx);
    }
    match press {
        Some(Press::Click(id, _)) => click(shell, cx, scope, &id),
        Some(Press::Secondary(..)) | None => {}
    }
    // While open, the explorer overlay passes events to its own roots via dispatch.
    shell.data.overlays.is_modal()
}

/// `DraggableScrollableSheet` gestures on the top compact draggable sheet: dragging up grows
/// the sheet to .9 then scrolls its list; dragging down scrolls back, then shrinks it. A
/// release snaps to .3/.6/.9, or dismisses below half the minimum. A drag past 8px cancels
/// the pending tap. Returns `true` when the sheet was dismissed.
fn drag_sheet(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, hit: &Hit) -> bool {
    let compact = shell.data.compact;
    let now = cx.seconds_since_app_start();
    let mut cancel_tap = false;
    let mut dismiss = false;
    {
        let Some(Modal::Sheet { sheet, motion, scroll, content_height, viewport, .. }) = shell.data.overlays.stack.last_mut() else { return false };
        if !compact || !matches!(sheet.style(), SheetStyle::Draggable { .. }) || motion.window_h <= 0.0 {
            return false;
        }
        match hit {
            Hit::FingerDown(fe) if motion.frame.contains(fe.abs) => {
                motion.drag = Some(fe.abs.y);
                motion.travel = 0.0;
                motion.start = None;
            }
            Hit::FingerMove(fe) => {
                let Some(last) = motion.drag else { return false };
                let dy = fe.abs.y - last;
                motion.drag = Some(fe.abs.y);
                motion.travel += dy.abs();
                cancel_tap = motion.travel > 8.0;
                let max_scroll = (*content_height + 4.0 + 32.0 - viewport.size.y).max(0.0);
                if dy < 0.0 {
                    let grow = (-dy / motion.window_h).min(DRAG_SNAPS[2] - motion.extent).max(0.0);
                    motion.extent += grow;
                    *scroll = (*scroll + (-dy - grow * motion.window_h)).min(max_scroll);
                } else {
                    let back = dy.min(*scroll);
                    *scroll -= back;
                    motion.extent = (motion.extent - (dy - back) / motion.window_h).max(0.0);
                }
            }
            Hit::FingerUp(_) => {
                if motion.drag.take().is_some() && motion.travel > 8.0 {
                    if motion.extent < DRAG_SNAPS[0] * 0.5 {
                        dismiss = true;
                    } else {
                        motion.animate(now, SheetMotion::snap(motion.extent), 0.2);
                    }
                }
            }
            _ => return false,
        }
    }
    if cancel_tap {
        shell.data.overlays.pointer.pressed = None;
    }
    if dismiss {
        dismiss_top(shell, cx, scope);
        return true;
    }
    shell.redraw(cx);
    false
}

fn scroll_top(shell: &mut PaseoShell, delta: f64) {
    if let Some(Modal::Sheet { scroll, .. }) = shell.data.overlays.stack.last_mut() {
        *scroll = (*scroll + delta).max(0.0);
    }
    if let Some(Modal::Menu(menu)) = shell.data.overlays.stack.last_mut() {
        menu.scroll = (menu.scroll + delta).max(0.0);
    }
    if let Some(Modal::Select(select)) = shell.data.overlays.stack.last_mut() {
        select.scroll = (select.scroll + delta).max(0.0);
    }
}

fn answer(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, owner: &str, id: u64, args: Value) {
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        shell.overlay_answer(cx, ui, owner, id, args);
    }
    shell.redraw(cx);
}

fn dismiss_top(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope) {
    let Some(modal) = shell.data.overlays.stack.pop() else { return };
    match modal {
        Modal::Menu(menu) => answer(shell, cx, scope, &menu.owner, menu.id, json!({ "cancelled": true })),
        Modal::Select(select) => answer(shell, cx, scope, &select.owner, select.id, json!({ "cancelled": true })),
        Modal::Confirm { id, owner, .. } => answer(shell, cx, scope, &owner, id, json!({ "confirmed": false })),
        Modal::Prompt { id, owner, .. } => answer(shell, cx, scope, &owner, id, json!({ "cancelled": true })),
        Modal::Sheet { mut sheet, .. } => {
            if let Some(ui) = scope.data.get_mut::<UiContext>() {
                let mut ops = SheetOps::new(ui);
                sheet.closed(cx, &mut ops);
                crate::shell::sheets::apply_ops(shell, cx, ui, ops, None);
            }
        }
    }
    shell.redraw(cx);
}

fn click(shell: &mut PaseoShell, cx: &mut Cx, scope: &mut Scope, id: &str) {
    let Some(top) = shell.data.overlays.stack.len().checked_sub(1) else {
        if id == "compact:backdrop" {
            shell.data.layout.show_compact_explorer(false);
            shell.redraw(cx);
        } else if let Some(ui) = scope.data.get_mut::<UiContext>() {
            crate::shell::commands::click(shell, cx, ui, id, KeyModifiers::default());
        }
        return;
    };
    if id == "overlay:backdrop" || id == "dialog:close" || id == "sheet:close" {
        dismiss_top(shell, cx, scope);
        return;
    }
    if id == "menu:surface" || id == "dialog:surface" || id == "select:surface" {
        return;
    }
    match &mut shell.data.overlays.stack[top] {
        Modal::Select(select) => {
            if let Some(index) = id.strip_prefix("select:item:").and_then(|i| i.parse::<usize>().ok())
                && let Some(option) = select.options.get(index).filter(|o| o.enabled)
            {
                let (owner, select_id, value) = (select.owner.clone(), select.id, option.value.clone());
                shell.data.overlays.stack.pop();
                answer(shell, cx, scope, &owner, select_id, json!({ "value": value }));
            }
        }
        Modal::Menu(menu) => {
            if id == "menu:back" {
                if menu.pages.len() > 1 {
                    menu.pages.pop();
                    menu.scroll = 0.0;
                }
                shell.redraw(cx);
                return;
            }
            let entries = menu.pages.last().map(|(_, e)| e.clone()).unwrap_or_default();
            if let Some(index) = id.strip_prefix("menu:sub:").and_then(|i| i.parse::<usize>().ok()) {
                if let Some(MenuEntry::Submenu { label, entries, .. }) = entries.get(index) {
                    menu.pages.push((Some(label.clone()), entries.clone()));
                    menu.scroll = 0.0;
                }
                shell.redraw(cx);
                return;
            }
            if let Some(index) = id.strip_prefix("menu:item:").and_then(|i| i.parse::<usize>().ok())
                && let Some(MenuEntry::Item(item)) = entries.get(index)
            {
                let (owner, menu_id) = (menu.owner.clone(), menu.id);
                let value = item.value.clone();
                if !item.keep_open {
                    shell.data.overlays.stack.pop();
                }
                answer(shell, cx, scope, &owner, menu_id, json!({ "value": value }));
            }
        }
        Modal::Confirm { id: cid, owner, .. } => {
            let (cid, owner) = (*cid, owner.clone());
            let confirmed = match id {
                "confirm:ok" => true,
                "confirm:cancel" => false,
                _ => return,
            };
            shell.data.overlays.stack.pop();
            answer(shell, cx, scope, &owner, cid, json!({ "confirmed": confirmed }));
        }
        Modal::Prompt { id: pid, owner, field, .. } => {
            let (pid, owner, text) = (*pid, owner.clone(), field.text());
            match id {
                "prompt:ok" => {
                    shell.data.overlays.stack.pop();
                    answer(shell, cx, scope, &owner, pid, json!({ "text": text }));
                }
                "prompt:cancel" => {
                    shell.data.overlays.stack.pop();
                    answer(shell, cx, scope, &owner, pid, json!({ "cancelled": true }));
                }
                _ => {}
            }
        }
        Modal::Sheet { sheet, id: sheet_id, .. } => {
            let sheet_id = *sheet_id;
            let Some(ui) = scope.data.get_mut::<UiContext>() else { return };
            let mut ops = SheetOps::for_sheet(ui, sheet_id);
            let flow = sheet.click(cx, id, &mut ops);
            crate::shell::sheets::apply_ops(shell, cx, ui, ops, Some(sheet_id));
            apply_flow(shell, cx, scope, top, flow);
        }
    }
    shell.redraw(cx);
}

fn sheet_event(shell: &mut PaseoShell, cx: &mut Cx, event: &Event, scope: &mut Scope, index: usize) -> Option<SheetFlow> {
    let Modal::Sheet { sheet, id, .. } = &mut shell.data.overlays.stack[index] else { return None };
    let sheet_id = *id;
    let flow = sheet.handle_event(cx, event, scope);
    let (ops, after) = {
        let ui = scope.data.get_mut::<UiContext>()?;
        let mut ops = SheetOps::for_sheet(ui, sheet_id);
        let after = sheet.after_event(cx, &mut ops);
        (ops, after)
    };
    if let Some(ui) = scope.data.get_mut::<UiContext>() {
        crate::shell::sheets::apply_ops(shell, cx, ui, ops, Some(sheet_id));
    }
    shell.redraw(cx);
    Some(if matches!(flow, SheetFlow::Stay) { after } else { flow })
}

/// Applies a sheet's navigation result to the overlay stack.
pub fn apply_flow(shell: &mut PaseoShell, cx: &mut Cx, _scope: &mut Scope, index: usize, flow: SheetFlow) {
    match flow {
        SheetFlow::Stay => {}
        SheetFlow::Close => {
            if index < shell.data.overlays.stack.len() {
                shell.data.overlays.stack.remove(index);
            }
        }
        SheetFlow::Replace(next) => {
            if index < shell.data.overlays.stack.len() {
                shell.data.overlays.stack.remove(index);
            }
            shell.data.overlays.open_sheet(next);
        }
        SheetFlow::Push(next) => {
            shell.data.overlays.open_sheet(next);
        }
    }
    shell.redraw(cx);
}

/// Margin helper for sheets.
pub fn padded(r: Rect, d: f64) -> Rect {
    inset(r, d, d)
}

/// Text spec helper for sheet bodies.
pub fn body_spec(ui: &Ui) -> TextSpec {
    ui.spec(Font::Regular, 14.0, ui.p.foreground)
}
