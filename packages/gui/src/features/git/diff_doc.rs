//! The commit document (original diff_view.dart `DiffDocument`): every
//! changed file in one scroll view with a sticky per-file header. Headers come
//! from the summary; a file's patch is requested only when its placeholder is
//! drawn near the viewport, and collapsed files never load. Rows are
//! virtualized: only rows intersecting the viewport are painted.

use super::chrome::{change_icon, diff_stat, measure_diff_stat, palette, typography, ui};
use super::json::{int, text};
use super::model::{DiffLine, LineKind, change_kind, compare_diff_paths, number_diff};
use super::paint::{Font, GitPaint, Hits, Retained, hex, rect, rect_key, with_alpha};
use super::text::t;
use crate::common::paint::intersect;
use crate::features::timeline::canvas::{floor_char_boundary, range_rects};
use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::makepad_draw::text::shaper::TAB_STOP_IN_SPACES;
use makepad_widgets::*;
use paseo_highlight::{
    ColorScheme, HighlightStyle, SyntaxThemeId, highlight_code, resolve_syntax_colors,
};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextPos {
    pub piece: PieceKey,
    pub offset: usize,
}

/// A piece drawn this frame with the layout it was drawn from.
#[derive(Clone)]
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

#[derive(Clone, Copy, Debug, PartialEq, Hash)]
enum Row {
    Line(usize),
    Pair(Option<usize>, Option<usize>),
    Hidden { start: usize, end: usize },
}

/// Per-patch projection retained across draws: numbered lines, syntax runs,
/// collapsed context and unified/split rows with their heights.
struct FileModel {
    lines: Vec<DiffLine>,
    max_line: i64,
    /// Widest line in display columns (see [`display_columns`]).
    max_columns: usize,
    expanded: HashSet<usize>,
    rows: Option<(bool, Vec<Row>)>,
    runs: Option<HashMap<usize, Vec<Run>>>,
    /// Row tops relative to the body with their layout key and total height.
    heights: Option<RowHeights>,
}

/// (split, wrap, text width, typography epoch).
type HeightKey = (bool, bool, f64, u64);
/// Cache key, row tops relative to the body, total body height.
type RowHeights = (HeightKey, Vec<f64>, f64);

