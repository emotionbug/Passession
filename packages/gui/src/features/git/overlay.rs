//! Git overlays. Menus, confirmations and searchable selects are the shell's
//! shared overlays (Flutter `showPaseoMenu`, `confirmAction`,
//! `showPaseoSelect`) under a per-host widget owner; their answers come back
//! through [`OverlayHost::poll`]. The two `showAdaptiveSheet` contents of the
//! git feature — the review comment sheet (`_ReviewCommentSheet`) and the
//! compact jump-to-file tree (`_jumpToFile`) — are drawn here with the
//! original `AdaptiveModalSheet` + `SheetContents` chrome.

use super::chrome::{ButtonSize, ButtonVariant, TREE_ROW_HEIGHT, button, button_height, file_tree_row, measure_button, palette, ui};
use super::json::text;
use super::model::{TreeRow, changed_tree_rows};
use super::paint::{Font, GitPaint, Hits, rect, with_alpha};
use super::text::t;
use crate::common::UiContext;
use crate::common::overlay as shared;
use crate::theme::DrawBox;
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub value: String,
    pub label: String,
    pub icon: Option<&'static str>,
    pub svg: Option<String>,
    pub hint: Option<String>,
    pub selected: bool,
    pub enabled: bool,
    /// Unavailable actions render muted but stay pressable (they explain themselves).
    pub muted: bool,
    pub destructive: bool,
}

impl MenuItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            icon: None,
            svg: None,
            hint: None,
            selected: false,
            enabled: true,
            muted: false,
            destructive: false,
        }
    }

    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }

    pub fn svg(mut self, svg: String) -> Self {
        self.svg = Some(svg);
        self
    }

    pub fn hint(mut self, hint: Option<String>) -> Self {
        self.hint = hint;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    fn shared(self) -> shared::MenuItem {
        let mut item = shared::MenuItem::new(self.value, self.label)
            .enabled(self.enabled)
            .muted(self.muted)
            .destructive(self.destructive)
            .checked(self.selected);
        if let Some(icon) = self.icon {
            item = item.icon(icon);
        }
        if let Some(svg) = self.svg {
            item = item.svg(svg);
        }
        if let Some(hint) = self.hint {
            item = item.hint(hint);
        }
        item
    }
}

#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

impl MenuEntry {
    fn shared(self) -> shared::MenuEntry {
        match self {
            MenuEntry::Item(item) => shared::MenuEntry::Item(item.shared()),
            MenuEntry::Separator => shared::MenuEntry::Separator,
        }
    }
}

/// A shared overlay request.
#[derive(Clone, Debug)]
pub enum OverlayKind {
    /// `showPaseoMenu`.
    Menu {
        title: Option<String>,
        anchor: Rect,
        align_end: bool,
        width: Option<f64>,
        entries: Vec<MenuEntry>,
    },
    /// `confirmAction`.
    Confirm {
        title: String,
        message: String,
        confirm: String,
        destructive: bool,
    },
    /// `showPaseoSelect` with its search row (branch switcher).
    Picker {
        title: String,
        anchor: Rect,
        min_width: f64,
        icon: Option<&'static str>,
        options: Vec<(String, String)>,
        selected: Option<String>,
        placeholder: String,
        empty: String,
    },
}

/// A `showAdaptiveSheet` content drawn by the git feature.
#[derive(Clone, Debug)]
pub enum SheetRequest {
    /// `_ReviewCommentSheet`: a 5–12 line field; Save disabled while blank.
    Comment {
        title: String,
        subtitle: String,
        placeholder: String,
        initial: String,
    },
    /// `_jumpToFile`: a transient, fully expanded changed-files tree sheet.
    FileTree {
        title: String,
        files: Vec<Value>,
        selected: Option<String>,
    },
}

pub struct Overlay<P> {
    pub purpose: P,
    pub kind: OverlayKind,
}

#[derive(Debug)]
pub enum OverlayResult<P> {
    Chose(P, String),
    Confirmed(P),
    Text(P, String),
    Dismissed(P),
}

