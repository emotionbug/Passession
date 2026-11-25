//! Arborium — High-performance syntax highlighting
//!
//! Arborium provides batteries-included syntax highlighting powered by tree-sitter.
//! It supports 60+ languages with automatic language injection (e.g., CSS/JS in HTML).
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use arborium::Highlighter;
//!
//! let mut hl = Highlighter::new();
//! let html = hl.highlight("rust", "fn main() {}")?;
//! // Output: <a-k>fn</a-k> <a-f>main</a-f>() {}
//! ```
//!
//! # HTML vs ANSI Output
//!
//! Use [`Highlighter`] for HTML output (web pages, documentation):
//!
//! ```rust,ignore
//! use arborium::{Highlighter, Config, HtmlFormat};
//!
//! // Default: custom elements (<a-k>, <a-f>, etc.)
//! let mut hl = Highlighter::new();
//!
//! // Or use class-based output for CSS compatibility
//! let config = Config {
//!     html_format: HtmlFormat::ClassNames,
//!     ..Default::default()
//! };
//! let mut hl = Highlighter::with_config(config);
//! ```
//!
//! Use [`AnsiHighlighter`] for terminal output:
//!
//! ```rust,ignore
//! use arborium::AnsiHighlighter;
//! use arborium::theme::builtin;
//!
//! let theme = builtin::catppuccin_mocha().clone();
//! let mut hl = AnsiHighlighter::new(theme);
//! let colored = hl.highlight("rust", "fn main() {}")?;
//! println!("{}", colored);
//! ```
//!
//! # Language Support
//!
//! Enable languages via feature flags:
//!
//! ```toml
//! [dependencies]
//! arborium = { version = "0.1", features = ["lang-rust", "lang-python"] }
//! ```
//!
//! Or enable all languages:
//!
//! ```toml
//! [dependencies]
//! arborium = { version = "0.1", features = ["all-languages"] }
//! ```
//!
//! ## Supported Languages
//!
//! ### Permissively Licensed (111 languages, included by default)
//!
//! | Language | Feature Flag | License |
//! |----------|--------------|---------|

//! | Ada | `lang-ada` | MIT |

//! | Agda | `lang-agda` | MIT |

//! | Asciidoc | `lang-asciidoc` | Apache-2.0 |

//! | Assembly | `lang-asm` | MIT |

//! | AWK | `lang-awk` | MIT |

//! | Bash | `lang-bash` | MIT |

//! | Batch | `lang-batch` | MIT |

//! | C | `lang-c` | MIT |

//! | C# | `lang-c-sharp` | MIT |

//! | Caddyfile | `lang-caddy` | MIT |

//! | Cap&#039;n Proto | `lang-capnp` | MIT |

//! | Cedar | `lang-cedar` | MIT OR Apache-2.0 |

//! | Cedar Schema | `lang-cedarschema` | MIT OR Apache-2.0 |

//! | Clojure | `lang-clojure` | Unlicense |

//! | CMake | `lang-cmake` | MIT |

//! | COBOL | `lang-cobol` | MIT |

//! | Common Lisp | `lang-commonlisp` | MIT |

//! | C++ | `lang-cpp` | MIT |

//! | CSS | `lang-css` | MIT |

//! | D | `lang-d` | MIT |

//! | Dart | `lang-dart` | MIT |

//! | Device Tree | `lang-devicetree` | MIT |

//! | Diff | `lang-diff` | MIT |

//! | Dockerfile | `lang-dockerfile` | MIT |

//! | DOT/Graphviz | `lang-dot` | MIT |

//! | Emacs Lisp | `lang-elisp` | MIT |

//! | Elixir | `lang-elixir` | Apache-2.0 |

//! | Elm | `lang-elm` | MIT |

//! | Erlang | `lang-erlang` | Apache-2.0 |

//! | Fish | `lang-fish` | Unlicense |

//! | F# | `lang-fsharp` | MIT |

//! | .gitattributes | `lang-gitattributes` | MIT |

//! | Gleam | `lang-gleam` | Apache-2.0 |

//! | GLSL | `lang-glsl` | MIT |

//! | Go | `lang-go` | MIT |

//! | GraphQL | `lang-graphql` | MIT |

//! | Groovy | `lang-groovy` | MIT |

//! | Haskell | `lang-haskell` | MIT |

//! | HCL | `lang-hcl` | Apache-2.0 |

//! | HLSL | `lang-hlsl` | MIT |

//! | HTML | `lang-html` | MIT |

//! | Idris | `lang-idris` | MIT |

//! | INI | `lang-ini` | Apache-2.0 |

//! | Java | `lang-java` | MIT |

//! | JavaScript | `lang-javascript` | MIT |

//! | Jinja2 | `lang-jinja2` | Apache-2.0 |

//! | jq | `lang-jq` | MIT |

//! | JSDoc | `lang-jsdoc` | MIT |

//! | JSON | `lang-json` | MIT |

//! | Julia | `lang-julia` | MIT |

//! | Just | `lang-just` | MIT |

//! | Kconfig | `lang-kconfig` | MIT |

//! | KDL | `lang-kdl` | MIT |

//! | Kotlin | `lang-kotlin` | MIT |

//! | Lean | `lang-lean` | MIT |

