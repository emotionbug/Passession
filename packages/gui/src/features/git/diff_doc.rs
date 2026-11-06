//! The Changes document (original diff_view.dart `DiffDocument`): every
//! changed file in one scroll view with a sticky per-file header. Headers come
//! from the summary; a file's patch is requested only when its placeholder is
//! drawn near the viewport, and collapsed files never load. Rows are
//! virtualized: only rows intersecting the viewport are painted.

use super::chrome::{change_icon, diff_stat, measure_diff_stat, palette, typography, ui};
use super::json::{int, text};
use super::model::{DiffLine, LineKind, change_kind, compare_diff_paths, number_diff};
use super::paint::{Font, GitPaint, Hits, hex, rect, with_alpha};
use super::text::t;
use crate::common::paint::intersect;
use crate::features::timeline::canvas::{floor_char_boundary, range_rects};
use makepad_widgets::*;
use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use paseo_highlight::{ColorScheme, HighlightStyle, SyntaxThemeId, highlight_code, resolve_syntax_colors};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// One selectable text of the original document (a `Text` inside its
/// `SelectionArea`), in reading order: (file order, row, slot). Row 0 is the
/// file header (slot 0 name, 1 directory); row `r + 1` is body row `r` with
/// slot `side * 2` the gutter number and `side * 2 + 1` the code (unified rows
/// use side 0; a hidden-context row has its label in slot 0).
pub type PieceKey = (usize, usize, u8);

/// A text position: a piece and a byte offset into its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TextPos {
    pub piece: PieceKey,
    pub offset: usize,
}

/// A piece drawn this frame with the layout it was drawn from.
struct Piece {
    key: PieceKey,
    laid: Rc<LaidoutText>,
    origin: DVec2,
    clip: Rect,
    /// Drawn in the sticky header layer.
    header: bool,
}

const HEADER_HEIGHT: f64 = 30.0;
const HIGHLIGHT_LINE_LIMIT: usize = 8000;
const HIGHLIGHT_WIDTH_LIMIT: usize = 2000;

type Run = (String, Option<HighlightStyle>);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Line(usize),
    Pair(Option<usize>, Option<usize>),
    Hidden { start: usize, end: usize },
}

/// Per-patch projection retained across draws: numbered lines, syntax runs,
/// collapsed context and unified/split rows with their heights.
struct FileModel {
    revision: u64,
    lines: Vec<DiffLine>,
    max_line: i64,
    max_chars: usize,
    expanded: HashSet<usize>,
    rows: Option<(bool, Vec<Row>)>,
    runs: Option<HashMap<usize, Vec<Run>>>,
    /// Row tops relative to the body with their layout key and total height.
    heights: Option<RowHeights>,
}

/// (split, wrap, text width, comment revision).
type HeightKey = (bool, bool, i64, u64);
/// Cache key, row tops relative to the body, total body height.
type RowHeights = (HeightKey, Vec<f64>, f64);

impl FileModel {
    fn new(patch: &Value, revision: u64) -> Self {
        let lines = number_diff(patch);
        let mut max_line = 0;
        let mut max_chars = 0;
        for line in &lines {
            max_line = max_line.max(line.old_number.unwrap_or(0)).max(line.new_number.unwrap_or(0));
            max_chars = max_chars.max(line.content.chars().count());
        }
        Self {
            revision,
            lines,
            max_line,
            max_chars,
            expanded: HashSet::new(),
            rows: None,
            runs: None,
            heights: None,
        }
    }

    /// Original `_FileModel.rows`: context runs longer than 8 collapse to 3 +
    /// a hidden row + 3; split pairs removals with the following additions.
    fn rows(&mut self, split: bool) -> &[Row] {
        if self.rows.as_ref().is_none_or(|(s, _)| *s != split) {
            let lines = &self.lines;
            let single = |index: usize| {
                if !split || lines[index].kind == LineKind::Header {
                    Row::Line(index)
                } else {
                    Row::Pair(
                        (lines[index].kind != LineKind::Add).then_some(index),
                        (lines[index].kind != LineKind::Remove).then_some(index),
                    )
                }
            };
            let mut rows = Vec::new();
            let mut i = 0;
            while i < lines.len() {
                let kind = lines[i].kind;
                if kind == LineKind::Context {
                    let mut end = i;
                    while end < lines.len() && lines[end].kind == LineKind::Context {
                        end += 1;
                    }
                    if end - i > 8 && !self.expanded.contains(&i) {
                        rows.extend((i..i + 3).map(single));
                        rows.push(Row::Hidden { start: i, end });
                        rows.extend((end - 3..end).map(single));
                    } else {
                        rows.extend((i..end).map(single));
                    }
                    i = end;
                } else if split && kind == LineKind::Remove {
                    let mut end = i;
                    while end < lines.len() && lines[end].kind == LineKind::Remove {
                        end += 1;
                    }
                    let mut added_end = end;
                    while added_end < lines.len() && lines[added_end].kind == LineKind::Add {
                        added_end += 1;
                    }
                    for n in 0..(end - i).max(added_end - end) {
                        rows.push(Row::Pair(
                            (n < end - i).then_some(i + n),
                            (n < added_end - end).then_some(end + n),
                        ));
                    }
                    i = added_end;
                } else {
                    rows.push(single(i));
                    i += 1;
                }
            }
            self.rows = Some((split, rows));
            self.heights = None;
        }
        &self.rows.as_ref().unwrap().1
    }

    fn invalidate(&mut self) {
        self.rows = None;
        self.heights = None;
    }

    /// The comment context: up to three lines either side within the hunk.
    pub fn context_for(&self, index: usize) -> Vec<DiffLine> {
        let hunk = self.lines[index].hunk;
        let start = index.saturating_sub(3);
        let end = (index + 4).min(self.lines.len());
        self.lines[start..end]
            .iter()
            .filter(|l| l.kind != LineKind::Header && l.hunk == hunk)
            .cloned()
            .collect()
    }

