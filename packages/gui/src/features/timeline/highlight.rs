//! Syntax colors for fenced code and read/write tool content. Both share one
//! parser worker with a bounded latest-per-block queue, independent of Makepad
//! fonts and UI draw state.

use makepad_widgets::{Cx, DrawListId, ToUIReceiver, Vec4};
use paseo_highlight::{
    ColorScheme, Highlighter, Language, StyledRange, SyntaxThemeId, get_language_for_file,
    resolve_syntax_colors,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::{Rc, Weak};
#[cfg(not(target_arch = "wasm32"))]
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
    /// The draw list that last drew this block; a current result repaints it.
    owner: Option<DrawListId>,
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

    /// Stores a result for the current revision and returns the owner to
    /// repaint; an obsolete result changes nothing shown.
    fn complete(&mut self, key: u64, result: Option<RawColored>) -> Option<DrawListId> {
        if self.submitted == Some(key) {
            self.submitted = None;
        }
        if self.key != Some(key) {
            return None;
        }
        self.result = result.map(Rc::new);
        self.ready = true;
        self.owner
    }
}

/// Owned by a row/block, so completion cannot attach to a recycled row.
#[derive(Default)]
pub struct FenceRequest(Rc<RefCell<FenceSlot>>);

impl FenceRequest {
    pub fn is_pending(&self) -> bool {
        let slot = self.0.borrow();
        slot.key.is_some() && !slot.ready
    }
}

struct FenceWork {
    key: u64,
    code: String,
    language: Language,
    theme: String,
    dark: bool,
}

fn highlight(highlighter: &mut Option<Highlighter>, job: FenceWork) -> Option<RawColored> {
    highlighter
        .as_mut()
        .and_then(|h| h.highlight_spans(&job.code, job.language).ok())
        .map(|spans| resolve(spans, &job.theme, job.dark))
}

struct QueuedFence {
    slot: Weak<RefCell<FenceSlot>>,
    work: FenceWork,
}

struct FenceWorker {
    #[cfg(not(target_arch = "wasm32"))]
    sender: Option<SyncSender<FenceWork>>,
    receiver: ToUIReceiver<Option<RawColored>>,
    active: Option<(Weak<RefCell<FenceSlot>>, u64)>,
    pending: VecDeque<QueuedFence>,
    /// Owners of blocks turned away by a full queue, repainted to retry once it drains.
    deferred: Vec<DrawListId>,
}

impl FenceWorker {
    fn new() -> Self {
        let receiver = ToUIReceiver::default();
        #[cfg(not(target_arch = "wasm32"))]
        let sender = {
            let done = receiver.sender();
            let (sender, work) = sync_channel::<FenceWork>(1);
            let spawned = std::thread::Builder::new()
                .name("paseo-chat-highlight".into())
                .spawn(move || {
                    let mut highlighter = Highlighter::new().ok();
                    while let Ok(job) = work.recv() {
                        if done.send(highlight(&mut highlighter, job)).is_err() {
                            break;
                        }
                    }
                });
            spawned.ok().map(|_| sender)
        };
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            sender,
            receiver,
            active: None,
            pending: VecDeque::new(),
            deferred: Vec::new(),
        }
    }

    fn submit(&self, work: FenceWork) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.sender
                .as_ref()
                .is_some_and(|sender| sender.try_send(work).is_ok())
        }
        #[cfg(target_arch = "wasm32")]
        {
            thread_local! {
                static HIGHLIGHTER: RefCell<Option<Highlighter>> =
                    RefCell::new(Highlighter::new().ok());
            }
            let done = self.receiver.sender();
            crate::common::background::spawn("paseo-chat-highlight", move || {
                let result = HIGHLIGHTER.with(|h| highlight(&mut h.borrow_mut(), work));
                let _ = done.send(result);
            })
            .is_ok()
        }
    }

    /// `settled` receives owners of blocks completed as plain text without a parse.
    fn dispatch(&mut self, mut settled: impl FnMut(DrawListId)) {
        if self.active.is_some() {
            return;
        }
        while let Some(job) = self.pending.pop_front() {
            let Some(slot) = job.slot.upgrade() else {
                continue;
            };
            if slot.borrow().ready || slot.borrow().key != Some(job.work.key) {
                continue;
            }
            let key = job.work.key;
            if self.submit(job.work) {
                self.active = Some((job.slot, key));
                break;
            }
            // Match existing unsupported/parser-failure rendering: exact plain
            // text, without repeatedly scheduling a failed worker.
            let owner = slot.borrow_mut().complete(key, None);
            if let Some(owner) = owner {
                settled(owner);
            }
        }
    }
}

