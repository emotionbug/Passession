/// Native grammar identity, independent of a browser editor or parser object.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Language {
    JavaScript,
    Jsx,
    TypeScript,
    Tsx,
    C,
    Cpp,
    ObjectiveC,
    Json,
    Css,
    Scss,
    Html,
    Svelte,
    Astro,
    Vue,
    Xml,
    Java,
    Python,
    Go,
    Php,
    Yaml,
    Rust,
    Swift,
    Dart,
    CSharp,
    Nix,
    Elixir,
    Markdown,
    /// Shell scripts (Flutter highlighted `sh`/`zsh` files and `bash`/`shell` fences as bash).
    Bash,
}

impl Language {
    pub const fn name(self) -> &'static str {
        match self {
            Self::JavaScript => "javascript",
            Self::Jsx => "jsx",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::ObjectiveC => "objc",
            Self::Json => "json",
            Self::Css => "css",
            Self::Scss => "scss",
            Self::Html => "html",
            Self::Svelte => "svelte",
            Self::Astro => "astro",
            Self::Vue => "vue",
            Self::Xml => "xml",
            Self::Java => "java",
            Self::Python => "python",
            Self::Go => "go",
            Self::Php => "php",
            Self::Yaml => "yaml",
            Self::Rust => "rust",
            Self::Swift => "swift",
            Self::Dart => "dart",
            Self::CSharp => "c-sharp",
            Self::Nix => "nix",
            Self::Elixir => "elixir",
            Self::Markdown => "markdown",
            Self::Bash => "bash",
        }
    }

    pub(crate) const fn grammar_name(self) -> &'static str {
        // The JavaScript grammar includes JSX; TSX is a distinct grammar.
        match self {
            Self::Jsx => "javascript",
            _ => self.name(),
        }
    }
}

// Preserve both coverage and discovery order from the original SDK.
const EXTENSIONS: &[(&str, Language)] = &[
    ("js", Language::JavaScript),
    ("jsx", Language::Jsx),
    ("ts", Language::TypeScript),
    ("tsx", Language::Tsx),
    ("mjs", Language::JavaScript),
    ("cjs", Language::JavaScript),
    ("c", Language::C),
    ("h", Language::Cpp),
    ("cc", Language::Cpp),
    ("cpp", Language::Cpp),
    ("cxx", Language::Cpp),
    ("hpp", Language::Cpp),
    ("hxx", Language::Cpp),
    ("m", Language::ObjectiveC),
    ("mm", Language::ObjectiveC),
    ("json", Language::Json),
    ("css", Language::Css),
    ("scss", Language::Scss),
    ("html", Language::Html),
    ("htm", Language::Html),
    ("svelte", Language::Svelte),
    ("astro", Language::Astro),
    ("vue", Language::Vue),
    ("xml", Language::Xml),
    ("java", Language::Java),
    ("py", Language::Python),
    ("go", Language::Go),
    ("php", Language::Php),
    ("yaml", Language::Yaml),
    ("yml", Language::Yaml),
    ("rs", Language::Rust),
    ("swift", Language::Swift),
    ("dart", Language::Dart),
    ("cs", Language::CSharp),
    ("nix", Language::Nix),
    ("ex", Language::Elixir),
    ("exs", Language::Elixir),
    ("md", Language::Markdown),
    ("mdx", Language::Markdown),
    ("sh", Language::Bash),
    ("bash", Language::Bash),
    ("zsh", Language::Bash),
];

/// Case-insensitive final-extension lookup. Bare extension names are accepted,
/// as in the former SDK. Both Unix and Windows path separators are understood.
pub fn get_language_for_file(filename: &str) -> Option<Language> {
    let basename = filename.rsplit(['/', '\\']).next()?;
    let extension = basename.rsplit('.').next()?;
    EXTENSIONS
        .iter()
        .find_map(|(ext, language)| ext.eq_ignore_ascii_case(extension).then_some(*language))
}

pub fn is_language_supported(filename: &str) -> bool {
    get_language_for_file(filename).is_some()
}

pub fn get_supported_extensions() -> impl ExactSizeIterator<Item = &'static str> {
    EXTENSIONS.iter().map(|(extension, _)| *extension)
}
