use {
    super::{
        color::Color,
        font::{Font, FontId, GlyphId},
        font_family::{FontFamily, FontFamilyId},
        geom::{Point, Rect, Size},
        loader::{self, FontDefinition, FontFamilyDefinition, Loader},
        rasterizer::{self, RasterizedGlyph, Rasterizer},
        sdfer,
        selection::{Cursor, CursorPosition, Selection},
        shaper::{self, ShapedText},
        source_index::{GlyphIndex, SourceIndex},
        substr::Substr,
    },
    std::{
        borrow::Borrow,
        cell::RefCell,
        collections::{HashMap, VecDeque},
        hash::{Hash, Hasher},
        rc::Rc,
        sync::Arc,
    },
    unicode_segmentation::UnicodeSegmentation,
};

const LPXS_PER_INCH: f32 = 96.0;
const PTS_PER_INCH: f32 = 72.0;

#[derive(Debug)]
pub struct Layouter {
    loader: Loader,
    cache_size: usize,
    cached_params: VecDeque<OwnedLayoutParams>,
    cached_results: HashMap<OwnedLayoutParams, Rc<LaidoutText>>,
}

impl Layouter {
    pub fn new(settings: Settings) -> Self {
        Self {
            loader: Loader::new(settings.loader),
            cache_size: settings.cache_size,
            cached_params: VecDeque::with_capacity(settings.cache_size),
            cached_results: HashMap::with_capacity(settings.cache_size),
        }
    }

    pub fn rasterizer(&self) -> &Rc<RefCell<Rasterizer>> {
        self.loader.rasterizer()
    }

    pub fn is_font_family_known(&self, id: FontFamilyId) -> bool {
        self.loader.is_font_family_known(id)
    }

    pub fn is_font_known(&self, id: FontId) -> bool {
        self.loader.is_font_known(id)
    }

    pub fn define_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        self.loader.define_font_family(id, definition);
    }

    pub fn define_font(&mut self, id: FontId, definition: FontDefinition) {
        self.loader.define_font(id, definition);
    }

    /// Replaces a family's font list and forgets every cached layout, since any
    /// of them may have shaped text with the previous list.
    pub fn redefine_font_family(&mut self, id: FontFamilyId, definition: FontFamilyDefinition) {
        self.loader.redefine_font_family(id, definition);
        self.cached_params.clear();
        self.cached_results.clear();
    }

    /// Loads a font that a layout worker already placed glyphs from. The worker
    /// parsed the same immutable definition bytes, so loading cannot fail here.
    pub(super) fn load_font(&mut self, id: FontId) -> Rc<Font> {
        self.loader
            .get_or_load_font(id)
            .expect("font placed by the layout worker must load")
            .clone()
    }

    /// Loads a defined font; `None` when its face does not parse.
    pub fn try_load_font(&mut self, id: FontId) -> Option<Rc<Font>> {
        self.loader.get_or_load_font(id).cloned()
    }

    /// Uncached authoritative layout for the worker; never hashes or copies the source.
    pub(super) fn layout_shared(
        &mut self,
        text: Substr,
        spans: &[Span],
        options: LayoutOptions,
    ) -> LaidoutText {
        LayoutContext::new(&mut self.loader, text, options).layout(spans)
    }

    pub fn get_or_layout(&mut self, params: impl LayoutParams) -> Rc<LaidoutText> {
        if let Some(result) = self.cached_results.get(&params as &dyn LayoutParams) {
            return result.clone();
        }
        if self.cached_params.len() == self.cache_size {
            let params = self.cached_params.pop_front().unwrap();
            self.cached_results.remove(&params);
        }
        let params = params.to_owned();
        let result = Rc::new(self.layout(params.clone()));
        self.cached_params.push_back(params.clone());
        self.cached_results.insert(params, result.clone());
        result
    }

    fn layout(&mut self, params: OwnedLayoutParams) -> LaidoutText {
        LayoutContext::new(&mut self.loader, params.text, params.options).layout(&params.spans)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub loader: loader::Settings,
    pub cache_size: usize,
}

impl Default for Settings {
    fn default() -> Self {
        let r = Self {
            loader: loader::Settings {
                shaper: shaper::Settings { cache_size: 4096 },
                rasterizer: rasterizer::Settings {
                    sdfer: sdfer::Settings {
                        padding: 4,
                        radius: 8.0,
                        cutoff: 0.25,
                    },
                    grayscale_atlas_size: Size::new(4096, 4096),
                    color_atlas_size: Size::new(2048, 2048),
                },
            },
            cache_size: 4096,
        };
        r
    }
}

#[derive(Debug)]
struct LayoutContext<'a> {
    loader: &'a mut Loader,
    text: Substr,
    options: LayoutOptions,
    current_point_in_lpxs: Point<f32>,
    current_row_start: usize,
    current_row_end: usize,
    rows: Vec<LaidoutRow>,
    glyphs: Vec<LaidoutGlyph>,
}

