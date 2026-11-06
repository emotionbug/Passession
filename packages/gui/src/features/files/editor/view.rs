//! The native source editor (Flutter `FilePanel._source` TextField + `_SourceLineNumbers` +
//! `SourceController`): monospace rows with an optional wrap, a line-number gutter, syntax
//! colouring from the worker, selection, IME commits, clipboard, undo/redo and keyboard
//! navigation. Only visible rows are laid out; row geometry is cached per text revision.
//!
//! The editor is not a widget: its owner forwards hits inside the editor rect and draws it into
//! a rect, so one widget area serves the whole file view.

use std::collections::HashMap;
use std::rc::Rc;

use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::*;

use crate::common::paint::Font;
use crate::theme::{Palette, with_alpha};

use super::buffer::{EditKind, Selection, TextBuffer};
use crate::features::files::highlight::{self, Highlighted};
use crate::features::files::kit::{Kit, RichFont, RichSpan, rect, spec};
use makepad_widgets::makepad_draw::event::DigitId;

/// Tab characters display as this many spaces.
const TAB_WIDTH: usize = 4;

/// What drawing needs besides the text.
#[derive(Clone)]
pub struct EditorLook {
    pub font_size: f64,
    pub compact: bool,
    pub wrap: bool,
    pub highlighting: bool,
    pub syntax: [Vec4; 20],
    pub palette: Palette,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EditorResult {
    pub text_changed: bool,
    pub selection_changed: bool,
    /// A secondary click or long press at this window position (context menu).
    pub context_menu: Option<DVec2>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragUnit {
    Char,
    Word,
    Line,
}

#[derive(Clone, Copy, Debug)]
enum Reveal {
    /// Keep the caret inside the viewport.
    Caret,
    /// Centre a line in the viewport (Flutter `_revealLine`).
    CenterLine(usize),
}

#[derive(Default)]
struct Geometry {
    revision: u64,
    cols: usize,
    wrap: bool,
    font_size: f64,
    /// Cumulative visual rows before each line (len = lines + 1).
    prefix: Vec<usize>,
    max_cols: usize,
    /// Row starts (byte offsets within the line) of wrapped lines, filled on demand.
    starts: HashMap<usize, Vec<usize>>,
}

pub struct SourceEditor {
    pub buffer: TextBuffer,
    pub read_only: bool,
    /// File name for language selection.
    pub filename: String,
    /// Highlight worker key (unique per editor instance).
    pub highlight_key: String,
    pub focused: bool,
    pub blink: super::Blink,
    scroll: DVec2,
    viewport: Rect,
    char_width: f64,
    char_width_size: f64,
    geometry: Geometry,
    layouts: HashMap<(usize, usize), Rc<LaidoutText>>,
    layouts_key: (u64, u64, u64, bool, usize, bool),
    /// The owner widget's keyboard focus area, distinct from IME caret geometry.
    ime_area: Area,
    ime_id: u64,
    ime_caret_area: Area,
    preferred_x: Option<f64>,
    drag: Option<(DragUnit, usize, usize)>,
    touch_caret: Option<(DigitId, u64)>,
    thumb_drag: Option<(f64, f64)>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    ime_last: Option<(usize, usize)>,
    show_actions: bool,
    selection_invalidated: bool,
    owns_selection: bool,
    reveal: Option<Reveal>,
    /// The text revision the last highlight request was for.
    highlight_requested: u64,
    pub content_rect: Rect,
}

fn char_cols(c: char) -> usize {
    let code = c as u32;
    let wide = matches!(code,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD);
    if c == '\t' {
        TAB_WIDTH
    } else if wide {
        2
    } else {
        1
    }
}

fn line_cols(line: &str) -> usize {
    if line.is_ascii() && !line.contains('\t') {
        return line.len();
    }
    line.chars().map(char_cols).sum()
}

/// Row starts of `line` wrapped at `cols` columns, breaking after whitespace when possible.
fn wrap_line(line: &str, cols: usize) -> Vec<usize> {
    let mut starts = vec![0];
    if cols == 0 {
        return starts;
    }
    let mut row_cols = 0;
    let mut last_break: Option<usize> = None;
    let mut row_start = 0;
    for (index, c) in line.char_indices() {
        let width = char_cols(c);
        if row_cols + width > cols && index > row_start {
            let start = match last_break {
                Some(brk) if brk > row_start => brk,
                _ => index,
            };
            starts.push(start);
            row_start = start;
            row_cols = line_cols(&line[start..index]);
            last_break = None;
        }
        row_cols += width;
        if c == ' ' || c == '\t' {
            last_break = Some(index + c.len_utf8());
        }
    }
    starts
}

/// Display text of a row: tabs expanded. Returns the text and the byte positions in the
/// display string of each source char boundary (`map[i]` for source byte `i` of the row).
fn display(row: &str) -> (String, Vec<(usize, usize)>) {
    if !row.contains('\t') {
        return (row.to_owned(), Vec::new());
    }
    let mut text = String::with_capacity(row.len() + 8);
    let mut map = Vec::new();
    for (index, c) in row.char_indices() {
        map.push((index, text.len()));
        if c == '\t' {
            text.push_str(&" ".repeat(TAB_WIDTH));
        } else {
            text.push(c);
        }
    }
    map.push((row.len(), text.len()));
    (text, map)
}

fn to_display(map: &[(usize, usize)], source: usize) -> usize {
    if map.is_empty() {
        return source;
    }
    map.iter().find(|(s, _)| *s >= source).map(|(_, d)| *d).unwrap_or_else(|| map.last().unwrap().1)
}

fn to_source(map: &[(usize, usize)], display: usize) -> usize {
    if map.is_empty() {
        return display;
    }
    let mut best = 0;
    for (source, d) in map {
        if *d <= display {
            best = *source;
        } else {
            break;
        }
    }
    best
}

impl SourceEditor {
    pub fn new(text: String, filename: &str, highlight_key: String, read_only: bool) -> SourceEditor {
        SourceEditor {
            buffer: TextBuffer::new(text),
            read_only,
            filename: filename.to_owned(),
            highlight_key,
            focused: false,
            blink: super::Blink::default(),
            scroll: DVec2::default(),
            viewport: Rect::default(),
            char_width: 0.0,
            char_width_size: 0.0,
            geometry: Geometry::default(),
            layouts: HashMap::new(),
            layouts_key: (u64::MAX, 0, 0, false, 0, false),
            ime_area: Area::Empty,
            ime_id: LiveId::unique().0,
            ime_caret_area: Area::Empty,
            preferred_x: None,
            drag: None,
            touch_caret: None,
            thumb_drag: None,
            touch_scroll: Default::default(),
            ime_last: None,
            show_actions: false,
            selection_invalidated: false,
            owns_selection: false,
            reveal: None,
            highlight_requested: u64::MAX,
            content_rect: Rect::default(),
        }
    }

    pub fn text(&self) -> &str {
        self.buffer.text()
    }

    /// Replaces the text from the model (reload, remote change), invalidating
    /// selection ownership; no-op when equal.
    pub fn sync_text(&mut self, text: &str) {
        if self.buffer.text() != text {
            self.buffer.set_text(text.to_owned());
            self.ime_last = None;
            self.buffer.set_selection(Selection::caret(self.buffer.selection.head));
            self.drag = None;
            self.touch_caret = None;
            self.show_actions = false;
            self.selection_invalidated = true;
        }
    }

    pub fn clear_selection(&mut self, cx: &mut Cx) {
        if self.owns_selection {
            cx.clear_text_selection(self.ime_area);
        }
        self.owns_selection = false;
        self.buffer.set_selection(Selection::caret(self.buffer.selection.head));
        self.drag = None;
        self.touch_caret = None;
        self.thumb_drag = None;
        self.touch_scroll.reset();
        self.show_actions = false;
        self.selection_invalidated = false;
    }

    fn text_action_capabilities(&self) -> TextActionCapabilities {
        let selected = !self.buffer.selection.is_empty();
        TextActionCapabilities {
            copy: selected,
            cut: selected && !self.read_only,
            paste: !self.read_only,
            select_all: !self.buffer.is_empty(),
        }
    }

    pub fn location(&self) -> (usize, usize) {
        self.buffer.location(self.buffer.selection.head)
    }

    pub fn line_count(&self) -> usize {
        self.buffer.line_count()
    }

    /// Original `_goToLocation`: selects the line start (or the `line..line_end` range) and
    /// centres the line.
    pub fn go_to(&mut self, line: usize, column: usize, line_end: Option<usize>) {
        self.touch_caret = None;
        let line = line.max(1);
        if !self.read_only {
            match line_end {
                Some(end) if end > line => {
                    let start = self.buffer.offset_at(line, 1);
                    let end = self.buffer.offset_at(end, usize::MAX);
                    self.buffer.set_selection(Selection { anchor: start, head: end });
                }
                _ => {
                    let offset = self.buffer.offset_at(line, column.max(1));
                    self.buffer.set_selection(Selection::caret(offset));
                }
            }
        }
        self.reveal = Some(Reveal::CenterLine(line - 1));
    }

    pub fn reveal_caret(&mut self) {
        self.reveal = Some(Reveal::Caret);
    }

    /// Selects `start..end` and reveals it (find).
    pub fn select(&mut self, start: usize, end: usize) {
        self.touch_caret = None;
        self.buffer.set_selection(Selection { anchor: start, head: end });
        self.reveal = Some(Reveal::Caret);
    }

    fn line_height(&self, look: &EditorLook) -> f64 {
        (look.font_size * 1.45).round()
    }

    fn padding(look: &EditorLook) -> (f64, f64, f64, f64) {
        if look.compact { (0.0, 0.0, 0.0, 0.0) } else { (6.0, 16.0, 2.0, 16.0) }
    }

    fn gutter(&self, look: &EditorLook) -> f64 {
        if look.compact {
            return 56.0;
        }
        let digits = self.buffer.line_count().to_string().len() as f64;
        (digits * self.char_width).max(20.0).ceil() + 8.0 + 1.0
    }

    fn ensure_char_width(&mut self, cx: &mut Cx, kit: &mut Kit, size: f64) {
        if self.char_width_size != size || self.char_width <= 0.0 {
            let width = kit.measure(cx, spec(Font::Mono, size, Vec4::default()), "0000000000").x / 10.0;
            self.char_width = width.max(1.0);
            self.char_width_size = size;
        }
    }

    fn ensure_geometry(&mut self, look: &EditorLook, text_width: f64) {
        let cols = if look.wrap { ((text_width / self.char_width).floor() as usize).max(1) } else { 0 };
        let g = &self.geometry;
        if g.revision == self.buffer.revision
            && g.cols == cols
            && g.wrap == look.wrap
            && g.font_size == look.font_size
            && !g.prefix.is_empty()
        {
            return;
        }
        let mut prefix = Vec::with_capacity(self.buffer.line_count() + 1);
        let mut total = 0usize;
        let mut max_cols = 0usize;
        for line in 0..self.buffer.line_count() {
            prefix.push(total);
            let text = self.buffer.line(line);
            let width = line_cols(text);
            max_cols = max_cols.max(width);
            total += if look.wrap && width > cols { wrap_line(text, cols).len() } else { 1 };
        }
        prefix.push(total);
        self.geometry = Geometry {
            revision: self.buffer.revision,
            cols,
            wrap: look.wrap,
            font_size: look.font_size,
            prefix,
            max_cols,
            starts: HashMap::new(),
        };
    }

    fn total_rows(&self) -> usize {
        *self.geometry.prefix.last().unwrap_or(&1)
    }

    fn row_starts(&mut self, line: usize) -> Vec<usize> {
        let rows = self.geometry.prefix[line + 1] - self.geometry.prefix[line];
        if rows <= 1 {
            return vec![0];
        }
        if let Some(starts) = self.geometry.starts.get(&line) {
            return starts.clone();
        }
        let starts = wrap_line(self.buffer.line(line), self.geometry.cols);
        self.geometry.starts.insert(line, starts.clone());
        starts
    }

    /// (line, row within line) of a visual row.
    fn row_to_line(&self, row: usize) -> (usize, usize) {
        let prefix = &self.geometry.prefix;
        let line = match prefix.binary_search(&row) {
            Ok(mut line) => {
                // Skip lines with zero rows never happen (each line has ≥ 1 row).
                while line + 1 < prefix.len() - 1 && prefix[line + 1] == row {
                    line += 1;
                }
                line
            }
            Err(next) => next - 1,
        };
        let line = line.min(self.buffer.line_count() - 1);
        (line, row - prefix[line])
    }

    /// Byte range (absolute) of a visual row.
    fn row_range(&mut self, line: usize, row: usize) -> (usize, usize) {
        let starts = self.row_starts(line);
        let base = self.buffer.line_start(line);
        let start = base + starts[row.min(starts.len() - 1)];
        let end = match starts.get(row + 1) {
            Some(next) => base + next,
            None => self.buffer.line_end(line),
        };
        (start, end)
    }

    /// The visual row of an absolute offset.
    fn row_of(&mut self, offset: usize) -> usize {
        let line = self.buffer.line_of(offset);
        let starts = self.row_starts(line);
        let within = offset - self.buffer.line_start(line);
        let mut row = 0;
        for (index, start) in starts.iter().enumerate() {
            if *start <= within {
                row = index;
            }
        }
        self.geometry.prefix[line] + row
    }

    fn layout_row(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, line: usize, row: usize, highlighted: Option<&Highlighted>) -> (Rc<LaidoutText>, Vec<(usize, usize)>, usize) {
        let (start, end) = self.row_range(line, row);
        let row_text = self.buffer.text()[start..end].to_owned();
        let (display_text, map) = display(&row_text);
        if let Some(laid) = self.layouts.get(&(line, row)) {
            return (laid.clone(), map, start);
        }
        let fg = look.palette.foreground;
        let mut spans = Vec::new();
        let line_start = self.buffer.line_start(line);
        let tokens = if look.highlighting {
            highlighted.and_then(|h| h.line(line, self.buffer.line(line)))
        } else {
            None
        };
        match tokens {
            Some(tokens) => {
                // Cut the line's token runs to this row, then to display bytes.
                let mut position = line_start;
                for (len, style) in tokens {
                    let (a, b) = (position.max(start), (position + len).min(end));
                    position += len;
                    if a >= b {
                        continue;
                    }
                    let da = to_display(&map, a - start);
                    let db = to_display(&map, b - start);
                    let color = style.map(|s| look.syntax[s as usize]).unwrap_or(fg);
                    spans.push(RichSpan { len: db - da, font: RichFont::Mono, size: look.font_size, color });
                }
                let covered: usize = spans.iter().map(|s| s.len).sum();
                if covered < display_text.len() {
                    spans.push(RichSpan { len: display_text.len() - covered, font: RichFont::Mono, size: look.font_size, color: fg });
                }
            }
            None => spans.push(RichSpan { len: display_text.len(), font: RichFont::Mono, size: look.font_size, color: fg }),
        }
        if display_text.is_empty() {
            spans.clear();
            spans.push(RichSpan { len: 0, font: RichFont::Mono, size: look.font_size, color: fg });
        }
        let laid = kit.layout_rich(cx, &display_text, &spans, None, 1.0);
        self.layouts.insert((line, row), laid.clone());
        (laid, map, start)
    }

    fn x_in_row(laid: &LaidoutText, map: &[(usize, usize)], within: usize) -> f64 {
        let display = to_display(map, within);
        laid.rows.first().map(|row| row.index_to_x_in_lpxs(display.min(row.text.len())) as f64).unwrap_or(0.0)
    }

    fn index_at_x(laid: &LaidoutText, map: &[(usize, usize)], x: f64) -> usize {
        let display = laid.rows.first().map(|row| row.x_in_lpxs_to_index(x.max(0.0) as f32)).unwrap_or(0);
        to_source(map, display)
    }

    /// Offset under a window position (clamped into the text).
    fn offset_at_point(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, abs: DVec2) -> usize {
        let line_height = self.line_height(look);
        let (_, top, _, _) = Self::padding(look);
        let rel_y = abs.y - self.content_rect.pos.y + self.scroll.y - top;
        let total = self.total_rows();
        let row = if rel_y < 0.0 { 0 } else { ((rel_y / line_height) as usize).min(total.saturating_sub(1)) };
        let (line, within_row) = self.row_to_line(row);
        let highlighted = highlight::result(&self.highlight_key);
        let (laid, map, start) = self.layout_row(cx, kit, look, line, within_row, highlighted.as_deref());
        let rel_x = abs.x - self.content_rect.pos.x + self.scroll.x;
        let index = Self::index_at_x(&laid, &map, rel_x);
        let (_, end) = self.row_range(line, within_row);
        (start + index).min(end)
    }

    fn caret_point(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, offset: usize) -> DVec2 {
        let line_height = self.line_height(look);
        let row = self.row_of(offset);
        let (line, within_row) = self.row_to_line(row);
        let highlighted = highlight::result(&self.highlight_key);
        let (laid, map, start) = self.layout_row(cx, kit, look, line, within_row, highlighted.as_deref());
        dvec2(Self::x_in_row(&laid, &map, offset - start), row as f64 * line_height)
    }

    fn scroll_bounds(&self, look: &EditorLook) -> DVec2 {
        let (_, top, _, bottom) = Self::padding(look);
        let content_h = top + self.total_rows() as f64 * self.line_height(look) + bottom;
        let content_w = self.geometry.max_cols as f64 * self.char_width + 2.0;
        dvec2(
            if look.wrap { 0.0 } else { (content_w - self.content_rect.size.x).max(0.0) },
            (content_h - self.content_rect.size.y).max(0.0),
        )
    }

    fn clamp_scroll(&mut self, look: &EditorLook) {
        let max = self.scroll_bounds(look);
        self.scroll.x = self.scroll.x.clamp(0.0, max.x);
        self.scroll.y = self.scroll.y.clamp(0.0, max.y);
    }

    /// Draws the editor into `area`. The caret does not blink, so an idle editor draws nothing.
    /// `ime_area` owns keyboard focus; the caret has separate IME avoidance geometry.
    pub fn draw(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, look: &EditorLook, ime_area: Area) {
        if self.selection_invalidated {
            if self.owns_selection {
                cx.clear_text_selection(self.ime_area);
            }
            self.owns_selection = false;
            self.selection_invalidated = false;
        }
        self.ime_area = ime_area;
        let p = look.palette;
        self.ensure_char_width(cx, kit, look.font_size);
        let gutter = self.gutter(look);
        let (left, top, right, bottom) = Self::padding(look);
        let content = rect(area.pos.x + gutter + left, area.pos.y, area.size.x - gutter - left - right, area.size.y);
        self.content_rect = content;
        self.viewport = area;
        self.ensure_geometry(look, content.size.x);
        // Highlight the newest text (worker; debounced by one in-flight job per editor).
        if look.highlighting && self.highlight_requested != self.buffer.revision {
            self.highlight_requested = self.buffer.revision;
            highlight::request(&self.highlight_key, self.buffer.revision, self.buffer.text(), &self.filename);
        }
        let highlighted = highlight::result(&self.highlight_key);
        let style_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            look.font_size.to_bits().hash(&mut hasher);
            for color in look.syntax.iter().chain(std::iter::once(&p.foreground)) {
                for channel in [color.x, color.y, color.z, color.w] {
                    channel.to_bits().hash(&mut hasher);
                }
            }
            hasher.finish()
        };
        let key = (
            self.buffer.revision,
            highlighted.as_ref().map(|h| h.revision).unwrap_or(u64::MAX),
            style_key,
            look.highlighting,
            self.geometry.cols,
            look.wrap,
        );
        if key != self.layouts_key {
            self.layouts.clear();
            self.layouts_key = key;
        }
        let line_height = self.line_height(look);
        // Pending reveal.
        if let Some(reveal) = self.reveal.take() {
            match reveal {
                Reveal::Caret => {
                    let point = self.caret_point(cx, kit, look, self.buffer.selection.head);
                    let y = point.y + top;
                    if y < self.scroll.y {
                        self.scroll.y = y;
                    } else if y + line_height > self.scroll.y + content.size.y {
                        self.scroll.y = y + line_height - content.size.y;
                    }
                    if !look.wrap {
                        if point.x < self.scroll.x {
                            self.scroll.x = (point.x - 20.0).max(0.0);
                        } else if point.x > self.scroll.x + content.size.x - 4.0 {
                            self.scroll.x = point.x - content.size.x + 24.0;
                        }
                    }
                }
                Reveal::CenterLine(line) => {
                    let line = line.min(self.buffer.line_count() - 1);
                    let y = top + self.geometry.prefix[line] as f64 * line_height;
                    self.scroll.y = y + line_height / 2.0 - content.size.y / 2.0;
                }
            }
        }
        self.clamp_scroll(look);

        cx.begin_turtle(Walk::abs_rect(area), Layout::default());
        let total = self.total_rows();
        let first = (((self.scroll.y - top) / line_height).floor().max(0.0)) as usize;
        let visible = (content.size.y / line_height).ceil() as usize + 2;
        let last = (first + visible).min(total);
        let sel = self.buffer.selection;
        let selection_color = with_alpha(p.accent, 0.35);
        let mut caret_rect = None;
        let mut action_anchor = None;
        let active_line = self.buffer.line_of(sel.head);
        let mut numbers: Vec<(usize, f64)> = Vec::new();
        for row in first..last {
            let (line, within_row) = self.row_to_line(row);
            let y = content.pos.y + top + row as f64 * line_height - self.scroll.y;
            let (laid, map, start) = self.layout_row(cx, kit, look, line, within_row, highlighted.as_deref());
            let (_, end) = self.row_range(line, within_row);
            let x0 = content.pos.x - self.scroll.x;
            // Selection under the text.
            let (a, b) = (sel.start(), sel.end());
            if !(sel.is_empty() || b < start || a > end || b == start) {
                let (ra, rb) = (a.max(start), b.min(end));
                let xa = Self::x_in_row(&laid, &map, ra - start);
                let mut xb = Self::x_in_row(&laid, &map, rb - start);
                // A selected line break shows as a char-wide tail.
                if b > end && end == self.buffer.line_end(line) {
                    xb += self.char_width * 0.5;
                }
                if xb > xa {
                    kit.fill(cx, rect(x0 + xa, y, xb - xa, line_height), selection_color, 0.0);
                    let visible = rect(x0 + xa, y, xb - xa, line_height).clip((content.pos, content.pos + content.size));
                    if action_anchor.is_none() && visible.size.x > 0.0 && visible.size.y > 0.0 {
                        action_anchor = Some(visible);
                    }
                }
            }
            let text_h = laid.size_in_lpxs.height as f64;
            kit.draw_rich(cx, dvec2(x0, y + (line_height - text_h) / 2.0), &laid);
            if within_row == 0 {
                numbers.push((line, y));
            }
            if self.focused && !self.read_only {
                let head = sel.head;
                let at_row_end = head == end && (end == self.buffer.line_end(line));
                if head >= start && (head < end || at_row_end) {
                    let x = Self::x_in_row(&laid, &map, head - start);
                    caret_rect = Some(rect(x0 + x, y, 2.0, line_height));
                }
            }
        }
        if caret_rect.is_some() {
            self.blink.arm(cx);
        } else {
            self.blink.disarm(cx);
        }
        if let Some(caret) = caret_rect {
            if self.blink.visible() {
                kit.fill(cx, caret, p.foreground, 0.0);
            }
            // Keep KeyboardView avoidance local to the caret, not the entire pane.
            cx.add_aligned_rect_area(&mut self.ime_caret_area, caret);
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                self.buffer.selection.anchor, self.buffer.selection.head, false, false);
            cx.show_text_ime(self.ime_caret_area, dvec2(0.0, caret.size.y));
        }
        if self.focused && (self.show_actions || (self.owns_selection && cx.text_actions_active(self.ime_area))) {
            self.show_actions = false;
            let anchor = action_anchor.or(caret_rect).map(|r| r.clip((content.pos, content.pos + content.size)));
            if let Some(anchor) = anchor.filter(|r| r.size.x > 0.0 && r.size.y > 0.0) {
                cx.show_text_actions(self.ime_area, anchor, self.text_action_capabilities());
            } else {
                cx.clear_text_selection(self.ime_area);
                cx.claim_text_selection(self.ime_area);
            }
        }
        // Gutter over horizontally scrolled text.
        kit.fill(cx, rect(area.pos.x, area.pos.y, gutter, area.size.y), p.surface0, 0.0);
        if !look.compact {
            kit.vline(cx, area.pos.x + gutter - 1.0, area.pos.y, area.size.y, p.border);
        }
        let number_padding = if look.compact { 12.0 } else { 3.0 };
        let right_edge = area.pos.x + gutter - if look.compact { 0.0 } else { 1.0 } - number_padding;
        for (line, y) in numbers {
            let color = if !look.compact && line == active_line { p.foreground } else { p.muted };
            let label = (line + 1).to_string();
            let s = spec(Font::Mono, look.font_size, color);
            let width = kit.measure(cx, s, &label).x;
            kit.text_in(cx, rect(right_edge - width, y, width + 1.0, line_height), s, &label, 0.0);
        }
        // Scroll thumb.
        let content_h = top + total as f64 * line_height + bottom;
        if content_h > area.size.y + 1.0 {
            let ratio = area.size.y / content_h;
            let thumb_h = (area.size.y * ratio).max(24.0);
            let max_scroll = content_h - area.size.y;
            let thumb_y = area.pos.y + (area.size.y - thumb_h) * (self.scroll.y / max_scroll).clamp(0.0, 1.0);
            kit.fill(cx, rect(area.pos.x + area.size.x - 6.0, thumb_y, 4.0, thumb_h), with_alpha(p.muted, 0.35), 2.0);
        }
        cx.end_turtle();
    }