//! | Lua | `lang-lua` | MIT |

//! | Make | `lang-make` | MIT |

//! | Markdown | `lang-markdown` | MIT |

//! | MATLAB | `lang-matlab` | MIT |

//! | Meson | `lang-meson` | MIT |

//! | Ninja | `lang-ninja` | MIT |

//! | Nix | `lang-nix` | MIT |

//! | Objective-C | `lang-objc` | MIT |

//! | OCaml | `lang-ocaml` | MIT |

//! | Odin | `lang-odin` | MIT |

//! | Perl | `lang-perl` | MIT |

//! | PHP | `lang-php` | MIT |

//! | PostScript | `lang-postscript` | MIT |

//! | PowerShell | `lang-powershell` | MIT |

//! | Prolog | `lang-prolog` | MIT |

//! | Protocol Buffers | `lang-proto` | MIT |

//! | Python | `lang-python` | MIT |

//! | Tree-sitter Query | `lang-query` | Apache-2.0 |

//! | R | `lang-r` | MIT |

//! | Regular Expression | `lang-regex` | MIT |

//! | Rego | `lang-rego` | MIT |

//! | ReScript | `lang-rescript` | MIT |

//! | RON | `lang-ron` | MIT OR Apache-2.0 |

//! | Ruby | `lang-ruby` | MIT |

//! | Rust | `lang-rust` | MIT |

//! | Scala | `lang-scala` | MIT |

//! | Scheme | `lang-scheme` | MIT |

//! | SCSS | `lang-scss` | MIT |

//! | Solidity | `lang-solidity` | MIT |

//! | SPARQL | `lang-sparql` | MIT |

//! | SQL | `lang-sql` | MIT |

//! | SSH Config | `lang-ssh-config` | MIT |

//! | Starlark | `lang-starlark` | MIT |

//! | Styx | `lang-styx` | MIT OR Apache-2.0 |

//! | Svelte | `lang-svelte` | MIT |

//! | Swift | `lang-swift` | MIT |

//! | Text Proto | `lang-textproto` | ISC |

//! | Thrift | `lang-thrift` | MIT |

//! | TLA+ | `lang-tlaplus` | MIT |

//! | TOML | `lang-toml` | MIT |

//! | TSX | `lang-tsx` | MIT |

//! | TypeScript | `lang-typescript` | MIT |

//! | Typst | `lang-typst` | MIT |

//! | Uiua | `lang-uiua` | MPL-2.0 |

//! | Visual Basic | `lang-vb` | MIT |

//! | Verilog | `lang-verilog` | MIT |

//! | VHDL | `lang-vhdl` | MIT |

//! | Vimscript | `lang-vim` | MIT |

//! | Vue | `lang-vue` | MIT |

//! | WIT | `lang-wit` | Apache-2.0 WITH LLVM-exception |

//! | x86 Assembly | `lang-x86asm` | MIT |

//! | XML | `lang-xml` | MIT |

//! | YAML | `lang-yaml` | MIT |

//! | Yuri | `lang-yuri` | Apache-2.0 |

//! | Zig | `lang-zig` | MIT |

//! | Zsh | `lang-zsh` | MIT |

//!
//! ### GPL Licensed (1 languages, opt-in)
//!
//! These require explicit opt-in via feature flags due to their copyleft license.
//!
//! | Language | Feature Flag | License |
//! |----------|--------------|---------|

//! | nginx | `lang-nginx` | GPL-3.0 |

//!
//! # Advanced Usage
//!
//! For building custom grammar providers or working with raw spans, see the
//! [`advanced`] module.

// Internal modules
mod error;
mod highlighter;
pub(crate) mod store;

// Public modules
pub mod advanced;

/// Theme system for ANSI output.
///
/// Re-exports types from `arborium-theme` for configuring syntax colors.
pub mod theme {
    pub use arborium_theme::theme::{builtin, Color, Modifiers, Style, Theme};
}

// Primary API exports
pub use error::Error;
pub use highlighter::{AnsiHighlighter, Highlighter};
pub use store::GrammarStore;

// Configuration types (re-exported from arborium-highlight)
pub use arborium_highlight::HtmlFormat;

/// Configuration for highlighting.
///
/// Controls injection depth and HTML output format.
#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum depth for processing language injections.
    ///
    /// - `0`: No injections (just primary language)
    /// - `3`: Default, handles most cases (HTML with CSS/JS, Markdown with code blocks)
    /// - Higher: For deeply nested content
    pub max_injection_depth: u32,

    /// HTML output format.
    ///
    /// See [`HtmlFormat`] for options.
    pub html_format: HtmlFormat,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_injection_depth: 3,
            html_format: HtmlFormat::default(),
        }
    }
}

impl From<Config> for arborium_highlight::HighlightConfig {
    fn from(config: Config) -> Self {
        arborium_highlight::HighlightConfig {
            max_injection_depth: config.max_injection_depth,
            html_format: config.html_format,
        }
    }
}

// Tree-sitter re-export for advanced users
pub use arborium_tree_sitter as tree_sitter;

// WASM allocator (wasm32-unknown-unknown only; WASI links wasi-libc)
// Provides malloc/calloc/realloc/free symbols for tree-sitter's C code
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm;

// Highlight names constant
use arborium_theme::highlights;

