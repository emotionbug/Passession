//! Worker-only, offline Mermaid rendering. Never export the renderer's raw SVG:
//! usvg resolves outline fonts, shapes text and serializes a canonical, path-only SVG.
//! The same tree supplies preview/PNG pixels. Diagram-controlled resource loading
//! is disabled even though usvg normally permits local image paths.

use std::sync::{Arc, Mutex};
#[cfg(target_arch = "wasm32")]
use std::{cell::OnceCell, rc::Rc};

use image::ImageEncoder;
use mermaid_rs_renderer::{self as mermaid, DiagramKind, LayoutConfig, Theme};
#[cfg(not(target_arch = "wasm32"))]
use paseo_gui_platform::appearance::{self as fonts, FontFace, FontRequest, Role};
use resvg::{tiny_skia, usvg};

use super::{Failure, FailureKind};

fn invalid(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::Invalid, detail)
}
fn unsupported(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::Unsupported, detail)
}
fn too_large(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::TooLarge, detail)
}
fn font(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::Font, detail)
}
fn worker(detail: impl Into<String>) -> Failure {
    Failure::new(FailureKind::Worker, detail)
}

const MAX_SOURCE_BYTES: usize = 32 * 1024;
const MAX_LINES: usize = 512;
const MAX_LINE_BYTES: usize = 4096;
const MAX_NODES: usize = 128;
const MAX_EDGES: usize = 512;
const MAX_DEPTH: usize = 32;
const MAX_SVG_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 8 * 1024 * 1024;