    pub fn scroll_by(&mut self, look: &EditorLook, delta: DVec2) {
        let max = self.scroll_bounds(look);
        self.scroll.x = (self.scroll.x + delta.x).clamp(0.0, max.x);
        self.touch_scroll.apply_delta(&mut self.scroll.y, delta.y, max.y);
    }

    pub fn animate_scroll(&mut self, cx: &mut Cx, event: &Event, look: &EditorLook, enabled: bool) -> bool {
        if !enabled {
            self.touch_scroll.reset();
            return false;
        }
        let Some(delta) = self.touch_scroll.frame(cx, event) else { return false };
        let before = self.scroll;
        self.scroll_by(look, dvec2(0.0, delta));
        self.scroll != before
    }

    /// The vertical thumb rect (for drags).
    fn thumb(&self, look: &EditorLook) -> Option<(Rect, f64)> {
        let (_, top, _, bottom) = Self::padding(look);
        let area = self.viewport;
        let content_h = top + self.total_rows() as f64 * self.line_height(look) + bottom;
        if content_h <= area.size.y + 1.0 {
            return None;
        }
        let thumb_h = (area.size.y * area.size.y / content_h).max(24.0);
        let max_scroll = content_h - area.size.y;
        let thumb_y = area.pos.y + (area.size.y - thumb_h) * (self.scroll.y / max_scroll).clamp(0.0, 1.0);
        Some((rect(area.pos.x + area.size.x - 10.0, thumb_y, 10.0, thumb_h), max_scroll / (area.size.y - thumb_h).max(1.0)))
    }

