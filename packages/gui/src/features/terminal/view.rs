//! `WaTerminalView`: the interactive terminal surface (original `TerminalEmulator` of
//! `terminal-pane.tsx`): the shared cell grid sized to the viewport, the cursor, scrollback
//! with follow-bottom on output and input, selection with copy, and keyboard, paste and
//! input-method input written to the daemon terminal.
//!
//! The grid size fits the viewport; changes are debounced 100ms, then the local emulator
//! reflows and the size is sent: the focused pane `claim`s the PTY size, other visible
//! panes send `update`, which applies only while they own it (`terminal-pane-focus-claim.ts`).

use makepad_widgets::*;
use paseo_gui_core::CoreHandle;
use paseo_terminal::{CellStyle, CursorShape, Key, Line, Modifiers};

use super::grid::{grapheme_column, selected_text, CellGrid, CellPos, GridPaint, GridSelection};
use super::input::{self, ImeBridge, KeyCommand, ReplaceTracker, IME_SENTINEL};
use super::stream::Stream;
use crate::common::{UiContext, UiEffect};
use crate::theme;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::features::terminal::grid::*;

    pub WaTerminalView = {{WaTerminalView}} {
        width: Fill, height: Fill
        scroll_bars: <ScrollBars> { show_scroll_x: true, show_scroll_y: true }
        grid: <WaCellGrid> {}
        draw_hit: { color: #0000 }
    }
}

const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));
const APPLE: bool = cfg!(target_vendor = "apple");
const ANDROID: bool = cfg!(target_os = "android");
/// `TERMINAL_RESIZE_DEBOUNCE_MS`.
const RESIZE_DEBOUNCE: f64 = 0.1;

#[derive(Clone, Debug, DefaultNone)]
pub enum WaTerminalViewAction {
    None,
    /// Sticky key-bar modifiers were consumed by typed input.
    ModifiersConsumed,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaTerminalView {
    #[redraw]
    #[live]
    scroll_bars: ScrollBars,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[live]
    grid: CellGrid,
    #[live]
    draw_hit: DrawColor,
    #[rust]
    pub session: Option<Stream>,
    /// The host connection (set by the panel).
    #[rust]
    pub connected: bool,
    #[rust]
    pub follow: bool,
    #[rust(GridSelection::new(cx))]
    selection: GridSelection,
    #[rust]
    hit_origin: DVec2,
    #[rust]
    viewport: DVec2,
    #[rust]
    max_scroll: f64,
    /// The pane is focused: keyboard focus and the size claim.
    #[rust]
    claim: bool,
    #[rust]
    focus_requested: bool,
    #[rust]
    has_focus: bool,
    /// The soft keyboard was asked for (tap, key-bar toggle); mobile only opens it then.
    #[rust]
    show_ime: bool,
    #[rust(LiveId::unique().0)]
    ime_id: u64,
    #[rust]
    ime_caret: Area,
    #[rust]
    ime: ImeBridge,
    /// The Android editor's text and UTF-8 selection as last reported or published.
    #[rust(IME_SENTINEL.to_owned())]
    ime_text: String,
    #[rust((IME_SENTINEL.len(), IME_SENTINEL.len()))]
    ime_selection: (usize, usize),
    /// Republish the editor as [`IME_SENTINEL`] in a new input session on the next draw.
    #[rust]
    ime_reset: bool,
    #[rust]
    replaced: ReplaceTracker,
    /// The `TextInput` of an encoded Ctrl/Alt key press is not sent again.
    #[rust]
    suppress_text: bool,
    /// Sticky key-bar modifiers for the next key or character.
    #[rust]
    sticky: Modifiers,
    /// The grid size the viewport fits.
    #[rust]
    fitted: Option<(u16, u16)>,
    /// The last size sent: rows, cols, claim.
    #[rust]
    sent: Option<(u16, u16, bool)>,
    #[rust]
    resize_timer: Timer,
}

fn core_of(scope: &mut Scope) -> Option<CoreHandle> {
    scope.data.get_mut::<UiContext>().map(|ui| ui.core.clone())
}

/// The grapheme, style and cell width at `col` of a presented row.
fn cell_at(line: &Line, col: usize) -> Option<(String, CellStyle, usize)> {
    let mut start = 0;
    let mut first_char = 0;
    for run in &line.runs {
        let count = run.text.chars().count();
        let columns = line.columns.get(first_char..first_char + count).unwrap_or(&[]);
        if col < start + run.cells {
            let text = run.text.chars().zip(columns).filter(|(_, c)| **c == col).map(|(ch, _)| ch).collect();
            let next = columns.iter().copied().find(|c| *c > col).unwrap_or(start + run.cells);
            return Some((text, run.style, next.saturating_sub(col).max(1)));
        }
        start += run.cells;
        first_char += count;
    }
    None
}

impl WaTerminalView {
    pub fn sticky(&self) -> Modifiers {
        self.sticky
    }

