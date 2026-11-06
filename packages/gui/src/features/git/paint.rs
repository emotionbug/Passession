//! Git drawing helpers over the shell's shared [`Painter`] (common/paint.rs):
//! sized text in logical px, Lucide glyphs centred on a point, arcs, a
//! spinner, clipping and a hit registry with clip rects.

use crate::common::paint::{Painter, TextSpec};
use crate::features::files::{DrawFilesImage, file_icon};
use makepad_widgets::*;
use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use std::rc::Rc;

pub use crate::common::paint::Font;

// Defined next to the struct: Makepad resolves a `#[live]` field's DSL class in the module
// that defines its Rust type.
live_design! {
    use crate::common::paint::PaseoPainter;
    use crate::features::files::kit::DrawFilesImage;

    pub GitPaint = {{GitPaint}} {
        painter: <PaseoPainter> {}
        file_icon: <DrawFilesImage> {}
        // Original `_manualGearPath` (pull_request_panel.dart, 15-unit viewBox at -0.5).
        manual_gear: {
            svg_path: "M10.5 7.63V6.37l-.787-.13c-.044-.175-.132-.349-.263-.61l.481-.652-.918-.913-.657.478a2.346 2.346 0 0 0-.612-.26L7.656 3.5H6.388l-.132.783c-.219.043-.394.13-.612.26l-.657-.478-.918.913.437.652c-.131.218-.175.392-.262.61l-.744.086v1.261l.787.13c.044.218.132.392.263.61l-.438.651.92.913.655-.434c.175.086.394.173.613.26l.131.783h1.313l.131-.783c.219-.043.394-.13.613-.26l.656.478.918-.913-.48-.652c.13-.218.218-.435.262-.61l.656-.13zM7 8.283a1.285 1.285 0 0 1-1.313-1.305c0-.739.57-1.304 1.313-1.304.744 0 1.313.565 1.313 1.304 0 .74-.57 1.305-1.313 1.305z"
        }
    }
}

/// The draw primitives of one git widget.
#[derive(Live, LiveHook, LiveRegister)]
pub struct GitPaint {
    #[live]
    pub painter: Painter,
    /// Material file icons (the Files feature's `FileIcon` textures).
    #[live]
    pub file_icon: DrawFilesImage,
    #[live]
    pub manual_gear: DrawIcon,
}

pub fn transparent() -> Vec4 {
    vec4(0.0, 0.0, 0.0, 0.0)
}

pub fn with_alpha(color: Vec4, alpha: f32) -> Vec4 {
    vec4(color.x, color.y, color.z, color.w * alpha)
}

/// `#rrggbb` (syntax palettes) to a colour.
pub fn hex(value: &str) -> Vec4 {
    let digits = value.trim_start_matches('#');
    let parse = |range: std::ops::Range<usize>| {
        digits
            .get(range)
            .and_then(|h| u8::from_str_radix(h, 16).ok())
            .unwrap_or(0) as f32
            / 255.0
    };
    let alpha = if digits.len() >= 8 { parse(6..8) } else { 1.0 };
    vec4(parse(0..2), parse(2..4), parse(4..6), alpha)
}

pub fn rgb(value: u32) -> Vec4 {
    vec4(
        ((value >> 16) & 0xff) as f32 / 255.0,
        ((value >> 8) & 0xff) as f32 / 255.0,
        (value & 0xff) as f32 / 255.0,
        1.0,
    )
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect {
        pos: dvec2(x, y),
        size: dvec2(w.max(0.0), h.max(0.0)),
    }
}

impl GitPaint {
    fn spec(font: Font, size_px: f64, color: Vec4) -> TextSpec {
        TextSpec::new(font, size_px, color)
    }

    /// Starts new draw calls so what follows paints over everything drawn
    /// earlier in this draw list (sticky headers over scrolled rows).
    pub fn new_layer(&mut self, cx: &mut Cx2d) {
        let p = &self.painter;
        cx.new_draw_call(&p.draw_box.draw_vars);
        cx.new_draw_call(&p.draw_ring.draw_vars);
        cx.new_draw_call(&p.text_regular.draw_vars);
        cx.new_draw_call(&p.text_medium.draw_vars);
        cx.new_draw_call(&p.text_mono.draw_vars);
        cx.new_draw_call(&p.text_icon.draw_vars);
    }

    /// The material icon of `file_name` in `area`; false while it is still
    /// decoding (redraw when `files::revision()` changes).
    pub fn file_icon(&mut self, cx: &mut Cx2d, area: Rect, file_name: &str) -> bool {
        match file_icon(file_name) {
            Some(texture) => {
                self.texture(cx, area, &texture, 0.0);
                true
            }
            None => false,
        }
    }

