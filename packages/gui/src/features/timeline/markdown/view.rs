//! Native Markdown painter (chat_markdown.dart PaseoMarkdown + _BlockShaper +
//! MarkdownCodeBlock, components/markdown/styles.ts): paragraphs at 1.4 line
//! height, bold headings scaled 1.6/1.4/1.2/1.1/1/1 at 1.3, muted list
//! markers with 20px nesting, surface1 quotes with a 3px surface2 rule,
//! GitHub alerts, accentBright links (underlined while hovered), surface2
//! inline-code chips, fenced code with syntax colors and a copy glyph,
//! bordered tables with a surface2 header, rules and images. Every text run is
//! selectable and copied with its exact source spelling.

use super::super::canvas::*;
use super::super::highlight;
use super::super::images::{self, ImageState};
use super::super::links::FileLinkSourceType;
use super::super::paint::{Draw, Look};
use super::model::{AlertKind, Block, Inline, ListItem, Style};
use makepad_widgets::*;
use paseo_gui_core::CoreHandle;
use pulldown_cmark::Alignment;
use regex::Regex;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use makepad_widgets::makepad_draw::text::AsyncLayout;
use std::time::{Duration, Instant};
use makepad_widgets::makepad_draw::text::layouter::{LaidoutText, Span as FontSpan};
use super::worker::{INTERACTIVE_SOURCE_BYTES, ParagraphKey, ParsedSnapshot, SemanticParagraph};
use makepad_widgets::makepad_draw::text::selection::Cursor;
use makepad_widgets::makepad_draw::text::substr::Substr;

/// Find highlight: the `occurrence`-th match across the message's text blocks.
pub struct FindMark {
    pub pattern: Regex,
    pub occurrence: usize,
    pub seen: usize,
}

pub struct MdOpts<'a> {
    /// Paragraph font size (content size, or ui 14 for compact surfaces).
    pub font_size: f64,
    /// Prefix for copy/scroll/image keys (unique per rendered message).
    pub key: &'a str,
    pub find: Option<&'a mut FindMark>,
    pub core: Option<&'a CoreHandle>,
    pub host_id: &'a str,
    pub workspace_root: Option<&'a str>,
    pub syntax_theme: &'a str,
    /// Sequential block counter: stable keys for code/table scroll and copy state.
    pub counter: usize,
}

impl MdOpts<'_> {
    fn next_key(&mut self, kind: &str) -> String {
        self.counter += 1;
        format!("{}:{kind}:{}", self.key, self.counter)
    }
}

pub fn block_gap(compact: bool) -> f64 {
    if compact { 12.0 } else { 16.0 }
}

/// A paragraph under construction: text, styled spans and decorated ranges.
#[derive(Clone, Default)]
struct Rich {
    text: String,
    spans: Vec<RichSpan>,
    codes: Vec<(usize, usize)>,
    strikes: Vec<(usize, usize)>,
    links: Vec<(usize, usize, String, FileLinkSourceType)>,
}

struct PreparedRich {
    rich: Arc<SemanticParagraph>,
    laidout: Rc<LaidoutText>,
    found: Option<(usize, usize)>,
}

#[derive(Clone, Copy, PartialEq)]
struct ParagraphStyle {
    look: Look,
    size: f64,
    color: Vec4,
    bold: bool,
    width: f64,
    height: f64,
}

struct PendingParagraph {
    rich: Arc<SemanticParagraph>,
    spans: Vec<FontSpan>,
    ready_spans: Option<Arc<Vec<FontSpan>>>,
    cursor: usize,
    byte: usize,
    remaining: usize,
    colors: Option<highlight::Colored>,
    color_cursor: usize,
    layout: AsyncLayout,
}

struct CachedParagraph {
    owner: Weak<Vec<Block>>,
    len: usize,
    style: ParagraphStyle,
    pattern: Option<String>,
    found: Option<(usize, usize)>,
    prepared: Option<Rc<PreparedRich>>,
    pending: Option<PendingParagraph>,
}

/// Soft allowance of one document preparation pass, checked between paragraphs.
const PREPARATION_PASS: Duration = Duration::from_millis(4);

#[derive(Default)]
pub struct ParagraphCache {
    owner: Option<Rc<Vec<Block>>>,
    snapshot: Option<Rc<ParsedSnapshot>>,
    entries: HashMap<(usize, u64, u64, bool), CachedParagraph>,
    pending: bool,
    advance_available: bool,
    advance_deadline: Option<Instant>,
    frozen: bool,
    inline_remaining: usize,
    inline_deadline: Option<Instant>,
}