/// Straight-alpha RGBA8, plus the self-contained vector artifact it represents.
#[derive(Debug)]
pub struct Rendered {
    pub svg: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Blocking parse, font resolution, shaping and rasterization. Call only
/// from a worker. No network, browser, JavaScript or source-directed file reads.
pub fn render(source: &str, dark: bool) -> Result<Rendered, Failure> {
    validate_source(source)?;
    // A malformed streaming fence must not kill the shared render worker.
    std::panic::catch_unwind(|| render_inner(source, dark))
        .map_err(|_| invalid("The Mermaid renderer could not process this diagram"))?
}

fn render_inner(source: &str, dark: bool) -> Result<Rendered, Failure> {
    canonicalize(&diagram_svg(source, dark)?)
}

fn diagram_svg(source: &str, dark: bool) -> Result<String, Failure> {
    let mut parsed = mermaid::parse_mermaid_strict(source).map_err(|e| invalid(e.to_string()))?;
    if parsed.graph.nodes.len() > MAX_NODES || parsed.graph.edges.len() > MAX_EDGES {
        return Err(too_large(
            "Mermaid diagram has too many participants, nodes or messages",
        ));
    }
    if parsed.graph.kind == DiagramKind::Sequence {
        validate_sequence(source, &parsed.graph)?;
    }
    // Links are not part of a static diagram. Never propagate their targets to
    // SVG, even if the renderer currently escapes them correctly.
    parsed.graph.node_links.clear();
    // Do not apply init_config: untrusted directives cannot alter our resource,
    // theme, font or layout policy.
    let mut theme = if dark { Theme::dark() } else { Theme::modern() };
    theme.font_family = "sans-serif".into();
    let config = LayoutConfig::default();
    let layout = mermaid::compute_layout(&parsed.graph, &theme, &config);
    if let mermaid::layout::DiagramData::Error(error) = &layout.diagram {
        return Err(invalid(error.message.clone()));
    }
    let dimensions = mermaid::measure_svg_dimensions(&layout, &config, None);
    image_size(dimensions.width, dimensions.height)?;
    Ok(mermaid::render::render_svg_with_dimensions(
        &layout,
        &theme,
        &config,
        Some((dimensions.width, dimensions.height)),
    ))
}

fn validate_source(source: &str) -> Result<(), Failure> {
    if source.trim().is_empty() {
        return Err(invalid("Mermaid source is empty"));
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(too_large("Mermaid source exceeds the 32 KiB limit"));
    }
    let mut depth = 0usize;
    for (index, line) in source.lines().enumerate() {
        if index >= MAX_LINES || line.len() > MAX_LINE_BYTES {
            return Err(too_large(
                "Mermaid source has too many lines or an overlong line",
            ));
        }
        let line = line.trim();
        if line.starts_with("%%{") || line == "---" {
            return Err(unsupported(
                "Mermaid configuration directives and frontmatter are not supported",
            ));
        }
        if line.starts_with("%%") {
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        if [
            "alt", "opt", "loop", "par", "rect", "critical", "break", "box", "subgraph", "block",
        ]
        .iter()
        .any(|word| keyword.eq_ignore_ascii_case(word))
        {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(too_large("Mermaid nesting exceeds the 32-level limit"));
            }
        } else if keyword.eq_ignore_ascii_case("end") {
            depth = depth.saturating_sub(1);
        }
    }
    Ok(())
}

/// Upstream intentionally ignores unrecognized sequence lines. Ask its parser
/// whether each non-boundary statement has semantics rather than silently
/// showing a complete-looking preview of a truncated/unsupported message.
/// This is an admission check, not a second Mermaid grammar or layout engine.
fn validate_sequence(source: &str, graph: &mermaid::Graph) -> Result<(), Failure> {
    if graph.sequence_participants.is_empty() {
        return Err(invalid("Sequence diagram has no participants or messages"));
    }
    // The upstream parser can backtrack an unfinished `A->>` into `A->`
    // followed by a participant literally named `>`. Do not accept that as a
    // completed diagram. Human-readable punctuation belongs in an alias.
    if graph
        .sequence_participants
        .iter()
        .any(|id| id.is_empty() || id.chars().any(|c| matches!(c, '<' | '>' | ':')))
    {
        return Err(invalid(
            "Sequence diagram contains an incomplete arrow or invalid participant identifier",
        ));
    }
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.eq_ignore_ascii_case("sequenceDiagram")
        {
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        if ["end", "else", "and", "option", "autonumber"]
            .iter()
            .any(|word| keyword.eq_ignore_ascii_case(word))
        {
            continue;
        }
        let single = mermaid::parse_mermaid(&format!("sequenceDiagram\n{line}"))
            .map_err(|error| invalid(error.to_string()))?;
        if single.graph.nodes.is_empty()
            && single.graph.sequence_frames.is_empty()
            && single.graph.sequence_boxes.is_empty()
        {
            return Err(invalid(format!(
                "Unsupported or incomplete Mermaid statement on line {}",
                index + 1
            )));
        }
    }
    Ok(())
}

fn image_size(width: f32, height: f32) -> Result<(u32, u32), Failure> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err(invalid("Mermaid diagram has invalid dimensions"));
    }
    if width.ceil() > MAX_SIDE as f32 || height.ceil() > MAX_SIDE as f32 {
        return Err(too_large(
            "Mermaid diagram exceeds the 4096-pixel side limit",
        ));
    }
    let (width, height) = (width.ceil() as u32, height.ceil() as u32);
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(too_large("Mermaid diagram exceeds the 8-megapixel limit"));
    }
    Ok((width, height))
}

fn svg_document(svg: &str, outlined: bool) -> Result<usvg::roxmltree::Document<'_>, Failure> {
    if svg.len() > MAX_SVG_BYTES {
        return Err(too_large("Mermaid SVG exceeds the 8 MiB limit"));
    }
    // roxmltree's default explicitly forbids DTD/entity expansion.
    let doc = usvg::roxmltree::Document::parse(svg)
        .map_err(|e| unsupported(format!("Invalid Mermaid SVG: {e}")))?;
    for node in doc.descendants().filter(|n| n.is_element()) {
        let tag = node.tag_name();
        if tag.namespace() != Some("http://www.w3.org/2000/svg")
            || !matches!(
                tag.name(),
                "svg"
                    | "g"
                    | "defs"
                    | "path"
                    | "rect"
                    | "line"
                    | "polyline"
                    | "polygon"
                    | "circle"
                    | "ellipse"
                    | "marker"
                    | "text"
                    | "tspan"
                    | "title"
                    | "desc"
                    | "linearGradient"
                    | "radialGradient"
                    | "stop"
                    | "clipPath"
                    | "mask"
            )
            || (outlined && matches!(tag.name(), "text" | "tspan"))
        {
            return Err(unsupported(format!(
                "Unsafe or unsupported Mermaid SVG element: {}",
                tag.name()
            )));
        }
        if node.ancestors().take(129).count() > 128 {
            return Err(too_large("Mermaid SVG nesting is too deep"));
        }
        for attribute in node.attributes() {
            let name = attribute.name();
            if name.to_ascii_lowercase().starts_with("on")
                || (name == "href" && !attribute.value().starts_with('#'))
                || name == "base"
            {
                return Err(unsupported(
                    "Mermaid SVG contains an unsafe resource or event attribute",
                ));
            }
            // Canonical usvg output never needs CSS. Reject it on the exported
            // boundary, so CSS escapes/imports cannot hide an external URL.
            if outlined && name == "style" {
                return Err(unsupported("Mermaid SVG unexpectedly contains CSS"));
            }
            let value = attribute.value();
            static RESOURCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r#"(?i)url\s*\(\s*['"]?([^)'"]+)['"]?\s*\)"#).unwrap()
            });
            if RESOURCE
                .captures_iter(value)
                .any(|capture| !capture[1].trim().starts_with('#'))
            {
                return Err(unsupported(
                    "Mermaid SVG contains an external paint reference",
                ));
            }
        }
    }
    Ok(doc)
}