    pub fn set_sticky(&mut self, sticky: Modifiers) {
        self.sticky = sticky;
    }

    /// Whether the soft keyboard was requested.
    pub fn keyboard_requested(&self) -> bool {
        self.show_ime
    }

    /// The sticky modifiers merged into `mods`, then released.
    fn take_sticky(&mut self, mods: Modifiers) -> (Modifiers, bool) {
        let sticky = std::mem::take(&mut self.sticky);
        let merged = Modifiers { shift: mods.shift || sticky.shift, control: mods.control || sticky.control, alt: mods.alt || sticky.alt, logo: mods.logo };
        (merged, sticky != Modifiers::default())
    }

    fn send(&mut self, cx: &mut Cx, core: &CoreHandle, bytes: &[u8]) {
        let Some(session) = self.session.as_ref() else { return };
        if !self.connected || bytes.is_empty() {
            return;
        }
        session.send_input(core, bytes);
        // Typing scrolls back to the prompt and ends the selection.
        self.follow = true;
        if self.selection.range().is_some() {
            self.selection.clear(cx, self.draw_hit.area());
        }
        self.redraw(cx);
    }

    /// Sends an encoded key; returns whether sticky modifiers were consumed.
    pub fn key(&mut self, cx: &mut Cx, core: &CoreHandle, key: Key, mods: Modifiers) -> bool {
        let (mods, consumed) = self.take_sticky(mods);
        let Some(session) = self.session.as_mut() else { return consumed };
        let bytes = session.screen.encode_key(key, mods);
        self.send(cx, core, &bytes);
        consumed
    }

    /// Sends committed text; returns whether sticky modifiers were consumed.
    pub fn text(&mut self, cx: &mut Cx, core: &CoreHandle, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        let (sticky, consumed) = self.take_sticky(Modifiers::default());
        let bytes = if sticky.control || sticky.alt {
            match (input::sticky_key(text, sticky), self.session.as_mut()) {
                (Some((key, mods)), Some(session)) => session.screen.encode_key(key, mods),
                _ => {
                    let mut bytes = if sticky.alt { vec![0x1b] } else { Vec::new() };
                    bytes.extend(input::text_bytes(text));
                    bytes
                }
            }
        } else if sticky.shift {
            input::text_bytes(&text.to_uppercase())
        } else {
            input::text_bytes(text)
        };
        self.send(cx, core, &bytes);
        consumed
    }

    fn rub_out(&mut self, cx: &mut Cx, core: &CoreHandle, count: usize) {
        if count == 0 {
            return;
        }
        let Some(session) = self.session.as_mut() else { return };
        let bytes = session.screen.encode_key(Key::Backspace, Modifiers::default()).repeat(count);
        self.send(cx, core, &bytes);
    }

    /// Pastes text: sanitized, bracketed when the program enabled bracketed paste.
    pub fn paste(&mut self, cx: &mut Cx, core: &CoreHandle, text: &str) {
        self.replaced.clear();
        let Some(session) = self.session.as_ref() else { return };
        let bytes = session.screen.encode_paste(text);
        self.send(cx, core, &bytes);
    }

    fn selected_text(&mut self) -> String {
        let Some((start, end)) = self.selection.ordered() else { return String::new() };
        let Some(session) = self.session.as_mut() else { return String::new() };
        let lines = session.read(start.row, end.row - start.row + 1);
        selected_text(&lines, start, end)
    }

    /// The selection, or every output row without one (toolbar copy).
    pub fn copy_text(&mut self) -> String {
        if self.selection.has_text() {
            return self.selected_text();
        }
        self.session.as_ref().map(Stream::rendered_text).unwrap_or_default()
    }

    fn copy_selection(&mut self, cx: &mut Cx) {
        let text = self.selected_text();
        if !text.is_empty() {
            cx.copy_to_clipboard(&text);
        }
        self.clear_selection(cx);
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        self.selection.clear(cx, self.draw_hit.area());
        self.redraw(cx);
    }