impl ParagraphCache {
    fn can_layout_inline(&self, bytes: usize) -> bool {
        bytes <= self.inline_remaining
            && self.inline_deadline.is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Admits one paragraph's background advance (span mapping, layout
    /// admission, hydration). A pass admits its first paragraph even when
    /// late, then every further one until its deadline: publication time
    /// follows the work, not two frames for each paragraph of the document.
    fn take_advance(&mut self, now: Instant) -> bool {
        let admitted = self.advance_available;
        if admitted {
            self.advance_available = self.advance_deadline.is_some_and(|deadline| now < deadline);
        }
        admitted
    }

    pub(super) fn set_owner(&mut self, owner: Option<Rc<Vec<Block>>>) -> Option<Rc<Vec<Block>>> {
        self.entries.retain(|_, entry| entry.owner.strong_count() > 0);
        std::mem::replace(&mut self.owner, owner)
    }
}

struct DocumentLayout {
    snapshot: Rc<ParsedSnapshot>,
    look: Look,
    width: f64,
    font_size: f64,
    occurrence: Option<usize>,
    cache: ParagraphCache,
}

#[derive(Default)]
pub struct MarkdownPresentation {
    epoch: u64,
    published: Option<DocumentLayout>,
    preparing: Option<DocumentLayout>,
}

impl DocumentLayout {
    fn matches(&self, snapshot: &Rc<ParsedSnapshot>, look: Look, width: f64, opts: &MdOpts) -> bool {
        Rc::ptr_eq(&self.snapshot, snapshot) && self.look == look && self.width == width
            && self.font_size == opts.font_size && self.occurrence == opts.find.as_ref().map(|find| find.occurrence)
    }
}

/// Publish coherent source/layout revisions. Small live revisions can finish
/// in this frame; larger or over-budget work retains the previous snapshot.
#[allow(clippy::too_many_arguments)]
pub fn draw_worker(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, x: f64, y: f64, w: f64) -> f64 {
    let mut presentation = std::mem::take(&mut d.state.markdown_presentation);
    let epoch = d.state.markdown_worker.epoch();
    if presentation.epoch != epoch {
        presentation = MarkdownPresentation { epoch, ..Default::default() };
    }
    if !d.state.managed_preparation || d.state.preparing {
        let candidate = d.state.markdown_worker.candidate()
            .or_else(|| presentation.published.as_ref().map(|doc| doc.snapshot.clone()));
        if let Some(candidate) = candidate {
            let current = presentation.published.as_ref().is_some_and(|doc| doc.matches(&candidate, d.look, w, opts));
            let pending = presentation.preparing.as_ref().is_some_and(|doc| doc.matches(&candidate, d.look, w, opts));
            if !current && !pending {
                presentation.preparing = Some(DocumentLayout {
                    snapshot: candidate.clone(), look: d.look, width: w, font_size: opts.font_size,
                    occurrence: opts.find.as_ref().map(|find| find.occurrence),
                    cache: ParagraphCache { owner: Some(candidate.blocks.clone()), snapshot: Some(candidate), ..Default::default() },
                });
            }
        }
        if let Some(mut preparing) = presentation.preparing.take() {
            let deadline = Instant::now() + PREPARATION_PASS;
            preparing.cache.pending = false;
            preparing.cache.advance_available = true;
            preparing.cache.advance_deadline = Some(deadline);
            preparing.cache.frozen = false;
            preparing.cache.inline_remaining = if preparing.snapshot.interactive { INTERACTIVE_SOURCE_BYTES } else { 0 };
            preparing.cache.inline_deadline = preparing.snapshot.interactive.then_some(deadline);
            measured(cx, d, opts, |cx, d, opts| draw_document(cx, d, opts, &mut preparing, x, y));
            if !preparing.cache.pending {
                d.state.markdown_worker.publish(preparing.snapshot.revision);
                presentation.published = Some(preparing);
            } else {
                presentation.preparing = Some(preparing);
            }
        }
    }
    d.animating |= presentation.preparing.is_some() || d.state.markdown_worker.is_pending();
    let height = if let Some(published) = presentation.published.as_mut() {
        published.cache.frozen = true;
        published.cache.advance_available = false;
        draw_document(cx, d, opts, published, x, y)
    } else {
        0.0
    };
    d.state.markdown_presentation = presentation;
    height
}

fn draw_document(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, doc: &mut DocumentLayout, x: f64, y: f64) -> f64 {
    let look = std::mem::replace(&mut d.look, doc.look);
    let font_size = std::mem::replace(&mut opts.font_size, doc.font_size);
    let previous_find_y = d.state.find_y;
    let current_find = doc.occurrence == opts.find.as_ref().map(|find| find.occurrence)
        && doc.snapshot.find_pattern.as_deref() == opts.find.as_ref().map(|find| find.pattern.as_str());
    std::mem::swap(&mut d.state.markdown_paragraphs, &mut doc.cache);
    let height = draw_blocks(cx, d, opts, &doc.snapshot.blocks, x, y, doc.width, doc.look.palette.foreground, false);
    if !current_find { d.state.find_y = previous_find_y; }
    std::mem::swap(&mut d.state.markdown_paragraphs, &mut doc.cache);
    opts.font_size = font_size;
    d.look = look;
    height
}

fn find_in_matches(opts: &mut MdOpts, matches: &[(usize, usize)]) -> Option<(usize, usize)> {
    let find = opts.find.as_deref_mut()?;
    let found = find.occurrence.checked_sub(find.seen).and_then(|index| matches.get(index)).copied();
    find.seen += matches.len();
    found
}

#[allow(clippy::too_many_arguments)]
fn prepare_paragraph(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, inlines: &[Inline], w: f64, para: &Para, height: f64) -> Option<Rc<PreparedRich>> {
    let key = (inlines.as_ptr() as usize, w.to_bits(), height.to_bits(), para.bold);
    if d.state.markdown_paragraphs.frozen {
        let entry = d.state.markdown_paragraphs.entries.get(&key)?;
        let prepared = entry.prepared.clone()?;
        find_in_matches(opts, &prepared.rich.find_matches);
        return Some(prepared);
    }
    let semantic = d.state.markdown_paragraphs.snapshot.as_ref()
        .and_then(|snapshot| snapshot.paragraphs.get(&ParagraphKey::of(inlines))).cloned();
    if let Some(semantic) = semantic {
        return prepare_semantic(cx, d, opts, key, semantic, w, para, height, None);
    }
    let style = ParagraphStyle { look: d.look, size: para.size, color: para.color, bold: para.bold, width: w, height };
    let pattern = opts.find.as_ref().map(|find| find.pattern.as_str());
    if let Some(cached) = d.state.markdown_paragraphs.entries.get(&key)
        && cached.len == inlines.len() && cached.style == style && cached.pattern.as_deref() == pattern
        && cached.owner.upgrade().zip(d.state.markdown_paragraphs.owner.as_ref()).is_some_and(|(old, owner)| Rc::ptr_eq(&old, owner))
        && let Some(prepared) = &cached.prepared
    {
        let prepared = prepared.clone();
        if find_in_matches(opts, &prepared.rich.find_matches) == cached.found {
            return Some(prepared);
        }
        if let Some(find) = opts.find.as_deref_mut() { find.seen -= prepared.rich.find_matches.len(); }
    }
    let mut rich = Rich::default();
    build_inlines(&mut rich, inlines, d, para, None);
    if rich.text.trim_matches('\n').is_empty() { return None }
    let prepared = prepare_legacy(cx, d, opts, rich, w, height);
    if let Some(owner) = &d.state.markdown_paragraphs.owner {
        d.state.markdown_paragraphs.entries.insert(key, CachedParagraph {
            owner: Rc::downgrade(owner), len: inlines.len(), style,
            pattern: opts.find.as_ref().map(|find| find.pattern.as_str().to_owned()),
            found: prepared.found, prepared: Some(prepared.clone()), pending: None,
        });
    }
    Some(prepared)
}

#[allow(clippy::too_many_arguments)]
fn prepare_semantic(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, key: (usize, u64, u64, bool), rich: Arc<SemanticParagraph>, w: f64, para: &Para, height: f64, colors: Option<highlight::Colored>) -> Option<Rc<PreparedRich>> {
    if rich.empty && !d.state.markdown_paragraphs.snapshot.as_ref().is_some_and(|snapshot| snapshot.codes.contains_key(&key.0)) { return None }
    if d.state.markdown_paragraphs.frozen {
        let prepared = d.state.markdown_paragraphs.entries.get(&key)?.prepared.clone()?;
        find_in_matches(opts, &prepared.rich.find_matches);
        return Some(prepared);
    }
    let found = find_in_matches(opts, &rich.find_matches);
    let style = ParagraphStyle { look: d.look, size: para.size, color: para.color, bold: para.bold, width: w, height };
    let mut entry = d.state.markdown_paragraphs.entries.remove(&key).unwrap_or_else(|| CachedParagraph {
        owner: d.state.markdown_paragraphs.owner.as_ref().map(Rc::downgrade).unwrap_or_default(),
        len: rich.spans.len(), style, pattern: None, found, prepared: None,
        pending: Some(PendingParagraph {
            remaining: rich.spans.first().map_or(0, |span| span.len),
            rich, spans: Vec::new(), ready_spans: None, cursor: 0, byte: 0,
            colors, color_cursor: 0, layout: AsyncLayout::default(),
        }),
    });
    let inline = entry.pending.as_ref().is_some_and(|pending| {
        !pending.layout.is_pending() && d.state.markdown_paragraphs.can_layout_inline(pending.rich.text.len())
    });
    // Evaluated first: an inline paragraph must not end the pass for the
    // background paragraphs after it.
    if entry.prepared.is_none() && (d.state.markdown_paragraphs.take_advance(Instant::now()) || inline) {
        let pending = entry.pending.as_mut().expect("unpublished paragraph");
        for _ in 0..256 {
            let Some(span) = pending.rich.spans.get(pending.cursor) else { break };
            let mut text_style = span.style;
            text_style.bold |= para.bold;
            let font = if span.code { Font::Mono } else { style_font(text_style) };
            let size = if span.code { d.look.code() } else { para.size };
            let mut color = if span.link { d.look.palette.accent_bright } else { para.color };
            let a = pending.byte;
            let mut b = a + pending.remaining;
            if let Some(colors) = &pending.colors {
                while colors.get(pending.color_cursor).is_some_and(|(range, _)| range.end <= a) {
                    pending.color_cursor += 1;
                }
                if let Some((range, syntax_color)) = colors.get(pending.color_cursor) {
                    if range.start > a {
                        b = b.min(range.start);
                    } else {
                        b = b.min(range.end);
                        color = syntax_color.unwrap_or(color);
                    }
                }
            }
            let mut cuts = [a, b, b, b];
            if let Some((s, e)) = found {
                cuts = [a, s.clamp(a, b), e.clamp(a, b), b];
            }
            for bounds in cuts.windows(2) {
                if bounds[0] == bounds[1] { continue }
                let highlighted = found.is_some_and(|(s, e)| bounds[0] >= s && bounds[1] <= e);
                let mapped = d.kit.rich_span(RichSpan { len: bounds[1] - bounds[0], font, size_px: size, color: if highlighted { d.look.palette.surface0 } else { color } });
                if let Some(last) = pending.spans.last_mut().filter(|last| last.style == mapped.style) {
                    last.len += mapped.len;
                } else {
                    pending.spans.push(mapped);
                }
            }
            pending.byte = b;
            pending.remaining -= b - a;
            if pending.remaining == 0 {
                pending.cursor += 1;
                pending.remaining = pending.rich.spans.get(pending.cursor).map_or(0, |span| span.len);
            }
        }
        if pending.cursor == pending.rich.spans.len() {
            if pending.spans.is_empty() && pending.ready_spans.is_none() {
                pending.spans.push(d.kit.rich_span(RichSpan { len: 0, font: Font::Mono, size_px: d.look.code(), color: para.color }));
            }
            let spans = pending.ready_spans.get_or_insert_with(|| Arc::new(std::mem::take(&mut pending.spans))).clone();
            let base = pending.rich.spans.first().map(|span| {
                let mut style = span.style; style.bold |= para.bold;
                if span.code { Font::Mono } else { style_font(style) }
            }).unwrap_or(Font::Regular);
            let laidout = if inline && d.state.markdown_paragraphs.can_layout_inline(pending.rich.text.len()) {
                d.state.markdown_paragraphs.inline_remaining -= pending.rich.text.len();
                Some(d.kit.layout_mapped_rich(cx, &pending.rich.text, &spans, base, w.is_finite().then_some(w), height))
            } else {
                d.kit.advance_rich_layout(cx, &mut pending.layout, 1, pending.rich.text.clone(), spans, base, w.is_finite().then_some(w), height);
                (!pending.layout.is_pending()).then(|| pending.layout.completed().cloned()).flatten()
            };
            if let Some(laidout) = laidout {
                entry.prepared = Some(Rc::new(PreparedRich { rich: pending.rich.clone(), laidout, found }));
                entry.pending = None;
            }
        }
    }
    let prepared = entry.prepared.clone();
    d.state.markdown_paragraphs.pending |= prepared.is_none();
    d.state.markdown_paragraphs.entries.insert(key, entry);
    prepared
}

impl Rich {
    fn push(&mut self, s: &str, font: Font, size: f64, color: Vec4) {
        if s.is_empty() {
            return;
        }
        self.text.push_str(s);
        match self.spans.last_mut() {
            Some(last) if last.font == font && last.size_px == size && last.color == color => last.len += s.len(),
            _ => self.spans.push(RichSpan { len: s.len(), font, size_px: size, color }),
        }
    }

