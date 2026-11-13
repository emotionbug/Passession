//! OS font discovery: resolves the OS UI font, the OS Hangul font and a user
//! family list to validated sfnt bytes the Makepad text engine can load.
//!
//! Backends: fontconfig + XDG portal (Linux), DirectWrite (Windows), CoreText
//! (macOS, iOS) and `AFontMatcher` / `fonts.xml` (Android). Every face returned
//! here parses at its index with the parser Makepad itself uses, has outlines
//! and maps the role's probe character, so defining it can never panic.

use makepad_widgets::makepad_draw::text::font_face::FontFace as ParsedFace;
use std::{
    collections::HashMap,
    sync::Arc,
};

/// What to resolve. Families are unquoted, in CSS order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontRequest {
    pub ui_families: Vec<String>,
    pub mono_families: Vec<String>,
    /// BCP 47 tag (`ko-KR`); POSIX forms (`ko_KR.UTF-8`) are accepted.
    pub locale: String,
}

impl FontRequest {
    /// Builds a request from the CSS-like preference values (empty = default).
    pub fn from_css(ui: &str, mono: &str, locale: &str) -> Self {
        Self { ui_families: parse_family_list(ui), mono_families: parse_family_list(mono), locale: locale.to_owned() }
    }
}

/// One loadable face: sfnt bytes, collection index and variation coordinates.
#[derive(Clone, Debug)]
pub struct FontFace {
    pub data: Arc<Vec<u8>>,
    pub index: u32,
    /// Axis values to apply (`wght` for the requested weight, …), clamped to the font's ranges.
    pub variations: Vec<([u8; 4], f32)>,
    /// File path or description, for logs.
    pub source: String,
}

#[derive(Clone, Debug, Default)]
pub struct RoleSet {
    pub regular: Option<FontFace>,
    pub semibold: Option<FontFace>,
    pub italic: Option<FontFace>,
    pub bold_italic: Option<FontFace>,
}

#[derive(Clone, Debug, Default)]
pub struct SystemFonts {
    pub ui: RoleSet,
    pub hangul: RoleSet,
    pub mono: Option<FontFace>,
    pub ui_family_name: Option<String>,
    pub mono_family_name: Option<String>,
}

/// Blocking (fontconfig/DirectWrite/CoreText/AFontMatcher); call off the UI thread.
/// Each call reads current OS settings; the GUI deduplicates unchanged requests.
pub fn system_fonts(request: &FontRequest) -> SystemFonts {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| backend::system_fonts(request))).unwrap_or_default()
}

/// Splits a CSS `font-family` list: `'Pretendard', system-ui, "Noto Sans"`.
/// Quotes are removed; unquoted names have their whitespace collapsed.
pub fn parse_family_list(css: &str) -> Vec<String> {
    let mut families = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    let mut flush = |current: &mut String, quoted: &mut bool| {
        let name = if *quoted { current.trim().to_owned() } else { current.split_whitespace().collect::<Vec<_>>().join(" ") };
        if !name.is_empty() {
            families.push(name);
        }
        current.clear();
        *quoted = false;
    };
    for ch in css.chars() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                quoted = true;
            }
            None if ch == ',' => flush(&mut current, &mut quoted),
            None => current.push(ch),
        }
    }
    flush(&mut current, &mut quoted);
    families
}

/// A requested role: weight and slant. Semibold is the app's "bold".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Regular,
    Semibold,
    Italic,
    BoldItalic,
}

impl Role {
    /// OpenType `usWeightClass`.
    pub(crate) fn weight(self) -> u16 {
        match self {
            Role::Regular | Role::Italic => 400,
            Role::Semibold | Role::BoldItalic => 600,
        }
    }

    pub(crate) fn italic(self) -> bool {
        matches!(self, Role::Italic | Role::BoldItalic)
    }
}

/// Hangul probe: 한.
pub(crate) const HANGUL_PROBE: char = '\u{D55C}';
/// Latin probe for UI and mono faces.
pub(crate) const LATIN_PROBE: char = 'A';