#[derive(Clone, Debug, PartialEq)]
enum SheetHit {
    Backdrop,
    Card,
    Close,
    Field,
    Row(usize),
    Confirm,
    Cancel,
}

enum SheetKind {
    Comment { title: String, subtitle: String },
    Tree { title: String, files: Vec<Value>, selected: Option<String>, collapsed: HashSet<String> },
}

struct Sheet<P> {
    purpose: P,
    kind: SheetKind,
}

/// Material's modal bottom sheet barrier (`Colors.black54`).
const SHEET_BARRIER: f32 = 0.54;
/// `showAdaptiveSheet` dialog barrier (`0x8c000000`).
const DIALOG_BARRIER: f32 = 140.0 / 255.0;
/// Material 3 `showDragHandle` area (`kMinInteractiveDimension`).
const DRAG_HANDLE_AREA: f64 = 48.0;

static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

/// A shared overlay request waiting for a `UiContext` (returns its id).
type SharedRequest = Box<dyn FnOnce(&mut UiContext) -> u64>;

/// The overlay a git widget waits on or shows.
pub struct OverlayHost<P> {
    /// Widget owner of the shared overlays this host opens.
    owner: String,
    /// A shared overlay opened during event handling, sent by [`flush`](Self::flush).
    queued: Option<(P, SharedRequest)>,
    /// The shared overlay awaiting its answer.
    waiting: Option<(u64, P)>,
    sheet: Option<Sheet<P>>,
    hits: Hits<SheetHit>,
    hovered: Option<SheetHit>,
    pressed: Option<SheetHit>,
    text: String,
    focus_pending: bool,
    scroll: f64,
    list_rect: Rect,
    content_height: f64,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    /// The backdrop area the sheet's sweep lock was taken with.
    locked: Area,
    window_modal: Option<shared::WindowModal>,
}

impl<P> Default for OverlayHost<P> {
    fn default() -> Self {
        let id = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
        Self {
            owner: format!("{}git-{id}", shared::WIDGET_OWNER_PREFIX),
            queued: None,
            waiting: None,
            sheet: None,
            hits: Hits::default(),
            hovered: None,
            pressed: None,
            text: String::new(),
            focus_pending: false,
            scroll: 0.0,
            list_rect: Rect::default(),
            content_height: 0.0,
            touch_scroll: Default::default(),
            locked: Area::Empty,
            window_modal: None,
        }
    }
}

impl<P> OverlayHost<P> {
    /// A shared overlay is queued or pending, or a sheet is shown.
    pub fn is_open(&self) -> bool {
        self.queued.is_some() || self.waiting.is_some() || self.sheet.is_some()
    }

    /// A sheet drawn by this host takes the pointer.
    pub fn blocks_input(&self) -> bool {
        self.sheet.is_some()
    }

    /// What the open overlay was opened for.
    pub fn purpose(&self) -> Option<&P> {
        self.queued
            .as_ref()
            .map(|(purpose, _)| purpose)
            .or_else(|| self.waiting.as_ref().map(|(_, purpose)| purpose))
            .or_else(|| self.sheet.as_ref().map(|sheet| &sheet.purpose))
    }

    /// Queues a shared overlay; it goes to the shell at the next [`flush`](Self::flush).
    pub fn open(&mut self, overlay: Overlay<P>) {
        let Overlay { purpose, kind } = overlay;
        self.waiting = None;
        let request: SharedRequest = match kind {
            OverlayKind::Menu { title, anchor, align_end, width, entries } => {
                let entries = entries.into_iter().map(MenuEntry::shared).collect();
                let options = shared::MenuOptions { width: width.unwrap_or(220.0), align_end, sheet: true };
                Box::new(move |ui| shared::open_menu_with(ui, anchor, title, entries, options))
            }
            OverlayKind::Confirm { title, message, confirm, destructive } => {
                Box::new(move |ui| shared::confirm(ui, &title, &message, &confirm, destructive))
            }
            OverlayKind::Picker { title, anchor, min_width, icon, options, selected, placeholder, empty } => {
                let options = options
                    .into_iter()
                    .map(|(value, label)| {
                        let option = shared::SelectOption::new(value, label);
                        match icon {
                            Some(icon) => option.icon(icon),
                            None => option,
                        }
                    })
                    .collect();
                let mut request = shared::SelectRequest::new(title, options).searchable(Some(placeholder)).empty_text(empty).min_width(min_width);
                if let Some(selected) = selected {
                    request = request.value(selected);
                }
                Box::new(move |ui| shared::open_select(ui, anchor, request))
            }
        };
        self.queued = Some((purpose, request));
    }

