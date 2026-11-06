//! Syntax highlighting of source views on a native worker (`paseo-highlight`), replacing the
//! Flutter `SourceController` highlight pass. A document is parsed whole (never line by line);
//! results carry each line's fingerprint so a row edited since the parse draws plain until the
//! next result lands. One job per document runs at a time; the newest text is parsed next.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use makepad_widgets::*;
use paseo_highlight::{ColorScheme, HighlightStyle, SyntaxThemeId, resolve_syntax_colors};

use super::kit::hex_color;

/// One line's tokens as `(byte length, style)` runs.
pub type LineTokens = Vec<(usize, Option<HighlightStyle>)>;

pub struct Highlighted {
    pub revision: u64,
    lines: Vec<(u64, LineTokens)>,
}

impl Highlighted {
    /// The tokens of `line` if its text still matches what was parsed.
    pub fn line(&self, index: usize, text: &str) -> Option<&LineTokens> {
        let (fingerprint, tokens) = self.lines.get(index)?;
        (*fingerprint == line_fingerprint(text)).then_some(tokens)
    }
}

pub fn line_fingerprint(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

struct Done {
    key: String,
    revision: u64,
    result: Result<Vec<(u64, LineTokens)>, String>,
}

#[derive(Default)]
struct Doc {
    running: bool,
    next: Option<(u64, Arc<str>, String)>,
    result: Option<Rc<Highlighted>>,
}

use std::rc::Rc;

struct Worker {
    receiver: ToUIReceiver<Done>,
    docs: HashMap<String, Doc>,
}

thread_local! {
    static WORKER: RefCell<Worker> = RefCell::new(Worker { receiver: ToUIReceiver::default(), docs: HashMap::new() });
}

fn spawn(sender: ToUISender<Done>, key: String, revision: u64, text: Arc<str>, filename: String) {
    std::thread::Builder::new()
        .name("paseo-files-highlight".into())
        .spawn(move || {
            let result = paseo_highlight::highlight_code(&text, &filename)
                .map(|lines| {
                    let raw: Vec<&str> = text.split('\n').collect();
                    lines
                        .into_iter()
                        .enumerate()
                        .map(|(index, tokens)| {
                            let line = raw.get(index).copied().unwrap_or("");
                            let runs = tokens.iter().map(|token| (token.text.len(), token.style)).collect();
                            (line_fingerprint(line), runs)
                        })
                        .collect()
                })
                .map_err(|error| error.to_string());
            let _ = sender.send(Done { key, revision, result });
        })
        .ok();
}

/// Asks for tokens of `text` at `revision` (no-op when that revision is parsed or queued).
pub fn request(key: &str, revision: u64, text: &str, filename: &str) {
    WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        let sender = worker.receiver.sender();
        let doc = worker.docs.entry(key.to_owned()).or_default();
        if doc.result.as_ref().is_some_and(|r| r.revision == revision)
            || doc.next.as_ref().is_some_and(|(r, _, _)| *r == revision)
        {
            return;
        }
        let text: Arc<str> = Arc::from(text);
        if doc.running {
            doc.next = Some((revision, text, filename.to_owned()));
            return;
        }
        doc.running = true;
        spawn(sender, key.to_owned(), revision, text, filename.to_owned());
    });
}

pub fn result(key: &str) -> Option<Rc<Highlighted>> {
    WORKER.with(|worker| worker.borrow().docs.get(key).and_then(|doc| doc.result.clone()))
}

pub fn forget(key: &str) {
    WORKER.with(|worker| {
        worker.borrow_mut().docs.remove(key);
    });
}

/// Collects finished parses; returns whether any arrived.
pub fn poll() -> bool {
    WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        let sender = worker.receiver.sender();
        let mut any = false;
        while let Ok(done) = worker.receiver.try_recv() {
            let Some(doc) = worker.docs.get_mut(&done.key) else { continue };
            any = true;
            doc.running = false;
            if let Ok(lines) = done.result {
                doc.result = Some(Rc::new(Highlighted { revision: done.revision, lines }));
            }
            if let Some((revision, text, filename)) = doc.next.take() {
                doc.running = true;
                spawn(sender.clone(), done.key.clone(), revision, text, filename);
            }
        }
        any
    })
}

/// The role colours of the `syntaxTheme` preference (default `one`) for the app brightness.
pub fn colors(theme: &str, dark: bool) -> [Vec4; 20] {
    let id = SyntaxThemeId::from_name(theme).unwrap_or(SyntaxThemeId::One);
    let scheme = if dark { ColorScheme::Dark } else { ColorScheme::Light };
    let palette = resolve_syntax_colors(id, scheme);
    let mut out = [Vec4::default(); 20];
    for role in HighlightStyle::ALL {
        out[role as usize] = hex_color(palette.color(role));
    }
    out
}