    fn select_all(&mut self, cx: &mut Cx) {
        let rows = self.session.as_ref().map(|s| s.screen.total_rows()).unwrap_or(0);
        if rows == 0 {
            return;
        }
        cx.claim_text_selection(self.draw_hit.area());
        let end = self.snap_cell(CellPos { row: rows - 1, col: usize::MAX }, false).unwrap_or_default();
        self.selection.set(CellPos::default(), end);
        self.redraw(cx);
    }

    /// Takes keyboard focus on the next draw.
    pub fn focus(&mut self, cx: &mut Cx) {
        self.focus_requested = true;
        self.redraw(cx);
    }

    pub fn blur(&mut self, cx: &mut Cx) {
        self.focus_requested = false;
        self.show_ime = false;
        cx.clear_key_focus_if(self.draw_hit.area());
    }

    pub fn show_keyboard(&mut self, cx: &mut Cx) {
        self.show_ime = true;
        self.focus(cx);
    }

    pub fn hide_keyboard(&mut self, cx: &mut Cx) {
        self.show_ime = false;
        cx.hide_text_ime();
        self.redraw(cx);
    }

    /// The pane focus: focused panes claim the PTY size.
    pub fn set_claim(&mut self, claim: bool) {
        self.claim = claim;
    }

    /// Forgets the last sent size (a new subscription or terminal).
    pub fn reset_size(&mut self) {
        self.sent = None;
    }

    /// Reflows the local screen to the fitted size and sends it to the daemon.
    pub fn sync_size(&mut self, core: &CoreHandle, force: bool) {
        let (Some((rows, cols)), Some(session)) = (self.fitted, self.session.as_mut()) else { return };
        match session.screen.resize(rows, cols) {
            Ok(true) => session.publish(),
            Ok(false) => {}
            Err(_) => return,
        }
        if !self.connected || !session.live(true) || !session.is_visible() {
            return;
        }
        let next = (rows, cols, self.claim);
        if !force && self.sent == Some(next) {
            return;
        }
        session.send_resize(core, rows, cols, self.claim);
        self.sent = Some(next);
    }

    fn pos_to_cell(&mut self, cx: &Cx, abs: DVec2) -> Option<CellPos> {
        let (width, height) = self.grid.cell()?;
        let rect = self.draw_hit.area().rect(cx);
        let local = abs - rect.pos + self.hit_origin;
        let row = (local.y / height).floor().max(0.0) as usize;
        let col = (local.x / width).round().max(0.0) as usize;
        self.snap_cell(CellPos { row, col }, false)
    }

    fn snap_cell(&mut self, pos: CellPos, next: bool) -> Option<CellPos> {
        let session = self.session.as_mut()?;
        let row = pos.row.min(session.screen.total_rows().checked_sub(1)?);
        let lines = session.read(row, 1);
        let line = lines.first()?;
        Some(CellPos { row, col: grapheme_column(line, pos.col, next) })
    }

    fn handle_selection_event(&mut self, cx: &mut Cx, event: &Event) -> bool {
        let Some(action) = self.selection.handle_event(cx, event, self.draw_hit.area()) else { return false };
        self.scroll_bars.cancel_touch_scroll();
        if action.phase == TextHandlePhase::Start {
            self.follow = false;
        }
        let mut point = None;
        if action.phase != TextHandlePhase::Cancel {
            if action.scroll != DVec2::default() {
                let scroll = self.scroll_bars.get_scroll_pos();
                self.scroll_bars.set_scroll_pos(cx, scroll + action.scroll);
                self.hit_origin = self.scroll_bars.get_scroll_pos();
                self.follow = false;
            }
            point = self.pos_to_cell(cx, action.abs);
        }
        self.selection.move_endpoint(&action, point);
        self.redraw(cx);
        true
    }