    /// Shows a sheet at the next draw.
    pub fn open_sheet(&mut self, cx: &mut Cx, purpose: P, request: SheetRequest, input: &mut TextInput) {
        self.sheet = None;
        self.hovered = None;
        self.pressed = None;
        self.scroll = 0.0;
        let kind = match request {
            SheetRequest::Comment { title, subtitle, placeholder, initial } => {
                // The field's DSL style is fixed at load; palette and type size are runtime.
                let colors = palette();
                let (foreground, muted) = (colors.foreground, colors.muted);
                let size = crate::theme::Typography::pt(ui(14.0));
                input.apply_over(
                    cx,
                    live! {
                        draw_text: { color: (foreground), color_empty: (muted), text_style: { font_size: (size) } }
                    },
                );
                input.set_text(cx, &initial);
                input.set_empty_text(cx, placeholder);
                self.text = initial;
                self.focus_pending = true;
                SheetKind::Comment { title, subtitle }
            }
            SheetRequest::FileTree { title, files, selected } => SheetKind::Tree { title, files, selected, collapsed: HashSet::new() },
        };
        self.sheet = Some(Sheet { purpose, kind });
        cx.redraw_all();
    }

    /// Sends a queued shared overlay to the shell under this host's owner.
    pub fn flush(&mut self, ui: &mut UiContext) {
        let owner = self as *const Self as u64;
        shared::WindowModal::sync(&mut self.window_modal, ui, owner, self.sheet.is_some());
        let Some((purpose, request)) = self.queued.take() else { return };
        let owner = self.owner.clone();
        let id = shared::with_owner(ui, &owner, request);
        self.waiting = Some((id, purpose));
    }

    /// Drops the overlay without a result (the owner hid or rebound). A
    /// pending shared overlay's late answer is ignored.
    pub fn close(&mut self, cx: &mut Cx) -> Option<P> {
        self.touch_scroll.reset();
        if let Some((purpose, _)) = self.queued.take() {
            return Some(purpose);
        }
        if let Some((_, purpose)) = self.waiting.take() {
            return Some(purpose);
        }
        let sheet = self.sheet.take()?;
        self.unlock(cx);
        cx.redraw_all();
        Some(sheet.purpose)
    }

    /// Releases the sheet's sweep lock. Makepad keeps the first lock until
    /// exactly that `Area` unlocks it, and an area changes identity on every
    /// redraw: the lock must be released through the area it was taken with.
    fn unlock(&mut self, cx: &mut Cx) {
        self.window_modal = None;
        cx.sweep_unlock(self.locked);
        self.locked = Area::Empty;
    }

    /// The answer of the pending shared overlay, once the shell delivered it.
    pub fn poll(&mut self) -> Option<OverlayResult<P>> {
        let answers = shared::take_answers(&self.owner);
        let id = self.waiting.as_ref()?.0;
        let (_, args) = answers.into_iter().find(|(answer, _)| *answer == id)?;
        let (_, purpose) = self.waiting.take()?;
        Some(if let Some(value) = args.get("value").and_then(Value::as_str) {
            OverlayResult::Chose(purpose, value.to_owned())
        } else if args.get("confirmed") == Some(&Value::Bool(true)) {
            OverlayResult::Confirmed(purpose)
        } else {
            OverlayResult::Dismissed(purpose)
        })
    }

    fn finish(&mut self, cx: &mut Cx, make: impl FnOnce(P) -> OverlayResult<P>) -> Option<OverlayResult<P>> {
        self.touch_scroll.reset();
        let sheet = self.sheet.take()?;
        self.unlock(cx);
        cx.redraw_all();
        Some(make(sheet.purpose))
    }

