//! Markdown preview of a file (Flutter `FilePanel._markdownPreview`: YAML front matter table
//! over `PaseoMarkdown`, links through `_link`, images through `_markdownImage`). Native
//! rendering on pulldown-cmark: headings, paragraphs with bold/italic/strike/code/link spans,
//! lists with task markers, block quotes, fenced code, tables, rules, HTML as source text and
//! images. Text is selectable and copyable; layout is cached per text/width/theme and only
//! visible items draw.

use std::collections::HashMap;
use std::rc::Rc;

use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::makepad_draw::text::selection::{Cursor as TextCursor, Selection as TextSelection};
use makepad_widgets::*;
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Options, Parser, Tag, TagEnd};
use serde_json::Value;

use crate::theme::{Palette, Typography, with_alpha};

use super::images::ImageSlot;
use super::kit::{Kit, RichFont, RichSpan, laidout_size, rect};

// ---------------------------------------------------------------------------------------------
// Front matter

/// Splits `^\uFEFF?---[ \t]*\r?\n(…)\r?\n---[ \t]*(\r?\n|$)` front matter into display rows
/// and the remaining body. Invalid or non-mapping metadata stays visible as source.
pub fn front_matter(text: &str) -> (Vec<(String, String)>, &str) {
    let body = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = body.strip_prefix("---") else { return (Vec::new(), text) };
    let rest = rest.trim_start_matches([' ', '\t']);
    let rest = match rest.strip_prefix("\r\n").or_else(|| rest.strip_prefix('\n')) {
        Some(rest) => rest,
        None => return (Vec::new(), text),
    };
    // The first `---` line after at least one metadata line closes the block (the regex's lazy
    // group must be followed by a line break and the marker).
    let mut offset = 0;
    let mut closing = None;
    for line in rest.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\n', '\r']).trim_end_matches([' ', '\t']);
        if stripped == "---" && offset > 0 {
            closing = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((yaml_end, body_start)) = closing else { return (Vec::new(), text) };
    let yaml = rest[..yaml_end].trim_end_matches(['\n', '\r']);
    let Ok(metadata) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else { return (Vec::new(), text) };
    let serde_yaml::Value::Mapping(map) = metadata else { return (Vec::new(), text) };
    let rows = map
        .iter()
        .map(|(key, value)| (yaml_scalar(key).unwrap_or_else(|| format_yaml(key)), format_yaml(value)))
        .collect();
    let consumed = text.len() - rest.len() + body_start;
    (rows, &text[consumed.min(text.len())..])
}

fn yaml_scalar(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => Some("null".to_owned()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Tagged(tagged) => yaml_scalar(&tagged.value),
        _ => None,
    }
}

fn format_yaml(value: &serde_yaml::Value) -> String {
    if let Some(scalar) = yaml_scalar(value) {
        return scalar;
    }
    if let serde_yaml::Value::Sequence(items) = value
        && let Some(parts) = items.iter().map(yaml_scalar).collect::<Option<Vec<_>>>() {
            return parts.join(", ");
        }
    let json: Value = serde_yaml::from_value(value.clone()).unwrap_or(Value::Null);
    serde_json::to_string_pretty(&json).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// Document model

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: Option<usize>,
}

#[derive(Clone, Debug, Default)]
struct Inline {
    text: String,
    spans: Vec<(usize, InlineStyle)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: InlineStyle) {
        if text.is_empty() {
            return;
        }
        match self.spans.last_mut() {
            Some((len, last)) if *last == style => *len += text.len(),
            _ => self.spans.push((text.len(), style)),
        }
        self.text.push_str(text);
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

#[derive(Clone, Debug)]
struct ListItem {
    task: Option<bool>,
    blocks: Vec<Block>,
}

#[derive(Clone, Debug)]
enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code(String),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<ListItem> },
    Rule,
    Table { head: Vec<Inline>, rows: Vec<Vec<Inline>> },
    Image { src: String, alt: String },
    Html(String),
}

enum Frame {
    Blocks(Vec<Block>),
    Quote(Vec<Block>),
    List { ordered: bool, start: u64, items: Vec<ListItem> },
    Item { task: Option<bool>, blocks: Vec<Block> },
    Table { head: Vec<Inline>, rows: Vec<Vec<Inline>>, row: Vec<Inline>, in_head: bool },
}

#[derive(Clone, Copy, PartialEq)]
enum InlineKind {
    Paragraph,
    Heading(u8),
    Cell,
}

