//! Syntax colors for fenced code and read/write tool content. Both use one
//! native parser worker with a bounded latest-per-block queue; UI font shaping
//! stays on the drawing thread.

use makepad_widgets::{ToUIReceiver, Vec4};
use paseo_highlight::{ColorScheme, Highlighter, Language, StyledRange, SyntaxThemeId, get_language_for_file, resolve_syntax_colors};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{SyncSender, sync_channel};

/// Content longer than this renders as plain (selectable) text.
pub const HIGHLIGHT_LIMIT: usize = 100_000;

/// One colored byte range of the source (None = foreground).
pub type Colored = Rc<Vec<(std::ops::Range<usize>, Option<Vec4>)>>;


type RawColored = Vec<(std::ops::Range<usize>, Option<Vec4>)>;
const PENDING_FENCES: usize = 16;

#[derive(Default)]
struct FenceSlot {
    key: Option<u64>,
    submitted: Option<u64>,
    ready: bool,
    result: Option<Colored>,
}

impl FenceSlot {
    fn request(&mut self, key: u64) {
        if self.key != Some(key) {
            self.key = Some(key);
            self.submitted = None;
            self.ready = false;
            self.result = None;
        }
    }

    fn complete(&mut self, key: u64, result: Option<RawColored>) {
        if self.submitted == Some(key) {
            self.submitted = None;
        }
        if self.key == Some(key) {
            self.result = result.map(Rc::new);
            self.ready = true;
        }
    }
}

/// Owned by a row/block, so completion cannot attach to a recycled row.
#[derive(Default)]
pub struct FenceRequest(Rc<RefCell<FenceSlot>>);

struct FenceWork {
    key: u64,
    code: String,
    language: Language,
    theme: String,
    dark: bool,
}

struct QueuedFence {
    slot: Weak<RefCell<FenceSlot>>,
    work: FenceWork,
}

struct FenceWorker {
    sender: Option<SyncSender<FenceWork>>,
    receiver: ToUIReceiver<Option<RawColored>>,
    active: Option<(Weak<RefCell<FenceSlot>>, u64)>,
    pending: VecDeque<QueuedFence>,
}

impl FenceWorker {
    fn new() -> Self {
        let receiver = ToUIReceiver::default();
        let done = receiver.sender();
        let (sender, work) = sync_channel::<FenceWork>(1);
        let spawned = std::thread::Builder::new().name("paseo-chat-highlight".into()).spawn(move || {
            let mut highlighter = Highlighter::new().ok();
            while let Ok(job) = work.recv() {
                let result = highlighter.as_mut()
                    .and_then(|h| h.highlight_spans(&job.code, job.language).ok())
                    .map(|spans| resolve(spans, &job.theme, job.dark));
                if done.send(result).is_err() {
                    break;
                }
            }
        });
        Self {
            sender: spawned.ok().map(|_| sender),
            receiver,
            active: None,
            pending: VecDeque::new(),
        }
    }

    fn dispatch(&mut self) {
        if self.active.is_some() {
            return;
        }
        while let Some(job) = self.pending.pop_front() {
            let Some(slot) = job.slot.upgrade() else { continue };
            if slot.borrow().ready || slot.borrow().key != Some(job.work.key) {
                continue;
            }
            let key = job.work.key;
            if self.sender.as_ref().is_some_and(|sender| sender.try_send(job.work).is_ok()) {
                self.active = Some((job.slot, key));
                break;
            }
            // Match existing unsupported/parser-failure rendering: exact plain
            // text, without repeatedly scheduling a failed worker.
            slot.borrow_mut().complete(key, None);
        }
    }
}

thread_local! {
    static FENCE_WORKER: RefCell<Option<FenceWorker>> = const { RefCell::new(None) };
}

/// Collects the single active parse and starts the newest queued snapshot.
/// The timeline signal handler redraws after completion, including settled rows.
pub fn poll() -> bool {
    FENCE_WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        let Some(worker) = worker.as_mut() else { return false };
        let mut changed = false;
        while let Ok(result) = worker.receiver.try_recv() {
            changed = true;
            if let Some((slot, key)) = worker.active.take()
                && let Some(slot) = slot.upgrade()
            {
                slot.borrow_mut().complete(key, result);
            }
        }
        worker.dispatch();
        changed
    })
}

/// MarkdownCodeBlock `_aliases` mapped onto the native grammar set.
fn fence_language(info: &str) -> Option<Language> {
    let first = info.split_whitespace().next().unwrap_or("").trim_start_matches('.').to_lowercase();
    if first.is_empty() {
        return None;
    }
    let extension = match first.as_str() {
        "typescript" => "ts",
        "javascript" => "js",
        "python" => "py",
        "rust" => "rs",
        "golang" => "go",
        "c++" | "cc" | "cxx" => "cpp",
        "c#" | "csharp" => "cs",
        "objc" | "objective-c" | "objectivec" => "m",
        "markdown" => "md",
        "elixir" => "ex",
        "kotlin" | "kt" => "kt",
        "yml" => "yaml",
        // chat_markdown.dart `_aliases`: shell fences highlight as bash.
        "sh" | "shell" | "zsh" | "console" => "bash",
        other => other,
    };
    get_language_for_file(&format!("x.{extension}"))
}

fn color(hex: &str) -> Vec4 {
    super::canvas::hex_color(hex)
}