/// Standard highlight names used for syntax highlighting.
///
/// These names are used to configure tree-sitter's `HighlightConfiguration`.
/// The indices correspond to HTML element tags (e.g., index 7 = `<a-k>` for keyword).
pub const HIGHLIGHT_NAMES: [&str; highlights::COUNT] = highlights::names();

/// Detect the language from a file path or name.
///
/// Extracts the file extension and maps it to a canonical language identifier.
/// Returns `None` if the extension is not recognized.
///
/// # Example
///
/// ```rust
/// use arborium::detect_language;
///
/// assert_eq!(detect_language("main.rs"), Some("rust"));
/// assert_eq!(detect_language("/path/to/script.py"), Some("python"));
/// assert_eq!(detect_language("styles.css"), Some("css"));
/// assert_eq!(detect_language("unknown.xyz"), None);
/// ```
pub fn detect_language(path: &str) -> Option<&'static str> {
    // Extract extension from path
    let ext = path
        .rsplit('.')
        .next()
        .filter(|e| !e.contains('/') && !e.contains('\\'))?;

    // Map extension to canonical language ID
    Some(match ext.to_lowercase().as_str() {

        "ada" => "ada",

        "adoc" => "asciidoc",

        "agda" => "agda",

        "asciidoc" => "asciidoc",

        "asm" => "asm",

        "assembly" => "asm",

        "awk" => "awk",

        "bash" => "bash",

        "bat" => "batch",

        "batch" => "batch",

        "bazel" => "starlark",

        "bzl" => "starlark",

        "c" => "c",

        "c++" => "cpp",

        "c-sharp" => "c-sharp",

        "caddy" => "caddy",

        "capnp" => "capnp",

        "cbl" => "cobol",

        "cedar" => "cedar",

        "cedarschema" => "cedarschema",

        "cfg" => "ini",

        "cjs" => "javascript",

        "cl" => "commonlisp",

        "clj" => "clojure",

        "clojure" => "clojure",

        "cmake" => "cmake",

        "cmd" => "batch",

        "cob" => "cobol",

        "cobol" => "cobol",

        "commonlisp" => "commonlisp",

        "conf" => "ini",

        "cpp" => "cpp",

        "cpy" => "cobol",

        "cs" => "c-sharp",

        "csharp" => "c-sharp",

        "css" => "css",

        "cts" => "typescript",

        "cxx" => "cpp",

        "d" => "d",

        "dart" => "dart",

        "devicetree" => "devicetree",

        "diff" => "diff",

        "dlang" => "d",

        "docker" => "dockerfile",

        "dockerfile" => "dockerfile",

        "dot" => "dot",

        "el" => "elisp",

        "elisp" => "elisp",

        "elixir" => "elixir",

        "elm" => "elm",

        "emacs-lisp" => "elisp",

        "erl" => "erlang",

        "erlang" => "erlang",

        "ex" => "elixir",

        "exs" => "elixir",

        "f#" => "fsharp",

        "fish" => "fish",

        "frag" => "glsl",

        "fs" => "fsharp",

        "fsharp" => "fsharp",

        "gitattributes" => "gitattributes",

        "gleam" => "gleam",

        "glsl" => "glsl",

        "go" => "go",

        "golang" => "go",

        "gql" => "graphql",

        "graphql" => "graphql",

        "groovy" => "groovy",

        "gsh" => "groovy",

        "gvy" => "groovy",

        "gy" => "groovy",

        "h" => "c",

        "haskell" => "haskell",

        "hcl" => "hcl",

        "hlsl" => "hlsl",

        "hpp" => "cpp",

        "hs" => "haskell",

        "htm" => "html",

        "html" => "html",

        "idr" => "idris",

        "idris" => "idris",

        "ini" => "ini",

        "j2" => "jinja2",

        "java" => "java",

        "javascript" => "javascript",

        "jinja" => "jinja2",

        "jinja2" => "jinja2",

        "jl" => "julia",

        "jq" => "jq",

        "js" => "javascript",

        "jsdoc" => "jsdoc",

        "json" => "json",

        "jsonc" => "json",

        "jsx" => "javascript",

        "julia" => "julia",

        "just" => "just",

        "kconfig" => "kconfig",

        "kdl" => "kdl",

        "kotlin" => "kotlin",

        "kt" => "kotlin",

        "kts" => "kotlin",

        "lean" => "lean",

        "lisp" => "commonlisp",

        "lua" => "lua",

        "m" => "matlab",

        "make" => "make",

        "markdown" => "markdown",

        "matlab" => "matlab",

        "md" => "markdown",

        "mdx" => "markdown",

        "meson" => "meson",

        "mjs" => "javascript",

        "ml" => "ocaml",

        "mm" => "objc",

        "mts" => "typescript",

        "mysql" => "sql",

        "nasm" => "x86asm",

        "nginx" => "nginx",

        "ninja" => "ninja",

        "nix" => "nix",

        "objc" => "objc",

        "objective-c" => "objc",

        "ocaml" => "ocaml",

        "odin" => "odin",

        "opa" => "rego",

        "patch" => "diff",

        "pbtxt" => "textproto",

        "perl" => "perl",

        "php" => "php",

        "pl" => "perl",

        "pm" => "perl",

        "postgres" => "sql",

        "postgresql" => "sql",

        "postscript" => "postscript",

        "powershell" => "powershell",

        "pro" => "prolog",

        "prolog" => "prolog",

        "proto" => "proto",

        "protobuf" => "proto",

        "ps" => "postscript",

        "ps1" => "powershell",

        "pwsh" => "powershell",

        "py" => "python",

        "py3" => "python",

        "python" => "python",

        "python3" => "python",

        "query" => "query",

        "r" => "r",

        "rb" => "ruby",

        "regex" => "regex",

        "rego" => "rego",

        "res" => "rescript",

        "rescript" => "rescript",

        "rkt" => "scheme",

        "rlang" => "r",

        "ron" => "ron",

        "rq" => "sparql",

        "rs" => "rust",

        "ruby" => "ruby",

        "rust" => "rust",

        "sass" => "scss",

        "scala" => "scala",

        "scheme" => "scheme",

        "scm" => "query",

        "scss" => "scss",

        "sh" => "bash",

        "shell" => "bash",

        "sol" => "solidity",

        "solidity" => "solidity",

        "sparql" => "sparql",

        "sql" => "sql",

        "sqlite" => "sql",

        "ss" => "scheme",

        "ssh-config" => "ssh-config",

        "starlark" => "starlark",

        "styx" => "styx",

        "sv" => "verilog",

        "svelte" => "svelte",

        "svg" => "xml",

        "swift" => "swift",

        "systemverilog" => "verilog",

        "terraform" => "hcl",

        "textpb" => "textproto",

        "textproto" => "textproto",

        "tf" => "hcl",

        "thrift" => "thrift",

        "tla" => "tlaplus",

        "tlaplus" => "tlaplus",

        "toml" => "toml",

        "ts" => "typescript",

        "tsx" => "tsx",

        "typ" => "typst",

        "typescript" => "typescript",

        "typst" => "typst",

        "ua" => "uiua",

        "uiua" => "uiua",

        "v" => "verilog",

        "vb" => "vb",

        "vbnet" => "vb",

        "verilog" => "verilog",

        "vert" => "glsl",

        "vhd" => "vhdl",

        "vhdl" => "vhdl",

        "vim" => "vim",

        "viml" => "vim",

        "vimscript" => "vim",

        "visualbasic" => "vb",

        "vue" => "vue",

        "wasm-interface" => "wit",

        "wit" => "wit",

        "x86" => "x86asm",

        "x86asm" => "x86asm",

        "xml" => "xml",

        "xsl" => "xml",

        "xslt" => "xml",

        "yaml" => "yaml",

        "yml" => "yaml",

        "yuri" => "yuri",

        "zig" => "zig",

        "zsh" => "zsh",

        _ => return None,
    })
}