struct Builder {
    frames: Vec<Frame>,
    inline: Option<(InlineKind, Inline)>,
    style: InlineStyle,
    links: Vec<String>,
    code: Option<String>,
    image: Option<(String, String)>,
    html: Option<String>,
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        match self.frames.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Quote(blocks) | Frame::Item { blocks, .. }) => blocks.push(block),
            _ => {
                if let Some(Frame::Blocks(blocks)) = self.frames.first_mut() {
                    blocks.push(block);
                }
            }
        }
    }

    fn inline(&mut self) -> &mut Inline {
        if self.inline.is_none() {
            self.inline = Some((InlineKind::Paragraph, Inline::default()));
        }
        &mut self.inline.as_mut().unwrap().1
    }

    fn flush_inline(&mut self) {
        if let Some((kind, inline)) = self.inline.take() {
            match kind {
                InlineKind::Paragraph if !inline.is_empty() => self.push_block(Block::Paragraph(inline)),
                InlineKind::Heading(level) => self.push_block(Block::Heading(level, inline)),
                InlineKind::Cell => {
                    if let Some(Frame::Table { row, .. }) = self.frames.last_mut() {
                        row.push(inline);
                    }
                }
                _ => {}
            }
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(code) = self.code.as_mut() {
            code.push_str(text);
            return;
        }
        if let Some((_, alt)) = self.image.as_mut() {
            alt.push_str(text);
            return;
        }
        if let Some(html) = self.html.as_mut() {
            html.push_str(text);
            return;
        }
        let style = self.style;
        self.inline().push(text, style);
    }
}

fn parse(body: &str) -> (Vec<Block>, Vec<String>) {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut b = Builder {
        frames: vec![Frame::Blocks(Vec::new())],
        inline: None,
        style: InlineStyle::default(),
        links: Vec::new(),
        code: None,
        image: None,
        html: None,
    };
    for event in Parser::new_ext(body, options) {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Paragraph => {
                    b.flush_inline();
                    b.inline = Some((InlineKind::Paragraph, Inline::default()));
                }
                Tag::Heading { level, .. } => {
                    b.flush_inline();
                    b.inline = Some((InlineKind::Heading(level as u8), Inline::default()));
                }
                Tag::BlockQuote(_) => {
                    b.flush_inline();
                    b.frames.push(Frame::Quote(Vec::new()));
                }
                Tag::CodeBlock(kind) => {
                    b.flush_inline();
                    let _ = matches!(kind, CodeBlockKind::Fenced(_));
                    b.code = Some(String::new());
                }
                Tag::HtmlBlock => {
                    b.flush_inline();
                    b.html = Some(String::new());
                }
                Tag::List(start) => {
                    b.flush_inline();
                    b.frames.push(Frame::List { ordered: start.is_some(), start: start.unwrap_or(1), items: Vec::new() });
                }
                Tag::Item => {
                    b.flush_inline();
                    b.frames.push(Frame::Item { task: None, blocks: Vec::new() });
                }
                Tag::Table(_) => {
                    b.flush_inline();
                    b.frames.push(Frame::Table { head: Vec::new(), rows: Vec::new(), row: Vec::new(), in_head: false });
                }
                Tag::TableHead => {
                    if let Some(Frame::Table { in_head, row, .. }) = b.frames.last_mut() {
                        *in_head = true;
                        row.clear();
                    }
                }
                Tag::TableRow => {
                    if let Some(Frame::Table { row, .. }) = b.frames.last_mut() {
                        row.clear();
                    }
                }
                Tag::TableCell => b.inline = Some((InlineKind::Cell, Inline::default())),
                Tag::Emphasis => b.style.italic = true,
                Tag::Strong => b.style.bold = true,
                Tag::Strikethrough => b.style.strike = true,
                Tag::Link { dest_url, .. } => {
                    b.links.push(dest_url.to_string());
                    b.style.link = Some(b.links.len() - 1);
                }
                Tag::Image { dest_url, .. } => {
                    // Images render on their own row: the text before them is a paragraph.
                    let kind = b.inline.as_ref().map(|(kind, _)| *kind);
                    if kind == Some(InlineKind::Paragraph) {
                        b.flush_inline();
                        b.inline = Some((InlineKind::Paragraph, Inline::default()));
                    }
                    b.image = Some((dest_url.to_string(), String::new()));
                }
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Paragraph => b.flush_inline(),
                TagEnd::Heading(_) => b.flush_inline(),
                TagEnd::BlockQuote(_) => {
                    b.flush_inline();
                    if let Some(Frame::Quote(blocks)) = b.frames.pop() {
                        b.push_block(Block::Quote(blocks));
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(code) = b.code.take() {
                        b.push_block(Block::Code(code.trim_end_matches('\n').to_owned()));
                    }
                }
                TagEnd::HtmlBlock => {
                    if let Some(html) = b.html.take() {
                        b.push_block(Block::Html(html.trim_end_matches('\n').to_owned()));
                    }
                }
                TagEnd::Item => {
                    b.flush_inline();
                    if let Some(Frame::Item { task, blocks }) = b.frames.pop()
                        && let Some(Frame::List { items, .. }) = b.frames.last_mut() {
                            items.push(ListItem { task, blocks });
                        }
                }
                TagEnd::List(_) => {
                    b.flush_inline();
                    if let Some(Frame::List { ordered, start, items }) = b.frames.pop() {
                        b.push_block(Block::List { ordered, start, items });
                    }
                }
                TagEnd::TableCell => b.flush_inline(),
                TagEnd::TableHead => {
                    if let Some(Frame::Table { head, row, in_head, .. }) = b.frames.last_mut() {
                        *head = std::mem::take(row);
                        *in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(Frame::Table { rows, row, .. }) = b.frames.last_mut() {
                        rows.push(std::mem::take(row));
                    }
                }
                TagEnd::Table => {
                    if let Some(Frame::Table { head, rows, .. }) = b.frames.pop() {
                        b.push_block(Block::Table { head, rows });
                    }
                }
                TagEnd::Emphasis => b.style.italic = false,
                TagEnd::Strong => b.style.bold = false,
                TagEnd::Strikethrough => b.style.strike = false,
                TagEnd::Link => b.style.link = None,
                TagEnd::Image => {
                    if let Some((src, alt)) = b.image.take() {
                        b.push_block(Block::Image { src, alt });
                    }
                }
                _ => {}
            },
            MdEvent::Text(text) => b.text(&text),
            MdEvent::Code(text) => {
                let mut style = b.style;
                style.code = true;
                b.inline().push(&text, style);
            }
            MdEvent::InlineHtml(html) | MdEvent::Html(html) => {
                if b.html.is_some() {
                    b.text(&html);
                } else {
                    let mut style = b.style;
                    style.code = true;
                    b.inline().push(&html, style);
                }
            }
            MdEvent::SoftBreak => b.text(" "),
            MdEvent::HardBreak => b.text("\n"),
            MdEvent::Rule => {
                b.flush_inline();
                b.push_block(Block::Rule);
            }
            MdEvent::TaskListMarker(checked) => {
                if let Some(Frame::Item { task, .. }) = b.frames.last_mut() {
                    *task = Some(checked);
                }
            }
            MdEvent::FootnoteReference(name) => b.text(&format!("[{name}]")),
            _ => {}
        }
    }
    b.flush_inline();
    while b.frames.len() > 1 {
        match b.frames.pop() {
            Some(Frame::Quote(blocks)) => b.push_block(Block::Quote(blocks)),
            Some(Frame::List { ordered, start, items }) => b.push_block(Block::List { ordered, start, items }),
            Some(Frame::Item { task, blocks }) => {
                if let Some(Frame::List { items, .. }) = b.frames.last_mut() {
                    items.push(ListItem { task, blocks });
                }
            }
            _ => {}
        }
    }
    let blocks = match b.frames.pop() {
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    };
    (blocks, b.links)
}