    fn key_down(&mut self, cx: &mut Cx, core: Option<&CoreHandle>, key: &KeyEvent) -> bool {
        self.suppress_text = false;
        self.replaced.clear();
        match input::key_command(key.key_code, key.modifiers, self.selection.has_text(), APPLE, ANDROID) {
            KeyCommand::Send(code, mods) => {
                self.suppress_text = matches!(code, Key::Char(_) | Key::Space);
                // A finished line starts the input method's editor over (`ImeBridge`).
                self.ime_reset |= ANDROID && code == Key::Enter;
                core.is_some_and(|core| self.key(cx, core, code, mods))
            }
            KeyCommand::Copy => {
                self.copy_selection(cx);
                false
            }
            KeyCommand::SelectAll => {
                self.select_all(cx);
                false
            }
            KeyCommand::Find | KeyCommand::Ignore => false,
        }
    }
}

impl Widget for WaTerminalView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.resize_timer.is_event(event).is_some() {
            self.resize_timer = Timer::default();
            if let Some(core) = core_of(scope) {
                self.sync_size(&core, false);
                self.redraw(cx);
            }
        }
        if self.handle_selection_event(cx, event) {
            return;
        }
        let uid = self.widget_uid();
        let area = self.draw_hit.area();
        let before = self.scroll_bars.get_scroll_pos();
        self.scroll_bars.handle_main_event(cx, event, scope, &mut Vec::new());
        let mut consumed = false;
        match event.hits(cx, area) {
            Hit::KeyFocus(_) => {
                self.has_focus = true;
                if let Some(ui) = scope.data.get_mut::<UiContext>() {
                    ui.push(UiEffect::SetFocusScope { owner: uid, scope: Some("terminal".into()) });
                }
                self.redraw(cx);
            }
            Hit::KeyFocusLost(_) => {
                self.has_focus = false;
                self.show_ime = false;
                self.suppress_text = false;
                self.selection.clear(cx, area);
                if let Some(ui) = scope.data.get_mut::<UiContext>() {
                    ui.push(UiEffect::SetFocusScope { owner: uid, scope: None });
                }
                self.redraw(cx);
            }
            Hit::FingerDown(down) => {
                cx.set_key_focus(area);
                self.selection.dragging = false;
                if !down.device.is_touch() && down.device.is_primary_hit() && let Some(pos) = self.pos_to_cell(cx, down.abs) {
                    self.selection.touch = false;
                    self.selection.set(pos, pos);
                    self.selection.dragging = true;
                    self.redraw(cx);
                }
            }
            Hit::FingerLongPress(press) => {
                if let Some(pos) = self.pos_to_cell(cx, press.abs) {
                    cx.claim_text_selection(area);
                    self.selection.touch = true;
                    let end = self.snap_cell(pos, true).unwrap_or(pos);
                    self.selection.set(pos, end);
                    self.selection.dragging = true;
                    self.redraw(cx);
                }
            }
            Hit::FingerMove(moved) => {
                if self.selection.dragging && let (Some(pos), Some((anchor, _))) = (self.pos_to_cell(cx, moved.abs), self.selection.range()) {
                    if let Event::TouchUpdate(update) = event {
                        for touch in &update.touches {
                            if matches!(touch.state, makepad_widgets::makepad_draw::event::TouchState::Move) {
                                touch.handled.set(area);
                            }
                        }
                    }
                    self.follow = false;
                    self.selection.set(anchor, pos);
                    self.redraw(cx);
                }
            }
            Hit::FingerUp(up) => {
                if up.device.is_touch() && up.was_tap() && !self.selection.dragging {
                    // Tapping the grid shows the soft keyboard (original `showKeyboard`).
                    self.selection.clear(cx, area);
                    self.show_keyboard(cx);
                }
                if cfg!(target_os = "android") && up.device.is_touch() && self.selection.dragging {
                    self.selection.show_actions = self.selection.has_text();
                    self.redraw(cx);
                }
                self.selection.dragging = false;
                if self.selection.range().is_some() && !self.selection.has_text() {
                    self.clear_selection(cx);
                }
            }
            Hit::KeyDown(key) => consumed = self.key_down(cx, core_of(scope).as_ref(), &key),
            Hit::KeyUp(_) => self.suppress_text = false,
            Hit::TextInput(text) => {
                if let Some(core) = core_of(scope).as_ref() {
                    if text.was_paste {
                        self.paste(cx, core, &text.input);
                    } else if std::mem::take(&mut self.suppress_text) {
                        // Already sent by the key press (Option/Alt characters on macOS).
                    } else {
                        let removed = self.replaced.apply(&text.input, text.replace_last);
                        self.rub_out(cx, core, removed);
                        consumed = self.text(cx, core, &text.input);
                    }
                }
            }
            Hit::TextIme(edit) => {
                let change = self.ime.apply(&edit.text, edit.composing);
                if let Some(core) = core_of(scope).as_ref() {
                    self.rub_out(cx, core, change.removed);
                    consumed = self.text(cx, core, &change.inserted);
                }
                // Acknowledge the snapshot unchanged so the platform's copy matches the editor;
                // a reset is published on draw, outside this edit, as a new input session.
                self.ime_text = edit.text.clone();
                self.ime_selection = edit.selection_utf8();
                cx.update_terminal_text_input(area, self.ime_id, &self.ime_text, self.ime_selection.0, self.ime_selection.1);
                if change.reset {
                    self.ime_reset = true;
                    self.redraw(cx);
                }
            }
            Hit::TextCopy(copy) => {
                let text = self.selected_text();
                if !text.is_empty() {
                    *copy.response.borrow_mut() = Some(text);
                }
            }
            _ => {}
        }
        if consumed {
            cx.widget_action(uid, &scope.path, WaTerminalViewAction::ModifiersConsumed);
        }
        self.scroll_bars.handle_scroll_event(cx, event, scope, &mut Vec::new());
        let after = self.scroll_bars.get_scroll_pos();
        if (after.y - before.y).abs() > 0.5 && !matches!(event, Event::NextFrame(_)) {
            // Scrolling back to the bottom follows output again.
            self.follow = after.y >= self.max_scroll - 1.0;
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let (width, height) = self.grid.measure(cx);
        let palette = theme::current_palette();
        let paint = GridPaint::new(&palette);
        self.scroll_bars.begin(cx, walk, self.layout);
        let origin = cx.turtle().pos();
        let (rows, cols, history) = match self.session.as_ref() {
            Some(session) => (session.screen.total_rows(), session.cols(), session.screen.history()),
            None => (0, 0, 0),
        };
        // Following keeps the screen's first row at the viewport top, like a terminal window:
        // the content extends below the last row by the viewport's fractional row.
        let screen_top = history as f64 * height;
        let content = dvec2(cols as f64 * width, (rows as f64 * height).max(screen_top + self.viewport.y));
        let viewport = self.viewport;
        let scroll = self.scroll_bars.get_scroll_pos();
        let visible_height = if viewport.y > 0.0 { viewport.y } else { 2000.0 };
        let first = (scroll.y / height).floor().max(0.0) as usize;
        let count = (visible_height / height).ceil() as usize + 2;
        self.hit_origin = scroll;
        self.draw_hit.draw_abs(cx, Rect { pos: origin + scroll, size: dvec2(viewport.x.max(1.0), visible_height) });
        let area = self.draw_hit.area();
        let hit_rect = area.rect(cx);
        if std::mem::take(&mut self.focus_requested) {
            cx.set_key_focus(area);
        }
        self.grid.begin(cx, origin);
        self.selection.begin_frame();
        let mut caret = None;
        if let Some(session) = self.session.as_mut() {
            let cursor = session.screen.cursor();
            let exited = session.exited;
            if first < rows {
                let lines = session.read(first, count.min(rows - first));
                for (index, line) in lines.iter().enumerate() {
                    let row = first + index;
                    let cells: usize = line.runs.iter().map(|run| run.cells).sum();
                    let span = self.selection.span(row, cells);
                    if let Some(rect) = self.grid.draw_line(cx, origin, row, line, &paint, &[], span) {
                        self.selection.painted(rect, hit_rect);
                    }
                }
                let row = history + usize::from(cursor.row);
                let col = usize::from(cursor.col).min(cols.saturating_sub(1));
                if (first..first + lines.len()).contains(&row) {
                    let cell = cell_at(lines[row - first], col);
                    let span = cell.as_ref().map(|(_, _, span)| *span).unwrap_or(1);
                    let rect = Rect { pos: origin + dvec2(col as f64 * width, row as f64 * height), size: dvec2(span as f64 * width, height) };
                    caret = Some(rect);
                    if cursor.visible && !exited {
                        let color = palette.foreground;
                        let line_w = 1.0_f64.max((width * 0.12).round());
                        let hollow = !self.has_focus || cursor.shape == CursorShape::BlockHollow;
                        match cursor.shape {
                            CursorShape::Bar => self.grid.fill(cx, Rect { size: dvec2(line_w.max(2.0), height), ..rect }, color),
                            CursorShape::Underline => {
                                self.grid.fill(cx, Rect { pos: rect.pos + dvec2(0.0, height - 2.0), size: dvec2(rect.size.x, 2.0) }, color)
                            }
                            _ if hollow => {
                                for edge in [
                                    Rect { size: dvec2(rect.size.x, 1.0), ..rect },
                                    Rect { pos: rect.pos + dvec2(0.0, height - 1.0), size: dvec2(rect.size.x, 1.0) },
                                    Rect { size: dvec2(1.0, height), ..rect },
                                    Rect { pos: rect.pos + dvec2(rect.size.x - 1.0, 0.0), size: dvec2(1.0, height) },
                                ] {
                                    self.grid.fill(cx, edge, color);
                                }
                            }
                            _ => {
                                // The cell batch paints under every glyph: the glyph under the
                                // block is redrawn in the background colour on top.
                                self.grid.fill(cx, rect, color);
                                if let Some((text, style, _)) = cell.filter(|(text, _, _)| !text.trim().is_empty()) {
                                    self.grid.draw_glyph(cx, rect.pos, &text, &style, palette.surface0);
                                }
                            }
                        }
                    }
                }
            }
        }
        cx.walk_turtle(Walk::size(Size::Fixed(content.x.max(1.0)), Size::Fixed(content.y.max(1.0))));
        self.scroll_bars.end(cx);
        let viewport = self.scroll_bars.area().rect(cx).size;
        let resized = viewport != self.viewport && viewport.y > 0.0;
        if viewport.y > 0.0 {
            self.viewport = viewport;
            self.max_scroll = (content.y - viewport.y).max(0.0);
            if self.follow {
                let x = self.scroll_bars.get_scroll_pos().x;
                self.scroll_bars.set_scroll_pos(cx, dvec2(x, self.max_scroll));
            }
            let fitted = (((viewport.y / height).floor().max(1.0)).min(f64::from(u16::MAX)) as u16, ((viewport.x / width).floor().max(2.0)).min(f64::from(u16::MAX)) as u16);
            if self.fitted != Some(fitted) {
                self.fitted = Some(fitted);
                cx.stop_timer(self.resize_timer);
                self.resize_timer = cx.start_timeout(RESIZE_DEBOUNCE);
            }
        }
        if self.has_focus && let Some(caret) = caret {
            // Input methods place their candidate window at the cursor; Android edits a
            // hidden editor (`ImeBridge`) republished here, with a pending reset first.
            cx.add_aligned_rect_area(&mut self.ime_caret, caret);
            if std::mem::take(&mut self.ime_reset) {
                self.ime.reset();
                self.ime_text = IME_SENTINEL.to_owned();
                self.ime_selection = (IME_SENTINEL.len(), IME_SENTINEL.len());
            }
            cx.update_terminal_text_input(area, self.ime_id, &self.ime_text, self.ime_selection.0, self.ime_selection.1);
            if !MOBILE || self.show_ime {
                cx.show_text_ime(self.ime_caret, dvec2(0.0, caret.size.y));
            }
        }
        self.selection.draw_overlays(cx, area, origin, (width, height), true);
        if (resized || self.scroll_bars.get_scroll_pos() != scroll)
            && let Some(draw_list) = self.scroll_bars.area().draw_list_id()
        {
            cx.redraw_list_in_draw(draw_list);
        }
        DrawStep::done()
    }
}

impl WaTerminalViewRef {
    pub fn modifiers_consumed(&self, actions: &Actions) -> bool {
        actions
            .filter_widget_actions(self.widget_uid())
            .any(|action| matches!(action.cast(), WaTerminalViewAction::ModifiersConsumed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_terminal::Run;

    #[test]
    fn cursor_cells_know_wide_glyphs() {
        let style = CellStyle { bold: true, ..CellStyle::default() };
        let line = Line {
            text: "a한b".into(),
            columns: vec![0, 1, 3],
            runs: vec![
                Run { text: "a".into(), cells: 1, style: CellStyle::default() },
                Run { text: "한b".into(), cells: 3, style },
            ],
            wraps_next: false,
        };
        assert_eq!(cell_at(&line, 0), Some(("a".into(), CellStyle::default(), 1)));
        assert_eq!(cell_at(&line, 1), Some(("한".into(), style, 2)));
        assert_eq!(cell_at(&line, 3), Some(("b".into(), style, 1)));
        assert_eq!(cell_at(&line, 9), None);
    }
}
