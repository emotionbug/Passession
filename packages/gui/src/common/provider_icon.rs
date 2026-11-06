//! Port of `features/chat/provider_icon.dart` (shared by the sidebar, tabs and chat):
//! built-in glyphs win, then the host snapshot's sanitized SVG, then the vendored catalog,
//! then the Lucide `bot` glyph.
//!
//! Makepad's icon atlas fills one SVG path, so each provider SVG is flattened once into a
//! single absolute path by `usvg` (the full SVG path grammar, including compact arc flags,
//! shapes, `<use>` and transforms; arcs become cubics). Only filled, visible paths count:
//! stroke-only elements are not fillable, and an icon with nothing fillable falls back to `bot`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use makepad_widgets::*;
use serde_json::Value;

use crate::common::paint::Painter;
use crate::theme;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use crate::common::paint::PaseoPainter;

    pub ProviderIcon = {{ProviderIcon}} {
        width: 16, height: 16,
        painter: <PaseoPainter> {}
        draw_icon: {
            fn get_color(self) -> vec4 {
                return self.color;
            }
        }
    }
}

const CATALOG: &str = include_str!("../../resources/icons/provider-icons.json");

thread_local! {
    static CATALOGS: (HashMap<String, String>, HashMap<String, String>) = {
        let value: Value = serde_json::from_str(CATALOG).expect("bundled provider icons are valid JSON");
        let read = |key: &str| -> HashMap<String, String> {
            value
                .get(key)
                .and_then(Value::as_object)
                .map(|map| map.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                .unwrap_or_default()
        };
        (read("builtin"), read("catalog"))
    };
    /// Flattened paths by SVG source, `None` when nothing is fillable. The `Arc` identity is
    /// the atlas cache key, so it must stay stable.
    static PATHS: RefCell<HashMap<String, Option<Arc<String>>>> = RefCell::new(HashMap::new());
}

/// `ProviderIcon.svgFor`.
pub fn svg_for(provider: &str, snapshot_svg: Option<&str>) -> Option<String> {
    CATALOGS.with(|(builtin, catalog)| {
        builtin
            .get(provider)
            .cloned()
            .or_else(|| snapshot_svg.filter(|s| !s.is_empty()).map(str::to_string))
            .or_else(|| catalog.get(provider).cloned())
    })
}

/// The flattened fill path of an SVG document (cached).
pub fn icon_path(svg: &str) -> Option<Arc<String>> {
    PATHS.with(|paths| {
        let mut paths = paths.borrow_mut();
        if let Some(found) = paths.get(svg) {
            return found.clone();
        }
        let flattened = flatten_svg(svg).map(Arc::new);
        paths.insert(svg.to_string(), flattened.clone());
        flattened
    })
}

/// Flattens the filled, visible shapes of an SVG document into one absolute path string.
fn flatten_svg(svg: &str) -> Option<String> {
    use resvg::usvg;
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut out = String::new();
    write_group(&mut out, tree.root());
    let trimmed = out.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn write_group(out: &mut String, group: &resvg::usvg::Group) {
    use resvg::tiny_skia::{PathSegment, Point};
    use resvg::usvg::Node;
    for node in group.children() {
        match node {
            Node::Group(child) => write_group(out, child),
            Node::Path(path) if path.is_visible() && path.fill().is_some() => {
                let t = path.abs_transform();
                let map = |mut p: Point| {
                    t.map_point(&mut p);
                    p
                };
                for segment in path.data().segments() {
                    let _ = match segment {
                        PathSegment::MoveTo(p) => {
                            let p = map(p);
                            write!(out, "M{} {} ", p.x, p.y)
                        }
                        PathSegment::LineTo(p) => {
                            let p = map(p);
                            write!(out, "L{} {} ", p.x, p.y)
                        }
                        PathSegment::QuadTo(c, p) => {
                            let (c, p) = (map(c), map(p));
                            write!(out, "Q{} {} {} {} ", c.x, c.y, p.x, p.y)
                        }
                        PathSegment::CubicTo(c1, c2, p) => {
                            let (c1, c2, p) = (map(c1), map(c2), map(p));
                            write!(out, "C{} {} {} {} {} {} ", c1.x, c1.y, c2.x, c2.y, p.x, p.y)
                        }
                        PathSegment::Close => write!(out, "Z "),
                    };
                }
            }
            _ => {}
        }
    }
}

/// Provider glyph tinted with `currentColor` (Flutter `SvgTheme(currentColor: tint)`).
#[derive(Live, LiveHook, Widget)]
pub struct ProviderIcon {
    #[redraw]
    #[live]
    painter: Painter,
    #[live]
    draw_icon: DrawIcon,
    #[walk]
    walk: Walk,
    #[rust]
    provider: String,
    #[rust]
    path: Option<Arc<String>>,
    #[rust]
    color: Option<Vec4>,
}

impl ProviderIcon {
    /// Draws the icon for `provider` in `rect` (shell-owned immediate drawing).
    pub fn paint(
        painter: &mut Painter,
        draw_icon: &mut DrawIcon,
        cx: &mut Cx2d,
        rect: Rect,
        provider: &str,
        snapshot_svg: Option<&str>,
        color: Vec4,
    ) {
        match svg_for(provider, snapshot_svg).and_then(|svg| icon_path(&svg)) {
            Some(path) => {
                draw_icon.svg_path = ArcStringMut::Arc(path);
                draw_icon.color = color;
                draw_icon.draw_walk(cx, Walk::abs_rect(rect));
            }
            None => painter.icon(cx, rect, "bot", rect.size.y, color),
        }
    }
}

impl Widget for ProviderIcon {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let r = cx.walk_turtle(walk);
        let color = self.color.unwrap_or(p.muted);
        match &self.path {
            Some(path) => {
                self.draw_icon.svg_path = ArcStringMut::Arc(path.clone());
                self.draw_icon.color = color;
                self.draw_icon.draw_walk(cx, Walk::abs_rect(r));
            }
            None => self.painter.icon(cx, r, "bot", r.size.y, color),
        }
        DrawStep::done()
    }
}

impl ProviderIconRef {
    pub fn set_provider(&self, cx: &mut Cx, provider: &str, snapshot_svg: Option<&str>) {
        if let Some(mut icon) = self.borrow_mut() {
            let path = svg_for(provider, snapshot_svg).and_then(|svg| icon_path(&svg));
            if icon.provider != provider || icon.path != path {
                icon.provider = provider.to_string();
                icon.path = path;
                icon.redraw(cx);
            }
        }
    }

    pub fn set_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut icon) = self.borrow_mut()
            && icon.color != color
        {
            icon.color = color;
            icon.redraw(cx);
        }
    }
}
