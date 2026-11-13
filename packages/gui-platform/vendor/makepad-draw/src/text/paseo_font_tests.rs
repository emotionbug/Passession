//! Runtime font redefinition, variable instances, collection faces and
//! invalid-face skipping (see VENDORED.md "Runtime font families").

use {
    super::{
        font::FontId,
        font_face::FontFace,
        font_family::FontFamilyId,
        layouter::{BorrowedLayoutParams, LaidoutText, LayoutOptions, Layouter, Settings, Span, Style},
        loader::{FontDefinition, FontFamilyDefinition},
    },
    std::{rc::Rc, sync::Arc},
};

const MONO: &[u8] = include_bytes!("../../../makepad-widgets/resources/LiberationMono-Regular.ttf");
const SANS: &[u8] = include_bytes!("../../../makepad-widgets/resources/NotoSans-Regular.ttf");

fn definition(data: Vec<u8>, index: u32, variations: Vec<([u8; 4], f32)>) -> FontDefinition {
    FontDefinition {
        data: Arc::new(data),
        index,
        variations,
        ascender_fudge_in_ems: 0.0,
        descender_fudge_in_ems: 0.0,
    }
}

fn layout(layouter: &mut Layouter, family: FontFamilyId, text: &str) -> Rc<LaidoutText> {
    let style = Style { font_family_id: family, font_size_in_pts: 12.0, color: None };
    layouter.get_or_layout(BorrowedLayoutParams {
        text,
        spans: &[Span { style, len: text.len() }],
        options: LayoutOptions::default(),
    })
}

fn glyph_fonts(text: &LaidoutText) -> Vec<FontId> {
    text.rows.iter().flat_map(|row| &row.glyphs).map(|glyph| glyph.font.id()).collect()
}

/// A local font file named by `var`, else `default` when it exists. These
/// fixtures are system fonts that cannot be vendored; absent ones skip.
fn system_fixture(var: &str, default: &str) -> Option<Vec<u8>> {
    let path = std::env::var(var).unwrap_or_else(|_| default.to_owned());
    match std::fs::read(&path) {
        Ok(data) => Some(data),
        Err(_) => {
            eprintln!("skipping: set {var} to a font file ({path} is unavailable)");
            None
        }
    }
}

#[test]
fn redefining_a_family_replaces_cached_layout_output() {
    let mut layouter = Layouter::new(Settings::default());
    let (mono, sans, family) = (FontId::from(9101), FontId::from(9102), FontFamilyId::from(9103));
    layouter.define_font(mono, definition(MONO.to_vec(), 0, Vec::new()));
    layouter.define_font(sans, definition(SANS.to_vec(), 0, Vec::new()));
    layouter.define_font_family(family, FontFamilyDefinition { font_ids: vec![mono] });

    let before = layout(&mut layouter, family, "Illuminate");
    assert!(glyph_fonts(&before).iter().all(|&id| id == mono));

    layouter.redefine_font_family(family, FontFamilyDefinition { font_ids: vec![sans, mono] });
    let after = layout(&mut layouter, family, "Illuminate");
    assert!(!Rc::ptr_eq(&before, &after), "cached layout survived the redefinition");
    assert!(glyph_fonts(&after).iter().all(|&id| id == sans));
    assert_ne!(before.size_in_lpxs.width, after.size_in_lpxs.width);
}

#[test]
fn a_face_that_does_not_parse_is_skipped_by_its_family() {
    let mut layouter = Layouter::new(Settings::default());
    let (broken, missing_index, mono, family) =
        (FontId::from(9201), FontId::from(9202), FontId::from(9203), FontFamilyId::from(9204));
    layouter.define_font(broken, definition(b"not a font".to_vec(), 0, Vec::new()));
    layouter.define_font(missing_index, definition(MONO.to_vec(), 7, Vec::new()));
    layouter.define_font(mono, definition(MONO.to_vec(), 0, Vec::new()));
    layouter.define_font_family(family, FontFamilyDefinition { font_ids: vec![broken, missing_index, mono] });

    let text = layout(&mut layouter, family, "fallback");
    let fonts = glyph_fonts(&text);
    assert_eq!(fonts.len(), "fallback".len());
    assert!(fonts.iter().all(|&id| id == mono));
    // A second family listing the same broken face does not retry or panic.
    let other = FontFamilyId::from(9205);
    layouter.define_font_family(other, FontFamilyDefinition { font_ids: vec![broken, mono] });
    assert!(glyph_fonts(&layout(&mut layouter, other, "x")).iter().all(|&id| id == mono));
}

#[test]
fn a_wght_variation_changes_advance_widths() {
    let Some(data) = system_fixture(
        "MAKEPAD_TEST_VARIABLE_FONT",
        "/usr/share/fonts/truetype/ubuntu/UbuntuSans[wdth,wght].ttf",
    ) else {
        return;
    };
    let face = FontFace::from_data_and_index(Arc::new(data.clone()), 0).expect("fixture parses");
    let axis = face
        .as_ttf_parser_face()
        .variation_axes()
        .into_iter()
        .find(|axis| axis.tag.to_bytes() == *b"wght")
        .expect("fixture has a wght axis");

    let mut layouter = Layouter::new(Settings::default());
    let (light, heavy) = (FontId::from(9301), FontId::from(9302));
    let (light_family, heavy_family) = (FontFamilyId::from(9303), FontFamilyId::from(9304));
    layouter.define_font(light, definition(data.clone(), 0, vec![(*b"wght", axis.min_value)]));
    layouter.define_font(heavy, definition(data, 0, vec![(*b"wght", axis.max_value)]));
    layouter.define_font_family(light_family, FontFamilyDefinition { font_ids: vec![light] });
    layouter.define_font_family(heavy_family, FontFamilyDefinition { font_ids: vec![heavy] });

    let light = layout(&mut layouter, light_family, "WWWWmmmm");
    let heavy = layout(&mut layouter, heavy_family, "WWWWmmmm");
    assert!(
        heavy.size_in_lpxs.width > light.size_in_lpxs.width,
        "wght {} -> {} did not widen: {} vs {}",
        axis.min_value,
        axis.max_value,
        light.size_in_lpxs.width,
        heavy.size_in_lpxs.width,
    );
}

#[test]
fn a_collection_face_above_index_zero_loads_and_shapes() {
    let Some(data) = system_fixture(
        "MAKEPAD_TEST_FONT_COLLECTION",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ) else {
        return;
    };
    let data = Arc::new(data);
    assert!(FontFace::from_data_and_index(data.clone(), 1).is_some());

    let mut layouter = Layouter::new(Settings::default());
    let (face, family) = (FontId::from(9401), FontFamilyId::from(9402));
    layouter.define_font(face, FontDefinition {
        data,
        index: 1,
        variations: Vec::new(),
        ascender_fudge_in_ems: 0.0,
        descender_fudge_in_ems: 0.0,
    });
    layouter.define_font_family(family, FontFamilyDefinition { font_ids: vec![face] });
    let text = layout(&mut layouter, family, "한글");
    let glyphs: Vec<_> = text.rows.iter().flat_map(|row| &row.glyphs).collect();
    assert_eq!(glyphs.len(), 2);
    assert!(glyphs.iter().all(|glyph| glyph.font.id() == face && glyph.id != 0));
}