    /// A texture in `area` with rounded corners. The texture is bound before
    /// the draw call opens: Makepad copies the slots when it creates the call
    /// and treats an empty slot as matching any texture, so binding afterwards
    /// makes later images append to a call drawn with the wrong (or no) texture.
    pub fn texture(&mut self, cx: &mut Cx2d, area: Rect, texture: &Texture, radius: f64) {
        self.file_icon.draw_vars.set_texture(0, texture);
        self.file_icon.new_draw_call(cx);
        self.file_icon.radius = radius as f32;
        self.file_icon.opacity = 1.0;
        self.file_icon.draw_abs(cx, area);
    }

    /// Original GitLab manual / action-required check glyph: a 1.25-unit
    /// circle of radius 6.25 around the gear path, in a 15-unit box of `size`.
    pub fn manual_check(&mut self, cx: &mut Cx2d, size: f64, color: Vec4, center: DVec2) {
        let unit = size / 15.0;
        let stroke = 1.25 * unit;
        // DrawRing radius = half the rect − stroke / 2 − 0.5.
        let side = 2.0 * (6.25 * unit + stroke / 2.0 + 0.5);
        self.painter.ring(cx, rect(center.x - side / 2.0, center.y - side / 2.0, side, side), color, stroke, 1.0, 0.0);
        // The gear spans 3.5..10.5 of the viewBox; DrawIcon fits its bounds to the rect.
        self.manual_gear.color = color;
        let origin = center - dvec2(7.5 * unit, 7.5 * unit);
        self.manual_gear.draw_walk(
            cx,
            Walk { abs_pos: Some(origin + dvec2(4.0 * unit, 4.0 * unit)), margin: Margin::default(), width: Size::Fixed(7.0 * unit), height: Size::Fixed(7.0 * unit) },
        );
    }

    /// The layout `text_centered` draws for `text` (fitted to `max_width`)
    /// and its top-left corner: geometry for selection hit tests.
    #[allow(clippy::too_many_arguments)]
    pub fn line_layout(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, x: f64, top: f64, height: f64, text: &str, max_width: f64) -> Option<(Rc<LaidoutText>, DVec2)> {
        if text.is_empty() || max_width <= 0.0 {
            return None;
        }
        let spec = Self::spec(font, size_px, transparent());
        let fitted = self.painter.fit(cx, spec, text, max_width);
        let laid = self.painter.layout(cx, spec, &fitted, None);
        let pos = dvec2(x, top + (height - laid.size_in_lpxs.height as f64) * 0.5);
        Some((laid, pos))
    }

    /// The layout `text_wrapped` draws for `text` at `max_width`.
    pub fn wrapped_layout(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, text: &str, max_width: f64) -> Rc<LaidoutText> {
        self.painter.layout(cx, Self::spec(font, size_px, transparent()), text, Some(max_width.max(1.0)))
    }

    pub fn fill(&mut self, cx: &mut Cx2d, area: Rect, color: Vec4) {
        if area.size.x > 0.0 && area.size.y > 0.0 {
            self.painter.fill(cx, area, color, 0.0);
        }
    }

    /// A rounded box with an optional `(colour, width)` border inside its bounds.
    pub fn rounded(&mut self, cx: &mut Cx2d, area: Rect, color: Vec4, radius: f64, border: Option<(Vec4, f64)>) {
        if area.size.x <= 0.0 || area.size.y <= 0.0 {
            return;
        }
        match border {
            Some((border, width)) => self.painter.outline(cx, area, color, border, width, radius),
            None => self.painter.fill(cx, area, color, radius),
        }
    }

    /// An arc from `start` to `end` (fractions of a turn clockwise from twelve).
    pub fn arc(&mut self, cx: &mut Cx2d, area: Rect, color: Vec4, start: f32, end: f32, stroke: f64) {
        let progress = (end - start).max(0.0) as f64;
        if progress <= 0.0 {
            return;
        }
        // DrawRing's zero angle points down (six o'clock); twelve is half a turn on.
        self.painter.ring(cx, area, color, stroke, progress, start as f64 + 0.5);
    }

    /// A turning partial ring; callers request frames only while pending.
    pub fn spinner(&mut self, cx: &mut Cx2d, center: DVec2, size: f64, color: Vec4, phase: f64) {
        let area = rect(center.x - size / 2.0, center.y - size / 2.0, size, size);
        self.painter.ring(cx, area, color, (size / 9.0).max(1.5), 0.7, (phase * 0.8).fract());
    }