    /// Recolors bytes `start..end` (find highlight text).
    fn recolor(&mut self, start: usize, end: usize, color: Vec4) {
        let mut out = Vec::with_capacity(self.spans.len() + 2);
        let mut offset = 0;
        for span in &self.spans {
            let (a, b) = (offset, offset + span.len);
            offset = b;
            let (s, e) = (start.max(a), end.min(b));
            if s >= e {
                out.push(*span);
                continue;
            }
            if s > a {
                out.push(RichSpan { len: s - a, ..*span });
            }
            out.push(RichSpan { len: e - s, color, ..*span });
            if b > e {
                out.push(RichSpan { len: b - e, ..*span });
            }
        }
        self.spans = out;
    }
}

fn style_font(style: Style) -> Font {
    match (style.bold, style.italic) {
        (true, true) => Font::BoldItalic,
        (true, false) => Font::Bold,
        (false, true) => Font::Italic,
        (false, false) => Font::Regular,
    }
}

struct Para {
    size: f64,
    color: Vec4,
    bold: bool,
}

fn build_inlines(rich: &mut Rich, inlines: &[Inline], d: &Draw, para: &Para, link: Option<(String, FileLinkSourceType)>) {
    let p = d.look.palette;
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                let mut style = *style;
                style.bold |= para.bold;
                let start = rich.text.len();
                let color = if link.is_some() { p.accent_bright } else { para.color };
                rich.push(text, style_font(style), para.size, color);
                if style.strike {
                    rich.strikes.push((start, rich.text.len()));
                }
            }
            Inline::Code { text, style } => {
                let start = rich.text.len();
                let color = if link.is_some() { p.accent_bright } else { para.color };
                rich.push(text, Font::Mono, d.look.code(), color);
                rich.codes.push((start, rich.text.len()));
                if style.strike {
                    rich.strikes.push((start, rich.text.len()));
                }
            }
            Inline::Link { href, kind, children } => {
                let start = rich.text.len();
                build_inlines(rich, children, d, para, Some((href.clone(), *kind)));
                let end = rich.text.len();
                if end > start {
                    rich.links.push((start, end, href.clone(), *kind));
                }
            }
            // Images are split out by the caller; alt text stays searchable.
            Inline::Image { .. } => {}
            Inline::Break => rich.push("\n", style_font(Style { bold: para.bold, ..Style::default() }), para.size, para.color),
        }
    }
}

