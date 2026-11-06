//! Syntax colors for fenced code (chat_code.dart / MarkdownCodeBlock) and
//! read/write tool content (`_HighlightedLines`), through the native
//! `paseo-highlight` parsers and the user's `syntaxTheme` preference. Parses
//! are cached (48 most recent) so a redraw never re-runs a parser.

use makepad_widgets::Vec4;
use paseo_highlight::{ColorScheme, Highlighter, Language, StyledRange, SyntaxThemeId, get_language_for_file, resolve_syntax_colors};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

/// Content longer than this renders as plain (selectable) text.
pub const HIGHLIGHT_LIMIT: usize = 100_000;
const CACHE: usize = 48;

/// One colored byte range of the source (None = foreground).
pub type Colored = Rc<Vec<(std::ops::Range<usize>, Option<Vec4>)>>;

#[derive(Default)]
struct Cache {
    highlighter: Option<Highlighter>,
    failed: bool,
    entries: VecDeque<(u64, Colored)>,
}

thread_local! {
    static CACHE_CELL: RefCell<Cache> = RefCell::new(Cache::default());
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

fn compute(code: &str, language: Language, theme: &str, dark: bool) -> Option<Colored> {
    CACHE_CELL.with(|cell| {
        let cache = &mut *cell.borrow_mut();
        if cache.highlighter.is_none() && !cache.failed {
            match Highlighter::new() {
                Ok(h) => cache.highlighter = Some(h),
                Err(_) => cache.failed = true,
            }
        }
        let highlighter = cache.highlighter.as_mut()?;
        let spans: Vec<StyledRange> = highlighter.highlight_spans(code, language).ok()?;
        let theme = SyntaxThemeId::from_name(theme).unwrap_or(SyntaxThemeId::One);
        let colors = resolve_syntax_colors(theme, if dark { ColorScheme::Dark } else { ColorScheme::Light });
        Some(Rc::new(spans.into_iter().map(|s| (s.range, s.style.map(|role| color(colors.color(role))))).collect()))
    })
}

fn cached(key: u64, make: impl FnOnce() -> Option<Colored>) -> Option<Colored> {
    if let Some(hit) = CACHE_CELL.with(|cell| cell.borrow().entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())) {
        return Some(hit);
    }
    let value = make()?;
    CACHE_CELL.with(|cell| {
        let entries = &mut cell.borrow_mut().entries;
        entries.push_back((key, value.clone()));
        while entries.len() > CACHE {
            entries.pop_front();
        }
    });
    Some(value)
}

fn key(code: &str, language: Language, theme: &str, dark: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    code.hash(&mut hasher);
    (language as u32).hash(&mut hasher);
    theme.hash(&mut hasher);
    dark.hash(&mut hasher);
    hasher.finish()
}

/// Colors for a fenced block (`lang` = the fence info string).
pub fn fence(code: &str, lang: &str, theme: &str, dark: bool) -> Option<Colored> {
    if code.len() > HIGHLIGHT_LIMIT {
        return None;
    }
    let language = fence_language(lang)?;
    cached(key(code, language, theme, dark), || compute(code, language, theme, dark))
}

/// Colors for file content by its path (`SourceController.languageFor`).
pub fn file(code: &str, path: &str, theme: &str, dark: bool) -> Option<Colored> {
    if code.len() > HIGHLIGHT_LIMIT {
        return None;
    }
    let language = get_language_for_file(path)?;
    cached(key(code, language, theme, dark), || compute(code, language, theme, dark))
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