impl<'a> LayoutContext<'a> {
    fn new(loader: &'a mut Loader, text: Substr, options: LayoutOptions) -> Self {
        Self {
            loader,
            text,
            options,
            current_point_in_lpxs: Point::new(options.first_row_indent_in_lpxs, 0.0),
            current_row_start: 0,
            current_row_end: 0,
            rows: Vec::new(),
            glyphs: Vec::new(),
        }
    }

    fn current_row_is_first(&self) -> bool {
        self.rows.is_empty()
    }

    fn current_row_is_continuation(&self) -> bool {
        self.current_row_is_first() && self.options.first_row_indent_in_lpxs > 0.0
    }

    fn current_row_is_empty(&self) -> bool {
        self.current_row_start == self.current_row_end
    }

    fn current_row_len(&self) -> usize {
        self.current_row_end - self.current_row_start
    }

    fn span_text(&self, len: usize) -> Substr {
        self.text
            .substr(self.current_row_end..self.current_row_end + len)
    }

    fn remaining_width_in_lpxs(&self) -> Option<f32> {
        if self.options.wrap {
            self.options
                .max_width_in_lpxs
                .map(|max_width_in_lpxs| max_width_in_lpxs - self.current_point_in_lpxs.x)
        } else {
            None
        }
    }

    fn layout(mut self, spans: &[Span]) -> LaidoutText {
        for (span_index, span) in spans.iter().enumerate() {
            self.layout_span_multiline(span, span_index == spans.len() - 1);
        }
        self.finish()
    }

    fn layout_span_multiline(&mut self, span: &Span, is_last: bool) {
        let font_family = self
            .loader
            .get_or_load_font_family(span.style.font_family_id)
            .clone();
        for (line_index, len) in self
            .span_text(span.len)
            .split('\n')
            .map(|line| line.len())
            .enumerate()
        {
            if line_index != 0 {
                self.finish_current_row(&font_family, span.style, true);
            }
            self.layout_span(&font_family, span.style, len);
        }
        if is_last {
            self.finish_current_row(&font_family, span.style, false);
        }
    }

    fn layout_span(&mut self, font_family: &Rc<FontFamily>, style: Style, len: usize) {
        if self.remaining_width_in_lpxs().is_none() {
            self.layout_span_directly(font_family, style, len);
        } else {
            self.layout_span_by_word(font_family, style, len);
        }
    }

    fn layout_span_by_word(&mut self, font_family: &Rc<FontFamily>, style: Style, len: usize) {
        let mut fitter = Fitter::new(
            self.span_text(len),
            font_family.clone(),
            style.font_size_in_lpxs(),
            SegmentKind::Word,
        );
        while !fitter.is_empty() {
            match fitter.fit(self.current_point_in_lpxs.x, self.remaining_width_in_lpxs().unwrap()) {
                Some(text) => self.append_text(style, &text),
                None => {
                    if self.current_row_is_empty() && !self.current_row_is_continuation() {
                        self.layout_span_by_grapheme(font_family, style, fitter.pop());
                    } else {
                        self.finish_current_row(font_family, style, false);
                    }
                }
            }
        }
    }

    fn layout_span_by_grapheme(&mut self, font_family: &Rc<FontFamily>, style: Style, len: usize) {
        let mut fitter = Fitter::new(
            self.span_text(len),
            font_family.clone(),
            style.font_size_in_lpxs(),
            SegmentKind::Grapheme,
        );
        while !fitter.is_empty() {
            match fitter.fit(self.current_point_in_lpxs.x, self.remaining_width_in_lpxs().unwrap()) {
                Some(text) => self.append_text(style, &text),
                None => {
                    if self.current_row_is_empty() {
                        self.layout_span_directly(font_family, style, fitter.pop());
                    } else {
                        self.finish_current_row(font_family, style, false);
                    }
                }
            }
        }
    }

    fn layout_span_directly(&mut self, font_family: &FontFamily, style: Style, len: usize) {
        self.append_text(
            style,
            &font_family.get_or_shape(
                self.text
                    .substr(self.current_row_end..self.current_row_end + len),
            ),
        );
    }

    fn append_text(&mut self, style: Style, text: &ShapedText) {
        use super::num::Zero;

        let font_size_in_lpxs = style.font_size_in_lpxs();
        for glyph in &text.glyphs {
            let mut glyph = LaidoutGlyph {
                origin_in_lpxs: Point::ZERO,
                font: glyph.font.clone(),
                font_size_in_lpxs,
                color: style.color,
                id: glyph.id,
                cluster: self.current_row_len() + glyph.cluster,
                advance_in_ems: glyph.advance_in_ems_at(self.current_point_in_lpxs.x, font_size_in_lpxs),
                offset_in_ems: glyph.offset_in_ems,
                paint_bounds_in_ems: glyph.font.paint_bounds_in_ems(glyph.id),
            };
            glyph.origin_in_lpxs.x = self.current_point_in_lpxs.x;
            self.current_point_in_lpxs.x += glyph.advance_in_lpxs();
            self.glyphs.push(glyph);
        }
        self.current_row_end += text.text.len();
    }