#[cfg(not(target_arch = "wasm32"))]
fn install_face(db: &mut usvg::fontdb::Database, face: &FontFace) -> Option<usvg::fontdb::ID> {
    if let Some(existing) = db.faces().find(|info| {
        info.index == face.index
            && db.with_face_data(info.id, |data, _| data.as_ptr() == face.data.as_ptr())
                == Some(true)
    }) {
        return Some(existing.id);
    }
    db.load_font_source(usvg::fontdb::Source::Binary(face.data.clone()))
        .into_iter()
        .find(|id| db.face(*id).is_some_and(|info| info.index == face.index))
}

#[cfg(not(target_arch = "wasm32"))]
fn font_options(missing: Arc<Mutex<Option<char>>>) -> Result<usvg::Options<'static>, Failure> {
    let request = FontRequest {
        locale: sys_locale::get_locale().unwrap_or_default(),
        ..Default::default()
    };
    let system = fonts::system_fonts(&request);
    let mut db = usvg::fontdb::Database::new();
    let primary = system
        .ui
        .regular
        .as_ref()
        .ok_or_else(|| font("No usable system font is installed"))?;
    let primary_id =
        install_face(&mut db, primary).ok_or_else(|| font("Could not load the system font"))?;
    let family = db
        .face(primary_id)
        .and_then(|face| face.families.first())
        .map(|(family, _)| family.clone())
        .ok_or_else(|| font("System font has no family name"))?;
    for face in [
        system.ui.semibold,
        system.ui.italic,
        system.ui.bold_italic,
        system.hangul.regular,
        system.hangul.semibold,
        system.hangul.italic,
        system.hangul.bold_italic,
    ]
    .into_iter()
    .flatten()
    {
        install_face(&mut db, &face);
    }
    db.set_sans_serif_family(&family);
    db.set_serif_family(&family);
    let fallback = usvg::FontResolver::default_fallback_selector();
    let mut options = render_options(family, Arc::new(db));
    options.font_resolver.select_fallback = Box::new(move |character, excluded, db| {
        if let Some(id) = fallback(character, excluded, db) {
            return Some(id);
        }
        let role = excluded
            .first()
            .and_then(|id| db.face(*id))
            .map(|face| {
                match (
                    face.weight.0 >= 600,
                    face.style != usvg::fontdb::Style::Normal,
                ) {
                    (false, false) => Role::Regular,
                    (true, false) => Role::Semibold,
                    (false, true) => Role::Italic,
                    (true, true) => Role::BoldItalic,
                }
            })
            .unwrap_or(Role::Regular);
        let face = fonts::fallback_fonts(&request, &[(role, character)])
            .into_iter()
            .next()
            .flatten();
        if let Some(face) = face
            && let Some(id) =
                install_face(Arc::make_mut(db), &face).filter(|id| !excluded.contains(id))
        {
            return Some(id);
        }
        if let Ok(mut missing) = missing.lock() {
            missing.get_or_insert(character);
        }
        None
    });
    Ok(options)
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static BUNDLED_FILES: OnceCell<Result<Vec<Rc<Vec<u8>>>, Failure>> = const { OnceCell::new() };
    static BUNDLED_FONTS: OnceCell<Result<(String, Arc<usvg::fontdb::Database>), Failure>> =
        const { OnceCell::new() };
}