/// Draws a rich paragraph at (x, y) wrapped to `w`; returns its height.
#[allow(clippy::too_many_arguments)]
fn draw_rich(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, rich: Rich, x: f64, y: f64, w: f64, height: f64) -> f64 {
    if rich.text.is_empty() { return 0.0 }
    let prepared = prepare_legacy(cx, d, opts, rich, w, height);
    draw_prepared(cx, d, opts, &prepared, x, y, w, height)
}

fn prepare_legacy(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, mut rich: Rich, w: f64, height: f64) -> Rc<PreparedRich> {
    let matches: Arc<[(usize, usize)]> = opts.find.as_ref().map(|find| {
        find.pattern.find_iter(&rich.text).map(|m| (m.start(), m.end())).collect::<Vec<_>>().into()
    }).unwrap_or_default();
    let found = find_in_matches(opts, &matches);
    if let Some((s, e)) = found { rich.recolor(s, e, d.look.palette.surface0); }
    let laidout = d.kit.layout_rich(cx, &rich.text, &rich.spans, w.is_finite().then_some(w), height);
    let mut links: Vec<_> = rich.links.into_iter().map(|(s, e, href, kind)| {
        (s, e, Arc::<str>::from(href), kind, Arc::<str>::from(&rich.text[s..e]))
    }).collect();
    links.sort_by_key(|link| (link.0, link.1));
    let mut end = 0;
    let link_max_ends = links.iter().map(|link| { end = end.max(link.1); end }).collect::<Vec<_>>().into();
    let copy_end = rich.text.trim_end_matches('\n').len();
    Rc::new(PreparedRich {
        rich: Arc::new(SemanticParagraph {
            text: rich.text.into(), empty: false, copy_end, spans: Arc::from([]),
            codes: rich.codes.into(), strikes: rich.strikes.into(), links: links.into(), link_max_ends,
            find_matches: matches,
        }),
        laidout, found,
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_prepared(cx: &mut Cx2d, d: &mut Draw, _opts: &mut MdOpts, prepared: &PreparedRich, x: f64, y: f64, _w: f64, _height: f64) -> f64 {
    let rich = &prepared.rich;
    let p = d.look.palette;
    let found = prepared.found;
    let laidout = prepared.laidout.clone();
    let size = laidout_size(&laidout);
    if d.dry() {
        return size.y;
    }
    let pos = dvec2(x, y);
    let visible = d.kit.viewport.map(|viewport| laidout.visible_row_range(
        (viewport.pos.y - y) as f32, (viewport.pos.y + viewport.size.y - y) as f32,
    )).unwrap_or(0..laidout.rows.len());
    let visible_start = laidout.rows.get(visible.start).map(|row| row.text.start_in_parent()).unwrap_or(laidout.text.len());
    let visible_end = visible.end.checked_sub(1).and_then(|index| laidout.rows.get(index)).map(|row| row.text.end_in_parent()).unwrap_or(0);
    for &(s, e) in rich.codes.iter().skip(rich.codes.partition_point(|&(_, end)| end <= visible_start)).take_while(|&&(start, _)| start < visible_end) {
        for r in range_rects_visible(&laidout, s, e, pos, d.kit.viewport) {
            let r = r.translate(pos);
            d.kit.rect(cx, Rect { pos: r.pos - dvec2(3.0, 0.0), size: r.size + dvec2(6.0, 0.0) }, p.surface2, 4.0);
        }
    }
    if let Some((s, e)) = found {
        let location = laidout.cursor_to_position(Cursor { index: s, prefer_next_row: true });
        if let Some(row) = laidout.rows.get(location.row_index) {
            d.state.find_y.get_or_insert(y + (row.origin_in_lpxs.y - row.ascender_in_lpxs) as f64);
        }
        for r in range_rects_visible(&laidout, s, e, pos, d.kit.viewport) {
            d.kit.rect(cx, r.translate(pos), p.warning, 2.0);
        }
    }
    d.kit.draw_rich(cx, pos, &laidout);
    for &(s, e) in rich.strikes.iter().skip(rich.strikes.partition_point(|&(_, end)| end <= visible_start)).take_while(|&&(start, _)| start < visible_end) {
        for r in range_rects_visible(&laidout, s, e, pos, d.kit.viewport) {
            let r = r.translate(pos);
            d.kit.rect(cx, rect_xywh(r.pos.x, r.pos.y + r.size.y * 0.5, r.size.x, 1.0), p.foreground, 0.0);
        }
    }
    let first_link = rich.link_max_ends.partition_point(|&end| end <= visible_start);
    for (s, e, href, kind, text) in rich.links.iter().skip(first_link).take_while(|link| link.0 < visible_end) {
        let hovered = d.state.hover_link.as_deref() == Some(href.as_ref()) && !d.look.mobile;
        for r in range_rects_visible(&laidout, *s, *e, pos, d.kit.viewport) {
            let r = r.translate(pos);
            if hovered {
                d.kit.rect(cx, rect_xywh(r.pos.x, r.pos.y + r.size.y - 2.0, r.size.x, 1.0), p.accent_bright, 0.0);
            }
            d.canvas.add_hit(r, HitTarget::Link { href: href.clone(), kind: *kind, text: text.clone() });
        }
    }
    d.canvas.add_run(laidout, pos, Some('\n'));
    size.y
}

#[allow(clippy::too_many_arguments)]
fn paragraph(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, inlines: &[Inline], x: f64, y: f64, w: f64, para: &Para, height: f64) -> f64 {
    if d.state.markdown_paragraphs.snapshot.as_ref().is_some_and(|snapshot| snapshot.paragraphs.contains_key(&ParagraphKey::of(inlines))) {
        return prepare_paragraph(cx, d, opts, inlines, w, para, height)
            .map(|prepared| draw_prepared(cx, d, opts, &prepared, x, y, w, height)).unwrap_or(0.0);
    }
    // Images break the paragraph into text / image / text segments.
    let mut cursor = y;
    let mut start = 0;
    let mut first = true;
    let flush = |cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, segment: &[Inline], cursor: &mut f64, first: &mut bool| {
        if segment.is_empty() {
            return;
        }
        let Some(prepared) = prepare_paragraph(cx, d, opts, segment, w, para, height) else { return };
        if !*first {
            *cursor += 8.0;
        }
        *cursor += draw_prepared(cx, d, opts, &prepared, x, *cursor, w, height);
        *first = false;
    };
    for (index, inline) in inlines.iter().enumerate() {
        if let Inline::Image { url, alt } = inline {
            flush(cx, d, opts, &inlines[start..index], &mut cursor, &mut first);
            start = index + 1;
            if !first {
                cursor += 8.0;
            }
            cursor += image(cx, d, opts, url, alt, x, cursor, w, para);
            first = false;
        }
    }
    flush(cx, d, opts, &inlines[start..], &mut cursor, &mut first);
    cursor - y
}

/// markdown/images.ts: a 320 high contain box with 8px corners; the box stays
/// reserved while loading; an unreadable image shows its alt text.
#[allow(clippy::too_many_arguments)]
fn image(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, url: &str, alt: &str, x: f64, y: f64, w: f64, para: &Para) -> f64 {
    let state = images::image(cx, opts.core, opts.host_id, opts.workspace_root, url, opts.key);
    match state {
        ImageState::Loading => 320.0,
        ImageState::Failed => {
            if alt.is_empty() {
                return 0.0;
            }
            let mut rich = Rich::default();
            rich.push(alt, Font::Regular, para.size, para.color);
            draw_rich(cx, d, opts, rich, x, y, w, 1.4)
        }
        ImageState::Ready { texture, width, height } => {
            let aspect = width as f64 / height.max(1) as f64;
            let mut ih = 320.0f64;
            let mut iw = ih * aspect;
            if iw > w {
                iw = w;
                ih = iw / aspect;
            }
            let r = rect_xywh(x + (w - iw) * 0.5, y + (320.0 - ih) * 0.5, iw, ih);
            d.kit.texture(cx, r, &texture, 8.0);
            d.canvas.add_hit(r, HitTarget::Image { key: url.to_owned() });
            320.0
        }
    }
}

fn heading_scale(level: u8) -> f64 {
    match level {
        1 => 1.6,
        2 => 1.4,
        3 => 1.2,
        4 => 1.1,
        _ => 1.0,
    }
}

/// Draws `blocks`; returns the total height.
#[allow(clippy::too_many_arguments)]
pub fn draw_blocks(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, blocks: &[Block], x: f64, y: f64, w: f64, color: Vec4, tight: bool) -> f64 {
    let gap = if tight { 4.0 } else { block_gap(d.look.compact) };
    let mut cursor = y;
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            cursor += gap;
        }
        cursor += draw_block(cx, d, opts, block, x, cursor, w, color, 0, None);
    }
    cursor - y
}

#[derive(Clone, Copy, PartialEq)]
enum Marker {
    Bullet,
    Ordered,
    Task(bool),
}

/// enriched-markdown `BaseListSpan`, relative to the list's `x` (the enclosing
/// item's text edge): the marker's right edge and the item text's left edge.
/// Nesting level d reserves its marker column d × marginLeft (20) from the root
/// list, and the text starts gapWidth (7) after the item's own column,
/// truncated like the span's Int leading margin. The app's pinned nightly
/// indented nested text a fixed 20 from its parent's text instead, which drew
/// a marker wider than the parent's (a number under a bullet) over the text.
/// `parent` is the enclosing item's column width and `marker` this item's,
/// both without `extra`: the whole pixels an ordered list adds to fit numbers
/// wider than "99.".
fn list_columns(parent: Option<f64>, marker: f64, extra: f64) -> (f64, f64) {
    let origin = parent.map_or(0.0, |parent| 20.0 - (parent + 7.0).floor());
    (origin + marker + extra, origin + (marker + 7.0).floor() + extra)
}

#[allow(clippy::too_many_arguments)]
fn draw_block(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, block: &Block, x: f64, y: f64, w: f64, color: Vec4, depth: usize, parent: Option<f64>) -> f64 {
    let p = d.look.palette;
    let fs = opts.font_size;
    match block {
        Block::Paragraph(inlines) => paragraph(cx, d, opts, inlines, x, y, w, &Para { size: fs, color, bold: false }, (fs * 1.4).round() / fs),
        Block::Heading { level, inlines } => {
            let size = (fs * heading_scale(*level)).round();
            let color = if *level == 6 { p.muted } else { color };
            paragraph(cx, d, opts, inlines, x, y, w, &Para { size, color, bold: true }, (fs * heading_scale(*level) * 1.3).round() / size)
        }
        Block::Rule => {
            d.kit.rect(cx, rect_xywh(x, y + 8.0, w, 1.0), p.border, 0.0);
            17.0
        }
        Block::Code { lang, text } => code_block(cx, d, opts, lang, text, x, y, w),
        Block::Quote(blocks) => {
            let inner_x = x + 23.0;
            let inner_w = (w - 35.0).max(1.0);
            let quote_color = with_alpha(p.foreground, p.foreground.w * 0.8);
            // Measure first so the quote box renders beneath its content.
            let h = measured(cx, d, opts, |cx, d, opts| draw_blocks(cx, d, opts, blocks, inner_x, y + 12.0, inner_w, quote_color, false)) + 24.0;
            d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface1, 0.0);
            d.kit.rect(cx, rect_xywh(x, y, 3.0, h), p.surface2, 0.0);
            draw_blocks(cx, d, opts, blocks, inner_x, y + 12.0, inner_w, quote_color, false);
            h
        }
        Block::Alert { kind, blocks } => {
            let (tint, icon, title) = match kind {
                AlertKind::Note => (p.accent_bright, "info", "Note"),
                AlertKind::Tip => (p.success, "lightbulb", "Tip"),
                AlertKind::Important => (p.merged, "message-square-warning", "Important"),
                AlertKind::Warning => (p.warning, "triangle-alert", "Warning"),
                AlertKind::Caution => (p.danger, "octagon-alert", "Caution"),
            };
            let icon_size = fs.ceil();
            let band = (icon_size.max(fs * 1.2)).ceil();
            let body_y = y + 12.0 + band + (fs * 0.4).round();
            let inner_w = (w - 35.0).max(1.0);
            let quote_color = with_alpha(p.foreground, p.foreground.w * 0.8);
            let body = measured(cx, d, opts, |cx, d, opts| draw_blocks(cx, d, opts, blocks, x + 23.0, body_y, inner_w, quote_color, false));
            let h = body + (body_y - y) + 12.0;
            d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface2, 0.0);
            d.kit.rect(cx, rect_xywh(x, y, 3.0, h), tint, 0.0);
            d.kit.icon(cx, icon, rect_xywh(x + 23.0, y + 12.0, icon_size, band), icon_size, tint);
            d.kit.label(cx, Font::Bold, fs, 1.2, tint, dvec2(x + 23.0 + icon_size + (icon_size * 0.4).round(), y + 12.0), title, None);
            draw_blocks(cx, d, opts, blocks, x + 23.0, body_y, inner_w, quote_color, false);
            h
        }
        Block::List { start, items } => list(cx, d, opts, *start, items, x, y, w, color, depth, parent),
        Block::Table { aligns, head, rows } => table(cx, d, opts, aligns, head, rows, x, y, w, color),
    }
}

