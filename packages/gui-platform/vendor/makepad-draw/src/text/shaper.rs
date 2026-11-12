use {
    super::{
        font::{Font, GlyphId},
        slice::SliceExt,
        substr::Substr,
    },
    makepad_rustybuzz as rustybuzz,
    rustybuzz::UnicodeBuffer,
    std::{
        collections::{HashMap, VecDeque},
        hash::Hash,
        mem,
        rc::Rc,
    },
    unicode_segmentation::UnicodeSegmentation,
};

#[derive(Debug)]
pub struct Shaper {
    reusable_glyphs: Vec<Vec<ShapedGlyph>>,
    reusable_unicode_buffer: UnicodeBuffer,
    cache_size: usize,
    cached_params: VecDeque<ShapeParams>,
    cached_results: HashMap<ShapeParams, Rc<ShapedText>>,
}

impl Shaper {
    pub fn new(settings: Settings) -> Self {
        Self {
            reusable_glyphs: Vec::new(),
            reusable_unicode_buffer: UnicodeBuffer::new(),
            cache_size: settings.cache_size,
            cached_params: VecDeque::with_capacity(settings.cache_size),
            cached_results: HashMap::with_capacity(settings.cache_size),
        }
    }

    pub fn get_or_shape(&mut self, params: ShapeParams) -> Rc<ShapedText> {
        if let Some(result) = self.cached_results.get(&params) {
            return result.clone();
        }
        if self.cached_params.len() == self.cache_size {
            let params = self.cached_params.pop_front().unwrap();
            self.cached_results.remove(&params);
        }
        let result = Rc::new(self.shape(params.clone()));
        self.cached_params.push_back(params.clone());
        self.cached_results.insert(params, result.clone());
        result
    }

    fn shape(&mut self, params: ShapeParams) -> ShapedText {
        let mut glyphs = Vec::new();
        if params.fonts.is_empty() {
            println!("WARNING: encountered empty font family");
        } else {
            self.shape_recursive(
                &params.text,
                &params.fonts,
                0,
                params.text.len(),
                &mut glyphs,
            );
        }
        ShapedText {
            text: params.text,
            width_in_ems: glyphs.iter().map(|glyph| glyph.advance_in_ems).sum(),
            has_tabs: glyphs.iter().any(|glyph| glyph.is_tab),
            glyphs,
        }
    }

    fn shape_recursive(
        &mut self,
        text: &str,
        fonts: &[Rc<Font>],
        start: usize,
        end: usize,
        out_glyphs: &mut Vec<ShapedGlyph>,
    ) {
        let (font, fonts) = fonts.split_first().unwrap();
        let mut glyphs = self.reusable_glyphs.pop().unwrap_or(Vec::new());
        self.shape_step(text, font, start, end, &mut glyphs);
        let mut glyph_groups = glyphs
            .group_by(|glyph_0, glyph_1| glyph_0.cluster == glyph_1.cluster)
            .peekable();
        while let Some(glyph_group) = glyph_groups.next() {
            if glyph_group.iter().any(|glyph| glyph.id == 0) && !fonts.is_empty() {
                let missing_start = glyph_group[0].cluster;
                while glyph_groups.peek().map_or(false, |glyph_group| {
                    glyph_group.iter().any(|glyph| glyph.id == 0)
                }) {
                    glyph_groups.next();
                }
                let missing_end = glyph_groups
                    .peek()
                    .map_or(end, |next_glyph_group| next_glyph_group[0].cluster);
                self.shape_recursive(text, fonts, missing_start, missing_end, out_glyphs);
            } else {
                out_glyphs.extend(glyph_group.iter().cloned());
            }
        }
        drop(glyph_groups);
        glyphs.clear();
        self.reusable_glyphs.push(glyphs);
    }

