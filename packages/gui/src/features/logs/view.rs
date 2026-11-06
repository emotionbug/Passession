//! `WaLogView`: the read-only execution output surface (Flutter `_output` of
//! `execution_log_panel.dart`): a monospaced cell grid in the code font with the terminal
//! ANSI palette, only the rows inside the viewport laid out and painted, horizontal and
//! vertical scrolling, follow-output, find highlights and drag selection with copy.

use makepad_widgets::*;

use super::session::{Line, LogSession, Run, TermColor};
use crate::theme::{self, Palette, Typography};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;

    pub WaLogView = {{WaLogView}} {
        width: Fill, height: Fill
        scroll_bars: <ScrollBars> { show_scroll_x: true, show_scroll_y: true }
        draw_text: {
            text_style: <PASEO_FONT_MONO> { line_spacing: 1.0 }
        }
        draw_text_bold: {
            text_style: <PASEO_FONT_MONO> { line_spacing: 1.0 }
        }
        // The code font ships no italic face; like the original's synthesized oblique, glyphs
        // shear 0.2 around the bottom of their box.
        draw_text_italic: {
            text_style: <PASEO_FONT_MONO> { line_spacing: 1.0 }
            fn vertex(self) -> vec4 {
                let p = mix(self.rect_pos, self.rect_pos + self.rect_size, self.geom_pos);
                let p_clipped = clamp(p, self.draw_clip.xy, self.draw_clip.zw);
                let p_normalized: vec2 = (p_clipped - self.rect_pos) / self.rect_size;
                self.pos = p_normalized;
                self.t = mix(self.t_min, self.t_max, p_normalized.xy);
                let shear = (self.rect_pos.y + self.rect_size.y - p_clipped.y) * 0.2;
                self.world = self.view_transform * vec4(
                    p_clipped.x + shear,
                    p_clipped.y,
                    self.glyph_depth + self.draw_zbias,
                    1.
                );
                return self.camera_projection * (self.camera_view * (self.world));
            }
        }
        draw_cell: {}
        draw_hit: { color: #0000 }
    }
}

/// A find match in presented row/char offsets (`_LogMatch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogMatch {
    pub start_row: usize,
    pub start_offset: usize,
    pub end_row: usize,
    pub end_offset: usize,
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaLogViewAction {
    None,
    /// The user scrolled the output (pauses following).
    UserScrolled,
    /// Ctrl/Cmd+F while the output has focus.
    FindRequested,
}

/// Colours and the find revision shared by every painted row.
#[derive(Clone, Copy)]
struct LinePaint<'a> {
    ansi: &'a [Vec4; 16],
    palette: &'a Palette,
    revision: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct CellPos {
    row: usize,
    col: usize,
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
    draw_text: DrawText,
    #[live]
    draw_text_bold: DrawText,
    #[live]
    draw_text_italic: DrawText,
    #[live]
    draw_cell: DrawColor,
    #[live]
    draw_hit: DrawColor,
    #[rust]
    pub session: Option<LogSession>,
    /// Cell size for the code font size it was measured at.
    #[rust]
    cell: Option<(f64, f64, f64)>,
    #[rust]
    pub follow: bool,
    #[rust]
    pub matches: Vec<LogMatch>,
    #[rust]
    pub active_match: Option<usize>,
    #[rust]
    pub matches_revision: u64,
    #[rust]
    reveal: Option<LogMatch>,
    #[rust]
    selection: Option<(CellPos, CellPos)>,
    #[rust]
    dragging: bool,
    #[rust]
    hit_origin: DVec2,
    #[rust]
    viewport: DVec2,
    #[rust]
    show_actions: bool,
    #[rust]
    action_anchor: Option<Rect>,
}