/// A measuring pass that leaves the find and key counters untouched.
pub fn measured<R>(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, f: impl FnOnce(&mut Cx2d, &mut Draw, &mut MdOpts) -> R) -> R {
    let seen = opts.find.as_deref().map(|f| f.seen);
    let counter = opts.counter;
    let result = d.measure(|d| f(cx, d, opts));
    if let (Some(seen), Some(find)) = (seen, opts.find.as_deref_mut()) {
        find.seen = seen;
    }
    opts.counter = counter;
    result
}

#[allow(clippy::too_many_arguments)]
fn list(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, start: Option<u64>, items: &[ListItem], x: f64, y: f64, w: f64, color: Vec4, depth: usize, parent: Option<f64>) -> f64 {
    let p = d.look.palette;
    let fs = opts.font_size;
    let line = (fs * 1.4).round();
    // OrderedListSpan: every ordered list reserves the "99." column, widened by
    // whole pixels when one of its numbers is wider.
    let ordered = start.map(|first| {
        let column = d.kit.measure(cx, Font::Medium, fs, 1.4, "99.", None).x;
        let labels: Vec<(String, f64)> = (0..items.len() as u64).map(|index| {
            let label = format!("{}.", first + index);
            let width = d.kit.measure(cx, Font::Medium, fs, 1.4, &label, None).x;
            (label, width)
        }).collect();
        let widest = labels.iter().fold(column, |widest, (_, width)| widest.max(*width));
        (column, (widest - column).ceil(), labels)
    });
    let mut cursor = y;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            cursor += 4.0;
        }
        let (kind, marker, extra) = match (item.task, &ordered) {
            (Some(checked), _) => (Marker::Task(checked), 14.0, 0.0),
            (None, Some((column, extra, _))) => (Marker::Ordered, *column, *extra),
            (None, None) => (Marker::Bullet, 3.0, 0.0),
        };
        let (anchor, column) = list_columns(parent, marker, extra);
        let center = cursor + line * 0.5;
        match kind {
            Marker::Ordered => {
                if let Some((label, width)) = ordered.as_ref().and_then(|(_, _, labels)| labels.get(index)) {
                    d.kit.label(cx, Font::Medium, fs, 1.4, p.muted, dvec2(x + anchor - width, cursor), label, None);
                }
            }
            Marker::Bullet => {
                let r = rect_xywh(x + anchor - 3.0, center - 3.0, 6.0, 6.0);
                match depth {
                    0 => d.kit.rect(cx, r, p.muted, 3.0),
                    1 => d.kit.bordered(cx, r, TRANSPARENT, 3.0, p.muted, 0.9),
                    _ => d.kit.rect(cx, r, p.muted, 0.0),
                }
            }
            Marker::Task(checked) => {
                let r = rect_xywh(x + anchor - 14.0, center - 7.0, 14.0, 14.0);
                if checked {
                    d.kit.rect(cx, r, p.accent, 3.0);
                    d.kit.icon(cx, "check", r, 11.0, p.on_tertiary);
                } else {
                    d.kit.bordered(cx, r, TRANSPARENT, 3.0, p.muted, 1.3);
                }
            }
        }
        // taskList.checkedTextColor: muted text for a checked item.
        let item_color = if kind == Marker::Task(true) { p.muted } else { color };
        let mut item_y = cursor;
        for (b, block) in item.blocks.iter().enumerate() {
            if b > 0 {
                item_y += 4.0;
            }
            item_y += draw_block(cx, d, opts, block, x + column, item_y, (w - column).max(1.0), item_color, depth + 1, Some(marker));
        }
        cursor = item_y.max(cursor + line);
    }
    cursor - y
}