    fn shape_step(
        &mut self,
        text: &str,
        font: &Rc<Font>,
        start: usize,
        end: usize,
        out_glyphs: &mut Vec<ShapedGlyph>,
    ) {
        let mut unicode_buffer = mem::take(&mut self.reusable_unicode_buffer);
        let mut has_controls = false;
        for (index, grapheme) in text[start..end].grapheme_indices(true) {
            let cluster = start + index;
            for char in grapheme.chars() {
                // No font draws a control character: shaped as itself it becomes `.notdef`
                // in every font of the family. It takes the blank space glyph instead and
                // keeps its own cluster, so byte-indexed selection, copy and find are unchanged.
                has_controls |= char.is_control();
                unicode_buffer.add(if char.is_control() { ' ' } else { char }, cluster as u32);
            }
        }
        let mut glyph_buffer = rustybuzz::shape(font.rustybuzz_face(), &[], unicode_buffer);
        let out_glyph_start = out_glyphs.len();
        let shaped_glyph = |glyph_info: &rustybuzz::GlyphInfo, glyph_position: &rustybuzz::GlyphPosition| {
            let cluster = glyph_info.cluster as usize;
            let control = if has_controls {
                text[cluster..].chars().next().filter(|char| char.is_control())
            } else {
                None
            };
            // A TAB keeps the space advance as its tab-stop unit (see `advance_in_ems_at`);
            // every other control (CR of CRLF, form feed, ESC, ...) is zero-width.
            let scale = if matches!(control, Some(char) if char != '\t') { 0.0 } else { 1.0 };
            ShapedGlyph {
                font: font.clone(),
                id: glyph_info.glyph_id as u16,
                cluster,
                advance_in_ems: scale * glyph_position.x_advance as f32 / font.units_per_em(),
                offset_in_ems: scale * glyph_position.x_offset as f32 / font.units_per_em(),
                is_tab: control == Some('\t'),
            }
        };
        out_glyphs.extend(
            glyph_buffer
                .glyph_infos()
                .iter()
                .zip(glyph_buffer.glyph_positions())
                .map(|(glyph_info, glyph_position)| shaped_glyph(glyph_info, glyph_position)),
        );

        // HACK: We don't support right-to-left scripts yet. When such scripts are used, cluster
        // values may be monotonically decreasing instead of increasing, as we assume. This should
        // be fixed by properly handling bidirectional text (by further breaking up spans into
        // spans with a single direction), but for now we just check if the cluster values are
        // monotonically increasing. If they are not, we re-shape the text by replacing each
        // grapheme with a single character. This is not ideal, but it works for now.
        if out_glyphs[out_glyph_start..]
            .windows(2)
            .any(|glyphs| {
                glyphs[0].cluster > glyphs[1].cluster
            })
        {
            out_glyphs.truncate(out_glyph_start);
            let mut unicode_buffer = glyph_buffer.clear();
            for (index, grapheme) in text[start..end].grapheme_indices(true) {
                let cluster = start + index;
                let control = grapheme.starts_with(|char: char| char.is_control());
                unicode_buffer.add(if control { ' ' } else { '□' }, cluster as u32);
            }
            glyph_buffer = rustybuzz::shape(font.rustybuzz_face(), &[], unicode_buffer);
            out_glyphs.extend(
                glyph_buffer
                    .glyph_infos()
                    .iter()
                    .zip(glyph_buffer.glyph_positions())
                    .map(|(glyph_info, glyph_position)| shaped_glyph(glyph_info, glyph_position)),
            );
        }

        self.reusable_unicode_buffer = glyph_buffer.clear();
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub cache_size: usize,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ShapeParams {
    pub text: Substr,
    pub fonts: Rc<[Rc<Font>]>,
}

#[derive(Clone, Debug)]
pub struct ShapedText {
    pub text: Substr,
    /// Sum of the shaped advances: every TAB counts as one space. Use
    /// [`ShapedText::width_in_lpxs_at`] for the width in a row.
    pub width_in_ems: f32,
    pub has_tabs: bool,
    pub glyphs: Vec<ShapedGlyph>,
}

impl ShapedText {
    /// Width of the text when it starts `x_in_lpxs` into its row.
    pub fn width_in_lpxs_at(&self, x_in_lpxs: f32, font_size_in_lpxs: f32) -> f32 {
        if !self.has_tabs {
            return self.width_in_ems * font_size_in_lpxs;
        }
        self.glyphs.iter().fold(x_in_lpxs, |x, glyph| {
            x + glyph.advance_in_ems_at(x, font_size_in_lpxs) * font_size_in_lpxs
        }) - x_in_lpxs
    }
}

#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    pub font: Rc<Font>,
    pub id: GlyphId,
    pub cluster: usize,
    pub advance_in_ems: f32,
    pub offset_in_ems: f32,
    /// A TAB: the blank space glyph, whose advance in a row is the distance to the next stop.
    pub is_tab: bool,
}

impl ShapedGlyph {
    /// Advance of the glyph when its origin is `x_in_lpxs` into its row. A TAB reaches the
    /// next multiple of [`TAB_STOP_IN_SPACES`] space advances, so text after it lines up in
    /// columns whatever precedes it; any other glyph keeps its shaped advance.
    pub fn advance_in_ems_at(&self, x_in_lpxs: f32, font_size_in_lpxs: f32) -> f32 {
        let stop_in_lpxs = TAB_STOP_IN_SPACES * self.advance_in_ems * font_size_in_lpxs;
        if !self.is_tab || stop_in_lpxs <= 0.0 {
            return self.advance_in_ems;
        }
        // Summed f32 advances can end a hair short of a stop; that TAB belongs to the next one.
        let stop = (x_in_lpxs / stop_in_lpxs + 1.0e-3).floor() + 1.0;
        (stop * stop_in_lpxs - x_in_lpxs) / font_size_in_lpxs
    }
}

/// Tab stops repeat every this many space advances.
pub const TAB_STOP_IN_SPACES: f32 = 4.0;