impl WaLogView {
    fn cell_size(&mut self, cx: &mut Cx2d) -> (f64, f64) {
        // The original sizes the output from the code font size preference (12px default).
        let px = theme::current_typography().code_font_size;
        if let Some((width, height, size)) = self.cell
            && size == px
        {
            return (width, height);
        }
        self.draw_text.text_style.font_size = Typography::pt(px);
        self.draw_text_bold.text_style.font_size = Typography::pt(px);
        self.draw_text_italic.text_style.font_size = Typography::pt(px);
        let laid = self.draw_text.layout(cx, 0.0, 0.0, None, false, Align::default(), "MMMMMMMMMM");
        let width = (laid.size_in_lpxs.width as f64 / 10.0).max(4.0);
        // xterm `lineHeight: 1.0`: rows are exactly one cell tall.
        let height = (laid.size_in_lpxs.height as f64).ceil().max(8.0);
        self.cell = Some((width, height, px));
        (width, height)
    }

    fn ansi(&self, palette: &Palette) -> [Vec4; 16] {
        let h = crate::features::workspace::identity::rgb;
        if palette.is_dark {
            [
                palette.sidebar, h(0xe07070), h(0x5dba80), h(0xd4a44a), h(0x6a9de0), h(0xb07ad0), h(0x4aabb8), h(0xd4d4d8),
                palette.surface3, h(0xe89090), h(0x7ecf9a), h(0xe0be6e), h(0x8ab4e8), h(0xc49ae0), h(0x6ec2cc), h(0xf0f0f2),
            ]
        } else {
            [
                h(0x1a1a1e), h(0xdc2626), h(0x16a34a), h(0xca8a04), h(0x2563eb), h(0x9333ea), h(0x0891b2), h(0xffffff),
                h(0x3f3f46), h(0xef4444), h(0x22c55e), h(0xf59e0b), h(0x3b82f6), h(0xa855f7), h(0x06b6d4), h(0xfafafa),
            ]
        }
    }