/// TableContainerView: bold header on surface2, 1px border with a 6px radius,
/// 12x8 cell padding, columns sized to content between 60 and 300 (plus
/// padding); body rows alternate surface0/surface1; wide tables scroll sideways.
#[allow(clippy::too_many_arguments)]
fn table(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, aligns: &[Alignment], head: &[Vec<Inline>], rows: &[Vec<Vec<Inline>>], x: f64, y: f64, w: f64, color: Vec4) -> f64 {
    let p = d.look.palette;
    let fs = opts.font_size;
    let height = (fs * 1.4).round() / fs;
    let columns = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return 0.0;
    }
    let key = opts.next_key("table");
    let mut widths = vec![84.0f64; columns];
    let matches = measured(cx, d, opts, |cx, d, opts| {
        let before = opts.find.as_ref().map_or(0, |find| find.seen);
        for (bold, cells) in std::iter::once((true, head)).chain(rows.iter().map(|row| (false, row.as_slice()))) {
            for (c, cell) in cells.iter().enumerate() {
                if let Some(prepared) = prepare_paragraph(cx, d, opts, cell, f64::INFINITY, &Para { size: fs, color, bold }, height) {
                    widths[c] = widths[c].max(laidout_size(&prepared.laidout).x + 24.0);
                }
            }
        }
        opts.find.as_ref().map_or(0, |find| find.seen).saturating_sub(before)
    });
    if d.state.markdown_paragraphs.pending {
        if let Some(find) = opts.find.as_deref_mut() { find.seen += matches; }
        return 0.0;
    }
    for width in &mut widths {
        *width = width.min(324.0);
    }
    let total: f64 = widths.iter().sum();
    let max_scroll = (total - w).max(0.0);
    d.state.extent_x.insert(key.clone(), max_scroll);
    let scroll = d.state.scroll_x.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_scroll);
    let view = rect_xywh(x, y, w, 0.0);

    // Row heights.
    let mut row_heights = Vec::with_capacity(rows.len() + 1);
    let all_rows: Vec<(bool, &[Vec<Inline>])> = std::iter::once((true, head)).chain(rows.iter().map(|r| (false, r.as_slice()))).collect();
    measured(cx, d, opts, |cx, d, opts| {
        for (bold, cells) in &all_rows {
            let mut h: f64 = 0.0;
            for (c, cell) in cells.iter().enumerate() {
                if let Some(prepared) = prepare_paragraph(cx, d, opts, cell, widths[c] - 24.0, &Para { size: fs, color, bold: *bold }, height) {
                    h = h.max(laidout_size(&prepared.laidout).y);
                }
            }
            row_heights.push(h.max((fs * 1.4).round()) + 16.0);
        }
    });
    if d.state.markdown_paragraphs.pending {
        if let Some(find) = opts.find.as_deref_mut() { find.seen += matches; }
        return 0.0;
    }
    let table_h: f64 = row_heights.iter().sum();
    let clip = Rect { size: dvec2(w, table_h), ..view };
    cx.begin_turtle(Walk { abs_pos: Some(clip.pos), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(table_h) }, Layout::default());
    d.canvas.push_clip(clip);
    let tx = x - scroll;
    d.kit.rect(cx, rect_xywh(tx, y, total, table_h), p.surface0, 6.0);
    let mut cy = y;
    for (r, (bold, cells)) in all_rows.iter().enumerate() {
        let rh = row_heights[r];
        let fill = if *bold { p.surface2 } else if r % 2 == 0 { p.surface1 } else { p.surface0 };
        let radii = if r == 0 { vec4(6.0, 6.0, 0.0, 0.0) } else if r + 1 == all_rows.len() { vec4(0.0, 0.0, 6.0, 6.0) } else { vec4(0.0, 0.0, 0.0, 0.0) };
        d.kit.rect_full(cx, rect_xywh(tx, cy, total, rh), fill, radii, TRANSPARENT, 0.0);
        let mut cx_pos = tx;
        for (c, &col_w) in widths.iter().enumerate().take(columns) {
            if let Some(cell) = cells.get(c) {
                let laid_w = col_w - 24.0;
                if let Some(prepared) = prepare_paragraph(cx, d, opts, cell, laid_w, &Para { size: fs, color, bold: *bold }, height) {
                    let size = laidout_size(&prepared.laidout);
                    let dx = match aligns.get(c) {
                        Some(Alignment::Center) => (laid_w - size.x) * 0.5,
                        Some(Alignment::Right) => laid_w - size.x,
                        _ => 0.0,
                    };
                    draw_prepared(cx, d, opts, &prepared, cx_pos + 12.0 + dx.max(0.0), cy + 8.0, laid_w, height);
                }
            }
            if c > 0 {
                d.kit.rect(cx, rect_xywh(cx_pos, cy, 1.0, rh), p.border, 0.0);
            }
            cx_pos += col_w;
        }
        if r > 0 {
            d.kit.rect(cx, rect_xywh(tx, cy, total, 1.0), p.border, 0.0);
        }
        cy += rh;
    }
    d.kit.bordered(cx, rect_xywh(tx, y, total, table_h), TRANSPARENT, 6.0, p.border, 1.0);
    d.canvas.pop_clip();
    cx.end_turtle();
    if max_scroll > 0.0 {
        d.canvas.add_hit(clip, HitTarget::ScrollX(key));
    }
    table_h
}