    /// Routes an event: the shared overlay's answer, else the sheet's input.
    pub fn handle(&mut self, cx: &mut Cx, event: &Event, input: &mut TextInput) -> Option<OverlayResult<P>> {
        if let Some(result) = self.poll() {
            return Some(result);
        }
        let area = self.locked;
        let is_comment = matches!(self.sheet.as_ref()?.kind, SheetKind::Comment { .. });
        if is_comment {
            cx.sweep_unlock(area);
            let actions = cx.capture_actions(|cx| input.handle_event(cx, event, &mut Scope::empty()));
            cx.sweep_lock(area);
            for action in &actions {
                let Some(action) = action.as_widget_action() else { continue };
                match action.action.downcast_ref::<TextInputAction>() {
                    Some(TextInputAction::Changed(text)) => {
                        self.text = text.clone();
                        cx.redraw_all();
                    }
                    Some(TextInputAction::Escaped) => {
                        return self.finish(cx, OverlayResult::Dismissed);
                    }
                    // Flutter's multiline TextField: Enter inserts a line break
                    // (makepad only does that for Shift+Enter).
                    Some(TextInputAction::Returned(text, _)) => {
                        let selection = input.selection();
                        let start = selection.start().index.min(text.len());
                        let end = selection.end().index.min(text.len());
                        if text.is_char_boundary(start) && text.is_char_boundary(end) {
                            let next = format!("{}\n{}", &text[..start], &text[end..]);
                            input.set_text(cx, &next);
                            let cursor = makepad_widgets::makepad_draw::text::selection::Cursor { index: start + 1, prefer_next_row: false };
                            input.set_cursor(cx, cursor, false);
                            self.text = next;
                            cx.redraw_all();
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Event::KeyDown(key) = event
            && key.key_code == KeyCode::Escape
        {
            return self.finish(cx, OverlayResult::Dismissed);
        }
        if event.back_pressed() {
            return self.finish(cx, OverlayResult::Dismissed);
        }
        let hit = event.hits_with_sweep_area(cx, area, area);
        if !area.is_valid(cx) {
            self.touch_scroll.reset();
        }
        if let Some(delta) = self.touch_scroll.handle_event(cx, event, &hit) {
            self.pressed = None;
            self.scroll_list(cx, self.touch_scroll.origin(), delta);
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hovered = self.hits.find(e.abs).map(|(_, hit)| hit);
                if hovered != self.hovered {
                    self.hovered = hovered;
                    cx.redraw_all();
                }
            }
            Hit::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    cx.redraw_all();
                }
            }
            Hit::FingerScroll(e) => self.scroll_list(cx, e.abs, e.scroll.y),
            Hit::FingerDown(e) => {
                self.pressed = self.hits.find(e.abs).map(|(_, hit)| hit);
            }
            Hit::FingerUp(e) => {
                let pressed = self.pressed.take();
                let released = self.hits.find(e.abs).map(|(_, hit)| hit);
                if pressed != released || !e.is_over || (e.device.is_touch() && !e.was_tap()) {
                    return None;
                }
                match released {
                    Some(SheetHit::Backdrop) | Some(SheetHit::Cancel) | Some(SheetHit::Close) => {
                        return self.finish(cx, OverlayResult::Dismissed);
                    }
                    Some(SheetHit::Confirm) => {
                        if self.text.trim().is_empty() {
                            return None;
                        }
                        let text = self.text.clone();
                        return self.finish(cx, |purpose| OverlayResult::Text(purpose, text));
                    }
                    Some(SheetHit::Row(index)) => return self.tap_row(cx, index),
                    Some(SheetHit::Card) | Some(SheetHit::Field) | None => {}
                }
            }
            _ => {}
        }
        None
    }

    fn scroll_list(&mut self, cx: &mut Cx, point: DVec2, delta: f64) {
        if self.list_rect.contains(point) {
            let max = (self.content_height - self.list_rect.size.y).max(0.0);
            self.touch_scroll.apply_delta(&mut self.scroll, delta, max);
            cx.redraw_all();
        } else {
            self.touch_scroll.reset();
        }
    }

    /// A folder row toggles (the transient collapsed set); a file row answers its path.
    fn tap_row(&mut self, cx: &mut Cx, index: usize) -> Option<OverlayResult<P>> {
        let Some(Sheet { kind: SheetKind::Tree { files, collapsed, .. }, .. }) = self.sheet.as_mut() else { return None };
        match changed_tree_rows(files, collapsed).get(index)? {
            TreeRow::Folder { path, .. } => {
                if !collapsed.remove(path) {
                    collapsed.insert(path.clone());
                }
                cx.redraw_all();
                None
            }
            TreeRow::File { index, .. } => {
                let path = text(&files[*index], "path").to_owned();
                self.finish(cx, |purpose| OverlayResult::Chose(purpose, path))
            }
        }
    }

    /// Draws the open sheet into `list` above everything else in the window.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cx: &mut Cx2d, list: &mut DrawList2d, backdrop: &mut DrawBox, paint: &mut GitPaint, input: &mut TextInput, compact: bool) -> bool {
        let Some(sheet) = &self.sheet else {
            return false;
        };
        self.hits.clear();
        list.begin_overlay_reuse(cx);
        let window = cx.current_pass_size();
        // Pass-sized: a parent turtle's alignment (shell header) never shifts the overlay.
        cx.begin_sized_turtle(window, Layout::default());
        let scrim = if compact { SHEET_BARRIER } else { DIALOG_BARRIER };
        backdrop.paint(cx, rect(0.0, 0.0, window.x, window.y), vec4(0.0, 0.0, 0.0, scrim), 0.0);
        // Re-take the lock with this frame's area (see `unlock`).
        let area = backdrop.area();
        if self.locked != area {
            cx.sweep_unlock(self.locked);
            cx.sweep_lock(area);
            self.locked = area;
        }
        self.hits.push(rect(0.0, 0.0, window.x, window.y), SheetHit::Backdrop);
        let icons_pending = match &sheet.kind {
            SheetKind::Comment { title, subtitle } => {
                let (title, subtitle) = (title.clone(), subtitle.clone());
                self.draw_comment(cx, paint, window, compact, &title, &subtitle, input);
                false
            }
            SheetKind::Tree { .. } => self.draw_tree(cx, paint, window, compact),
        };
        cx.end_pass_sized_turtle();
        list.end(cx);
        icons_pending
    }

    /// Original `AdaptiveModalSheet`: a surface0 bottom sheet with a 16px top
    /// radius and a drag handle on compact layouts, a centred 520px surface1
    /// card (12px radius, surface2 border, 85% max height) on wider ones.
    /// `content` is the `SheetContents` height; returns the card and the top
    /// of its contents.
    fn sheet_frame(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, window: DVec2, compact: bool, content: f64) -> (Rect, f64) {
        let colors = palette();
        if compact {
            let height = (DRAG_HANDLE_AREA + content).min(window.y);
            // The extra 16px hides the bottom corners below the window edge.
            let card = rect(0.0, window.y - height, window.x, height + 16.0);
            paint.rounded(cx, card, colors.surface0, 16.0, None);
            // Material 3 drag handle: 32×4, onSurfaceVariant (muted) at 40%.
            let handle = rect(window.x / 2.0 - 16.0, card.pos.y + 22.0, 32.0, 4.0);
            paint.rounded(cx, handle, with_alpha(colors.muted, 0.4), 2.0, None);
            self.hits.push(card, SheetHit::Card);
            (card, card.pos.y + DRAG_HANDLE_AREA)
        } else {
            let width = (window.x - 48.0).min(520.0);
            let height = content.min(window.y * 0.85).min(window.y - 48.0);
            let card = rect((window.x - width) / 2.0, (window.y - height) / 2.0, width, height);
            paint.rounded(cx, card, colors.surface1, 12.0, Some((colors.surface2, 1.0)));
            self.hits.push(card, SheetHit::Card);
            (card, card.pos.y)
        }
    }

    /// Height of the `SheetContents` header: 16px insets around the title
    /// (and subtitle) column, at least the 32px close glyph.
    fn header_height(cx: &mut Cx2d, paint: &mut GitPaint, subtitle: bool) -> f64 {
        let title = paint.measure(cx, Font::Medium, ui(14.0), "Ag").y;
        let column = if subtitle { title + 4.0 + paint.measure(cx, Font::Regular, ui(12.0), "Ag").y } else { title };
        16.0 + column.max(32.0) + 16.0 + 1.0
    }

    /// `SheetContents` header: title (base size, medium) and muted subtitle,
    /// the close glyph, and a surface2 rule beneath.
    fn draw_header(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, card: Rect, top: f64, title: &str, subtitle: Option<&str>) -> f64 {
        let colors = palette();
        let height = Self::header_height(cx, paint, subtitle.is_some());
        let row = rect(card.pos.x + 24.0, top + 16.0, card.size.x - 24.0 - 16.0, height - 33.0);
        let close = rect(row.pos.x + row.size.x - 32.0, row.pos.y + (row.size.y - 32.0) / 2.0, 32.0, 32.0);
        let text_width = (close.pos.x - 8.0 - row.pos.x).max(0.0);
        let title_height = paint.measure(cx, Font::Medium, ui(14.0), "Ag").y;
        let column = if subtitle.is_some() { title_height + 4.0 + paint.measure(cx, Font::Regular, ui(12.0), "Ag").y } else { title_height };
        let mut y = row.pos.y + (row.size.y - column) / 2.0;
        paint.text_fit(cx, Font::Medium, ui(14.0), colors.foreground, dvec2(row.pos.x, y), title, text_width);
        if let Some(subtitle) = subtitle {
            y += title_height + 4.0;
            paint.text_fit(cx, Font::Regular, ui(12.0), colors.muted, dvec2(row.pos.x, y), subtitle, text_width);
        }
        if self.hovered == Some(SheetHit::Close) {
            paint.rounded(cx, close, colors.interaction_highlight, 8.0, None);
        }
        paint.icon(cx, "x", 16.0, colors.muted, close.center());
        self.hits.push(close, SheetHit::Close);
        paint.fill(cx, rect(card.pos.x, top + height - 1.0, card.size.x, 1.0), colors.surface2);
        height
    }

    /// Original `_ReviewCommentSheet` in `SheetContents`: the 5–12 line field
    /// (ui 14 at 1.4) in 24px body insets, then Cancel and the primary Save.
    #[allow(clippy::too_many_arguments)]
    fn draw_comment(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, window: DVec2, compact: bool, title: &str, subtitle: &str, input: &mut TextInput) {
        let colors = palette();
        let line = (ui(14.0) * 1.4).round();
        let buttons = button_height(ButtonSize::Md, compact);
        let header = Self::header_height(cx, paint, true);
        let footer = 1.0 + 12.0 + buttons + 12.0;
        // Width first (the field wraps), then the line count it needs.
        let width = if compact { window.x } else { (window.x - 48.0).min(520.0) };
        let field_width = width - 48.0;
        let lines = (paint.measure_wrapped(cx, Font::Regular, ui(14.0), &self.text, field_width - 32.0) / line).ceil().clamp(5.0, 12.0);
        let field_height = lines * line + 24.0;
        let content = header + 24.0 + field_height + 24.0 + footer;
        let (card, top) = self.sheet_frame(cx, paint, window, compact, content);
        let header = self.draw_header(cx, paint, card, top, title, Some(subtitle));
        let field = rect(card.pos.x + 24.0, top + header + 24.0, field_width, field_height);
        // paseoInputDecoration: surface2 fill, 8px radius, borderAccent while hovered or focused.
        let focused = cx.has_key_focus(input.area());
        let border = (focused || self.hovered == Some(SheetHit::Field)).then_some((colors.border_accent, 1.0));
        paint.rounded(cx, field, colors.surface2, 8.0, border);
        self.hits.push(field, SheetHit::Field);
        let walk = Walk {
            abs_pos: Some(field.pos + dvec2(16.0, 12.0)),
            margin: Margin::default(),
            width: Size::Fixed(field.size.x - 32.0),
            height: Size::Fixed(field.size.y - 24.0),
        };
        let _ = input.draw_walk(cx, &mut Scope::empty(), walk);
        if std::mem::take(&mut self.focus_pending) {
            cx.set_key_focus(input.area());
        }
        let footer_top = field.pos.y + field.size.y + 24.0;
        paint.fill(cx, rect(card.pos.x, footer_top, card.size.x, 1.0), colors.surface2);
        let y = footer_top + 1.0 + 12.0;
        let cancel = t("review.comment.cancel");
        let save = t("review.comment.save");
        let save_width = measure_button(cx, paint, &save, false, ButtonSize::Md, compact);
        let cancel_width = measure_button(cx, paint, &cancel, false, ButtonSize::Md, compact);
        let right = card.pos.x + card.size.x - 24.0;
        let save_rect = rect(right - save_width, y, save_width, buttons);
        let cancel_rect = rect(save_rect.pos.x - 8.0 - cancel_width, y, cancel_width, buttons);
        let enabled = !self.text.trim().is_empty();
        button(cx, paint, cancel_rect, &cancel, None, ButtonVariant::Secondary, ButtonSize::Md, compact, true, self.hovered == Some(SheetHit::Cancel));
        button(cx, paint, save_rect, &save, None, ButtonVariant::Primary, ButtonSize::Md, compact, enabled, self.hovered == Some(SheetHit::Confirm));
        self.hits.push(cancel_rect, SheetHit::Cancel);
        if enabled {
            self.hits.push(save_rect, SheetHit::Confirm);
        }
    }

    /// Original `_jumpToFile`: `SheetContents(scroll: false)` around a list 60%
    /// of the window tall holding the changed-files tree, padded 12/8/24/32.
    /// Returns true while a file icon is still decoding.
    fn draw_tree(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, window: DVec2, compact: bool) -> bool {
        let Some(Sheet { kind: SheetKind::Tree { title, files, selected, collapsed }, .. }) = &self.sheet else { return false };
        let (title, files, selected, collapsed) = (title.clone(), files.clone(), selected.clone(), collapsed.clone());
        let header = Self::header_height(cx, paint, false);
        let list_height = (window.y * 0.6).round();
        let (card, top) = self.sheet_frame(cx, paint, window, compact, header + list_height);
        let header = self.draw_header(cx, paint, card, top, &title, None);
        let list = rect(card.pos.x, top + header, card.size.x, list_height.min(card.pos.y + card.size.y - top - header));
        let rows = changed_tree_rows(&files, &collapsed);
        self.list_rect = list;
        let row_height = TREE_ROW_HEIGHT.max(paint.measure(cx, Font::Regular, ui(14.0), "Ag").y + 8.0);
        self.content_height = 8.0 + rows.len() as f64 * row_height + 32.0;
        self.scroll = self.scroll.clamp(0.0, (self.content_height - list.size.y).max(0.0));
        paint.begin_clip(cx, list);
        let mut pending = false;
        for (index, row) in rows.iter().enumerate() {
            let band = rect(list.pos.x + 12.0, list.pos.y + 8.0 + index as f64 * row_height - self.scroll, list.size.x - 12.0 - 24.0, row_height);
            if band.pos.y + band.size.y < list.pos.y || band.pos.y > list.pos.y + list.size.y {
                continue;
            }
            let path = match row {
                TreeRow::Folder { path, .. } => path.as_str(),
                TreeRow::File { index, .. } => text(&files[*index], "path"),
            };
            let hit = SheetHit::Row(index);
            let hovered = self.hovered.as_ref() == Some(&hit);
            if !file_tree_row(cx, paint, band, row, &files, &collapsed, hovered, selected.as_deref() == Some(path)) {
                pending = true;
            }
            self.hits.push_clipped(band, list, hit);
        }
        paint.end_clip(cx);
        pending
    }
}