    fn finish_current_row(
        &mut self,
        fallback_font_family: &FontFamily,
        fallback_style: Style,
        newline: bool,
    ) {
        use {super::num::Zero, std::mem};

        let glyphs = mem::take(&mut self.glyphs);
        let fallback_font = fallback_font_family.fonts().get(0);
        let fallback_font_size_in_lpxs = fallback_style.font_size_in_lpxs();
        let fallback_ascender_in_lpxs =
            fallback_font.map_or(0.0, |font| font.ascender_in_ems()) * fallback_font_size_in_lpxs;
        let fallback_descender_in_lpxs =
            fallback_font.map_or(0.0, |font| font.descender_in_ems()) * fallback_font_size_in_lpxs;
        let fallback_line_gap_in_lpxs =
            fallback_font.map_or(0.0, |font| font.line_gap_in_ems()) * fallback_font_size_in_lpxs;

        let text = self
            .text
            .substr(self.current_row_start..self.current_row_end);
        let width_in_lpxs = self.current_point_in_lpxs.x;
        let ascender_in_lpxs = glyphs
            .iter()
            .map(|glyph| glyph.ascender_in_lpxs())
            .reduce(f32::max)
            .unwrap_or(fallback_ascender_in_lpxs);
        let descender_in_lpxs = glyphs
            .iter()
            .map(|glyph| glyph.descender_in_lpxs())
            .reduce(f32::min)
            .unwrap_or(fallback_descender_in_lpxs);
        let line_gap_in_lpxs = glyphs
            .iter()
            .map(|glyph| glyph.line_gap_in_lpxs())
            .reduce(f32::max)
            .unwrap_or(fallback_line_gap_in_lpxs);
        let line_spacing_scale = self.options.line_spacing_scale;
        let line_spacing_above_in_lpxs = ascender_in_lpxs * line_spacing_scale;
        let line_spacing_below_in_lpxs =
            (-descender_in_lpxs + line_gap_in_lpxs) * line_spacing_scale;
        let line_spacing_below_in_lpxs =
            line_spacing_below_in_lpxs.max(if self.current_row_is_first() {
                self.options.first_row_min_line_spacing_below_in_lpxs - line_spacing_above_in_lpxs
            } else {
                0.0
            });
        let glyph_index = Arc::new(GlyphIndex::new(&text, &glyphs, width_in_lpxs));
        let mut row = LaidoutRow {
            origin_in_lpxs: Point::ZERO,
            text,
            newline,
            width_in_lpxs,
            ascender_in_lpxs,
            descender_in_lpxs,
            line_gap_in_lpxs,
            line_spacing_above_in_lpxs,
            line_spacing_below_in_lpxs,
            glyph_index,
            glyphs,
        };

        self.current_point_in_lpxs.x = 0.0;
        self.current_point_in_lpxs.y += self.rows.last().map_or(row.ascender_in_lpxs, |prev_row| {
            prev_row.line_spacing_in_lpxs(&row)
        });
        let max_width_in_lpxs = self.options.max_width_in_lpxs.unwrap_or(row.width_in_lpxs);
        let remaining_width_in_lpxs = max_width_in_lpxs - row.width_in_lpxs;
        row.origin_in_lpxs.x = self.options.align * remaining_width_in_lpxs;
        row.origin_in_lpxs.y = self.current_point_in_lpxs.y;
        self.current_row_start = self.current_row_end;
        if newline {
            self.current_row_start += 1;
            self.current_row_end += 1;
        }
        self.rows.push(row);
    }

    fn finish(self) -> LaidoutText {
        LaidoutText {
            source_index: Arc::new(SourceIndex::new(&self.text, &self.rows)),
            text: self.text,
            size_in_lpxs: Size::new(
                self.rows
                    .iter()
                    .map(|row| row.width_in_lpxs)
                    .reduce(f32::max)
                    .unwrap_or(0.0),
                self.current_point_in_lpxs.y - self.rows.last().map_or(0.0, |row| row.descender_in_lpxs),
            ),
            rows: self.rows,
        }
    }
}

#[derive(Debug)]
struct Fitter {
    text: Substr,
    font_family: Rc<FontFamily>,
    font_size_in_lpxs: f32,
    lens: VecDeque<usize>,
    widths_in_lpxs: VecDeque<f32>,
    hang_spaces: bool,
}