/// Code text colored by syntax ranges, as one rich text.
pub fn code_rich(code: &str, colors: Option<&highlight::Colored>, fg: Vec4, size: f64) -> (String, Vec<RichSpan>) {
    let mut spans = Vec::new();
    match colors {
        Some(colors) => {
            let mut offset = 0;
            for (range, color) in colors.iter() {
                if range.start > offset {
                    spans.push(RichSpan { len: range.start - offset, font: Font::Mono, size_px: size, color: fg });
                }
                if range.end > range.start {
                    spans.push(RichSpan { len: range.end - range.start, font: Font::Mono, size_px: size, color: color.unwrap_or(fg) });
                }
                offset = range.end;
            }
            if offset < code.len() {
                spans.push(RichSpan { len: code.len() - offset, font: Font::Mono, size_px: size, color: fg });
            }
        }
        None => spans.push(RichSpan { len: code.len(), font: Font::Mono, size_px: size, color: fg }),
    }
    (code.to_owned(), spans)
}

#[allow(clippy::too_many_arguments)]
fn code_block(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, lang: &str, text: &str, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let code_size = d.look.code();
    let key = opts.next_key("code");
    let mobile = d.look.mobile && !lang.trim().eq_ignore_ascii_case("mermaid");
    let line_height = if mobile { (d.look.content() * 1.4).round() / code_size } else { (code_size * 1.45).round() / code_size };
    let layout_width = if mobile { f64::INFINITY } else { (w - 24.0).max(1.0) };
    let semantic = d.state.markdown_paragraphs.snapshot.as_ref().and_then(|snapshot| snapshot.codes.get(&(text.as_ptr() as usize))).cloned();
    let (laid, found, copy_text) = if let Some(semantic) = semantic {
        let colors = if d.state.markdown_paragraphs.frozen {
            None
        } else {
            let request = d.state.fence_highlights.entry(key.clone()).or_default();
            let colors = highlight::fence(request, text, lang, opts.syntax_theme, d.look.dark());
            if request.is_pending() {
                find_in_matches(opts, &semantic.find_matches);
                d.state.markdown_paragraphs.pending = true;
                return 0.0;
            }
            colors
        };
        let cache_key = (text.as_ptr() as usize, layout_width.to_bits(), line_height.to_bits(), false);
        let Some(prepared) = prepare_semantic(cx, d, opts, cache_key, semantic, layout_width, &Para { size: code_size, color: p.foreground, bold: false }, line_height, colors) else { return 0.0 };
        let copy = Substr::from(prepared.rich.text.clone()).substr(..prepared.rich.copy_end);
        (prepared.laidout.clone(), prepared.found, copy)
    } else {
        let request = d.state.fence_highlights.entry(key.clone()).or_default();
        let colors = highlight::fence(request, text, lang, opts.syntax_theme, d.look.dark());
        let (code, spans) = code_rich(text, colors.as_ref(), p.foreground, code_size);
        let mut rich = Rich { text: code, spans, ..Default::default() };
        let matches: Vec<_> = opts.find.as_ref().map(|find| find.pattern.find_iter(text).map(|m| (m.start(), m.end())).collect()).unwrap_or_default();
        let found = find_in_matches(opts, &matches);
        if let Some((s, e)) = found { rich.recolor(s, e, p.surface0); }
        let laid = d.kit.layout_rich(cx, &rich.text, &rich.spans, layout_width.is_finite().then_some(layout_width), line_height);
        (laid, found, Substr::from(text.trim_end_matches('\n')))
    };
    let copied = d.state.copied_now(&key, d.now);
    d.animating |= copied;
    if mobile {
        // CodeBlockContainerView: bordered header (language, copy), divider,
        // horizontally scrolling non-wrapping pane at the paragraph line height.
        let inset = 13.0;
        let label = code_size * 0.85;
        let header = inset + (label * 1.171875).ceil();
        let divider = header + inset / 2.0 - 1.5;
        let size = laidout_size(&laid);
        let pane_top = y + divider + 1.0 + (header + inset - 1.0 - divider - 1.0);
        let h = (pane_top - y) + size.y + inset;
        d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface2, 8.0, p.border, 1.0);
        let secondary = with_alpha(p.foreground, 0x99 as f32 / 255.0);
        d.kit.label(cx, Font::Medium, label, 1.171875, secondary, dvec2(x + inset - 1.0, y + (divider - label * 1.17) * 0.5), &highlight::display_language_name(lang), Some((w - 2.0 * inset - header).max(1.0)));
        let copy = rect_xywh(x + w - inset + 1.0 - header, y, header, divider);
        d.kit.icon(cx, if copied { "check" } else { "copy" }, copy, (code_size * 1.08).ceil(), secondary);
        d.canvas.add_hit(copy, HitTarget::Copy { key: key.clone(), text: copy_text });
        d.kit.rect(cx, rect_xywh(x + 1.0, y + divider, w - 2.0, 1.0), with_alpha(p.foreground, 0x33 as f32 / 255.0), 0.0);
        let pane = rect_xywh(x + 1.0, y + divider + 1.0, w - 2.0, h - divider - 2.0);
        let max_scroll = (size.x + 2.0 * (inset - 1.0) - pane.size.x).max(0.0);
        d.state.extent_x.insert(key.clone(), max_scroll);
        let scroll = d.state.scroll_x.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_scroll);
        cx.begin_turtle(Walk { abs_pos: Some(pane.pos), margin: Margin::default(), width: Size::Fixed(pane.size.x), height: Size::Fixed(pane.size.y) }, Layout::default());
        d.canvas.push_clip(pane);
        let origin = dvec2(x + inset - 1.0 - scroll, pane_top);
        paint_find(cx, d, &laid, origin, found);
        d.kit.draw_rich(cx, origin, &laid);
        d.canvas.add_run(laid, origin, Some('\n'));
        d.canvas.pop_clip();
        cx.end_turtle();
        if max_scroll > 0.0 {
            d.canvas.add_hit(pane, HitTarget::ScrollX(key));
        }
        return h;
    }
    // components/highlighted-code-block.tsx: surface2, 8px radius, 12 padding,
    // wrapped at 1.45 line height, copy glyph 8px from the top-right corner.
    let size = laidout_size(&laid);
    let h = size.y.max((code_size * 1.45).round()) + 24.0;
    d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface2, 8.0);
    let origin = dvec2(x + 12.0, y + 12.0);
    paint_find(cx, d, &laid, origin, found);
    d.kit.draw_rich(cx, origin, &laid);
    d.canvas.add_run(laid, origin, Some('\n'));
    let visible = d.look.compact || d.state.hovered;
    let copy = rect_xywh(x + w - 30.0, y + 8.0, 22.0, 22.0);
    if visible {
        let hovered = d.state.hover_hit.as_deref() == Some(key.as_str());
        d.kit.icon(cx, if copied { "check" } else { "copy" }, copy, 14.0, if hovered { p.foreground } else { p.muted });
        d.canvas.add_hit(copy, HitTarget::Copy { key, text: copy_text });
    }
    h
}

