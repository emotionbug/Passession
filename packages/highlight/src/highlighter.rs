use crate::{HighlightStyle, HighlightToken, Language, Parser, get_language_for_file};
use arborium::advanced::{GrammarConfig, Span};
use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::BTreeSet,
    fmt,
    num::NonZeroUsize,
    ops::Range,
    sync::{
        Arc, LazyLock,
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    task::Poll,
};

#[derive(Debug)]
pub enum Error {
    Grammar(String),
    Highlight(arborium::Error),
    InputTooLarge,
    InvalidParserRange { start: usize, end: usize },
    Worker(std::io::Error),
    WorkerDisconnected,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Grammar(message) => write!(f, "cannot initialize syntax grammar: {message}"),
            Self::Highlight(error) => error.fmt(f),
            Self::InputTooLarge => {
                f.write_str("source exceeds the native parser's 32-bit byte range")
            }
            Self::InvalidParserRange { start, end } => {
                write!(f, "parser emitted invalid UTF-8 range {start}..{end}")
            }
            Self::Worker(error) => write!(f, "cannot spawn highlight worker: {error}"),
            Self::WorkerDisconnected => f.write_str("highlight worker stopped before completion"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Highlight(error) => Some(error),
            Self::Worker(error) => Some(error),
            _ => None,
        }
    }
}
impl From<arborium::Error> for Error {
    fn from(error: arborium::Error) -> Self {
        Self::Highlight(error)
    }
}

fn register(
    store: &arborium::GrammarStore,
    name: &str,
    language: arborium::tree_sitter::Language,
    highlights_query: &str,
    injections_query: &str,
    locals_query: &str,
) -> Result<(), String> {
    let grammar = Parser::new(GrammarConfig {
        language,
        highlights_query,
        injections_query,
        locals_query,
    })
    .map_err(|error| format!("{name}: {error}"))?;
    store.insert(name, Arc::new(grammar));
    Ok(())
}

fn store() -> Result<Arc<arborium::GrammarStore>, Error> {
    static STORE: LazyLock<Result<Arc<arborium::GrammarStore>, String>> = LazyLock::new(|| {
        let store = Arc::new(arborium::GrammarStore::new());
        for (name, language, highlights, injections, locals) in [
            (
                "javascript",
                arborium::lang_javascript::language(),
                arborium::lang_javascript::HIGHLIGHTS_QUERY,
                arborium::lang_javascript::INJECTIONS_QUERY,
                arborium::lang_javascript::LOCALS_QUERY,
            ),
            (
                "tsx",
                arborium::lang_tsx::language(),
                &arborium::lang_tsx::HIGHLIGHTS_QUERY,
                arborium::lang_tsx::INJECTIONS_QUERY,
                arborium::lang_tsx::LOCALS_QUERY,
            ),
        ] {
            register(
                &store,
                name,
                language.into(),
                &format!("{highlights}\n{}", include_str!("jsx.scm")),
                injections,
                locals,
            )?;
        }
        // Preserve interpolation openers even when Vue's raw-text scanner
        // recovers through an embedded JavaScript string as malformed HTML.
        register(
            &store,
            "vue",
            arborium::lang_vue::language().into(),
            &format!(
                "{}\n{}",
                &*arborium::lang_vue::HIGHLIGHTS_QUERY,
                "\"{{\" @paseo.interpolation",
            ),
            arborium::lang_vue::INJECTIONS_QUERY,
            arborium::lang_vue::LOCALS_QUERY,
        )?;
        register(
            &store,
            "astro",
            tree_sitter_astro_next::LANGUAGE.into(),
            &tree_sitter_astro_next::HIGHLIGHTS_QUERY.replace("@property", "@attribute"),
            tree_sitter_astro_next::INJECTIONS_QUERY,
            "",
        )?;
        // The block-only grammar cannot highlight links, inline code or embedded
        // HTML. Register both upstream Markdown grammars for nested injections.
        register(
            &store,
            "markdown",
            tree_sitter_md::LANGUAGE.into(),
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK,
            "",
        )?;
        register(
            &store,
            "markdown_inline",
            tree_sitter_md::INLINE_LANGUAGE.into(),
            tree_sitter_md::HIGHLIGHT_QUERY_INLINE,
            tree_sitter_md::INJECTION_QUERY_INLINE,
            "",
        )?;
        // Keep CSS selectors distinct from property declarations, as in Paseo.
        register(
            &store,
            "css",
            arborium::lang_css::language().into(),
            &arborium::lang_css::HIGHLIGHTS_QUERY
                .replace("(class_name) @property", "(class_name) @class"),
            arborium::lang_css::INJECTIONS_QUERY,
            arborium::lang_css::LOCALS_QUERY,
        )?;
        register(
            &store,
            "scss",
            arborium::lang_scss::language().into(),
            &arborium::lang_scss::HIGHLIGHTS_QUERY
                .replace("(class_name) @property", "(class_name) @class"),
            arborium::lang_scss::INJECTIONS_QUERY,
            arborium::lang_scss::LOCALS_QUERY,
        )?;
        Ok(store)
    });
    STORE
        .as_ref()
        .map(Arc::clone)
        .map_err(|message| Error::Grammar(message.clone()))
}

