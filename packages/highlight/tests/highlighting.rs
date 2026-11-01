use paseo_highlight::{
    ColorScheme, HighlightStep, HighlightStepsOptions, HighlightStyle as Style, HighlightToken,
    Highlighter, Language, OwnedHighlightToken, SYNTAX_THEME_IDS, SyntaxThemeId, byte_to_utf16,
    create_highlight_style, get_language_for_file, get_parser_for_file, get_supported_extensions,
    highlight_code, highlight_code_in_steps, highlight_line, is_language_supported,
    resolve_syntax_colors, utf16_to_byte,
};
use std::{num::NonZeroUsize, sync::Arc};

fn reconstruct(lines: &[Vec<HighlightToken<'_>>]) -> String {
    lines
        .iter()
        .map(|line| line.iter().map(|token| token.text).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_style(source: &str, filename: &str, needle: &str, style: Style) {
    let start = source.find(needle).expect("fixture contains needle");
    assert_style_at(source, filename, start..start + needle.len(), style);
}

fn assert_style_at(source: &str, filename: &str, range: std::ops::Range<usize>, style: Style) {
    let start = range.start;
    let end = range.end;
    let needle = &source[range];
    let mut highlighter = Highlighter::new().unwrap();
    let spans = highlighter
        .highlight_spans(source, get_language_for_file(filename).unwrap())
        .unwrap();
    assert!(
        spans.iter().any(|span| span.range.start <= start
            && span.range.end >= end
            && span.style == Some(style)),
        "{filename}: expected {needle:?} to be {style:?}; spans: {spans:?}"
    );
}

#[test]
fn every_previous_extension_uses_a_real_native_grammar() {
    for extension in get_supported_extensions() {
        assert!(is_language_supported(extension));
        assert!(
            get_parser_for_file(extension).unwrap().is_some(),
            "{extension}"
        );
    }
    let cases = [
        ("js", "const count = 42;", "const", Style::Keyword),
        ("jsx", "const view = <section />;", "section", Style::Tag),
        ("ts", "const count: number = 42;", "const", Style::Keyword),
        ("tsx", "const view = <section />;", "section", Style::Tag),
        (
            "c",
            "int main(void) { return 42; }",
            "return",
            Style::Keyword,
        ),
        (
            "cpp",
            "class Box { public: int get() { return 42; } };",
            "return",
            Style::Keyword,
        ),
        (
            "mm",
            "@interface Box : NSObject\n@end",
            "@interface",
            Style::Keyword,
        ),
        ("json", "{\"count\": 42}", "42", Style::Number),
        ("css", ".box { color: red; }", "color", Style::Property),
        (
            "scss",
            "$size: 42px; .box { width: $size; }",
            "width",
            Style::Property,
        ),
        (
            "html",
            "<section class=\"box\">text</section>",
            "section",
            Style::Tag,
        ),
        (
            "svelte",
            "<script>let count = 42;</script><p>{count}</p>",
            "let",
            Style::Keyword,
        ),
        (
            "astro",
            "---\nconst count = 42;\n---\n<p>{count}</p>",
            "const",
            Style::Keyword,
        ),
        (
            "vue",
            "<script setup>const count = 42;</script><template><p>{{count}}</p></template>",
            "const",
            Style::Keyword,
        ),
        (
            "xml",
            "<section id=\"box\">text</section>",
            "section",
            Style::Tag,
        ),
        (
            "java",
            "class Box { int get() { return 42; } }",
            "return",
            Style::Keyword,
        ),
        ("py", "def get():\n    return 42", "return", Style::Keyword),
        (
            "go",
            "package main\nfunc get() int { return 42 }",
            "return",
            Style::Keyword,
        ),
        (
            "php",
            "<?php function get() { return 42; }",
            "return",
            Style::Keyword,
        ),
        ("yaml", "count: 42\n", "42", Style::Number),
        (
            "rs",
            "fn get() -> i32 { return 42; }",
            "return",
            Style::Keyword,
        ),
        (
            "swift",
            "func get() -> Int { return 42 }",
            "return",
            Style::Keyword,
        ),
        ("dart", "int get() { return 42; }", "return", Style::Keyword),
        (
            "cs",
            "class Box { int Get() { return 42; } }",
            "return",
            Style::Keyword,
        ),
        ("nix", "let count = 42; in count", "let", Style::Keyword),
        (
            "ex",
            "defmodule Box do\n  def get, do: 42\nend",
            "42",
            Style::Number,
        ),
        (
            "md",
            "# Heading\n\nA [link](https://example.com).",
            "Heading",
            Style::Heading,
        ),
    ];
    for (extension, source, needle, style) in cases {
        let lines = highlight_code(source, extension).unwrap();
        assert_eq!(reconstruct(&lines), source, "{extension}");
        assert_style(source, extension, needle, style);
    }
}

#[test]
fn detects_case_insensitive_suffixes_without_confusing_directory_names() {
    assert_eq!(
        get_language_for_file("/some.dir/COMPONENT.TSX"),
        Some(Language::Tsx)
    );
    assert_eq!(
        get_language_for_file("C:\\folder.ts\\component.VUE"),
        Some(Language::Vue)
    );
    assert_eq!(get_language_for_file("/folder.ts/README"), None);
    assert_eq!(get_language_for_file("mdx"), Some(Language::Markdown));
    assert_eq!(get_language_for_file("file.unknown"), None);
    assert!(get_parser_for_file("file.unknown").unwrap().is_none());
}

#[test]
fn tokens_preserve_unicode_crlf_blank_lines_and_trailing_newlines() {
    for source in [
        "",
        "\n",
        "\n\n",
        "const café = \"🦀é漢字\";\r\n\r\n// fin\n",
    ] {
        let lines = highlight_code(source, "ts").unwrap();
        assert_eq!(reconstruct(&lines), source);
        assert_eq!(lines.len(), source.split('\n').count());
        for line in &lines {
            assert!(!line.is_empty());
            assert!(line.windows(2).all(|pair| pair[0].style != pair[1].style));
        }
    }
    let plain = "🦀\r\ntext\n";
    let lines = highlight_code(plain, "unknown").unwrap();
    assert_eq!(reconstruct(&lines), plain);
    assert!(lines.iter().flatten().all(|token| token.style.is_none()));
    assert_eq!(
        highlight_line("const n = 42;\nignored", "js").unwrap(),
        highlight_code("const n = 42;\nignored", "js").unwrap()[0]
    );
}

#[test]
fn utf16_adapters_reject_partial_scalars_and_surrogates() {
    let source = "aé🦀漢";
    for (byte, utf16) in [(0, 0), (1, 1), (3, 2), (7, 4), (10, 5)] {
        assert_eq!(byte_to_utf16(source, byte), Some(utf16));
        assert_eq!(utf16_to_byte(source, utf16), Some(byte));
    }
    for byte in [2, 4, 5, 6, 8, 9, 11] {
        assert_eq!(byte_to_utf16(source, byte), None);
    }
    assert_eq!(utf16_to_byte(source, 3), None);
    assert_eq!(utf16_to_byte(source, 6), None);
}

#[test]
fn mixed_languages_keep_script_style_and_expression_boundaries() {
    let svelte = r#"<script lang="ts">let count: number = 42;</script>
{#if count > 0}<p class="badge">{`a ${`}`} b`}</p>{/if}
<style>.badge { color: red; }</style>"#;
    let astro = r#"---
const title: string = "hé🦀";
---
<p class="badge">{`a ${`}`} b`}</p>
<style>.badge { color: red; }</style>"#;
    let vue = r#"<script setup lang="ts">const count: number = 42;</script>
<template><p v-if="count > 0" :class="'badge'">{{ {count: 1} }} {{ 'a}}b' }} {{ `a ${`}`} b` }}</p></template>
<style>.badge { color: red; }</style>"#;
    for (filename, source) in [("svelte", svelte), ("astro", astro), ("vue", vue)] {
        assert_eq!(
            reconstruct(&highlight_code(source, filename).unwrap()),
            source
        );
        assert_style(source, filename, "color", Style::Property);
        let selector = source.rfind(".badge").unwrap() + 1;
        assert_style_at(source, filename, selector..selector + 5, Style::Class);
        let tag = source.find("<p").unwrap() + 1;
        assert_style_at(source, filename, tag..tag + 1, Style::Tag);
    }
    assert_style(svelte, "svelte", "let", Style::Keyword);
    assert_style(astro, "astro", "const", Style::Keyword);
    assert_style(vue, "vue", "const", Style::Keyword);
    assert_style(vue, "vue", "42", Style::Number);
    assert_style(vue, "vue", "a}}b", Style::String);
    for (filename, source) in [("svelte", svelte), ("astro", astro), ("vue", vue)] {
        assert_style(source, filename, " b", Style::String);
    }
    for (filename, source) in [
        ("svelte", "<p>{}</p>"),
        ("astro", "<p>{}</p>"),
        ("vue", "<template><p>{{}}</p></template>"),
    ] {
        assert_eq!(
            reconstruct(&highlight_code(source, filename).unwrap()),
            source
        );
        let tag = source.find("<p").unwrap() + 1;
        assert_style_at(source, filename, tag..tag + 1, Style::Tag);
    }
}

#[test]
fn markdown_fences_and_inline_links_are_parsed_in_their_own_languages() {
    let source = "# Example\n\nA [link](https://example.com).\n\n```typescript\nconst café: number = 42;\n```\n";
    assert_style(source, "md", "Example", Style::Heading);
    assert_style(source, "md", "https://example.com", Style::Link);
    assert_style(source, "md", "const", Style::Keyword);
    assert_style(source, "md", "42", Style::Number);
    assert_eq!(reconstruct(&highlight_code(source, "md").unwrap()), source);
}

#[test]
fn native_ranges_partition_source_on_utf8_boundaries() {
    let source = "/* 🦀\ncomment */\nconst café = `漢 ${42}`;\n";
    let mut highlighter = Highlighter::new().unwrap();
    let spans = highlighter
        .highlight_spans(source, Language::TypeScript)
        .unwrap();
    let mut offset = 0;
    for span in &spans {
        assert_eq!(span.range.start, offset);
        assert!(span.range.end > span.range.start);
        assert!(source.is_char_boundary(span.range.start));
        assert!(source.is_char_boundary(span.range.end));
        offset = span.range.end;
    }
    assert_eq!(offset, source.len());
    assert_eq!(
        highlighter
            .fork()
            .highlight_spans(source, Language::TypeScript)
            .unwrap(),
        spans
    );
}

#[test]
fn incremental_delivery_matches_whole_document_across_multiline_boundaries() {
    let mut source = String::from("/* opening 🦀\n");
    for _ in 0..405 {
        source.push_str("still inside comment: const fake = 42;\n");
    }
    source.push_str("*/\nconst actual = `hé\ncontinued ${42}`;\n");
    let expected: Vec<Vec<OwnedHighlightToken>> = highlight_code(&source, "ts")
        .unwrap()
        .into_iter()
        .map(|line| line.into_iter().map(Into::into).collect())
        .collect();
    let mut steps = highlight_code_in_steps(
        Arc::<str>::from(source),
        "ts",
        HighlightStepsOptions {
            first_lines: 2,
            lines_per_step: NonZeroUsize::new(200).unwrap(),
        },
    )
    .unwrap();
    let mut actual = Vec::new();
    let mut saw_preview = false;
    let mut saw_finished = false;
    for step in steps.by_ref() {
        match step.unwrap() {
            HighlightStep::Preview { lines } => {
                assert_eq!(lines.len(), 2);
                saw_preview = true;
            }
            HighlightStep::Lines { first_line, lines } => {
                assert_eq!(first_line, actual.len());
                assert!(!lines.is_empty() && lines.len() <= 200);
                actual.extend(lines);
            }
            HighlightStep::Finished { line_count } => {
                assert_eq!(line_count, expected.len());
                saw_finished = true;
            }
        }
    }
    assert!(saw_preview && saw_finished);
    assert_eq!(actual, expected);
    assert!(
        actual[200]
            .iter()
            .all(|token| token.style == Some(Style::Comment))
    );
    assert!(steps.next().is_none());
    assert!(matches!(steps.poll_next(), std::task::Poll::Ready(None)));
}

#[test]
fn empty_incremental_documents_still_emit_one_line_and_finish() {
    let steps: Vec<_> = highlight_code_in_steps("", "ts", HighlightStepsOptions::default())
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        steps,
        vec![
            HighlightStep::Lines {
                first_line: 0,
                lines: vec![vec![OwnedHighlightToken {
                    text: String::new(),
                    style: None
                }]]
            },
            HighlightStep::Finished { line_count: 1 },
        ]
    );
}

#[test]
fn every_theme_retains_all_semantic_roles_and_exact_palette_colors() {
    for theme in SYNTAX_THEME_IDS {
        assert_eq!(SyntaxThemeId::from_name(theme.as_str()), Some(theme));
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            let colors = resolve_syntax_colors(theme, scheme);
            let rules = create_highlight_style(colors);
            for (role, rule) in Style::ALL.into_iter().zip(rules) {
                assert_eq!(rule.role, role);
                assert_eq!(rule.color, colors.color(role));
                assert_eq!(rule.color.len(), 7);
                assert!(rule.color.starts_with('#'));
                assert!(rule.color[1..].bytes().all(|byte| byte.is_ascii_hexdigit()));
                assert_eq!(Style::from_name(role.as_str()), Some(role));
            }
        }
    }
    assert_eq!(
        resolve_syntax_colors(SyntaxThemeId::Github, ColorScheme::Dark).color(Style::Keyword),
        "#ff7b72"
    );
    assert_eq!(
        resolve_syntax_colors(SyntaxThemeId::Github, ColorScheme::Light).color(Style::String),
        "#0a3069"
    );
    assert_eq!(
        resolve_syntax_colors(SyntaxThemeId::Nord, ColorScheme::Light).color(Style::Comment),
        "#6b7280"
    );
    assert_eq!(
        resolve_syntax_colors(SyntaxThemeId::Dracula, ColorScheme::Light),
        resolve_syntax_colors(SyntaxThemeId::Dracula, ColorScheme::Dark)
    );
}