// =============================================================================
// Language grammar re-exports based on enabled features.
// Each module provides:
// - `language()` - Returns the tree-sitter Language
// - `HIGHLIGHTS_QUERY` - The highlight query string
// - `INJECTIONS_QUERY` - The injection query string
// - `LOCALS_QUERY` - The locals query string
// =============================================================================


#[cfg(feature = "lang-ada")]
pub use arborium_ada as lang_ada;


#[cfg(feature = "lang-agda")]
pub use arborium_agda as lang_agda;


#[cfg(feature = "lang-asciidoc")]
pub use arborium_asciidoc as lang_asciidoc;


#[cfg(feature = "lang-asm")]
pub use arborium_asm as lang_asm;


#[cfg(feature = "lang-awk")]
pub use arborium_awk as lang_awk;


#[cfg(feature = "lang-bash")]
pub use arborium_bash as lang_bash;


#[cfg(feature = "lang-batch")]
pub use arborium_batch as lang_batch;


#[cfg(feature = "lang-c")]
pub use arborium_c as lang_c;


#[cfg(feature = "lang-c-sharp")]
pub use arborium_c_sharp as lang_c_sharp;


#[cfg(feature = "lang-caddy")]
pub use arborium_caddy as lang_caddy;


#[cfg(feature = "lang-capnp")]
pub use arborium_capnp as lang_capnp;


#[cfg(feature = "lang-cedar")]
pub use arborium_cedar as lang_cedar;


#[cfg(feature = "lang-cedarschema")]
pub use arborium_cedarschema as lang_cedarschema;


#[cfg(feature = "lang-clojure")]
pub use arborium_clojure as lang_clojure;


#[cfg(feature = "lang-cmake")]
pub use arborium_cmake as lang_cmake;


#[cfg(feature = "lang-cobol")]
pub use arborium_cobol as lang_cobol;


#[cfg(feature = "lang-commonlisp")]
pub use arborium_commonlisp as lang_commonlisp;


#[cfg(feature = "lang-cpp")]
pub use arborium_cpp as lang_cpp;


#[cfg(feature = "lang-css")]
pub use arborium_css as lang_css;


#[cfg(feature = "lang-d")]
pub use arborium_d as lang_d;


#[cfg(feature = "lang-dart")]
pub use arborium_dart as lang_dart;


#[cfg(feature = "lang-devicetree")]
pub use arborium_devicetree as lang_devicetree;


#[cfg(feature = "lang-diff")]
pub use arborium_diff as lang_diff;


