//! The monospaced terminal cell grid shared by the execution log output and the
//! interactive terminal: code-font cell metrics, the terminal ANSI palette, styled row
//! painting (backgrounds, find highlights, selection, glyphs, decorations) and the
//! selection model with its touch handles and text actions.

use makepad_widgets::*;
use paseo_terminal::{CellStyle, Color, Line};

use crate::theme::{self, Palette, Typography};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use crate::theme::*;

    pub WaCellGrid = {{CellGrid}} {
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
    }
}

/// A grid position in presented rows and cell columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPos {
    pub row: usize,
    pub col: usize,
}

/// Colours shared by every painted row of one frame.
pub struct GridPaint<'a> {
    pub ansi: [Vec4; 16],
    pub palette: &'a Palette,
}

impl<'a> GridPaint<'a> {
    pub fn new(palette: &'a Palette) -> Self {
        Self { ansi: ansi(palette), palette }
    }

    /// Foreground and optional background of a styled cell (`_TerminalPalette`): inverse
    /// swaps them, dim halves the foreground alpha.
    pub fn colors(&self, style: &CellStyle) -> (Vec4, Option<Vec4>) {
        let mut fg = color(&self.ansi, style.fg, style.bold).unwrap_or(self.palette.foreground);
        let mut bg = color(&self.ansi, style.bg, false);
        if style.inverse {
            let swapped = bg.unwrap_or(self.palette.surface0);
            bg = Some(fg);
            fg = swapped;
        }
        if style.dim {
            fg.w *= 0.5;
        }
        (fg, bg)
    }

    pub fn selection(&self) -> Vec4 {
        if self.palette.is_dark { vec4(1.0, 1.0, 1.0, 0.2) } else { vec4(0.0, 0.0, 0.0, 0.15) }
    }
}