    pub fn cancel_touch_focus(&mut self) {
        self.touch_caret = None;
    }

    pub fn wants_touch_focus(&self, hit: &Hit) -> bool {
        match hit {
            Hit::FingerUp(fe) => self.touch_caret == Some((fe.digit_id, self.buffer.revision))
                && fe.is_over && fe.was_tap() && self.viewport.contains(fe.abs),
            Hit::FingerLongPress(fe) => self.touch_caret == Some((fe.digit_id, self.buffer.revision))
                && self.viewport.contains(fe.abs),
            _ => false,
        }
    }

    pub fn observe_event(&mut self, event: &Event) {
        if matches!(event, Event::KeyFocus(_)) {
            self.touch_caret = None;
        }
        if matches!(event, Event::TouchUpdate(update) if update.cancelled) {
            self.touch_caret = None;
            self.drag = None;
            self.thumb_drag = None;
            self.touch_scroll.reset();
            self.show_actions = false;
        }
    }

    /// Pointer and keyboard input already routed to the editor.
    pub fn handle_hit(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, hit: &Hit) -> EditorResult {
        if self.selection_invalidated {
            if self.owns_selection {
                cx.clear_text_selection(self.ime_area);
            }
            self.owns_selection = false;
            self.selection_invalidated = false;
        }
        let mut result = EditorResult::default();
        let before = (self.buffer.revision, self.buffer.selection);
        match hit {
            Hit::FingerDown(fe) => {
                self.touch_scroll.reset();
                self.touch_caret = None;
                if fe.device.mouse_button().is_some_and(|b| b.is_secondary()) {
                    result.context_menu = Some(fe.abs);
                    return result;
                }
                if let Some((thumb, ratio)) = self.thumb(look)
                    && thumb.contains(fe.abs) {
                        self.thumb_drag = Some((fe.abs.y, self.scroll.y));
                        let _ = ratio;
                        return result;
                    }
                self.touch_scroll.handle_hit(hit);
                if fe.device.is_touch() {
                    self.touch_caret = Some((fe.digit_id, self.buffer.revision));
                    self.drag = None;
                    return result;
                }
                cx.claim_text_selection(self.ime_area);
                self.owns_selection = true;
                let offset = self.offset_at_point(cx, kit, look, fe.abs);
                self.preferred_x = None;
                self.ime_last = None;
                match fe.tap_count {
                    2 => {
                        let (a, b) = self.buffer.word_at(offset);
                        self.buffer.set_selection(Selection { anchor: a, head: b });
                        self.drag = Some((DragUnit::Word, a, b));
                    }
                    3 => {
                        let (a, b) = self.buffer.line_range_at(offset);
                        self.buffer.set_selection(Selection { anchor: a, head: b });
                        self.drag = Some((DragUnit::Line, a, b));
                    }
                    _ => {
                        self.buffer.move_head(offset, fe.modifiers.shift);
                        self.drag = Some((DragUnit::Char, self.buffer.selection.anchor, self.buffer.selection.anchor));
                    }
                }
            }
            Hit::FingerMove(fe) => {
                if fe.device.is_touch() && (!fe.is_over || !self.viewport.contains(fe.abs)) {
                    self.touch_caret = None;
                }
                if let Some((start_y, start_scroll)) = self.thumb_drag {
                    if let Some((_, ratio)) = self.thumb(look) {
                        self.scroll.y = start_scroll + (fe.abs.y - start_y) * ratio;
                        self.clamp_scroll(look);
                    }
                    return result;
                }
                if let Some(dy) = self.touch_scroll.handle_hit(hit) {
                    self.touch_caret = None;
                    self.scroll_by(look, dvec2(0.0, dy));
                    return result;
                }
                if let Some((unit, a, b)) = self.drag {
                    // Dragging beyond the viewport scrolls it.
                    let line_height = self.line_height(look);
                    if fe.abs.y < self.content_rect.pos.y {
                        self.scroll_by(look, dvec2(0.0, -line_height));
                    } else if fe.abs.y > self.content_rect.pos.y + self.content_rect.size.y {
                        self.scroll_by(look, dvec2(0.0, line_height));
                    }
                    let offset = self.offset_at_point(cx, kit, look, fe.abs);
                    let selection = match unit {
                        DragUnit::Char => Selection { anchor: a, head: offset },
                        DragUnit::Word | DragUnit::Line => {
                            let (wa, wb) = if unit == DragUnit::Word { self.buffer.word_at(offset) } else { self.buffer.line_range_at(offset) };
                            if offset < a { Selection { anchor: b, head: wa } } else { Selection { anchor: a, head: wb.max(b) } }
                        }
                    };
                    self.buffer.set_selection(selection);
                }
            }
            Hit::FingerUp(fe) => {
                self.touch_scroll.release(fe.time);
                if self.wants_touch_focus(hit) {
                    let offset = self.offset_at_point(cx, kit, look, fe.abs);
                    cx.clear_text_selection(self.ime_area);
                    cx.claim_text_selection(self.ime_area);
                    self.owns_selection = true;
                    self.show_actions = false;
                    self.preferred_x = None;
                    self.ime_last = None;
                    let selection = match fe.tap_count {
                        2 => {
                            let (anchor, head) = self.buffer.word_at(offset);
                            Selection { anchor, head }
                        }
                        3 => {
                            let (anchor, head) = self.buffer.line_range_at(offset);
                            Selection { anchor, head }
                        }
                        _ => Selection::caret(offset),
                    };
                    self.buffer.set_selection(selection);
                    if cfg!(target_os = "android") && !selection.is_empty() {
                        self.show_actions = true;
                    }
                }
                self.touch_caret = None;
                if cfg!(target_os = "android") && fe.device.is_touch() && self.drag.is_some() {
                    self.show_actions = true;
                }
                self.drag = None;
                self.thumb_drag = None;
            }
            Hit::FingerLongPress(fe) if self.wants_touch_focus(hit) => {
                self.touch_scroll.reset();
                self.touch_caret = None;
                cx.claim_text_selection(self.ime_area);
                self.owns_selection = true;
                let offset = self.offset_at_point(cx, kit, look, fe.abs);
                let (a, b) = self.buffer.word_at(offset);
                self.buffer.set_selection(Selection { anchor: a, head: b });
                self.drag = Some((DragUnit::Word, a, b));
                self.show_actions = cfg!(target_os = "android");
                if !cfg!(target_os = "android") {
                    result.context_menu = Some(fe.abs);
                }
            }
            Hit::FingerScroll(fe) => {
                self.touch_scroll.reset();
                let mut delta = fe.scroll;
                if fe.modifiers.shift && delta.x == 0.0 {
                    delta = dvec2(delta.y, 0.0);
                }
                self.scroll_by(look, delta);
            }
            Hit::KeyDown(ke) => {
                self.handle_key(cx, kit, look, ke);
            }
            Hit::TextIme(edit) if !self.read_only => {
                self.buffer.apply_ime(edit, false);
                self.ime_last = None;
                self.preferred_x = None;
                self.reveal = Some(Reveal::Caret);
            }
            Hit::TextInput(te) => {
                if !self.read_only && !te.input.is_empty() {
                    let text = te.input.replace("\r\n", "\n").replace('\r', "\n");
                    let replaced = if te.replace_last { self.ime_last } else { None };
                    let start = match replaced {
                        Some((a, b)) => {
                            self.buffer.replace(a, b, &text, EditKind::Other);
                            a
                        }
                        None => {
                            let start = self.buffer.selection.start();
                            let kind = if te.was_paste || text.chars().count() > 1 { EditKind::Other } else { EditKind::Insert };
                            self.buffer.insert(&text, kind);
                            start
                        }
                    };
                    self.ime_last = Some((start, start + text.len()));
                    self.preferred_x = None;
                    self.reveal = Some(Reveal::Caret);
                }
            }
            Hit::TextCopy(ce) => {
                *ce.response.borrow_mut() = Some(self.buffer.selected_text().to_owned());
            }
            Hit::TextCut(ce) => {
                *ce.response.borrow_mut() = Some(self.buffer.selected_text().to_owned());
                if !self.read_only && !self.buffer.selection.is_empty() {
                    self.buffer.insert("", EditKind::Other);
                }
            }
            Hit::KeyFocusLost(_) => {
                self.focused = false;
                self.clear_selection(cx);
            }
            _ => {}
        }
        result.text_changed = self.buffer.revision != before.0;
        result.selection_changed = self.buffer.selection != before.1;
        if result.selection_changed && !self.buffer.selection.is_empty() {
            cx.claim_text_selection(self.ime_area);
            self.owns_selection = true;
        }
        if result.text_changed || result.selection_changed {
            self.blink.restart(cx);
        }
        if self.focused && !self.read_only {
            cx.update_text_input(self.ime_area, self.ime_id, self.buffer.text(),
                self.buffer.selection.anchor, self.buffer.selection.head, false, false);
        }
        result
    }