#[cfg(feature = "lang-dockerfile")]
pub use arborium_dockerfile as lang_dockerfile;


#[cfg(feature = "lang-dot")]
pub use arborium_dot as lang_dot;


#[cfg(feature = "lang-elisp")]
pub use arborium_elisp as lang_elisp;


#[cfg(feature = "lang-elixir")]
pub use arborium_elixir as lang_elixir;


#[cfg(feature = "lang-elm")]
pub use arborium_elm as lang_elm;


#[cfg(feature = "lang-erlang")]
pub use arborium_erlang as lang_erlang;


#[cfg(feature = "lang-fish")]
pub use arborium_fish as lang_fish;


#[cfg(feature = "lang-fsharp")]
pub use arborium_fsharp as lang_fsharp;


#[cfg(feature = "lang-gitattributes")]
pub use arborium_gitattributes as lang_gitattributes;


#[cfg(feature = "lang-gleam")]
pub use arborium_gleam as lang_gleam;


#[cfg(feature = "lang-glsl")]
pub use arborium_glsl as lang_glsl;


#[cfg(feature = "lang-go")]
pub use arborium_go as lang_go;


#[cfg(feature = "lang-graphql")]
pub use arborium_graphql as lang_graphql;


#[cfg(feature = "lang-groovy")]
pub use arborium_groovy as lang_groovy;


#[cfg(feature = "lang-haskell")]
pub use arborium_haskell as lang_haskell;


#[cfg(feature = "lang-hcl")]
pub use arborium_hcl as lang_hcl;


#[cfg(feature = "lang-hlsl")]
pub use arborium_hlsl as lang_hlsl;


#[cfg(feature = "lang-html")]
pub use arborium_html as lang_html;


#[cfg(feature = "lang-idris")]
pub use arborium_idris as lang_idris;


#[cfg(feature = "lang-ini")]
pub use arborium_ini as lang_ini;


#[cfg(feature = "lang-java")]
pub use arborium_java as lang_java;


#[cfg(feature = "lang-javascript")]
pub use arborium_javascript as lang_javascript;


#[cfg(feature = "lang-jinja2")]
pub use arborium_jinja2 as lang_jinja2;


#[cfg(feature = "lang-jq")]
pub use arborium_jq as lang_jq;


#[cfg(feature = "lang-jsdoc")]
pub use arborium_jsdoc as lang_jsdoc;


#[cfg(feature = "lang-json")]
pub use arborium_json as lang_json;


#[cfg(feature = "lang-julia")]
pub use arborium_julia as lang_julia;


#[cfg(feature = "lang-just")]
pub use arborium_just as lang_just;


#[cfg(feature = "lang-kconfig")]
pub use arborium_kconfig as lang_kconfig;


#[cfg(feature = "lang-kdl")]
pub use arborium_kdl as lang_kdl;


#[cfg(feature = "lang-kotlin")]
pub use arborium_kotlin as lang_kotlin;


#[cfg(feature = "lang-lean")]
pub use arborium_lean as lang_lean;


#[cfg(feature = "lang-lua")]
pub use arborium_lua as lang_lua;


#[cfg(feature = "lang-make")]
pub use arborium_make as lang_make;


#[cfg(feature = "lang-markdown")]
pub use arborium_markdown as lang_markdown;


#[cfg(feature = "lang-matlab")]
pub use arborium_matlab as lang_matlab;


#[cfg(feature = "lang-meson")]
pub use arborium_meson as lang_meson;


#[cfg(feature = "lang-nginx")]
pub use arborium_nginx as lang_nginx;


#[cfg(feature = "lang-ninja")]
pub use arborium_ninja as lang_ninja;


#[cfg(feature = "lang-nix")]
pub use arborium_nix as lang_nix;


#[cfg(feature = "lang-objc")]
pub use arborium_objc as lang_objc;


#[cfg(feature = "lang-ocaml")]
pub use arborium_ocaml as lang_ocaml;


#[cfg(feature = "lang-odin")]
pub use arborium_odin as lang_odin;


#[cfg(feature = "lang-perl")]
pub use arborium_perl as lang_perl;


#[cfg(feature = "lang-php")]
pub use arborium_php as lang_php;


#[cfg(feature = "lang-postscript")]
pub use arborium_postscript as lang_postscript;


#[cfg(feature = "lang-powershell")]
pub use arborium_powershell as lang_powershell;


#[cfg(feature = "lang-prolog")]
pub use arborium_prolog as lang_prolog;


#[cfg(feature = "lang-proto")]
pub use arborium_proto as lang_proto;


#[cfg(feature = "lang-python")]
pub use arborium_python as lang_python;


#[cfg(feature = "lang-query")]
pub use arborium_query as lang_query;


#[cfg(feature = "lang-r")]
pub use arborium_r as lang_r;


#[cfg(feature = "lang-regex")]
pub use arborium_regex as lang_regex;


#[cfg(feature = "lang-rego")]
pub use arborium_rego as lang_rego;


#[cfg(feature = "lang-rescript")]
pub use arborium_rescript as lang_rescript;


#[cfg(feature = "lang-ron")]
pub use arborium_ron as lang_ron;


#[cfg(feature = "lang-ruby")]
pub use arborium_ruby as lang_ruby;


