//! Operating-system fonts for the theme's text styles.
//!
//! Every Paseo and stock text style resolves to one of five DSL font families
//! (`THEME_FONT_REGULAR`, `_BOLD`, `_ITALIC`, `_BOLD_ITALIC`, `_CODE`), each a
//! bundled fallback chain. [`SystemFontState`] resolves UI/mono preferences on a
//! worker thread and installs a missing-glyph queue shared by both layout paths.
//! Missing characters in any script resolve through the OS off-thread; completed
//! faces extend the role's chain and invalidate layout/theme epochs. Latin mono,
//! bundled emoji and the original last-resort faces keep their existing priority.
//!
//! System faces get ascender/descender fudges so their line height equals the
//! bundled face they precede, keeping authored line spacing stable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use makepad_widgets::makepad_draw::text::fallback::{FallbackRequests, MissingGlyph};
use makepad_widgets::makepad_draw::text::font::FontId;
use makepad_widgets::makepad_draw::text::font_face::FontFace as ParsedFace;
use makepad_widgets::makepad_draw::text::font_family::FontFamilyId;
use makepad_widgets::makepad_draw::text::fonts::Fonts;
use makepad_widgets::makepad_draw::text::loader::{FontDefinition, FontFamilyDefinition};
use makepad_widgets::*;
use paseo_gui_platform::appearance::{self, FontFace, FontRequest, Role, SystemFonts};
use serde_json::Value;

