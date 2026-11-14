//! `WaLogView`: the read-only execution output surface (Flutter `_output` of
//! `execution_log_panel.dart`): the shared terminal cell grid with only the rows inside the
//! viewport read and painted, horizontal and vertical scrolling, follow-output (resumed by
//! scrolling back to the end), find highlights, links (Ctrl/Cmd+click, tap) and drag
//! selection with copy.

use makepad_widgets::*;

use super::session::LogSession;
use crate::common::{UiContext, UiEffect};
use crate::features::terminal::find::{FindHits, FindMatch, SearchMode};
use crate::features::terminal::grid::{grapheme_column, selected_text, CellGrid, CellPos, GridPaint, GridSelection};
use crate::features::terminal::links;
use crate::theme;

const APPLE: bool = cfg!(target_vendor = "apple");

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::features::terminal::grid::*;

    pub WaLogView = {{WaLogView}} {
        width: Fill, height: Fill
        scroll_bars: <ScrollBars> { show_scroll_x: true, show_scroll_y: true }
        grid: <WaCellGrid> {}
        draw_hit: { color: #0000 }
    }
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaLogViewAction {
    None,
    /// The user scrolled the output: following stops, or resumes at the end.
    UserScrolled,
    /// Ctrl/Cmd+F while the output has focus.
    FindRequested,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaLogView {
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
    pub session: Option<LogSession>,
    #[rust]
    pub follow: bool,
    /// Find matches painted over the rows.
    #[rust]
    pub find: FindHits,
    #[rust]
    reveal: Option<FindMatch>,
    #[rust(GridSelection::new(cx))]
    selection: GridSelection,
    #[rust]
    hit_origin: DVec2,
    #[rust]
    viewport: DVec2,
    #[rust]
    max_scroll: f64,
}

impl WaLogView {
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
        let row = pos.row.min(session.line_count().checked_sub(1)?);
        let lines = session.read(row, 1);
        let line = lines.first()?;
        Some(CellPos { row, col: grapheme_column(line, pos.col, next) })
    }

    fn handle_selection_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) -> bool {
        let Some(action) = self.selection.handle_event(cx, event, self.draw_hit.area()) else { return false };
        self.scroll_bars.cancel_touch_scroll();
        if action.phase == TextHandlePhase::Start {
            self.follow = false;
            cx.widget_action(self.widget_uid(), &scope.path, WaLogViewAction::UserScrolled);
        }
        let mut point = None;
        if action.phase != TextHandlePhase::Cancel {
            if action.scroll != DVec2::default() {
                let scroll = self.scroll_bars.get_scroll_pos();
                self.scroll_bars.set_scroll_pos(cx, scroll + action.scroll);
                self.hit_origin = self.scroll_bars.get_scroll_pos();
                self.follow = false;
                cx.widget_action(self.widget_uid(), &scope.path, WaLogViewAction::UserScrolled);
            }
            point = self.pos_to_cell(cx, action.abs);
        }
        self.selection.move_endpoint(&action, point);
        self.redraw(cx);
        true
    }

    /// The selected text, with logical lines joined like `renderedText`.
    fn selected_text(&mut self) -> String {
        let Some((start, end)) = self.selection.ordered() else { return String::new() };
        let Some(session) = self.session.as_mut() else { return String::new() };
        let lines = session.read(start.row, end.row - start.row + 1);
        selected_text(&lines, start, end)
    }

    /// Searches the output for the panel's find card and reveals the chosen match.
    pub fn run_find(&mut self, cx: &mut Cx, query: &str, mode: SearchMode) {
        match self.session.as_ref() {
            Some(session) => {
                if let Some(target) = self.find.search(session, query, mode) {
                    self.follow = false;
                    self.reveal = Some(target);
                }
            }
            None => self.find.clear(),
        }
        self.redraw(cx);
    }

    pub fn clear_find(&mut self, cx: &mut Cx) {
        self.find.clear();
        self.redraw(cx);
    }

    /// The link under `abs`, following the row's soft wraps.
    fn link_at(&mut self, cx: &Cx, abs: DVec2) -> Option<String> {
        let (width, height) = self.grid.cell()?;
        let local = abs - self.draw_hit.area().rect(cx).pos + self.hit_origin;
        if local.x < 0.0 || local.y < 0.0 {
            return None;
        }
        let row = (local.y / height).floor() as usize;
        let session = self.session.as_mut()?;
        if row >= session.line_count() {
            return None;
        }
        links::link_at(session, row, (local.x / width).floor() as usize)
    }

    fn open_link(scope: &mut Scope, url: String) {
        if let Some(ui) = scope.data.get_mut::<UiContext>() {
            ui.push(UiEffect::OpenExternal { url });
        }
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        self.selection.clear(cx, self.draw_hit.area());
        self.redraw(cx);
    }
}