/// A CSS family name, classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family<'a> {
    /// `system-ui`, `sans-serif`, `-apple-system`, …: the OS UI font.
    SystemUi,
    /// `monospace`, `ui-monospace`: the OS monospace font.
    SystemMono,
    /// An installed family name (or another CSS generic such as `serif`).
    Named(&'a str),
}

impl<'a> Family<'a> {
    pub(crate) fn classify(name: &'a str) -> Self {
        const UI: [&str; 5] = ["system-ui", "sans-serif", "-apple-system", "blinkmacsystemfont", "ui-sans-serif"];
        const MONO: [&str; 2] = ["monospace", "ui-monospace"];
        let lower = name.to_ascii_lowercase();
        if UI.contains(&lower.as_str()) {
            Family::SystemUi
        } else if MONO.contains(&lower.as_str()) {
            Family::SystemMono
        } else {
            Family::Named(name)
        }
    }
}

/// One OS font source. `resolve` drives the shared order of lookups.
pub(crate) trait Backend {
    /// The face for `family` at `role`, with the family name the OS reports.
    /// `None` when the family is not installed (no substitution).
    fn face(&mut self, family: Family<'_>, role: Role) -> Option<(FontFace, String)>;
    /// The OS Hangul face for `role`, preferring one that pairs with `ui_family`.
    fn hangul(&mut self, ui_family: Option<&str>, role: Role) -> Option<FontFace>;
}

/// The shared resolution order: the first UI family that resolves (empty list =
/// the OS UI font), the OS Hangul font, and the first mono family that resolves
/// (empty list = none, so the bundled code font stays).
pub(crate) fn resolve(backend: &mut dyn Backend, request: &FontRequest) -> SystemFonts {
    let mut fonts = SystemFonts::default();
    let system_ui = ["system-ui".to_owned()];
    let ui_families: &[String] = if request.ui_families.is_empty() { &system_ui } else { &request.ui_families };
    for name in ui_families {
        let family = Family::classify(name);
        let Some((regular, family_name)) = backend.face(family, Role::Regular) else { continue };
        fonts.ui.regular = Some(regular);
        fonts.ui.semibold = backend.face(family, Role::Semibold).map(|(face, _)| face);
        fonts.ui.italic = backend.face(family, Role::Italic).map(|(face, _)| face);
        fonts.ui.bold_italic = backend.face(family, Role::BoldItalic).map(|(face, _)| face);
        fonts.ui_family_name = Some(family_name);
        break;
    }
    let ui_family = fonts.ui_family_name.clone();
    fonts.hangul = RoleSet {
        regular: backend.hangul(ui_family.as_deref(), Role::Regular),
        semibold: backend.hangul(ui_family.as_deref(), Role::Semibold),
        italic: backend.hangul(ui_family.as_deref(), Role::Italic),
        bold_italic: backend.hangul(ui_family.as_deref(), Role::BoldItalic),
    };
    for name in &request.mono_families {
        if let Some((face, family_name)) = backend.face(Family::classify(name), Role::Regular) {
            fonts.mono = Some(face);
            fonts.mono_family_name = Some(family_name);
            break;
        }
    }
    fonts
}

/// Validates a face and completes its variation coordinates.
///
/// Rejects bytes that do not parse at `index`, faces without `glyf`/`CFF`/`CFF2`
/// outlines (bitmap-only, Type 1, PCF never parse) and faces that do not map
/// `probe`. Supplied variations are kept only for axes the font has, clamped to
/// their ranges; a missing `wght` axis value is set to `weight`, and `ital` to 1
/// for italic roles.
pub(crate) fn finish_face(
    data: Arc<Vec<u8>>,
    index: u32,
    supplied: &[([u8; 4], f32)],
    role: Role,
    probe: char,
    source: String,
) -> Option<FontFace> {
    let parsed = ParsedFace::from_data_and_index(data.clone(), index)?;
    let face = parsed.as_ttf_parser_face();
    let tables = face.tables();
    if tables.glyf.is_none() && tables.cff.is_none() && tables.cff2.is_none() {
        return None;
    }
    if face.glyph_index(probe).is_none_or(|glyph| glyph.0 == 0) {
        return None;
    }
    let mut variations = Vec::new();
    for axis in face.variation_axes() {
        let tag = axis.tag.0.to_be_bytes();
        let wanted = supplied.iter().find(|(supplied_tag, value)| *supplied_tag == tag && value.is_finite()).map(|(_, value)| *value);
        let value = match (wanted, &tag) {
            (Some(value), _) => value,
            (None, b"wght") => f32::from(role.weight()),
            (None, b"ital") if role.italic() => 1.0,
            _ => continue,
        };
        let (low, high) = (axis.min_value.min(axis.max_value), axis.max_value.max(axis.min_value));
        variations.push((tag, value.clamp(low, high)));
    }
    Some(FontFace { data, index, variations, source })
}

/// Reads font files once per resolution; roles sharing a file share its bytes.
#[derive(Default)]
#[cfg_attr(not(any(target_os = "linux", windows, target_os = "android")), allow(dead_code))]
pub(crate) struct FileCache(HashMap<String, Option<Arc<Vec<u8>>>>);

#[cfg_attr(not(any(target_os = "linux", windows, target_os = "android")), allow(dead_code))]
impl FileCache {
    pub(crate) fn read(&mut self, path: &str) -> Option<Arc<Vec<u8>>> {
        self.0.entry(path.to_owned()).or_insert_with(|| std::fs::read(path).ok().map(Arc::new)).clone()
    }
}

/// Rebuilds an sfnt from its tables (CoreText exposes tables, not files).
/// Tables are written in tag order, 4-byte aligned, with checksums.
#[cfg_attr(not(any(target_os = "macos", target_os = "ios", test)), allow(dead_code))]
pub(crate) fn build_sfnt(tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    fn checksum(bytes: &[u8]) -> u32 {
        bytes.chunks(4).fold(0u32, |sum, chunk| {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            sum.wrapping_add(u32::from_be_bytes(word))
        })
    }
    let mut sorted: Vec<&([u8; 4], Vec<u8>)> = tables.iter().collect();
    sorted.sort_by_key(|(tag, _)| *tag);
    let count = sorted.len() as u16;
    let version: u32 = if sorted.iter().any(|(tag, _)| tag == b"CFF " || tag == b"CFF2") { 0x4F54_544F } else { 0x0001_0000 };
    let entry_selector = if count == 0 { 0 } else { 15 - count.leading_zeros() as u16 };
    let search_range = if count == 0 { 0 } else { (1u16 << entry_selector).wrapping_mul(16) };
    let range_shift = count.wrapping_mul(16).wrapping_sub(search_range);
    let directory = 12 + 16 * sorted.len();
    let mut out = Vec::with_capacity(directory + sorted.iter().map(|(_, data)| data.len() + 3).sum::<usize>());
    out.extend_from_slice(&version.to_be_bytes());
    for value in [count, search_range, entry_selector, range_shift] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    let mut offset = directory;
    let mut head_offset = None;
    for (tag, data) in &sorted {
        out.extend_from_slice(tag);
        // `head.checkSumAdjustment` is zero for the table checksum and must
        // be recomputed after changing the sfnt directory and table offsets.
        let mut sum = checksum(data);
        if *tag == *b"head" && data.len() >= 12 {
            sum = sum.wrapping_sub(u32::from_be_bytes(data[8..12].try_into().unwrap()));
            head_offset = Some(offset);
        }
        out.extend_from_slice(&sum.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += data.len().next_multiple_of(4);
    }
    for (_, data) in &sorted {
        out.extend_from_slice(data);
        out.resize(out.len().next_multiple_of(4), 0);
    }
    if let Some(offset) = head_offset {
        out[offset + 8..offset + 12].fill(0);
        let adjustment = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out[offset + 8..offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    out
}

/// Family from a Pango font description (`Ubuntu Sans Bold Italic 11`).
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn pango_family(description: &str) -> Option<String> {
    const STYLE_WORDS: [&str; 30] = [
        "normal", "roman", "oblique", "italic", "small-caps", "thin", "ultra-light", "extra-light", "light", "semi-light",
        "demi-light", "book", "regular", "medium", "semi-bold", "demi-bold", "bold", "ultra-bold", "extra-bold", "heavy",
        "black", "ultra-heavy", "extra-heavy", "ultra-condensed", "extra-condensed", "condensed", "semi-condensed",
        "semi-expanded", "expanded", "extra-expanded",
    ];
    // Families may be listed before the style: "Cantarell,Noto Sans 11".
    let mut words: Vec<&str> = description.split_whitespace().collect();
    if words.last().is_some_and(|size| size.trim_end_matches("px").parse::<f64>().is_ok()) {
        words.pop();
    }
    while words.len() > 1 && words.last().is_some_and(|word| STYLE_WORDS.contains(&word.to_ascii_lowercase().as_str())) {
        words.pop();
    }
    let joined = words.join(" ");
    let family = joined.split(',').next().unwrap_or("").trim().trim_matches(['"', '\'']).trim();
    (!family.is_empty()).then(|| family.to_owned())
}

/// Family from a Qt `QFont::toString()` value (`Noto Sans,10,-1,5,50,0,0,0,0,0`).
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn qt_family(description: &str) -> Option<String> {
    let family = description.split(',').next()?.trim();
    (!family.is_empty()).then(|| family.to_owned())
}

/// `ko_KR.UTF-8` → `ko-KR`; empty stays empty.
#[cfg_attr(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios", test)), allow(dead_code))]
pub(crate) fn bcp47(locale: &str) -> String {
    let base = locale.split(['.', '@']).next().unwrap_or("");
    let base = if base == "C" || base == "POSIX" { "" } else { base };
    base.replace('_', "-")
}

#[cfg(target_os = "linux")]
use crate::desktop::linux::fonts as backend;
#[cfg(windows)]
use crate::desktop::windows::fonts as backend;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use crate::apple_fonts as backend;
#[cfg(target_os = "android")]
use crate::android::native::fonts as backend;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn css_family_list_unquotes_and_orders() {
        assert_eq!(
            parse_family_list(r#"'Pretendard',  system-ui , "Noto  Sans KR", Apple   SD Gothic Neo,,sans-serif"#),
            ["Pretendard", "system-ui", "Noto  Sans KR", "Apple SD Gothic Neo", "sans-serif"]
        );
        assert_eq!(parse_family_list("  "), Vec::<String>::new());
        assert_eq!(parse_family_list("'A, B', C"), ["A, B", "C"]);
        let request = FontRequest::from_css("", "'JetBrains Mono', monospace", "ko_KR.UTF-8");
        assert!(request.ui_families.is_empty());
        assert_eq!(request.mono_families, ["JetBrains Mono", "monospace"]);
    }

    #[test]
    fn generic_families_classify() {
        assert_eq!(Family::classify("system-ui"), Family::SystemUi);
        assert_eq!(Family::classify("-apple-system"), Family::SystemUi);
        assert_eq!(Family::classify("Sans-Serif"), Family::SystemUi);
        assert_eq!(Family::classify("ui-monospace"), Family::SystemMono);
        assert_eq!(Family::classify("serif"), Family::Named("serif"));
        assert_eq!(Family::classify("Pretendard"), Family::Named("Pretendard"));
    }

    #[test]
    fn pango_and_qt_descriptions_yield_family() {
        assert_eq!(pango_family("Ubuntu Sans 11").as_deref(), Some("Ubuntu Sans"));
        assert_eq!(pango_family("Cantarell Bold Italic 10.5").as_deref(), Some("Cantarell"));
        assert_eq!(pango_family("Noto Sans CJK KR Medium 12").as_deref(), Some("Noto Sans CJK KR"));
        assert_eq!(pango_family("Inter,Noto Sans 11").as_deref(), Some("Inter"));
        assert_eq!(pango_family("Bold"), Some("Bold".into()));
        assert_eq!(pango_family("  "), None);
        assert_eq!(qt_family("Noto Sans,10,-1,5,50,0,0,0,0,0").as_deref(), Some("Noto Sans"));
        assert_eq!(qt_family("Inter,10,-1,5,400,0,0,0,0,0,0,0,0,0,0,1,Regular").as_deref(), Some("Inter"));
        assert_eq!(qt_family(""), None);
    }

    #[test]
    fn locale_normalizes_to_bcp47() {
        assert_eq!(bcp47("ko_KR.UTF-8"), "ko-KR");
        assert_eq!(bcp47("en-US"), "en-US");
        assert_eq!(bcp47("C.UTF-8"), "");
        assert_eq!(bcp47("sr_RS@latin"), "sr-RS");
    }

    #[test]
    fn validation_rejects_non_sfnt_bytes() {
        let garbage = Arc::new(b"not a font, just some bytes that are long enough to look like a header".to_vec());
        assert!(finish_face(garbage, 0, &[], Role::Regular, LATIN_PROBE, "garbage".into()).is_none());
        assert!(finish_face(Arc::new(Vec::new()), 0, &[], Role::Regular, LATIN_PROBE, "empty".into()).is_none());
        // A syntactically valid sfnt with no tables has no outlines and no cmap.
        let empty_sfnt = Arc::new(build_sfnt(&[]));
        assert!(finish_face(empty_sfnt, 0, &[], Role::Regular, LATIN_PROBE, "empty sfnt".into()).is_none());
    }

    /// Splits an sfnt into its tables (test-only reader for `build_sfnt`).
    fn split_sfnt(data: &[u8]) -> Vec<([u8; 4], Vec<u8>)> {
        let be32 = |at: usize| u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let count = u16::from_be_bytes([data[4], data[5]]) as usize;
        (0..count)
            .map(|i| {
                let record = 12 + 16 * i;
                let tag: [u8; 4] = data[record..record + 4].try_into().unwrap();
                let (offset, length) = (be32(record + 8), be32(record + 12));
                (tag, data[offset..offset + length].to_vec())
            })
            .collect()
    }

    #[test]
    fn rebuilt_sfnt_round_trips_with_valid_checksums() {
        let bytes = include_bytes!("../vendor/makepad-widgets/resources/IBMPlexSans-Text.ttf");
        let mut tables = split_sfnt(bytes);
        // Force a different directory/layout and an obsolete adjustment.
        tables.retain(|(tag, _)| tag != b"DSIG");
        let (_, head) = tables.iter_mut().find(|(tag, _)| tag == b"head").unwrap();
        head[8..12].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        let rebuilt = build_sfnt(&tables);
        let checksum = |data: &[u8]| {
            data.chunks(4).fold(0u32, |sum, chunk| {
                let mut word = [0; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                sum.wrapping_add(u32::from_be_bytes(word))
            })
        };
        assert_eq!(checksum(&rebuilt), 0xB1B0_AFBA);
        for (i, (tag, mut data)) in split_sfnt(&rebuilt).into_iter().enumerate() {
            if tag == *b"head" {
                data[8..12].fill(0);
            }
            let at = 12 + i * 16 + 4;
            let recorded = u32::from_be_bytes(rebuilt[at..at + 4].try_into().unwrap());
            assert_eq!(recorded, checksum(&data), "{tag:?}");
        }
        let face = finish_face(Arc::new(rebuilt), 0, &[(*b"wght", 700.0)], Role::Regular, LATIN_PROBE, "rebuilt".into());
        let face = face.expect("rebuilt sfnt validates");
        assert!(face.variations.is_empty(), "static font has no axes");
    }
}