impl Fitter {
    fn new(
        text: Substr,
        font_family: Rc<FontFamily>,
        font_size_in_lpxs: f32,
        segment_kind: SegmentKind,
    ) -> Self {
        let lens: VecDeque<_> = match segment_kind {
            SegmentKind::Word => unicode_linebreak::linebreaks(&text)
                .scan(0, |start, (end, _)| {
                    let len = end - *start;
                    *start = end;
                    Some(len)
                })
                .collect(),
            SegmentKind::Grapheme => text.graphemes(true).map(|segment| segment.len()).collect(),
        };
        // Keep the upstream fast rejection for grapheme fallback. Word fitting
        // measures trimmed candidates, so shaping every untrimmed word is wasted.
        let widths_in_lpxs: VecDeque<_> = if matches!(segment_kind, SegmentKind::Grapheme) { lens
            .iter()
            .copied()
            .scan(0, |state, len| {
                let start = *state;
                let end = start + len;
                let segment = font_family.get_or_shape(text.substr(start..end));
                let width_in_lpxs = segment.width_in_ems * font_size_in_lpxs;
                *state = end;
                Some(width_in_lpxs)
            })
            .collect() } else { VecDeque::new() };
        Self {
            text,
            font_family,
            font_size_in_lpxs,
            lens,
            widths_in_lpxs,
            hang_spaces: matches!(segment_kind, SegmentKind::Word),
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// `x_in_lpxs` is where the fitted text starts in its row: TAB stops depend on it.
    fn fit(&mut self, x_in_lpxs: f32, wrap_width_in_lpxs: f32) -> Option<Rc<ShapedText>> {
        // Bracket the next line locally, rather than shaping half the remaining
        // paragraph on every row. As in the original binary search, fitting is
        // monotonic in the ordered Unicode break opportunities.
        let mut min_count = 1;
        let mut max_count = 1;
        let mut best_count = None;
        while max_count <= self.lens.len() && self.can_fit(max_count, x_in_lpxs, wrap_width_in_lpxs) {
            best_count = Some(max_count);
            min_count = max_count + 1;
            max_count = max_count.saturating_mul(2);
        }
        max_count = max_count.min(self.lens.len() + 1);
        while min_count < max_count {
            let mid_count = min_count + (max_count - min_count) / 2;
            if self.can_fit(mid_count, x_in_lpxs, wrap_width_in_lpxs) {
                best_count = Some(mid_count);
                min_count = mid_count + 1;
            } else {
                max_count = mid_count;
            }
        }
        if let Some(best_count) = best_count {
            let best_len = self.lens.iter().take(best_count).sum();
            let best_text = self.font_family.get_or_shape(self.text.substr(0..best_len));
            self.lens.drain(..best_count);
            if !self.hang_spaces {
                self.widths_in_lpxs.drain(..best_count);
            }
            self.text = self.text.substr(best_len..);
            Some(best_text)
        } else {
            None
        }
    }

    fn can_fit(&self, count: usize, x_in_lpxs: f32, wrap_width_in_lpxs: f32) -> bool {
        let len = self.lens.iter().take(count).sum();
        let candidate = self.text.substr(0..len);
        // Fit the ink before trailing breakable whitespace, but return the complete
        // shaped substring from `fit`: selection/copy offsets must keep those bytes.
        // Nonbreaking spaces are part of the word, not hanging whitespace.
        let measured_len = if self.hang_spaces {
            candidate.trim_end_matches(|c: char| {
                c.is_whitespace() && c != '\u{00a0}' && c != '\u{202f}'
            }).len()
        } else {
            candidate.len()
        };
        if measured_len == 0 {
            return true;
        }
        if !self.hang_spaces {
            let estimated_width_in_lpxs: f32 = self.widths_in_lpxs.iter().take(count).sum();
            if 0.5 * estimated_width_in_lpxs > wrap_width_in_lpxs {
                return false;
            }
        }
        let text = self.font_family.get_or_shape(candidate.substr(..measured_len));
        let actual_width_in_lpxs = text.width_in_lpxs_at(x_in_lpxs, self.font_size_in_lpxs);
        if actual_width_in_lpxs > wrap_width_in_lpxs {
            return false;
        }
        true
    }

    fn pop(&mut self) -> usize {
        let len = self.lens.pop_front().unwrap();
        if !self.hang_spaces {
            self.widths_in_lpxs.pop_front();
        }
        self.text = self.text.substr(len..);
        len
    }
}

#[derive(Clone, Copy, Debug)]
enum SegmentKind {
    Word,
    Grapheme,
}

pub trait LayoutParams {
    fn to_owned(self) -> OwnedLayoutParams;
    fn text(&self) -> &str;
    fn spans(&self) -> &[Span];
    fn options(&self) -> LayoutOptions;
}

impl Eq for dyn LayoutParams + '_ {}

impl Hash for dyn LayoutParams + '_ {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.text().hash(hasher);
        self.spans().hash(hasher);
        self.options().hash(hasher);
    }
}