fn paint_find(cx: &mut Cx2d, d: &mut Draw, laid: &LaidoutText, origin: DVec2, found: Option<(usize, usize)>) {
    if d.dry() { return }
    let Some((s, e)) = found else { return };
    let location = laid.cursor_to_position(Cursor { index: s, prefer_next_row: true });
    if let Some(row) = laid.rows.get(location.row_index) {
        d.state.find_y.get_or_insert(origin.y + (row.origin_in_lpxs.y - row.ascender_in_lpxs) as f64);
    }
    for r in range_rects_visible(laid, s, e, origin, d.kit.viewport) {
        d.kit.rect(cx, r.translate(origin), d.look.palette.warning, 2.0);
    }
}

#[cfg(test)]
mod list_column_tests {
    use super::list_columns;

    // Marker column widths: bullet radius, task box, and a "99." number column.
    const KINDS: [f64; 3] = [3.0, 14.0, 16.7];

    #[test]
    fn every_nesting_keeps_the_marker_left_of_the_item_text() {
        for marker in KINDS {
            let (anchor, text) = list_columns(None, marker, 0.0);
            assert_eq!((anchor, text), (marker, (marker + 7.0).floor()));
            for parent in KINDS {
                let (anchor, text) = list_columns(Some(parent), marker, 0.0);
                // A bullet is centred on its column edge, so its 3px radius hangs into the gap.
                assert!(anchor + 3.0 < text, "{marker} under {parent}: marker {anchor}, text {text}");
                assert!(text - anchor <= 7.0, "{marker} under {parent}: gap {}", text - anchor);
            }
        }
    }

    #[test]
    fn same_kind_nesting_indents_by_twenty() {
        for marker in KINDS {
            let (root_anchor, root_text) = list_columns(None, marker, 0.0);
            let (anchor, text) = list_columns(Some(marker), marker, 0.0);
            assert_eq!(text, 20.0);
            assert!(((text - anchor) - (root_text - root_anchor)).abs() < 1e-9);
        }
    }

    #[test]
    fn numbers_wider_than_the_default_column_stay_inside_it() {
        let (column, widest) = (16.7_f64, 24.3_f64);
        let extra = (widest - column).ceil();
        for parent in [None, Some(3.0), Some(column)] {
            let (anchor, text) = list_columns(parent, column, extra);
            let (plain_anchor, plain_text) = list_columns(parent, column, 0.0);
            assert!(anchor - widest >= plain_anchor - column, "the number starts inside its column");
            assert!(((text - anchor) - (plain_text - plain_anchor)).abs() < 1e-9);
        }
    }
}

#[cfg(test)]
mod preparation_pass_tests {
    use super::{PREPARATION_PASS, ParagraphCache};
    use std::time::{Duration, Instant};

    fn pass(start: Instant) -> ParagraphCache {
        ParagraphCache { advance_available: true, advance_deadline: Some(start + PREPARATION_PASS), ..Default::default() }
    }

    #[test]
    fn one_pass_admits_every_paragraph_inside_its_allowance() {
        let start = Instant::now();
        let mut cache = pass(start);
        // A complete long message: every paragraph is requested by this pass,
        // not one paragraph for each frame.
        assert!((0..500).all(|_| cache.take_advance(start + Duration::from_millis(1))));
        // The paragraph that crosses the deadline is the last one of the pass.
        assert!(cache.take_advance(start + PREPARATION_PASS));
        assert!(!cache.take_advance(start + PREPARATION_PASS));
    }

    #[test]
    fn a_late_pass_still_advances_one_paragraph() {
        let start = Instant::now();
        let mut cache = pass(start);
        let late = start + Duration::from_secs(1);
        assert!(cache.take_advance(late));
        assert!(!cache.take_advance(late));
    }

    #[test]
    fn the_published_snapshot_admits_no_advance() {
        let mut cache = ParagraphCache::default();
        assert!(!cache.take_advance(Instant::now()));
    }
}