/// Return the actual compiled native grammar (or `None` for an unknown suffix).
/// Grammar initialization errors are not mistaken for unsupported files.
pub fn get_parser_for_file(filename: &str) -> Result<Option<Arc<Parser>>, Error> {
    let Some(language) = get_language_for_file(filename) else {
        return Ok(None);
    };
    store()?
        .get(language.grammar_name())
        .map(Some)
        .ok_or_else(|| Error::Grammar(language.name().into()))
}

/// A non-overlapping half-open UTF-8 byte range; plain regions have no style.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledRange {
    pub range: Range<usize>,
    pub style: Option<HighlightStyle>,
}

/// Reusable native parser context with a shared, lazily compiled grammar cache.
pub struct Highlighter {
    inner: arborium::Highlighter,
}

impl Highlighter {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            inner: arborium::Highlighter::with_store_and_config(
                store()?,
                arborium::Config {
                    max_injection_depth: 64,
                    ..Default::default()
                },
            ),
        })
    }

    /// A separate parser context that shares already-compiled grammar queries.
    pub fn fork(&self) -> Self {
        Self {
            inner: self.inner.fork(),
        }
    }

    /// Highlight an explicitly selected language. This does not fall back to text.
    pub fn highlight_spans(
        &mut self,
        code: &str,
        language: Language,
    ) -> Result<Vec<StyledRange>, Error> {
        if code.len() > u32::MAX as usize {
            return Err(Error::InputTooLarge);
        }
        let spans = if language == Language::Vue {
            crate::vue::highlight(&mut self.inner, code)?
        } else {
            self.inner.highlight_spans(language.grammar_name(), code)?
        };
        flatten(code, spans)
    }

    pub fn highlight_code<'a>(
        &mut self,
        code: &'a str,
        filename: &str,
    ) -> Result<Vec<Vec<HighlightToken<'a>>>, Error> {
        let spans = self.ranges_for_file(code, filename)?;
        Ok(lines(code, &spans).collect())
    }

    /// Isolated-line convenience. Use `highlight_code` for context inside a
    /// multi-line string, comment, template or injected language.
    pub fn highlight_line<'a>(
        &mut self,
        code: &'a str,
        filename: &str,
    ) -> Result<Vec<HighlightToken<'a>>, Error> {
        let spans = self.ranges_for_file(code, filename)?;
        Ok(lines(code, &spans).next().unwrap_or_default())
    }

    fn ranges_for_file(&mut self, code: &str, filename: &str) -> Result<Vec<StyledRange>, Error> {
        match get_language_for_file(filename) {
            Some(language) => self.highlight_spans(code, language),
            None => Ok(vec![StyledRange {
                range: 0..code.len(),
                style: None,
            }]),
        }
    }
}

thread_local! { static LOCAL: RefCell<Option<Highlighter>> = const { RefCell::new(None) }; }

fn with_highlighter<T>(run: impl FnOnce(&mut Highlighter) -> Result<T, Error>) -> Result<T, Error> {
    LOCAL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Highlighter::new()?);
        }
        run(slot.as_mut().expect("initialized above"))
    })
}

pub fn highlight_code<'a>(
    code: &'a str,
    filename: &str,
) -> Result<Vec<Vec<HighlightToken<'a>>>, Error> {
    with_highlighter(|highlighter| highlighter.highlight_code(code, filename))
}

pub fn highlight_line<'a>(code: &'a str, filename: &str) -> Result<Vec<HighlightToken<'a>>, Error> {
    with_highlighter(|highlighter| highlighter.highlight_line(code, filename))
}

