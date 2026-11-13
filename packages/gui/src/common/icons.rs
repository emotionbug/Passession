//! Lucide glyphs (the font behind `lucide_icons_flutter` `*300` icons) drawn as text with
//! `PASEO_FONT_ICON`. Names are Lucide kebab-case names (`square-pen`, `message-square`).

use std::collections::HashMap;

const CODEPOINTS: &str = include_str!("../../resources/icons/lucide-codepoints.json");
/// `git/forge-icon.tsx` brand marks (`components/icons/*-icon.tsx`, Flutter `forgeBrandSvgs`),
/// filled with `currentColor`, keyed by the forge manifest's `iconKind`.
const FORGE_BRANDS: &str = include_str!("../../resources/icons/forge-brands.json");

thread_local! {
    static GLYPHS: HashMap<String, char> = {
        let map: HashMap<String, u32> =
            serde_json::from_str(CODEPOINTS).expect("bundled lucide codepoints are valid JSON");
        map.into_iter().filter_map(|(name, code)| char::from_u32(code).map(|c| (name, c))).collect()
    };
    static BRANDS: HashMap<String, String> =
        serde_json::from_str(FORGE_BRANDS).expect("bundled forge brand marks are valid JSON");
}

/// The glyph of a Lucide icon, `None` for an unknown name.
pub fn glyph(name: &str) -> Option<char> {
    GLYPHS.with(|glyphs| glyphs.get(name).copied())
}

/// The glyph as a one-character string; unknown names render the Lucide `circle-help`.
pub fn text(name: &str) -> String {
    glyph(name).or_else(|| glyph("circle-help")).map(String::from).unwrap_or_default()
}

/// `ForgeBrandIcon`: the SVG brand mark of a forge (`github` for an empty kind), `None` for an
/// unknown forge (callers fall back to the `git-pull-request` glyph). Draw it with
/// `MenuItem::svg` or `ProviderIcon::paint(.., Some(svg), ..)`.
pub fn forge_brand_svg(kind: &str) -> Option<String> {
    let kind = if kind.is_empty() { "github" } else { kind };
    BRANDS.with(|brands| brands.get(kind).cloned())
}

/// Flutter shell `_icon(kind)` for tab kinds.
pub fn for_tab_kind(kind: &str) -> &'static str {
    match kind {
        "draft" => "square-pen",
        "agent" | "provider_subagent" => "message-square",
        "files" => "files",
        "file" => "file",
        "changes_tree" | "working_diff" => "file-diff",
        "commit_diff" => "git-commit-horizontal",
        "git_log" => "git-graph",
        "pull_request" => "git-pull-request",
        "execution_log" => "scroll-text",
        "setup" | "terminal" => "square-terminal",
        "project_settings" => "settings",
        _ => "plus",
    }
}