/// The 16 ANSI colours of the current theme.
fn ansi(palette: &Palette) -> [Vec4; 16] {
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
fn color(ansi: &[Vec4; 16], color: Color, bold: bool) -> Option<Vec4> {
    match color {
        Color::Default => None,
        Color::Indexed(index) if index < 16 => {
            // xterm.js `drawBoldTextInBrightColors`: bold normal colors brighten.
            let index = if bold && index < 8 { index + 8 } else { index };
            Some(ansi[usize::from(index)])
        }
        Color::Indexed(index) if index < 232 => {
            const STEPS: [f32; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
            let cube = usize::from(index - 16);
            Some(vec4(STEPS[cube / 36] / 255.0, STEPS[(cube / 6) % 6] / 255.0, STEPS[cube % 6] / 255.0, 1.0))
        }
        Color::Indexed(index) => {
            let gray = (8 + usize::from(index - 232) * 10) as f32 / 255.0;
            Some(vec4(gray, gray, gray, 1.0))
        }
        Color::Rgb(r, g, b) => Some(vec4(f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0, 1.0)),
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct CellGrid {
    #[live]
    draw_text: DrawText,
    #[live]
    draw_text_bold: DrawText,
    #[live]
    draw_text_italic: DrawText,
    #[live]
    draw_cell: DrawColor,
    /// Cell size for the code font size and theme epoch (font changes) it was measured at.
    #[rust]
    cell: Option<(f64, f64, f64, u64)>,
}

impl CellGrid {
    /// The cell size of the code font size preference (12px default); a theme change (the
    /// code font itself) measures again.
    pub fn measure(&mut self, cx: &mut Cx2d) -> (f64, f64) {
        let px = theme::current_typography().code_font_size;
        let epoch = theme::epoch();
        if let Some((width, height, size, measured)) = self.cell
            && size == px
            && measured == epoch
        {
            return (width, height);
        }
        for drawer in [&mut self.draw_text, &mut self.draw_text_bold, &mut self.draw_text_italic] {
            drawer.text_style.font_size = Typography::pt(px);
        }
        let laid = self.draw_text.layout(cx, 0.0, 0.0, None, false, Align::default(), "MMMMMMMMMM");
        let width = (f64::from(laid.size_in_lpxs.width) / 10.0).max(4.0);
        // xterm `lineHeight: 1.0`: rows are exactly one cell tall.
        let height = f64::from(laid.size_in_lpxs.height).ceil().max(8.0);
        self.cell = Some((width, height, px, epoch));
        (width, height)
    }

    /// The last measured cell size.
    pub fn cell(&self) -> Option<(f64, f64)> {
        self.cell.map(|(width, height, _, _)| (width, height))
    }

    /// Makepad batches later instances into the first draw call. Start the cell batch before
    /// any text, or a background on a later row can cover that row's glyphs.
    pub fn begin(&mut self, cx: &mut Cx2d, origin: DVec2) {
        self.draw_cell.color = Vec4::default();
        self.draw_cell.draw_abs(cx, Rect { pos: origin, size: dvec2(1.0, 1.0) });
    }

    pub fn fill(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4) {
        self.draw_cell.color = color;
        self.draw_cell.draw_abs(cx, rect);
    }

    /// Paints `line` at its absolute row `origin`: backgrounds, find `highlights`
    /// (char ranges, active flag), the selected cell span, glyphs and decorations. Returns the
    /// painted selection rect.
    pub fn draw_line(
        &mut self,
        cx: &mut Cx2d,
        origin: DVec2,
        line: &Line,
        paint: &GridPaint<'_>,
        highlights: &[(usize, usize, bool)],
        selection: Option<(usize, usize)>,
    ) -> Option<Rect> {
        let (width, height) = self.cell().unwrap_or((8.0, 14.0));
        let y = origin.y;
        let cells = |from: usize, count: usize| Rect { pos: dvec2(origin.x + from as f64 * width, y), size: dvec2(count as f64 * width, height) };
        // Backgrounds first.
        let mut col = 0usize;
        for run in &line.runs {
            if let (_, Some(bg)) = paint.colors(&run.style) {
                self.fill(cx, cells(col, run.cells), bg);
            }
            col += run.cells;
        }
        // Find highlights in char offsets, painted under the glyphs.
        for &(start, end, active) in highlights {
            let from = line.columns.get(start).copied().unwrap_or(0);
            let to = line.columns.get(end - 1).copied().map(|c| c + 1).unwrap_or(from + 1);
            let base = if active { paint.ansi[4] } else { paint.ansi[3] };
            self.fill(cx, cells(from, to - from), vec4(base.x, base.y, base.z, if active { 0.45 } else { 0.35 }));
        }
        let mut selected = None;
        if let Some((from, to)) = selection
            && to > from
        {
            let rect = cells(from, to - from);
            self.fill(cx, rect, paint.selection());
            selected = Some(rect);
        }
        // Glyphs, one draw per run.
        let mut col = 0usize;
        for run in &line.runs {
            let style = &run.style;
            let (fg, _) = paint.colors(style);
            let pos = dvec2(origin.x + col as f64 * width, y);
            // SGR 8 (`CellAttr.invisible`): the glyphs and their decorations take a transparent
            // foreground; the cells keep their width, background and selection.
            if style.hidden {
                col += run.cells;
                continue;
            }
            if !run.text.trim().is_empty() {
                // The code font has one weight: bold is synthesized by a second pass half a
                // pixel to the right, italic by the sheared drawer.
                let drawer = if style.italic {
                    &mut self.draw_text_italic
                } else if style.bold {
                    &mut self.draw_text_bold
                } else {
                    &mut self.draw_text
                };
                drawer.color = fg;
                drawer.draw_abs(cx, pos, &run.text);
                if style.bold {
                    drawer.draw_abs(cx, pos + dvec2(0.5, 0.0), &run.text);
                }
            }
            if style.underline {
                self.fill(cx, Rect { pos: dvec2(pos.x, y + height - 1.0), size: dvec2(run.cells as f64 * width, 1.0) }, fg);
            }
            // SGR 9 (`TextDecoration.lineThrough`) at the middle of the row.
            if style.strike {
                self.fill(cx, Rect { pos: dvec2(pos.x, (y + height * 0.5).floor()), size: dvec2(run.cells as f64 * width, 1.0) }, fg);
            }
            col += run.cells;
        }
        selected
    }

    /// Draws `text` (one grapheme) in `style`'s weight at a cell position with `color`.
    pub fn draw_glyph(&mut self, cx: &mut Cx2d, pos: DVec2, text: &str, style: &CellStyle, color: Vec4) {
        let drawer = if style.italic {
            &mut self.draw_text_italic
        } else if style.bold {
            &mut self.draw_text_bold
        } else {
            &mut self.draw_text
        };
        drawer.color = color;
        drawer.draw_abs(cx, pos, text);
        if style.bold {
            drawer.draw_abs(cx, pos + dvec2(0.5, 0.0), text);
        }
    }
}

/// Terminal cells may share a column (combining marks) or span two columns.
/// Snap only at complete grapheme boundaries, never in a Hangul/emoji cluster.
pub fn grapheme_column(line: &Line, col: usize, next: bool) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    let mut chars = 0;
    let cells: usize = line.runs.iter().map(|run| run.cells).sum();
    for grapheme in line.text.graphemes(true) {
        let start = line.columns.get(chars).copied().unwrap_or(cells);
        chars += grapheme.chars().count();
        let end = line.columns.get(chars).copied().unwrap_or(cells);
        if col < end {
            return if next || col.saturating_sub(start) > end.saturating_sub(col) { end } else { start };
        }
    }
    cells
}

/// The text of the selection `start..end` over `lines` (presented rows from `start.row`),
/// logical lines joined like `renderedText`.
pub fn selected_text(lines: &[&Line], start: CellPos, end: CellPos) -> String {
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        let row = start.row + index;
        if row > end.row {
            break;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { usize::MAX };
        out.extend(line.text.chars().zip(line.columns.iter()).filter(|(_, col)| **col >= from && **col < to).map(|(ch, _)| ch));
        if row != end.row && !line.wraps_next {
            out.push('\n');
        }
    }
    out
}

/// Drag/long-press selection over presented rows, with the platform touch handles and the
/// floating copy actions (Android).
pub struct GridSelection {
    range: Option<(CellPos, CellPos)>,
    pub dragging: bool,
    /// Long-press selection shows draggable handles.
    pub touch: bool,
    pub show_actions: bool,
    action_anchor: Option<Rect>,
    handles: TextSelectionHandles,
}

impl GridSelection {
    pub fn new(cx: &mut Cx) -> Self {
        Self { range: None, dragging: false, touch: false, show_actions: false, action_anchor: None, handles: TextSelectionHandles::new(cx) }
    }

    pub fn range(&self) -> Option<(CellPos, CellPos)> {
        self.range
    }

    pub fn set(&mut self, anchor: CellPos, cursor: CellPos) {
        self.range = Some((anchor, cursor));
    }

    /// Start and end in reading order.
    pub fn ordered(&self) -> Option<(CellPos, CellPos)> {
        self.range.map(|(a, b)| if a <= b { (a, b) } else { (b, a) })
    }

    /// A non-empty range is selected.
    pub fn has_text(&self) -> bool {
        self.range.is_some_and(|(a, b)| a != b)
    }

    /// The selected cells of `row`, a row of `cells` painted cells.
    pub fn span(&self, row: usize, cells: usize) -> Option<(usize, usize)> {
        let (start, end) = self.ordered()?;
        if row < start.row || row > end.row || start == end {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col.min(cells.max(1)) } else { cells.max(1) };
        (to > from).then_some((from, to))
    }

    pub fn clear(&mut self, cx: &mut Cx, owner: Area) {
        self.handles.cancel(cx);
        self.touch = false;
        cx.clear_text_selection(owner);
        self.dragging = false;
        self.show_actions = false;
        self.action_anchor = None;
        self.range = None;
    }

    /// Routes handle drags; returns the action for the owner to scroll and resolve the
    /// dragged endpoint with [`GridSelection::move_endpoint`].
    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, owner: Area) -> Option<TextHandleAction> {
        let action = self.handles.handle_event(cx, event, owner)?;
        self.dragging = false;
        self.show_actions = false;
        Some(action)
    }

    pub fn move_endpoint(&mut self, action: &TextHandleAction, point: Option<CellPos>) {
        if action.phase != TextHandlePhase::Cancel
            && let (Some(point), Some((anchor, cursor))) = (point, self.range)
        {
            self.range = Some(match action.endpoint {
                TextHandleEndpoint::Anchor => (point, cursor),
                TextHandleEndpoint::Cursor | TextHandleEndpoint::Caret => (anchor, point),
            });
        }
        if action.phase == TextHandlePhase::End {
            self.show_actions = self.has_text();
        }
    }

    /// Call before painting rows.
    pub fn begin_frame(&mut self) {
        self.action_anchor = None;
    }

    /// Records the first selection rect visible in `viewport` as the actions anchor.
    pub fn painted(&mut self, rect: Rect, viewport: Rect) {
        let visible = rect.clip((viewport.pos, viewport.pos + viewport.size));
        if self.action_anchor.is_none() && visible.size.x > 0.0 && visible.size.y > 0.0 {
            self.action_anchor = Some(visible);
        }
    }

    /// Draws the touch handles and requests the floating text actions after the rows.
    pub fn draw_overlays(&mut self, cx: &mut Cx2d, owner: Area, origin: DVec2, cell: (f64, f64), paste: bool) {
        let (width, height) = cell;
        let viewport = owner.rect(cx);
        if self.touch && let Some((anchor, cursor)) = self.range {
            let caret = |point: CellPos| Rect { pos: origin + dvec2(point.col as f64 * width, point.row as f64 * height), size: dvec2(1.0, height) };
            self.handles.draw(cx, owner, viewport, caret(anchor), caret(cursor), anchor == cursor, false);
        }
        if !self.handles.is_dragging() && (self.show_actions || cx.text_actions_active(owner)) {
            self.show_actions = false;
            if let Some(anchor) = self.action_anchor {
                cx.show_text_actions(owner, anchor, TextActionCapabilities { copy: true, select_all: true, paste, ..Default::default() });
            } else {
                cx.clear_text_selection(owner);
                cx.claim_text_selection(owner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_terminal::Run;

    fn line(text: &str, columns: &[usize], cells: usize) -> Line {
        Line {
            text: text.to_owned(),
            columns: columns.to_vec(),
            runs: vec![Run { text: text.to_owned(), cells, style: CellStyle::default() }],
            wraps_next: false,
        }
    }

    #[test]
    fn endpoints_skip_wide_hangul_and_combining_cells() {
        let row = line("한e\u{301}🙂", &[0, 2, 2, 3], 5);
        assert_eq!(grapheme_column(&row, 1, false), 0);
        assert_eq!(grapheme_column(&row, 0, true), 2);
        assert_eq!(grapheme_column(&row, 2, true), 3);
        assert_eq!(grapheme_column(&row, 4, false), 3);
        assert_eq!(grapheme_column(&row, usize::MAX, false), 5);
    }

    #[test]
    fn endpoints_keep_joined_emoji_and_code_whitespace() {
        let row = line("  👩\u{200d}💻\t", &[0, 1, 2, 2, 4, 6], 7);
        assert_eq!(grapheme_column(&row, 1, false), 1);
        assert_eq!(grapheme_column(&row, 2, true), 6);
        assert_eq!(grapheme_column(&row, 4, false), 2);
        assert_eq!(grapheme_column(&row, 6, true), 7);
    }

    #[test]
    fn selected_text_joins_soft_wraps_only() {
        let mut first = line("abcd", &[0, 1, 2, 3], 4);
        first.wraps_next = true;
        let second = line("ef", &[0, 1], 2);
        let third = line("gh", &[0, 1], 2);
        let lines = [&first, &second, &third];
        let text = selected_text(&lines, CellPos { row: 0, col: 2 }, CellPos { row: 2, col: 1 });
        assert_eq!(text, "cdef\ng");
    }

    #[test]
    fn colors_follow_bold_brightening_inverse_and_dim() {
        let palette = theme::current_palette();
        let paint = GridPaint::new(&palette);
        let bold = CellStyle { fg: Color::Indexed(1), bold: true, ..CellStyle::default() };
        assert_eq!(paint.colors(&bold).0, paint.ansi[9]);
        let inverse = CellStyle { inverse: true, ..CellStyle::default() };
        assert_eq!(paint.colors(&inverse), (palette.surface0, Some(palette.foreground)));
        let dim = CellStyle { fg: Color::Rgb(255, 0, 0), dim: true, ..CellStyle::default() };
        assert_eq!(paint.colors(&dim).0.w, 0.5);
    }
}