// ---------------------------------------------------------------------------------------------
// Layout

/// A laid-out selectable text run.
struct Run {
    laid: Rc<LaidoutText>,
    pos: DVec2,
    /// Link ranges (bytes) and their hrefs.
    links: Vec<(usize, usize, String)>,
}

enum Item {
    Fill { rect: Rect, color: Vec4, radius: f64 },
    Border { rect: Rect, color: Vec4, radius: f64 },
    Text(usize),
    Image { rect: Rect, key: String, alt: String },
}

/// What the owner tells the preview about an image source.
pub enum ImageState {
    Loading,
    Ready { texture: Texture, width: usize, height: usize },
    Failed(String),
}

pub struct MarkdownLook {
    pub palette: Palette,
    pub typography: Typography,
    pub compact_width: bool,
}

#[derive(Clone, Debug)]
pub enum PreviewEvent {
    None,
    Link(String),
}

pub struct MarkdownPreview {
    source: Option<String>,
    front: Vec<(String, String)>,
    blocks: Vec<Block>,
    links: Vec<String>,
    layout_key: Option<(u64, u64, u64, u64)>,
    runs: Vec<Run>,
    items: Vec<(f64, f64, Item)>,
    height: f64,
    pub scroll_y: f64,
    area: Rect,
    origin: DVec2,
    selection: Option<((usize, usize), (usize, usize))>,
    dragging: bool,
    thumb_drag: Option<(f64, f64)>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    /// Image sources in document order (relative to the file), for the owner to resolve.
    pub image_sources: Vec<String>,
}

