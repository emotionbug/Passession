//! Markdown source → native block model (chat_markdown.dart PaseoMarkdown):
//! GitHub-flavored Markdown without footnotes, GitHub alerts, linkified
//! http(s) URLs and file paths in prose, inline-code file links. Rendering is
//! `widget.rs`; this module never touches the GPU.

use super::super::links::{FileLinkSourceType, is_inline_code_file_link, linkify_ranges};
use super::html::normalize_markdown_html;
use pulldown_cmark::{Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Inline {
    Text { text: String, style: Style },
    Code { text: String, style: Style },
    Link { href: String, kind: FileLinkSourceType, children: Vec<Inline> },
    Image { url: String, alt: String },
    /// Hard or soft line break (softLineBreak: true renders soft breaks as newlines).
    Break,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ListItem {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertKind {
    Note,
    Tip,
    Important,
    Warning,
    Caution,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading { level: u8, inlines: Vec<Inline> },
    List { start: Option<u64>, items: Vec<ListItem> },
    Quote(Vec<Block>),
    Alert { kind: AlertKind, blocks: Vec<Block> },
    Code { lang: String, text: String },
    Table { aligns: Vec<Alignment>, head: Vec<Vec<Inline>>, rows: Vec<Vec<Vec<Inline>>> },
    Rule,
}

#[derive(Default)]
struct Builder {
    /// Open containers, innermost last.
    stack: Vec<Frame>,
    style: Style,
    /// Open links: (href, kind, children collected so far).
    links: Vec<(String, FileLinkSourceType, Vec<Inline>)>,
    image: Option<(String, String)>,
    workspace_root: Option<String>,
}

enum Frame {
    Root(Vec<Block>),
    Paragraph(Vec<Inline>),
    Heading(u8, Vec<Inline>),
    Quote(Option<AlertKind>, Vec<Block>),
    List(Option<u64>, Vec<ListItem>),
    Item(Option<bool>, Vec<Block>, Vec<Inline>),
    Code(String, String),
    Table(Vec<Alignment>, Vec<Vec<Inline>>, Vec<Vec<Vec<Inline>>>, bool),
    Row(Vec<Vec<Inline>>),
    Cell(Vec<Inline>),
}

impl Builder {
    fn push_block(&mut self, block: Block) {
        // A list item's loose text becomes its own paragraph first.
        if let Some(Frame::Item(_, blocks, inlines)) = self.stack.last_mut() {
            if !inlines.is_empty() {
                blocks.push(Block::Paragraph(std::mem::take(inlines)));
            }
            blocks.push(block);
            return;
        }
        match self.stack.last_mut() {
            Some(Frame::Root(blocks)) | Some(Frame::Quote(_, blocks)) => blocks.push(block),
            _ => {}
        }
    }

    fn push_inline(&mut self, inline: Inline) {
        if let Some(image) = &mut self.image {
            // Image alt text collects its text children.
            if let Inline::Text { text, .. } | Inline::Code { text, .. } = &inline {
                image.1.push_str(text);
            }
            return;
        }
        if let Some((_, _, children)) = self.links.last_mut() {
            children.push(inline);
            return;
        }
        match self.stack.last_mut() {
            Some(Frame::Paragraph(inlines)) | Some(Frame::Heading(_, inlines)) | Some(Frame::Cell(inlines)) => inlines.push(inline),
            Some(Frame::Item(_, _, inlines)) => inlines.push(inline),
            _ => {
                // Tight content outside any paragraph: open one implicitly.
                self.stack.push(Frame::Paragraph(vec![inline]));
                self.close_paragraph();
            }
        }
    }

    fn close_paragraph(&mut self) {
        if let Some(Frame::Paragraph(inlines)) = self.stack.pop() {
            let inlines = linkify(inlines, self.workspace_root.as_deref());
            if !inlines.is_empty() {
                self.push_block(Block::Paragraph(inlines));
            }
        }
    }

    fn text(&mut self, text: &str) {
        if let Some(Frame::Code(_, code)) = self.stack.last_mut() {
            code.push_str(text);
            return;
        }
        self.push_inline(Inline::Text { text: text.to_owned(), style: self.style });
    }

    fn code(&mut self, code: &str) {
        let inline = Inline::Code { text: code.to_owned(), style: self.style };
        if self.links.is_empty() && self.image.is_none() && is_inline_code_file_link(code, self.workspace_root.as_deref()) {
            self.push_inline(Inline::Link { href: code.to_owned(), kind: FileLinkSourceType::InlineCode, children: vec![inline] });
        } else {
            self.push_inline(inline);
        }
    }
}

fn alert_kind(kind: BlockQuoteKind) -> AlertKind {
    match kind {
        BlockQuoteKind::Note => AlertKind::Note,
        BlockQuoteKind::Tip => AlertKind::Tip,
        BlockQuoteKind::Important => AlertKind::Important,
        BlockQuoteKind::Warning => AlertKind::Warning,
        BlockQuoteKind::Caution => AlertKind::Caution,
    }
}

/// `_LinkifySyntax`: prose text outside links and code gains recognized
/// http(s) URL and file path links.
fn linkify(inlines: Vec<Inline>, _workspace_root: Option<&str>) -> Vec<Inline> {
    let mut out = Vec::with_capacity(inlines.len());
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                let ranges = linkify_ranges(&text);
                if ranges.is_empty() {
                    out.push(Inline::Text { text, style });
                    continue;
                }
                let mut cursor = 0;
                for (start, end) in ranges {
                    if start > cursor {
                        out.push(Inline::Text { text: text[cursor..start].to_owned(), style });
                    }
                    let token = text[start..end].to_owned();
                    out.push(Inline::Link {
                        href: token.clone(),
                        kind: FileLinkSourceType::Linkify,
                        children: vec![Inline::Text { text: token, style }],
                    });
                    cursor = end;
                }
                if cursor < text.len() {
                    out.push(Inline::Text { text: text[cursor..].to_owned(), style });
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Parses assistant/plan Markdown. `linkify_files`: chat messages recognize
/// file links (PaseoMarkdown with onTapFileLink); other surfaces only follow
/// authored links.
pub fn parse(source: &str, workspace_root: Option<&str>, linkify_files: bool) -> Vec<Block> {
    let normalized = normalize_markdown_html(source);
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_GFM);
    let mut b = Builder {
        stack: vec![Frame::Root(Vec::new())],
        workspace_root: workspace_root.map(str::to_owned),
        ..Default::default()
    };
    for event in Parser::new_ext(&normalized, options) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => b.stack.push(Frame::Paragraph(Vec::new())),
                Tag::Heading { level, .. } => {
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        HeadingLevel::H4 => 4,
                        HeadingLevel::H5 => 5,
                        HeadingLevel::H6 => 6,
                    };
                    b.stack.push(Frame::Heading(level, Vec::new()));
                }
                Tag::BlockQuote(kind) => b.stack.push(Frame::Quote(kind.map(alert_kind), Vec::new())),
                Tag::CodeBlock(kind) => {
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_owned(),
                        CodeBlockKind::Indented => String::new(),
                    };
                    b.stack.push(Frame::Code(lang, String::new()));
                }
                Tag::List(start) => b.stack.push(Frame::List(start, Vec::new())),
                Tag::Item => b.stack.push(Frame::Item(None, Vec::new(), Vec::new())),
                Tag::Table(aligns) => b.stack.push(Frame::Table(aligns, Vec::new(), Vec::new(), false)),
                Tag::TableHead => {
                    if let Some(Frame::Table(_, _, _, in_head)) = b.stack.last_mut() {
                        *in_head = true;
                    }
                    b.stack.push(Frame::Row(Vec::new()));
                }
                Tag::TableRow => b.stack.push(Frame::Row(Vec::new())),
                Tag::TableCell => b.stack.push(Frame::Cell(Vec::new())),
                Tag::Emphasis => b.style.italic = true,
                Tag::Strong => b.style.bold = true,
                Tag::Strikethrough => b.style.strike = true,
                Tag::Link { dest_url, .. } => {
                    let kind = FileLinkSourceType::Markdown;
                    b.links.push((dest_url.to_string(), kind, Vec::new()));
                }
                Tag::Image { dest_url, .. } => b.image = Some((dest_url.to_string(), String::new())),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if linkify_files {
                        b.close_paragraph();
                    } else if let Some(Frame::Paragraph(inlines)) = b.stack.pop()
                        && !inlines.is_empty()
                    {
                        b.push_block(Block::Paragraph(inlines));
                    }
                }
                TagEnd::Heading(_) => {
                    if let Some(Frame::Heading(level, inlines)) = b.stack.pop() {
                        let inlines = if linkify_files { linkify(inlines, workspace_root) } else { inlines };
                        b.push_block(Block::Heading { level, inlines });
                    }
                }
                TagEnd::BlockQuote(_) => {
                    if let Some(Frame::Quote(kind, blocks)) = b.stack.pop() {
                        b.push_block(match kind {
                            Some(kind) => Block::Alert { kind, blocks },
                            None => Block::Quote(blocks),
                        });
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(Frame::Code(lang, mut text)) = b.stack.pop() {
                        // MarkdownCodeBlock drops the fence's final newline.
                        if text.ends_with('\n') {
                            text.pop();
                        }
                        b.push_block(Block::Code { lang, text });
                    }
                }
                TagEnd::List(_) => {
                    if let Some(Frame::List(start, items)) = b.stack.pop() {
                        b.push_block(Block::List { start, items });
                    }
                }
                TagEnd::Item => {
                    if let Some(Frame::Item(task, mut blocks, inlines)) = b.stack.pop() {
                        if !inlines.is_empty() {
                            let inlines = if linkify_files { linkify(inlines, workspace_root) } else { inlines };
                            blocks.push(Block::Paragraph(inlines));
                        }
                        if let Some(Frame::List(_, items)) = b.stack.last_mut() {
                            items.push(ListItem { task, blocks });
                        }
                    }
                }
                TagEnd::TableHead => {
                    if let Some(Frame::Row(cells)) = b.stack.pop()
                        && let Some(Frame::Table(_, head, _, in_head)) = b.stack.last_mut()
                    {
                        *head = cells;
                        *in_head = false;
                    }
                }
                TagEnd::TableRow => {
                    if let Some(Frame::Row(cells)) = b.stack.pop()
                        && let Some(Frame::Table(_, _, rows, _)) = b.stack.last_mut()
                    {
                        rows.push(cells);
                    }
                }
                TagEnd::TableCell => {
                    if let Some(Frame::Cell(inlines)) = b.stack.pop() {
                        let inlines = if linkify_files { linkify(inlines, workspace_root) } else { inlines };
                        if let Some(Frame::Row(cells)) = b.stack.last_mut() {
                            cells.push(inlines);
                        }
                    }
                }
                TagEnd::Table => {
                    if let Some(Frame::Table(aligns, head, rows, _)) = b.stack.pop() {
                        b.push_block(Block::Table { aligns, head, rows });
                    }
                }
                TagEnd::Emphasis => b.style.italic = false,
                TagEnd::Strong => b.style.bold = false,
                TagEnd::Strikethrough => b.style.strike = false,
                TagEnd::Link => {
                    if let Some((href, kind, children)) = b.links.pop() {
                        b.push_inline(Inline::Link { href, kind, children });
                    }
                }
                TagEnd::Image => {
                    if let Some((url, alt)) = b.image.take() {
                        b.push_inline(Inline::Image { url, alt });
                    }
                }
                _ => {}
            },
            Event::Text(text) => b.text(&text),
            Event::Code(code) => {
                if linkify_files {
                    b.code(&code);
                } else {
                    b.push_inline(Inline::Code { text: code.to_string(), style: b.style });
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => b.text(&html),
            Event::FootnoteReference(label) => b.text(&format!("[^{label}]")),
            Event::SoftBreak | Event::HardBreak => {
                if let Some(Frame::Code(_, code)) = b.stack.last_mut() {
                    code.push('\n');
                } else {
                    b.push_inline(Inline::Break);
                }
            }
            Event::Rule => b.push_block(Block::Rule),
            Event::TaskListMarker(checked) => {
                if let Some(Frame::Item(task, _, _)) = b.stack.last_mut() {
                    *task = Some(checked);
                }
            }
            Event::InlineMath(math) => b.text(&format!("${math}$")),
            Event::DisplayMath(math) => b.text(&format!("$${math}$$")),
        }
    }
    while b.stack.len() > 1 {
        // Unterminated containers (streaming text) close in order.
        match b.stack.pop() {
            Some(Frame::Paragraph(inlines)) => b.push_block(Block::Paragraph(inlines)),
            Some(Frame::Code(lang, text)) => b.push_block(Block::Code { lang, text }),
            _ => {}
        }
    }
    match b.stack.pop() {
        Some(Frame::Root(blocks)) => blocks,
        _ => Vec::new(),
    }
}

// ---- streaming (chat_markdown.dart markdownStableSplit) ----

static HTML_BLOCK_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^ {0,3}<[A-Za-z!/?]").unwrap());
static REFERENCE_DEFINITION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^ {0,3}\[[^\]\n]+\]:").unwrap());
static STREAM_FENCE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(`{3,}|~{3,})").unwrap());
static LIST_MARKER_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:[-*+]|\d{1,9}[.)])(?:\s|$)").unwrap());

/// Where the last top-level block of `source` starts, when the text before it
/// parses into exactly the blocks it forms in the whole text.
pub fn markdown_stable_split(source: &str) -> Option<usize> {
    if HTML_BLOCK_LINE.is_match(source) || REFERENCE_DEFINITION.is_match(source) {
        return None;
    }
    let mut split = None;
    let mut fence: Option<String> = None;
    let mut previous_blank = false;
    let mut start = 0;
    while start < source.len() {
        let newline = source[start..].find('\n').map(|i| start + i);
        let end = newline.unwrap_or(source.len());
        let line = &source[start..end];
        let marker = STREAM_FENCE.captures(line).map(|c| c[1].to_owned());
        match &fence {
            Some(open) => {
                if let Some(m) = &marker
                    && m.as_bytes()[0] == open.as_bytes()[0]
                    && m.len() >= open.len()
                    && line.trim() == m
                {
                    fence = None;
                }
                previous_blank = false;
            }
            None => {
                if previous_blank && start > 0 && !line.is_empty() && line.trim_start().len() == line.len() && !LIST_MARKER_LINE.is_match(line) {
                    split = Some(start);
                }
                if marker.is_some() {
                    fence = marker.clone();
                }
                previous_blank = marker.is_none() && line.trim().is_empty();
            }
        }
        match newline {
            Some(n) => start = n + 1,
            None => break,
        }
    }
    split
}

// ---- chat find (chat_markdown.dart chatFindPattern / chatFindMarkdownMatchCount) ----

/// chat-find/ranges.web.ts findRenderedMatches: the trimmed query's words
/// joined by any whitespace, case-insensitive.
pub fn find_pattern(query: &str) -> Option<Regex> {
    let words: Vec<String> = query.split_whitespace().map(regex::escape).collect();
    if words.is_empty() {
        return None;
    }
    Regex::new(&format!("(?i){}", words.join(r"\s+"))).ok()
}

/// The searchable text of each rendered text block, in document order.
pub fn text_blocks(blocks: &[Block], out: &mut Vec<String>) {
    for block in blocks {
        match block {
            Block::Paragraph(inlines) | Block::Heading { inlines, .. } => out.push(inline_text(inlines)),
            Block::List { items, .. } => {
                for item in items {
                    text_blocks(&item.blocks, out);
                }
            }
            Block::Quote(blocks) | Block::Alert { blocks, .. } => text_blocks(blocks, out),
            Block::Code { text, .. } => out.push(text.clone()),
            Block::Table { head, rows, .. } => {
                for cell in head.iter().chain(rows.iter().flatten()) {
                    out.push(inline_text(cell));
                }
            }
            Block::Rule => {}
        }
    }
}

pub fn inline_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } | Inline::Code { text, .. } => out.push_str(text),
            Inline::Link { children, .. } => out.push_str(&inline_text(children)),
            Inline::Image { .. } => {}
            Inline::Break => out.push('\n'),
        }
    }
    out
}

/// Occurrences of `query` in the rendered text of `source` (a match never
/// spans two blocks).
pub fn find_match_count(source: &str, query: &str) -> usize {
    let Some(pattern) = find_pattern(query) else { return 0 };
    let mut texts = Vec::new();
    text_blocks(&parse(source, None, false), &mut texts);
    texts.iter().map(|t| pattern.find_iter(t).count()).sum()
}