live_design! {
    use link::theme::*;

    // Never drawn: applying these styles defines the five redefinable families
    // and exposes their ids.
    pub PaseoFontRoles = {{FontRoles}} {
        regular: { text_style: <THEME_FONT_REGULAR> {} }
        bold: { text_style: <THEME_FONT_BOLD> {} }
        italic: { text_style: <THEME_FONT_ITALIC> {} }
        bold_italic: { text_style: <THEME_FONT_BOLD_ITALIC> {} }
        code: { text_style: <THEME_FONT_CODE> {} }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct FontRoles {
    #[live]
    regular: DrawText,
    #[live]
    bold: DrawText,
    #[live]
    italic: DrawText,
    #[live]
    bold_italic: DrawText,
    #[live]
    code: DrawText,
}

impl FontRoles {
    /// Family ids in [`Role`] order.
    fn family_ids(&self) -> [FontFamilyId; ROLES] {
        [
            &self.regular,
            &self.bold,
            &self.italic,
            &self.bold_italic,
            &self.code,
        ]
        .map(|draw| draw.text_style.font_family.font_family_id())
    }
}

const ROLES: usize = 5;
/// UI primary and eager Hangul fallback; all other scripts resolve on demand.
const SLOTS: usize = 2;
/// Line metrics in ems, as `Font::{ascender,descender,line_gap}_in_ems` report them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Metrics {
    pub ascender: f32,
    pub descender: f32,
    pub line_gap: f32,
}

/// A validated system face prepared off the UI thread.
#[derive(Clone, Debug)]
struct Prepared {
    face: FontFace,
    content: u64,
    natural: Metrics,
}

#[derive(Debug)]
struct Resolved {
    request: FontRequest,
    /// `[role][slot]`; all roles share the same script order.
    slots: [[Option<Prepared>; SLOTS]; ROLES],
    ui_family_name: Option<String>,
    mono_family_name: Option<String>,
}

#[derive(Default)]
pub struct SystemFontState {
    requested: Option<FontRequest>,
    pending: Option<Receiver<Resolved>>,
    /// Bundled chains captured before the first redefinition, by family.
    original: Option<[(FontFamilyId, Vec<FontId>); ROLES]>,
    fallback_requests: Option<Arc<FallbackRequests>>,
    fallback_pending: Option<Receiver<Vec<(FontFamilyId, Prepared)>>>,
}

impl SystemFontState {
    /// Starts resolving fonts for `preferences` unless that exact request is
    /// already applied or in flight. `refresh` re-resolves anyway (OS change).
    pub fn request(&mut self, preferences: &Value, refresh: bool) {
        let text = |key: &str| {
            preferences
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        let request = FontRequest::from_css(
            &text("uiFontFamily"),
            &text("monoFontFamily"),
            &appearance::system_language(),
        );
        if !refresh && self.requested.as_ref() == Some(&request) {
            return;
        }
        self.requested = Some(request.clone());
        // Dropping old receivers fences both positive and negative discoveries
        // across preference/locale/appearance changes, including same-name fonts.
        self.fallback_requests = None;
        self.fallback_pending = None;
        let (sender, receiver) = mpsc::channel();
        self.pending = Some(receiver);
        let spawned = std::thread::Builder::new()
            .name("paseo-system-fonts".into())
            .spawn(move || {
                let resolved = prepare(request, appearance::system_fonts);
                if sender.send(resolved).is_ok() {
                    SignalToUI::set_ui_signal();
                }
            });
        if let Err(error) = spawned {
            log!("system fonts: worker did not start ({error}); keeping bundled fonts");
            self.pending = None;
        }
    }

    /// Applies a finished resolution, if any (call on `Event::Signal`).
    pub fn poll(&mut self, cx: &mut Cx, roles: &FontRoles) {
        if let Some(receiver) = &self.pending {
            match receiver.try_recv() {
                Ok(resolved) => {
                    self.pending = None;
                    self.apply(cx, roles, resolved);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }
        self.poll_fallbacks(cx, roles);
    }

    fn poll_fallbacks(&mut self, cx: &mut Cx, roles: &FontRoles) {
        if let Some(receiver) = &self.fallback_pending {
            match receiver.try_recv() {
                Ok(resolved) => {
                    self.fallback_pending = None;
                    let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
                    let mut fonts = fonts.borrow_mut();
                    for (family, prepared) in resolved {
                        let Some(mut definition) = fonts.font_family_definition(family).cloned()
                        else {
                            continue;
                        };
                        let reference = definition.font_ids.first().copied();
                        let metrics =
                            reference
                                .and_then(|id| fonts.get_or_load_font(id))
                                .map(|font| Metrics {
                                    ascender: font.ascender_in_ems(),
                                    descender: font.descender_in_ems(),
                                    line_gap: font.line_gap_in_ems(),
                                });
                        let (ascender_fudge, descender_fudge) = metrics
                            .map_or((0.0, 0.0), |metrics| line_fudges(prepared.natural, metrics));
                        let id = system_font_id(prepared.content, reference);
                        if !fonts.is_font_known(id) {
                            log!(
                                "system font fallback: family={:?} source={}",
                                family,
                                prepared.face.source
                            );
                            fonts.define_font(
                                id,
                                FontDefinition {
                                    data: prepared.face.data,
                                    index: prepared.face.index,
                                    variations: prepared.face.variations,
                                    ascender_fudge_in_ems: ascender_fudge,
                                    descender_fudge_in_ems: descender_fudge,
                                },
                            );
                        }
                        if !definition.font_ids.contains(&id) {
                            definition.font_ids.push(id);
                            fonts.redefine_font_family(family, definition);
                        }
                    }
                    // Also retry misses omitted by the bounded queue. Negative
                    // results stay deduplicated until the appearance epoch resets.
                    for family in roles.family_ids() {
                        if let Some(definition) = fonts.font_family_definition(family).cloned() {
                            fonts.redefine_font_family(family, definition);
                        }
                    }
                    drop(fonts);
                    crate::theme::bump_epoch();
                    // Font metrics change every layer's layout.
                    #[allow(clippy::disallowed_methods)]
                    cx.redraw_all();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.fallback_pending = None;
                    log!("system fonts: fallback worker disconnected");
                }
            }
        }
        let (Some(requests), Some(request)) = (&self.fallback_requests, &self.requested) else {
            return;
        };
        let (missing, _) = requests.drain();
        if missing.is_empty() {
            return;
        }
        let request = request.clone();
        let families = roles.family_ids();
        let (sender, receiver) = mpsc::channel();
        self.fallback_pending = Some(receiver);
        if let Err(error) = std::thread::Builder::new()
            .name("paseo-font-fallback".into())
            .spawn(move || {
                let resolved = prepare_fallbacks(&request, &families, &missing);
                if sender.send(resolved).is_ok() {
                    SignalToUI::set_ui_signal();
                }
            })
        {
            self.fallback_pending = None;
            log!("system fonts: fallback worker did not start ({error})");
        }
    }

    fn apply(&mut self, cx: &mut Cx, roles: &FontRoles, resolved: Resolved) {
        // An older worker finishing after a newer request is not applied.
        if self.requested.as_ref() != Some(&resolved.request) {
            return;
        }
        let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
        let mut fonts = fonts.borrow_mut();
        let original = match &self.original {
            Some(original) => original.clone(),
            None => {
                let ids = roles.family_ids();
                let Some(chains) = ids
                    .iter()
                    .map(|id| {
                        fonts
                            .font_family_definition(*id)
                            .map(|definition| (*id, definition.font_ids.clone()))
                    })
                    .collect::<Option<Vec<_>>>()
                else {
                    log!(
                        "system fonts: theme font families are not defined; keeping bundled fonts"
                    );
                    return;
                };
                let chains: [(FontFamilyId, Vec<FontId>); ROLES] =
                    chains.try_into().expect("one chain per role");
                self.original = Some(chains.clone());
                chains
            }
        };
        let requests = Arc::new(FallbackRequests::new(roles.family_ids().to_vec()));
        fonts.set_fallback_requests(requests.clone());
        self.fallback_requests = Some(requests);
        let mut bundled_metrics: HashMap<FontId, Option<Metrics>> = HashMap::new();
        for (role, ((family, bundled), slots)) in original.iter().zip(&resolved.slots).enumerate() {
            let mut system = [None; SLOTS];
            for (slot, prepared) in slots.iter().enumerate() {
                let Some(prepared) = prepared else { continue };
                // Every proportional CJK face fits the bundled Korean metrics,
                // never the emoji/symbol face at a coincidentally matching index.
                let reference_slot = usize::from(slot > 0 && role != ROLES - 1);
                let reference = bundled.get(reference_slot).or(bundled.first()).copied();
                let reference_metrics = reference.and_then(|id| {
                    *bundled_metrics.entry(id).or_insert_with(|| {
                        fonts.get_or_load_font(id).map(|font| Metrics {
                            ascender: font.ascender_in_ems(),
                            descender: font.descender_in_ems(),
                            line_gap: font.line_gap_in_ems(),
                        })
                    })
                });
                let (ascender_fudge, descender_fudge) = reference_metrics
                    .map_or((0.0, 0.0), |reference| {
                        line_fudges(prepared.natural, reference)
                    });
                let id = system_font_id(prepared.content, reference);
                if !fonts.is_font_known(id) {
                    fonts.define_font(
                        id,
                        FontDefinition {
                            data: prepared.face.data.clone(),
                            index: prepared.face.index,
                            variations: prepared.face.variations.clone(),
                            ascender_fudge_in_ems: ascender_fudge,
                            descender_fudge_in_ems: descender_fudge,
                        },
                    );
                }
                system[slot] = Some(id);
            }
            let font_ids = compose(&system, bundled);
            fonts.redefine_font_family(*family, FontFamilyDefinition { font_ids });
        }
        drop(fonts);
        let sources: Vec<&str> = resolved
            .slots
            .iter()
            .flatten()
            .flatten()
            .map(|p| p.face.source.as_str())
            .collect();
        log!(
            "system fonts: ui={:?} mono={:?} faces={:?}",
            resolved.ui_family_name,
            resolved.mono_family_name,
            sources
        );
        crate::theme::bump_epoch();
        // Font metrics change every layer's layout.
        #[allow(clippy::disallowed_methods)]
        cx.redraw_all();
    }
}

/// Worker side: resolves, validates metrics and hashes face contents.
fn prepare(request: FontRequest, resolve: impl FnOnce(&FontRequest) -> SystemFonts) -> Resolved {
    let fonts = resolve(&request);
    let mut contents: HashMap<usize, u64> = HashMap::new();
    let mut prepare =
        |face: Option<FontFace>| face.and_then(|face| prepare_face(face, &mut contents));
    let SystemFonts {
        ui,
        hangul,
        mono,
        ui_family_name,
        mono_family_name,
    } = fonts;
    let slots = [
        [prepare(ui.regular), prepare(hangul.regular)],
        [prepare(ui.semibold), prepare(hangul.semibold)],
        [prepare(ui.italic), prepare(hangul.italic)],
        [prepare(ui.bold_italic), prepare(hangul.bold_italic)],
        [prepare(mono), None],
    ];
    Resolved {
        request,
        slots,
        ui_family_name,
        mono_family_name,
    }
}

fn prepare_face(face: FontFace, contents: &mut HashMap<usize, u64>) -> Option<Prepared> {
    let natural = natural_metrics(&face)?;
    let data_hash = *contents
        .entry(Arc::as_ptr(&face.data) as usize)
        .or_insert_with(|| {
            let mut hasher = DefaultHasher::new();
            face.data.hash(&mut hasher);
            hasher.finish()
        });
    let mut hasher = DefaultHasher::new();
    data_hash.hash(&mut hasher);
    face.index.hash(&mut hasher);
    for (tag, value) in &face.variations {
        tag.hash(&mut hasher);
        value.to_bits().hash(&mut hasher);
    }
    Some(Prepared {
        content: hasher.finish(),
        face,
        natural,
    })
}

fn prepare_fallbacks(
    request: &FontRequest,
    families: &[FontFamilyId; ROLES],
    missing: &[MissingGlyph],
) -> Vec<(FontFamilyId, Prepared)> {
    let probes: Vec<_> = missing
        .iter()
        .map(|missing| {
            let role = match families.iter().position(|family| *family == missing.family) {
                Some(1) => Role::Semibold,
                Some(2) => Role::Italic,
                Some(3) => Role::BoldItalic,
                _ => Role::Regular,
            };
            (role, missing.character)
        })
        .collect();
    let faces = appearance::fallback_fonts(request, &probes);
    let mut contents = HashMap::new();
    let mut resolved = Vec::new();
    for (missing, face) in missing.iter().zip(faces) {
        let Some(prepared) = face.and_then(|face| prepare_face(face, &mut contents)) else {
            continue;
        };
        if !resolved
            .iter()
            .any(|(family, known): &(FontFamilyId, Prepared)| {
                *family == missing.family && known.content == prepared.content
            })
        {
            resolved.push((missing.family, prepared));
        }
    }
    resolved
}

/// Unfudged line metrics of a face at its variation instance; `None` when it does not parse.
fn natural_metrics(face: &FontFace) -> Option<Metrics> {
    let parsed = ParsedFace::from_data_index_and_variations(
        face.data.clone(),
        face.index,
        &face.variations,
    )?;
    let face = parsed.as_ttf_parser_face();
    let units = f32::from(face.units_per_em());
    (units > 0.0).then(|| Metrics {
        ascender: f32::from(face.ascender()) / units,
        descender: f32::from(face.descender()) / units,
        line_gap: f32::from(face.line_gap()) / units,
    })
}

/// A system face's id: its contents plus the bundled face its fudges were fitted to.
fn system_font_id(content: u64, reference: Option<FontId>) -> FontId {
    let mut hasher = DefaultHasher::new();
    "paseo-system-font".hash(&mut hasher);
    content.hash(&mut hasher);
    reference.hash(&mut hasher);
    FontId::from(hasher.finish())
}

/// Ascender/descender fudges (ems) that scale `natural` about the baseline so
/// ascender − descender + line gap equals `reference`'s. Bounded to half/double
/// size so a broken metrics table cannot collapse or explode rows.
pub(crate) fn line_fudges(natural: Metrics, reference: Metrics) -> (f32, f32) {
    let target = reference.ascender - reference.descender + reference.line_gap;
    let span = natural.ascender - natural.descender;
    if !(span > 0.0 && target.is_finite() && target > 0.0) {
        return (0.0, 0.0);
    }
    let scale = ((target - natural.line_gap) / span).clamp(0.5, 2.0);
    (
        natural.ascender * (scale - 1.0),
        natural.descender * (scale - 1.0),
    )
}

/// System primary faces precede the bundled chain. Lazy fallback faces are
/// appended later, so they cannot replace an existing Latin or emoji glyph.
fn compose(system: &[Option<FontId>], bundled: &[FontId]) -> Vec<FontId> {
    let mut ids = Vec::with_capacity(system.len() + bundled.len());
    for id in system.iter().flatten().chain(bundled) {
        if !ids.contains(id) {
            ids.push(*id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLEX: &[u8] =
        include_bytes!("../../gui-platform/vendor/makepad-widgets/resources/IBMPlexSans-Text.ttf");
    const NOTO: &[u8] = include_bytes!("../resources/fonts/NotoSansKR-Regular.otf");

    fn face(data: &[u8]) -> FontFace {
        FontFace {
            data: Arc::new(data.to_vec()),
            index: 0,
            variations: Vec::new(),
            source: "fixture".into(),
        }
    }

    fn height(metrics: Metrics) -> f32 {
        metrics.ascender - metrics.descender + metrics.line_gap
    }

    #[test]
    fn fudged_line_height_matches_the_bundled_reference() {
        let natural = natural_metrics(&face(NOTO)).unwrap();
        // The bundled Plex regular face with its DSL `-0.1` ascender fudge.
        let plex = natural_metrics(&face(PLEX)).unwrap();
        let reference = Metrics {
            ascender: plex.ascender - 0.1,
            ..plex
        };
        let (ascender, descender) = line_fudges(natural, reference);
        let fitted = Metrics {
            ascender: natural.ascender + ascender,
            descender: natural.descender + descender,
            ..natural
        };
        assert!(
            (height(fitted) - height(reference)).abs() < 1e-5,
            "{fitted:?} vs {reference:?}"
        );
        // Scaling about the baseline keeps the ascender/descender proportion.
        assert!(
            (fitted.ascender / fitted.descender - natural.ascender / natural.descender).abs()
                < 1e-4
        );
    }

    #[test]
    fn degenerate_metrics_get_no_fudge() {
        let flat = Metrics {
            ascender: 0.0,
            descender: 0.0,
            line_gap: 0.0,
        };
        let normal = Metrics {
            ascender: 0.9,
            descender: -0.25,
            line_gap: 0.0,
        };
        assert_eq!(line_fudges(flat, normal), (0.0, 0.0));
        assert_eq!(line_fudges(normal, flat), (0.0, 0.0));
        // A face 10x too tall is only shrunk to half.
        let huge = Metrics {
            ascender: 9.0,
            descender: -2.5,
            line_gap: 0.0,
        };
        let (ascender, _) = line_fudges(huge, normal);
        assert!((huge.ascender + ascender - 4.5).abs() < 1e-5);
    }

    #[test]
    fn system_faces_precede_the_bundled_chain_once() {
        let [a, b, plex, kr, emoji] = [1u64, 2, 3, 4, 5].map(FontId::from);
        assert_eq!(
            compose(&[Some(a), Some(b)], &[plex, kr, emoji]),
            [a, b, plex, kr, emoji]
        );
        assert_eq!(compose(&[None, Some(b)], &[plex, kr]), [b, plex, kr]);
        assert_eq!(compose(&[Some(a), Some(a)], &[plex, a]), [a, plex]);
        assert_eq!(compose(&[None, None], &[plex]), [plex]);
    }

    #[test]
    fn prepare_validates_hashes_and_places_role_slots() {
        let request = FontRequest::from_css("'Inter', system-ui", "", "ko-KR");
        let resolved = prepare(request.clone(), |seen| {
            assert_eq!(seen, &request);
            let mut fonts = SystemFonts::default();
            fonts.ui.regular = Some(face(PLEX));
            fonts.ui.semibold = Some(FontFace {
                variations: vec![(*b"wght", 600.0)],
                ..face(PLEX)
            });
            fonts.hangul.regular = Some(face(NOTO));
            fonts.mono = Some(face(b"not a font"));
            fonts
        });
        assert_eq!(resolved.request, request);
        let [regular, hangul] = &resolved.slots[0];
        let (regular, hangul) = (regular.as_ref().unwrap(), hangul.as_ref().unwrap());
        assert_ne!(regular.content, hangul.content);
        // Variation coordinates distinguish otherwise identical bytes.
        assert_ne!(
            resolved.slots[1][0].as_ref().unwrap().content,
            regular.content
        );
        assert!(resolved.slots[1][1].is_none());
        assert!(resolved.slots[2].iter().all(Option::is_none));
        // A face that does not parse is dropped, never defined.
        assert!(resolved.slots[4][0].is_none());
        let reference = Some(FontId::from(7));
        assert_eq!(
            system_font_id(regular.content, reference),
            system_font_id(regular.content, reference)
        );
        assert_ne!(
            system_font_id(regular.content, reference),
            system_font_id(regular.content, None)
        );
    }

    fn collection_face() -> FontFace {
        let mut data = b"ttcf\0\x01\0\0\0\0\0\x02".to_vec();
        data.resize(20, 0);
        for (index, bytes) in [PLEX, NOTO].into_iter().enumerate() {
            while !data.len().is_multiple_of(4) {
                data.push(0);
            }
            let base = data.len();
            data[12 + index * 4..16 + index * 4].copy_from_slice(&(base as u32).to_be_bytes());
            data.extend_from_slice(bytes);
            let count = u16::from_be_bytes(bytes[4..6].try_into().unwrap()) as usize;
            // TTC table offsets are absolute, not relative to each face.
            for table in 0..count {
                let at = base + 12 + table * 16 + 8;
                let offset = u32::from_be_bytes(data[at..at + 4].try_into().unwrap());
                data[at..at + 4].copy_from_slice(&(offset + base as u32).to_be_bytes());
            }
        }
        FontFace {
            data: Arc::new(data),
            index: 1,
            variations: Vec::new(),
            source: "fixture TTC#1".into(),
        }
    }

    fn check_lazy_fallback(
        mono: &[u8],
        text: &str,
        resolve: impl FnOnce(&[MissingGlyph], FontFamilyId) -> Vec<(FontFamilyId, Prepared)>,
    ) {
        use makepad_widgets::makepad_draw::text::{
            geom::Size,
            layouter::{BorrowedLayoutParams, LayoutOptions, Layouter, Settings, Span, Style},
        };
        let mut settings = Settings::default();
        settings.loader.rasterizer.grayscale_atlas_size = Size::new(1, 1);
        settings.loader.rasterizer.color_atlas_size = Size::new(1, 1);
        let mut layouter = Layouter::new(settings);
        let primary = FontId::from(8201);
        let family = FontFamilyId::from(8202);
        let requests = Arc::new(FallbackRequests::new(vec![family]));
        layouter.set_fallback_requests(Some(requests.clone()));
        layouter.define_font(
            primary,
            FontDefinition {
                data: Arc::new(mono.to_vec()),
                index: 0,
                variations: Vec::new(),
                ascender_fudge_in_ems: 0.0,
                descender_fudge_in_ems: 0.0,
            },
        );
        layouter.define_font_family(
            family,
            FontFamilyDefinition {
                font_ids: vec![primary],
            },
        );
        let spans = [Span {
            style: Style {
                font_family_id: family,
                font_size_in_pts: 12.0,
                color: None,
            },
            len: text.len(),
        }];
        let params = BorrowedLayoutParams {
            text,
            spans: &spans,
            options: LayoutOptions::default(),
        };
        let before = layouter.get_or_layout(params);
        let (missing, overflow) = requests.drain();
        assert!(!overflow);
        assert!(
            !missing.is_empty(),
            "fixture exercises missing-glyph discovery"
        );
        let mut ids = vec![primary];
        for (seen, prepared) in resolve(&missing, family) {
            assert_eq!(seen, family);
            let id = system_font_id(prepared.content, Some(primary));
            if ids.contains(&id) {
                continue;
            }
            layouter.define_font(
                id,
                FontDefinition {
                    data: prepared.face.data,
                    index: prepared.face.index,
                    variations: prepared.face.variations,
                    ascender_fudge_in_ems: 0.0,
                    descender_fudge_in_ems: 0.0,
                },
            );
            ids.push(id);
        }
        layouter.redefine_font_family(family, FontFamilyDefinition { font_ids: ids });
        let after = layouter.get_or_layout(params);
        assert!(
            !Rc::ptr_eq(&before, &after),
            "missing layout was invalidated"
        );
        let glyphs = &after.rows[0].glyphs;
        let cell = glyphs[0].advance_in_ems;
        for glyph in glyphs {
            let ch = text[glyph.cluster..].chars().next().unwrap();
            assert_ne!(glyph.id, 0, "missing {ch} after OS fallback");
            if ch.is_ascii() {
                assert_eq!(
                    glyph.font.id(),
                    primary,
                    "fallback replaced mono Latin {ch}"
                );
                assert!(
                    (glyph.advance_in_ems - cell).abs() < 1e-5,
                    "non-mono Latin {ch}"
                );
            }
            if "中文漢字あいうアイウ한글".contains(ch) {
                assert!(
                    glyph.advance_in_ems >= cell && glyph.advance_in_ems <= cell * 2.1,
                    "CJK advance for {ch}"
                );
            }
            if !ch.is_whitespace() {
                assert!(
                    glyph.font.glyph_outline(glyph.id).is_some(),
                    "missing outline for {ch}"
                );
            }
        }
    }

    #[test]
    fn lazy_code_fallback_preserves_mono_and_collection_outlines() {
        let jetendard = include_bytes!("../resources/fonts/Jetendard-Regular.ttf");
        let user = include_bytes!(
            "../../gui-platform/vendor/makepad-widgets/resources/LiberationMono-Regular.ttf"
        );
        for mono in [jetendard.as_slice(), user.as_slice()] {
            check_lazy_fallback(
                mono,
                "+ 123 daemon workflow step 中文漢字 あいう アイウ 한글",
                |_, family| {
                    let prepared = prepare_face(collection_face(), &mut HashMap::new()).unwrap();
                    assert_eq!(prepared.face.index, 1);
                    vec![(family, prepared)]
                },
            );
        }
    }

    #[test]
    fn installed_system_fallback_shapes_unfamiliar_scripts() {
        // Explicit input makes missing system fixtures fail instead of silently
        // skipping; native/emulator smoke can require its installed languages.
        let Ok(text) = std::env::var("PASEO_TEST_FALLBACK_TEXT") else {
            eprintln!("set PASEO_TEST_FALLBACK_TEXT to require installed-script coverage");
            return;
        };
        let text = format!("+ 123 daemon workflow step {text}");
        let mono = include_bytes!("../resources/fonts/Jetendard-Regular.ttf");
        for role in 0..ROLES {
            check_lazy_fallback(mono, &text, |missing, family| {
                let mut families = [10u64, 11, 12, 13, 14].map(FontFamilyId::from);
                families[role] = family;
                prepare_fallbacks(&FontRequest::from_css("", "", "en-US"), &families, missing)
            });
        }
    }
}