impl Default for MarkdownPreview {
    fn default() -> Self {
        MarkdownPreview {
            source: None,
            front: Vec::new(),
            blocks: Vec::new(),
            links: Vec::new(),
            layout_key: None,
            runs: Vec::new(),
            items: Vec::new(),
            height: 0.0,
            scroll_y: 0.0,
            area: Rect::default(),
            origin: DVec2::default(),
            selection: None,
            dragging: false,
            thumb_drag: None,
            touch_scroll: Default::default(),
            image_sources: Vec::new(),
        }
    }
}

fn collect_images(blocks: &[Block], out: &mut Vec<String>) {
    for block in blocks {
        match block {
            Block::Image { src, .. } => out.push(src.clone()),
            Block::Quote(children) => collect_images(children, out),
            Block::List { items, .. } => items.iter().for_each(|item| collect_images(&item.blocks, out)),
            _ => {}
        }
    }
}

struct Ctx<'a> {
    kit: &'a mut Kit,
    look: &'a MarkdownLook,
    images: &'a dyn Fn(&str) -> ImageState,
    runs: Vec<Run>,
    items: Vec<(f64, f64, Item)>,
    links: &'a [String],
}

impl Ctx<'_> {
    fn base_size(&self) -> f64 {
        self.look.typography.content_font_size
    }

    #[allow(clippy::too_many_arguments)] // independent text layout inputs
    fn rich(&mut self, cx: &mut Cx, inline: &Inline, size: f64, bold: bool, color: Vec4, width: f64, pos: DVec2) -> f64 {
        let p = &self.look.palette;
        let code_size = (size - 1.0).max(9.0);
        let mut spans = Vec::new();
        let mut links = Vec::new();
        let mut code_ranges = Vec::new();
        let mut offset = 0;
        for (len, style) in &inline.spans {
            let font = if style.code {
                RichFont::Mono
            } else {
                match (bold || style.bold, style.italic) {
                    (true, true) => RichFont::BoldItalic,
                    (true, false) => RichFont::Bold,
                    (false, true) => RichFont::Italic,
                    (false, false) => RichFont::Regular,
                }
            };
            let span_color = if style.link.is_some() { p.accent_bright } else if style.strike { p.muted } else { color };
            spans.push(RichSpan { len: *len, font, size: if style.code { code_size } else { size }, color: span_color });
            if let Some(link) = style.link {
                links.push((offset, offset + len, self.links.get(link).cloned().unwrap_or_default()));
            }
            if style.code {
                code_ranges.push((offset, offset + len));
            }
            offset += len;
        }
        if spans.is_empty() {
            return 0.0;
        }
        let laid = self.kit.layout_rich(cx, &inline.text, &spans, Some(width.max(1.0)), 1.15);
        let size_px = laidout_size(&laid);
        for (a, b) in code_ranges {
            for r in range_rects(&laid, a, b) {
                let fill = rect(pos.x + r.pos.x - 2.0, pos.y + r.pos.y, r.size.x + 4.0, r.size.y);
                self.items.push((fill.pos.y, fill.size.y, Item::Fill { rect: fill, color: p.surface2, radius: 4.0 }));
            }
        }
        let index = self.runs.len();
        self.runs.push(Run { laid, pos, links });
        self.items.push((pos.y, size_px.y, Item::Text(index)));
        size_px.y
    }

    #[allow(clippy::too_many_arguments)] // independent text layout inputs
    fn plain(&mut self, cx: &mut Cx, text: &str, font: RichFont, size: f64, color: Vec4, width: Option<f64>, pos: DVec2) -> DVec2 {
        let spans = [RichSpan { len: text.len(), font, size, color }];
        let laid = self.kit.layout_rich(cx, text, &spans, width.map(|w| w.max(1.0)), 1.15);
        let size_px = laidout_size(&laid);
        let index = self.runs.len();
        self.runs.push(Run { laid, pos, links: Vec::new() });
        self.items.push((pos.y, size_px.y, Item::Text(index)));
        size_px
    }

    fn fill(&mut self, r: Rect, color: Vec4, radius: f64) {
        self.items.push((r.pos.y, r.size.y, Item::Fill { rect: r, color, radius }));
    }

    fn border(&mut self, r: Rect, color: Vec4, radius: f64) {
        self.items.push((r.pos.y, r.size.y, Item::Border { rect: r, color, radius }));
    }

    /// Lays out `blocks` from `y` at `x` within `width`; returns the new y.
    fn blocks(&mut self, cx: &mut Cx, blocks: &[Block], x: f64, mut y: f64, width: f64) -> f64 {
        let p = self.look.palette;
        let size = self.base_size();
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                y += size * 0.75;
            }
            match block {
                Block::Paragraph(inline) => y += self.rich(cx, inline, size, false, p.foreground, width, dvec2(x, y)),
                Block::Heading(level, inline) => {
                    let scale = match level {
                        1 => 1.6,
                        2 => 1.35,
                        3 => 1.17,
                        _ => 1.0,
                    };
                    if index > 0 {
                        y += size * 0.5;
                    }
                    y += self.rich(cx, inline, (size * scale).round(), true, p.foreground, width, dvec2(x, y));
                    if *level <= 2 {
                        y += 6.0;
                        self.fill(rect(x, y, width, 1.0), p.border, 0.0);
                        y += 1.0;
                    }
                }
                Block::Code(code) => {
                    let code_size = self.look.typography.code_font_size;
                    let measured = self.kit.layout_rich(
                        cx,
                        code,
                        &[RichSpan { len: code.len(), font: RichFont::Mono, size: code_size, color: p.foreground }],
                        Some((width - 24.0).max(1.0)),
                        1.2,
                    );
                    let h = laidout_size(&measured).y + 24.0;
                    self.fill(rect(x, y, width, h), p.surface2, 6.0);
                    self.plain(cx, code, RichFont::Mono, code_size, p.foreground, Some(width - 24.0), dvec2(x + 12.0, y + 12.0));
                    y += h;
                }
                Block::Html(html) => {
                    let code_size = self.look.typography.code_font_size;
                    y += self.plain(cx, html, RichFont::Mono, code_size, p.muted, Some(width), dvec2(x, y)).y;
                }
                Block::Quote(children) => {
                    let start = y;
                    let end = self.blocks(cx, children, x + 14.0, y, width - 14.0);
                    self.fill(rect(x, start, 3.0, (end - start).max(size)), p.border, 1.5);
                    y = end;
                }
                Block::List { ordered, start, items } => {
                    for (number, item) in items.iter().enumerate() {
                        if number > 0 {
                            y += size * 0.25;
                        }
                        let marker = match item.task {
                            Some(true) => "☑".to_owned(),
                            Some(false) => "☐".to_owned(),
                            None if *ordered => format!("{}.", *start as usize + number),
                            None => "•".to_owned(),
                        };
                        let marker_w = if *ordered { 24.0 } else { 18.0 };
                        self.plain(cx, &marker, RichFont::Regular, size, p.muted, None, dvec2(x, y));
                        let end = self.blocks(cx, &item.blocks, x + marker_w, y, width - marker_w);
                        y = end.max(y + size * 1.15);
                    }
                }
                Block::Rule => {
                    y += size * 0.5;
                    self.fill(rect(x, y, width, 1.0), p.border, 0.0);
                    y += 1.0 + size * 0.5;
                }
                Block::Table { head, rows } => {
                    let columns = head.len().max(rows.iter().map(Vec::len).max().unwrap_or(0)).max(1);
                    let col_w = width / columns as f64;
                    let top = y;
                    let mut all: Vec<(bool, &Vec<Inline>)> = vec![(true, head)];
                    all.extend(rows.iter().map(|row| (false, row)));
                    for (is_head, row) in all {
                        let mut row_h: f64 = size * 1.15;
                        for (column, cell) in row.iter().enumerate() {
                            let h = self.rich(cx, cell, size, is_head, p.foreground, col_w - 16.0, dvec2(x + column as f64 * col_w + 8.0, y + 6.0));
                            row_h = row_h.max(h);
                        }
                        if is_head {
                            self.fill(rect(x, y, width, row_h + 12.0), p.surface2, 0.0);
                        }
                        y += row_h + 12.0;
                        self.fill(rect(x, y - 1.0, width, 1.0), p.border, 0.0);
                    }
                    self.border(rect(x, top, width, y - top), p.border, 0.0);
                }
                Block::Image { src, alt } => {
                    let state = (self.images)(src);
                    let r = match &state {
                        ImageState::Ready { width: w, height: h, .. } => {
                            let w = *w as f64;
                            let h = *h as f64;
                            let scale = (width / w).min(1.0);
                            rect(x, y, w * scale, h * scale)
                        }
                        ImageState::Loading => rect(x, y, 16.0, 16.0),
                        ImageState::Failed(error) => {
                            // Flutter `errorBuilder`: `Text(alt ?? error.toString())`.
                            let label = if alt.is_empty() { error.as_str() } else { alt.as_str() };
                            let measured = self.plain(cx, label, RichFont::Regular, size, p.muted, Some(width), dvec2(x, y));
                            y += measured.y;
                            continue;
                        }
                    };
                    self.items.push((r.pos.y, r.size.y, Item::Image { rect: r, key: src.clone(), alt: alt.clone() }));
                    y += r.size.y;
                }
            }
        }
        y
    }
}