#[cfg(feature = "lang-rust")]
pub use arborium_rust as lang_rust;


#[cfg(feature = "lang-scala")]
pub use arborium_scala as lang_scala;


#[cfg(feature = "lang-scheme")]
pub use arborium_scheme as lang_scheme;


#[cfg(feature = "lang-scss")]
pub use arborium_scss as lang_scss;


#[cfg(feature = "lang-solidity")]
pub use arborium_solidity as lang_solidity;


#[cfg(feature = "lang-sparql")]
pub use arborium_sparql as lang_sparql;


#[cfg(feature = "lang-sql")]
pub use arborium_sql as lang_sql;


#[cfg(feature = "lang-ssh-config")]
pub use arborium_ssh_config as lang_ssh_config;


#[cfg(feature = "lang-starlark")]
pub use arborium_starlark as lang_starlark;


#[cfg(feature = "lang-styx")]
pub use arborium_styx as lang_styx;


#[cfg(feature = "lang-svelte")]
pub use arborium_svelte as lang_svelte;


#[cfg(feature = "lang-swift")]
pub use arborium_swift as lang_swift;


#[cfg(feature = "lang-textproto")]
pub use arborium_textproto as lang_textproto;


#[cfg(feature = "lang-thrift")]
pub use arborium_thrift as lang_thrift;


#[cfg(feature = "lang-tlaplus")]
pub use arborium_tlaplus as lang_tlaplus;


#[cfg(feature = "lang-toml")]
pub use arborium_toml as lang_toml;


#[cfg(feature = "lang-tsx")]
pub use arborium_tsx as lang_tsx;


#[cfg(feature = "lang-typescript")]
pub use arborium_typescript as lang_typescript;


#[cfg(feature = "lang-typst")]
pub use arborium_typst as lang_typst;


#[cfg(feature = "lang-uiua")]
pub use arborium_uiua as lang_uiua;


#[cfg(feature = "lang-vb")]
pub use arborium_vb as lang_vb;


#[cfg(feature = "lang-verilog")]
pub use arborium_verilog as lang_verilog;


#[cfg(feature = "lang-vhdl")]
pub use arborium_vhdl as lang_vhdl;


#[cfg(feature = "lang-vim")]
pub use arborium_vim as lang_vim;


#[cfg(feature = "lang-vue")]
pub use arborium_vue as lang_vue;


#[cfg(feature = "lang-wit")]
pub use arborium_wit as lang_wit;


#[cfg(feature = "lang-x86asm")]
pub use arborium_x86asm as lang_x86asm;


#[cfg(feature = "lang-xml")]
pub use arborium_xml as lang_xml;


#[cfg(feature = "lang-yaml")]
pub use arborium_yaml as lang_yaml;


#[cfg(feature = "lang-yuri")]
pub use arborium_yuri as lang_yuri;


#[cfg(feature = "lang-zig")]
pub use arborium_zig as lang_zig;


#[cfg(feature = "lang-zsh")]
pub use arborium_zsh as lang_zsh;