    fn row_move(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, rows: isize, extend: bool) {
        let head = self.buffer.selection.head;
        let point = self.caret_point(cx, kit, look, head);
        let x = *self.preferred_x.get_or_insert(point.x);
        let row = self.row_of(head) as isize + rows;
        let total = self.total_rows() as isize;
        let target = if row < 0 {
            0
        } else if row >= total {
            self.buffer.len()
        } else {
            let (line, within) = self.row_to_line(row as usize);
            let highlighted = highlight::result(&self.highlight_key);
            let (laid, map, start) = self.layout_row(cx, kit, look, line, within, highlighted.as_deref());
            let (_, end) = self.row_range(line, within);
            (start + Self::index_at_x(&laid, &map, x)).min(end)
        };
        let preferred = self.preferred_x;
        self.buffer.move_head(target, extend);
        self.preferred_x = preferred;
    }

    fn handle_key(&mut self, cx: &mut Cx, kit: &mut Kit, look: &EditorLook, ke: &KeyEvent) {
        let m = ke.modifiers;
        let primary = m.is_primary();
        let word = if cfg!(target_vendor = "apple") { m.alt } else { m.control };
        let line_jump = cfg!(target_vendor = "apple") && m.logo;
        let extend = m.shift;
        let head = self.buffer.selection.head;
        let editable = !self.read_only;
        let mut keep_preferred = false;
        match ke.key_code {
            KeyCode::ArrowLeft => {
                let sel = self.buffer.selection;
                let target = if !extend && !sel.is_empty() && !word && !line_jump {
                    sel.start()
                } else if line_jump {
                    self.buffer.line_start(self.buffer.line_of(head))
                } else if word {
                    self.buffer.word_left(head)
                } else {
                    self.buffer.prev(head)
                };
                self.buffer.move_head(target, extend);
            }
            KeyCode::ArrowRight => {
                let sel = self.buffer.selection;
                let target = if !extend && !sel.is_empty() && !word && !line_jump {
                    sel.end()
                } else if line_jump {
                    self.buffer.line_end(self.buffer.line_of(head))
                } else if word {
                    self.buffer.word_right(head)
                } else {
                    self.buffer.next(head)
                };
                self.buffer.move_head(target, extend);
            }
            KeyCode::ArrowUp => {
                if line_jump {
                    self.buffer.move_head(0, extend);
                } else {
                    self.row_move(cx, kit, look, -1, extend);
                    keep_preferred = true;
                }
            }
            KeyCode::ArrowDown => {
                if line_jump {
                    let len = self.buffer.len();
                    self.buffer.move_head(len, extend);
                } else {
                    self.row_move(cx, kit, look, 1, extend);
                    keep_preferred = true;
                }
            }
            KeyCode::Home => {
                let target = if primary { 0 } else { self.buffer.line_start(self.buffer.line_of(head)) };
                self.buffer.move_head(target, extend);
            }
            KeyCode::End => {
                let target = if primary { self.buffer.len() } else { self.buffer.line_end(self.buffer.line_of(head)) };
                self.buffer.move_head(target, extend);
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let rows = (self.content_rect.size.y / self.line_height(look)).floor().max(1.0) as isize;
                let rows = if ke.key_code == KeyCode::PageUp { -rows } else { rows };
                self.scroll_by(look, dvec2(0.0, rows as f64 * self.line_height(look)));
                self.row_move(cx, kit, look, rows, extend);
                keep_preferred = true;
            }
            KeyCode::KeyA if primary => {
                let len = self.buffer.len();
                self.buffer.set_selection(Selection { anchor: 0, head: len });
                return;
            }
            KeyCode::KeyZ if primary && editable => {
                if m.shift { self.buffer.redo(); } else { self.buffer.undo(); }
            }
            KeyCode::KeyY if primary && editable && !cfg!(target_vendor = "apple") => {
                self.buffer.redo();
            }
            KeyCode::Backspace if editable => self.buffer.backspace(word),
            KeyCode::Delete if editable => self.buffer.delete(word),
            KeyCode::ReturnKey | KeyCode::NumpadEnter if editable && !primary => {
                self.buffer.insert("\n", EditKind::Other);
            }
            KeyCode::Tab if editable && !primary && !m.shift => {
                self.buffer.insert("\t", EditKind::Insert);
            }
            _ => return,
        }
        if !keep_preferred {
            self.preferred_x = None;
        }
        self.ime_last = None;
        self.reveal = Some(Reveal::Caret);
    }