/// Rects of `a..b` in a laid-out text (relative to its origin).
pub fn range_rects(laid: &LaidoutText, a: usize, b: usize) -> Vec<Rect> {
    let a = a.min(laid.text.len());
    let b = b.min(laid.text.len());
    if a >= b {
        return Vec::new();
    }
    laid.selection_rects_in_lpxs(TextSelection {
        anchor: TextCursor { index: a, prefer_next_row: false },
        cursor: TextCursor { index: b, prefer_next_row: false },
    })
    .into_iter()
    .map(|r| rect(r.origin.x as f64, r.origin.y as f64, r.size.width as f64, r.size.height as f64))
    .collect()
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

impl MarkdownPreview {
    /// Re-parses when the text changed.
    pub fn set_text(&mut self, text: &str) {
        if self.source.as_deref() == Some(text) {
            return;
        }
        let (front, body) = front_matter(text);
        let (blocks, links) = parse(body);
        self.front = front;
        self.blocks = blocks;
        self.links = links;
        self.image_sources.clear();
        collect_images(&self.blocks, &mut self.image_sources);
        self.source = Some(text.to_owned());
        self.layout_key = None;
        self.selection = None;
    }

    fn layout(&mut self, cx: &mut Cx, kit: &mut Kit, look: &MarkdownLook, width: f64, images: &dyn Fn(&str) -> ImageState, image_epoch: u64) {
        let key = (
            hash_text(self.source.as_deref().unwrap_or("")),
            width.to_bits(),
            crate::theme::epoch(),
            image_epoch,
        );
        if self.layout_key == Some(key) {
            return;
        }
        self.layout_key = Some(key);
        let p = look.palette;
        let t = look.typography;
        let outer = if look.compact_width { 12.0 } else { 16.0 };
        let frame = (width - outer * 2.0).clamp(0.0, t.content_max_width);
        let x = ((width - frame) / 2.0).max(0.0) + 8.0;
        let inner = (frame - 16.0).max(1.0);
        let mut ctx = Ctx { kit, look, images, runs: Vec::new(), items: Vec::new(), links: &self.links };
        let mut y = 16.0;
        if !self.front.is_empty() {
            let key_w = (inner * 0.3).clamp(0.0, 180.0);
            let base = t.ui_base_font_size;
            let top = y;
            for (index, (key, value)) in self.front.clone().iter().enumerate() {
                let key_h = ctx
                    .kit
                    .layout_rich(cx, key, &[RichSpan { len: key.len(), font: RichFont::Mono, size: base, color: p.muted }], Some(key_w - 24.0), 20.0 / base / 1.15)
                    .size_in_lpxs
                    .height as f64;
                let value_h = ctx
                    .kit
                    .layout_rich(cx, value, &[RichSpan { len: value.len(), font: RichFont::Regular, size: base, color: p.foreground }], Some(inner - key_w - 24.0), 20.0 / base / 1.15)
                    .size_in_lpxs
                    .height as f64;
                let row_h = key_h.max(value_h) + 16.0;
                if index > 0 {
                    ctx.fill(rect(x, y, inner, 1.0), p.border, 0.0);
                }
                ctx.fill(rect(x, y, key_w, row_h), p.surface2, 0.0);
                ctx.plain(cx, key, RichFont::Mono, base, p.muted, Some(key_w - 24.0), dvec2(x + 12.0, y + 8.0));
                ctx.plain(cx, value, RichFont::Regular, base, p.foreground, Some(inner - key_w - 24.0), dvec2(x + key_w + 12.0, y + 8.0));
                y += row_h;
            }
            ctx.border(rect(x, top, inner, y - top), p.border, 8.0);
            y += 24.0;
        }
        let blocks = std::mem::take(&mut self.blocks);
        y = ctx.blocks(cx, &blocks, x, y, inner);
        self.blocks = blocks;
        self.runs = ctx.runs;
        self.items = ctx.items;
        self.height = y + 16.0;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        cx: &mut Cx2d,
        kit: &mut Kit,
        area: Rect,
        look: &MarkdownLook,
        images: &dyn Fn(&str) -> ImageState,
        image_epoch: u64,
    ) {
        self.layout(cx, kit, look, area.size.x, images, image_epoch);
        self.area = area;
        let max = (self.height - area.size.y).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max);
        self.origin = dvec2(area.pos.x, area.pos.y - self.scroll_y);
        let p = look.palette;
        cx.begin_turtle(Walk::abs_rect(area), Layout::default());
        let (top, bottom) = (self.scroll_y, self.scroll_y + area.size.y);
        // Selection highlights under their text.
        let selection = self.ordered_selection();
        for (y, h, item) in &self.items {
            if y + h < top || *y > bottom {
                continue;
            }
            match item {
                Item::Fill { rect: r, color, radius } => kit.fill(cx, r.translate(self.origin), *color, *radius),
                Item::Border { rect: r, color, radius } => {
                    kit.outline(cx, r.translate(self.origin), Vec4::default(), *color, 1.0, *radius)
                }
                Item::Text(index) => {
                    let run = &self.runs[*index];
                    if let Some(((sa, sb), (ea, eb))) = selection
                        && *index >= sa && *index <= ea {
                            let a = if *index == sa { sb } else { 0 };
                            let b = if *index == ea { eb } else { run.laid.text.len() };
                            for r in range_rects(&run.laid, a, b) {
                                kit.fill(cx, r.translate(run.pos + self.origin), with_alpha(p.accent, 0.35), 0.0);
                            }
                        }
                    kit.draw_rich(cx, run.pos + self.origin, &run.laid);
                }
                Item::Image { rect: r, key, alt } => match images(key) {
                    ImageState::Ready { texture, .. } => kit.texture(cx, r.translate(self.origin), &texture, 0.0, 1.0),
                    ImageState::Loading => kit.spinner(cx, r.translate(self.origin), p.muted, 0.0),
                    ImageState::Failed(_) => {
                        let _ = alt;
                    }
                },
            }
        }
        if self.height > area.size.y + 1.0 {
            let thumb_h = (area.size.y * area.size.y / self.height).max(24.0);
            let thumb_y = area.pos.y + (area.size.y - thumb_h) * (self.scroll_y / max).clamp(0.0, 1.0);
            kit.fill(cx, rect(area.pos.x + area.size.x - 6.0, thumb_y, 4.0, thumb_h), with_alpha(p.muted, 0.35), 2.0);
        }
        cx.end_turtle();
    }

    fn ordered_selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a, b) = self.selection?;
        if a == b {
            return None;
        }
        Some(if a <= b { (a, b) } else { (b, a) })
    }

    /// (run, byte) under a window point, nearest run by vertical distance.
    fn point_at(&self, abs: DVec2) -> Option<(usize, usize)> {
        let rel = abs - self.origin;
        let mut best: Option<(f64, usize)> = None;
        for (index, run) in self.runs.iter().enumerate() {
            let size = laidout_size(&run.laid);
            let dy = if rel.y < run.pos.y { run.pos.y - rel.y } else if rel.y > run.pos.y + size.y { rel.y - run.pos.y - size.y } else { 0.0 };
            let dx = if rel.x < run.pos.x { run.pos.x - rel.x } else if rel.x > run.pos.x + size.x { rel.x - run.pos.x - size.x } else { 0.0 };
            let distance = dy * 1000.0 + dx;
            if best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, index));
            }
        }
        let (_, index) = best?;
        let run = &self.runs[index];
        let local = rel - run.pos;
        let cursor = run.laid.point_in_lpxs_to_cursor(makepad_widgets::makepad_draw::text::geom::Point::new(local.x as f32, local.y as f32));
        Some((index, cursor.index))
    }

    fn link_at(&self, abs: DVec2) -> Option<String> {
        let rel = abs - self.origin;
        for run in &self.runs {
            for (a, b, href) in &run.links {
                for r in range_rects(&run.laid, *a, *b) {
                    if r.translate(run.pos).contains(rel) {
                        return Some(href.clone());
                    }
                }
            }
        }
        None
    }

    pub fn is_link(&self, abs: DVec2) -> bool {
        self.link_at(abs).is_some()
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((sa, sb), (ea, eb)) = self.ordered_selection()?;
        let mut out = String::new();
        for index in sa..=ea {
            let text = self.runs.get(index)?.laid.text.as_str();
            let a = if index == sa { sb } else { 0 };
            let b = if index == ea { eb } else { text.len() };
            if index > sa {
                out.push('\n');
            }
            out.push_str(&text[a.min(text.len())..b.min(text.len())]);
        }
        Some(out)
    }

    pub fn animate_scroll(&mut self, cx: &mut Cx, event: &Event, enabled: bool) -> bool {
        if !enabled {
            self.touch_scroll.reset();
            return false;
        }
        let Some(delta) = self.touch_scroll.frame(cx, event) else { return false };
        self.touch_scroll.apply_delta(&mut self.scroll_y, delta, self.height - self.area.size.y)
    }

    pub fn handle_hit(&mut self, hit: &Hit) -> PreviewEvent {
        match hit {
            Hit::FingerDown(fe) if fe.device.is_primary_hit() => {
                self.touch_scroll.reset();
                let max = (self.height - self.area.size.y).max(0.0);
                if max > 0.0 && fe.abs.x > self.area.pos.x + self.area.size.x - 10.0 {
                    self.thumb_drag = Some((fe.abs.y, self.scroll_y));
                    return PreviewEvent::None;
                }
                self.touch_scroll.handle_hit(hit);
                self.dragging = !fe.device.is_touch();
                let point = self.point_at(fe.abs);
                self.selection = point.map(|p| (p, p));
                if fe.tap_count == 2
                    && let Some((run, offset)) = point {
                        let text = self.runs[run].laid.text.as_str();
                        let (a, b) = word_bounds(text, offset);
                        self.selection = Some(((run, a), (run, b)));
                    }
            }
            Hit::FingerLongPress(fe) => {
                self.touch_scroll.reset();
                if let Some((run, offset)) = self.point_at(fe.abs) {
                    let (a, b) = word_bounds(self.runs[run].laid.text.as_str(), offset);
                    self.selection = Some(((run, a), (run, b)));
                    self.dragging = true;
                }
            }
            Hit::FingerMove(fe) => {
                if let Some((start_y, start_scroll)) = self.thumb_drag {
                    let ratio = self.height / self.area.size.y.max(1.0);
                    self.scroll_y = start_scroll + (fe.abs.y - start_y) * ratio;
                    return PreviewEvent::None;
                }
                if let Some(dy) = self.touch_scroll.handle_hit(hit) {
                    self.touch_scroll.apply_delta(&mut self.scroll_y, dy, self.height - self.area.size.y);
                    return PreviewEvent::None;
                }
                if self.dragging {
                    if fe.abs.y < self.area.pos.y {
                        self.scroll_y -= 20.0;
                    } else if fe.abs.y > self.area.pos.y + self.area.size.y {
                        self.scroll_y += 20.0;
                    }
                    if let (Some((anchor, _)), Some(point)) = (self.selection, self.point_at(fe.abs)) {
                        self.selection = Some((anchor, point));
                    }
                }
            }
            Hit::FingerUp(fe) => {
                self.touch_scroll.release(fe.time);
                self.thumb_drag = None;
                let was_drag = self.dragging;
                self.dragging = false;
                if (was_drag || fe.device.is_touch()) && fe.is_over && fe.was_tap() && self.ordered_selection().is_none()
                    && let Some(href) = self.link_at(fe.abs) {
                        return PreviewEvent::Link(href);
                    }
            }
            Hit::FingerScroll(fe) => {
                self.touch_scroll.reset();
                self.scroll_y += fe.scroll.y;
            }
            Hit::TextCopy(ce) => {
                if let Some(text) = self.selected_text() {
                    *ce.response.borrow_mut() = Some(text);
                }
            }
            Hit::KeyDown(ke) if ke.key_code == KeyCode::KeyA && ke.modifiers.is_primary() => {
                if let Some(last) = self.runs.len().checked_sub(1) {
                    self.selection = Some(((0, 0), (last, self.runs[last].laid.text.len())));
                }
            }
            _ => {}
        }
        PreviewEvent::None
    }

    pub fn image_keys(&self) -> &[String] {
        &self.image_sources
    }
}