impl Widget for WaLogView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if self.handle_selection_event(cx, event, scope) {
            return;
        }
        let uid = self.widget_uid();
        let area = self.draw_hit.area();
        let before = self.scroll_bars.get_scroll_pos();
        self.scroll_bars.handle_main_event(cx, event, scope, &mut Vec::new());
        match event.hits(cx, area) {
            Hit::FingerHoverIn(hover) | Hit::FingerHoverOver(hover) => {
                let primary = if APPLE { hover.modifiers.logo } else { hover.modifiers.control };
                if primary && self.link_at(cx, hover.abs).is_some() {
                    cx.set_cursor(MouseCursor::Hand);
                }
            }
            Hit::FingerDown(down) => {
                cx.claim_text_selection(area);
                self.selection.dragging = false;
                let primary = if APPLE { down.modifiers.logo } else { down.modifiers.control };
                if !down.device.is_touch() && down.device.is_primary_hit() && primary && let Some(url) = self.link_at(cx, down.abs) {
                    // Ctrl/Cmd+click opens a link instead of starting a selection.
                    Self::open_link(scope, url);
                } else if !down.device.is_touch() && down.device.is_primary_hit() && let Some(pos) = self.pos_to_cell(cx, down.abs) {
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
                    self.selection.set(anchor, pos);
                    self.redraw(cx);
                }
            }
            Hit::FingerUp(up) => {
                if up.device.is_touch() && up.was_tap() && !self.selection.dragging {
                    self.clear_selection(cx);
                    // Tapping a link opens it (phones have no Ctrl/Cmd+click).
                    if let Some(url) = self.link_at(cx, up.abs) {
                        Self::open_link(scope, url);
                    }
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
            Hit::TextCopy(copy) => {
                let text = self.selected_text();
                if !text.is_empty() {
                    *copy.response.borrow_mut() = Some(text);
                }
            }
            Hit::KeyDown(key) => {
                if key.key_code == KeyCode::KeyF && key.modifiers.is_primary() {
                    cx.widget_action(uid, &scope.path, WaLogViewAction::FindRequested);
                }
                if key.key_code == KeyCode::KeyA && key.modifiers.is_primary() {
                    let rows = self.session.as_ref().map(|s| s.line_count()).unwrap_or(0);
                    if rows > 0 {
                        cx.claim_text_selection(area);
                        let end = self.snap_cell(CellPos { row: rows - 1, col: usize::MAX }, false).unwrap_or_default();
                        self.selection.set(CellPos::default(), end);
                        self.redraw(cx);
                    }
                }
            }
            Hit::KeyFocusLost(_) => self.clear_selection(cx),
            _ => {}
        }
        self.scroll_bars.handle_scroll_event(cx, event, scope, &mut Vec::new());
        let after = self.scroll_bars.get_scroll_pos();
        if (after.y - before.y).abs() > 0.5 && !matches!(event, Event::NextFrame(_)) {
            // Scrolling away pauses following; scrolling back to the end resumes it.
            self.follow = after.y >= self.max_scroll - 1.0;
            cx.widget_action(uid, &scope.path, WaLogViewAction::UserScrolled);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let (width, height) = self.grid.measure(cx);
        let palette = theme::current_palette();
        let paint = GridPaint::new(&palette);
        self.scroll_bars.begin(cx, walk, self.layout);
        let origin = cx.turtle().pos();
        let (rows, cols) = match self.session.as_ref() {
            Some(session) => (session.line_count(), session.cols()),
            None => (0, 0),
        };
        let content = dvec2(cols as f64 * width, rows as f64 * height);
        let viewport = self.viewport;
        let scroll = self.scroll_bars.get_scroll_pos();
        let visible_height = if viewport.y > 0.0 { viewport.y } else { 2000.0 };
        let first = (scroll.y / height).floor().max(0.0) as usize;
        let count = (visible_height / height).ceil() as usize + 2;
        // Hit surface over the visible viewport.
        self.hit_origin = dvec2(scroll.x, scroll.y);
        self.draw_hit.draw_abs(cx, Rect { pos: origin + scroll, size: dvec2(viewport.x.max(1.0), visible_height) });
        let area = self.draw_hit.area();
        let hit_rect = area.rect(cx);
        self.grid.begin(cx, origin);
        self.selection.begin_frame();
        if let Some(session) = self.session.as_mut() && first < rows {
            let lines = session.read(first, count.min(rows - first));
            for (index, line) in lines.iter().enumerate() {
                let row = first + index;
                let highlights = self.find.row(row, line.columns.len());
                let cells: usize = line.runs.iter().map(|run| run.cells).sum();
                let span = self.selection.span(row, cells);
                if let Some(rect) = self.grid.draw_line(cx, dvec2(origin.x, origin.y + row as f64 * height), line, &paint, &highlights, span) {
                    self.selection.painted(rect, hit_rect);
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
            // `end` measures the current content and viewport. Before it, scroll
            // setters clamp against the previous frame (zero on the initial draw).
            if let Some(target) = self.reveal.take() {
                let y = (target.start_row as f64 * height - viewport.y / 3.0).clamp(0.0, self.max_scroll);
                let x = (target.start_offset as f64 * width - viewport.x / 2.0).clamp(0.0, (content.x - viewport.x).max(0.0));
                self.scroll_bars.set_scroll_pos(cx, dvec2(x, y));
            } else if self.follow {
                let x = self.scroll_bars.get_scroll_pos().x;
                self.scroll_bars.set_scroll_pos(cx, dvec2(x, self.max_scroll));
            }
        }
        self.selection.draw_overlays(cx, area, origin, (width, height), false);
        // The turtle and visible row range were drawn with `scroll`. Repaint only
        // this list when measurement or positioning changed; ordinary redraw()
        // requests are ignored inside draw. The settled frame schedules nothing.
        if (resized || self.scroll_bars.get_scroll_pos() != scroll)
            && let Some(draw_list) = self.scroll_bars.area().draw_list_id()
        {
            cx.redraw_list_in_draw(draw_list);
        }
        DrawStep::done()
    }
}

impl WaLogViewRef {
    pub fn user_scrolled(&self, actions: &Actions) -> bool {
        actions
            .filter_widget_actions(self.widget_uid())
            .any(|action| matches!(action.cast(), WaLogViewAction::UserScrolled))
    }

    pub fn find_requested(&self, actions: &Actions) -> bool {
        actions
            .filter_widget_actions(self.widget_uid())
            .any(|action| matches!(action.cast(), WaLogViewAction::FindRequested))
    }
}
