//! Immediate-mode drawing helpers for custom-drawn surfaces (shell chrome, menus, dialogs,
//! primitives). Colours and sizes are passed per call so the runtime [`Palette`] applies.
//!
//! Hit testing: a custom widget owns one Makepad `Area` (from `cx.end_turtle_with_area` or
//! [`Painter::capture_box`]) for capture/hover, and keeps the rects it painted in a [`HitMap`]
//! to resolve which item is under the pointer.

use std::borrow::Cow;
use std::rc::Rc;

use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::*;

use crate::common::icons;
use crate::theme::{DrawBox, DrawRing, Typography};

live_design! {
    use link::shaders::*;
    use link::theme::*;
    use crate::theme::*;

    pub PaseoPainter = {{Painter}} {
        draw_box: {}
        capture: {}
        draw_ring: {}
        text_regular: { text_style: <PASEO_FONT_REGULAR> { line_spacing: 1.2 } }
        text_medium: { text_style: <PASEO_FONT_MEDIUM> { line_spacing: 1.2 } }
        text_mono: { text_style: <PASEO_FONT_MONO> { line_spacing: 1.35 } }
        text_icon: { text_style: <PASEO_FONT_ICON> {} }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    Regular,
    /// Flutter w500/w600 (IBM Plex SemiBold, Noto Sans KR Bold).
    Medium,
    Mono,
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct Painter {
    #[live]
    pub draw_box: DrawBox,
    /// One instance per frame: the widget's capture area (see [`Painter::capture_box`]).
    #[live]
    pub capture: DrawBox,
    #[live]
    pub draw_ring: DrawRing,
    #[live]
    pub text_regular: DrawText,
    #[live]
    pub text_medium: DrawText,
    #[live]
    pub text_mono: DrawText,
    #[live]
    pub text_icon: DrawText,
    /// Multiplies everything this painter draws (fade-in transitions); 1 when opaque.
    #[rust(1.0)]
    alpha: f32,
}

/// A text style for one call: font, size in logical px (already scaled) and colour.
#[derive(Clone, Copy, Debug)]
pub struct TextSpec {
    pub font: Font,
    pub size: f64,
    pub color: Vec4,
}

impl TextSpec {
    pub fn new(font: Font, size: f64, color: Vec4) -> Self {
        Self { font, size, color }
    }
}

/// Presents hard line breaks as spaces without dropping later label content.
/// Paragraph/code layout must keep the original text instead.
pub fn single_line_text(text: &str) -> Cow<'_, str> {
    let is_break = |c| matches!(c, '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{0085}' | '\u{2028}' | '\u{2029}');
    let Some(first) = text.find(is_break) else { return Cow::Borrowed(text) };
    let mut line = String::with_capacity(text.len());
    line.push_str(&text[..first]);
    let mut chars = text[first..].chars().peekable();
    while let Some(c) = chars.next() {
        if is_break(c) {
            line.push(' ');
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            line.push(c);
        }
    }
    Cow::Owned(line)
}

impl Painter {
    /// The area of the draw list this painter last drew into (`#[redraw]` target): every draw
    /// call of one painter lands in the same list, so any drawn component identifies it.
    pub fn area(&self) -> Area {
        [self.draw_box.area(), self.text_regular.area(), self.text_medium.area(), self.text_mono.area(), self.text_icon.area(), self.draw_ring.area()]
            .into_iter()
            .find(|area| !matches!(area, Area::Empty))
            .unwrap_or(Area::Empty)
    }

    pub fn redraw(&self, cx: &mut Cx) {
        self.area().redraw(cx);
    }

    fn drawer(&mut self, font: Font) -> &mut DrawText {
        match font {
            Font::Regular => &mut self.text_regular,
            Font::Medium => &mut self.text_medium,
            Font::Mono => &mut self.text_mono,
        }
    }

    fn prepare(&mut self, spec: TextSpec) -> &mut DrawText {
        let alpha = self.alpha;
        let drawer = self.drawer(spec.font);
        drawer.text_style.font_size = Typography::pt(spec.size);
        drawer.color = Vec4 { w: spec.color.w * alpha, ..spec.color };
        drawer
    }

    /// Fades everything drawn next by `alpha` (`FadeTransition`); reset to 1 afterwards.
    pub fn set_alpha(&mut self, alpha: f32) {
        self.alpha = alpha;
        self.draw_box.opacity = alpha;
    }

    pub fn layout(&mut self, cx: &mut Cx, spec: TextSpec, text: &str, max_width: Option<f64>) -> Rc<LaidoutText> {
        let drawer = self.prepare(spec);
        drawer.layout(cx, 0.0, 0.0, max_width.map(|w| w as f32), max_width.is_some(), Align::default(), text)
    }

    /// Single-line size.
    pub fn measure(&mut self, cx: &mut Cx, spec: TextSpec, text: &str) -> DVec2 {
        let laid = self.layout(cx, spec, text, None);
        dvec2(laid.size_in_lpxs.width as f64, laid.size_in_lpxs.height as f64)
    }

    /// Line height of the style (empty string still has one row).
    pub fn line_height(&mut self, cx: &mut Cx, spec: TextSpec) -> f64 {
        self.measure(cx, spec, "Ag").y
    }

    /// Draws single-line text at its top-left corner; returns its size.
    pub fn text(&mut self, cx: &mut Cx2d, pos: DVec2, spec: TextSpec, text: &str) -> DVec2 {
        let size = self.measure(cx, spec, text);
        self.prepare(spec).draw_abs(cx, pos, text);
        size
    }

    /// Single line inside `rect`, vertically centred, horizontally aligned by `align_x`
    /// (0 left, 0.5 centre, 1 right) and truncated with an ellipsis (`maxLines: 1`,
    /// `TextOverflow.ellipsis`). Returns the drawn rect.
    pub fn text_in(&mut self, cx: &mut Cx2d, rect: Rect, spec: TextSpec, text: &str, align_x: f64) -> Rect {
        let fitted = self.fit(cx, spec, text, rect.size.x);
        let size = self.measure(cx, spec, &fitted);
        let pos = dvec2(
            rect.pos.x + ((rect.size.x - size.x) * align_x).max(0.0),
            rect.pos.y + ((rect.size.y - size.y) * 0.5),
        );
        self.prepare(spec).draw_abs(cx, pos, &fitted);
        Rect { pos, size }
    }

    /// `text` shortened with "…" so it fits `max_width`.
    pub fn fit(&mut self, cx: &mut Cx, spec: TextSpec, text: &str, max_width: f64) -> String {
        let line = text.lines().next().unwrap_or("");
        let laid = self.layout(cx, spec, line, None);
        if (laid.size_in_lpxs.width as f64) <= max_width + 0.5 || max_width <= 0.0 {
            return line.to_string();
        }
        let ellipsis = self.measure(cx, spec, "…").x;
        let Some(row) = laid.rows.first() else { return String::new() };
        let budget = (max_width - ellipsis).max(0.0) as f32;
        let mut index = row.x_in_lpxs_to_index(budget).min(line.len());
        while index > 0 && !line.is_char_boundary(index) {
            index -= 1;
        }
        // x_in_lpxs_to_index rounds to the nearest grapheme; step back until it fits.
        loop {
            let candidate = format!("{}…", line[..index].trim_end());
            if index == 0 || self.measure(cx, spec, &candidate).x <= max_width + 0.5 {
                return candidate;
            }
            index -= 1;
            while index > 0 && !line.is_char_boundary(index) {
                index -= 1;
            }
        }
    }

    /// Wrapped paragraph from `pos` within `max_width`; returns its size.
    pub fn paragraph(&mut self, cx: &mut Cx2d, pos: DVec2, max_width: f64, spec: TextSpec, text: &str) -> DVec2 {
        let laid = self.layout(cx, spec, text, Some(max_width));
        let size = dvec2(laid.size_in_lpxs.width as f64, laid.size_in_lpxs.height as f64);
        let drawer = self.prepare(spec);
        drawer.draw_walk_laidout(cx, Walk::abs_rect(Rect { pos, size }), &laid);
        size
    }

    /// Height a wrapped paragraph needs.
    pub fn paragraph_height(&mut self, cx: &mut Cx, max_width: f64, spec: TextSpec, text: &str) -> f64 {
        self.layout(cx, spec, text, Some(max_width)).size_in_lpxs.height as f64
    }

    /// A Lucide icon of `size` centred in `rect`.
    pub fn icon(&mut self, cx: &mut Cx2d, rect: Rect, name: &str, size: f64, color: Vec4) {
        let glyph = icons::text(name);
        let drawer = &mut self.text_icon;
        drawer.text_style.font_size = Typography::pt(size);
        drawer.color = Vec4 { w: color.w * self.alpha, ..color };
        let laid = drawer.layout(cx, 0.0, 0.0, None, false, Align::default(), &glyph);
        let w = laid.size_in_lpxs.width as f64;
        let h = laid.size_in_lpxs.height as f64;
        let pos = dvec2(rect.pos.x + (rect.size.x - w) * 0.5, rect.pos.y + (rect.size.y - h) * 0.5);
        drawer.draw_abs(cx, pos, &glyph);
    }

    pub fn fill(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4, radius: f64) {
        if color.w <= 0.0 {
            return;
        }
        self.draw_box.paint(cx, rect, color, radius);
    }

    pub fn outline(&mut self, cx: &mut Cx2d, rect: Rect, fill: Vec4, border: Vec4, width: f64, radius: f64) {
        self.draw_box.paint_bordered(cx, rect, fill, border, width, radius);
    }

    /// Paints `rect` and returns the widget's capture/hover area. Makepad hands pointer captures
    /// and key focus along each draw primitive's own instance chain on redraw, so an earlier
    /// instance of the shared `draw_box` goes stale between FingerDown and FingerUp and the
    /// press never completes. The area is therefore an invisible instance of the dedicated
    /// `capture` box; the visible box goes through `draw_box` right after it, which also opens
    /// `draw_box`'s batch before any text of this painter (batches draw in creation order).
    /// Call at most once per frame per painter, before its other drawing.
    pub fn capture_box(&mut self, cx: &mut Cx2d, rect: Rect, fill: Vec4, border: Vec4, width: f64, radius: f64) -> Area {
        self.capture.paint(cx, rect, Vec4::default(), 0.0);
        self.draw_box.paint_bordered(cx, rect, fill, border, width, radius);
        self.capture.area()
    }

    /// Starts a compositing layer above earlier content, keeping batching within the layer.
    /// A fresh batch alone is not a barrier: Makepad can search past it for an older batch
    /// with matching text textures/uniforms. Isolate every primitive with the same group,
    /// as TextInput does, and reserve the background before any foreground primitives.
    pub fn new_layer(&mut self, cx: &mut Cx2d) {
        let group = LiveId::unique();
        for vars in [
            &mut self.draw_box.draw_vars,
            &mut self.draw_ring.draw_vars,
            &mut self.text_regular.draw_vars,
            &mut self.text_medium.draw_vars,
            &mut self.text_mono.draw_vars,
            &mut self.text_icon.draw_vars,
        ] {
            vars.set_draw_call_group(group);
        }
        cx.new_draw_call(&self.draw_box.draw_vars);
    }

    /// Horizontal hairline (Flutter `Divider(height: 1)`).
    pub fn hline(&mut self, cx: &mut Cx2d, x: f64, y: f64, width: f64, color: Vec4) {
        self.draw_box.paint(cx, Rect { pos: dvec2(x, y), size: dvec2(width, 1.0) }, color, 0.0);
    }

    pub fn vline(&mut self, cx: &mut Cx2d, x: f64, y: f64, height: f64, color: Vec4) {
        self.draw_box.paint(cx, Rect { pos: dvec2(x, y), size: dvec2(1.0, height) }, color, 0.0);
    }

    pub fn dot(&mut self, cx: &mut Cx2d, center: DVec2, diameter: f64, color: Vec4) {
        let r = diameter * 0.5;
        self.draw_box.paint(
            cx,
            Rect { pos: dvec2(center.x - r, center.y - r), size: dvec2(diameter, diameter) },
            color,
            r,
        );
    }

    /// Ring or arc (`progress` of a full turn starting at `rotation` turns).
    pub fn ring(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4, stroke: f64, progress: f64, rotation: f64) {
        self.draw_ring.color = Vec4 { w: color.w * self.alpha, ..color };
        self.draw_ring.stroke = stroke as f32;
        self.draw_ring.progress = progress as f32;
        self.draw_ring.rotation = rotation as f32;
        self.draw_ring.draw_abs(cx, rect);
    }
}

/// Rects painted for interactive items, resolved against pointer positions.
#[derive(Clone, Debug)]
pub struct HitMap<T: Clone> {
    items: Vec<(Rect, T)>,
    clip: Option<Rect>,
}

impl<T: Clone> Default for HitMap<T> {
    fn default() -> Self {
        Self { items: Vec::new(), clip: None }
    }
}

impl<T: Clone + PartialEq> HitMap<T> {
    pub fn clear(&mut self) {
        self.items.clear();
        self.clip = None;
    }

    /// Restricts following pushes to `clip` (scrolled list viewport).
    pub fn set_clip(&mut self, clip: Option<Rect>) {
        self.clip = clip;
    }

    pub fn push(&mut self, rect: Rect, item: T) {
        let rect = match self.clip {
            Some(clip) => intersect(rect, clip),
            None => rect,
        };
        if rect.size.x > 0.0 && rect.size.y > 0.0 {
            self.items.push((rect, item));
        }
    }

    /// Topmost (last pushed) item under `pos`.
    pub fn at(&self, pos: DVec2) -> Option<T> {
        self.items.iter().rev().find(|(rect, _)| rect.contains(pos)).map(|(_, item)| item.clone())
    }

    pub fn rect_of(&self, item: &T) -> Option<Rect> {
        self.items.iter().rev().find(|(_, it)| it == item).map(|(rect, _)| *rect)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

pub fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.pos.x.max(b.pos.x);
    let y0 = a.pos.y.max(b.pos.y);
    let x1 = (a.pos.x + a.size.x).min(b.pos.x + b.size.x);
    let y1 = (a.pos.y + a.size.y).min(b.pos.y + b.size.y);
    Rect { pos: dvec2(x0, y0), size: dvec2((x1 - x0).max(0.0), (y1 - y0).max(0.0)) }
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { pos: dvec2(x, y), size: dvec2(w.max(0.0), h.max(0.0)) }
}

/// `rect` shrunk by `dx`/`dy` on each side.
pub fn inset(r: Rect, dx: f64, dy: f64) -> Rect {
    rect(r.pos.x + dx, r.pos.y + dy, r.size.x - dx * 2.0, r.size.y - dy * 2.0)
}

#[cfg(test)]
mod tests {
    use super::single_line_text;
    use std::borrow::Cow;

    #[test]
    fn single_line_labels_borrow_unchanged_text() {
        for text in ["", "hello world", "긴 한국어 제목과 English", "a\tb", "👩‍💻e\u{301}"] {
            assert!(matches!(single_line_text(text), Cow::Borrowed(s) if s == text));
        }
    }

    #[test]
    fn single_line_labels_preserve_content_across_hard_breaks() {
        for separator in ["\n", "\r", "\r\n", "\u{000b}", "\u{000c}", "\u{0085}", "\u{2028}", "\u{2029}"] {
            let text = format!("# Target{separator}packages/한글");
            assert_eq!(single_line_text(&text), "# Target packages/한글");
            assert_eq!(single_line_text(separator), " ");
        }
        assert_eq!(single_line_text("\r\n한글\n\nEnglish\u{2029}"), " 한글  English ");
        assert_eq!(single_line_text("\r\r\n\n"), "   ");
    }
}