impl PartialEq for dyn LayoutParams + '_ {
    fn eq(&self, other: &Self) -> bool {
        if self.text() != other.text() {
            return false;
        }
        if self.spans() != other.spans() {
            return false;
        }
        if self.options() != other.options() {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct OwnedLayoutParams {
    pub text: Substr,
    pub spans: Rc<[Span]>,
    pub options: LayoutOptions,
}

impl<'a> Borrow<dyn LayoutParams + 'a> for OwnedLayoutParams {
    fn borrow(&self) -> &(dyn LayoutParams + 'a) {
        self
    }
}

impl LayoutParams for OwnedLayoutParams {
    fn to_owned(self) -> Self {
        self
    }

    fn text(&self) -> &str {
        &self.text
    }

    fn spans(&self) -> &[Span] {
        &self.spans
    }

    fn options(&self) -> LayoutOptions {
        self.options
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BorrowedLayoutParams<'a> {
    pub text: &'a str,
    pub spans: &'a [Span],
    pub options: LayoutOptions,
}

impl<'a> Borrow<dyn LayoutParams + 'a> for BorrowedLayoutParams<'a> {
    fn borrow(&self) -> &(dyn LayoutParams + 'a) {
        self
    }
}

impl<'a> LayoutParams for BorrowedLayoutParams<'a> {
    fn to_owned(self) -> OwnedLayoutParams {
        OwnedLayoutParams {
            text: self.text.into(),
            spans: self.spans.into(),
            options: self.options,
        }
    }

    fn text(&self) -> &str {
        self.text
    }

    fn spans(&self) -> &[Span] {
        self.spans
    }

    fn options(&self) -> LayoutOptions {
        self.options
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Span {
    pub style: Style,
    pub len: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub font_family_id: FontFamilyId,
    pub font_size_in_pts: f32,
    pub color: Option<Color>,
}

impl Style {
    fn font_size_in_lpxs(&self) -> f32 {
        self.font_size_in_pts * LPXS_PER_INCH / PTS_PER_INCH
    }
}

impl Eq for Style {}

impl Hash for Style {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.font_family_id.hash(hasher);
        self.font_size_in_pts.to_bits().hash(hasher);
        self.color.hash(hasher);
    }
}

impl PartialEq for Style {
    fn eq(&self, other: &Self) -> bool {
        if self.font_family_id != other.font_family_id {
            return false;
        }
        if self.font_size_in_lpxs().to_bits() != other.font_size_in_lpxs().to_bits() {
            return false;
        }
        if self.color != other.color {
            return false;
        }
        true
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LayoutOptions {
    pub first_row_indent_in_lpxs: f32,
    pub first_row_min_line_spacing_below_in_lpxs: f32,
    pub max_width_in_lpxs: Option<f32>,
    pub wrap: bool,
    pub align: f32,
    pub line_spacing_scale: f32,
}

impl Default for LayoutOptions {
    fn default() -> Self {
        Self {
            first_row_indent_in_lpxs: 0.0,
            first_row_min_line_spacing_below_in_lpxs: 0.0,
            max_width_in_lpxs: None,
            wrap: false,
            align: 0.0,
            line_spacing_scale: 1.0,
        }
    }
}

impl Eq for LayoutOptions {}

impl Hash for LayoutOptions {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        self.first_row_indent_in_lpxs.to_bits().hash(hasher);
        self.first_row_min_line_spacing_below_in_lpxs
            .to_bits()
            .hash(hasher);
        self.max_width_in_lpxs.map(f32::to_bits).hash(hasher);
        self.align.to_bits().hash(hasher);
        self.wrap.hash(hasher);
        self.line_spacing_scale.to_bits().hash(hasher);
    }
}

impl PartialEq for LayoutOptions {
    fn eq(&self, other: &Self) -> bool {
        if self.first_row_indent_in_lpxs.to_bits() != other.first_row_indent_in_lpxs.to_bits() {
            return false;
        }
        if self.first_row_min_line_spacing_below_in_lpxs.to_bits()
            != other.first_row_min_line_spacing_below_in_lpxs.to_bits()
        {
            return false;
        }
        if self.max_width_in_lpxs.map(f32::to_bits) != other.max_width_in_lpxs.map(f32::to_bits) {
            return false;
        }
        if self.align.to_bits() != other.align.to_bits()
            || self.wrap != other.wrap
            || self.line_spacing_scale.to_bits() != other.line_spacing_scale.to_bits()
        {
            return false;
        }
        true
    }
}

#[derive(Clone, Debug)]
pub struct LaidoutText {
    pub text: Substr,
    pub size_in_lpxs: Size<f32>,
    pub rows: Vec<LaidoutRow>,
    pub(super) source_index: Arc<SourceIndex>,
}

impl Drop for LaidoutText {
    fn drop(&mut self) {
        super::async_layout::retire_rows(&mut self.rows);
    }
}

impl LaidoutText {
    /// Construct from authoritative rows, or no rows for source-only consumers.
    /// Builds the complete source index synchronously: giant UI documents must
    /// use AsyncLayout instead. Existing row indexes and disposal ownership stay
    /// attached to the rows; empty rows have zero layout size.
    pub fn from_rows(text: Substr, rows: Vec<LaidoutRow>) -> Self {
        let source_index = Arc::new(SourceIndex::new(&text, &rows));
        let size_in_lpxs = Size::new(
            rows.iter().map(|row| row.width_in_lpxs).reduce(f32::max).unwrap_or(0.0),
            rows.last().map_or(0.0, |row| row.origin_in_lpxs.y - row.descender_in_lpxs),
        );
        Self { text, size_in_lpxs, rows, source_index }
    }

    /// Hash of exactly the first `end` source bytes (at most 4095 bytes scanned).
    pub fn text_prefix_hash(&self, end: usize) -> u64 {
        self.source_index.prefix_hash(&self.text, end)
    }

    pub fn word_at(&self, byte: usize) -> (usize, usize) {
        self.source_index.word_at(byte, self.text.len())
    }

    pub fn visible_row_range(&self, top: f32, bottom: f32) -> std::ops::Range<usize> {
        self.source_index.visible_rows(top, bottom)
    }

    pub fn cursor_to_position(&self, cursor: Cursor) -> CursorPosition {
        let row_index = self.cursor_to_row_index(cursor);
        let row = &self.rows[row_index];
        let x_in_lpxs = row.index_to_x_in_lpxs(cursor.index - row.text.start_in_parent());
        CursorPosition {
            row_index,
            x_in_lpxs,
        }
    }

    fn cursor_to_row_index(&self, cursor: Cursor) -> usize {
        self.rows.partition_point(|row| {
            row.text.end_in_parent() < cursor.index
                || (row.text.end_in_parent() == cursor.index && !row.newline && cursor.prefer_next_row)
        }).min(self.rows.len().saturating_sub(1))
    }

    pub fn point_in_lpxs_to_cursor(&self, point_in_lpxs: Point<f32>) -> Cursor {
        let row_index = self.y_in_lpxs_to_row_index(point_in_lpxs.y);
        self.position_to_cursor(CursorPosition {
            row_index,
            x_in_lpxs: point_in_lpxs.x,
        })
    }

    fn y_in_lpxs_to_row_index(&self, y_in_lpxs: f32) -> usize {
        self.source_index.row_at_y(y_in_lpxs)
    }

    pub fn position_to_cursor(&self, position: CursorPosition) -> Cursor {
        let row = &self.rows[position.row_index];
        let index = row.x_in_lpxs_to_index(position.x_in_lpxs);
        Cursor {
            index: row.text.start_in_parent() + index,
            prefer_next_row: if index == 0 { true } else { false },
        }
    }

    pub fn selection_rects_in_lpxs(&self, selection: Selection) -> Vec<Rect<f32>> {
        self.selection_rects_in_lpxs_clipped(selection, f32::NEG_INFINITY, f32::INFINITY)
    }

    pub fn selection_rects_in_lpxs_clipped(&self, selection: Selection, top: f32, bottom: f32) -> Vec<Rect<f32>> {
        if self.rows.is_empty() { return Vec::new(); }
        let start_row = self.cursor_to_row_index(selection.start());
        let end_row = self.cursor_to_row_index(selection.end());
        let visible = self.visible_row_range(top, bottom);
        let start = visible.start.max(start_row);
        let end = visible.end.min(end_row + 1);
        let mut rects = Vec::with_capacity(end.saturating_sub(start));
        for row_index in start..end {
            let row = &self.rows[row_index];
            let left = if row_index == start_row {
                row.index_to_x_in_lpxs(selection.start().index - row.text.start_in_parent())
            } else if row_index == end_row { 0.0 } else { row.origin_in_lpxs.x };
            let right = if row_index == end_row {
                row.index_to_x_in_lpxs(selection.end().index - row.text.start_in_parent())
            } else if row_index == start_row { row.width_in_lpxs } else { row.origin_in_lpxs.x + row.width_in_lpxs };
            rects.push(Rect::new(
                Point::new(left, row.origin_in_lpxs.y - row.ascender_in_lpxs),
                Size::new(right - left, row.ascender_in_lpxs - row.descender_in_lpxs),
            ));
        }
        rects
    }
}

#[derive(Clone, Debug)]
pub struct LaidoutRow {
    pub origin_in_lpxs: Point<f32>,
    pub text: Substr,
    pub newline: bool,
    pub width_in_lpxs: f32,
    pub ascender_in_lpxs: f32,
    pub descender_in_lpxs: f32,
    pub line_gap_in_lpxs: f32,
    pub line_spacing_above_in_lpxs: f32,
    pub line_spacing_below_in_lpxs: f32,
    pub glyphs: Vec<LaidoutGlyph>,
    pub(super) glyph_index: Arc<GlyphIndex>,
}

impl LaidoutRow {
    /// Conservative glyph candidates, including marks, bitmap strikes and atlas
    /// fringe. Coordinates exclude the row origin, and include glyph offsets.
    pub fn visible_glyph_range(&self, left: f32, right: f32) -> std::ops::Range<usize> {
        self.glyph_index.visible(left, right)
    }

    pub fn line_spacing_in_lpxs(&self, next_row: &LaidoutRow) -> f32 {
        self.line_spacing_below_in_lpxs + next_row.line_spacing_above_in_lpxs
    }

    pub fn x_in_lpxs_to_index(&self, x_in_lpxs: f32) -> usize {
        self.glyph_index.x_to_index(x_in_lpxs, self.text.len())
    }

    pub fn index_to_x_in_lpxs(&self, index: usize) -> f32 {
        self.glyph_index.index_to_x(index, self.width_in_lpxs)
    }
}

#[derive(Clone, Debug)]
pub struct LaidoutGlyph {
    pub origin_in_lpxs: Point<f32>,
    pub font: Rc<Font>,
    pub font_size_in_lpxs: f32,
    pub color: Option<Color>,
    pub id: GlyphId,
    pub cluster: usize,
    pub advance_in_ems: f32,
    pub offset_in_ems: f32,
    pub(super) paint_bounds_in_ems: Rect<f32>,
}

impl LaidoutGlyph {
    pub fn paint_bounds_in_lpxs(&self) -> Rect<f32> {
        let bounds = self.paint_bounds_in_ems;
        Rect::new(
            Point::new(
                self.origin_in_lpxs.x + self.offset_in_lpxs() + bounds.origin.x * self.font_size_in_lpxs,
                self.origin_in_lpxs.y + bounds.origin.y * self.font_size_in_lpxs,
            ),
            bounds.size * self.font_size_in_lpxs,
        )
    }

    pub fn advance_in_lpxs(&self) -> f32 {
        self.advance_in_ems * self.font_size_in_lpxs
    }

    pub fn offset_in_lpxs(&self) -> f32 {
        self.offset_in_ems * self.font_size_in_lpxs
    }

    pub fn ascender_in_lpxs(&self) -> f32 {
        self.font.ascender_in_ems() * self.font_size_in_lpxs
    }

    pub fn descender_in_lpxs(&self) -> f32 {
        self.font.descender_in_ems() * self.font_size_in_lpxs
    }

    pub fn line_gap_in_lpxs(&self) -> f32 {
        self.font.line_gap_in_ems() * self.font_size_in_lpxs
    }

    pub fn rasterize(&self, dpx_per_em: f32) -> Option<RasterizedGlyph> {
        self.font.rasterize_glyph(self.id, dpx_per_em)
    }
}

#[cfg(test)]
mod paseo_linebreak_tests {
    use super::*;

    fn layouter() -> (Layouter, Style) {
        let mut layouter = Layouter::new(Settings::default());
        let font_id = FontId::from("paseo-wrap-test");
        let family_id = FontFamilyId::from("paseo-wrap-test");
        // Existing upstream fixture in the adjacent vendored widgets crate.
        let data = include_bytes!("../../../makepad-widgets/resources/LiberationMono-Regular.ttf");
        layouter.define_font(font_id, FontDefinition {
            data: std::sync::Arc::new(data.to_vec()),
            index: 0,
            variations: Vec::new(),
            ascender_fudge_in_ems: 0.0,
            descender_fudge_in_ems: 0.0,
        });
        layouter.define_font_family(family_id, FontFamilyDefinition { font_ids: vec![font_id] });
        (layouter, Style { font_family_id: family_id, font_size_in_pts: 12.0, color: None })
    }

    fn layout(layouter: &mut Layouter, style: Style, text: &str, width: Option<f32>) -> Rc<LaidoutText> {
        layouter.get_or_layout(BorrowedLayoutParams {
            text,
            spans: &[Span { style, len: text.len() }],
            options: LayoutOptions { wrap: width.is_some(), max_width_in_lpxs: width, ..LayoutOptions::default() },
        })
    }

    #[test]
    fn punctuation_moves_with_its_word() {
        let (mut layouter, style) = layouter();
        let width = layout(&mut layouter, style, "a word", None).size_in_lpxs.width + 0.01;
        let result = layout(&mut layouter, style, "a word.", Some(width));
        let rows: Vec<_> = result.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, ["a ", "word."]);
    }

    #[test]
    fn hanging_spaces_preserve_text_and_cursor_byte_ranges() {
        let (mut layouter, style) = layouter();
        let width = layout(&mut layouter, style, "word", None).size_in_lpxs.width + 0.01;
        let text = "word   café";
        let result = layout(&mut layouter, style, text, Some(width));
        assert_eq!(result.rows[0].text.as_str(), "word   ");
        assert_eq!(result.rows[1].text.as_str(), "café");
        assert_eq!(result.rows[1].text.start_in_parent(), 7);
        assert_eq!(result.rows[1].text.end_in_parent(), text.len());
        for index in [0, 4, 7, text.len()] {
            let cursor = Cursor { index, prefer_next_row: true };
            let position = result.cursor_to_position(cursor);
            assert_eq!(result.position_to_cursor(position).index, index);
        }
    }

    #[test]
    fn nonbreaking_space_stays_inside_the_word() {
        let (mut layouter, style) = layouter();
        let width = layout(&mut layouter, style, "a b", None).size_in_lpxs.width + 0.01;
        let result = layout(&mut layouter, style, "a b\u{a0}c", Some(width));
        let rows: Vec<_> = result.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, ["a ", "b\u{a0}c"]);
    }

    fn cell(layouter: &mut Layouter, style: Style) -> f32 {
        layout(layouter, style, "0", None).size_in_lpxs.width
    }

    fn near(actual: f32, expected: f32) -> bool {
        (actual - expected).abs() < 0.01
    }

    #[test]
    fn tabs_are_blank_glyphs_advancing_to_column_stops() {
        let (mut layouter, style) = layouter();
        let cell = cell(&mut layouter, style);
        let space = layout(&mut layouter, style, " ", None).rows[0].glyphs[0].id;
        let text = "\t\tab\tc\n1\t2";
        let result = layout(&mut layouter, style, text, None);
        let rows: Vec<_> = result.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, ["\t\tab\tc", "1\t2"]);
        assert_eq!(result.text.as_str(), text);
        for row in &result.rows {
            for glyph in &row.glyphs {
                assert_ne!(glyph.id, 0, "missing glyph at byte {} of {:?}", glyph.cluster, row.text.as_str());
                if row.text.as_bytes()[glyph.cluster] == b'\t' {
                    assert_eq!(glyph.id, space);
                }
            }
        }
        // Leading tabs indent by whole stops; a tab after text reaches the next stop.
        let columns = [0.0, 4.0, 8.0, 9.0, 10.0, 12.0, 13.0];
        for (index, column) in columns.into_iter().enumerate() {
            let x = result.rows[0].index_to_x_in_lpxs(index);
            assert!(near(x / cell, column), "byte {index} at {} cells", x / cell);
            let cursor = Cursor { index, prefer_next_row: true };
            assert_eq!(result.position_to_cursor(result.cursor_to_position(cursor)).index, index);
        }
        assert!(near(result.rows[1].index_to_x_in_lpxs(2) / cell, 4.0));
        let first_tab = result.selection_rects_in_lpxs(Selection {
            anchor: Cursor { index: 0, prefer_next_row: true },
            cursor: Cursor { index: 1, prefer_next_row: false },
        });
        assert!(near(first_tab[0].size.width / cell, 4.0));
    }

    #[test]
    fn other_controls_are_blank_and_zero_width() {
        let (mut layouter, style) = layouter();
        let cell = cell(&mut layouter, style);
        let text = "a\r\nb\u{1b}[0m\u{c}\u{7f}\u{85}";
        let result = layout(&mut layouter, style, text, None);
        let rows: Vec<_> = result.rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(rows, ["a\r", "b\u{1b}[0m\u{c}\u{7f}\u{85}"]);
        assert!(result.rows.iter().all(|row| row.glyphs.iter().all(|glyph| glyph.id != 0)));
        assert!(near(result.rows[0].width_in_lpxs / cell, 1.0));
        assert!(near(result.rows[1].width_in_lpxs / cell, 4.0));
    }

    #[test]
    fn wrapping_measures_tabs_at_their_stops_and_hangs_them() {
        let (mut layouter, style) = layouter();
        let cell = cell(&mut layouter, style);
        let text = "ab\tcd";
        assert!(near(layout(&mut layouter, style, text, None).size_in_lpxs.width / cell, 6.0));
        let mut rows = |width: f32| -> Vec<String> {
            layout(&mut layouter, style, text, Some(width)).rows.iter().map(|row| row.text.as_str().to_owned()).collect()
        };
        assert_eq!(rows(6.0 * cell + 0.01), ["ab\tcd"]);
        // Counting the TAB as one space would claim five cells and overflow this row.
        assert_eq!(rows(5.0 * cell + 0.01), ["ab\t", "cd"]);
    }

    #[test]
    fn a_first_row_indent_continues_the_tab_stops_of_its_line() {
        let (mut layouter, style) = layouter();
        let cell = cell(&mut layouter, style);
        // A run painted two cells into a line: its TAB ends at the line's first stop.
        let text = "\tc";
        let result = layouter.get_or_layout(BorrowedLayoutParams {
            text,
            spans: &[Span { style, len: text.len() }],
            options: LayoutOptions { first_row_indent_in_lpxs: 2.0 * cell, ..LayoutOptions::default() },
        });
        assert!(near(result.rows[0].index_to_x_in_lpxs(1) / cell, 4.0));
        assert!(near(result.size_in_lpxs.width / cell, 5.0));
    }
}