    /// Highlights each hunk's old and new sides as whole sequences so
    /// multi-line comments and strings keep their state, then splits per line.
    fn runs(&mut self, path: &str) -> &HashMap<usize, Vec<Run>> {
        if self.runs.is_none() {
            let mut result = HashMap::new();
            if self.lines.len() <= HIGHLIGHT_LINE_LIMIT && self.max_chars <= HIGHLIGHT_WIDTH_LIMIT {
                let mut hunks: Vec<(usize, Vec<usize>, Vec<usize>)> = Vec::new();
                for (index, line) in self.lines.iter().enumerate() {
                    if line.kind == LineKind::Header {
                        hunks.push((line.hunk, Vec::new(), Vec::new()));
                        continue;
                    }
                    let Some(entry) = hunks.last_mut() else { continue };
                    if line.kind != LineKind::Add {
                        entry.1.push(index);
                    }
                    if line.kind != LineKind::Remove {
                        entry.2.push(index);
                    }
                }
                for (_, old, new) in &hunks {
                    for (side, is_new) in [(new, true), (old, false)] {
                        if side.is_empty() {
                            continue;
                        }
                        let code = side.iter().map(|&i| self.lines[i].content.as_str()).collect::<Vec<_>>().join("\n");
                        let Ok(tokens) = highlight_code(&code, path) else {
                            continue;
                        };
                        for (row, line_tokens) in tokens.into_iter().enumerate() {
                            let Some(&index) = side.get(row) else { break };
                            // Context lines take their runs from the new side only.
                            if !is_new && self.lines[index].kind == LineKind::Context {
                                continue;
                            }
                            result.insert(
                                index,
                                line_tokens.into_iter().map(|token| (token.text.to_owned(), token.style)).collect(),
                            );
                        }
                    }
                }
            }
            self.runs = Some(result);
        }
        self.runs.as_ref().unwrap()
    }
}

/// What a press on a document region does.
#[derive(Clone, Debug, PartialEq)]
pub enum DocHit {
    Header(String),
    Expand { path: String, start: usize },
    /// The gutter of a numbered line: comment on that side.
    Gutter { path: String, line: usize, side: &'static str },
    LineMenu { path: String, line: usize },
    Retry(String),
    EditComment(String),
    DeleteComment(String),
}

/// The line a gutter or menu targets.
#[derive(Clone, Debug)]
pub struct LineTarget {
    pub path: String,
    pub line: DiffLine,
    pub context: Vec<DiffLine>,
}

/// Inputs of one document draw.
pub struct DocInput<'a> {
    pub files: &'a [Value],
    pub patches: &'a HashMap<String, Value>,
    pub patch_errors: &'a HashMap<String, String>,
    pub patch_revision: u64,
    pub collapsed: &'a HashSet<String>,
    /// path → "side:line" → review draft comments.
    pub comments: &'a HashMap<String, HashMap<String, Vec<Value>>>,
    pub comments_revision: u64,
    pub split: bool,
    pub wrap: bool,
    pub syntax_theme: &'a str,
    pub bottom_inset: f64,
}

pub struct DiffDoc {
    pub scroll_y: f64,
    pub scroll_x: f64,
    models: HashMap<String, FileModel>,
    pub hits: Hits<DocHit>,
    pub hovered: Option<DocHit>,
    viewport: Rect,
    content_height: f64,
    content_width: f64,
    reveal: Option<String>,
    /// Scroll anchor from the last draw: the first visible file and the
    /// offset into it, plus the scroll it was taken at. Blocks above the
    /// viewport change height as their patches load; the anchor keeps the
    /// visible content still (Flutter keeps ensureVisible targets in place).
    anchor: Option<(String, f64)>,
    anchored_scroll_y: f64,
    /// Files whose placeholders were drawn near the viewport this frame.
    pub wanted: Vec<String>,
    /// A loading placeholder was painted (the panel requests the next frame).
    pub spinning: bool,
    /// Selectable texts drawn this frame.
    pieces: Vec<Piece>,
    /// Paths in document order at the last draw (piece file indices).
    order: Vec<String>,
    /// Split layout at the last draw (piece rows).
    split: bool,
    /// Collapsed files at the last draw (their bodies hold no text).
    collapsed: HashSet<String>,
    /// `(anchor, focus)` of the `SelectionArea` selection.
    pub selection: Option<(TextPos, TextPos)>,
    /// A mouse drag is extending the selection; the pointer drives
    /// edge auto-scroll while it is outside the viewport.
    pub drag_pointer: Option<DVec2>,
}

impl Default for DiffDoc {
    fn default() -> Self {
        Self {
            scroll_y: 0.0,
            scroll_x: 0.0,
            models: HashMap::new(),
            hits: Hits::default(),
            hovered: None,
            viewport: Rect::default(),
            content_height: 0.0,
            content_width: 0.0,
            reveal: None,
            anchor: None,
            anchored_scroll_y: 0.0,
            wanted: Vec::new(),
            spinning: false,
            pieces: Vec::new(),
            order: Vec::new(),
            split: false,
            collapsed: HashSet::new(),
            selection: None,
            drag_pointer: None,
        }
    }
}

fn line_height() -> f64 {
    (typography().code_font_size * 1.5).round()
}

fn char_width() -> f64 {
    (typography().code_font_size * 0.62).ceil()
}

fn gutter_width(max_line: i64) -> f64 {
    (max_line.to_string().len().max(2)) as f64 * char_width() + 12.0
}

/// A hidden-context row's label (selectable text of the expand row).
fn hidden_label(start: usize, end: usize) -> String {
    format!("Expand {} unchanged lines", end - start - 6)
}

/// The gutter number a cell shows.
fn gutter_number(line: &DiffLine, side: &str) -> Option<i64> {
    if line.kind == LineKind::Header {
        None
    } else if side == "old" {
        line.old_number
    } else {
        line.new_number
    }
}

/// Word bounds around byte `offset` of `text`.
fn word_bounds(text: &str, offset: usize) -> (usize, usize) {
    let offset = floor_char_boundary(text, offset.min(text.len()));
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let start = text[..offset].char_indices().rev().take_while(|(_, c)| is_word(*c)).last().map(|(i, _)| i).unwrap_or(offset);
    let end = text[offset..].char_indices().find(|(_, c)| !is_word(*c)).map(|(i, _)| offset + i).unwrap_or(text.len());
    (start, end)
}