    /// Inserts pasted text from the context menu.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.read_only || text.is_empty() {
            return false;
        }
        self.buffer.insert(&text.replace("\r\n", "\n"), EditKind::Other);
        self.reveal = Some(Reveal::Caret);
        true
    }

    /// Cuts the selection; returns the removed text.
    pub fn cut(&mut self) -> Option<String> {
        if self.read_only || self.buffer.selection.is_empty() {
            return None;
        }
        let text = self.buffer.selected_text().to_owned();
        self.buffer.insert("", EditKind::Other);
        Some(text)
    }

    pub fn select_all(&mut self) {
        self.touch_caret = None;
        let len = self.buffer.len();
        self.buffer.set_selection(Selection { anchor: 0, head: len });
    }

    pub fn forget(&self) {
        highlight::forget(&self.highlight_key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_touch_does_not_focus_or_move_selection_before_release() {
        use super::super::field::touch_tests::{cancel, down, moved_out, up};
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut kit = Kit::new(&mut cx);
        let look = EditorLook {
            font_size: 14.0, compact: true, wrap: false, highlighting: false,
            syntax: [Vec4::default(); 20], palette: crate::theme::current_palette(),
        };
        for transition in 0..8 {
            let mut editor = SourceEditor::new("한글 text".into(), "test.txt", "touch".into(), false);
            editor.viewport = rect(0.0, 0.0, 100.0, 100.0);
            editor.select(0, "한글".len());
            let selected = editor.buffer.selection;
            editor.handle_hit(&mut cx, &mut kit, &look, &down());
            assert!(!editor.focused);
            assert_eq!(editor.buffer.selection, selected);
            assert!(editor.wants_touch_focus(&Hit::FingerUp(up())));
            let mut release = up();
            match transition {
                0 => release.abs.x = -1.0,
                1 => { editor.handle_hit(&mut cx, &mut kit, &look, &moved_out()); }
                2 => editor.observe_event(&cancel()),
                3 => editor.sync_text("new model"),
                4 => editor.observe_event(&Event::KeyFocus(KeyFocusEvent { prev: Area::Empty, focus: Area::Empty })),
                5 => editor.viewport.pos.x = 20.0,
                6 => release.tap_cancelled = true,
                _ => editor.buffer.insert("model edit", EditKind::Other),
            }
            let selection = editor.buffer.selection;
            assert!(!editor.wants_touch_focus(&Hit::FingerUp(release.clone())));
            editor.handle_hit(&mut cx, &mut kit, &look, &Hit::FingerUp(release));
            assert!(!editor.focused);
            assert_eq!(editor.buffer.selection, selection);
        }
    }


    #[test]
    fn native_text_actions_respect_read_only_and_empty_selection() {
        let mut editor = SourceEditor::new("한글 text".into(), "test.txt", "text-actions".into(), true);
        editor.select(0, "한글".len());
        let actions = editor.text_action_capabilities();
        assert!(actions.copy && actions.select_all);
        assert!(!actions.cut && !actions.paste);
        assert!(editor.cut().is_none());
        assert!(!editor.paste("replacement"));
        assert_eq!(editor.buffer.selected_text(), "한글");

        editor.read_only = false;
        let actions = editor.text_action_capabilities();
        assert!(actions.copy && actions.cut && actions.paste);
        assert_eq!(editor.cut().as_deref(), Some("한글"));
        assert_eq!(editor.text(), " text");
        let actions = editor.text_action_capabilities();
        assert!(!actions.copy && !actions.cut);
        assert!(actions.paste && actions.select_all);
        editor.buffer.undo();
        assert_eq!(editor.text(), "한글 text");
    }

    #[test]
    fn model_replacement_invalidates_selection_without_breaking_unicode() {
        let mut editor = SourceEditor::new("한글 text".into(), "test.txt", "replacement".into(), false);
        editor.select(0, "한글".len());
        editor.show_actions = true;
        editor.sync_text("한글 text");
        assert_eq!(editor.buffer.selected_text(), "한글");
        assert!(editor.show_actions);

        editor.sync_text("새 문서");
        assert!(editor.buffer.selection.is_empty());
        assert!(editor.text().is_char_boundary(editor.buffer.selection.head));
        assert!(editor.selection_invalidated);
        assert!(!editor.show_actions);
        assert!(editor.drag.is_none());
    }

    #[test]
    fn wraps_after_whitespace() {
        assert_eq!(wrap_line("hello world foo", 8), vec![0, 6, 12]);
        assert_eq!(wrap_line("abcdefghij", 4), vec![0, 4, 8]);
        assert_eq!(wrap_line("short", 10), vec![0]);
        assert_eq!(line_cols("한글"), 4);
    }

    #[test]
    fn tab_display_mapping() {
        let (text, map) = display("a\tb");
        assert_eq!(text, "a    b");
        assert_eq!(to_display(&map, 2), 5);
        assert_eq!(to_source(&map, 3), 1);
        assert_eq!(to_source(&map, 5), 2);
    }
}