/// Capture dependencies that Makepad loaded for the bundled theme. The queued renderer
/// builds and caches its outline-font database, rather than parsing fonts during drawing.
#[cfg(target_arch = "wasm32")]
pub(super) fn load_bundled_fonts(cx: &makepad_widgets::Cx) {
    BUNDLED_FILES.with(|files| {
        files.get_or_init(|| {
            let registry = cx.live_registry.borrow();
            let dependency = |crate_name, name| {
                let directory = registry
                    .crate_name_to_cargo_manifest_path(crate_name)
                    .ok_or_else(|| font(format!("Missing bundled font: {name}")))?;
                cx.get_dependency(&format!("{directory}/resources/{name}"))
                    .map_err(font)
            };
            [
                ("makepad_widgets", "IBMPlexSans-Text.ttf"),
                ("makepad_widgets", "IBMPlexSans-SemiBold.ttf"),
                ("makepad_widgets", "IBMPlexSans-Italic.ttf"),
                ("makepad_widgets", "IBMPlexSans-BoldItalic.ttf"),
                ("paseo_gui", "fonts/NotoSansKR-Regular.otf"),
                ("paseo_gui", "fonts/NotoSansKR-Bold.otf"),
                ("paseo_gui", "fonts/Jetendard-Regular.ttf"),
            ]
            .into_iter()
            .map(|(crate_name, name)| dependency(crate_name, name))
            .collect()
        });
    });
}

#[cfg(target_arch = "wasm32")]
fn bundled_database() -> Result<(String, Arc<usvg::fontdb::Database>), Failure> {
    BUNDLED_FILES.with(|files| {
        let files = files
            .get()
            .ok_or_else(|| font("Bundled diagram fonts are unavailable"))?
            .as_ref()
            .map_err(Clone::clone)?;
        let mut db = usvg::fontdb::Database::new();
        for data in files {
            db.load_font_data(data.as_ref().clone());
        }
        let family = db
            .faces()
            .next()
            .and_then(|face| face.families.first())
            .map(|(family, _)| family.clone())
            .ok_or_else(|| font("No usable bundled outline font is available"))?;
        db.set_sans_serif_family(&family);
        db.set_serif_family(&family);
        Ok((family, Arc::new(db)))
    })
}

#[cfg(target_arch = "wasm32")]
fn font_options(missing: Arc<Mutex<Option<char>>>) -> Result<usvg::Options<'static>, Failure> {
    BUNDLED_FONTS.with(|fonts| {
        let (family, db) = fonts
            .get_or_init(bundled_database)
            .as_ref()
            .map_err(Clone::clone)?;
        let fallback = usvg::FontResolver::default_fallback_selector();
        let mut options = render_options(family.clone(), db.clone());
        options.font_resolver.select_fallback = Box::new(move |character, excluded, db| {
            fallback(character, excluded, db).or_else(|| {
                if let Ok(mut missing) = missing.lock() {
                    missing.get_or_insert(character);
                }
                None
            })
        });
        Ok(options)
    })
}