impl DiffDoc {
    /// Scrolls so `path`'s header sits at the top on the next draw.
    pub fn reveal(&mut self, path: &str) {
        self.reveal = Some(path.to_owned());
    }

    pub fn viewport(&self) -> Rect {
        self.viewport
    }

    /// The selection in reading order, when it covers any text.
    fn ordered_selection(&self) -> Option<(TextPos, TextPos)> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        (start != end).then_some((start, end))
    }

    /// The selected byte range of a piece of `len` bytes.
    fn piece_span(selection: (TextPos, TextPos), key: PieceKey, len: usize) -> Option<(usize, usize)> {
        let (start, end) = selection;
        if key < start.piece || key > end.piece {
            return None;
        }
        let a = if key == start.piece { start.offset.min(len) } else { 0 };
        let b = if key == end.piece { end.offset.min(len) } else { len };
        (b > a).then_some((a, b))
    }

    /// The text position nearest to `abs` among this frame's pieces; inside a
    /// sticky header only that header's texts count.
    pub fn pos_at(&self, abs: DVec2) -> Option<TextPos> {
        let in_header = self.pieces.iter().find(|p| p.header && p.clip.contains(abs)).map(|p| p.key.0);
        let mut best: Option<(f64, TextPos)> = None;
        for piece in &self.pieces {
            match in_header {
                Some(file) if !(piece.header && piece.key.0 == file) => continue,
                None if piece.header => continue,
                _ => {}
            }
            let size = dvec2(piece.laid.size_in_lpxs.width as f64, piece.laid.size_in_lpxs.height as f64);
            let local = abs - piece.origin;
            let dy = if local.y < 0.0 { -local.y } else if local.y > size.y { local.y - size.y } else { 0.0 };
            let dx = if local.x < 0.0 { -local.x } else if local.x > size.x { local.x - size.x } else { 0.0 };
            let distance = dy * 1000.0 + dx;
            if best.is_some_and(|(d, _)| d <= distance) {
                continue;
            }
            let point = makepad_widgets::makepad_draw::text::geom::Point::new(local.x.clamp(0.0, size.x) as f32, local.y.clamp(0.0, size.y) as f32);
            let offset = piece.laid.point_in_lpxs_to_cursor(point).index.min(piece.laid.text.len());
            best = Some((distance, TextPos { piece: piece.key, offset }));
        }
        best.map(|(_, pos)| pos)
    }

    fn drawn_text(&self, key: PieceKey) -> Option<&str> {
        self.pieces.iter().find(|p| p.key == key).map(|p| p.laid.text.as_str())
    }

    /// The word around `pos` (double click / long press).
    pub fn word_at(&self, pos: TextPos) -> (TextPos, TextPos) {
        let (a, b) = self.drawn_text(pos.piece).map(|text| word_bounds(text, pos.offset)).unwrap_or((pos.offset, pos.offset));
        (TextPos { piece: pos.piece, offset: a }, TextPos { piece: pos.piece, offset: b })
    }

    /// The whole text under `pos` (triple click selects the paragraph).
    pub fn paragraph_at(&self, pos: TextPos) -> (TextPos, TextPos) {
        let len = self.drawn_text(pos.piece).map(str::len).unwrap_or(pos.offset);
        (TextPos { piece: pos.piece, offset: 0 }, TextPos { piece: pos.piece, offset: len })
    }

    /// Selects every text of the document (Ctrl/Cmd+A).
    pub fn select_all(&mut self) {
        let last = self.order.len().saturating_sub(1);
        self.selection = Some((
            TextPos { piece: (0, 0, 0), offset: 0 },
            TextPos { piece: (last, usize::MAX, u8::MAX), offset: usize::MAX },
        ));
    }

    /// The selected text as the original `SelectionArea` copies it: every
    /// selected `Text` in reading order, concatenated without separators —
    /// headers, gutter numbers and code, including rows outside the viewport.
    pub fn selected_text(&mut self) -> Option<String> {
        let selection = self.ordered_selection()?;
        let (start, end) = selection;
        let mut out = String::new();
        let last = end.piece.0.min(self.order.len().saturating_sub(1));
        for file in start.piece.0..=last {
            let Some(path) = self.order.get(file).cloned() else { break };
            let mut texts: Vec<(PieceKey, String)> = Vec::new();
            let (directory, name) = match path.rfind('/') {
                Some(slash) => (&path[..slash], &path[slash + 1..]),
                None => ("", path.as_str()),
            };
            texts.push(((file, 0, 0), name.to_owned()));
            if !directory.is_empty() {
                texts.push(((file, 0, 1), directory.to_owned()));
            }
            let split = self.split;
            if !self.collapsed.contains(&path)
                && let Some(model) = self.models.get_mut(&path)
            {
                let rows = model.rows(split).to_vec();
                let lines = &model.lines;
                // A cell's gutter number then its code.
                let cell = |texts: &mut Vec<(PieceKey, String)>, row_key: usize, side_index: u8, line: usize, side: &str| {
                    let line = &lines[line];
                    let number = gutter_number(line, side).map(|n| n.to_string()).unwrap_or_default();
                    texts.push(((file, row_key, side_index * 2), number));
                    texts.push(((file, row_key, side_index * 2 + 1), line.content.clone()));
                };
                for (index, row) in rows.into_iter().enumerate() {
                    let row_key = index + 1;
                    match row {
                        Row::Hidden { start, end } => texts.push(((file, row_key, 0), hidden_label(start, end))),
                        Row::Line(line) => {
                            let side = if lines[line].kind == LineKind::Remove { "old" } else { "new" };
                            cell(&mut texts, row_key, 0, line, side);
                        }
                        Row::Pair(left, right) => {
                            if let Some(line) = left {
                                cell(&mut texts, row_key, 0, line, "old");
                            }
                            if let Some(line) = right {
                                cell(&mut texts, row_key, 1, line, "new");
                            }
                        }
                    }
                }
            }
            for (key, text) in texts {
                if let Some((a, b)) = Self::piece_span(selection, key, text.len()) {
                    out.push_str(&text[floor_char_boundary(&text, a)..floor_char_boundary(&text, b)]);
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Highlights the selected parts of this frame's pieces of one layer
    /// (Material's default selection colour: `colorScheme.primary` at 40%).
    fn draw_selection(&self, cx: &mut Cx2d, paint: &mut GitPaint, header: bool) {
        let Some(selection) = self.ordered_selection() else { return };
        let color = with_alpha(palette().primary, 0.4);
        for piece in self.pieces.iter().filter(|p| p.header == header) {
            let Some((a, b)) = Self::piece_span(selection, piece.key, piece.laid.text.len()) else { continue };
            for r in range_rects(&piece.laid, a, b) {
                let r = r.translate(piece.origin).clip((piece.clip.pos, piece.clip.pos + piece.clip.size));
                paint.fill(cx, r, color);
            }
        }
    }

    /// Scrolls by a wheel delta; returns true when the offset changed.
    pub fn scroll_by(&mut self, delta: DVec2, wrap: bool) -> bool {
        let before = (self.scroll_x, self.scroll_y);
        let max_y = (self.content_height - self.viewport.size.y).max(0.0);
        self.scroll_y = (self.scroll_y + delta.y).clamp(0.0, max_y);
        if !wrap {
            let max_x = (self.content_width - self.viewport.size.x).max(0.0);
            self.scroll_x = (self.scroll_x + delta.x).clamp(0.0, max_x);
        } else {
            self.scroll_x = 0.0;
        }
        before != (self.scroll_x, self.scroll_y)
    }

    pub fn toggle_expand(&mut self, path: &str, start: usize) {
        if let Some(model) = self.models.get_mut(path) {
            model.expanded.insert(start);
            model.invalidate();
        }
    }

    pub fn line_target(&self, path: &str, index: usize) -> Option<LineTarget> {
        let model = self.models.get(path)?;
        Some(LineTarget {
            path: path.to_owned(),
            line: model.lines.get(index)?.clone(),
            context: model.context_for(index),
        })
    }

    fn model(&mut self, path: &str, patch: &Value, revision: u64) -> &mut FileModel {
        let stale = self.models.get(path).is_none_or(|m| m.revision != revision);
        if stale {
            self.models.insert(path.to_owned(), FileModel::new(patch, revision));
        }
        self.models.get_mut(path).unwrap()
    }

    /// Comments shown under a cell (original `bodies`).
    fn bodies<'a>(input: &'a DocInput<'_>, path: &str, line: &DiffLine, side: &str, unified: bool) -> Vec<&'a Value> {
        let Some(file) = input.comments.get(path) else {
            return Vec::new();
        };
        let number = if side == "old" { line.old_number } else { line.new_number };
        let mut bodies: Vec<&Value> = Vec::new();
        if let Some(number) = number
            && let Some(found) = file.get(&format!("{side}:{number}"))
        {
            bodies.extend(found.iter());
        }
        if unified
            && line.kind == LineKind::Context
            && let Some(old) = line.old_number
            && let Some(found) = file.get(&format!("old:{old}"))
        {
            bodies.extend(found.iter());
        }
        bodies
    }

    fn comment_card_height(cx: &mut Cx2d, paint: &mut GitPaint, body: &str, width: f64) -> f64 {
        let size = typography().content_font_size;
        let line = (size * 1.4).round();
        let measured = paint.measure_wrapped(cx, Font::Regular, size, body, (width - 24.0 - 60.0).max(40.0));
        16.0 + measured.min(line * 2.0).max(line).max(26.0 - 16.0)
    }

    fn thread_height(cx: &mut Cx2d, paint: &mut GitPaint, bodies: &[&Value], width: f64) -> f64 {
        if bodies.is_empty() {
            return 0.0;
        }
        let cards: f64 = bodies
            .iter()
            .map(|b| Self::comment_card_height(cx, paint, text(b, "body"), width - 24.0))
            .sum();
        16.0 + cards + 6.0 * (bodies.len() as f64 - 1.0)
    }

    fn cell_text_height(cx: &mut Cx2d, paint: &mut GitPaint, content: &str, width: f64) -> f64 {
        let lh = line_height();
        let size = typography().code_font_size;
        paint.measure_wrapped(cx, Font::Mono, size, content, width.max(20.0)).max(lh)
    }

    /// Row tops of a loaded body, cached per layout; returns the total height.
    fn ensure_heights(cx: &mut Cx2d, paint: &mut GitPaint, input: &DocInput<'_>, path: &str, model: &mut FileModel, column_width: f64) -> f64 {
        let lh = line_height();
        let gutter = gutter_width(model.max_line);
        let key = (input.split, input.wrap, column_width.round() as i64, input.comments_revision);
        if let Some((cached, _, total)) = &model.heights
            && *cached == key
        {
            return *total;
        }
        let text_width = column_width - gutter - 1.0 - 8.0 - 4.0 - 24.0;
        model.rows(input.split);
        let rows = &model.rows.as_ref().expect("rows computed above").1;
        let lines = &model.lines;
        let mut tops = Vec::with_capacity(rows.len());
        let mut y = 0.0;
        for row in rows {
            tops.push(y);
            let height = match *row {
                Row::Hidden { .. } => lh,
                Row::Line(index) => {
                    let line = &lines[index];
                    let side = if line.kind == LineKind::Remove { "old" } else { "new" };
                    let base = if input.wrap && line.kind != LineKind::Header {
                        Self::cell_text_height(cx, paint, &line.content, text_width)
                    } else {
                        lh
                    };
                    let bodies = Self::bodies(input, path, line, side, true);
                    base + Self::thread_height(cx, paint, &bodies, column_width)
                }
                Row::Pair(left, right) => {
                    let mut height: f64 = lh;
                    for (cell, side) in [(left, "old"), (right, "new")] {
                        let Some(index) = cell else { continue };
                        let line = &lines[index];
                        let base = if input.wrap { Self::cell_text_height(cx, paint, &line.content, text_width) } else { lh };
                        let bodies = Self::bodies(input, path, line, side, false);
                        height = height.max(base + Self::thread_height(cx, paint, &bodies, column_width));
                    }
                    height
                }
            };
            y += height;
        }
        model.heights = Some((key, tops, y));
        y
    }

    /// The height of a file's body (without header and closing rule).
    fn body_height(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, input: &DocInput<'_>, file: &Value, column_width: f64) -> f64 {
        let path = text(file, "path");
        let lh = line_height();
        let message = lh + 24.0;
        match input.patches.get(path) {
            None if input.patch_errors.contains_key(path) => message,
            None => ((int(file, "additions") + int(file, "deletions") + 2).clamp(2, 400)) as f64 * lh,
            Some(patch) => {
                let status = text(patch, "status");
                if status == "binary" || status == "too_large" {
                    return message;
                }
                let empty = {
                    let model = self.model(path, patch, input.patch_revision);
                    model.rows(input.split).is_empty()
                };
                if empty {
                    return message;
                }
                let model = self.model(path, patch, input.patch_revision);
                Self::ensure_heights(cx, paint, input, path, model, column_width)
            }
        }
    }

    /// Draws the document into `area`. Fills [`wanted`](Self::wanted) with
    /// files whose patches should load.
    pub fn draw(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, input: &DocInput<'_>) {
        let colors = palette();
        self.hits.clear();
        self.wanted.clear();
        self.viewport = area;
        paint.fill(cx, area, colors.surface0);
        let mut files: Vec<&Value> = input.files.iter().collect();
        files.sort_by(|a, b| compare_diff_paths(text(a, "path"), text(b, "path")));
        let live: HashSet<&str> = files.iter().map(|f| text(f, "path")).collect();
        self.models.retain(|path, model| {
            live.contains(path.as_str()) && input.patches.contains_key(path) && model.revision == input.patch_revision
        });
        self.spinning = false;
        // Selection context of this frame: pieces register as they are drawn.
        self.pieces.clear();
        let order: Vec<String> = files.iter().map(|f| text(f, "path").to_owned()).collect();
        if order != self.order {
            // Piece keys index files by order; another file list drops the selection.
            self.selection = None;
            self.order = order;
        }
        if self.split != input.split {
            self.selection = None;
            self.split = input.split;
        }
        if self.collapsed != *input.collapsed {
            self.collapsed = input.collapsed.clone();
        }
        // Column width: the widest loaded line, at least the viewport.
        let mut widest = 0usize;
        let mut max_line = 0i64;
        for file in &files {
            let path = text(file, "path");
            if let Some(patch) = input.patches.get(path) {
                let model = self.model(path, patch, input.patch_revision);
                widest = widest.max(model.max_chars);
                max_line = max_line.max(model.max_line);
            }
        }
        let column = gutter_width(max_line) + widest as f64 * char_width() + 48.0;
        let content_width = if input.wrap {
            area.size.x
        } else {
            area.size.x.max(if input.split { column * 2.0 + 1.0 } else { column })
        };
        self.content_width = content_width;
        let column_width = if input.split { (content_width - 1.0) / 2.0 } else { content_width };
        // Block layout.
        let mut blocks: Vec<(String, f64, f64)> = Vec::with_capacity(files.len());
        let mut y = 0.0;
        for file in &files {
            let path = text(file, "path").to_owned();
            let body = if input.collapsed.contains(&path) {
                0.0
            } else {
                self.body_height(cx, paint, input, file, column_width) + 1.0
            };
            blocks.push((path, y, HEADER_HEIGHT + body));
            y += HEADER_HEIGHT + body;
        }
        self.content_height = y + 24.0 + input.bottom_inset;
        // Original `SelectionArea` edge auto-scroll while a selection drag is
        // held beyond the viewport: faster the farther out, frames only while it runs.
        let mut autoscroll = false;
        if let Some(pointer) = self.drag_pointer {
            let overshoot = if pointer.y < area.pos.y {
                pointer.y - area.pos.y
            } else if pointer.y > area.pos.y + area.size.y {
                pointer.y - (area.pos.y + area.size.y)
            } else {
                0.0
            };
            if overshoot != 0.0 {
                self.scroll_y += overshoot.clamp(-120.0, 120.0) * 0.25;
                autoscroll = true;
            }
        }
        if let Some(path) = self.reveal.take()
            && let Some((_, top, _)) = blocks.iter().find(|(p, _, _)| *p == path)
        {
            self.scroll_y = *top;
        } else if let Some((path, offset)) = &self.anchor
            && let Some((_, top, _)) = blocks.iter().find(|(p, _, _)| p == path)
        {
            // Scrolling since the last draw moves relative to the anchor.
            self.scroll_y = top + offset + (self.scroll_y - self.anchored_scroll_y);
        }
        let max_y = (self.content_height - area.size.y).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_y);
        self.anchor = blocks
            .iter()
            .find(|(_, top, height)| top + height > self.scroll_y)
            .map(|(path, top, _)| (path.clone(), self.scroll_y - top));
        self.anchored_scroll_y = self.scroll_y;
        self.scroll_x = if input.wrap {
            0.0
        } else {
            self.scroll_x.clamp(0.0, (content_width - area.size.x).max(0.0))
        };
        let view_top = self.scroll_y;
        let view_bottom = self.scroll_y + area.size.y;
        let syntax = SyntaxThemeId::from_name(input.syntax_theme).unwrap_or(SyntaxThemeId::One);
        let syntax_colors = resolve_syntax_colors(syntax, if colors.is_dark { ColorScheme::Dark } else { ColorScheme::Light });
        paint.begin_clip(cx, area);
        let origin = dvec2(area.pos.x - self.scroll_x, area.pos.y - self.scroll_y);
        for (file_index, (path, top, height)) in blocks.iter().enumerate() {
            if top + height < view_top || *top > view_bottom {
                // Placeholders just below the viewport preload.
                if *top > view_bottom && *top < view_bottom + area.size.y
                    && !input.collapsed.contains(path)
                    && !input.patches.contains_key(path)
                    && !input.patch_errors.contains_key(path)
                {
                    self.wanted.push(path.clone());
                }
                continue;
            }
            if input.collapsed.contains(path) {
                continue;
            }
            let body_top = origin.y + top + HEADER_HEIGHT;
            let body_height = height - HEADER_HEIGHT - 1.0;
            self.draw_body(cx, paint, input, file_index, path, dvec2(origin.x, body_top), body_height, column_width, area, &syntax_colors);
            paint.fill(cx, rect(area.pos.x, body_top + body_height, area.size.x.max(content_width), 1.0), colors.border);
        }
        // Selection beneath the body glyphs (the box batch precedes the text batch).
        self.draw_selection(cx, paint, false);
        // Sticky headers paint over the rows.
        paint.new_layer(cx);
        for (file_index, (file, (path, top, height))) in files.iter().zip(blocks.iter()).enumerate() {
            if top + height < view_top || *top > view_bottom {
                continue;
            }
            let natural = area.pos.y + top - self.scroll_y;
            let pinned = natural.max(area.pos.y).min(area.pos.y + top + height - self.scroll_y - HEADER_HEIGHT);
            self.draw_header(cx, paint, file_index, file, path, rect(area.pos.x, pinned, area.size.x, HEADER_HEIGHT), area);
        }
        self.draw_selection(cx, paint, true);
        paint.end_clip(cx);
        if autoscroll {
            // The held pointer keeps extending the selection over newly scrolled text.
            if let Some(pointer) = self.drag_pointer {
                let inside = dvec2(
                    pointer.x.clamp(area.pos.x, area.pos.x + area.size.x),
                    pointer.y.clamp(area.pos.y + 1.0, area.pos.y + area.size.y - 1.0),
                );
                if let (Some(pos), Some((anchor, _))) = (self.pos_at(inside), self.selection) {
                    self.selection = Some((anchor, pos));
                }
            }
            self.spinning = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_header(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, file_index: usize, file: &Value, path: &str, header: Rect, clip: Rect) {
        let colors = palette();
        let hit = DocHit::Header(path.to_owned());
        let hovered = self.hovered.as_ref() == Some(&hit);
        paint.fill(cx, header, if hovered { colors.surface1 } else { colors.surface0 });
        paint.fill(cx, rect(header.pos.x, header.pos.y + header.size.y - 1.0, header.size.x, 1.0), colors.border_accent);
        let center_y = header.pos.y + (HEADER_HEIGHT - 1.0) / 2.0;
        let right = header.pos.x + header.size.x - 8.0;
        change_icon(cx, paint, dvec2(right - 7.0, center_y), change_kind(file));
        let stat_right = right - 14.0 - 8.0;
        let stat_width = measure_diff_stat(cx, paint, int(file, "additions"), int(file, "deletions"));
        diff_stat(cx, paint, stat_right, center_y, int(file, "additions"), int(file, "deletions"), true);
        let size = ui(14.0);
        let name_left = header.pos.x + 12.0;
        let available = (stat_right - stat_width - 4.0 - name_left).max(0.0);
        let (directory, name) = match path.rfind('/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => ("", path),
        };
        let piece_clip = intersect(header, clip);
        let drawn = paint.text_centered(cx, Font::Regular, size, colors.foreground, name_left, header.pos.y, HEADER_HEIGHT - 1.0, name, available);
        if let Some((laid, origin)) = paint.line_layout(cx, Font::Regular, size, name_left, header.pos.y, HEADER_HEIGHT - 1.0, name, available) {
            self.pieces.push(Piece { key: (file_index, 0, 0), laid, origin, clip: piece_clip, header: true });
        }
        if !directory.is_empty() && drawn + 4.0 < available {
            let x = name_left + drawn + 4.0;
            paint.text_centered(cx, Font::Regular, size, colors.muted, x, header.pos.y, HEADER_HEIGHT - 1.0, directory, available - drawn - 4.0);
            if let Some((laid, origin)) = paint.line_layout(cx, Font::Regular, size, x, header.pos.y, HEADER_HEIGHT - 1.0, directory, available - drawn - 4.0) {
                self.pieces.push(Piece { key: (file_index, 0, 1), laid, origin, clip: piece_clip, header: true });
            }
        }
        self.hits.push_clipped(header, clip, hit);
    }

    fn draw_message(cx: &mut Cx2d, paint: &mut GitPaint, origin: DVec2, width: f64, message: &str, action: Option<&str>) -> Option<Rect> {
        let colors = palette();
        let lh = line_height();
        let band = rect(origin.x, origin.y, width, lh + 24.0);
        paint.fill(cx, band, colors.surface0);
        let size = typography().code_font_size;
        let action_width = action.map(|a| paint.measure(cx, Font::Medium, ui(12.0), a).x + 16.0).unwrap_or(0.0);
        let drawn = paint.text_centered(cx, Font::Mono, size, colors.muted, origin.x + 12.0, band.pos.y, band.size.y, message, (width - 24.0 - action_width - 8.0).max(0.0));
        let action = action?;
        let button = rect(origin.x + 12.0 + drawn + 8.0, band.pos.y + (band.size.y - 24.0) / 2.0, action_width, 24.0);
        paint.text_centered(cx, Font::Medium, ui(12.0), colors.muted, button.pos.x + 8.0, button.pos.y, 24.0, action, action_width);
        Some(button)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_body(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        input: &DocInput<'_>,
        file_index: usize,
        path: &str,
        origin: DVec2,
        body_height: f64,
        column_width: f64,
        clip: Rect,
        syntax: &paseo_highlight::SyntaxColors,
    ) {
        let colors = palette();
        let width = column_width * if input.split { 2.0 } else { 1.0 } + if input.split { 1.0 } else { 0.0 };
        let Some(patch) = input.patches.get(path) else {
            if let Some(error) = input.patch_errors.get(path) {
                let retry = t("common.actions.retry");
                if let Some(button) = Self::draw_message(cx, paint, dvec2(clip.pos.x, origin.y), clip.size.x, error, Some(&retry)) {
                    self.hits.push_clipped(button, clip, DocHit::Retry(path.to_owned()));
                }
                return;
            }
            // Placeholder sized like the expected patch; requests it once drawn.
            let lh = line_height();
            paint.fill(cx, rect(clip.pos.x, origin.y, clip.size.x, body_height), colors.surface0);
            let band = rect(clip.pos.x + 12.0, origin.y + 8.0, clip.size.x - 24.0, lh);
            paint.spinner(cx, dvec2(band.pos.x + 6.0, band.pos.y + lh / 2.0), 12.0, colors.muted, cx.seconds_since_app_start());
            paint.text_centered(cx, Font::Regular, ui(12.0), colors.muted, band.pos.x + 12.0 + 8.0, band.pos.y, lh, "Loading patch", band.size.x - 20.0);
            self.spinning = true;
            self.wanted.push(path.to_owned());
            return;
        };
        let status = text(patch, "status");
        if status == "binary" || status == "too_large" {
            let key = if status == "binary" { "workspace.git.diff.binaryFile" } else { "workspace.git.diff.tooLarge" };
            Self::draw_message(cx, paint, dvec2(clip.pos.x, origin.y), clip.size.x, &t(key), None);
            return;
        }
        let mut model = {
            self.model(path, patch, input.patch_revision);
            self.models.remove(path).expect("model inserted above")
        };
        if model.rows(input.split).is_empty() {
            self.models.insert(path.to_owned(), model);
            Self::draw_message(cx, paint, dvec2(clip.pos.x, origin.y), clip.size.x, "No textual changes.", None);
            return;
        }
        Self::ensure_heights(cx, paint, input, path, &mut model, column_width);
        let lh = line_height();
        let view_top = clip.pos.y;
        let view_bottom = clip.pos.y + clip.size.y;
        // The visible row range by binary search over the cached row tops.
        let visible: Vec<(usize, Row, f64, f64)> = {
            let tops = &model.heights.as_ref().expect("heights ensured above").1;
            let rows = &model.rows.as_ref().expect("rows computed above").1;
            let first = tops.partition_point(|top| origin.y + top < view_top).saturating_sub(1);
            let last = tops.partition_point(|top| origin.y + top <= view_bottom).min(rows.len());
            (first..last.max(first))
                .map(|index| {
                    let top = origin.y + tops[index];
                    let bottom = tops.get(index + 1).map(|next| origin.y + next).unwrap_or(origin.y + body_height);
                    (index, rows[index], top, bottom)
                })
                .collect()
        };
        let gutter = gutter_width(model.max_line);
        for (row_index, row, top, bottom) in visible {
            let row_key = row_index + 1;
            if bottom < view_top {
                continue;
            }
            match row {
                Row::Hidden { start, end } => {
                    let band = rect(origin.x, top, width, lh);
                    let hit = DocHit::Expand { path: path.to_owned(), start };
                    if self.hovered.as_ref() == Some(&hit) {
                        paint.fill(cx, band, colors.surface1);
                    }
                    let x = origin.x + gutter + 9.0;
                    paint.icon(cx, "chevrons-up-down", 14.0, colors.muted, dvec2(x + 7.0, top + lh / 2.0));
                    let label = hidden_label(start, end);
                    let code = typography().code_font_size;
                    paint.text_centered(cx, Font::Mono, code, colors.muted, x + 14.0 + 8.0, top, lh, &label, width);
                    if let Some((laid, origin)) = paint.line_layout(cx, Font::Mono, code, x + 14.0 + 8.0, top, lh, &label, width) {
                        self.pieces.push(Piece { key: (file_index, row_key, 0), laid, origin, clip: intersect(band, clip), header: false });
                    }
                    self.hits.push_clipped(band, clip, hit);
                }
                Row::Line(index) => {
                    let runs = Self::cell_runs(&mut model, path, index, input.wrap);
                    let line = &model.lines[index];
                    let side = if line.kind == LineKind::Remove { "old" } else { "new" };
                    let cell = rect(origin.x, top, width, bottom - top);
                    self.draw_cell(cx, paint, input, path, (file_index, row_key, 0), Some((index, line)), runs, side, true, cell, gutter, clip, syntax);
                }
                Row::Pair(left, right) => {
                    let left_runs = left.and_then(|i| Self::cell_runs(&mut model, path, i, input.wrap));
                    let right_runs = right.and_then(|i| Self::cell_runs(&mut model, path, i, input.wrap));
                    let left_cell = rect(origin.x, top, column_width, bottom - top);
                    let right_cell = rect(origin.x + column_width + 1.0, top, column_width, bottom - top);
                    let left_line = left.map(|i| (i, &model.lines[i]));
                    let right_line = right.map(|i| (i, &model.lines[i]));
                    self.draw_cell(cx, paint, input, path, (file_index, row_key, 0), left_line, left_runs, "old", false, left_cell, gutter, clip, syntax);
                    self.draw_cell(cx, paint, input, path, (file_index, row_key, 1), right_line, right_runs, "new", false, right_cell, gutter, clip, syntax);
                }
            }
        }
        self.models.insert(path.to_owned(), model);
    }

    /// Syntax runs of a line (none for headers and wrapped lines).
    fn cell_runs(model: &mut FileModel, path: &str, index: usize, wrap: bool) -> Option<Vec<Run>> {
        if wrap || model.lines[index].kind == LineKind::Header {
            return None;
        }
        model.runs(path).get(&index).cloned()
    }

    /// Original `_cell`: gutter sized to the widest number, a 1px rule, then
    /// syntax-coloured code inset by 8px, the line actions button and any
    /// inline review thread under the line.
    #[allow(clippy::too_many_arguments)]
    fn draw_cell(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        input: &DocInput<'_>,
        path: &str,
        piece: (usize, usize, u8),
        line: Option<(usize, &DiffLine)>,
        runs: Option<Vec<Run>>,
        side: &'static str,
        unified: bool,
        cell: Rect,
        gutter: f64,
        clip: Rect,
        syntax: &paseo_highlight::SyntaxColors,
    ) {
        let colors = palette();
        let lh = line_height();
        let code_size = typography().code_font_size;
        let Some((index, line)) = line else {
            paint.fill(cx, cell, colors.surface0);
            return;
        };
        let header = line.kind == LineKind::Header;
        let background = match line.kind {
            LineKind::Add => with_alpha(colors.success, 0.15),
            LineKind::Remove => with_alpha(colors.danger, 0.1),
            _ => colors.surface0,
        };
        let number_color = match line.kind {
            LineKind::Add => colors.success,
            LineKind::Remove => colors.danger,
            _ => colors.muted,
        };
        let bodies = Self::bodies(input, path, line, side, unified);
        let thread = Self::thread_height(cx, paint, &bodies, cell.size.x);
        let line_box = rect(cell.pos.x, cell.pos.y, cell.size.x, cell.size.y - thread);
        paint.fill(cx, cell, colors.surface0);
        paint.fill(cx, line_box, background);
        let number = gutter_number(line, side);
        let gutter_rect = rect(cell.pos.x, cell.pos.y, gutter, line_box.size.y);
        if !bodies.is_empty() {
            paint.fill(cx, gutter_rect, colors.surface2);
        }
        let (file_index, row_key, side_index) = piece;
        let piece_clip = intersect(cell, clip);
        if let Some(number) = number {
            let label = number.to_string();
            let label_width = paint.measure(cx, Font::Mono, code_size, &label).x;
            let x = gutter_rect.pos.x + gutter - 7.0 - label_width;
            paint.text_centered(cx, Font::Mono, code_size, number_color, x, cell.pos.y, lh, &label, label_width + 1.0);
            if let Some((laid, origin)) = paint.line_layout(cx, Font::Mono, code_size, x, cell.pos.y, lh, &label, label_width + 1.0) {
                self.pieces.push(Piece { key: (file_index, row_key, side_index * 2), laid, origin, clip: piece_clip, header: false });
            }
            self.hits.push_clipped(gutter_rect, clip, DocHit::Gutter { path: path.to_owned(), line: index, side });
        }
        paint.fill(cx, rect(cell.pos.x + gutter, cell.pos.y, 1.0, line_box.size.y), colors.border);
        let text_x = cell.pos.x + gutter + 1.0 + 8.0;
        let text_width = (cell.size.x - gutter - 1.0 - 8.0 - 4.0 - if header { 0.0 } else { 24.0 }).max(0.0);
        let foreground = if header { colors.muted } else { colors.foreground };
        let code_key = (file_index, row_key, side_index * 2 + 1);
        if input.wrap && !header {
            let origin = dvec2(text_x, cell.pos.y + (lh - code_size * 1.2) / 2.0);
            paint.text_wrapped(cx, Font::Mono, code_size, foreground, origin, &line.content, text_width);
            if !line.content.is_empty() {
                let laid = paint.wrapped_layout(cx, Font::Mono, code_size, &line.content, text_width);
                self.pieces.push(Piece { key: code_key, laid, origin, clip: piece_clip, header: false });
            }
        } else {
            // One layout of the whole line gives the run geometry (monospace).
            let max_width = if input.wrap { text_width } else { f64::MAX / 4.0 };
            if let Some((laid, origin)) = paint.line_layout(cx, Font::Mono, code_size, text_x, cell.pos.y, lh, &line.content, max_width) {
                self.pieces.push(Piece { key: code_key, laid, origin, clip: piece_clip, header: false });
            }
            match runs {
                Some(runs) => {
                    let mut x = text_x;
                    for (run, style) in runs {
                        if x > clip.pos.x + clip.size.x {
                            break;
                        }
                        let color = style.map(|s| hex(syntax.color(s))).unwrap_or(foreground);
                        let width = paint.measure(cx, Font::Mono, code_size, &run).x;
                        if x + width >= clip.pos.x {
                            paint.text_centered(cx, Font::Mono, code_size, color, x, cell.pos.y, lh, &run, width + 1.0);
                        }
                        x += width;
                    }
                }
                None => {
                    paint.text_centered(cx, Font::Mono, code_size, foreground, text_x, cell.pos.y, lh, &line.content, if input.wrap { text_width } else { f64::MAX / 4.0 });
                }
            }
        }
        if !header {
            let button = rect(cell.pos.x + cell.size.x - 4.0 - 24.0, cell.pos.y, 24.0, lh);
            let hit = DocHit::LineMenu { path: path.to_owned(), line: index };
            if self.hovered.as_ref() == Some(&hit) {
                paint.rounded(cx, button, colors.surface2, 4.0, None);
            }
            paint.icon(cx, "ellipsis", 14.0, colors.extra_muted, button.center());
            self.hits.push_clipped(button, clip, hit);
        }
        if bodies.is_empty() {
            return;
        }
        // Original inline review thread.
        let size = typography().content_font_size;
        let mut y = line_box.pos.y + line_box.size.y + 8.0;
        for body in bodies {
            let card_height = Self::comment_card_height(cx, paint, text(body, "body"), cell.size.x - 24.0);
            let card = rect(cell.pos.x + 12.0, y, cell.size.x - 24.0, card_height);
            paint.rounded(cx, card, colors.surface2, 8.0, Some((colors.border_accent, 1.0)));
            let text_area = rect(card.pos.x + 12.0, card.pos.y + 8.0, card.size.x - 24.0 - 60.0, card_height - 16.0);
            paint.begin_clip(cx, text_area);
            paint.text_wrapped(cx, Font::Regular, size, colors.foreground, text_area.pos, text(body, "body"), text_area.size.x);
            paint.end_clip(cx);
            let id = text(body, "id").to_owned();
            let center_y = card.pos.y + card_height / 2.0;
            let edit = rect(card.pos.x + card.size.x - 12.0 - 26.0 - 4.0 - 26.0, center_y - 13.0, 26.0, 26.0);
            let delete = rect(card.pos.x + card.size.x - 12.0 - 26.0, center_y - 13.0, 26.0, 26.0);
            for (area, icon, color, hit) in [
                (edit, "pencil", colors.muted, DocHit::EditComment(id.clone())),
                (delete, "trash-2", colors.danger, DocHit::DeleteComment(id.clone())),
            ] {
                if self.hovered.as_ref() == Some(&hit) {
                    paint.rounded(cx, area, colors.surface3, 13.0, None);
                }
                paint.icon(cx, icon, 14.0, color, area.center());
                self.hits.push_clipped(area, clip, hit);
            }
            y += card_height + 6.0;
        }
    }

    /// Whether `point` is inside the drawn document.
    pub fn contains(&self, point: DVec2) -> bool {
        self.viewport.contains(point)
    }
}

/// A rect row used for the jump list sorting (keeps the document order).
pub fn sorted_paths(files: &[Value]) -> Vec<String> {
    let mut paths: Vec<String> = files.iter().map(|f| text(f, "path").to_owned()).collect();
    paths.sort_by(|a, b| compare_diff_paths(a, b));
    paths
}