// Sweep parser spans rather than allocating a style entry per byte. Later-starting,
// shorter (inner) spans win; equal ranges prefer later query patterns. The final
// index breaks ties in favor of injected captures, which the engine appends last.
fn flatten(code: &str, spans: Vec<Span>) -> Result<Vec<StyledRange>, Error> {
    let mut events = Vec::with_capacity(spans.len() * 2);
    let styles: Vec<_> = spans
        .iter()
        .map(|span| HighlightStyle::from_capture(&span.capture))
        .collect();
    for (index, span) in spans.iter().enumerate() {
        let start = span.start as usize;
        let end = span.end as usize;
        if start > end
            || end > code.len()
            || !code.is_char_boundary(start)
            || !code.is_char_boundary(end)
        {
            return Err(Error::InvalidParserRange { start, end });
        }
        if start == end || styles[index].is_none() {
            continue;
        }
        let key = (start, Reverse(end), span.pattern_index, index);
        events.push((start, true, key));
        events.push((end, false, key));
    }
    events.sort_unstable();
    let mut active = BTreeSet::new();
    let mut result = Vec::new();
    let mut cursor = 0;
    for (offset, start, key) in events {
        let style = active.last().and_then(|&(_, _, _, index)| styles[index]);
        append_range(&mut result, cursor..offset, style);
        if start {
            active.insert(key);
        } else {
            active.remove(&key);
        }
        cursor = offset;
    }
    append_range(&mut result, cursor..code.len(), None);
    Ok(result)
}

fn append_range(ranges: &mut Vec<StyledRange>, range: Range<usize>, style: Option<HighlightStyle>) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = ranges.last_mut()
        && last.style == style && last.range.end == range.start
    {
        last.range.end = range.end;
        return;
    }
    ranges.push(StyledRange { range, style });
}

fn lines<'a, 'r>(
    code: &'a str,
    ranges: &'r [StyledRange],
) -> impl Iterator<Item = Vec<HighlightToken<'a>>> + use<'a, 'r> {
    let mut offset = 0;
    let mut span_index = 0;
    code.split('\n').map(move |line| {
        let end = offset + line.len();
        let mut cursor = offset;
        let mut tokens: Vec<HighlightToken<'a>> = Vec::new();
        while cursor < end {
            while span_index < ranges.len() && ranges[span_index].range.end <= cursor {
                span_index += 1;
            }
            let (stop, style) = match ranges.get(span_index) {
                Some(span) if span.range.start <= cursor => (end.min(span.range.end), span.style),
                Some(span) => (end.min(span.range.start), None),
                None => (end, None),
            };
            if let Some(last) = tokens.last_mut().filter(|last| last.style == style) {
                let from = cursor - last.text.len();
                last.text = &code[from..stop];
            } else {
                tokens.push(HighlightToken {
                    text: &code[cursor..stop],
                    style,
                });
            }
            cursor = stop;
        }
        if tokens.is_empty() {
            tokens.push(HighlightToken {
                text: line,
                style: None,
            });
        }
        offset = end + 1;
        tokens
    })
}

/// Returns `None` for out-of-range offsets or an offset inside a UTF-8 scalar.
pub fn byte_to_utf16(source: &str, byte: usize) -> Option<usize> {
    source
        .get(..byte)
        .map(|prefix| prefix.encode_utf16().count())
}