    /// `_TerminalPalette._color`: indexed, 256-color cube, grays and RGB.
    fn color(ansi: &[Vec4; 16], color: TermColor, bold: bool) -> Option<Vec4> {
        match color {
            TermColor::Default => None,
            TermColor::Idx(index) if index < 16 => {
                // xterm.js `drawBoldTextInBrightColors`: bold normal colors brighten.
                let index = if bold && index < 8 { index + 8 } else { index };
                Some(ansi[index as usize])
            }
            TermColor::Idx(index) if index < 232 => {
                const STEPS: [f32; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
                let cube = (index - 16) as usize;
                Some(vec4(STEPS[cube / 36] / 255.0, STEPS[(cube / 6) % 6] / 255.0, STEPS[cube % 6] / 255.0, 1.0))
            }
            TermColor::Idx(index) => {
                let gray = (8 + (index as usize - 232) * 10) as f32 / 255.0;
                Some(vec4(gray, gray, gray, 1.0))
            }
            TermColor::Rgb(r, g, b) => Some(vec4(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)),
        }
    }

    fn run_colors(ansi: &[Vec4; 16], palette: &Palette, run: &Run) -> (Vec4, Option<Vec4>) {
        let mut fg = Self::color(ansi, run.fg, run.bold).unwrap_or(palette.foreground);
        let mut bg = Self::color(ansi, run.bg, false);
        if run.inverse {
            let swapped = bg.unwrap_or(palette.surface0);
            bg = Some(fg);
            fg = swapped;
        }
        if run.dim {
            fg.w *= 0.5;
        }
        (fg, bg)
    }

    fn pos_to_cell(&self, cx: &Cx, abs: DVec2) -> Option<CellPos> {
        let (width, height, _) = self.cell?;
        let rect = self.draw_hit.area().rect(cx);
        let local = abs - rect.pos + self.hit_origin;
        let row = (local.y / height).floor().max(0.0) as usize;
        let col = (local.x / width).round().max(0.0) as usize;
        Some(CellPos { row, col })
    }

    /// The selected text, with logical lines joined like `renderedText`.
    fn selected_text(&mut self) -> String {
        let Some((a, b)) = self.selection else { return String::new() };
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let Some(session) = self.session.as_mut() else { return String::new() };
        let lines = session.read(start.row, end.row - start.row + 1);
        let mut out = String::new();
        for (index, line) in lines.iter().enumerate() {
            let row = start.row + index;
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row { end.col } else { usize::MAX };
            let text: String = line
                .text
                .chars()
                .zip(line.columns.iter())
                .filter(|(_, col)| **col >= from && **col < to)
                .map(|(ch, _)| ch)
                .collect();
            out.push_str(&text);
            if row != end.row && !line.wraps_next {
                out.push('\n');
            }
        }
        out
    }

    /// Highlight ranges of `row` (`_rowHighlights`): every match and the active one.
    fn row_highlights(&self, row: usize, length: usize, revision: u64) -> Vec<(usize, usize, bool)> {
        if self.matches_revision != revision || self.matches.is_empty() {
            return Vec::new();
        }
        let first = self.matches.partition_point(|m| m.end_row < row);
        let mut out = Vec::new();
        for (index, m) in self.matches.iter().enumerate().skip(first) {
            if m.start_row > row {
                break;
            }
            let start = if row == m.start_row { m.start_offset } else { 0 }.min(length);
            let end = if row == m.end_row { m.end_offset } else { length }.clamp(start, length);
            if end > start {
                out.push((start, end, Some(index) == self.active_match));
            }
        }
        out
    }

    fn draw_line(&mut self, cx: &mut Cx2d, origin: DVec2, row: usize, line: &Line, paint: &LinePaint<'_>) {
        let LinePaint { ansi, palette, revision } = *paint;
        let (width, height) = self.cell.map(|(w, h, _)| (w, h)).unwrap_or((8.0, 14.0));
        let y = origin.y + row as f64 * height;
        // Backgrounds first.
        let mut col = 0usize;
        for run in &line.runs {
            let (_, bg) = Self::run_colors(ansi, palette, run);
            if let Some(bg) = bg {
                self.draw_cell.color = bg;
                self.draw_cell.draw_abs(cx, Rect { pos: dvec2(origin.x + col as f64 * width, y), size: dvec2(run.cells as f64 * width, height) });
            }
            col += run.cells;
        }
        // Find highlights in char offsets, painted under the glyphs.
        let char_count = line.columns.len();
        for (start, end, active) in self.row_highlights(row, char_count, revision) {
            let from = line.columns.get(start).copied().unwrap_or(0);
            let to = line.columns.get(end - 1).copied().map(|c| c + 1).unwrap_or(from + 1);
            let base = if active { ansi[4] } else { ansi[3] };
            self.draw_cell.color = vec4(base.x, base.y, base.z, if active { 0.45 } else { 0.35 });
            self.draw_cell.draw_abs(cx, Rect { pos: dvec2(origin.x + from as f64 * width, y), size: dvec2((to - from) as f64 * width, height) });
        }
        // Selection.
        if let Some((a, b)) = self.selection {
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            if row >= start.row && row <= end.row && start != end {
                let from = if row == start.row { start.col } else { 0 };
                let to = if row == end.row { end.col.min(col.max(1)) } else { col.max(1) };
                if to > from {
                    self.draw_cell.color = if palette.is_dark { vec4(1.0, 1.0, 1.0, 0.2) } else { vec4(0.0, 0.0, 0.0, 0.15) };
                    self.draw_cell.draw_abs(cx, Rect { pos: dvec2(origin.x + from as f64 * width, y), size: dvec2((to - from) as f64 * width, height) });
                    let viewport = self.draw_hit.area().rect(cx);
                    let visible = Rect { pos: dvec2(origin.x + from as f64 * width, y), size: dvec2((to - from) as f64 * width, height) }
                        .clip((viewport.pos, viewport.pos + viewport.size));
                    if self.action_anchor.is_none() && visible.size.x > 0.0 && visible.size.y > 0.0 {
                        self.action_anchor = Some(visible);
                    }
                }
            }
        }
        // Glyphs, one draw per run.
        let mut col = 0usize;
        for run in &line.runs {
            let (fg, _) = Self::run_colors(ansi, palette, run);
            let pos = dvec2(origin.x + col as f64 * width, y);
            // SGR 8 (`CellAttr.invisible`): the glyphs and their decorations take a transparent
            // foreground; the cells keep their width, background and selection.
            if run.hidden {
                col += run.cells;
                continue;
            }
            if !run.text.trim().is_empty() {
                // The code font has one weight: bold is synthesized by a second pass half a
                // pixel to the right, italic by the sheared drawer.
                let drawer = if run.italic {
                    &mut self.draw_text_italic
                } else if run.bold {
                    &mut self.draw_text_bold
                } else {
                    &mut self.draw_text
                };
                drawer.color = fg;
                drawer.draw_abs(cx, pos, &run.text);
                if run.bold {
                    drawer.draw_abs(cx, pos + dvec2(0.5, 0.0), &run.text);
                }
            }
            if run.underline {
                self.draw_cell.color = fg;
                self.draw_cell.draw_abs(cx, Rect { pos: dvec2(pos.x, y + height - 1.0), size: dvec2(run.cells as f64 * width, 1.0) });
            }
            // SGR 9 (`TextDecoration.lineThrough`) at the middle of the row.
            if run.strike {
                self.draw_cell.color = fg;
                self.draw_cell.draw_abs(cx, Rect { pos: dvec2(pos.x, (y + height * 0.5).floor()), size: dvec2(run.cells as f64 * width, 1.0) });
            }
            col += run.cells;
        }
    }

    pub fn reveal(&mut self, cx: &mut Cx, target: LogMatch) {
        self.reveal = Some(target);
        self.redraw(cx);
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        cx.clear_text_selection(self.draw_hit.area());
        self.dragging = false;
        self.show_actions = false;
        self.action_anchor = None;
        self.selection = None;
        self.redraw(cx);
    }
}

impl Widget for WaLogView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let uid = self.widget_uid();
        let before = self.scroll_bars.get_scroll_pos();
        self.scroll_bars.handle_main_event(cx, event, scope, &mut Vec::new());
        match event.hits(cx, self.draw_hit.area()) {
            Hit::FingerDown(down) => {
                cx.claim_text_selection(self.draw_hit.area());
                self.dragging = false;
                if !down.device.is_touch() && let Some(pos) = self.pos_to_cell(cx, down.abs) {
                    self.selection = Some((pos, pos));
                    self.dragging = true;
                    self.redraw(cx);
                }
            }
            Hit::FingerLongPress(press) => {
                if let Some(pos) = self.pos_to_cell(cx, press.abs) {
                    cx.claim_text_selection(self.draw_hit.area());
                    self.selection = Some((pos, CellPos { row: pos.row, col: pos.col + 1 }));
                    self.dragging = true;
                    self.redraw(cx);
                }
            }
            Hit::FingerMove(moved) => {
                if self.dragging && let (Some(pos), Some((anchor, _))) = (self.pos_to_cell(cx, moved.abs), self.selection) {
                    if let Event::TouchUpdate(update) = event {
                        for touch in &update.touches {
                            if matches!(touch.state, makepad_widgets::makepad_draw::event::TouchState::Move) {
                                touch.handled.set(self.draw_hit.area());
                            }
                        }
                    }
                    self.selection = Some((anchor, pos));
                    self.redraw(cx);
                }
            }
            Hit::FingerUp(up) => {
                if up.device.is_touch() && up.was_tap() && !self.dragging {
                    self.clear_selection(cx);
                }
                if cfg!(target_os = "android") && up.device.is_touch() && self.dragging {
                    self.show_actions = self.selection.is_some_and(|(a, b)| a != b);
                    self.redraw(cx);
                }
                self.dragging = false;
                if let Some((a, b)) = self.selection && a == b {
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
                        cx.claim_text_selection(self.draw_hit.area());
                        self.selection = Some((CellPos { row: 0, col: 0 }, CellPos { row: rows - 1, col: usize::MAX / 2 }));
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
            cx.widget_action(uid, &scope.path, WaLogViewAction::UserScrolled);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let (width, height) = self.cell_size(cx);
        let palette = theme::current_palette();
        let ansi = self.ansi(&palette);
        self.scroll_bars.begin(cx, walk, self.layout);
        let origin = cx.turtle().pos();
        let (rows, cols, revision) = match self.session.as_ref() {
            Some(session) => (session.line_count(), session.cols(), session.revision),
            None => (0, 0, 0),
        };
        let content = dvec2(cols as f64 * width, rows as f64 * height);
        let viewport = self.viewport;
        let scroll = self.scroll_bars.get_scroll_pos();
        let visible_height = if viewport.y > 0.0 { viewport.y } else { 2000.0 };
        let first = (scroll.y / height).floor().max(0.0) as usize;
        let count = (visible_height / height).ceil() as usize + 2;
        // Hit surface over the visible viewport.
        self.hit_origin = dvec2(scroll.x, scroll.y);
        self.draw_hit.draw_abs(
            cx,
            Rect { pos: origin + dvec2(scroll.x, scroll.y), size: dvec2(viewport.x.max(1.0), visible_height) },
        );
        // Makepad batches later instances into the first draw call. Start the cell batch
        // before any text, or a background on a later row can cover that row's glyphs.
        self.draw_cell.color = Vec4::default();
        self.draw_cell.draw_abs(cx, Rect { pos: origin, size: dvec2(1.0, 1.0) });
        self.action_anchor = None;
        if let Some(session) = self.session.as_mut() && first < rows {
            let lines = session.read(first, count.min(rows - first));
            let paint = LinePaint { ansi: &ansi, palette: &palette, revision };
            for (index, line) in lines.iter().enumerate() {
                self.draw_line(cx, origin, first + index, line, &paint);
            }
        }
        cx.walk_turtle(Walk::size(Size::Fixed(content.x.max(1.0)), Size::Fixed(content.y.max(1.0))));
        self.scroll_bars.end(cx);
        let viewport = self.scroll_bars.area().rect(cx).size;
        let resized = viewport != self.viewport && viewport.y > 0.0;
        if viewport.y > 0.0 {
            self.viewport = viewport;
            // `end` measures the current content and viewport. Before it, scroll
            // setters clamp against the previous frame (zero on the initial draw).
            if let Some(target) = self.reveal.take() {
                let y = (target.start_row as f64 * height).min((content.y - viewport.y).max(0.0));
                let x = (target.start_offset as f64 * width).min((content.x - viewport.x).max(0.0));
                self.scroll_bars.set_scroll_pos(cx, dvec2(x.max(0.0), y.max(0.0)));
            } else if self.follow {
                let x = self.scroll_bars.get_scroll_pos().x;
                self.scroll_bars.set_scroll_pos(cx, dvec2(x, (content.y - viewport.y).max(0.0)));
            }
        }
        if self.show_actions || cx.text_actions_active(self.draw_hit.area()) {
            self.show_actions = false;
            if let Some(anchor) = self.action_anchor {
                cx.show_text_actions(self.draw_hit.area(), anchor, TextActionCapabilities {
                    copy: true, select_all: true, ..Default::default()
                });
            } else {
                cx.clear_text_selection(self.draw_hit.area());
                cx.claim_text_selection(self.draw_hit.area());
            }
        }
        // The turtle and visible row range were drawn with `scroll`. Repaint only
        // this list when measurement or positioning changed; ordinary redraw()
        // requests are ignored inside draw. The settled frame schedules nothing.
        if resized || self.scroll_bars.get_scroll_pos() != scroll {
            if let Some(draw_list) = self.scroll_bars.area().draw_list_id() {
                cx.redraw_list_in_draw(draw_list);
            }
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
