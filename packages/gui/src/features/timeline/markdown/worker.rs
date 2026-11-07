//! Small live revisions follow the reveal clock without a worker round trip.
//! Larger documents capture bounded chunks for whole-document worker parsing.
//! A captured revision finishes before newer demand replaces it.

use super::model::{self, Block, Inline, Style};
use super::super::links::FileLinkSourceType;
use super::super::reveal::{clamp_to_safe_reveal_boundary, compute_reveal_step, trim_incomplete_trailing_cluster};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, LinkedList};
use std::rc::Rc;
use std::sync::{Arc, LazyLock, Mutex, mpsc};

const CAPTURE_BYTES: usize = 64 * 1024;
pub(super) const INTERACTIVE_SOURCE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParagraphKey {
    pub address: usize,
    pub len: usize,
}

impl ParagraphKey {
    pub fn of(inlines: &[Inline]) -> Self {
        Self { address: inlines.as_ptr() as usize, len: inlines.len() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SemanticSpan {
    pub len: usize,
    pub style: Style,
    pub code: bool,
    pub link: bool,
}

#[derive(Debug, Default)]
pub struct SemanticParagraph {
    pub text: Arc<str>,
    pub copy_end: usize,
    pub empty: bool,
    pub find_matches: Arc<[(usize, usize)]>,
    pub spans: Arc<[SemanticSpan]>,
    pub codes: Arc<[(usize, usize)]>,
    pub strikes: Arc<[(usize, usize)]>,
    pub links: Arc<[(usize, usize, Arc<str>, FileLinkSourceType, Arc<str>)]>,
    pub link_max_ends: Arc<[usize]>,
}

/// Inline allocations are immutable and stay at the same addresses when the
/// worker's Vec<Block> moves into this Rc. This owner keeps all index keys alive.
pub struct ParsedSnapshot {
    pub revision: u64,
    pub epoch: u64,
    pub source: Arc<str>,
    pub replaces_previous: bool,
    pub find_pattern: Option<String>,
    pub interactive: bool,
    pub codes: HashMap<usize, Arc<SemanticParagraph>>,
    pub blocks: Rc<Vec<Block>>,
    pub paragraphs: HashMap<ParagraphKey, Arc<SemanticParagraph>>,
}

struct ParsedDocument {
    revision: u64,
    epoch: u64,
    source: Arc<str>,
    replaces_previous: bool,
    find_pattern: Option<String>,
    interactive: bool,
    codes: HashMap<usize, Arc<SemanticParagraph>>,
    blocks: Vec<Block>,
    paragraphs: HashMap<ParagraphKey, Arc<SemanticParagraph>>,
}

impl From<ParsedDocument> for ParsedSnapshot {
    fn from(document: ParsedDocument) -> Self {
        Self {
            revision: document.revision, epoch: document.epoch, source: document.source,
            replaces_previous: document.replaces_previous, find_pattern: document.find_pattern,
            interactive: document.interactive, codes: document.codes,
            blocks: Rc::new(document.blocks), paragraphs: document.paragraphs,
        }
    }
}

#[derive(Clone)]
struct Demand {
    entry: Rc<Value>,
    bytes: usize,
    live: bool,
}

impl Demand {
    fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.entry, &other.entry) && self.bytes == other.bytes && self.live == other.live
    }

    fn text(&self) -> &str {
        self.entry.get("item").and_then(|item| item.get("text")).and_then(Value::as_str).unwrap_or("")
    }
}

struct Capture {
    demand: Demand,
    offset: usize,
    // A linked list avoids reallocating an ever-growing descriptor vector on
    // the event thread. The worker joins chunks into its own contiguous buffer.
    chunks: LinkedList<String>,
}

struct ParseRequest {
    revision: u64,
    epoch: u64,
    root: Option<String>,
    linkify: bool,
    find: Option<Regex>,
    previous: Option<Arc<str>>,
    result: mpsc::SyncSender<ParsedDocument>,
    chunks: LinkedList<String>,
    bytes: usize,
    reveal_bytes: usize,
    live: bool,
    interactive: bool,
}

fn parse_document(request: &mut ParseRequest) -> ParsedDocument {
    let mut text = String::with_capacity(request.bytes);
    for chunk in std::mem::take(&mut request.chunks) {
        text.push_str(&chunk);
    }
    let end = revealed_end(&text, request.reveal_bytes, request.live);
    let replaces_previous = request.previous.as_ref().is_some_and(|old| !text.starts_with(old.as_ref()));
    let blocks = model::parse(&text[..end], request.root.as_deref(), request.linkify);
    let mut paragraphs = HashMap::new();
    let mut codes = HashMap::new();
    collect_blocks(&blocks, &mut paragraphs, &mut codes);
    if let Some(pattern) = &request.find {
        for rich in paragraphs.values_mut().chain(codes.values_mut()) {
            let rich = Arc::get_mut(rich).expect("unshared paragraph");
            rich.find_matches = pattern.find_iter(&rich.text).map(|m| (m.start(), m.end())).collect::<Vec<_>>().into();
        }
    }
    ParsedDocument {
        revision: request.revision, epoch: request.epoch, source: Arc::from(text),
        replaces_previous, blocks, paragraphs, codes, interactive: request.interactive,
        find_pattern: request.find.as_ref().map(|pattern| pattern.as_str().to_owned()),
    }
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
        std::thread::Builder::new().name(format!("markdown-parse-{index}")).spawn(move || {
            loop {
                let request = {
                    let receive = receive.lock().expect("Markdown request queue");
                    receive.recv()
                };
                let Ok(mut request) = request else { break };
                let document = parse_document(&mut request);
                // Every owner has only one in-flight job and a one-slot result
                // mailbox; a deleted row simply drops this completed result.
                let _ = request.result.send(document);
            }
        }).expect("spawn Markdown parser");
    }
    send
});

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
}