/// Returns `None` for out-of-range offsets or the middle of a surrogate pair.
pub fn utf16_to_byte(source: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in source.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += ch.len_utf16();
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(source.len())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedHighlightToken {
    pub text: String,
    pub style: Option<HighlightStyle>,
}

impl From<HighlightToken<'_>> for OwnedHighlightToken {
    fn from(token: HighlightToken<'_>) -> Self {
        Self {
            text: token.text.into(),
            style: token.style,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HighlightStepsOptions {
    /// Optional prefix preview; the remaining lines stay plain in the caller's
    /// display until `Lines` arrive. Preview is produced only for a small prefix
    /// (less than half the document), and may change after the complete parse.
    pub first_lines: usize,
    /// Maximum rendered lines per delivered batch. Parsing runs on the worker.
    pub lines_per_step: NonZeroUsize,
}
impl Default for HighlightStepsOptions {
    fn default() -> Self {
        Self {
            first_lines: 0,
            lines_per_step: NonZeroUsize::new(200).unwrap(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HighlightStep {
    /// Speculative first lines; final `Lines` replace them, not append to them.
    Preview {
        lines: Vec<Vec<OwnedHighlightToken>>,
    },
    /// Final tokens at zero-based line indices. Batches are ordered and disjoint.
    Lines {
        first_line: usize,
        lines: Vec<Vec<OwnedHighlightToken>>,
    },
    Finished {
        line_count: usize,
    },
}

/// Native incremental-delivery handle. `poll_next` never waits on the parser;
/// `Iterator::next` waits for the next batch for command-line consumers. The
/// bounded channel provides backpressure. Dropping the handle cancels delivery;
/// an in-progress native parse can finish, but no caller thread waits for it.
/// Unlike independent line parsing, final batches always use the complete tree.
pub struct HighlightSteps {
    receiver: Receiver<Result<HighlightStep, Error>>,
    finished: bool,
}
impl HighlightSteps {
    pub fn poll_next(&mut self) -> Poll<Option<Result<HighlightStep, Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        match self.receiver.try_recv() {
            Ok(step) => Poll::Ready(Some(self.accept(step))),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Poll::Ready(Some(Err(Error::WorkerDisconnected)))
            }
        }
    }
    fn accept(&mut self, step: Result<HighlightStep, Error>) -> Result<HighlightStep, Error> {
        if matches!(step, Ok(HighlightStep::Finished { .. }) | Err(_)) {
            self.finished = true;
        }
        step
    }
}
impl Iterator for HighlightSteps {
    type Item = Result<HighlightStep, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.receiver.recv() {
            Ok(step) => Some(self.accept(step)),
            Err(_) => {
                self.finished = true;
                Some(Err(Error::WorkerDisconnected))
            }
        }
    }
}
impl std::iter::FusedIterator for HighlightSteps {}

/// Starts native work without parsing on the caller's thread. Pass an `Arc<str>`
/// to share an existing immutable document without copying it. This is scheduled
/// highlighting, not an edit-delta parser API; each document is parsed as a whole.
pub fn highlight_code_in_steps(
    code: impl Into<Arc<str>>,
    filename: impl Into<String>,
    options: HighlightStepsOptions,
) -> Result<HighlightSteps, Error> {
    let code = code.into();
    if code.len() > u32::MAX as usize {
        return Err(Error::InputTooLarge);
    }
    let filename = filename.into();
    let (sender, receiver) = mpsc::sync_channel(2);
    std::thread::Builder::new()
        .name("paseo-highlight".into())
        .spawn(move || {
            if let Err(error) = run_steps(&code, &filename, options, &sender) {
                let _ = sender.send(Err(error));
            }
        })
        .map_err(Error::Worker)?;
    Ok(HighlightSteps {
        receiver,
        finished: false,
    })
}

fn run_steps(
    code: &str,
    filename: &str,
    options: HighlightStepsOptions,
    sender: &SyncSender<Result<HighlightStep, Error>>,
) -> Result<(), Error> {
    let mut highlighter = Highlighter::new()?;
    let line_count = code.split('\n').count();
    if options.first_lines > 0 && options.first_lines < line_count / 2 {
        let end = code
            .match_indices('\n')
            .nth(options.first_lines - 1)
            .map(|(offset, _)| offset)
            .unwrap_or(code.len());
        let preview = highlighter
            .highlight_code(&code[..end], filename)?
            .into_iter()
            .map(|line| line.into_iter().map(Into::into).collect())
            .collect();
        if sender
            .send(Ok(HighlightStep::Preview { lines: preview }))
            .is_err()
        {
            return Ok(());
        }
    }
    let spans = highlighter.ranges_for_file(code, filename)?;
    let mut batch = Vec::new();
    let mut first_line = 0;
    for line in lines(code, &spans) {
        batch.push(line.into_iter().map(Into::into).collect());
        if batch.len() == options.lines_per_step.get() {
            let count = batch.len();
            if sender
                .send(Ok(HighlightStep::Lines {
                    first_line,
                    lines: std::mem::take(&mut batch),
                }))
                .is_err()
            {
                return Ok(());
            }
            first_line += count;
        }
    }
    if !batch.is_empty()
        && sender
            .send(Ok(HighlightStep::Lines {
                first_line,
                lines: batch,
            }))
            .is_err()
    {
        return Ok(());
    }
    let _ = sender.send(Ok(HighlightStep::Finished { line_count }));
    Ok(())
}