thread_local! {
    static FENCE_WORKER: RefCell<Option<FenceWorker>> = const { RefCell::new(None) };
}

/// Collects the single active parse and starts the newest queued snapshot.
/// Each current result repaints only the draw list that drew its block, including
/// settled rows; results for replaced revisions or dropped blocks repaint nothing.
pub fn poll(cx: &mut Cx) {
    FENCE_WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        let Some(worker) = worker.as_mut() else {
            return;
        };
        let mut received = false;
        while let Ok(result) = worker.receiver.try_recv() {
            received = true;
            let Some((slot, key)) = worker.active.take() else {
                continue;
            };
            let owner = slot
                .upgrade()
                .and_then(|slot| slot.borrow_mut().complete(key, result));
            if let Some(owner) = owner {
                cx.redraw_list(owner);
            }
        }
        if received {
            for owner in worker.deferred.drain(..) {
                cx.redraw_list(owner);
            }
        }
        worker.dispatch(|owner| cx.redraw_list(owner));
    })
}

/// MarkdownCodeBlock `_aliases` mapped onto the native grammar set.
fn fence_language(info: &str) -> Option<Language> {
    let first = info
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('.')
        .to_lowercase();
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
    let colors = resolve_syntax_colors(
        theme,
        if dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        },
    );
    spans
        .into_iter()
        .map(|s| (s.range, s.style.map(|role| color(colors.color(role)))))
        .collect()
}

fn key(code: &str, language: Language, theme: &str, dark: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    (language as u32).hash(&mut hasher);
    theme.hash(&mut hasher);
    dark.hash(&mut hasher);
    hasher.finish()
}

fn request_colors(
    request: &mut FenceRequest,
    owner: Option<DrawListId>,
    code: &str,
    language: Option<Language>,
    theme: &str,
    dark: bool,
) -> Option<Colored> {
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
        slot.owner = owner;
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
        let queued = worker
            .pending
            .iter()
            .position(|job| Weak::ptr_eq(&job.slot, &slot));
        if queued.is_none() && worker.pending.len() >= PENDING_FENCES {
            // Retried when the next result frees the queue.
            if let Some(owner) = owner
                && !worker.deferred.contains(&owner)
            {
                worker.deferred.push(owner);
            }
            return;
        }
        let job = QueuedFence {
            slot,
            work: FenceWork {
                key,
                code: code.to_owned(),
                language,
                theme: theme.to_owned(),
                dark,
            },
        };
        request.0.borrow_mut().submitted = Some(key);
        if let Some(index) = queued {
            worker.pending[index] = job;
        } else {
            worker.pending.push_back(job);
        }
        // Requested during a draw, where repaint requests are ignored: without a
        // working parser each block settles as it is requested and draws plain text now.
        worker.dispatch(|_| {});
    });
    None
}
/// Colors only for this source/language/theme revision. Pending revisions
/// draw selectable plain text, as file previews do while their worker runs.
/// `owner` is the draw list repainted when this revision's colors arrive.
pub fn fence(
    request: &mut FenceRequest,
    owner: Option<DrawListId>,
    code: &str,
    lang: &str,
    theme: &str,
    dark: bool,
) -> Option<Colored> {
    request_colors(request, owner, code, fence_language(lang), theme, dark)
}

/// Colors for file content by its path (`SourceController.languageFor`).
pub fn file(
    request: &mut FenceRequest,
    owner: Option<DrawListId>,
    code: &str,
    path: &str,
    theme: &str,
    dark: bool,
) -> Option<Colored> {
    request_colors(
        request,
        owner,
        code,
        get_language_for_file(path),
        theme,
        dark,
    )
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
