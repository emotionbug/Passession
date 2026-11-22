//! Small revisions prepare inline, whether completed or following the reveal clock.
//! Stable Markdown prefixes retain parsed blocks; only the mutable tail is
//! captured, parsed and flattened again. Unsafe context uses a full parse.
//! A captured revision finishes before newer demand replaces it.

use super::super::links::FileLinkSourceType;
use super::super::reveal::{
    TEXT_REVEAL_FRAME_INTERVAL_S, clamp_to_safe_reveal_boundary, compute_reveal_step,
    trim_incomplete_trailing_cluster,
};
use super::model::{self, Block, Inline, Style};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, LinkedList};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, mpsc};

const CAPTURE_BYTES: usize = 64 * 1024;
pub(super) const INTERACTIVE_SOURCE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum InlinePreparation {
    #[default]
    Background,
    Live,
    History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParagraphKey {
    pub address: usize,
    pub len: usize,
}

impl ParagraphKey {
    pub fn of(inlines: &[Inline]) -> Self {
        Self {
            address: inlines.as_ptr() as usize,
            len: inlines.len(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticSpan {
    pub len: usize,
    pub style: Style,
    pub code: bool,
    pub link: bool,
}

/// Byte range, destination, source kind and display text of one paragraph link.
pub type SemanticLink = (usize, usize, Arc<str>, FileLinkSourceType, Arc<str>);

#[derive(Debug, Default)]
pub struct SemanticParagraph {
    pub text: Arc<str>,
    pub copy_end: usize,
    pub empty: bool,
    pub contains_images: bool,
    pub find_matches: Arc<[(usize, usize)]>,
    pub spans: Arc<[SemanticSpan]>,
    pub codes: Arc<[(usize, usize)]>,
    pub strikes: Arc<[(usize, usize)]>,
    pub links: Arc<[SemanticLink]>,
    pub link_max_ends: Arc<[usize]>,
}

/// Immutable chunks keep paragraph addresses and geometry reusable across
/// append revisions; the prefix spine never retains superseded open tails.
pub struct ParsedChunk {
    pub blocks: Rc<Vec<Block>>,
    pub codes: HashMap<usize, Arc<SemanticParagraph>>,
    pub paragraphs: HashMap<ParagraphKey, Arc<SemanticParagraph>>,
}

pub struct ParsedPrefix {
    pub previous: Option<Rc<ParsedPrefix>>,
    pub chunk: Rc<ParsedChunk>,
}

impl Drop for ParsedPrefix {
    fn drop(&mut self) {
        // Evicting a response with many incremental boundaries must not
        // recursively destroy an arbitrarily deep persistent spine.
        let mut previous = self.previous.take();
        while let Some(node) = previous {
            match Rc::try_unwrap(node) {
                Ok(mut node) => previous = node.previous.take(),
                Err(_) => break,
            }
        }
    }
}

pub struct ParsedSnapshot {
    pub revision: u64,
    pub epoch: u64,
    pub source: Rc<Value>,
    pub replaces_previous: bool,
    pub find_pattern: Option<String>,
    pub(super) inline: InlinePreparation,
    pub prefix: Option<Rc<ParsedPrefix>>,
    pub tail: Rc<ParsedChunk>,
    pub stable_end: usize,
}

impl ParsedSnapshot {
    pub fn source_text(&self) -> &str {
        entry_text(&self.source)
    }

    pub fn chunks(&self) -> Vec<Rc<ParsedChunk>> {
        let mut chunks = Vec::new();
        let mut prefix = self.prefix.as_ref();
        while let Some(node) = prefix {
            chunks.push(node.chunk.clone());
            prefix = node.previous.as_ref();
        }
        chunks.reverse();
        chunks.push(self.tail.clone());
        chunks
    }
}

struct ParsedPart {
    blocks: Vec<Block>,
    codes: HashMap<usize, Arc<SemanticParagraph>>,
    paragraphs: HashMap<ParagraphKey, Arc<SemanticParagraph>>,
}

impl From<ParsedPart> for ParsedChunk {
    fn from(part: ParsedPart) -> Self {
        Self {
            blocks: Rc::new(part.blocks),
            codes: part.codes,
            paragraphs: part.paragraphs,
        }
    }
}

struct ParsedDocument {
    revision: u64,
    epoch: u64,
    replaces_previous: bool,
    find_pattern: Option<String>,
    inline: InlinePreparation,
    stable: Option<ParsedPart>,
    tail: ParsedPart,
    start: usize,
    stable_end: usize,
}

fn entry_text(entry: &Value) -> &str {
    entry
        .get("item")
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

#[derive(Clone)]
struct Demand {
    entry: Rc<Value>,
    bytes: usize,
    live: bool,
}

impl Demand {
    fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.entry, &other.entry)
            && self.bytes == other.bytes
            && self.live == other.live
    }

    fn text(&self) -> &str {
        entry_text(&self.entry)
    }
}

struct Capture {
    demand: Demand,
    offset: usize,
    // A linked list avoids reallocating an ever-growing descriptor vector on
    // the event thread. The worker joins chunks into its own contiguous buffer.
    chunks: LinkedList<String>,
    revision: ParseRevision,
}

struct ParseRevision {
    revision: u64,
    epoch: u64,
    reveal_bytes: usize,
    live: bool,
    inline: InlinePreparation,
    start: usize,
    replaces_previous: bool,
}

struct ParseRequest {
    revision: ParseRevision,
    root: Option<String>,
    linkify: bool,
    find: Option<Regex>,
    result: mpsc::SyncSender<ParsedDocument>,
    chunks: LinkedList<String>,
    bytes: usize,
}

fn parse_document(
    text: &str,
    revision: &ParseRevision,
    options: &ParseOptions<'_>,
) -> ParsedDocument {
    let end = revealed_end(
        text,
        revision.reveal_bytes.saturating_sub(revision.start),
        revision.live,
    );
    let text = &text[..end];
    let split = if incremental_safe(text) {
        model::markdown_stable_split(text)
    } else {
        None
    };
    let stable = split.map(|split| parse_part(&text[..split], options));
    let tail = parse_part(&text[split.unwrap_or(0)..], options);
    ParsedDocument {
        revision: revision.revision,
        epoch: revision.epoch,
        replaces_previous: revision.replaces_previous,
        stable,
        tail,
        start: revision.start,
        stable_end: revision.start + split.unwrap_or(0),
        inline: revision.inline,
        find_pattern: options.find.map(|pattern| pattern.as_str().to_owned()),
    }
}

fn parse_part(text: &str, options: &ParseOptions<'_>) -> ParsedPart {
    let blocks = model::parse(text, options.root, options.linkify);
    let mut paragraphs = HashMap::new();
    let mut codes = HashMap::new();
    collect_blocks(&blocks, &mut paragraphs, &mut codes);
    if let Some(pattern) = options.find {
        for rich in paragraphs.values_mut().chain(codes.values_mut()) {
            let rich = Arc::get_mut(rich).expect("unshared paragraph");
            rich.find_matches = pattern
                .find_iter(&rich.text)
                .map(|m| (m.start(), m.end()))
                .collect::<Vec<_>>()
                .into();
        }
    }
    ParsedPart {
        blocks,
        paragraphs,
        codes,
    }
}

/// References can retroactively resolve earlier prose. HTML normalization and
/// open fences also require full context; never promote those documents.
fn incremental_safe(text: &str) -> bool {
    static CONTEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[^\]\n]+\]:").unwrap());
    if text.contains('<') || CONTEXT.is_match(text) {
        return false;
    }
    let mut fence: Option<(u8, usize)> = None;
    for line in text.lines() {
        let line = line.trim_start();
        let byte = line.as_bytes().first().copied().unwrap_or(0);
        let len = line.bytes().take_while(|b| *b == byte).count();
        if matches!(byte, b'`' | b'~') && len >= 3 {
            match fence {
                Some((open, count))
                    if open == byte && len >= count && line[len..].trim().is_empty() =>
                {
                    fence = None
                }
                None => fence = Some((byte, len)),
                _ => {}
            }
        }
    }
    fence.is_none()
}

struct ParserMailbox {
    send: mpsc::SyncSender<ParsedDocument>,
    results: mpsc::Receiver<ParsedDocument>,
}

impl ParserMailbox {
    fn new() -> Self {
        let (send, results) = mpsc::sync_channel(1);
        Self { send, results }
    }
}

/// Fixed process-wide CPU concurrency and queue length, independent of how
/// many historical rows retain their source/layout snapshots.
static PARSER_QUEUE: LazyLock<mpsc::SyncSender<ParseRequest>> = LazyLock::new(|| {
    let (send, receive) = mpsc::sync_channel::<ParseRequest>(64);
    let receive = Arc::new(Mutex::new(receive));
    for index in 0..2 {
        let receive = receive.clone();
        std::thread::Builder::new()
            .name(format!("markdown-parse-{index}"))
            .spawn(move || {
                loop {
                    let request = {
                        let receive = receive.lock().expect("Markdown request queue");
                        receive.recv()
                    };
                    let Ok(mut request) = request else { break };
                    let mut text = String::with_capacity(request.bytes - request.revision.start);
                    for chunk in std::mem::take(&mut request.chunks) {
                        text.push_str(&chunk);
                    }
                    let document = parse_document(
                        &text,
                        &request.revision,
                        &ParseOptions {
                            root: request.root.as_deref(),
                            linkify: request.linkify,
                            find: request.find.as_ref(),
                        },
                    );
                    // Every owner has only one in-flight job and a one-slot result
                    // mailbox; a deleted row simply drops this completed result.
                    let _ = request.result.send(document);
                }
            })
            .expect("spawn Markdown parser");
    }
    send
});

/// Parsing settings shared by every revision until the worker's epoch changes.
pub struct ParseOptions<'a> {
    pub root: Option<&'a str>,
    pub linkify: bool,
    pub find: Option<&'a Regex>,
}

/// Per-row state. Rc entry identities are revision tokens, not content hashes;
/// same-length replacements therefore schedule a new authoritative parse too.
#[derive(Default)]
pub struct MarkdownWorker {
    owner: Option<(String, String, String)>,
    root: Option<String>,
    linkify: bool,
    find: Option<Regex>,
    parser: Option<ParserMailbox>,
    latest: Option<Demand>,
    capture: Option<Capture>,
    active: Option<Demand>,
    completed: Option<Demand>,
    candidate: Option<Rc<ParsedSnapshot>>,
    published: Option<Rc<ParsedSnapshot>>,
    revision: u64,
    epoch: u64,
    paced_bytes: usize,
    last_step: Option<f64>,
    release: bool,
}

impl MarkdownWorker {
    /// Call once in the row's preparation pass, never in its final paint pass.
    /// Returns whether the UI should schedule another preparation frame.
    /// `admit_history` is consulted only for short completed sources that
    /// have not already used the live inline path.
    pub fn update(
        &mut self,
        owner: (&str, &str, &str),
        entry: Rc<Value>,
        live: bool,
        options: ParseOptions<'_>,
        now: f64,
        admit_history: impl FnOnce(usize) -> bool,
    ) -> bool {
        let ParseOptions {
            root,
            linkify,
            find,
        } = options;
        let reset = self
            .owner
            .as_ref()
            .map(|(row, host, agent)| (row.as_str(), host.as_str(), agent.as_str()))
            != Some(owner)
            || self.root.as_deref() != root
            || self.linkify != linkify
            || self.find.as_ref().map(Regex::as_str) != find.map(Regex::as_str);
        if reset {
            // Keep one in-flight job and let its revision finish. Its epoch
            // cannot publish into recycled rows or changed settings.
            let revision = self.revision;
            let epoch = self.epoch.wrapping_add(1);
            let parser = self.parser.take();
            let active = self.active.take();
            *self = Self {
                owner: Some((owner.0.to_owned(), owner.1.to_owned(), owner.2.to_owned())),
                root: root.map(str::to_owned),
                linkify,
                find: find.cloned(),
                revision,
                epoch,
                parser,
                active,
                ..Self::default()
            };
        }
        let text = entry
            .get("item")
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let target = text.len();
        let release = std::mem::take(&mut self.release);
        if self.latest.is_none() || !live || release {
            self.paced_bytes = target;
            self.last_step = None;
        } else {
            self.paced_bytes = self.paced_bytes.min(target);
            if self.paced_bytes < target {
                // use-revealed-text.ts: the first frame after growth counts as one interval.
                let previous = self
                    .last_step
                    .replace(now)
                    .unwrap_or(now - TEXT_REVEAL_FRAME_INTERVAL_S);
                self.paced_bytes +=
                    compute_reveal_step(target - self.paced_bytes, (now - previous) * 1000.0);
            } else {
                self.last_step = None;
            }
        }
        self.latest = Some(Demand {
            entry,
            bytes: self.paced_bytes,
            live,
        });

        if let Some(parser) = &self.parser
            && let Ok(document) = parser.results.try_recv()
        {
            let finished = self.active.take();
            if document.epoch == self.epoch {
                let finished = finished.expect("active Markdown demand");
                self.candidate = Some(self.snapshot(document, &finished));
                self.completed = Some(finished);
            }
        }
        if self.active.is_none() && self.capture.is_none() && self.candidate.is_none() {
            let latest = self.latest.as_ref().expect("current demand");
            if !self
                .completed
                .as_ref()
                .is_some_and(|done| done.same(latest))
            {
                let previous = self.published.as_ref();
                let replaces_previous = previous.is_some_and(|old| {
                    !Rc::ptr_eq(&latest.entry, &old.source)
                        && !latest.text().starts_with(old.source_text())
                });
                let start = previous
                    .filter(|old| {
                        !replaces_previous
                            && latest.bytes >= old.stable_end
                            && incremental_safe(&latest.text()[old.stable_end..])
                    })
                    .map_or(0, |old| old.stable_end);
                let source_bytes = latest.text().len();
                // Short history shares the exact inline path with live text,
                // but the caller budgets this extra work across the frame.
                let inline = if source_bytes - start > INTERACTIVE_SOURCE_BYTES {
                    InlinePreparation::Background
                } else if latest.live
                    || previous.is_some_and(|snapshot| snapshot.inline == InlinePreparation::Live)
                {
                    InlinePreparation::Live
                } else if source_bytes <= INTERACTIVE_SOURCE_BYTES && admit_history(source_bytes) {
                    InlinePreparation::History
                } else {
                    InlinePreparation::Background
                };
                self.revision = self.revision.wrapping_add(1);
                let revision = ParseRevision {
                    revision: self.revision,
                    epoch: self.epoch,
                    start,
                    replaces_previous,
                    reveal_bytes: latest.bytes,
                    live: latest.live,
                    inline,
                };
                if inline != InlinePreparation::Background {
                    let document = parse_document(
                        &latest.text()[start..],
                        &revision,
                        &ParseOptions {
                            root: self.root.as_deref(),
                            linkify: self.linkify,
                            find: self.find.as_ref(),
                        },
                    );
                    self.candidate = Some(self.snapshot(document, latest));
                    self.completed = Some(latest.clone());
                } else {
                    self.capture = Some(Capture {
                        demand: latest.clone(),
                        offset: start,
                        chunks: LinkedList::new(),
                        revision,
                    });
                }
            }
        }
        if let Some(capture) = &mut self.capture {
            let text = capture.demand.text();
            // Retain complete tail context for grapheme-safe reveal; the
            // stable prefix is owned by immutable parsed chunks, not copied.
            let target = text.len();
            let mut end = capture.offset.saturating_add(CAPTURE_BYTES).min(target);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > capture.offset {
                capture
                    .chunks
                    .push_back(text[capture.offset..end].to_owned());
                capture.offset = end;
            }
            if capture.offset == target {
                let capture = self.capture.take().expect("finished capture");
                let parser = self.parser.get_or_insert_with(ParserMailbox::new);
                // Queue saturation retains the completed bounded capture; it
                // never blocks the UI or restarts source copying.
                let request = ParseRequest {
                    revision: capture.revision,
                    root: self.root.clone(),
                    linkify: self.linkify,
                    find: self.find.clone(),
                    result: parser.send.clone(),
                    chunks: capture.chunks,
                    bytes: capture.offset,
                };
                match PARSER_QUEUE.try_send(request) {
                    Ok(()) => self.active = Some(capture.demand),
                    Err(mpsc::TrySendError::Full(request)) => {
                        self.capture = Some(Capture {
                            demand: capture.demand,
                            offset: capture.offset,
                            revision: request.revision,
                            chunks: request.chunks,
                        });
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => {
                        panic!("Markdown parser pool disconnected")
                    }
                }
            }
        }
        self.is_pending()
    }

    fn snapshot(&self, document: ParsedDocument, demand: &Demand) -> Rc<ParsedSnapshot> {
        let mut prefix = if document.start > 0 {
            self.published.as_ref().and_then(|old| old.prefix.clone())
        } else {
            None
        };
        if let Some(stable) = document.stable {
            prefix = Some(Rc::new(ParsedPrefix {
                previous: prefix,
                chunk: Rc::new(stable.into()),
            }));
        }
        Rc::new(ParsedSnapshot {
            revision: document.revision,
            epoch: document.epoch,
            source: demand.entry.clone(),
            replaces_previous: document.replaces_previous,
            find_pattern: document.find_pattern,
            inline: document.inline,
            prefix,
            tail: Rc::new(document.tail.into()),
            stable_end: document.stable_end,
        })
    }

    /// The row returns to the screen after it was hidden: text received
    /// meanwhile is shown whole on its next update, like a first sight.
    pub fn release_backlog(&mut self) {
        self.release = true;
    }

    /// The final paint pass must retain preparation demand from the dry pass.
    pub fn is_pending(&self) -> bool {
        self.active.is_some()
            || self.capture.is_some()
            || self.candidate.is_some()
            || self.latest.as_ref().is_some_and(|latest| {
                latest.bytes < latest.text().len()
                    || !self
                        .completed
                        .as_ref()
                        .is_some_and(|done| done.same(latest))
            })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn candidate(&self) -> Option<Rc<ParsedSnapshot>> {
        self.candidate.clone()
    }

    pub fn published(&self) -> Option<Rc<ParsedSnapshot>> {
        self.published.clone()
    }

    /// The view calls this only after every layout in the candidate is ready.
    /// A source owner and its exact geometry are thus committed together.
    pub fn publish(&mut self, revision: u64) -> bool {
        let Some(candidate) = &self.candidate else {
            return false;
        };
        if candidate.revision != revision {
            return false;
        }
        self.published = self.candidate.take();
        true
    }
}

fn revealed_end(text: &str, bytes: usize, live: bool) -> usize {
    if !live {
        text.len()
    } else if bytes >= text.len() {
        trim_incomplete_trailing_cluster(text).len()
    } else {
        clamp_to_safe_reveal_boundary(text, bytes)
    }
}

fn collect_blocks(
    blocks: &[Block],
    paragraphs: &mut HashMap<ParagraphKey, Arc<SemanticParagraph>>,
    codes: &mut HashMap<usize, Arc<SemanticParagraph>>,
) {
    for block in blocks {
        match block {
            Block::Paragraph(inlines) | Block::Heading { inlines, .. } => {
                collect_inlines(inlines, paragraphs)
            }
            Block::Quote(blocks) | Block::Alert { blocks, .. } => {
                collect_blocks(blocks, paragraphs, codes)
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_blocks(&item.blocks, paragraphs, codes);
                }
            }
            Block::Table { head, rows, .. } => {
                for cell in head.iter().chain(rows.iter().flat_map(|row| row.iter())) {
                    collect_inlines(cell, paragraphs);
                }
            }
            Block::Code { text, .. } => {
                codes.insert(
                    text.as_ptr() as usize,
                    Arc::new(SemanticParagraph {
                        text: Arc::from(text.as_str()),
                        empty: text.is_empty(),
                        copy_end: text.len(),
                        spans: vec![SemanticSpan {
                            len: text.len(),
                            style: Style::default(),
                            code: true,
                            link: false,
                        }]
                        .into(),
                        ..SemanticParagraph::default()
                    }),
                );
            }
            Block::Rule => {}
        }
    }
}

fn collect_inlines(
    inlines: &[Inline],
    paragraphs: &mut HashMap<ParagraphKey, Arc<SemanticParagraph>>,
) {
    // The painter partitions top-level images into separate visual blocks.
    // Include the complete cell too: table layout consumes the unsplit slice.
    paragraphs.insert(ParagraphKey::of(inlines), Arc::new(flatten(inlines)));
    let mut start = 0;
    for (index, inline) in inlines.iter().enumerate() {
        if matches!(inline, Inline::Image { .. }) {
            if index > start {
                paragraphs.insert(
                    ParagraphKey::of(&inlines[start..index]),
                    Arc::new(flatten(&inlines[start..index])),
                );
            }
            start = index + 1;
        }
    }
    if start > 0 && start < inlines.len() {
        paragraphs.insert(
            ParagraphKey::of(&inlines[start..]),
            Arc::new(flatten(&inlines[start..])),
        );
    }
}

fn flatten(inlines: &[Inline]) -> SemanticParagraph {
    #[derive(Default)]
    struct FlatBuilder {
        spans: Vec<SemanticSpan>,
        codes: Vec<(usize, usize)>,
        strikes: Vec<(usize, usize)>,
        links: Vec<SemanticLink>,
        contains_images: bool,
    }
    fn push(
        text: &mut String,
        rich: &mut FlatBuilder,
        value: &str,
        style: Style,
        code: bool,
        link: bool,
    ) {
        if value.is_empty() {
            return;
        }
        let start = text.len();
        text.push_str(value);
        let span = SemanticSpan {
            len: value.len(),
            style,
            code,
            link,
        };
        match rich.spans.last_mut() {
            Some(last) if last.style == style && last.code == code && last.link == link => {
                last.len += value.len()
            }
            _ => rich.spans.push(span),
        }
        if code {
            rich.codes.push((start, text.len()));
        }
        if style.strike {
            rich.strikes.push((start, text.len()));
        }
    }
    fn visit(text: &mut String, rich: &mut FlatBuilder, inlines: &[Inline], link: bool) {
        for inline in inlines {
            match inline {
                Inline::Text { text: value, style } => push(text, rich, value, *style, false, link),
                Inline::Code { text: value, style } => push(text, rich, value, *style, true, link),
                Inline::Link {
                    href,
                    kind,
                    children,
                } => {
                    let start = text.len();
                    visit(text, rich, children, true);
                    if text.len() > start {
                        rich.links.push((
                            start,
                            text.len(),
                            Arc::from(href.as_str()),
                            *kind,
                            Arc::from(&text[start..]),
                        ));
                    }
                }
                // A soft break uses the paragraph color, even inside a link.
                Inline::Break => push(text, rich, "\n", Style::default(), false, false),
                Inline::Image { .. } => rich.contains_images = true,
            }
        }
    }
    let mut rich = FlatBuilder::default();
    let mut text = String::new();
    visit(&mut text, &mut rich, inlines, false);
    rich.links.sort_by_key(|link| (link.0, link.1));
    let mut max_end = 0;
    let link_max_ends: Arc<[usize]> = rich
        .links
        .iter()
        .map(|link| {
            max_end = max_end.max(link.1);
            max_end
        })
        .collect::<Vec<_>>()
        .into();
    let empty = text.trim_matches('\n').is_empty();
    let copy_end = text.trim_end_matches('\n').len();
    SemanticParagraph {
        text: Arc::from(text),
        empty,
        contains_images: rich.contains_images,
        copy_end,
        find_matches: Arc::default(),
        link_max_ends,
        spans: Arc::from(rich.spans),
        codes: Arc::from(rich.codes),
        strikes: Arc::from(rich.strikes),
        links: Arc::from(rich.links),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OPTIONS: ParseOptions<'_> = ParseOptions {
        root: None,
        linkify: true,
        find: None,
    };

    #[test]
    fn captures_bounded_chunks_and_does_not_restart_for_new_tokens() {
        let first = Rc::new(json!({"item": {"text": "a".repeat(CAPTURE_BYTES * 4)}}));
        let next = Rc::new(json!({"item": {"text": "b".repeat(CAPTURE_BYTES * 4)}}));
        let mut worker = MarkdownWorker::default();
        assert!(worker.update(OWNER, first.clone(), true, OPTIONS, 0.0, |_| false));
        let capture = worker.capture.as_ref().unwrap();
        assert_eq!(capture.offset, CAPTURE_BYTES);
        assert!(Rc::ptr_eq(&capture.demand.entry, &first));
        assert!(
            worker.is_pending(),
            "final paint must keep the dry-pass capture scheduled"
        );
        assert!(worker.update(OWNER, next.clone(), false, OPTIONS, 0.01, |_| false));
        let capture = worker.capture.as_ref().unwrap();
        assert_eq!(capture.offset, CAPTURE_BYTES * 2);
        assert!(Rc::ptr_eq(&capture.demand.entry, &first));
        let latest = worker.latest.as_ref().unwrap();
        assert!(Rc::ptr_eq(&latest.entry, &next));
        assert!(!latest.live);
        assert_eq!(latest.bytes, CAPTURE_BYTES * 4);
    }

    #[test]
    fn same_length_edits_are_distinct_demands() {
        let first = Demand {
            entry: Rc::new(json!({"item": {"text": "before"}})),
            bytes: 6,
            live: false,
        };
        let replacement = Demand {
            entry: Rc::new(json!({"item": {"text": "after!"}})),
            bytes: 6,
            live: false,
        };
        assert!(!first.same(&replacement));
        assert!(first.same(&first.clone()));
    }

    #[test]
    fn worker_reveal_uses_full_unicode_context_and_flushes_final_source() {
        let text = "prefix 👩🏽‍💻 suffix";
        assert_eq!(revealed_end(text, "prefix 👩".len(), true), "prefix ".len());
        assert_eq!(revealed_end(text, 1, false), text.len());
        let trailing = "complete 🇦";
        assert_eq!(
            revealed_end(trailing, trailing.len(), true),
            "complete ".len()
        );
        assert_eq!(
            revealed_end(trailing, trailing.len(), false),
            trailing.len()
        );
        let followed = "🇦 rest";
        assert_eq!(revealed_end(followed, "🇦".len(), true), "🇦".len());
    }

    #[test]
    fn semantic_text_and_ranges_preserve_utf8_and_nested_styles() {
        let inlines = vec![
            Inline::Text {
                text: "é ".into(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
            },
            Inline::Link {
                href: "file.rs".into(),
                kind: FileLinkSourceType::Markdown,
                children: vec![Inline::Code {
                    text: "👩🏽‍💻".into(),
                    style: Style {
                        strike: true,
                        ..Style::default()
                    },
                }],
            },
            Inline::Break,
            Inline::Text {
                text: "終".into(),
                style: Style::default(),
            },
        ];
        let rich = flatten(&inlines);
        assert_eq!(&*rich.text, "é 👩🏽‍💻\n終");
        assert_eq!(
            rich.spans.iter().map(|span| span.len).sum::<usize>(),
            rich.text.len()
        );
        let start = "é ".len();
        let end = "é 👩🏽‍💻".len();
        assert_eq!(&*rich.codes, &[(start, end)]);
        assert_eq!(&*rich.strikes, &[(start, end)]);
        assert_eq!((rich.links[0].0, rich.links[0].1), (start, end));
        assert_eq!(&*rich.links[0].2, "file.rs");
        assert!(rich.spans[1].code && rich.spans[1].link && rich.spans[1].style.strike);
    }

    #[test]
    fn semantic_index_covers_nested_blocks_cells_and_image_segments() {
        let blocks = model::parse(
            "# Heading\n\n> quoted\n\n- item\n\n| column |\n| --- |\n| cell |\n\nbefore ![alt](image.png) after",
            None,
            false,
        );
        let mut paragraphs = HashMap::new();
        collect_blocks(&blocks, &mut paragraphs, &mut HashMap::new());
        let Block::Paragraph(inlines) = blocks.last().unwrap() else {
            panic!("expected mixed image paragraph");
        };
        assert!(paragraphs[&ParagraphKey::of(inlines)].contains_images);
        for text in [
            "Heading", "quoted", "item", "column", "cell", "before ", " after",
        ] {
            assert!(
                paragraphs.values().any(|rich| &*rich.text == text),
                "missing {text:?}"
            );
        }
    }

    #[test]
    fn publication_is_exact_and_candidate_blocks_new_capture() {
        let entry = Rc::new(json!({"item": {"text": "latest"}}));
        let snapshot = Rc::new(ParsedSnapshot {
            revision: 7,
            epoch: 0,
            source: message("old"),
            replaces_previous: false,
            find_pattern: None,
            inline: InlinePreparation::Background,
            prefix: None,
            stable_end: 0,
            tail: Rc::new(ParsedChunk {
                blocks: Rc::new(Vec::new()),
                codes: HashMap::new(),
                paragraphs: HashMap::new(),
            }),
        });
        let mut worker = MarkdownWorker {
            owner: Some(("row".into(), "host".into(), "agent".into())),
            linkify: true,
            candidate: Some(snapshot.clone()),
            ..MarkdownWorker::default()
        };
        assert!(worker.update(OWNER, entry, false, OPTIONS, 0.0, |_| false));
        assert!(worker.capture.is_none());
        assert!(worker.parser.is_none());
        assert!(!worker.publish(6));
        assert!(worker.publish(7));
        assert!(Rc::ptr_eq(&worker.published().unwrap(), &snapshot));
        assert!(worker.candidate().is_none());
        assert!(
            worker.is_pending(),
            "publishing an older candidate must still schedule latest demand"
        );
    }

    #[test]
    fn owner_context_and_query_changes_advance_epoch_without_ui_parsing() {
        let entry = Rc::new(json!({"item": {"text": "a".repeat(CAPTURE_BYTES * 8)}}));
        let mut worker = MarkdownWorker::default();
        worker.update(OWNER, entry.clone(), false, OPTIONS, 0.0, |_| false);
        let first = worker.epoch();
        worker.update(
            ("row", "other-host", "agent"),
            entry.clone(),
            false,
            OPTIONS,
            0.0,
            |_| false,
        );
        assert_ne!(worker.epoch(), first);
        let second = worker.epoch();
        let pattern = model::find_pattern("a").unwrap();
        worker.update(
            ("row", "other-host", "agent"),
            entry,
            false,
            ParseOptions {
                find: Some(&pattern),
                ..OPTIONS
            },
            0.0,
            |_| false,
        );
        assert_ne!(worker.epoch(), second);
        assert!(worker.candidate().is_none());
        assert!(worker.published().is_none());
        assert!(worker.parser.is_none());
        assert_eq!(worker.capture.as_ref().unwrap().offset, CAPTURE_BYTES);
    }

    #[test]
    fn code_and_full_paragraph_semantics_are_uncapped() {
        let text = "é漢👩🏽‍💻".repeat(10_000);
        let blocks = model::parse(&text, None, false);
        let mut paragraphs = HashMap::new();
        let mut codes = HashMap::new();
        collect_blocks(&blocks, &mut paragraphs, &mut codes);
        assert_eq!(blocks.len(), 1);
        let paragraph = paragraphs.values().next().unwrap();
        assert_eq!(paragraph.text.as_ref(), text.as_str());
        let code = vec![Block::Code {
            lang: String::new(),
            text: text.clone(),
        }];
        collect_blocks(&code, &mut paragraphs, &mut codes);
        let Block::Code { text: source, .. } = &code[0] else {
            unreachable!()
        };
        let prepared = codes.get(&(source.as_ptr() as usize)).unwrap();
        assert_eq!(&*prepared.text, text);
        assert_eq!(prepared.spans[0].len, text.len());
        assert!(prepared.spans[0].code);
    }

    #[test]
    fn code_copy_preserves_blank_lines_from_fenced_source() {
        let blocks = model::parse("```text\né漢 \n\n```\n", None, false);
        let mut codes = HashMap::new();
        collect_blocks(&blocks, &mut HashMap::new(), &mut codes);
        let code = codes.values().next().unwrap();
        assert_eq!(&code.text[..code.copy_end], "é漢 \n\n");
    }

    const OWNER: (&str, &str, &str) = ("row", "host", "agent");

    fn message(text: &str) -> Rc<Value> {
        Rc::new(json!({"item": {"text": text}}))
    }

    #[test]
    fn short_history_is_exact_on_first_preparation_and_reuses_the_published_row() {
        let source = "# Completed\n\n**한국어** 🇦";
        let entry = message(source);
        let mut worker = MarkdownWorker::default();
        assert!(
            worker.update(OWNER, entry.clone(), false, OPTIONS, 0.0, |bytes| {
                assert_eq!(bytes, source.len());
                true
            })
        );
        assert!(worker.parser.is_none(), "no parser round trip");
        let first = worker
            .candidate()
            .expect("first preparation exposes a candidate");
        assert_eq!(first.inline, InlinePreparation::History);
        assert!(first.chunks().iter().any(|chunk| {
            chunk
                .paragraphs
                .values()
                .any(|paragraph| paragraph.text.as_ref() == "한국어 🇦")
        }));
        assert!(worker.publish(first.revision));

        // Scrolling back to a warm row consumes neither a reveal step nor
        // a new inline reservation; it keeps the same source/layout identity.
        assert!(
            !worker.update(OWNER, entry.clone(), false, OPTIONS, 1.0, |_| {
                panic!("a settled historical row must not reparse");
            })
        );
        assert!(Rc::ptr_eq(&worker.published().unwrap(), &first));

        let new_owner = ("recycled", "host", "agent");
        let replacement = message("Different completed row");
        worker.update(new_owner, replacement.clone(), false, OPTIONS, 1.0, |_| true);
        let next = worker.candidate().expect("recycled row uses its own source");
        assert_ne!(next.epoch, first.epoch);
        assert!(Rc::ptr_eq(&next.source, &replacement));
        assert!(worker.published().is_none());
    }

    #[test]
    fn live_backlog_reaches_the_received_text_within_the_horizon_rule() {
        let mut worker = MarkdownWorker::default();
        worker.update(OWNER, message("a"), true, OPTIONS, 100.0, |_| false);
        assert_eq!(worker.paced_bytes, 1, "first sight is whole");
        let grown = message(&"a".repeat(10_001));
        let mut now = 100.5;
        worker.update(OWNER, grown.clone(), true, OPTIONS, now, |_| false);
        assert_eq!(
            worker.paced_bytes,
            1 + 10_000_usize.div_ceil(9),
            "growth starts on the frame that sees it"
        );
        let mut frames = 1;
        while worker.paced_bytes < 10_001 {
            now += 1.0 / 60.0;
            frames += 1;
            assert!(
                worker.update(OWNER, grown.clone(), true, OPTIONS, now, |_| false),
                "held-back text keeps frames scheduled"
            );
            assert!(
                frames < 70,
                "10 kB must not trail the stream for more than about 1.1s"
            );
        }
        assert_eq!(worker.latest.as_ref().unwrap().bytes, 10_001);
    }

    #[test]
    fn completion_releases_the_backlog_at_once() {
        let mut worker = MarkdownWorker::default();
        worker.update(OWNER, message("a"), true, OPTIONS, 0.0, |_| false);
        let grown = message(&"a".repeat(50_000));
        worker.update(OWNER, grown.clone(), true, OPTIONS, 1.0, |_| false);
        assert!(worker.paced_bytes < 50_000);
        worker.update(OWNER, grown, false, OPTIONS, 1.01, |_| false);
        let latest = worker.latest.as_ref().unwrap();
        assert_eq!((latest.bytes, latest.live), (50_000, false));
    }

    #[test]
    fn text_received_while_hidden_is_shown_whole_on_return() {
        let mut worker = MarkdownWorker::default();
        worker.update(OWNER, message("a"), true, OPTIONS, 0.0, |_| false);
        // Hidden: no preparation pass ran while the text arrived.
        worker.release_backlog();
        worker.update(OWNER, message(&"a".repeat(50_000)), true, OPTIONS, 60.0, |_| false);
        assert_eq!(worker.latest.as_ref().unwrap().bytes, 50_000);
        // Growth the reader watches is paced again.
        worker.update(OWNER, message(&"a".repeat(60_000)), true, OPTIONS, 60.02, |_| false);
        let bytes = worker.latest.as_ref().unwrap().bytes;
        assert!(bytes > 50_000 && bytes < 60_000, "{bytes}");
    }

    #[test]
    fn background_parse_keeps_frames_scheduled_until_its_result_is_published() {
        let source = "# Title\n\nparagraph\n\n- item";
        let complete = message(source);
        let mut worker = MarkdownWorker::default();
        // An exhausted frame allowance falls back to the worker pool.
        assert!(worker.update(OWNER, complete.clone(), false, OPTIONS, 0.0, |_| false));
        assert!(worker.active.is_some() && worker.candidate.is_none());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while worker.candidate.is_none() {
            assert!(
                worker.is_pending(),
                "the final paint must schedule a frame while the parse is in flight"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "the parser result never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
            // The next frame's preparation pass collects the result; no other event is needed.
            assert!(worker.update(OWNER, complete.clone(), false, OPTIONS, 0.0, |_| false));
        }
        let revision = worker.candidate().unwrap().revision;
        assert!(
            worker.is_pending(),
            "an unpublished candidate still needs its layout passes"
        );
        assert!(worker.publish(revision));
        assert!(!worker.is_pending());
        assert!(
            !worker.update(OWNER, complete, false, OPTIONS, 0.0, |_| false),
            "a published document stops scheduling frames"
        );
        assert_eq!(worker.published().unwrap().source_text(), source);
    }

    #[test]
    fn incremental_prefix_reuse_matches_full_parse() {
        for revisions in [
            vec![
                "# Heading\n\nstable **bold**\n\nopen",
                "# Heading\n\nstable **bold**\n\nopen tail\n\nnext",
                "# Heading\n\nstable **bold**\n\nopen tail\n\nnext 👩🏽‍💻\n\nlast",
            ],
            vec![
                "[target]\n\nold\n\nnew",
                "[target]\n\nold\n\nnew\n\n[target]: https://example.com",
            ],
            vec![
                "head\n\nbody\n\nlast",
                "head\n\nbody\n\nlast\n\n```rs\nlet a = 1;",
                "head\n\nbody\n\nlast\n\n```rs\nlet a = 1;\n```\n\nend",
            ],
            vec![
                "head\n\nbody\n\nlast",
                "head\n\nbody\n\nlast\n\n<div>\nHTML\n</div>",
            ],
            vec!["head\n\nbody\n\nlast", "edited\n\nbody\n\nlast"],
            vec![
                "head\n\n| a | b |\n| - | - |\n| x | y |",
                "head\n\n| a | b |\n| - | - |\n| x | y |\n\nend",
            ],
        ] {
            let mut worker = MarkdownWorker::default();
            let mut previous: Option<Rc<ParsedSnapshot>> = None;
            for (index, source) in revisions.iter().enumerate() {
                worker.release_backlog();
                worker.update(
                    OWNER,
                    message(source),
                    index == 0,
                    OPTIONS,
                    index as f64,
                    |_| false,
                );
                let candidate = worker.candidate().expect("small revision parsed inline");
                let chunks = candidate.chunks();
                let actual: Vec<Block> = chunks
                    .iter()
                    .flat_map(|chunk| chunk.blocks.iter().cloned())
                    .collect();
                assert_eq!(actual, model::parse(source, None, true), "{source}");
                if index == 1 && revisions[0].starts_with("# Heading") {
                    let old = previous.as_ref().unwrap();
                    assert!(
                        Rc::ptr_eq(&old.chunks()[0].blocks, &candidate.chunks()[0].blocks,),
                        "stable blocks must retain paragraph identity"
                    );
                }
                assert!(worker.publish(candidate.revision));
                previous = Some(candidate);
            }
        }
    }
}
