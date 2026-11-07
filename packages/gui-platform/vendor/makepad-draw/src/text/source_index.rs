use {super::{layouter::{LaidoutGlyph, LaidoutRow}, substr::Substr}, std::ops::Range, unicode_segmentation::UnicodeSegmentation};

const HASH_STRIDE: usize = 4096;

/// Immutable indexes constructed alongside authoritative layout, never per draw.
#[derive(Debug)]
pub(super) struct SourceIndex {
    hashes: Vec<u64>,
    words: Vec<Range<usize>>,
    row_bottoms: Vec<f32>,
    row_tops: Vec<f32>,
    cursor_boundaries: Vec<f32>,
}

impl SourceIndex {
    pub fn new(text: &Substr, rows: &[LaidoutRow]) -> Self {
        let mut hashes = Vec::with_capacity(text.len() / HASH_STRIDE + 1);
        let mut hash = 0u64;
        hashes.push(hash);
        for (index, byte) in text.bytes().enumerate() {
            hash = hash.wrapping_mul(257).wrapping_add(byte as u64 + 1);
            if (index + 1) % HASH_STRIDE == 0 { hashes.push(hash); }
        }
        let words = text.split_word_bound_indices().map(|(start, word)| start..start + word.len()).collect();
        let mut row_bottoms = Vec::with_capacity(rows.len());
        let mut row_tops = Vec::with_capacity(rows.len());
        let mut cursor_boundaries = Vec::with_capacity(rows.len());
        let mut bottom = f32::NEG_INFINITY;
        let mut cursor_boundary = f32::NEG_INFINITY;
        for (index, row) in rows.iter().enumerate() {
            let top = (row.origin_in_lpxs.y - row.ascender_in_lpxs)
                .min(row.origin_in_lpxs.y + row.glyph_index.top);
            let row_bottom = (row.origin_in_lpxs.y - row.descender_in_lpxs)
                .max(row.origin_in_lpxs.y + row.glyph_index.bottom);
            bottom = bottom.max(row_bottom);
            row_bottoms.push(bottom);
            row_tops.push(top);
            let spacing = rows.get(index + 1).map_or(0.0, |next| row.line_spacing_in_lpxs(next));
            // Prefix maxima preserve the original first-matching-row search.
            cursor_boundary = cursor_boundary.max(row.origin_in_lpxs.y + spacing * 0.5);
            cursor_boundaries.push(cursor_boundary);
        }
        let mut top = f32::INFINITY;
        for row_top in row_tops.iter_mut().rev() { top = top.min(*row_top); *row_top = top; }
        Self { hashes, words, row_bottoms, row_tops, cursor_boundaries }
    }

    pub fn prefix_hash(&self, text: &str, end: usize) -> u64 {
        let end = end.min(text.len());
        let sample = end / HASH_STRIDE;
        text.as_bytes()[sample * HASH_STRIDE..end].iter().fold(self.hashes[sample], |hash, byte| {
            hash.wrapping_mul(257).wrapping_add(*byte as u64 + 1)
        })
    }

    pub fn word_at(&self, byte: usize, len: usize) -> (usize, usize) {
        if self.words.is_empty() { return (0, 0); }
        let index = self.words.partition_point(|word| word.end <= byte.min(len.saturating_sub(1)));
        let word = &self.words[index.min(self.words.len() - 1)];
        (word.start, word.end)
    }

    pub fn visible_rows(&self, top: f32, bottom: f32) -> Range<usize> {
        let start = self.row_bottoms.partition_point(|end| *end < top);
        let end = self.row_tops.partition_point(|start| *start <= bottom);
        start.min(end)..end
    }

    pub fn row_at_y(&self, y: f32) -> usize {
        if y < 0.0 { return 0; }
        self.cursor_boundaries.partition_point(|boundary| *boundary <= y).min(self.cursor_boundaries.len().saturating_sub(1))
    }
}

#[derive(Debug)]
pub(super) struct GlyphIndex {
    rights: Vec<f32>,
    lefts: Vec<f32>,
    cursor_xs: Vec<(usize, f32)>,
    hit_thresholds: Vec<f32>,
    hit_indices: Vec<usize>,
    top: f32,
    bottom: f32,
}

impl GlyphIndex {
    pub fn new(text: &str, glyphs: &[LaidoutGlyph], width: f32) -> Self {
        let mut rights = Vec::with_capacity(glyphs.len());
        let mut lefts = Vec::with_capacity(glyphs.len());
        let mut right = f32::NEG_INFINITY;
        let mut top = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for glyph in glyphs {
            let bounds = glyph.paint_bounds_in_lpxs();
            if bounds.size.height.is_finite() {
                top = top.min(bounds.origin.y);
                bottom = bottom.max(bounds.origin.y + bounds.size.height);
            } else {
                top = f32::NEG_INFINITY;
                bottom = f32::INFINITY;
            }
            if !bounds.size.width.is_finite() {
                right = f32::INFINITY;
                rights.push(right);
                lefts.push(f32::NEG_INFINITY);
                continue;
            }
            right = right.max(bounds.origin.x + bounds.size.width);
            rights.push(right);
            lefts.push(bounds.origin.x);
        }
        let mut left = f32::INFINITY;
        for value in lefts.iter_mut().rev() { left = left.min(*value); *value = left; }
        let mut cursor_xs = Vec::new();
        let mut hit_thresholds = Vec::new();
        let mut hit_indices = Vec::new();
        let mut group = 0;
        let mut threshold = f32::NEG_INFINITY;
        while group < glyphs.len() {
            let start = glyphs[group].cluster;
            let mut next = group + 1;
            while next < glyphs.len() && glyphs[next].cluster == start { next += 1; }
            let end = glyphs.get(next).map_or(text.len(), |glyph| glyph.cluster);
            let mut x = glyphs[group].origin_in_lpxs.x;
            let end_x = glyphs.get(next).map_or(width, |glyph| glyph.origin_in_lpxs.x);
            let count = text[start..end].graphemes(true).count();
            let advance = (end_x - x) / count as f32;
            for (offset, _) in text[start..end].grapheme_indices(true) {
                cursor_xs.push((start + offset, x));
                threshold = threshold.max(x + 0.5 * advance);
                hit_thresholds.push(threshold);
                hit_indices.push(start + offset);
                x += advance;
            }
            group = next;
        }
        Self { rights, lefts, cursor_xs, hit_thresholds, hit_indices, top, bottom }
    }

    pub fn visible(&self, left: f32, right: f32) -> Range<usize> {
        let start = self.rights.partition_point(|end| *end < left);
        let end = self.lefts.partition_point(|start| *start <= right);
        start.min(end)..end
    }

    pub fn index_to_x(&self, index: usize, width: f32) -> f32 {
        self.cursor_xs.binary_search_by_key(&index, |entry| entry.0).ok()
            .map_or(width, |at| self.cursor_xs[at].1)
    }

    pub fn x_to_index(&self, x: f32, len: usize) -> usize {
        let index = self.hit_thresholds.partition_point(|threshold| *threshold <= x);
        self.hit_indices.get(index).copied().unwrap_or(len)
    }
}