/// Returns the tree-sitter `Language` for the given language name.
///
/// This function only returns languages that are enabled via feature flags.
/// If no grammar matches the provided language name, it returns `None`.
///
/// # Example
///
/// ```rust,ignore
/// use arborium::get_language;
///
/// // Returns Some if the "lang-rust" feature is enabled
/// if let Some(lang) = get_language("rust") {
///     println!("Got Rust language: {:?}", lang);
/// }
///
/// // Returns None for unknown languages
/// assert!(get_language("unknown-language").is_none());
/// ```
pub fn get_language(name: &str) -> Option<tree_sitter::Language> {
    match name {

        #[cfg(feature = "lang-ada")]
        "ada" => Some(arborium_ada::language().into()),

        #[cfg(feature = "lang-agda")]
        "agda" => Some(arborium_agda::language().into()),

        #[cfg(feature = "lang-asciidoc")]
        "asciidoc" => Some(arborium_asciidoc::language().into()),

        #[cfg(feature = "lang-asm")]
        "asm" => Some(arborium_asm::language().into()),

        #[cfg(feature = "lang-awk")]
        "awk" => Some(arborium_awk::language().into()),

        #[cfg(feature = "lang-bash")]
        "bash" => Some(arborium_bash::language().into()),

        #[cfg(feature = "lang-batch")]
        "batch" => Some(arborium_batch::language().into()),

        #[cfg(feature = "lang-c")]
        "c" => Some(arborium_c::language().into()),

        #[cfg(feature = "lang-c-sharp")]
        "c-sharp" => Some(arborium_c_sharp::language().into()),

        #[cfg(feature = "lang-caddy")]
        "caddy" => Some(arborium_caddy::language().into()),

        #[cfg(feature = "lang-capnp")]
        "capnp" => Some(arborium_capnp::language().into()),

        #[cfg(feature = "lang-cedar")]
        "cedar" => Some(arborium_cedar::language().into()),

        #[cfg(feature = "lang-cedarschema")]
        "cedarschema" => Some(arborium_cedarschema::language().into()),

        #[cfg(feature = "lang-clojure")]
        "clojure" => Some(arborium_clojure::language().into()),

        #[cfg(feature = "lang-cmake")]
        "cmake" => Some(arborium_cmake::language().into()),

        #[cfg(feature = "lang-cobol")]
        "cobol" => Some(arborium_cobol::language().into()),

        #[cfg(feature = "lang-commonlisp")]
        "commonlisp" => Some(arborium_commonlisp::language().into()),

        #[cfg(feature = "lang-cpp")]
        "cpp" => Some(arborium_cpp::language().into()),

        #[cfg(feature = "lang-css")]
        "css" => Some(arborium_css::language().into()),

        #[cfg(feature = "lang-d")]
        "d" => Some(arborium_d::language().into()),

        #[cfg(feature = "lang-dart")]
        "dart" => Some(arborium_dart::language().into()),

        #[cfg(feature = "lang-devicetree")]
        "devicetree" => Some(arborium_devicetree::language().into()),

        #[cfg(feature = "lang-diff")]
        "diff" => Some(arborium_diff::language().into()),

        #[cfg(feature = "lang-dockerfile")]
        "dockerfile" => Some(arborium_dockerfile::language().into()),

        #[cfg(feature = "lang-dot")]
        "dot" => Some(arborium_dot::language().into()),

        #[cfg(feature = "lang-elisp")]
        "elisp" => Some(arborium_elisp::language().into()),

        #[cfg(feature = "lang-elixir")]
        "elixir" => Some(arborium_elixir::language().into()),

        #[cfg(feature = "lang-elm")]
        "elm" => Some(arborium_elm::language().into()),

        #[cfg(feature = "lang-erlang")]
        "erlang" => Some(arborium_erlang::language().into()),

        #[cfg(feature = "lang-fish")]
        "fish" => Some(arborium_fish::language().into()),

        #[cfg(feature = "lang-fsharp")]
        "fsharp" => Some(arborium_fsharp::language().into()),

        #[cfg(feature = "lang-gitattributes")]
        "gitattributes" => Some(arborium_gitattributes::language().into()),

        #[cfg(feature = "lang-gleam")]
        "gleam" => Some(arborium_gleam::language().into()),

        #[cfg(feature = "lang-glsl")]
        "glsl" => Some(arborium_glsl::language().into()),

        #[cfg(feature = "lang-go")]
        "go" => Some(arborium_go::language().into()),

        #[cfg(feature = "lang-graphql")]
        "graphql" => Some(arborium_graphql::language().into()),

        #[cfg(feature = "lang-groovy")]
        "groovy" => Some(arborium_groovy::language().into()),

        #[cfg(feature = "lang-haskell")]
        "haskell" => Some(arborium_haskell::language().into()),

        #[cfg(feature = "lang-hcl")]
        "hcl" => Some(arborium_hcl::language().into()),

        #[cfg(feature = "lang-hlsl")]
        "hlsl" => Some(arborium_hlsl::language().into()),

        #[cfg(feature = "lang-html")]
        "html" => Some(arborium_html::language().into()),

        #[cfg(feature = "lang-idris")]
        "idris" => Some(arborium_idris::language().into()),

        #[cfg(feature = "lang-ini")]
        "ini" => Some(arborium_ini::language().into()),

        #[cfg(feature = "lang-java")]
        "java" => Some(arborium_java::language().into()),

        #[cfg(feature = "lang-javascript")]
        "javascript" => Some(arborium_javascript::language().into()),

        #[cfg(feature = "lang-jinja2")]
        "jinja2" => Some(arborium_jinja2::language().into()),

        #[cfg(feature = "lang-jq")]
        "jq" => Some(arborium_jq::language().into()),

        #[cfg(feature = "lang-jsdoc")]
        "jsdoc" => Some(arborium_jsdoc::language().into()),

        #[cfg(feature = "lang-json")]
        "json" => Some(arborium_json::language().into()),

        #[cfg(feature = "lang-julia")]
        "julia" => Some(arborium_julia::language().into()),

        #[cfg(feature = "lang-just")]
        "just" => Some(arborium_just::language().into()),

        #[cfg(feature = "lang-kconfig")]
        "kconfig" => Some(arborium_kconfig::language().into()),

        #[cfg(feature = "lang-kdl")]
        "kdl" => Some(arborium_kdl::language().into()),

        #[cfg(feature = "lang-kotlin")]
        "kotlin" => Some(arborium_kotlin::language().into()),

        #[cfg(feature = "lang-lean")]
        "lean" => Some(arborium_lean::language().into()),

        #[cfg(feature = "lang-lua")]
        "lua" => Some(arborium_lua::language().into()),

        #[cfg(feature = "lang-make")]
        "make" => Some(arborium_make::language().into()),

        #[cfg(feature = "lang-markdown")]
        "markdown" => Some(arborium_markdown::language().into()),

        #[cfg(feature = "lang-matlab")]
        "matlab" => Some(arborium_matlab::language().into()),

        #[cfg(feature = "lang-meson")]
        "meson" => Some(arborium_meson::language().into()),

        #[cfg(feature = "lang-nginx")]
        "nginx" => Some(arborium_nginx::language().into()),

        #[cfg(feature = "lang-ninja")]
        "ninja" => Some(arborium_ninja::language().into()),

        #[cfg(feature = "lang-nix")]
        "nix" => Some(arborium_nix::language().into()),

        #[cfg(feature = "lang-objc")]
        "objc" => Some(arborium_objc::language().into()),

        #[cfg(feature = "lang-ocaml")]
        "ocaml" => Some(arborium_ocaml::language().into()),

        #[cfg(feature = "lang-odin")]
        "odin" => Some(arborium_odin::language().into()),

        #[cfg(feature = "lang-perl")]
        "perl" => Some(arborium_perl::language().into()),

        #[cfg(feature = "lang-php")]
        "php" => Some(arborium_php::language().into()),

        #[cfg(feature = "lang-postscript")]
        "postscript" => Some(arborium_postscript::language().into()),

        #[cfg(feature = "lang-powershell")]
        "powershell" => Some(arborium_powershell::language().into()),

        #[cfg(feature = "lang-prolog")]
        "prolog" => Some(arborium_prolog::language().into()),

        #[cfg(feature = "lang-proto")]
        "proto" => Some(arborium_proto::language().into()),

        #[cfg(feature = "lang-python")]
        "python" => Some(arborium_python::language().into()),

        #[cfg(feature = "lang-query")]
        "query" => Some(arborium_query::language().into()),

        #[cfg(feature = "lang-r")]
        "r" => Some(arborium_r::language().into()),

        #[cfg(feature = "lang-regex")]
        "regex" => Some(arborium_regex::language().into()),

        #[cfg(feature = "lang-rego")]
        "rego" => Some(arborium_rego::language().into()),

        #[cfg(feature = "lang-rescript")]
        "rescript" => Some(arborium_rescript::language().into()),

        #[cfg(feature = "lang-ron")]
        "ron" => Some(arborium_ron::language().into()),

        #[cfg(feature = "lang-ruby")]
        "ruby" => Some(arborium_ruby::language().into()),

        #[cfg(feature = "lang-rust")]
        "rust" => Some(arborium_rust::language().into()),

        #[cfg(feature = "lang-scala")]
        "scala" => Some(arborium_scala::language().into()),

        #[cfg(feature = "lang-scheme")]
        "scheme" => Some(arborium_scheme::language().into()),

        #[cfg(feature = "lang-scss")]
        "scss" => Some(arborium_scss::language().into()),

        #[cfg(feature = "lang-solidity")]
        "solidity" => Some(arborium_solidity::language().into()),

        #[cfg(feature = "lang-sparql")]
        "sparql" => Some(arborium_sparql::language().into()),

        #[cfg(feature = "lang-sql")]
        "sql" => Some(arborium_sql::language().into()),

        #[cfg(feature = "lang-ssh-config")]
        "ssh-config" => Some(arborium_ssh_config::language().into()),

        #[cfg(feature = "lang-starlark")]
        "starlark" => Some(arborium_starlark::language().into()),

        #[cfg(feature = "lang-styx")]
        "styx" => Some(arborium_styx::language().into()),

        #[cfg(feature = "lang-svelte")]
        "svelte" => Some(arborium_svelte::language().into()),

        #[cfg(feature = "lang-swift")]
        "swift" => Some(arborium_swift::language().into()),

        #[cfg(feature = "lang-textproto")]
        "textproto" => Some(arborium_textproto::language().into()),

        #[cfg(feature = "lang-thrift")]
        "thrift" => Some(arborium_thrift::language().into()),

        #[cfg(feature = "lang-tlaplus")]
        "tlaplus" => Some(arborium_tlaplus::language().into()),

        #[cfg(feature = "lang-toml")]
        "toml" => Some(arborium_toml::language().into()),

        #[cfg(feature = "lang-tsx")]
        "tsx" => Some(arborium_tsx::language().into()),

        #[cfg(feature = "lang-typescript")]
        "typescript" => Some(arborium_typescript::language().into()),

        #[cfg(feature = "lang-typst")]
        "typst" => Some(arborium_typst::language().into()),

        #[cfg(feature = "lang-uiua")]
        "uiua" => Some(arborium_uiua::language().into()),

        #[cfg(feature = "lang-vb")]
        "vb" => Some(arborium_vb::language().into()),

        #[cfg(feature = "lang-verilog")]
        "verilog" => Some(arborium_verilog::language().into()),

        #[cfg(feature = "lang-vhdl")]
        "vhdl" => Some(arborium_vhdl::language().into()),

        #[cfg(feature = "lang-vim")]
        "vim" => Some(arborium_vim::language().into()),

        #[cfg(feature = "lang-vue")]
        "vue" => Some(arborium_vue::language().into()),

        #[cfg(feature = "lang-wit")]
        "wit" => Some(arborium_wit::language().into()),

        #[cfg(feature = "lang-x86asm")]
        "x86asm" => Some(arborium_x86asm::language().into()),

        #[cfg(feature = "lang-xml")]
        "xml" => Some(arborium_xml::language().into()),

        #[cfg(feature = "lang-yaml")]
        "yaml" => Some(arborium_yaml::language().into()),

        #[cfg(feature = "lang-yuri")]
        "yuri" => Some(arborium_yuri::language().into()),

        #[cfg(feature = "lang-zig")]
        "zig" => Some(arborium_zig::language().into()),

        #[cfg(feature = "lang-zsh")]
        "zsh" => Some(arborium_zsh::language().into()),

        _ => None,
    }
}