    /// Single-line size of `text`.
    pub fn measure(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, text: &str) -> DVec2 {
        if text.is_empty() {
            return dvec2(0.0, self.painter.line_height(cx, Self::spec(font, size_px, transparent())));
        }
        self.painter.measure(cx, Self::spec(font, size_px, transparent()), text)
    }

    /// One line at its top-left `pos`; returns its width.
    pub fn text(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, color: Vec4, pos: DVec2, text: &str) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        self.painter.text(cx, pos, Self::spec(font, size_px, color), text).x
    }

    /// `text` with a trailing ellipsis so it fits `max_width`.
    pub fn ellipsize(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, text: &str, max_width: f64) -> String {
        if max_width <= 0.0 {
            return String::new();
        }
        self.painter.fit(cx, Self::spec(font, size_px, transparent()), text, max_width)
    }

    /// One line clipped with an ellipsis; returns the drawn width.
    #[allow(clippy::too_many_arguments)]
    pub fn text_fit(
        &mut self,
        cx: &mut Cx2d,
        font: Font,
        size_px: f64,
        color: Vec4,
        pos: DVec2,
        text: &str,
        max_width: f64,
    ) -> f64 {
        let fitted = self.ellipsize(cx, font, size_px, text, max_width);
        self.text(cx, font, size_px, color, pos, &fitted)
    }

    /// One line vertically centred in a band of `height` from `top`.
    #[allow(clippy::too_many_arguments)]
    pub fn text_centered(
        &mut self,
        cx: &mut Cx2d,
        font: Font,
        size_px: f64,
        color: Vec4,
        x: f64,
        top: f64,
        height: f64,
        text: &str,
        max_width: f64,
    ) -> f64 {
        if text.is_empty() || max_width <= 0.0 {
            return 0.0;
        }
        self.painter
            .text_in(cx, rect(x, top, max_width, height), Self::spec(font, size_px, color), text, 0.0)
            .size
            .x
    }

    /// Height of `text` word-wrapped at `max_width`.
    pub fn measure_wrapped(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, text: &str, max_width: f64) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        self.painter
            .paragraph_height(cx, max_width.max(1.0), Self::spec(font, size_px, transparent()), text)
    }

    /// Word-wrapped text from `pos`; returns its height.
    #[allow(clippy::too_many_arguments)]
    pub fn text_wrapped(
        &mut self,
        cx: &mut Cx2d,
        font: Font,
        size_px: f64,
        color: Vec4,
        pos: DVec2,
        text: &str,
        max_width: f64,
    ) -> f64 {
        if text.is_empty() {
            return 0.0;
        }
        self.painter
            .paragraph(cx, pos, max_width.max(1.0), Self::spec(font, size_px, color), text)
            .y
    }

    /// A Lucide glyph of `size_px` centred on `center`.
    pub fn icon(&mut self, cx: &mut Cx2d, name: &str, size_px: f64, color: Vec4, center: DVec2) {
        let box_size = size_px * 2.0;
        self.painter.icon(
            cx,
            rect(center.x - box_size / 2.0, center.y - box_size / 2.0, box_size, box_size),
            name,
            size_px,
            color,
        );
    }

    /// Clips what is drawn until [`end_clip`](Self::end_clip) to `area`.
    pub fn begin_clip(&mut self, cx: &mut Cx2d, area: Rect) {
        cx.begin_turtle(Walk::abs_rect(area), Layout::default());
    }

    pub fn end_clip(&mut self, cx: &mut Cx2d) {
        cx.end_turtle();
    }
}

/// Rects registered during draw with what a press on them does; the last
/// registered rect containing a point wins (overlays register last).
pub struct Hits<T> {
    items: Vec<(Rect, Option<Rect>, T)>,
}

impl<T: Clone + PartialEq> Default for Hits<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<T: Clone + PartialEq> Hits<T> {
    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn push(&mut self, area: Rect, target: T) {
        self.items.push((area, None, target));
    }

    /// A hit that only counts inside `clip` (scrolled regions).
    pub fn push_clipped(&mut self, area: Rect, clip: Rect, target: T) {
        self.items.push((area, Some(clip), target));
    }

    pub fn find(&self, point: DVec2) -> Option<(Rect, T)> {
        self.items
            .iter()
            .rev()
            .find(|(area, clip, _)| area.contains(point) && clip.is_none_or(|clip| clip.contains(point)))
            .map(|(area, _, target)| (*area, target.clone()))
    }

    pub fn rect_of(&self, target: &T) -> Option<Rect> {
        self.items.iter().rev().find(|(_, _, t)| t == target).map(|(area, _, _)| *area)
    }

    /// Every registered rect with its clip, in registration order.
    pub fn entries(&self) -> &[(Rect, Option<Rect>, T)] {
        &self.items
    }
}