fn word_bounds(text: &str, offset: usize) -> (usize, usize) {
    use unicode_segmentation::UnicodeSegmentation;
    for (index, word) in text.split_word_bound_indices() {
        if offset >= index && offset < index + word.len() {
            return (index, index + word.len());
        }
    }
    (offset, offset)
}

/// Per-panel cache of resolved image states by source (keeps `HashMap` lookups off `Kit`).
pub type ImageCache = HashMap<String, ImageSlot>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn front_matter_table_and_body() {
        let (rows, body) = front_matter("---\ntitle: Hello\ntags: [a, b]\nnested: {x: 1}\n---\n# Body\n");
        assert_eq!(rows[0], ("title".to_owned(), "Hello".to_owned()));
        assert_eq!(rows[1], ("tags".to_owned(), "a, b".to_owned()));
        assert!(rows[2].1.contains("\"x\": 1"));
        assert_eq!(body, "# Body\n");
        let (rows, body) = front_matter("---\n: [\n---\nx");
        assert!(rows.is_empty());
        assert_eq!(body, "---\n: [\n---\nx");
        let (rows, _) = front_matter("no front matter");
        assert!(rows.is_empty());
    }

    #[test]
    fn parses_blocks_and_images() {
        let (blocks, links) = parse("# T\n\ntext [l](a.md) ![alt](img.png) after\n\n- [x] done\n- b\n\n```\ncode\n```\n");
        assert!(matches!(blocks[0], Block::Heading(1, _)));
        assert!(matches!(blocks[1], Block::Paragraph(_)));
        assert!(matches!(&blocks[2], Block::Image { src, alt } if src == "img.png" && alt == "alt"));
        assert!(matches!(blocks[3], Block::Paragraph(_)));
        assert!(matches!(&blocks[4], Block::List { items, .. } if items[0].task == Some(true)));
        assert!(matches!(&blocks[5], Block::Code(code) if code == "code"));
        assert_eq!(links, vec!["a.md".to_owned()]);
    }
}