fn resolve(spans: Vec<StyledRange>, theme: &str, dark: bool) -> RawColored {
    let theme = SyntaxThemeId::from_name(theme).unwrap_or(SyntaxThemeId::One);
    let colors = resolve_syntax_colors(theme, if dark { ColorScheme::Dark } else { ColorScheme::Light });
    spans.into_iter().map(|s| (s.range, s.style.map(|role| color(colors.color(role))))).collect()
}


fn key(code: &str, language: Language, theme: &str, dark: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    (language as u32).hash(&mut hasher);
    theme.hash(&mut hasher);
    dark.hash(&mut hasher);
    hasher.finish()
}

fn request_colors(request: &mut FenceRequest, code: &str, language: Option<Language>, theme: &str, dark: bool) -> Option<Colored> {
    if code.len() > HIGHLIGHT_LIMIT {
        *request.0.borrow_mut() = FenceSlot::default();
        return None;
    }
    let Some(language) = language else {
        *request.0.borrow_mut() = FenceSlot::default();
        return None;
    };
    let key = key(code, language, theme, dark);
    {
        let mut slot = request.0.borrow_mut();
        slot.request(key);
        if slot.ready {
            return slot.result.clone();
        }
        if slot.submitted == Some(key) {
            return None;
        }
    }
    FENCE_WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        let worker = worker.get_or_insert_with(FenceWorker::new);
        let slot = Rc::downgrade(&request.0);
        worker.pending.retain(|job| job.slot.strong_count() > 0);
        let queued = worker.pending.iter().position(|job| Weak::ptr_eq(&job.slot, &slot));
        if queued.is_none() && worker.pending.len() >= PENDING_FENCES {
            return;
        }
        let job = QueuedFence {
            slot,
            work: FenceWork { key, code: code.to_owned(), language, theme: theme.to_owned(), dark },
        };
        request.0.borrow_mut().submitted = Some(key);
        if let Some(index) = queued {
            worker.pending[index] = job;
        } else {
            worker.pending.push_back(job);
        }
        worker.dispatch();
    });
    None
}
/// Colors only for this source/language/theme revision. Pending revisions
/// draw selectable plain text, as file previews do while their worker runs.
pub fn fence(request: &mut FenceRequest, code: &str, lang: &str, theme: &str, dark: bool) -> Option<Colored> {
    request_colors(request, code, fence_language(lang), theme, dark)
}


/// Colors for file content by its path (`SourceController.languageFor`).
pub fn file(request: &mut FenceRequest, code: &str, path: &str, theme: &str, dark: bool) -> Option<Colored> {
    request_colors(request, code, get_language_for_file(path), theme, dark)
}

/// cpp/highlight/CodeBlockLanguages.cpp displayNameForLanguage.
pub fn display_language_name(info: &str) -> String {
    let language = info.split_whitespace().next().unwrap_or("");
    if language.is_empty() {
        return String::new();
    }
    let lower = language.to_lowercase();
    let known = match lower.as_str() {
        "bash" => "Bash",
        "c" => "C",
        "cc" | "cpp" | "cxx" => "C++",
        "cs" | "csharp" => "C#",
        "css" => "CSS",
        "dockerfile" => "Dockerfile",
        "go" | "golang" => "Go",
        "graphql" => "GraphQL",
        "html" => "HTML",
        "java" => "Java",
        "javascript" | "js" => "JavaScript",
        "json" => "JSON",
        "jsx" => "JSX",
        "markdown" | "md" => "Markdown",
        "objc" | "objectivec" => "Objective-C",
        "php" => "PHP",
        "py" | "python" => "Python",
        "rb" | "ruby" => "Ruby",
        "rs" | "rust" => "Rust",
        "scss" => "SCSS",
        "sh" | "shell" => "Shell",
        "sql" => "SQL",
        "swift" => "Swift",
        "toml" => "TOML",
        "ts" | "typescript" => "TypeScript",
        "tsx" => "TSX",
        "xml" => "XML",
        "yaml" | "yml" => "YAML",
        "zsh" => "Zsh",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_owned();
    }
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obsolete_fence_result_cannot_color_current_revision() {
        let mut slot = FenceSlot::default();
        slot.request(1);
        slot.submitted = Some(1);
        slot.request(2);
        slot.submitted = Some(2);
        slot.complete(1, Some(vec![(0..12, Some(Vec4::default()))]));
        assert!(!slot.ready);
        assert!(slot.result.is_none());
        assert_eq!(slot.submitted, Some(2));

        slot.complete(2, Some(vec![(0..3, None)]));
        assert!(slot.ready);
        assert_eq!(slot.result.as_deref().unwrap(), &vec![(0..3, None)]);
        assert_eq!(slot.submitted, None);
    }

    #[test]
    fn returning_to_an_older_source_resubmits_after_its_result_was_replaced() {
        let mut slot = FenceSlot::default();
        slot.request(1);
        slot.submitted = Some(1);
        slot.complete(1, Some(vec![(0..3, None)]));
        slot.request(2);
        assert!(slot.result.is_none());
        slot.request(1);
        assert!(!slot.ready);
        assert_eq!(slot.submitted, None);
        slot.complete(2, Some(vec![(0..7, None)]));
        assert!(!slot.ready);
        assert!(slot.result.is_none());
    }
}
