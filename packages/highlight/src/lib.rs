//! Native, parser-backed syntax highlighting with Paseo's twenty semantic roles.
//!
//! [`highlight_code`] returns borrowed per-line tokens, preserving every source byte
//! except the separating LF. CRLF, trailing empty lines, and non-ASCII text are
//! retained. All ranges are **UTF-8 byte offsets**, not JavaScript UTF-16 indices;
//! [`byte_to_utf16`] and [`utf16_to_byte`] are explicit boundary-safe adapters.
//!
//! Reuse [`Highlighter`] to cache parsers. [`highlight_code_in_steps`] moves parsing
//! to a native worker and delivers a preview and bounded line batches without
//! blocking the caller on parsing. It does not parse each line in isolation.
//! Unknown extensions are plain text; a supported parser failure is an error,
//! never a silent plain-text fallback.
//!
//! Grammars and injection queries are compiled from published native sources by
//! Cargo. There is no JavaScript runtime, grammar generator, or network access at
//! runtime. Theme colors apply only to syntax, never the editor frame/background.

mod highlighter;
mod languages;
mod themes;

pub use arborium::advanced::CompiledGrammar as Parser;
pub use highlighter::{
    Error, HighlightStep, HighlightSteps, HighlightStepsOptions, Highlighter, OwnedHighlightToken,
    StyledRange, byte_to_utf16, get_parser_for_file, highlight_code, highlight_code_in_steps,
    highlight_line, utf16_to_byte,
};
pub use languages::{
    Language, get_language_for_file, get_supported_extensions, is_language_supported,
};
pub use themes::{
    ColorScheme, DARK_HIGHLIGHT_COLORS, LIGHT_HIGHLIGHT_COLORS, SYNTAX_THEME_IDS,
    SYNTAX_THEME_OPTIONS, SyntaxColors, SyntaxThemeId, SyntaxThemeOption, create_highlight_style,
    is_syntax_theme_id, resolve_syntax_colors,
};

/// Stable semantic style names, in the same order as the original SDK.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum HighlightStyle {
    Keyword,
    Comment,
    String,
    Number,
    Literal,
    Function,
    Definition,
    Class,
    Type,
    Tag,
    Attribute,
    Property,
    Variable,
    Operator,
    Punctuation,
    Regexp,
    Escape,
    Meta,
    Heading,
    Link,
}

impl HighlightStyle {
    pub const ALL: [Self; 20] = [
        Self::Keyword,
        Self::Comment,
        Self::String,
        Self::Number,
        Self::Literal,
        Self::Function,
        Self::Definition,
        Self::Class,
        Self::Type,
        Self::Tag,
        Self::Attribute,
        Self::Property,
        Self::Variable,
        Self::Operator,
        Self::Punctuation,
        Self::Regexp,
        Self::Escape,
        Self::Meta,
        Self::Heading,
        Self::Link,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Comment => "comment",
            Self::String => "string",
            Self::Number => "number",
            Self::Literal => "literal",
            Self::Function => "function",
            Self::Definition => "definition",
            Self::Class => "class",
            Self::Type => "type",
            Self::Tag => "tag",
            Self::Attribute => "attribute",
            Self::Property => "property",
            Self::Variable => "variable",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Regexp => "regexp",
            Self::Escape => "escape",
            Self::Meta => "meta",
            Self::Heading => "heading",
            Self::Link => "link",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == name)
    }

    pub(crate) fn from_capture(capture: &str) -> Option<Self> {
        use HighlightStyle::*;
        if capture.starts_with('_') {
            return None;
        }
        // More specific captures must precede their enclosing family.
        if capture.contains(".escape") {
            return Some(Escape);
        }
        if capture.contains(".regex") || capture == "regex" {
            return Some(Regexp);
        }
        if capture.starts_with("markup.heading") || capture.starts_with("text.title") {
            return Some(Heading);
        }
        if capture.starts_with("markup.link")
            || capture.starts_with("text.uri")
            || capture.starts_with("text.reference")
            || capture == "string.special.url"
        {
            return Some(Link);
        }
        if capture.starts_with("markup.raw") || capture.starts_with("text.literal") {
            return Some(String);
        }
        if capture == "tag.delimiter" {
            return Some(Punctuation);
        }
        if capture == "tag.attribute" {
            return Some(Attribute);
        }
        if capture.contains(".definition") {
            return Some(Definition);
        }
        match capture.split('.').next().unwrap_or(capture) {
            "keyword" | "conditional" | "repeat" | "exception" | "include" | "storageclass" => {
                Some(Keyword)
            }
            "comment" => Some(Comment),
            "string" | "character" => Some(String),
            "number" | "float" => Some(Number),
            "boolean" | "constant" => Some(Literal),
            "function" | "method" | "constructor" => Some(Function),
            "class" => Some(Class),
            "type" => Some(Type),
            "tag" => Some(Tag),
            "attribute" => Some(Attribute),
            "property" | "field" => Some(Property),
            "variable" | "parameter" => Some(Variable),
            "operator" => Some(Operator),
            "punctuation" => Some(Punctuation),
            "escape" => Some(Escape),
            "module" | "namespace" | "label" | "preproc" | "conceal" => Some(Meta),
            "heading" => Some(Heading),
            "url" | "link" => Some(Link),
            _ => None,
        }
    }
}

/// A zero-copy token; adjacent tokens with identical styles are merged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HighlightToken<'a> {
    pub text: &'a str,
    pub style: Option<HighlightStyle>,
}

/// Native replacement for the browser-specific CodeMirror style object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HighlightRule {
    pub role: HighlightStyle,
    pub color: &'static str,
}