impl MarkdownWorker {
    /// Call once in the row's preparation pass, never in its final paint pass.
    /// Returns whether the UI should schedule another preparation frame.
    pub fn update(&mut self, owner: (&str, &str, &str), entry: Rc<Value>, live: bool, root: Option<&str>, linkify: bool, now: f64, find: Option<&Regex>) -> bool {
        let reset = self.owner.as_ref().map(|(row, host, agent)| (row.as_str(), host.as_str(), agent.as_str())) != Some(owner)
            || self.root.as_deref() != root || self.linkify != linkify
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
                root: root.map(str::to_owned), linkify, find: find.cloned(),
                revision, epoch, parser, active, ..Self::default()
            };
        }
        let text = entry.get("item").and_then(|item| item.get("text")).and_then(Value::as_str).unwrap_or("");
        let target = text.len();
        if self.latest.is_none() || !live {
            self.paced_bytes = target;
            self.last_step = None;
        } else {
            self.paced_bytes = self.paced_bytes.min(target);
            if self.paced_bytes < target {
                let previous = self.last_step.replace(now).unwrap_or(now);
                self.paced_bytes += compute_reveal_step(target - self.paced_bytes, (now - previous) * 1000.0);
            } else {
                self.last_step = None;
            }
        }
        self.latest = Some(Demand { entry, bytes: self.paced_bytes, live });

        if let Some(parser) = &self.parser {
            if let Ok(document) = parser.results.try_recv() {
                let finished = self.active.take();
                if document.epoch == self.epoch {
                    self.completed = finished;
                    self.candidate = Some(Rc::new(document.into()));
                }
            }
        }
        if self.active.is_none() && self.capture.is_none() && self.candidate.is_none() {
            let latest = self.latest.as_ref().expect("current demand");
            if !self.completed.as_ref().is_some_and(|done| done.same(latest)) {
                self.capture = Some(Capture { demand: latest.clone(), offset: 0, chunks: LinkedList::new() });
            }
        }
        if let Some(capture) = &mut self.capture {
            let text = capture.demand.text();
            // Capture complete context so both inline and worker parsing choose
            // the same grapheme-safe paced prefix.
            let target = text.len();
            let mut end = capture.offset.saturating_add(CAPTURE_BYTES).min(target);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end > capture.offset {
                capture.chunks.push_back(text[capture.offset..end].to_owned());
                capture.offset = end;
            }
            if capture.offset == target {
                let capture = self.capture.take().expect("finished capture");
                self.revision = self.revision.wrapping_add(1);
                let parser = self.parser.get_or_insert_with(ParserMailbox::new);
                // Queue saturation retains the completed bounded capture; it
                // never blocks the UI or restarts source copying.
                let interactive = capture.offset <= INTERACTIVE_SOURCE_BYTES
                    && (capture.demand.live || self.published.as_ref().is_some_and(|snapshot| snapshot.interactive));
                let mut request = ParseRequest {
                    revision: self.revision, epoch: self.epoch, root: self.root.clone(), linkify: self.linkify, find: self.find.clone(),
                    previous: self.published.as_ref().map(|snapshot| snapshot.source.clone()),
                    result: parser.send.clone(),
                    chunks: capture.chunks, bytes: capture.offset, reveal_bytes: capture.demand.bytes, live: capture.demand.live,
                    interactive,
                };
                if interactive {
                    self.candidate = Some(Rc::new(parse_document(&mut request).into()));
                    self.completed = Some(capture.demand);
                    return self.is_pending();
                }
                match PARSER_QUEUE.try_send(request) {
                    Ok(()) => self.active = Some(capture.demand),
                    Err(mpsc::TrySendError::Full(request)) => {
                        self.capture = Some(Capture { demand: capture.demand, offset: capture.offset, chunks: request.chunks });
                    }
                    Err(mpsc::TrySendError::Disconnected(_)) => panic!("Markdown parser pool disconnected"),
                }
            }
        }
        self.is_pending()
    }

    /// The final paint pass must retain preparation demand from the dry pass.
    pub fn is_pending(&self) -> bool {
        self.active.is_some() || self.capture.is_some() || self.candidate.is_some()
            || self.latest.as_ref().is_some_and(|latest| {
                latest.bytes < latest.text().len()
                    || !self.completed.as_ref().is_some_and(|done| done.same(latest))
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
        let Some(candidate) = &self.candidate else { return false };
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

fn collect_blocks(blocks: &[Block], paragraphs: &mut HashMap<ParagraphKey, Arc<SemanticParagraph>>, codes: &mut HashMap<usize, Arc<SemanticParagraph>>) {
    for block in blocks {
        match block {
            Block::Paragraph(inlines) | Block::Heading { inlines, .. } => collect_inlines(inlines, paragraphs),
            Block::Quote(blocks) | Block::Alert { blocks, .. } => collect_blocks(blocks, paragraphs, codes),
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
                codes.insert(text.as_ptr() as usize, Arc::new(SemanticParagraph {
                    text: Arc::from(text.as_str()),
                    empty: text.is_empty(),
                    copy_end: text.trim_end_matches('\n').len(),
                    spans: vec![SemanticSpan { len: text.len(), style: Style::default(), code: true, link: false }].into(),
                    ..SemanticParagraph::default()
                }));
            }
            Block::Rule => {}
        }
    }
}

fn collect_inlines(inlines: &[Inline], paragraphs: &mut HashMap<ParagraphKey, Arc<SemanticParagraph>>) {
    // The painter partitions top-level images into separate visual blocks.
    // Include the complete cell too: table layout consumes the unsplit slice.
    paragraphs.insert(ParagraphKey::of(inlines), Arc::new(flatten(inlines)));
    let mut start = 0;
    for (index, inline) in inlines.iter().enumerate() {
        if matches!(inline, Inline::Image { .. }) {
            if index > start {
                paragraphs.insert(ParagraphKey::of(&inlines[start..index]), Arc::new(flatten(&inlines[start..index])));
            }
            start = index + 1;
        }
    }
    if start > 0 && start < inlines.len() {
        paragraphs.insert(ParagraphKey::of(&inlines[start..]), Arc::new(flatten(&inlines[start..])));
    }
}

fn flatten(inlines: &[Inline]) -> SemanticParagraph {
    #[derive(Default)]
    struct FlatBuilder {
        spans: Vec<SemanticSpan>,
        codes: Vec<(usize, usize)>,
        strikes: Vec<(usize, usize)>,
        links: Vec<(usize, usize, Arc<str>, FileLinkSourceType, Arc<str>)>,
    }
    fn push(text: &mut String, rich: &mut FlatBuilder, value: &str, style: Style, code: bool, link: bool) {
        if value.is_empty() {
            return;
        }
        let start = text.len();
        text.push_str(value);
        let span = SemanticSpan { len: value.len(), style, code, link };
        match rich.spans.last_mut() {
            Some(last) if last.style == style && last.code == code && last.link == link => last.len += value.len(),
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
                Inline::Link { href, kind, children } => {
                    let start = text.len();
                    visit(text, rich, children, true);
                    if text.len() > start {
                        rich.links.push((start, text.len(), Arc::from(href.as_str()), *kind, Arc::from(&text[start..])));
                    }
                }
                // A soft break uses the paragraph color, even inside a link.
                Inline::Break => push(text, rich, "\n", Style::default(), false, false),
                Inline::Image { .. } => {}
            }
        }
    }
    let mut rich = FlatBuilder::default();
    let mut text = String::new();
    visit(&mut text, &mut rich, inlines, false);
    rich.links.sort_by_key(|link| (link.0, link.1));
    let mut max_end = 0;
    let link_max_ends: Arc<[usize]> = rich.links.iter().map(|link| {
        max_end = max_end.max(link.1);
        max_end
    }).collect::<Vec<_>>().into();
    let empty = text.trim_matches('\n').is_empty();
    let copy_end = text.trim_end_matches('\n').len();
    SemanticParagraph {
        text: Arc::from(text),
        empty,
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

    #[test]
    fn captures_bounded_chunks_and_does_not_restart_for_new_tokens() {
        let first = Rc::new(json!({"item": {"text": "a".repeat(CAPTURE_BYTES * 4)}}));
        let next = Rc::new(json!({"item": {"text": "b".repeat(CAPTURE_BYTES * 4)}}));
        let mut worker = MarkdownWorker::default();
        assert!(worker.update(("row", "host", "agent"), first.clone(), true, None, true, 0.0, None));
        let capture = worker.capture.as_ref().unwrap();
        assert_eq!(capture.offset, CAPTURE_BYTES);
        assert!(Rc::ptr_eq(&capture.demand.entry, &first));
        assert!(worker.is_pending(), "final paint must keep the dry-pass capture scheduled");
        assert!(worker.update(("row", "host", "agent"), next.clone(), false, None, true, 0.01, None));
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
        let first = Demand { entry: Rc::new(json!({"item": {"text": "before"}})), bytes: 6, live: false };
        let replacement = Demand { entry: Rc::new(json!({"item": {"text": "after!"}})), bytes: 6, live: false };
        assert!(!first.same(&replacement));
        assert!(first.same(&first.clone()));
    }

    #[test]
    fn worker_reveal_uses_full_unicode_context_and_flushes_final_source() {
        let text = "prefix 👩🏽‍💻 suffix";
        assert_eq!(revealed_end(text, "prefix 👩".len(), true), "prefix ".len());
        assert_eq!(revealed_end(text, 1, false), text.len());
        let trailing = "complete 🇦";
        assert_eq!(revealed_end(trailing, trailing.len(), true), "complete ".len());
        assert_eq!(revealed_end(trailing, trailing.len(), false), trailing.len());
        let followed = "🇦 rest";
        assert_eq!(revealed_end(followed, "🇦".len(), true), "🇦".len());
    }

    #[test]
    fn semantic_text_and_ranges_preserve_utf8_and_nested_styles() {
        let inlines = vec![
            Inline::Text { text: "é ".into(), style: Style { bold: true, ..Style::default() } },
            Inline::Link {
                href: "file.rs".into(),
                kind: FileLinkSourceType::Markdown,
                children: vec![Inline::Code { text: "👩🏽‍💻".into(), style: Style { strike: true, ..Style::default() } }],
            },
            Inline::Break,
            Inline::Text { text: "終".into(), style: Style::default() },
        ];
        let rich = flatten(&inlines);
        assert_eq!(&*rich.text, "é 👩🏽‍💻\n終");
        assert_eq!(rich.spans.iter().map(|span| span.len).sum::<usize>(), rich.text.len());
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
        let blocks = model::parse("# Heading\n\n> quoted\n\n- item\n\n| column |\n| --- |\n| cell |\n\nbefore ![alt](image.png) after", None, false);
        let mut paragraphs = HashMap::new();
        collect_blocks(&blocks, &mut paragraphs, &mut HashMap::new());
        for text in ["Heading", "quoted", "item", "column", "cell", "before ", " after"] {
            assert!(paragraphs.values().any(|rich| &*rich.text == text), "missing {text:?}");
        }
    }

    #[test]
    fn publication_is_exact_and_candidate_blocks_new_capture() {
        let entry = Rc::new(json!({"item": {"text": "latest"}}));
        let snapshot = Rc::new(ParsedSnapshot {
            revision: 7, epoch: 0, source: Arc::from("old"), replaces_previous: false,
            find_pattern: None, codes: HashMap::new(), interactive: false,
            blocks: Rc::new(Vec::new()), paragraphs: HashMap::new(),
        });
        let mut worker = MarkdownWorker {
            owner: Some(("row".into(), "host".into(), "agent".into())), linkify: true, candidate: Some(snapshot.clone()), ..MarkdownWorker::default()
        };
        assert!(worker.update(("row", "host", "agent"), entry, false, None, true, 0.0, None));
        assert!(worker.capture.is_none());
        assert!(worker.parser.is_none());
        assert!(!worker.publish(6));
        assert!(worker.publish(7));
        assert!(Rc::ptr_eq(&worker.published().unwrap(), &snapshot));
        assert!(worker.candidate().is_none());
        assert!(worker.is_pending(), "publishing an older candidate must still schedule latest demand");
    }

    #[test]
    fn owner_context_and_query_changes_advance_epoch_without_ui_parsing() {
        let entry = Rc::new(json!({"item": {"text": "a".repeat(CAPTURE_BYTES * 8)}}));
        let mut worker = MarkdownWorker::default();
        worker.update(("row", "host", "agent"), entry.clone(), false, None, true, 0.0, None);
        let first = worker.epoch();
        worker.update(("row", "other-host", "agent"), entry.clone(), false, None, true, 0.0, None);
        assert_ne!(worker.epoch(), first);
        let second = worker.epoch();
        let pattern = model::find_pattern("a").unwrap();
        worker.update(("row", "other-host", "agent"), entry, false, None, true, 0.0, Some(&pattern));
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
        let code = vec![Block::Code { lang: String::new(), text: text.clone() }];
        collect_blocks(&code, &mut paragraphs, &mut codes);
        let Block::Code { text: source, .. } = &code[0] else { unreachable!() };
        let prepared = codes.get(&(source.as_ptr() as usize)).unwrap();
        assert_eq!(&*prepared.text, text);
        assert_eq!(prepared.spans[0].len, text.len());
        assert!(prepared.spans[0].code);
    }

    #[test]
    fn copy_boundary_preserves_unicode_and_only_trims_trailing_newlines() {
        let text = "é漢 \n\n";
        let blocks = vec![Block::Code { lang: String::new(), text: text.into() }];
        let mut codes = HashMap::new();
        collect_blocks(&blocks, &mut HashMap::new(), &mut codes);
        let code = codes.values().next().unwrap();
        assert_eq!(&code.text[..code.copy_end], "é漢 ");
        assert_eq!(&*code.text, text);
        let paragraph = flatten(&[
            Inline::Text { text: "é漢 ".into(), style: Style::default() },
            Inline::Break,
            Inline::Break,
        ]);
        assert_eq!(&paragraph.text[..paragraph.copy_end], "é漢 ");
        assert_eq!(paragraph.copy_end, code.copy_end);
    }
}