fn render_options(family: String, fontdb: Arc<usvg::fontdb::Database>) -> usvg::Options<'static> {
    usvg::Options {
        font_family: family,
        fontdb,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

fn canonicalize(raw: &str) -> Result<Rendered, Failure> {
    svg_document(raw, false)?;
    let missing = Arc::new(Mutex::new(None));
    let options = font_options(missing.clone())?;
    let tree = usvg::Tree::from_str(raw, &options).map_err(|e| unsupported(e.to_string()))?;
    if let Some(character) = *missing.lock().map_err(|_| worker("Font resolver failed"))? {
        return Err(font(format!(
            "No installed outline font supports U+{:04X}",
            character as u32
        )));
    }
    if tree.root().children().is_empty() {
        return Err(invalid("Mermaid diagram contains no visible content"));
    }
    let svg = tree.to_string(&usvg::WriteOptions::default());
    svg_document(&svg, true)?;
    let (width, height) = image_size(tree.size().width(), tree.size().height())?;
    let rgba = rasterize(&tree, width, height)?;
    Ok(Rendered {
        svg,
        width,
        height,
        rgba,
    })
}

fn rasterize(tree: &usvg::Tree, width: u32, height: u32) -> Result<Vec<u8>, Failure> {
    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| too_large("Could not allocate Mermaid image"))?;
    resvg::render(tree, tiny_skia::Transform::identity(), &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    // tiny-skia is premultiplied; the GUI image buffer and PNG encoder are not.
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u32::from(pixel[3]);
        if alpha > 0 && alpha < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(rgba)
}

/// Encode only on export, using exactly the displayed revision's RGBA pixels.
pub fn encode_png(rendered: &Rendered) -> Result<Vec<u8>, Failure> {
    image_size(rendered.width as f32, rendered.height as f32)?;
    let expected = u64::from(rendered.width) * u64::from(rendered.height) * 4;
    if rendered.rgba.len() as u64 != expected {
        return Err(worker("Mermaid image has invalid RGBA dimensions"));
    }
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(
            &rendered.rgba,
            rendered.width,
            rendered.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| worker(e.to_string()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEQUENCE: &str = "sequenceDiagram
participant A as Client
participant B as Server
A->>B: Request
activate B
Note over A,B: Shared state
alt Found
B-->>A: Cached
else Missing
loop Retry
A->>B: Again
end
B-->>A: Result
end
deactivate B";

    #[cfg(target_arch = "wasm32")]
    fn load_test_fonts() {
        let plex = include_bytes!(
            "../../../../../gui-platform/vendor/makepad-widgets/resources/IBMPlexSans-Text.ttf"
        );
        let korean = include_bytes!("../../../../resources/fonts/NotoSansKR-Regular.otf");
        BUNDLED_FILES.with(|files| {
            files.get_or_init(|| Ok(vec![Rc::new(plex.to_vec()), Rc::new(korean.to_vec())]));
        });
    }

    fn sequence_layout(source: &str, dark: bool) -> mermaid::Layout {
        let parsed = mermaid::parse_mermaid_strict(source).unwrap();
        let mut theme = if dark { Theme::dark() } else { Theme::modern() };
        theme.font_family = "sans-serif".into();
        mermaid::compute_layout(&parsed.graph, &theme, &LayoutConfig::default())
    }

    fn assert_frame_labels_inside(frame: &mermaid::layout::SequenceFrameLayout) {
        for label in &frame.section_labels {
            assert!(label.y - label.text.height / 2.0 > frame.y);
            assert!(label.y + label.text.height / 2.0 < frame.y + frame.height);
            assert!(label.x - label.text.width / 2.0 >= frame.x);
            assert!(label.x + label.text.width / 2.0 <= frame.x + frame.width);
        }
        if let Some(first) = frame.section_labels.first() {
            assert!(first.y - first.text.height / 2.0 > frame.label_box.1 + frame.label_box.3);
        }
    }

    #[test]
    fn sequence_groups_follow_notes_and_enclose_nested_events_in_svg() {
        let source = "sequenceDiagram
participant A as 앱 Client
participant B as 서버 Server
A->>B: 요청 Request
activate B
Note over A,B: 상태 확인
alt 캐시 있음
B-->>A: 결과
else 캐시 없음
loop 다시 시도
A->>B: 조회
end
B-->>A: 완료
end
deactivate B";
        for dark in [false, true] {
            let layout = sequence_layout(source, dark);
            let mermaid::layout::DiagramData::Sequence(sequence) = &layout.diagram else {
                panic!("expected sequence layout");
            };
            let alt = sequence
                .frames
                .iter()
                .find(|frame| frame.kind == mermaid::ir::SequenceFrameKind::Alt)
                .unwrap();
            let nested = sequence
                .frames
                .iter()
                .find(|frame| frame.kind == mermaid::ir::SequenceFrameKind::Loop)
                .unwrap();
            let note = &sequence.notes[0];
            let y: Vec<_> = layout.edges.iter().map(|edge| edge.points[0].1).collect();
            assert!(
                layout
                    .nodes
                    .values()
                    .all(|actor| actor.y + actor.height < y[0])
            );
            assert!(y[0] < note.y);
            assert!(
                note.y + note.height < alt.y,
                "preceding note must stay outside alt"
            );
            assert!(alt.y < y[1] && y[1] < alt.dividers[0]);
            let alternative = &alt.section_labels[1];
            assert!(alt.dividers[0] < alternative.y - alternative.text.height / 2.0);
            assert!(alternative.y + alternative.text.height / 2.0 < nested.y);
            assert!(nested.y < y[2] && y[2] < nested.y + nested.height);
            assert!(nested.y + nested.height < y[3] && y[3] < alt.y + alt.height);
            assert!(alt.x < nested.x && nested.x + nested.width < alt.x + alt.width);
            let activation = &sequence.activations[0];
            assert_eq!(
                activation.y, y[0],
                "activate follows the request, not the next reply"
            );
            assert!(activation.y < note.y);
            assert!(activation.y + activation.height >= alt.y + alt.height);
            assert!(
                sequence
                    .footboxes
                    .iter()
                    .all(|actor| actor.y > alt.y + alt.height)
            );
            for frame in &sequence.frames {
                assert_frame_labels_inside(frame);
            }
            // Check the production SVG carries these same rectangles, not a
            // preview-only adjustment or assumptions about stroke dash literals.
            let raw = diagram_svg(source, dark).unwrap();
            let doc = svg_document(&raw, false).unwrap();
            for frame in &sequence.frames {
                assert!(
                    doc.descendants()
                        .filter(|node| node.has_tag_name("rect"))
                        .any(|node| {
                            [
                                ("x", frame.x),
                                ("y", frame.y),
                                ("width", frame.width),
                                ("height", frame.height),
                            ]
                            .into_iter()
                            .all(|(name, expected)| {
                                node.attribute(name)
                                    .and_then(|value| value.parse::<f32>().ok())
                                    .is_some_and(|actual| (actual - expected).abs() < 0.02)
                            })
                        })
                );
            }
        }
    }

    #[test]
    fn sequence_note_only_sections_keep_distinct_source_boundaries() {
        let source = "sequenceDiagram
participant A
participant B
A->>B: Before
Note over A,B: Outside before
alt First
Note over A,B: Inside first
else Second
loop Nested
Note right of B: Inside nested
end
Note left of A: After nested
end
Note over A,B: Outside after
B-->>A: After";
        let layout = sequence_layout(source, false);
        let mermaid::layout::DiagramData::Sequence(sequence) = &layout.diagram else {
            panic!("expected sequence layout");
        };
        let alt = &sequence.frames[0];
        let nested = &sequence.frames[1];
        let notes = &sequence.notes;
        assert!(notes[0].y + notes[0].height < alt.y);
        assert!(alt.section_labels[0].y + alt.section_labels[0].text.height / 2.0 < notes[1].y);
        assert!(notes[1].y + notes[1].height < alt.dividers[0]);
        assert!(alt.section_labels[1].y + alt.section_labels[1].text.height / 2.0 < nested.y);
        assert!(nested.y < notes[2].y && notes[2].y + notes[2].height < nested.y + nested.height);
        assert!(nested.y + nested.height < notes[3].y);
        assert!(notes[3].y + notes[3].height < alt.y + alt.height);
        assert!(alt.y + alt.height < notes[4].y);
        assert!(notes[4].y + notes[4].height < layout.edges[1].points[0].1);
        for frame in &sequence.frames {
            assert_frame_labels_inside(frame);
        }
        for note in &notes[1..4] {
            assert!(alt.x < note.x && note.x + note.width < alt.x + alt.width);
        }
    }

    #[test]
    fn sequence_nested_first_events_and_self_messages_fit_their_frames() {
        let source = "sequenceDiagram
participant A
loop Outer condition with a long wrapped label
alt Inner condition with another wrapped label
Note right of A: Before self call
A->>A: Self call
else Other
A->>A: Other call
end
end
A->>A: Outside";
        let layout = sequence_layout(source, false);
        let mermaid::layout::DiagramData::Sequence(sequence) = &layout.diagram else {
            panic!("expected sequence layout");
        };
        let outer = &sequence.frames[0];
        let inner = &sequence.frames[1];
        assert!(outer.y > layout.nodes["A"].y + layout.nodes["A"].height);
        assert!(outer.section_labels[0].y + outer.section_labels[0].text.height / 2.0 < inner.y);
        assert!(inner.y + inner.height < outer.y + outer.height);
        for edge in &layout.edges[..2] {
            for &(x, y) in &edge.points {
                assert!(inner.x < x && x < inner.x + inner.width);
                assert!(inner.y < y && y < inner.y + inner.height);
            }
        }
        assert!(layout.edges[0].points.last().unwrap().1 < inner.dividers[0]);
        assert!(outer.y + outer.height < layout.edges[2].points[0].1);
        for frame in &sequence.frames {
            assert_frame_labels_inside(frame);
        }
    }

    #[test]
    fn zenuml_messages_retain_sequence_order() {
        let layout = sequence_layout("zenuml\nA->B: First\nB->A: Second", false);
        assert!(
            layout
                .nodes
                .values()
                .all(|actor| { actor.y + actor.height < layout.edges[0].points[0].1 })
        );
        assert!(layout.edges[0].points[0].1 < layout.edges[1].points[0].1);
    }

    #[test]
    fn malformed_and_streaming_statements_are_not_partial_successes() {
        for source in [
            "",
            "not a diagram",
            "sequenceDiagram",
            "sequenceDiagram\nA->>B: Fine\nA->>",
            "sequenceDiagram\nA->>B: Fine\nunrecognized text",
            "sequenceDiagram\nloop Retry\nA->>B: Request",
            "sequenceDiagram\nend\nA->>B: Request",
        ] {
            assert!(render(source, false).is_err(), "{source}");
        }
    }

    #[test]
    fn source_and_raster_limits_are_checked_without_allocation() {
        assert!(validate_source(&"x".repeat(MAX_SOURCE_BYTES + 1)).is_err());
        assert!(
            validate_source(&format!(
                "sequenceDiagram\n{}",
                "%% comment\n".repeat(MAX_LINES)
            ))
            .is_err()
        );
        assert!(
            validate_source(&format!(
                "sequenceDiagram\n{}",
                "loop x\n".repeat(MAX_DEPTH + 1)
            ))
            .is_err()
        );
        assert_eq!(image_size(100.2, 42.1).unwrap(), (101, 43));
        for (w, h) in [
            (f32::NAN, 1.0),
            (1.0, f32::INFINITY),
            (0.0, 1.0),
            (4097.0, 1.0),
            (4096.0, 4096.0),
        ] {
            assert!(image_size(w, h).is_err());
        }
        let mut source = String::from("sequenceDiagram\n");
        for i in 0..=MAX_NODES {
            source.push_str(&format!("participant P{i}\n"));
        }
        let failure = render(&source, false).unwrap_err();
        assert_eq!(failure.kind, FailureKind::TooLarge);
        assert!(failure.detail.contains("too many"));
        let invalid = Rendered {
            svg: String::new(),
            width: 2,
            height: 2,
            rgba: vec![0; 4],
        };
        assert!(encode_png(&invalid).is_err());
    }

    #[test]
    fn label_xml_escaping_preserves_unicode_and_literal_markup() {
        let label = "한글 العربية Ελληνικά <script>alert('x')</script> & \"quote\"";
        let source = format!("sequenceDiagram\nA->>B: {label}");
        let raw = diagram_svg(&source, false).unwrap();
        let doc = svg_document(&raw, false).unwrap();
        let label_group = doc
            .descendants()
            .find(|node| node.has_tag_name("g") && node.attribute("class") == Some("edgeLabel"))
            .unwrap();
        let actual: String = label_group
            .descendants()
            .filter(|n| n.has_tag_name("tspan"))
            .filter_map(|n| n.text())
            .flat_map(str::chars)
            .filter(|c| !c.is_whitespace())
            .collect();
        let expected: String = label.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(actual, expected);
        assert!(!doc.descendants().any(|n| n.has_tag_name("script")));
    }

    #[test]
    fn script_events_images_dtd_and_external_paints_fail_closed() {
        for body in [
            "<script>alert(1)</script>",
            "<rect width=\"1\" height=\"1\" onclick=\"alert(1)\"/>",
            "<image href=\"file:///etc/passwd\"/>",
            "<image href=\"https://example.invalid/secret\"/>",
            "<image href=\"data:image/svg+xml;base64,PHN2Zy8+\"/>",
            "<foreignObject/>",
            "<path d=\"M0 0L1 1\" fill=\"url(https://example.invalid/paint)\"/>",
            "<path d=\"M0 0L1 1\" fill=\"URL('file:///tmp/paint')\"/>",
            "<a href=\"javascript:alert(1)\"><rect width=\"1\" height=\"1\"/></a>",
        ] {
            let svg = format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">{body}</svg>"
            );
            assert!(svg_document(&svg, false).is_err(), "{body}");
        }
        assert!(svg_document("<!DOCTYPE svg [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><svg xmlns=\"http://www.w3.org/2000/svg\">&x;</svg>", false).is_err());
        assert!(
            render(
                "%%{init: {theme: 'dark'}}%%\nsequenceDiagram\nA->>B: x",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn flowchart_click_targets_never_reach_the_svg_boundary() {
        let raw = diagram_svg(
            "flowchart LR\nA[One] --> B[Two]\nclick A \"javascript:alert(1)\"",
            false,
        )
        .unwrap();
        let doc = svg_document(&raw, false).unwrap();
        assert!(!doc.descendants().any(|n| n.is_element()
            && (n.has_tag_name("a") || n.attributes().any(|a| a.name() == "href"))));
        assert!(
            doc.descendants()
                .any(|n| n.has_tag_name("path") && n.attribute("class") == Some("edgePath"))
        );
    }

    #[test]
    fn exported_vectors_are_portable_and_png_is_the_preview() {
        #[cfg(target_arch = "wasm32")]
        load_test_fonts();
        let rendered = render(SEQUENCE, false).unwrap();
        let doc = svg_document(&rendered.svg, true).unwrap();
        assert!(doc.descendants().filter(|n| n.has_tag_name("path")).count() > 30);
        let options = usvg::Options {
            image_href_resolver: usvg::ImageHrefResolver {
                resolve_data: Box::new(|_, _, _| panic!("SVG requested embedded resources")),
                resolve_string: Box::new(|_, _| panic!("SVG requested external resources")),
            },
            // Deliberately empty font DB: portable exported paths need no fonts.
            ..Default::default()
        };
        let tree = usvg::Tree::from_str(&rendered.svg, &options).unwrap();
        let portable = rasterize(&tree, rendered.width, rendered.height).unwrap();
        assert_eq!(portable.len(), rendered.rgba.len());
        assert!(
            portable
                .iter()
                .zip(&rendered.rgba)
                .all(|(a, b)| a.abs_diff(*b) <= 2)
        );
        let png = encode_png(&rendered).unwrap();
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.dimensions(), (rendered.width, rendered.height));
        assert_eq!(decoded.into_raw(), rendered.rgba);
        assert!(
            rendered
                .rgba
                .chunks_exact(4)
                .any(|pixel| pixel != &rendered.rgba[..4])
        );
        let dark = render(SEQUENCE, true).unwrap();
        assert_ne!(dark.rgba, rendered.rgba);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn multilingual_labels_are_outlined_or_report_missing_os_fonts() {
        let source = "sequenceDiagram\nA->>B: 한글 العربية Ελληνικά";
        match render(source, false) {
            Ok(rendered) => {
                let options = usvg::Options::default();
                let tree = usvg::Tree::from_str(&rendered.svg, &options).unwrap();
                let paths = svg_document(&rendered.svg, true)
                    .unwrap()
                    .descendants()
                    .filter(|node| node.has_tag_name("path"))
                    .count();
                assert!(
                    paths > 15,
                    "Unicode labels must produce glyph outlines, not disappear"
                );
                let portable = rasterize(&tree, rendered.width, rendered.height).unwrap();
                assert!(
                    portable
                        .iter()
                        .zip(&rendered.rgba)
                        .all(|(a, b)| a.abs_diff(*b) <= 2)
                );
            }
            Err(error) => {
                // Headless systems may lack a script's font. Require an honest
                // missing-glyph diagnostic, never a successful tofu/blank export.
                assert_eq!(error.kind, FailureKind::Font);
                let code = error
                    .detail
                    .strip_prefix("No installed outline font supports U+")
                    .unwrap_or_else(|| panic!("Unexpected font failure: {}", error.detail));
                let character = char::from_u32(u32::from_str_radix(code, 16).unwrap()).unwrap();
                let request = FontRequest {
                    locale: sys_locale::get_locale().unwrap_or_default(),
                    ..Default::default()
                };
                assert!(
                    fonts::fallback_fonts(&request, &[(Role::Regular, character)])
                        .into_iter()
                        .all(|font| font.is_none())
                );
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn bundled_korean_labels_are_outlined() {
        load_test_fonts();
        let source = "sequenceDiagram\nA->>B: 한글";
        let rendered = render(source, false).unwrap();
        svg_document(&rendered.svg, true).unwrap();
    }
}
