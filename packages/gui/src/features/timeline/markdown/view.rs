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
    rich: Rich,
    laidout: Rc<LaidoutText>,
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

struct CachedParagraph {
    owner: Weak<Vec<Block>>,
    len: usize,
    style: ParagraphStyle,
    prepared: Rc<PreparedRich>,
}

/// Pointer keys are valid only while the exact immutable parse owner survives.
/// Weak ownership prevents old revealed tails from retaining whole documents.
#[derive(Default)]
pub struct ParagraphCache {
    owner: Option<Rc<Vec<Block>>>,
    entries: HashMap<usize, CachedParagraph>,
}

impl ParagraphCache {
    pub(super) fn set_owner(&mut self, owner: Option<Rc<Vec<Block>>>) -> Option<Rc<Vec<Block>>> {
        if let Some(next) = &owner
            && !self.owner.as_ref().is_some_and(|old| Rc::ptr_eq(old, next))
        {
            self.entries.retain(|_, entry| entry.owner.strong_count() > 0);
        }
        std::mem::replace(&mut self.owner, owner)
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_paragraph(cx: &mut Cx2d, d: &mut Draw, inlines: &[Inline], w: f64, para: &Para, height: f64) -> Option<Rc<PreparedRich>> {
    let key = inlines.as_ptr() as usize;
    let style = ParagraphStyle { look: d.look, size: para.size, color: para.color, bold: para.bold, width: w, height };
    let owner = d.state.markdown_paragraphs.owner.clone();
    if let Some(owner) = &owner
        && let Some(cached) = d.state.markdown_paragraphs.entries.get(&key)
        && cached.len == inlines.len() && cached.style == style
        && cached.owner.upgrade().is_some_and(|old| Rc::ptr_eq(&old, owner))
    {
        return Some(cached.prepared.clone());
    }
    let mut rich = Rich::default();
    build_inlines(&mut rich, inlines, d, para, None);
    if rich.text.trim_matches('\n').is_empty() {
        return None;
    }
    let laidout = d.kit.layout_rich(cx, &rich.text, &rich.spans, Some(w), height);
    let prepared = Rc::new(PreparedRich { rich, laidout });
    if let Some(owner) = owner {
        d.state.markdown_paragraphs.entries.insert(key, CachedParagraph {
            owner: Rc::downgrade(&owner), len: inlines.len(), style, prepared: prepared.clone(),
        });
    }
    Some(prepared)
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
    if rich.text.is_empty() {
        return 0.0;
    }
    let laidout = d.kit.layout_rich(cx, &rich.text, &rich.spans, Some(w), height);
    draw_prepared(cx, d, opts, &PreparedRich { rich, laidout }, x, y, w, height)
}

#[allow(clippy::too_many_arguments)]
fn draw_prepared(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, prepared: &PreparedRich, x: f64, y: f64, w: f64, height: f64) -> f64 {
    let rich = &prepared.rich;
    let p = d.look.palette;
    // Find: the target occurrence's range inside this block, if it falls here.
    let mut found = None;
    if let Some(find) = opts.find.as_deref_mut() {
        for m in find.pattern.find_iter(&rich.text) {
            if find.seen == find.occurrence {
                found = Some((m.start(), m.end()));
            }
            find.seen += 1;
        }
    }
    let laidout = if let Some((s, e)) = found {
        let mut highlighted = rich.clone();
        highlighted.recolor(s, e, p.surface0);
        d.kit.layout_rich(cx, &highlighted.text, &highlighted.spans, Some(w), height)
    } else {
        prepared.laidout.clone()
    };
    let size = laidout_size(&laidout);
    if d.dry() {
        return size.y;
    }
    let pos = dvec2(x, y);
    for &(s, e) in &rich.codes {
        for r in range_rects(&laidout, s, e) {
            let r = r.translate(pos);
            d.kit.rect(cx, Rect { pos: r.pos - dvec2(3.0, 0.0), size: r.size + dvec2(6.0, 0.0) }, p.surface2, 4.0);
        }
    }
    if let Some((s, e)) = found {
        for r in range_rects(&laidout, s, e) {
            if !d.dry() { d.state.find_y.get_or_insert(r.pos.y + pos.y); }
            d.kit.rect(cx, r.translate(pos), p.warning, 2.0);
        }
    }
    d.kit.draw_rich(cx, pos, &laidout);
    for &(s, e) in &rich.strikes {
        for r in range_rects(&laidout, s, e) {
            let r = r.translate(pos);
            d.kit.rect(cx, rect_xywh(r.pos.x, r.pos.y + r.size.y * 0.5, r.size.x, 1.0), p.foreground, 0.0);
        }
    }
    for (s, e, href, kind) in &rich.links {
        let text = rich.text[*s..*e].to_owned();
        let hovered = d.state.hover_link.as_deref() == Some(href.as_str()) && !d.look.mobile;
        for r in range_rects(&laidout, *s, *e) {
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
    // Images break the paragraph into text / image / text segments.
    let mut cursor = y;
    let mut start = 0;
    let mut first = true;
    let flush = |cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, segment: &[Inline], cursor: &mut f64, first: &mut bool| {
        if segment.is_empty() {
            return;
        }
        let Some(prepared) = prepare_paragraph(cx, d, segment, w, para, height) else { return };
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

#[allow(clippy::too_many_arguments)]
fn draw_block(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, block: &Block, x: f64, y: f64, w: f64, color: Vec4, depth: usize, root: Option<Marker>) -> f64 {
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
        Block::List { start, items } => list(cx, d, opts, *start, items, x, y, w, color, depth, root),
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

fn marker_width(cx: &mut Cx2d, d: &mut Draw, kind: Marker, fs: f64) -> f64 {
    match kind {
        Marker::Bullet => 3.0,
        Marker::Task(_) => 14.0,
        Marker::Ordered => d.kit.measure(cx, Font::Medium, fs, 1.4, "99.", None).x,
    }
}

#[allow(clippy::too_many_arguments)]
fn list(cx: &mut Cx2d, d: &mut Draw, opts: &mut MdOpts, start: Option<u64>, items: &[ListItem], x: f64, y: f64, w: f64, color: Vec4, depth: usize, root: Option<Marker>) -> f64 {
    let p = d.look.palette;
    let fs = opts.font_size;
    let line = (fs * 1.4).round();
    let mut cursor = y;
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            cursor += 4.0;
        }
        let kind = match item.task {
            Some(checked) => Marker::Task(checked),
            None if start.is_some() => Marker::Ordered,
            None => Marker::Bullet,
        };
        let marker = marker_width(cx, d, kind, fs);
        let (column, anchor) = if depth == 0 {
            ((marker + 7.0).floor(), marker)
        } else {
            let root_kind = root.unwrap_or(kind);
            let root_margin = (marker_width(cx, d, root_kind, fs) + 7.0).floor();
            (20.0, 20.0 + marker - root_margin)
        };
        let center = cursor + line * 0.5;
        match kind {
            Marker::Ordered => {
                let label = format!("{}.", start.unwrap_or(1) + index as u64);
                let size = d.kit.measure(cx, Font::Medium, fs, 1.4, &label, None);
                d.kit.label(cx, Font::Medium, fs, 1.4, p.muted, dvec2(x + anchor - size.x, cursor), &label, None);
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
        let root_kind = if depth == 0 { Some(kind) } else { root };
        let mut item_y = cursor;
        for (b, block) in item.blocks.iter().enumerate() {
            if b > 0 {
                item_y += 4.0;
            }
            item_y += draw_block(cx, d, opts, block, x + column, item_y, (w - column).max(1.0), item_color, depth + 1, root_kind);
        }
        cursor = item_y.max(cursor + line);
    }
    cursor - y
}

fn cell_rich(d: &Draw, inlines: &[Inline], fs: f64, color: Vec4, bold: bool) -> Rich {
    let mut rich = Rich::default();
    build_inlines(&mut rich, inlines, d, &Para { size: fs, color, bold }, None);
    rich
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
    let mut widths = vec![84.0f64; columns];
    for (c, cell) in head.iter().enumerate() {
        let rich = cell_rich(d, cell, fs, color, true);
        let natural = d.kit.layout_rich(cx, &rich.text, &rich.spans, None, height);
        widths[c] = widths[c].max(laidout_size(&natural).x + 24.0);
    }
    for row in rows {
        for (c, cell) in row.iter().enumerate() {
            let rich = cell_rich(d, cell, fs, color, false);
            let natural = d.kit.layout_rich(cx, &rich.text, &rich.spans, None, height);
            widths[c] = widths[c].max(laidout_size(&natural).x + 24.0);
        }
    }
    for width in &mut widths {
        *width = width.min(324.0);
    }
    let total: f64 = widths.iter().sum();
    let key = opts.next_key("table");
    let max_scroll = (total - w).max(0.0);
    d.state.extent_x.insert(key.clone(), max_scroll);
    let scroll = d.state.scroll_x.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_scroll);
    let view = rect_xywh(x, y, w, 0.0);

    // Row heights.
    let mut row_heights = Vec::with_capacity(rows.len() + 1);
    let all_rows: Vec<(bool, &[Vec<Inline>])> = std::iter::once((true, head)).chain(rows.iter().map(|r| (false, r.as_slice()))).collect();
    for (bold, cells) in &all_rows {
        let mut h: f64 = 0.0;
        for (c, cell) in cells.iter().enumerate() {
            let rich = cell_rich(d, cell, fs, color, *bold);
            if rich.text.is_empty() {
                continue;
            }
            let laid = d.kit.layout_rich(cx, &rich.text, &rich.spans, Some(widths[c] - 24.0), height);
            h = h.max(laidout_size(&laid).y);
        }
        row_heights.push(h.max((fs * 1.4).round()) + 16.0);
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
                let rich = cell_rich(d, cell, fs, color, *bold);
                let laid_w = col_w - 24.0;
                let natural = d.kit.layout_rich(cx, &rich.text, &rich.spans, Some(laid_w), height);
                let size = laidout_size(&natural);
                let dx = match aligns.get(c) {
                    Some(Alignment::Center) => (laid_w - size.x) * 0.5,
                    Some(Alignment::Right) => laid_w - size.x,
                    _ => 0.0,
                };
                draw_rich(cx, d, opts, rich, cx_pos + 12.0 + dx.max(0.0), cy + 8.0, laid_w, height);
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
    let request = d.state.fence_highlights.entry(key.clone()).or_default();
    let colors = highlight::fence(request, text, lang, opts.syntax_theme, d.look.dark());
    let (code, mut spans) = code_rich(text, colors.as_ref(), p.foreground, code_size);
    let copy_text = text.trim_end_matches('\n').to_owned();
    let copied = d.state.copied_now(&key, d.now);
    if copied {
        d.animating = true;
    }
    // Find highlight inside the rendered code.
    let mut found = None;
    if let Some(find) = opts.find.as_deref_mut() {
        for m in find.pattern.find_iter(&code) {
            if find.seen == find.occurrence {
                found = Some((m.start(), m.end()));
            }
            find.seen += 1;
        }
    }
    if let Some((s, e)) = found {
        let mut rich = Rich { text: code.clone(), spans, ..Default::default() };
        rich.recolor(s, e, p.surface0);
        spans = rich.spans;
    }
    let mermaid = lang.trim().eq_ignore_ascii_case("mermaid");
    if d.look.mobile && !mermaid {
        // CodeBlockContainerView: bordered header (language, copy), divider,
        // horizontally scrolling non-wrapping pane at the paragraph line height.
        let inset = 13.0;
        let label = code_size * 0.85;
        let header = inset + (label * 1.171875).ceil();
        let divider = header + inset / 2.0 - 1.5;
        let line_height = (d.look.content() * 1.4).round() / code_size;
        let laid = d.kit.layout_rich(cx, &code, &spans, None, line_height);
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
        if let Some((s, e)) = found {
            for r in range_rects(&laid, s, e) {
                if !d.dry() { d.state.find_y.get_or_insert(r.pos.y + origin.y); }
                d.kit.rect(cx, r.translate(origin), p.warning, 2.0);
            }
        }
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
    let line_height = (code_size * 1.45).round() / code_size;
    let laid = d.kit.layout_rich(cx, &code, &spans, Some((w - 24.0).max(1.0)), line_height);
    let size = laidout_size(&laid);
    let h = size.y.max((code_size * 1.45).round()) + 24.0;
    d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface2, 8.0);
    let origin = dvec2(x + 12.0, y + 12.0);
    if let Some((s, e)) = found {
        for r in range_rects(&laid, s, e) {
            if !d.dry() { d.state.find_y.get_or_insert(r.pos.y + origin.y); }
            d.kit.rect(cx, r.translate(origin), p.warning, 2.0);
        }
    }
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