impl FileModel {
    fn new(patch: &Value) -> Self {
        let lines = number_diff(patch);
        let mut max_line = 0;
        let mut max_columns = 0;
        for line in &lines {
            max_line = max_line
                .max(line.old_number.unwrap_or(0))
                .max(line.new_number.unwrap_or(0));
            max_columns = max_columns.max(display_columns(&line.content));
        }
        Self {
            lines,
            max_line,
            max_columns,
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

    /// Highlights each hunk's old and new sides as whole sequences so
    /// multi-line comments and strings keep their state, then splits per line.
    fn runs(&mut self, path: &str) -> &HashMap<usize, Vec<Run>> {
        if self.runs.is_none() {
            let mut result = HashMap::new();
            if self.lines.len() <= HIGHLIGHT_LINE_LIMIT && self.max_columns <= HIGHLIGHT_WIDTH_LIMIT
            {
                let mut hunks: Vec<(usize, Vec<usize>, Vec<usize>)> = Vec::new();
                for (index, line) in self.lines.iter().enumerate() {
                    if line.kind == LineKind::Header {
                        hunks.push((line.hunk, Vec::new(), Vec::new()));
                        continue;
                    }
                    let Some(entry) = hunks.last_mut() else {
                        continue;
                    };
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
                        let code = side
                            .iter()
                            .map(|&i| self.lines[i].content.as_str())
                            .collect::<Vec<_>>()
                            .join("\n");
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
                                line_tokens
                                    .into_iter()
                                    .map(|token| (token.text.to_owned(), token.style))
                                    .collect(),
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
#[derive(Clone, Debug, PartialEq, Hash)]
pub enum DocHit {
    Header(String),
    Expand {
        path: String,
        start: usize,
    },
    /// Opens the file at the numbered line.
    Gutter {
        path: String,
        line: usize,
    },
    LineMenu {
        path: String,
        line: usize,
    },
    Retry(String),
}

/// The line a gutter or menu targets.
#[derive(Clone, Debug)]
pub struct LineTarget {
    pub path: String,
    pub line: DiffLine,
}

/// Inputs of one document draw.
pub struct DocInput<'a> {
    pub files: &'a [Value],
    pub files_revision: u64,
    pub patches: &'a HashMap<String, Value>,
    /// Existing patches are immutable within this generation.
    pub patch_generation: u64,
    pub patch_errors: &'a HashMap<String, String>,
    /// Changes on patch or error arrival, removal, or replacement.
    pub patch_revision: u64,
    pub collapsed: &'a HashSet<String>,
    pub collapse_revision: u64,
    pub split: bool,
    pub wrap: bool,
    pub syntax_theme: &'a str,
}

#[derive(Clone, Copy, PartialEq)]
struct GeometryKey {
    files_revision: u64,
    patch_generation: u64,
    patch_revision: u64,
    collapse_revision: u64,
    expansion_revision: u64,
    width: f64,
    split: bool,
    wrap: bool,
    theme_epoch: u64,
}

#[derive(Clone, Copy)]
struct Block {
    top: f64,
    bottom: f64,
    collapsed: bool,
}

pub struct DiffDoc {
    pub scroll_y: f64,
    pub scroll_x: f64,
    models: HashMap<String, FileModel>,
    geometry_key: Option<GeometryKey>,
    /// Indices into the summary, sorted by document order.
    file_indices: Vec<usize>,
    /// Blocks share document order; both edges increase monotonically.
    blocks: Vec<Block>,
    /// Largest line number and display width among loaded patches.
    widest: (i64, usize),
    #[cfg(test)]
    layout_builds: (usize, usize),
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
    anchor: Option<(usize, f64)>,
    anchored_scroll_y: f64,
    /// Files whose placeholders were drawn near the viewport this frame.
    pub wanted: Vec<String>,
    /// Selectable texts drawn this frame.
    pieces: Vec<Piece>,
    /// Paths in document order at the last draw (piece file indices).
    order: Vec<String>,
    /// Split layout at the last draw (piece rows).
    split: bool,
    /// `(anchor, focus)` of the `SelectionArea` selection.
    pub selection: Option<(TextPos, TextPos)>,
    /// A mouse drag is extending the selection; the pointer drives
    /// edge auto-scroll while it is outside the viewport.
    pub drag_pointer: Option<DVec2>,
    /// Scrollbar tracks (pointer zones) and thumbs of the last draw; empty
    /// while that axis does not scroll.
    bars: [ScrollBar; 2],
    /// A dragged thumb: its axis, the pointer coordinate and scroll at the press.
    bar_drag: Option<(Axis, f64, f64)>,
    /// The track under the pointer (its thumb widens).
    pub bar_hovered: Option<Axis>,
    presentations: HashMap<PieceKey, DocPresentation>,
    background: Retained<DocHit>,
    scrollbar_layer: Retained<DocHit>,
    expansion_revision: u64,
}

/// A scrollbar direction of the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    Vertical,
    Horizontal,
}

impl Axis {
    fn index(self) -> usize {
        match self {
            Axis::Vertical => 0,
            Axis::Horizontal => 1,
        }
    }

    /// The coordinate of `point` along this axis.
    fn along(self, point: DVec2) -> f64 {
        match self {
            Axis::Vertical => point.y,
            Axis::Horizontal => point.x,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ScrollBar {
    track: Rect,
    thumb: Rect,
}

/// Scrollbar geometry: the pointer zone along an edge, the drawn thumb's
/// thickness (wider while hovered or dragged), and the shortest thumb.
const BAR_ZONE: f64 = 12.0;
const BAR_THICKNESS: f64 = 6.0;
const BAR_ACTIVE_THICKNESS: f64 = 9.0;
const BAR_MIN_THUMB: f64 = 24.0;

/// A thumb's offset and length along a `track` long track for `scroll` of
/// `max` over content `view + max` long.
fn thumb_span(track: f64, view: f64, max: f64, scroll: f64) -> (f64, f64) {
    if track <= 0.0 || max <= 0.0 {
        return (0.0, track.max(0.0));
    }
    let length = (view / (view + max) * track).clamp(BAR_MIN_THUMB.min(track), track);
    let offset = (scroll / max).clamp(0.0, 1.0) * (track - length);
    (offset, length)
}

impl Default for DiffDoc {
    fn default() -> Self {
        Self {
            scroll_y: 0.0,
            scroll_x: 0.0,
            models: HashMap::new(),
            geometry_key: None,
            file_indices: Vec::new(),
            blocks: Vec::new(),
            widest: (0, 0),
            #[cfg(test)]
            layout_builds: (0, 0),
            hits: Hits::default(),
            hovered: None,
            viewport: Rect::default(),
            content_height: 0.0,
            content_width: 0.0,
            reveal: None,
            anchor: None,
            anchored_scroll_y: 0.0,
            wanted: Vec::new(),
            pieces: Vec::new(),
            order: Vec::new(),
            split: false,
            selection: None,
            drag_pointer: None,
            bars: [ScrollBar::default(); 2],
            bar_drag: None,
            bar_hovered: None,
            presentations: HashMap::new(),
            background: Retained::default(),
            scrollbar_layer: Retained::default(),
            expansion_revision: 0,
        }
    }
}

#[derive(Default)]
struct DocPresentation {
    layer: Retained<DocHit>,
    pieces: Vec<Piece>,
    piece_mark: usize,
    visited: bool,
}

impl DocPresentation {
    fn begin(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        area: Rect,
        inputs: u64,
        hits: &mut Hits<DocHit>,
        pieces: &mut Vec<Piece>,
    ) -> bool {
        self.visited = true;
        if self.layer.begin(cx, area, inputs, hits, paint) {
            self.piece_mark = pieces.len();
            return true;
        }
        pieces.extend_from_slice(&self.pieces);
        false
    }

    fn end(&mut self, cx: &mut Cx2d, hits: &Hits<DocHit>, pieces: &[Piece]) {
        self.layer.end(cx, hits);
        self.pieces.clear();
        self.pieces.extend_from_slice(&pieces[self.piece_mark..]);
    }
}

impl DiffDoc {
    pub fn presentation_key(&self) -> u64 {
        crate::common::layer::inputs((
            self.scroll_x.to_bits(),
            self.scroll_y.to_bits(),
            self.selection,
            &self.hovered,
            self.bar_hovered,
            self.bar_drag.map(|(axis, _, _)| axis),
            &self.reveal,
            self.expansion_revision,
            self.drag_pointer
                .map(|point| (point.x.to_bits(), point.y.to_bits())),
        ))
    }

    fn row_selection(&self, file: usize, row: usize) -> Option<(TextPos, TextPos)> {
        let (mut start, mut end) = self.ordered_selection()?;
        if (file, row) < (start.piece.0, start.piece.1) || (file, row) > (end.piece.0, end.piece.1)
        {
            return None;
        }
        if (start.piece.0, start.piece.1) < (file, row) {
            start = TextPos {
                piece: (file, row, 0),
                offset: 0,
            };
        }
        if (end.piece.0, end.piece.1) > (file, row) {
            end = TextPos {
                piece: (file, row, u8::MAX),
                offset: usize::MAX,
            };
        }
        Some((start, end))
    }
}

fn line_height() -> f64 {
    (typography().code_font_size * 1.5).round()
}

/// Columns `line` spans in the document: a TAB reaches the next tab stop of the text layout,
/// every other character counts one.
fn display_columns(line: &str) -> usize {
    let stop = TAB_STOP_IN_SPACES as usize;
    line.chars().fold(0, |column, c| {
        if c == '\t' {
            column + stop - column % stop
        } else {
            column + 1
        }
    })
}

fn char_width() -> f64 {
    (typography().code_font_size * 0.62).ceil()
}

fn gutter_width(max_line: i64) -> f64 {
    (max_line.to_string().len().max(2)) as f64 * char_width() + 12.0
}

/// A hidden-context row's label (selectable text of the expand row).
fn hidden_label(start: usize, end: usize) -> String {
    super::text::tr(
        "workspace.git.diff.expandUnchanged",
        &[("count", (end - start - 6).to_string())],
    )
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
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map(|(i, _)| offset + i)
        .unwrap_or(text.len());
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

    /// Hidden documents must not retain pointer targets over a replacement state.
    pub fn hide(&mut self) {
        self.viewport = Rect::default();
        self.bars = [ScrollBar::default(); 2];
        self.bar_drag = None;
        self.bar_hovered = None;
        self.drag_pointer = None;
        self.hovered = None;
        self.selection = None;
        self.pieces.clear();
        self.hits.clear();
    }

    /// The selection in reading order, when it covers any text.
    fn ordered_selection(&self) -> Option<(TextPos, TextPos)> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        (start != end).then_some((start, end))
    }

    /// The selected byte range of a piece of `len` bytes.
    fn piece_span(
        selection: (TextPos, TextPos),
        key: PieceKey,
        len: usize,
    ) -> Option<(usize, usize)> {
        let (start, end) = selection;
        if key < start.piece || key > end.piece {
            return None;
        }
        let a = if key == start.piece {
            start.offset.min(len)
        } else {
            0
        };
        let b = if key == end.piece {
            end.offset.min(len)
        } else {
            len
        };
        (b > a).then_some((a, b))
    }

    /// The text position nearest to `abs` among this frame's pieces; inside a
    /// sticky header only that header's texts count.
    pub fn pos_at(&self, abs: DVec2) -> Option<TextPos> {
        let in_header = self
            .pieces
            .iter()
            .find(|p| p.header && p.clip.contains(abs))
            .map(|p| p.key.0);
        let mut best: Option<(f64, TextPos)> = None;
        for piece in &self.pieces {
            match in_header {
                Some(file) if !(piece.header && piece.key.0 == file) => continue,
                None if piece.header => continue,
                _ => {}
            }
            let size = dvec2(
                piece.laid.size_in_lpxs.width as f64,
                piece.laid.size_in_lpxs.height as f64,
            );
            let local = abs - piece.origin;
            let dy = if local.y < 0.0 {
                -local.y
            } else if local.y > size.y {
                local.y - size.y
            } else {
                0.0
            };
            let dx = if local.x < 0.0 {
                -local.x
            } else if local.x > size.x {
                local.x - size.x
            } else {
                0.0
            };
            let distance = dy * 1000.0 + dx;
            if best.is_some_and(|(d, _)| d <= distance) {
                continue;
            }
            let point = makepad_widgets::makepad_draw::text::geom::Point::new(
                local.x.clamp(0.0, size.x) as f32,
                local.y.clamp(0.0, size.y) as f32,
            );
            let offset = piece
                .laid
                .point_in_lpxs_to_cursor(point)
                .index
                .min(piece.laid.text.len());
            best = Some((
                distance,
                TextPos {
                    piece: piece.key,
                    offset,
                },
            ));
        }
        best.map(|(_, pos)| pos)
    }

    fn drawn_text(&self, key: PieceKey) -> Option<&str> {
        self.pieces
            .iter()
            .find(|p| p.key == key)
            .map(|p| p.laid.text.as_str())
    }

    /// The word around `pos` (double click / long press).
    pub fn word_at(&self, pos: TextPos) -> (TextPos, TextPos) {
        let (a, b) = self
            .drawn_text(pos.piece)
            .map(|text| word_bounds(text, pos.offset))
            .unwrap_or((pos.offset, pos.offset));
        (
            TextPos {
                piece: pos.piece,
                offset: a,
            },
            TextPos {
                piece: pos.piece,
                offset: b,
            },
        )
    }

    /// The whole text under `pos` (triple click selects the paragraph).
    pub fn paragraph_at(&self, pos: TextPos) -> (TextPos, TextPos) {
        let len = self
            .drawn_text(pos.piece)
            .map(str::len)
            .unwrap_or(pos.offset);
        (
            TextPos {
                piece: pos.piece,
                offset: 0,
            },
            TextPos {
                piece: pos.piece,
                offset: len,
            },
        )
    }

    /// Selects every text of the document (Ctrl/Cmd+A).
    pub fn select_all(&mut self) {
        let last = self.order.len().saturating_sub(1);
        self.selection = Some((
            TextPos {
                piece: (0, 0, 0),
                offset: 0,
            },
            TextPos {
                piece: (last, usize::MAX, u8::MAX),
                offset: usize::MAX,
            },
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
            let Some(path) = self.order.get(file) else {
                break;
            };
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
            if !self.blocks[file].collapsed
                && let Some(model) = self.models.get_mut(path)
            {
                let rows = model.rows(split).to_vec();
                let lines = &model.lines;
                // A cell's gutter number then its code.
                let cell = |texts: &mut Vec<(PieceKey, String)>,
                            row_key: usize,
                            side_index: u8,
                            line: usize,
                            side: &str| {
                    let line = &lines[line];
                    let number = gutter_number(line, side)
                        .map(|n| n.to_string())
                        .unwrap_or_default();
                    texts.push(((file, row_key, side_index * 2), number));
                    texts.push(((file, row_key, side_index * 2 + 1), line.content.clone()));
                };
                for (index, row) in rows.into_iter().enumerate() {
                    let row_key = index + 1;
                    match row {
                        Row::Hidden { start, end } => {
                            texts.push(((file, row_key, 0), hidden_label(start, end)))
                        }
                        Row::Line(line) => {
                            let side = if lines[line].kind == LineKind::Remove {
                                "old"
                            } else {
                                "new"
                            };
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
                    out.push_str(
                        &text[floor_char_boundary(&text, a)..floor_char_boundary(&text, b)],
                    );
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Highlights the selected parts of this frame's pieces of one layer
    /// (Material's default selection colour: `colorScheme.primary` at 40%).
    fn draw_selection(&self, cx: &mut Cx2d, paint: &mut GitPaint, header: bool, from: usize) {
        let Some(selection) = self.ordered_selection() else {
            return;
        };
        let color = with_alpha(palette().primary, 0.4);
        for piece in self.pieces[from..].iter().filter(|p| p.header == header) {
            let Some((a, b)) = Self::piece_span(selection, piece.key, piece.laid.text.len()) else {
                continue;
            };
            for r in range_rects(&piece.laid, a, b) {
                let r = r
                    .translate(piece.origin)
                    .clip((piece.clip.pos, piece.clip.pos + piece.clip.size));
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

    fn max_scroll(&self, axis: Axis) -> f64 {
        match axis {
            Axis::Vertical => (self.content_height - self.viewport.size.y).max(0.0),
            Axis::Horizontal => (self.content_width - self.viewport.size.x).max(0.0),
        }
    }

    fn scroll_of(&mut self, axis: Axis) -> &mut f64 {
        match axis {
            Axis::Vertical => &mut self.scroll_y,
            Axis::Horizontal => &mut self.scroll_x,
        }
    }

    /// The scrollbar track under `point`.
    pub fn bar_at(&self, point: DVec2) -> Option<Axis> {
        [Axis::Vertical, Axis::Horizontal].into_iter().find(|axis| {
            let track = self.bars[axis.index()].track;
            track.size.x > 0.0 && track.size.y > 0.0 && track.contains(point)
        })
    }

    /// A press on a scrollbar: on the thumb it starts a drag, elsewhere on the
    /// track it pages toward the pointer. Returns false off the scrollbars.
    pub fn press_bar(&mut self, point: DVec2) -> bool {
        let Some(axis) = self.bar_at(point) else {
            return false;
        };
        let bar = self.bars[axis.index()];
        let start = *self.scroll_of(axis);
        if bar.thumb.contains(point) {
            self.bar_drag = Some((axis, axis.along(point), start));
            return true;
        }
        let view = match axis {
            Axis::Vertical => self.viewport.size.y,
            Axis::Horizontal => self.viewport.size.x,
        };
        let page = (view * 0.9).max(1.0);
        let before = axis.along(bar.thumb.pos) > axis.along(point);
        let max = self.max_scroll(axis);
        let scroll = self.scroll_of(axis);
        *scroll = (*scroll + if before { -page } else { page }).clamp(0.0, max);
        true
    }

    /// Moves the dragged thumb with the pointer; returns true while dragging.
    pub fn drag_bar(&mut self, point: DVec2) -> bool {
        let Some((axis, origin, start)) = self.bar_drag else {
            return false;
        };
        let bar = self.bars[axis.index()];
        let (track, thumb) = match axis {
            Axis::Vertical => (bar.track.size.y, bar.thumb.size.y),
            Axis::Horizontal => (bar.track.size.x, bar.thumb.size.x),
        };
        let travel = track - thumb;
        let max = self.max_scroll(axis);
        if travel > 0.0 {
            *self.scroll_of(axis) =
                (start + (axis.along(point) - origin) * max / travel).clamp(0.0, max);
        }
        true
    }

    /// Ends a thumb drag; returns true when one was running.
    pub fn end_bar_drag(&mut self) -> bool {
        self.bar_drag.take().is_some()
    }

    /// The vertical and (unwrapped, wider than the view) horizontal
    /// scrollbars over the viewport's right and bottom edges.
    fn draw_scrollbars(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, wrap: bool) {
        let colors = palette();
        let max_y = (self.content_height - area.size.y).max(0.0);
        let max_x = if wrap {
            0.0
        } else {
            (self.content_width - area.size.x).max(0.0)
        };
        let (show_v, show_h) = (max_y > 0.5, max_x > 0.5);
        // Each track leaves the shared corner to the other.
        let corner = |other: bool| if other { BAR_ZONE } else { 0.0 };
        self.bars = [ScrollBar::default(); 2];
        if show_v {
            let track = rect(
                area.pos.x + area.size.x - BAR_ZONE,
                area.pos.y,
                BAR_ZONE,
                area.size.y - corner(show_h),
            );
            let (offset, length) = thumb_span(track.size.y, area.size.y, max_y, self.scroll_y);
            self.bars[0] = ScrollBar {
                track,
                thumb: rect(track.pos.x, track.pos.y + offset, BAR_ZONE, length),
            };
        }
        if show_h {
            let track = rect(
                area.pos.x,
                area.pos.y + area.size.y - BAR_ZONE,
                area.size.x - corner(show_v),
                BAR_ZONE,
            );
            let (offset, length) = thumb_span(track.size.x, area.size.x, max_x, self.scroll_x);
            self.bars[1] = ScrollBar {
                track,
                thumb: rect(track.pos.x + offset, track.pos.y, length, BAR_ZONE),
            };
        }
        for axis in [Axis::Vertical, Axis::Horizontal] {
            let bar = self.bars[axis.index()];
            if bar.track.size.x <= 0.0 || bar.track.size.y <= 0.0 {
                continue;
            }
            let active = self.bar_hovered == Some(axis)
                || self.bar_drag.is_some_and(|(dragged, _, _)| dragged == axis);
            let thickness = if active {
                BAR_ACTIVE_THICKNESS
            } else {
                BAR_THICKNESS
            };
            if active {
                paint.fill(cx, bar.track, with_alpha(colors.muted, 0.08));
            }
            let thumb = match axis {
                Axis::Vertical => rect(
                    bar.thumb.pos.x + BAR_ZONE - 2.0 - thickness,
                    bar.thumb.pos.y + 2.0,
                    thickness,
                    bar.thumb.size.y - 4.0,
                ),
                Axis::Horizontal => rect(
                    bar.thumb.pos.x + 2.0,
                    bar.thumb.pos.y + BAR_ZONE - 2.0 - thickness,
                    bar.thumb.size.x - 4.0,
                    thickness,
                ),
            };
            paint.rounded(
                cx,
                thumb,
                with_alpha(colors.muted, if active { 0.6 } else { 0.35 }),
                thickness / 2.0,
                None,
            );
        }
    }

    pub fn toggle_expand(&mut self, path: &str, start: usize) {
        if let Some(model) = self.models.get_mut(path)
            && model.expanded.insert(start)
        {
            self.expansion_revision = self.expansion_revision.wrapping_add(1);
            model.invalidate();
        }
    }

    pub fn line_target(&self, path: &str, index: usize) -> Option<LineTarget> {
        let model = self.models.get(path)?;
        Some(LineTarget {
            path: path.to_owned(),
            line: model.lines.get(index)?.clone(),
        })
    }

    fn cell_text_height(cx: &mut Cx2d, paint: &mut GitPaint, content: &str, width: f64) -> f64 {
        let lh = line_height();
        let size = typography().code_font_size;
        paint
            .measure_wrapped(cx, Font::Mono, size, content, width.max(20.0))
            .max(lh)
    }

    /// Row tops of a loaded body, cached per layout; returns the total height.
    fn ensure_heights(
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        input: &DocInput<'_>,
        model: &mut FileModel,
        column_width: f64,
    ) -> f64 {
        let lh = line_height();
        let gutter = gutter_width(model.max_line);
        let key = (input.split, input.wrap, column_width, crate::theme::epoch());
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
                    if input.wrap && line.kind != LineKind::Header {
                        Self::cell_text_height(cx, paint, &line.content, text_width)
                    } else {
                        lh
                    }
                }
                Row::Pair(left, right) => {
                    let mut height: f64 = lh;
                    for cell in [left, right] {
                        let Some(index) = cell else { continue };
                        let line = &lines[index];
                        let base = if input.wrap {
                            Self::cell_text_height(cx, paint, &line.content, text_width)
                        } else {
                            lh
                        };
                        height = height.max(base);
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
    fn body_height(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        input: &DocInput<'_>,
        file: &Value,
        column_width: f64,
    ) -> f64 {
        let path = text(file, "path");
        let lh = line_height();
        let message = lh + 24.0;
        match input.patches.get(path) {
            None if input.patch_errors.contains_key(path) => message,
            None => {
                ((int(file, "additions") + int(file, "deletions") + 2).clamp(2, 400)) as f64 * lh
            }
            Some(patch) => {
                let status = text(patch, "status");
                if status == "binary" || status == "too_large" {
                    return message;
                }
                let model = self
                    .models
                    .get_mut(path)
                    .expect("loaded patch indexed above");
                if model.lines.is_empty() {
                    return message;
                }
                Self::ensure_heights(cx, paint, input, model, column_width)
            }
        }
    }

    fn order_index(order: &[String], path: &str) -> Option<usize> {
        let i = order.partition_point(|other| compare_diff_paths(other, path).is_lt());
        (order.get(i).is_some_and(|other| other == path)).then_some(i)
    }

    fn rebuild_order(&mut self, files: &[Value]) {
        #[cfg(test)]
        {
            self.layout_builds.0 += 1;
        }
        self.file_indices.clear();
        self.file_indices.extend(0..files.len());
        let path = |i: usize| text(&files[i], "path");
        self.file_indices
            .sort_by(|&a, &b| compare_diff_paths(path(a), path(b)));
        let paths = self.file_indices.iter().map(|&i| path(i));
        if !paths.clone().eq(self.order.iter().map(String::as_str)) {
            let order = paths.map(str::to_owned).collect::<Vec<_>>();
            self.anchor = self.anchor.and_then(|(i, offset)| {
                Self::order_index(&order, &self.order[i]).map(|i| (i, offset))
            });
            // Piece keys index files by order; another file list drops the selection.
            self.selection = None;
            self.order = order;
        }
    }

    fn ensure_geometry(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        input: &DocInput<'_>,
        width: f64,
    ) {
        let key = GeometryKey {
            files_revision: input.files_revision,
            patch_generation: input.patch_generation,
            patch_revision: input.patch_revision,
            collapse_revision: input.collapse_revision,
            expansion_revision: self.expansion_revision,
            width,
            split: input.split,
            wrap: input.wrap,
            theme_epoch: crate::theme::epoch(),
        };
        let previous = self.geometry_key;
        if previous == Some(key) {
            return;
        }
        #[cfg(test)]
        {
            self.layout_builds.1 += 1;
        }
        let files_changed = previous.is_none_or(|old| old.files_revision != key.files_revision);
        if files_changed {
            self.rebuild_order(input.files);
        }
        let patches_replaced =
            previous.is_none_or(|old| old.patch_generation != key.patch_generation);
        if patches_replaced {
            self.models.clear();
        } else if files_changed && !self.models.is_empty() {
            let live: HashSet<&str> = self.order.iter().map(String::as_str).collect();
            self.models.retain(|path, _| live.contains(path.as_str()));
        }
        if files_changed
            || patches_replaced
            || previous.is_none_or(|old| old.patch_revision != key.patch_revision)
        {
            self.widest = (0, 0);
            for path in &self.order {
                let Some(patch) = input.patches.get(path) else {
                    continue;
                };
                let (max_line, max_columns) = self
                    .models
                    .get(path)
                    .map(|model| (model.max_line, model.max_columns))
                    .unwrap_or_else(|| {
                        let model = FileModel::new(patch);
                        let dimensions = (model.max_line, model.max_columns);
                        self.models.insert(path.clone(), model);
                        dimensions
                    });
                self.widest.0 = self.widest.0.max(max_line);
                self.widest.1 = self.widest.1.max(max_columns);
            }
        }
        if self.split != input.split {
            self.selection = None;
            self.split = input.split;
        }
        let column = gutter_width(self.widest.0) + self.widest.1 as f64 * char_width() + 48.0;
        self.content_width = if input.wrap {
            width
        } else {
            width.max(if input.split {
                column * 2.0 + 1.0
            } else {
                column
            })
        };
        let column_width = self.column_width();
        self.blocks.clear();
        self.blocks.reserve(self.file_indices.len());
        let mut top = 0.0;
        for i in 0..self.file_indices.len() {
            let file = &input.files[self.file_indices[i]];
            let collapsed = input.collapsed.contains(text(file, "path"));
            let body = if collapsed {
                0.0
            } else {
                self.body_height(cx, paint, input, file, column_width) + 1.0
            };
            let bottom = top + HEADER_HEIGHT + body;
            self.blocks.push(Block {
                top,
                bottom,
                collapsed,
            });
            top = bottom;
        }
        self.content_height = top + 24.0;
        self.geometry_key = Some(key);
    }

    fn column_width(&self) -> f64 {
        if self.split {
            (self.content_width - 1.0) / 2.0
        } else {
            self.content_width
        }
    }

    /// Draws the document into `area`. Fills [`wanted`](Self::wanted) with
    /// files whose patches should load.
    pub fn draw(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, input: &DocInput<'_>) {
        let colors = palette();
        self.hits.clear();
        self.wanted.clear();
        self.viewport = area;
        for presentation in self.presentations.values_mut() {
            presentation.visited = false;
        }
        if self.background.begin(cx, area, 0, &mut self.hits, paint) {
            paint.fill(cx, area, colors.surface0);
            self.background.end(cx, &self.hits);
        }
        self.ensure_geometry(cx, paint, input, area.size.x);
        // Selection context of this frame: pieces register as they are drawn.
        self.pieces.clear();
        let content_width = self.content_width;
        let column_width = self.column_width();
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
        if let Some(file) = self
            .reveal
            .take()
            .and_then(|path| Self::order_index(&self.order, &path))
        {
            self.scroll_y = self.blocks[file].top;
        } else if let Some((file, offset)) = self.anchor {
            let top = self.blocks[file].top;
            // Scrolling since the last draw moves relative to the anchor.
            self.scroll_y = top + offset + (self.scroll_y - self.anchored_scroll_y);
        }
        let max_y = (self.content_height - area.size.y).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max_y);
        let anchor = self
            .blocks
            .partition_point(|block| block.bottom <= self.scroll_y);
        self.anchor = self
            .blocks
            .get(anchor)
            .map(|block| (anchor, self.scroll_y - block.top));
        self.anchored_scroll_y = self.scroll_y;
        self.scroll_x = if input.wrap {
            0.0
        } else {
            self.scroll_x
                .clamp(0.0, (content_width - area.size.x).max(0.0))
        };
        let view_top = self.scroll_y;
        let view_bottom = self.scroll_y + area.size.y;
        let first = self.blocks.partition_point(|block| block.bottom < view_top);
        let last = self
            .blocks
            .partition_point(|block| block.top <= view_bottom);
        let preload = self
            .blocks
            .partition_point(|block| block.top < view_bottom + area.size.y);
        let syntax = SyntaxThemeId::from_name(input.syntax_theme).unwrap_or(SyntaxThemeId::One);
        let syntax_colors = resolve_syntax_colors(
            syntax,
            if colors.is_dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            },
        );
        paint.begin_clip(cx, area);
        let origin = dvec2(area.pos.x - self.scroll_x, area.pos.y - self.scroll_y);
        for file_index in first..last.max(preload) {
            let Block {
                top,
                bottom,
                collapsed,
            } = self.blocks[file_index];
            if collapsed {
                continue;
            }
            let file = &input.files[self.file_indices[file_index]];
            let path = text(file, "path");
            if file_index >= last {
                // Placeholders just below the viewport preload.
                if !input.patches.contains_key(path) && !input.patch_errors.contains_key(path) {
                    self.wanted.push(path.to_owned());
                }
                continue;
            }
            let body_top = origin.y + top + HEADER_HEIGHT;
            let body_height = bottom - top - HEADER_HEIGHT - 1.0;
            self.draw_body(
                cx,
                paint,
                input,
                file_index,
                path,
                dvec2(origin.x, body_top),
                body_height,
                column_width,
                area,
                &syntax_colors,
            );
            paint.fill(
                cx,
                rect(
                    area.pos.x,
                    body_top + body_height,
                    area.size.x.max(content_width),
                    1.0,
                ),
                colors.border,
            );
        }
        // Sticky headers paint over the rows.
        paint.new_layer(cx);
        for file_index in first..last {
            let Block { top, bottom, .. } = self.blocks[file_index];
            let file = &input.files[self.file_indices[file_index]];
            let path = text(file, "path");
            let natural = area.pos.y + top - self.scroll_y;
            let pinned = natural
                .max(area.pos.y)
                .min(area.pos.y + bottom - self.scroll_y - HEADER_HEIGHT);
            let header = rect(area.pos.x, pinned, area.size.x, HEADER_HEIGHT);
            let key = crate::common::layer::inputs((
                path,
                int(file, "additions"),
                int(file, "deletions"),
                text(file, "status"),
                matches!(self.hovered.as_ref(), Some(DocHit::Header(hovered)) if hovered == path),
                rect_key(area),
                crate::theme::epoch(),
                self.row_selection(file_index, 0),
            ));
            let id = (file_index, 0, 0);
            let mut presentation = self.presentations.remove(&id).unwrap_or_default();
            if presentation.begin(cx, paint, header, key, &mut self.hits, &mut self.pieces) {
                self.draw_header(cx, paint, file_index, file, path, header, area);
                self.draw_selection(cx, paint, true, presentation.piece_mark);
                presentation.end(cx, &self.hits, &self.pieces);
            }
            self.presentations.insert(id, presentation);
        }
        let bars_key = crate::common::layer::inputs((
            self.scroll_x.to_bits(),
            self.scroll_y.to_bits(),
            self.content_width.to_bits(),
            self.content_height.to_bits(),
            self.bar_hovered,
            self.bar_drag.map(|(axis, _, _)| axis),
            input.wrap,
        ));
        if self
            .scrollbar_layer
            .begin(cx, area, bars_key, &mut self.hits, paint)
        {
            self.draw_scrollbars(cx, paint, area, input.wrap);
            self.scrollbar_layer.end(cx, &self.hits);
        }
        self.presentations
            .retain(|_, presentation| presentation.visited);
        paint.end_clip(cx);
        if autoscroll {
            // The held pointer keeps extending the selection over newly scrolled text.
            if let Some(pointer) = self.drag_pointer {
                let inside = dvec2(
                    pointer.x.clamp(area.pos.x, area.pos.x + area.size.x),
                    pointer
                        .y
                        .clamp(area.pos.y + 1.0, area.pos.y + area.size.y - 1.0),
                );
                if let (Some(pos), Some((anchor, _))) = (self.pos_at(inside), self.selection) {
                    self.selection = Some((anchor, pos));
                }
            }
            let mut redraw = Area::Empty;
            cx.add_rect_area(&mut redraw, area);
            crate::common::clock::register_every(cx, redraw, crate::common::clock::MOTION_PERIOD);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_header(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        file_index: usize,
        file: &Value,
        path: &str,
        header: Rect,
        clip: Rect,
    ) {
        let colors = palette();
        let hit = DocHit::Header(path.to_owned());
        let hovered = self.hovered.as_ref() == Some(&hit);
        paint.fill(
            cx,
            header,
            if hovered {
                colors.surface1
            } else {
                colors.surface0
            },
        );
        paint.fill(
            cx,
            rect(
                header.pos.x,
                header.pos.y + header.size.y - 1.0,
                header.size.x,
                1.0,
            ),
            colors.border_accent,
        );
        let center_y = header.pos.y + (HEADER_HEIGHT - 1.0) / 2.0;
        let right = header.pos.x + header.size.x - 8.0;
        change_icon(cx, paint, dvec2(right - 7.0, center_y), change_kind(file));
        let stat_right = right - 14.0 - 8.0;
        let stat_width =
            measure_diff_stat(cx, paint, int(file, "additions"), int(file, "deletions"));
        diff_stat(
            cx,
            paint,
            stat_right,
            center_y,
            int(file, "additions"),
            int(file, "deletions"),
            true,
        );
        let size = ui(14.0);
        let name_left = header.pos.x + 12.0;
        let available = (stat_right - stat_width - 4.0 - name_left).max(0.0);
        let (directory, name) = match path.rfind('/') {
            Some(slash) => (&path[..slash], &path[slash + 1..]),
            None => ("", path),
        };
        let piece_clip = intersect(header, clip);
        let drawn = paint.text_centered(
            cx,
            Font::Regular,
            size,
            colors.foreground,
            name_left,
            header.pos.y,
            HEADER_HEIGHT - 1.0,
            name,
            available,
        );
        if let Some((laid, origin)) = paint.line_layout(
            cx,
            Font::Regular,
            size,
            name_left,
            header.pos.y,
            HEADER_HEIGHT - 1.0,
            name,
            available,
        ) {
            self.pieces.push(Piece {
                key: (file_index, 0, 0),
                laid,
                origin,
                clip: piece_clip,
                header: true,
            });
        }
        if !directory.is_empty() && drawn + 4.0 < available {
            let x = name_left + drawn + 4.0;
            paint.text_centered(
                cx,
                Font::Regular,
                size,
                colors.muted,
                x,
                header.pos.y,
                HEADER_HEIGHT - 1.0,
                directory,
                available - drawn - 4.0,
            );
            if let Some((laid, origin)) = paint.line_layout(
                cx,
                Font::Regular,
                size,
                x,
                header.pos.y,
                HEADER_HEIGHT - 1.0,
                directory,
                available - drawn - 4.0,
            ) {
                self.pieces.push(Piece {
                    key: (file_index, 0, 1),
                    laid,
                    origin,
                    clip: piece_clip,
                    header: true,
                });
            }
        }
        self.hits.push_clipped(header, clip, hit);
    }

    fn draw_message(
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        origin: DVec2,
        width: f64,
        message: &str,
        action: Option<&str>,
    ) -> Option<Rect> {
        let colors = palette();
        let lh = line_height();
        let band = rect(origin.x, origin.y, width, lh + 24.0);
        paint.fill(cx, band, colors.surface0);
        let size = typography().code_font_size;
        let action_width = action
            .map(|a| paint.measure(cx, Font::Medium, ui(12.0), a).x + 16.0)
            .unwrap_or(0.0);
        let drawn = paint.text_centered(
            cx,
            Font::Mono,
            size,
            colors.muted,
            origin.x + 12.0,
            band.pos.y,
            band.size.y,
            message,
            (width - 24.0 - action_width - 8.0).max(0.0),
        );
        let action = action?;
        let button = rect(
            origin.x + 12.0 + drawn + 8.0,
            band.pos.y + (band.size.y - 24.0) / 2.0,
            action_width,
            24.0,
        );
        paint.text_centered(
            cx,
            Font::Medium,
            ui(12.0),
            colors.muted,
            button.pos.x + 8.0,
            button.pos.y,
            24.0,
            action,
            action_width,
        );
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
        let width = column_width * if input.split { 2.0 } else { 1.0 }
            + if input.split { 1.0 } else { 0.0 };
        let Some(patch) = input.patches.get(path) else {
            if let Some(error) = input.patch_errors.get(path) {
                let retry = t("common.actions.retry");
                if let Some(button) = Self::draw_message(
                    cx,
                    paint,
                    dvec2(clip.pos.x, origin.y),
                    clip.size.x,
                    error,
                    Some(&retry),
                ) {
                    self.hits
                        .push_clipped(button, clip, DocHit::Retry(path.to_owned()));
                }
                return;
            }
            // Placeholder sized like the expected patch; requests it once drawn.
            let lh = line_height();
            paint.fill(
                cx,
                rect(clip.pos.x, origin.y, clip.size.x, body_height),
                colors.surface0,
            );
            let band = rect(clip.pos.x + 12.0, origin.y + 8.0, clip.size.x - 24.0, lh);
            let spinner_top = origin.y.max(clip.pos.y + HEADER_HEIGHT);
            paint.begin_clip(
                cx,
                rect(
                    clip.pos.x,
                    spinner_top,
                    clip.size.x,
                    clip.pos.y + clip.size.y - spinner_top,
                ),
            );
            paint.spinner(
                cx,
                dvec2(band.pos.x + 6.0, band.pos.y + lh / 2.0),
                12.0,
                colors.muted,
            );
            paint.end_clip(cx);
            paint.text_centered(
                cx,
                Font::Regular,
                ui(12.0),
                colors.muted,
                band.pos.x + 12.0 + 8.0,
                band.pos.y,
                lh,
                &t("workspace.git.diff.loadingPatch"),
                band.size.x - 20.0,
            );
            self.wanted.push(path.to_owned());
            return;
        };
        let status = text(patch, "status");
        if status == "binary" || status == "too_large" {
            let key = if status == "binary" {
                "workspace.git.diff.binaryFile"
            } else {
                "workspace.git.diff.tooLarge"
            };
            Self::draw_message(
                cx,
                paint,
                dvec2(clip.pos.x, origin.y),
                clip.size.x,
                &t(key),
                None,
            );
            return;
        }
        let (model_path, mut model) = self
            .models
            .remove_entry(path)
            .expect("loaded patch indexed above");
        if model.lines.is_empty() {
            self.models.insert(model_path, model);
            Self::draw_message(
                cx,
                paint,
                dvec2(clip.pos.x, origin.y),
                clip.size.x,
                &t("workspace.git.diff.noTextualChanges"),
                None,
            );
            return;
        }
        let lh = line_height();
        let view_top = clip.pos.y;
        let view_bottom = clip.pos.y + clip.size.y;
        // The visible row range by binary search over the cached row tops.
        let tops = &model
            .heights
            .as_ref()
            .expect("visible patch has geometry")
            .1;
        let first = tops
            .partition_point(|top| origin.y + top < view_top)
            .saturating_sub(1);
        let last = tops.partition_point(|top| origin.y + top <= view_bottom);
        let gutter = gutter_width(model.max_line);
        for row_index in first..last {
            let tops = &model
                .heights
                .as_ref()
                .expect("visible patch has geometry")
                .1;
            let row = model.rows.as_ref().expect("visible patch has rows").1[row_index];
            let top = origin.y + tops[row_index];
            let bottom = tops
                .get(row_index + 1)
                .map(|next| origin.y + next)
                .unwrap_or(origin.y + body_height);
            let row_key = row_index + 1;
            if bottom < view_top {
                continue;
            }
            let id = (file_index, row_key, 0);
            let hovered = match self.hovered.as_ref() {
                Some(DocHit::Expand {
                    path: hovered_path,
                    start,
                }) => {
                    hovered_path == path
                        && matches!(row, Row::Hidden { start: row_start, .. } if row_start == *start)
                }
                Some(
                    DocHit::Gutter {
                        path: hovered_path,
                        line,
                    }
                    | DocHit::LineMenu {
                        path: hovered_path,
                        line,
                    },
                ) => {
                    hovered_path == path
                        && match row {
                            Row::Line(index) => index == *line,
                            Row::Pair(left, right) => left == Some(*line) || right == Some(*line),
                            Row::Hidden { .. } => false,
                        }
                }
                _ => false,
            };
            let key = crate::common::layer::inputs((
                path,
                row,
                input.patch_generation,
                input.split,
                input.wrap,
                input.syntax_theme,
                model.expanded.len(),
                hovered.then_some(&self.hovered),
                rect_key(clip),
                crate::theme::epoch(),
                self.row_selection(file_index, row_key),
            ));
            let mut presentation = self.presentations.remove(&id).unwrap_or_default();
            let band = rect(origin.x, top, width, bottom - top);
            if !presentation.begin(cx, paint, band, key, &mut self.hits, &mut self.pieces) {
                self.presentations.insert(id, presentation);
                continue;
            }
            match row {
                Row::Hidden { start, end } => {
                    let band = rect(origin.x, top, width, lh);
                    let hit = DocHit::Expand {
                        path: path.to_owned(),
                        start,
                    };
                    if self.hovered.as_ref() == Some(&hit) {
                        paint.fill(cx, band, colors.surface1);
                    }
                    let x = origin.x + gutter + 9.0;
                    paint.icon(
                        cx,
                        "chevrons-up-down",
                        14.0,
                        colors.muted,
                        dvec2(x + 7.0, top + lh / 2.0),
                    );
                    let label = hidden_label(start, end);
                    let code = typography().code_font_size;
                    paint.text_centered(
                        cx,
                        Font::Mono,
                        code,
                        colors.muted,
                        x + 14.0 + 8.0,
                        top,
                        lh,
                        &label,
                        width,
                    );
                    if let Some((laid, origin)) = paint.line_layout(
                        cx,
                        Font::Mono,
                        code,
                        x + 14.0 + 8.0,
                        top,
                        lh,
                        &label,
                        width,
                    ) {
                        self.pieces.push(Piece {
                            key: (file_index, row_key, 0),
                            laid,
                            origin,
                            clip: intersect(band, clip),
                            header: false,
                        });
                    }
                    self.hits.push_clipped(band, clip, hit);
                }
                Row::Line(index) => {
                    let runs = Self::cell_runs(&mut model, path, index, input.wrap);
                    let line = &model.lines[index];
                    let side = if line.kind == LineKind::Remove {
                        "old"
                    } else {
                        "new"
                    };
                    let cell = rect(origin.x, top, width, bottom - top);
                    self.draw_cell(
                        cx,
                        paint,
                        input,
                        path,
                        (file_index, row_key, 0),
                        Some((index, line)),
                        runs,
                        side,
                        cell,
                        gutter,
                        clip,
                        syntax,
                    );
                }
                Row::Pair(left, right) => {
                    let left_runs =
                        left.and_then(|i| Self::cell_runs(&mut model, path, i, input.wrap));
                    let right_runs =
                        right.and_then(|i| Self::cell_runs(&mut model, path, i, input.wrap));
                    let left_cell = rect(origin.x, top, column_width, bottom - top);
                    let right_cell = rect(
                        origin.x + column_width + 1.0,
                        top,
                        column_width,
                        bottom - top,
                    );
                    let left_line = left.map(|i| (i, &model.lines[i]));
                    let right_line = right.map(|i| (i, &model.lines[i]));
                    self.draw_cell(
                        cx,
                        paint,
                        input,
                        path,
                        (file_index, row_key, 0),
                        left_line,
                        left_runs,
                        "old",
                        left_cell,
                        gutter,
                        clip,
                        syntax,
                    );
                    self.draw_cell(
                        cx,
                        paint,
                        input,
                        path,
                        (file_index, row_key, 1),
                        right_line,
                        right_runs,
                        "new",
                        right_cell,
                        gutter,
                        clip,
                        syntax,
                    );
                }
            }
            self.draw_selection(cx, paint, false, presentation.piece_mark);
            presentation.end(cx, &self.hits, &self.pieces);
            self.presentations.insert(id, presentation);
        }
        self.models.insert(model_path, model);
    }

    /// Syntax runs of a line (none for headers and wrapped lines).
    fn cell_runs(model: &mut FileModel, path: &str, index: usize, wrap: bool) -> Option<Vec<Run>> {
        if wrap || model.lines[index].kind == LineKind::Header {
            return None;
        }
        model.runs(path).get(&index).cloned()
    }

    /// Original `_cell`: gutter sized to the widest number, a 1px rule, then
    /// syntax-coloured code inset by 8px and the line actions button.
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
        let line_box = cell;
        paint.fill(cx, line_box, background);
        let number = gutter_number(line, side);
        let gutter_rect = rect(cell.pos.x, cell.pos.y, gutter, line_box.size.y);
        let (file_index, row_key, side_index) = piece;
        let piece_clip = intersect(cell, clip);
        if let Some(number) = number {
            let label = number.to_string();
            let label_width = paint.measure(cx, Font::Mono, code_size, &label).x;
            let x = gutter_rect.pos.x + gutter - 7.0 - label_width;
            paint.text_centered(
                cx,
                Font::Mono,
                code_size,
                number_color,
                x,
                cell.pos.y,
                lh,
                &label,
                label_width + 1.0,
            );
            if let Some((laid, origin)) = paint.line_layout(
                cx,
                Font::Mono,
                code_size,
                x,
                cell.pos.y,
                lh,
                &label,
                label_width + 1.0,
            ) {
                self.pieces.push(Piece {
                    key: (file_index, row_key, side_index * 2),
                    laid,
                    origin,
                    clip: piece_clip,
                    header: false,
                });
            }
            self.hits.push_clipped(
                gutter_rect,
                clip,
                DocHit::Gutter {
                    path: path.to_owned(),
                    line: index,
                },
            );
        }
        paint.fill(
            cx,
            rect(cell.pos.x + gutter, cell.pos.y, 1.0, line_box.size.y),
            colors.border,
        );
        let text_x = cell.pos.x + gutter + 1.0 + 8.0;
        let text_width =
            (cell.size.x - gutter - 1.0 - 8.0 - 4.0 - if header { 0.0 } else { 24.0 }).max(0.0);
        let foreground = if header {
            colors.muted
        } else {
            colors.foreground
        };
        let code_key = (file_index, row_key, side_index * 2 + 1);
        if input.wrap && !header {
            let origin = dvec2(text_x, cell.pos.y + (lh - code_size * 1.2) / 2.0);
            paint.text_wrapped(
                cx,
                Font::Mono,
                code_size,
                foreground,
                origin,
                &line.content,
                text_width,
            );
            if !line.content.is_empty() {
                let laid =
                    paint.wrapped_layout(cx, Font::Mono, code_size, &line.content, text_width);
                self.pieces.push(Piece {
                    key: code_key,
                    laid,
                    origin,
                    clip: piece_clip,
                    header: false,
                });
            }
        } else {
            // One layout of the whole line gives the run geometry (monospace).
            let max_width = if input.wrap {
                text_width
            } else {
                f64::MAX / 4.0
            };
            if let Some((laid, origin)) = paint.line_layout(
                cx,
                Font::Mono,
                code_size,
                text_x,
                cell.pos.y,
                lh,
                &line.content,
                max_width,
            ) {
                self.pieces.push(Piece {
                    key: code_key,
                    laid,
                    origin,
                    clip: piece_clip,
                    header: false,
                });
            }
            match runs {
                Some(runs) => {
                    let mut x = text_x;
                    for (run, style) in runs {
                        if x > clip.pos.x + clip.size.x {
                            break;
                        }
                        let color = style.map(|s| hex(syntax.color(s))).unwrap_or(foreground);
                        x = paint.line_run(
                            cx,
                            Font::Mono,
                            code_size,
                            color,
                            text_x,
                            x,
                            cell.pos.y,
                            lh,
                            &run,
                            clip.pos.x,
                        );
                    }
                }
                None => {
                    paint.text_centered(
                        cx,
                        Font::Mono,
                        code_size,
                        foreground,
                        text_x,
                        cell.pos.y,
                        lh,
                        &line.content,
                        if input.wrap {
                            text_width
                        } else {
                            f64::MAX / 4.0
                        },
                    );
                }
            }
        }
        if !header && piece_clip.size.x > 0.0 {
            // Anchored to the visible end of the cell: with long unwrapped lines the cell
            // reaches past the viewport, and the button stays on screen with the line.
            let right = (piece_clip.pos.x + piece_clip.size.x).min(cell.pos.x + cell.size.x);
            let button = rect(
                (right - 4.0 - 24.0).max(cell.pos.x + gutter + 1.0),
                cell.pos.y,
                24.0,
                lh,
            );
            let hit = DocHit::LineMenu {
                path: path.to_owned(),
                line: index,
            };
            if !input.wrap {
                // Unwrapped code runs under the button; mask it.
                paint.fill(cx, button, colors.surface0);
                paint.fill(cx, button, background);
            }
            if self.hovered.as_ref() == Some(&hit) {
                paint.rounded(cx, button, colors.surface2, 4.0, None);
            }
            paint.icon(cx, "ellipsis", 14.0, colors.muted, button.center());
            self.hits.push_clipped(button, clip, hit);
        }
    }

    /// Whether `point` is inside the drawn document.
    pub fn contains(&self, point: DVec2) -> bool {
        self.viewport.size.x > 0.0 && self.viewport.size.y > 0.0 && self.viewport.contains(point)
    }
}

/// A rect row used for the jump list sorting (keeps the document order).
pub fn sorted_paths(files: &[Value]) -> Vec<String> {
    let mut paths: Vec<String> = files.iter().map(|f| text(f, "path").to_owned()).collect();
    paths.sort_by(|a, b| compare_diff_paths(a, b));
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use makepad_widgets::makepad_draw::CxDraw;
    use serde_json::json;

    #[test]
    fn draw_reuses_diff_geometry_until_layout_inputs_change() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        crate::common::paint::live_design(&mut cx);
        crate::features::files::kit::live_design(&mut cx);
        super::super::paint::live_design(&mut cx);
        GitPaint::register_main_module(&mut cx);
        cx.live_expand();
        cx.live_scan_dependencies();
        cx.native_load_dependencies();
        let mut paint = GitPaint::new_local(&mut cx);
        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(400.0, 300.0));
        let mut root = DrawList2d::new(&mut cx);
        let event = DrawEvent::default();
        let height = HEADER_HEIGHT + 4.0 * line_height();
        let mut draw = |doc: &mut DiffDoc, input: &DocInput<'_>, width| {
            let mut draw = CxDraw::new(&mut cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            root.begin_always(&mut draw);
            let mut draw = Cx2d::new(&mut draw);
            draw.begin_pass_sized_turtle(Layout::default());
            doc.draw(&mut draw, &mut paint, rect(0.0, 0.0, width, height), input);
            draw.end_pass_sized_turtle();
            root.end(&mut draw);
            draw.end_pass(&pass);
        };

        let files = [
            json!({"path": "c.rs", "additions": 40}),
            json!({"path": "b.rs", "additions": 4}),
            json!({"path": "dir/a.rs", "additions": 50}),
        ];
        let no_patches = HashMap::new();
        let no_errors = HashMap::new();
        let open = HashSet::new();
        let mut input = DocInput {
            files: &files,
            files_revision: 0,
            patches: &no_patches,
            patch_generation: 0,
            patch_errors: &no_errors,
            patch_revision: 0,
            collapsed: &open,
            collapse_revision: 0,
            split: false,
            wrap: false,
            syntax_theme: "one",
        };
        let mut doc = DiffDoc::default();
        draw(&mut doc, &input, 220.0);
        assert_eq!(doc.layout_builds, (1, 1));
        assert_eq!(doc.order, ["dir/a.rs", "b.rs", "c.rs"]);
        assert_eq!(doc.wanted, ["dir/a.rs"]);
        doc.scroll_y = 8.0;
        draw(&mut doc, &input, 220.0);
        assert_eq!(doc.layout_builds, (1, 1));
        let header = doc
            .hits
            .rect_of(&DocHit::Header("dir/a.rs".into()))
            .unwrap();
        assert_eq!(header.pos.y, 0.0);

        let collapsed = HashSet::from(["dir/a.rs".to_owned()]);
        input.collapsed = &collapsed;
        input.collapse_revision += 1;
        doc.scroll_y = 0.0;
        draw(&mut doc, &input, 220.0);
        assert_eq!(doc.layout_builds, (1, 2));
        assert_eq!(doc.blocks[1].top, HEADER_HEIGHT);
        assert_eq!(doc.wanted, ["b.rs", "c.rs"]);

        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (1, 3));
        assert_eq!(doc.content_width, 240.0);
        let old_end = doc.blocks[1].bottom;
        let context = vec![json!({"type": "context", "content": "unchanged"}); 12];
        let patches = HashMap::from([(
            "b.rs".to_owned(),
            json!({"hunks": [{"oldStart": 1, "newStart": 1, "lines": context}]}),
        )]);
        input.patches = &patches;
        input.patch_revision += 1;
        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (1, 4));
        assert!(doc.blocks[1].bottom > old_end);
        assert!(!doc.wanted.iter().any(|path| path == "b.rs"));

        let old_end = doc.blocks[1].bottom;
        doc.toggle_expand("b.rs", 1);
        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (1, 5));
        assert!(doc.blocks[1].bottom > old_end);

        let errors = HashMap::from([("c.rs".to_owned(), "Patch error".to_owned())]);
        input.patch_errors = &errors;
        input.patch_revision += 1;
        let old_end = doc.blocks[2].bottom;
        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (1, 6));
        assert!(doc.blocks[2].bottom < old_end);

        doc.reveal("b.rs");
        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (1, 6));
        assert_eq!(doc.scroll_y, doc.blocks[1].top);
        doc.select_all();
        let updated_files = [
            files[0].clone(),
            json!({"path": "aaa/new.rs"}),
            files[1].clone(),
            files[2].clone(),
        ];
        input.files = &updated_files;
        input.files_revision += 1;
        draw(&mut doc, &input, 240.0);
        assert_eq!(doc.layout_builds, (2, 7));
        assert_eq!(doc.order, ["aaa/new.rs", "dir/a.rs", "b.rs", "c.rs"]);
        assert_eq!(doc.scroll_y, doc.blocks[2].top);
        assert!(doc.selection.is_none());
    }

    #[test]
    fn hidden_document_releases_scrollbars_and_selection() {
        let viewport = rect(10.0, 20.0, 100.0, 100.0);
        let track = rect(98.0, 20.0, 12.0, 100.0);
        let point = dvec2(104.0, 30.0);
        let mut doc = DiffDoc {
            viewport,
            bars: [
                ScrollBar {
                    track,
                    thumb: rect(98.0, 20.0, 12.0, 24.0),
                },
                ScrollBar::default(),
            ],
            ..DiffDoc::default()
        };
        assert!(doc.contains(point));
        assert!(doc.press_bar(point));
        doc.select_all();
        doc.hide();
        assert!(!doc.contains(point));
        assert_eq!(doc.bar_at(point), None);
        assert!(!doc.drag_bar(point));
        assert!(doc.selection.is_none());
    }

    #[test]
    fn display_columns_follow_the_layout_tab_stops() {
        assert_eq!(display_columns(""), 0);
        assert_eq!(display_columns("\t\tfoo"), 11);
        assert_eq!(display_columns("ab\tc"), 5);
        assert_eq!(display_columns("abcd\t"), 8);
    }
}
