//! Immediate-mode drawing kit shared by the stream rows and the chat chrome:
//! rounded rects, text laid out with the pinned makepad text stack
//! (`DrawText::layout`, `Fonts::get_or_layout` for multi-span paragraphs,
//! `draw_walk_laidout`), Lucide glyphs from the shell's icon font, images,
//! spinners, and a per-widget registry of selectable text runs and hit
//! regions. Positions are stored relative to the widget rect so they stay
//! valid after PortalList shifts its items.

use crate::features::files::kit::DrawFilesImage;
use crate::theme::DrawBox;
use makepad_widgets::makepad_draw::cx_draw::CxDraw;
use makepad_widgets::makepad_draw::text::color::Color as TextColor;
use makepad_widgets::makepad_draw::text::font_family::FontFamilyId;
use makepad_widgets::makepad_draw::text::fonts::Fonts;
use makepad_widgets::makepad_draw::text::geom::Point;
use makepad_widgets::makepad_draw::text::layouter::{BorrowedLayoutParams, LaidoutText, LayoutOptions, Span, Style};
use makepad_widgets::makepad_draw::text::selection::{Cursor, Selection};
use makepad_widgets::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use makepad_widgets::makepad_draw::text::AsyncLayout;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use crate::theme::*;
    use crate::features::files::kit::DrawFilesImage;


    pub DrawChatSpinner = {{DrawChatSpinner}} {
        fn pixel(self) -> vec4 {
            let sdf = Sdf2d::viewport(self.pos * self.rect_size);
            let r = min(self.rect_size.x, self.rect_size.y) * 0.5 - self.thickness;
            let c = self.rect_size * 0.5;
            let start = self.phase * 6.283185307;
            sdf.arc_round_caps(c.x, c.y, r, start, start + 4.2, self.thickness);
            sdf.fill(self.color);
            return sdf.result;
        }
    }

    pub ChatKit = {{ChatKit}} {
        rect: <DrawBox> {}
        image: <DrawFilesImage> {}
        text_regular: { text_style: <PASEO_FONT_REGULAR> {} }
        text_medium: { text_style: <PASEO_FONT_MEDIUM> {} }
        text_semibold: { text_style: <PASEO_FONT_SEMIBOLD> {} }
        text_bold: { text_style: <PASEO_FONT_BOLD> {} }
        text_italic: { text_style: <THEME_FONT_ITALIC> {} }
        text_bold_italic: { text_style: <THEME_FONT_BOLD_ITALIC> {} }
        text_mono: { text_style: <PASEO_FONT_MONO> {} }
        text_icon: { text_style: <PASEO_FONT_ICON> {} }
        logo: {
            svg_path: "M291.495 91.399C333.897 104.892 379.155 135.075 416.229 173.191C453.389 211.394 484.429 259.725 495.708 311.251C497.555 319.693 498.865 328.216 499.586 336.776C509.755 326.554 519.867 317.815 529.89 311.547C540.647 304.821 553.808 299.297 568.641 299.785C584.29 300.299 597.395 307.326 607.747 317.632C632.173 341.947 629.612 372.898 619.872 397.936C610.185 422.833 591.557 447.826 572.732 469.124C553.591 490.78 532.713 510.308 516.779 524.318C508.775 531.355 501.936 537.073 497.07 541.052C494.635 543.043 492.689 544.603 491.334 545.679C490.657 546.217 490.126 546.635 489.756 546.926C489.571 547.071 489.425 547.184 489.321 547.265C489.269 547.305 489.227 547.338 489.196 547.362C489.181 547.374 489.168 547.385 489.157 547.393C489.153 547.397 489.147 547.401 489.144 547.403C489.134 547.4 488.837 547.06 473.001 528.499L489.135 547.411C478.157 555.911 462.033 554.334 453.122 543.89C444.213 533.448 445.887 518.094 456.861 509.592C456.863 509.591 456.865 509.588 456.869 509.586C456.88 509.577 456.902 509.561 456.933 509.536C456.997 509.487 457.101 509.404 457.245 509.292C457.533 509.066 457.979 508.715 458.569 508.247C459.749 507.31 461.506 505.901 463.742 504.073C468.216 500.414 474.589 495.088 482.073 488.508C497.114 475.284 516.315 457.282 533.578 437.75C551.157 417.862 565.26 398.01 571.859 381.048C578.403 364.227 575.681 356.302 570.724 351.367C568.928 349.579 567.744 348.902 567.267 348.676C566.888 348.496 566.811 348.52 566.804 348.52C566.605 348.513 563.971 348.537 557.953 352.3C545.161 360.299 528.815 377.492 506.807 403.867C494.927 418.106 481.871 434.435 467.547 451.957C463.709 457.28 459.503 462.538 454.91 467.717L454.702 467.549C420.808 508.347 380.37 553.856 332.335 593.848C301.853 619.226 262.656 622.597 228.642 614.743C194.834 606.936 162.658 587.448 142.217 561.686C108.054 518.631 100.57 469.801 108.223 427.836C115.56 387.606 137.391 351.005 166.502 331.557C161.248 315.813 156.813 299.49 153.519 283.013C142.593 228.368 143.239 167.031 174.28 119.619C186.922 100.31 205.846 89.1535 227.387 85.2773C248.1 81.5504 270.278 84.648 291.495 91.399ZM378.642 206.356C345.773 172.563 307.463 147.917 275.208 137.654C259.096 132.527 246.171 131.514 236.828 133.195C228.314 134.727 222.227 138.497 217.721 145.38C196.712 177.468 193.858 224.004 203.82 273.827C206.532 287.394 210.127 300.834 214.345 313.817C236.45 310.276 260.156 311.463 281.22 317.11C319.621 327.403 357.501 355.419 357.501 405.654C357.501 435.255 339.111 465.136 307.278 473.815C273.211 483.103 238.854 464.822 213.105 427.541C203.716 413.947 194.443 397.766 185.947 379.89C174.028 392.223 163.08 411.953 158.673 436.118C153.128 466.518 158.514 501.286 183.085 532.253C195.993 548.522 217.742 562.031 240.771 567.349C263.594 572.619 284.147 569.24 298.664 557.154C349.383 514.927 390.709 466.547 426.366 422.952C448.879 390.86 453.195 356.06 445.578 321.265C436.703 280.718 411.425 240.06 378.642 206.356ZM306.296 405.722C306.296 384.769 292.223 370.736 267.284 364.051C256.012 361.03 244.156 360.087 233.095 360.771C240.361 375.935 248.168 389.513 255.897 400.704C275.647 429.298 289.989 427.822 293.247 426.934C298.737 425.437 306.296 418.161 306.296 405.722Z"
        }
    }
}


#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawChatSpinner {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    pub color: Vec4,
    #[live(2.0)]
    pub thickness: f32,
    #[live]
    pub phase: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    Regular,
    Medium,
    Semibold,
    Bold,
    Italic,
    BoldItalic,
    Mono,
    Icon,
}

/// Natural line height of each family in ems (ascender − descender + gap),
/// used to turn Flutter's `height` ratios into makepad line spacing scales.
fn natural_line(font: Font) -> f64 {
    match font {
        Font::Mono => 1.13,
        Font::Icon => 1.0,
        _ => 1.3,
    }
}

/// One styled stretch (in bytes) of a rich text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RichSpan {
    pub len: usize,
    pub font: Font,
    pub size_px: f64,
    pub color: Vec4,
}

/// The font family a DrawText's text style resolved to (multi-span layout needs it).
fn family_id(dt: &DrawText) -> FontFamilyId {
    dt.text_style.font_family.font_family_id()
}

fn text_color(color: Vec4) -> TextColor {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    TextColor { r: byte(color.x), g: byte(color.y), b: byte(color.z), a: byte(color.w) }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct ChatKit {
    #[live]
    pub rect: DrawBox,
    #[live]
    pub image: DrawFilesImage,
    #[live]
    pub spinner: DrawChatSpinner,
    #[live]
    pub logo: DrawIcon,
    #[live]
    pub text_regular: DrawText,
    #[live]
    pub text_medium: DrawText,
    #[live]
    pub text_semibold: DrawText,
    #[live]
    pub text_bold: DrawText,
    #[live]
    pub text_italic: DrawText,
    #[live]
    pub text_bold_italic: DrawText,
    #[live]
    pub text_mono: DrawText,
    #[live]
    pub text_icon: DrawText,
    /// Measuring pass: layout runs, nothing is drawn or registered.
    #[rust]
    pub dry: bool,
    /// Only set by final-position painting, never preliminary PortalList layout.
    #[rust]
    pub viewport: Option<Rect>,
    /// The draw-call group `begin_layer` opened (rects return to it after `rects_on_top`).
    #[rust]
    layer_group: LiveId,
}

pub fn rgba(hex: u32) -> Vec4 {
    let a = ((hex >> 24) & 0xff) as f32 / 255.0;
    let r = ((hex >> 16) & 0xff) as f32 / 255.0;
    let g = ((hex >> 8) & 0xff) as f32 / 255.0;
    let b = (hex & 0xff) as f32 / 255.0;
    vec4(r, g, b, a)
}

/// `#rrggbb` (syntax theme colors).
pub fn hex_color(hex: &str) -> Vec4 {
    let digits = hex.trim_start_matches('#');
    match u32::from_str_radix(digits, 16) {
        Ok(v) if digits.len() == 6 => rgba(0xff00_0000 | v),
        Ok(v) if digits.len() == 8 => rgba(v.rotate_right(8)),
        _ => vec4(1.0, 1.0, 1.0, 1.0),
    }
}

pub fn with_alpha(color: Vec4, alpha: f32) -> Vec4 {
    vec4(color.x, color.y, color.z, color.w * alpha)
}

pub const TRANSPARENT: Vec4 = Vec4 { x: 0.0, y: 0.0, z: 0.0, w: 0.0 };

impl ChatKit {
    fn text(&mut self, font: Font) -> &mut DrawText {
        match font {
            Font::Regular => &mut self.text_regular,
            Font::Medium => &mut self.text_medium,
            Font::Semibold => &mut self.text_semibold,
            Font::Bold => &mut self.text_bold,
            Font::Italic => &mut self.text_italic,
            Font::BoldItalic => &mut self.text_bold_italic,
            Font::Mono => &mut self.text_mono,
            Font::Icon => &mut self.text_icon,
        }
    }

    /// Lays out `text` in `font` at `size_px` logical pixels with a line
    /// height of `height` × size (Flutter TextStyle.height), wrapping to
    /// `max_width` when given.
    #[allow(clippy::too_many_arguments)]
    pub fn layout(&mut self, cx: &mut Cx, font: Font, size_px: f64, height: f64, text: &str, max_width: Option<f64>, indent: f64) -> Rc<LaidoutText> {
        let dt = self.text(font);
        dt.text_style.font_size = (size_px * 0.75) as f32;
        dt.text_style.line_spacing = (height / natural_line(font)).max(0.5) as f32;
        dt.layout(cx, indent as f32, 0.0, max_width.map(|w| w.max(1.0) as f32), max_width.is_some(), Align::default(), text)
    }

    /// One laid-out text with styled spans (bold/italic/mono/colored runs in
    /// a single paragraph), so wrapping, baselines and selection are those of
    /// the whole paragraph. `height` is the paragraph's line height ratio.
    pub fn layout_rich(&mut self, cx: &mut Cx, text: &str, spans: &[RichSpan], max_width: Option<f64>, height: f64) -> Rc<LaidoutText> {
        let base = spans.first().map(|s| s.font).unwrap_or(Font::Regular);
        let layout_spans: Vec<Span> = spans
            .iter()
            .map(|span| self.rich_span(*span))
            .collect();
        self.layout_mapped_rich(cx, text, &layout_spans, base, max_width, height)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn layout_mapped_rich(&mut self, cx: &mut Cx, text: &str, spans: &[Span], base: Font, max_width: Option<f64>, height: f64) -> Rc<LaidoutText> {
        CxDraw::lazy_construct_fonts(cx);
        let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
        let mut fonts = fonts.borrow_mut();
        fonts.get_or_layout(BorrowedLayoutParams {
            text,
            spans,
            options: Self::rich_layout_options(base, max_width, height),
        })
    }

    pub fn rich_span(&mut self, span: RichSpan) -> Span {
        Span {
            style: Style {
                font_family_id: family_id(self.text(span.font)),
                font_size_in_pts: (span.size_px * 0.75) as f32,
                color: Some(text_color(span.color)),
            },
            len: span.len,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn advance_rich_layout(&mut self, cx: &mut Cx, request: &mut AsyncLayout, revision: u64, text: Arc<str>, spans: Arc<Vec<Span>>, base: Font, max_width: Option<f64>, height: f64) {
        CxDraw::lazy_construct_fonts(cx);
        let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
        let mut fonts = fonts.borrow_mut();
        request.request(&mut fonts, revision, text, spans, Self::rich_layout_options(base, max_width, height));
        request.poll(&mut fonts, 4096, 128);
    }

    fn rich_layout_options(base: Font, max_width: Option<f64>, height: f64) -> LayoutOptions {
        LayoutOptions {
            first_row_indent_in_lpxs: 0.0,
            first_row_min_line_spacing_below_in_lpxs: 0.0,
            max_width_in_lpxs: max_width.map(|w| w.max(1.0) as f32),
            wrap: max_width.is_some(),
            align: 0.0,
            line_spacing_scale: (height / natural_line(base)).max(0.5) as f32,
        }
    }

    /// Isolate every primitive from earlier batches, then reserve the
    /// background before text. A new rect call alone is not a layer barrier.
    pub fn begin_layer(&mut self, cx: &mut Cx2d) {
        let group = LiveId::unique();
        self.layer_group = group;
        for vars in [
            &mut self.rect.draw_vars,
            &mut self.image.draw_vars,
            &mut self.spinner.draw_vars,
            &mut self.logo.draw_vars,
            &mut self.text_regular.draw_vars,
            &mut self.text_medium.draw_vars,
            &mut self.text_semibold.draw_vars,
            &mut self.text_bold.draw_vars,
            &mut self.text_italic.draw_vars,
            &mut self.text_bold_italic.draw_vars,
            &mut self.text_mono.draw_vars,
            &mut self.text_icon.draw_vars,
        ] {
            vars.set_draw_call_group(group);
        }
        self.rect.new_draw_call(cx);
    }

    /// Draws `paint`'s rects above everything drawn so far in this layer (scroll bars, edge
    /// fades over text), then returns rects to the layer's background batch.
    pub fn rects_on_top(&mut self, cx: &mut Cx2d, paint: impl FnOnce(&mut Self, &mut Cx2d)) {
        if self.dry {
            return;
        }
        self.rect.draw_vars.set_draw_call_group(LiveId::unique());
        self.rect.new_draw_call(cx);
        paint(self, cx);
        self.rect.draw_vars.set_draw_call_group(self.layer_group);
    }

    pub fn draw_laidout(&mut self, cx: &mut Cx2d, font: Font, color: Vec4, pos: DVec2, laidout: &LaidoutText) {
        if self.dry {
            return;
        }
        let size = laidout_size(laidout);
        let viewport = self.viewport;
        let dt = self.text(font);
        dt.color = color;
        if let Some(viewport) = viewport {
            dt.draw_abs_laidout_viewport(cx, pos, laidout, viewport);
        } else {
            dt.draw_walk_laidout(
                cx,
                Walk { abs_pos: Some(pos), margin: Margin::default(), width: Size::Fixed(size.x.max(1.0)), height: Size::Fixed(size.y.max(1.0)) },
                laidout,
            );
        }
    }

    /// Draws a rich layout; glyph colors come from its spans.
    pub fn draw_rich(&mut self, cx: &mut Cx2d, pos: DVec2, laidout: &LaidoutText) {
        self.draw_laidout(cx, Font::Regular, vec4(1.0, 1.0, 1.0, 1.0), pos, laidout);
    }

    /// One-shot label: lays out and draws, returning the drawn size.
    #[allow(clippy::too_many_arguments)]
    pub fn label(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, height: f64, color: Vec4, pos: DVec2, text: &str, max_width: Option<f64>) -> DVec2 {
        if text.is_empty() {
            return DVec2::default();
        }
        let laidout = self.layout(cx, font, size_px, height, text, max_width, 0.0);
        self.draw_laidout(cx, font, color, pos, &laidout);
        laidout_size(&laidout)
    }

    pub fn measure(&mut self, cx: &mut Cx, font: Font, size_px: f64, height: f64, text: &str, max_width: Option<f64>) -> DVec2 {
        if text.is_empty() {
            return DVec2::default();
        }
        laidout_size(&self.layout(cx, font, size_px, height, text, max_width, 0.0))
    }

    /// `maxLines: lines, overflow: ellipsis`: the wrapped layout of the longest
    /// prefix (plus "…") that fits in `lines` rows of `max_width`.
    #[allow(clippy::too_many_arguments)]
    pub fn layout_clamped(&mut self, cx: &mut Cx, font: Font, size_px: f64, height: f64, text: &str, max_width: f64, lines: usize) -> Rc<LaidoutText> {
        let line = (lines == 1).then(|| crate::common::paint::single_line_text(text));
        let text = line.as_deref().unwrap_or(text);
        let wrap = lines > 1;
        let fits = |kit: &mut Self, cx: &mut Cx, candidate: &str| {
            let laid = kit.layout(cx, font, size_px, height, candidate, wrap.then_some(max_width), 0.0);
            let ok = if wrap { laid.rows.len() <= lines } else { laidout_size(&laid).x <= max_width + 0.5 };
            (ok, laid)
        };
        let (ok, laid) = fits(self, cx, text);
        if ok {
            return laid;
        }
        let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let (mut lo, mut hi) = (0usize, bounds.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let candidate = format!("{}…", text[..bounds.get(mid).copied().unwrap_or(text.len())].trim_end());
            if fits(self, cx, &candidate).0 { lo = mid } else { hi = mid - 1 }
        }
        let candidate = format!("{}…", text[..bounds.get(lo).copied().unwrap_or(text.len())].trim_end());
        fits(self, cx, &candidate).1
    }

    /// A single ellipsized line (`maxLines: 1`); returns the drawn size.
    #[allow(clippy::too_many_arguments)]
    pub fn label_line(&mut self, cx: &mut Cx2d, font: Font, size_px: f64, height: f64, color: Vec4, pos: DVec2, text: &str, max_width: f64) -> DVec2 {
        if text.is_empty() {
            return DVec2::default();
        }
        let laid = self.layout_clamped(cx, font, size_px, height, text, max_width.max(1.0), 1);
        self.draw_laidout(cx, font, color, pos, &laid);
        laidout_size(&laid)
    }

    pub fn rect(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4, radius: f64) {
        let rr = radius as f32;
        self.rect_full(cx, r, color, vec4(rr, rr, rr, rr), TRANSPARENT, 0.0);
    }

    pub fn bordered(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4, radius: f64, border: Vec4, width: f64) {
        let rr = radius as f32;
        self.rect_full(cx, r, color, vec4(rr, rr, rr, rr), border, width);
    }

    /// A box with per-corner radii (left-top, right-top, right-bottom,
    /// left-bottom) through the shell's exact rounded-rect `DrawBox`: equal
    /// corners draw directly; top-only / bottom-only rounding draws the box
    /// extended past its square side and clips the extension (redrawing that
    /// side's border); mixed corners (the user bubble's 16/2/16/16) square the
    /// smaller corners back in with corner patches of their own radius.
    pub fn rect_full(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4, radii: Vec4, border: Vec4, width: f64) {
        if r.size.x <= 0.0 || r.size.y <= 0.0 || self.dry {
            return;
        }
        let corners = [radii.x as f64, radii.y as f64, radii.z as f64, radii.w as f64];
        let [lt, rt, rb, lb] = corners;
        if lt == rt && rt == rb && rb == lb {
            self.rect.paint_bordered(cx, r, color, border, width, lt);
            return;
        }
        let top = lt == rt && rb == 0.0 && lb == 0.0;
        let bottom = rb == lb && lt == 0.0 && rt == 0.0;
        if top || bottom {
            let radius = if top { lt } else { rb };
            let extended = if top {
                Rect { pos: r.pos, size: dvec2(r.size.x, r.size.y + radius) }
            } else {
                Rect { pos: r.pos - dvec2(0.0, radius), size: dvec2(r.size.x, r.size.y + radius) }
            };
            cx.begin_turtle(Walk::abs_rect(r), Layout { clip_x: true, clip_y: true, ..Layout::default() });
            self.rect.paint_bordered(cx, extended, color, border, width, radius);
            cx.end_turtle();
            if width > 0.0 && border.w > 0.0 {
                let y = if top { r.pos.y + r.size.y - width } else { r.pos.y };
                self.rect.paint(cx, rect_xywh(r.pos.x, y, r.size.x, width), border, 0.0);
            }
            return;
        }
        let big = lt.max(rt).max(rb).max(lb).min(r.size.x.min(r.size.y) * 0.5);
        self.rect.paint_bordered(cx, r, color, border, width, big);
        let (x0, y0, x1, y1) = (r.pos.x, r.pos.y, r.pos.x + r.size.x - big, r.pos.y + r.size.y - big);
        for (radius, x, y) in [(lt, x0, y0), (rt, x1, y0), (rb, x1, y1), (lb, x0, y1)] {
            if radius < big {
                self.rect.paint(cx, rect_xywh(x, y, big, big), color, radius);
            }
        }
    }

    /// A Lucide glyph (shell icon font), centered in `r`.
    pub fn icon(&mut self, cx: &mut Cx2d, name: &str, r: Rect, size_px: f64, color: Vec4) {
        let Some(glyph) = crate::common::icons::glyph(name) else { return };
        let mut buffer = [0u8; 4];
        let text = glyph.encode_utf8(&mut buffer);
        let laidout = self.layout(cx, Font::Icon, size_px, 1.0, text, None, 0.0);
        let size = laidout_size(&laidout);
        let pos = r.center() - size * 0.5;
        self.draw_laidout(cx, Font::Icon, color, pos, &laidout);
    }

    pub fn paseo_logo(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4) {
        if self.dry {
            return;
        }
        self.logo.color = color;
        self.logo.draw_walk(cx, Walk { abs_pos: Some(r.pos), margin: Margin::default(), width: Size::Fixed(r.size.x), height: Size::Fixed(r.size.y) });
    }

    pub fn spinner(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4, phase: f64) {
        if self.dry {
            return;
        }
        self.spinner.color = color;
        self.spinner.phase = phase.fract() as f32;
        self.spinner.thickness = (r.size.x / 9.0).max(1.5) as f32;
        self.spinner.draw_abs(cx, r);
    }

    pub fn texture(&mut self, cx: &mut Cx2d, r: Rect, texture: &Texture, radius: f64) {
        if self.dry {
            return;
        }
        self.image.draw_texture(cx, r, texture, radius, 1.0);
    }
}

pub fn laidout_size(laidout: &LaidoutText) -> DVec2 {
    dvec2(laidout.size_in_lpxs.width as f64, laidout.size_in_lpxs.height as f64)
}

/// Rects (relative to the text origin) covering bytes `start..end`.
pub fn range_rects(laidout: &LaidoutText, start: usize, end: usize) -> Vec<Rect> {
    range_rects_between(laidout, start, end, f32::NEG_INFINITY, f32::INFINITY)
}

/// Paint-only range geometry. Source offsets and hit-testing keep the full layout.
pub fn range_rects_visible(laidout: &LaidoutText, start: usize, end: usize, pos: DVec2, viewport: Option<Rect>) -> Vec<Rect> {
    let Some(viewport) = viewport else { return range_rects(laidout, start, end) };
    range_rects_between(laidout, start, end, (viewport.pos.y - pos.y) as f32, (viewport.pos.y + viewport.size.y - pos.y) as f32)
}

/// A byte range has no caret affinity: at a soft wrap its first byte belongs to
/// the row that draws it and its end to the row of its last byte. A fragment
/// without width covers no glyph (a range starting at a row's hanging spaces or
/// line break), so padded backgrounds must not receive it.
fn range_rects_between(laidout: &LaidoutText, start: usize, end: usize, top: f32, bottom: f32) -> Vec<Rect> {
    let start = start.min(laidout.text.len());
    let end = end.min(laidout.text.len());
    if end <= start {
        return Vec::new();
    }
    let selection = Selection {
        anchor: Cursor { index: start, prefer_next_row: true },
        cursor: Cursor { index: end, prefer_next_row: false },
    };
    laidout.selection_rects_in_lpxs_clipped(selection, top, bottom)
        .into_iter()
        .filter(|r| r.size.width > 0.0)
        .map(|r| Rect { pos: dvec2(r.origin.x as f64, r.origin.y as f64), size: dvec2(r.size.width as f64, r.size.height as f64) })
        .collect()
}

/// Everything clickable in the timeline.
#[derive(Clone, Debug, PartialEq)]
pub enum HitTarget {
    Link { href: Arc<str>, kind: super::links::FileLinkSourceType, text: Arc<str> },
    /// Copy a code block / turn / message; `key` drives the 1.5s check glyph.
    Copy { key: String, text: makepad_widgets::makepad_draw::text::substr::Substr },
    Image { key: String },
    UserImage { index: usize },
    Toggle(String),
    OpenSheet(String),
    OpenFile(String),
    RewindMenu,
    ForkMenu { completed: bool },
    PermissionAction { key: String, action: String },
    QuestionTab { key: String, index: usize },
    QuestionOption { key: String, index: usize },
    QuestionDismiss { key: String },
    QuestionPrimary { key: String },
    /// Horizontal scroll region (code panes, tables, detail blocks).
    ScrollX(String),
    /// Vertically capped detail region.
    ScrollY(String),
    /// A region's scroll bar strip along one axis (thumb drag, track paging).
    ScrollBar { key: String, vertical: bool, track: super::scrollbar::Track },
    /// Panel chrome.
    Chrome(String),
}

impl HitTarget {
    pub fn is_link(&self) -> bool {
        matches!(self, HitTarget::Link { .. })
    }

    /// Scroll regions pass clicks through to selection.
    pub fn is_scroll_region(&self) -> bool {
        matches!(self, HitTarget::ScrollX(_) | HitTarget::ScrollY(_))
    }
}

/// A selectable run of laid-out text.
pub struct TextRun {
    pub laidout: Rc<LaidoutText>,
    /// Relative to the canvas origin (the widget rect's top-left).
    pub origin: DVec2,
    /// Offset of this run in the logical concatenated source.
    pub offset: usize,
    pub len: usize,
    /// Visible part (clipped regions); hit tests ignore the rest.
    pub clip: Option<Rect>,
    /// Separator inserted before this run, if any; not copied every paint.
    pub separator: Option<char>,
}

/// Selectable text and hit regions recorded while drawing one widget.
#[derive(Default)]
pub struct Canvas {
    /// Absolute position of the widget rect while drawing.
    pub origin: DVec2,
    pub runs: Vec<TextRun>,
    source_len: usize,
    last_char: Option<char>,
    /// Final absolute viewport, supplied only after measurement/alignment.
    pub viewport: Option<Rect>,
    pub hits: Vec<(Rect, HitTarget)>,
    /// (anchor, cursor) offsets in the logical concatenated source.
    pub selection: Option<(usize, usize)>,
    pub clip_stack: Vec<Rect>,
    /// Measuring pass: nothing is registered.
    pub dry: bool,
}

impl Canvas {
    pub fn begin(&mut self, origin: DVec2) {
        self.origin = origin;
        self.runs.clear();
        self.source_len = 0;
        self.last_char = None;
        self.hits.clear();
        self.clip_stack.clear();
    }

    /// Registers a run drawn at absolute `pos`; returns its plain offset.
    /// Blocks are separated by `separator` in the copied text.
    pub fn add_run(&mut self, laidout: Rc<LaidoutText>, pos: DVec2, separator: Option<char>) -> usize {
        if self.dry {
            return self.source_len;
        }
        let separator = separator.filter(|sep| self.source_len > 0 && self.last_char != Some(*sep));
        if let Some(sep) = separator {
            self.source_len += sep.len_utf8();
            self.last_char = Some(sep);
        }
        let offset = self.source_len;
        let len = laidout.text.len();
        self.source_len += len;
        if let Some(last) = laidout.text.chars().next_back() {
            self.last_char = Some(last);
        }
        let clip = self.clip_stack.last().copied();
        self.runs.push(TextRun { laidout, origin: pos - self.origin, offset, len, clip, separator });
        offset
    }

    pub fn source_len(&self) -> usize {
        self.source_len
    }

    fn source_boundary(&self, index: usize) -> usize {
        let index = index.min(self.source_len);
        let next = self.runs.partition_point(|run| run.offset <= index);
        if let Some(run) = next.checked_sub(1).and_then(|index| self.runs.get(index))
            && index <= run.offset + run.len
        {
            return run.offset + floor_char_boundary(&run.laidout.text, index - run.offset);
        }
        self.runs.get(next).and_then(|run| run.separator.map(|sep| run.offset - sep.len_utf8())).unwrap_or(index)
    }

    /// Materialize only on explicit copy/word operations, never during repaint.
    fn source_slice(&self, start: usize, end: usize) -> String {
        let (start, end) = (self.source_boundary(start), self.source_boundary(end));
        let mut text = String::with_capacity(end.saturating_sub(start));
        for run in &self.runs {
            if let Some(sep) = run.separator {
                let at = run.offset - sep.len_utf8();
                if at >= start && at < end {
                    text.push(sep);
                }
            }
            let a = start.max(run.offset);
            let b = end.min(run.offset + run.len);
            if a < b {
                text.push_str(&run.laidout.text[a - run.offset..b - run.offset]);
            }
            if run.offset + run.len >= end {
                break;
            }
        }
        text
    }

    pub fn add_hit(&mut self, r: Rect, target: HitTarget) {
        if self.dry {
            return;
        }
        let mut r = Rect { pos: r.pos - self.origin, size: r.size };
        if let Some(clip) = self.clip_stack.last() {
            r = r.clip((clip.pos, clip.pos + clip.size));
        }
        if r.size.x > 0.0 && r.size.y > 0.0 {
            self.hits.push((r, target));
        }
    }

    /// A hit region beneath everything registered so far in this widget
    /// (a badge header under its open-file glyph).
    pub fn add_hit_under(&mut self, r: Rect, target: HitTarget) {
        let before = self.hits.len();
        self.add_hit(r, target);
        if self.hits.len() > before {
            let hit = self.hits.pop().expect("pushed above");
            self.hits.insert(0, hit);
        }
    }

    /// Clip region (absolute) for runs/hits registered until `pop_clip`.
    pub fn push_clip(&mut self, r: Rect) {
        let mut rel = Rect { pos: r.pos - self.origin, size: r.size };
        if let Some(outer) = self.clip_stack.last() {
            rel = rel.clip((outer.pos, outer.pos + outer.size));
        }
        self.clip_stack.push(rel);
    }

    pub fn pop_clip(&mut self) {
        self.clip_stack.pop();
    }

    /// Topmost hit target at `rel` (relative to the widget rect).
    pub fn hit_at(&self, rel: DVec2) -> Option<&HitTarget> {
        self.hits.iter().rev().find(|(r, t)| r.contains(rel) && !t.is_scroll_region()).map(|(_, t)| t)
    }

    /// The deepest scroll region at `rel` along the axis.
    pub fn scroll_region_at(&self, rel: DVec2, vertical: bool) -> Option<&str> {
        self.scroll_regions_at(rel, vertical).next()
    }

    /// Scroll regions at `rel` along the axis, deepest first (enclosing
    /// regions register before their content).
    pub fn scroll_regions_at(&self, rel: DVec2, vertical: bool) -> impl Iterator<Item = &str> {
        self.hits.iter().rev().filter_map(move |(r, t)| match t {
            HitTarget::ScrollX(key) if !vertical && r.contains(rel) => Some(key.as_str()),
            HitTarget::ScrollY(key) if vertical && r.contains(rel) => Some(key.as_str()),
            _ => None,
        })
    }

    /// Text offset nearest to `rel`.
    pub fn offset_at(&self, rel: DVec2) -> Option<usize> {
        self.offset_at_impl(rel, false)
    }

    pub fn offset_at_handle(&self, rel: DVec2) -> Option<usize> {
        self.offset_at_impl(rel, true)
    }

    fn offset_at_impl(&self, rel: DVec2, handle: bool) -> Option<usize> {
        let mut best: Option<(f64, usize)> = None;
        for run in &self.runs {
            if let Some(clip) = run.clip
                && !clip.contains(rel) && !handle
            {
                continue;
            }
            let local = rel - run.origin;
            let size = laidout_size(&run.laidout);
            let dy = if local.y < 0.0 { -local.y } else if local.y > size.y { local.y - size.y } else { 0.0 };
            let dx = if local.x < 0.0 { -local.x } else if local.x > size.x { local.x - size.x } else { 0.0 };
            let distance = dy * 1000.0 + dx;
            if best.is_some_and(|(d, _)| d <= distance) {
                continue;
            }
            let visible = run.clip.map(|clip| dvec2(
                rel.x.clamp(clip.pos.x, (clip.pos.x + clip.size.x).max(clip.pos.x)),
                rel.y.clamp(clip.pos.y, (clip.pos.y + clip.size.y).max(clip.pos.y)),
            )).unwrap_or(rel) - run.origin;
            let point = Point::new(visible.x.clamp(0.0, size.x) as f32, visible.y.clamp(0.0, size.y.max(0.0)) as f32);
            let cursor = run.laidout.point_in_lpxs_to_cursor(point);
            best = Some((distance, run.offset + cursor.index.min(run.len)));
        }
        best.map(|(_, offset)| offset)
    }

    /// Endpoint geometry in current draw coordinates, retaining inner pane clipping.
    pub fn caret_rect(&self, offset: usize, prefer_next: bool) -> Option<Rect> {
        let run = if prefer_next {
            self.runs.iter().rev().find(|run| offset >= run.offset && offset <= run.offset + run.len)
        } else {
            self.runs.iter().find(|run| offset >= run.offset && offset <= run.offset + run.len)
        }?;
        if run.laidout.rows.is_empty() {
            return None;
        }
        let index = floor_char_boundary(&run.laidout.text, offset - run.offset);
        let pos = run.laidout.cursor_to_position(Cursor { index, prefer_next_row: prefer_next });
        let row = &run.laidout.rows[pos.row_index];
        let mut caret = Rect {
            pos: run.origin + dvec2(pos.x_in_lpxs as f64, (row.origin_in_lpxs.y - row.ascender_in_lpxs) as f64),
            size: dvec2(1.0, (row.ascender_in_lpxs - row.descender_in_lpxs) as f64),
        };
        if let Some(clip) = run.clip {
            caret = caret.clip((clip.pos, clip.pos + clip.size));
        }
        Some(caret.translate(self.origin))
    }

    pub fn selected_range(&self) -> Option<(usize, usize)> {
        let (a, b) = self.selection?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let end = end.min(self.source_len);
        (end > start).then_some((start, end))
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selected_range()?;
        Some(self.source_slice(start, end))
    }

    /// Detect replacements before the selected endpoint, while allowing a streamed
    /// suffix and geometry-only reflow to keep their source-indexed selection.
    pub fn selection_source_hash(&self) -> Option<u64> {
        let (_, end) = self.selected_range()?;
        let end = self.source_boundary(end);
        let mut hash = 0u64;
        for run in &self.runs {
            if let Some(sep) = run.separator
                && run.offset - sep.len_utf8() < end
            {
                let mut bytes = [0; 4];
                for byte in sep.encode_utf8(&mut bytes).bytes() {
                    hash = hash.wrapping_mul(257).wrapping_add(byte as u64 + 1);
                }
            }
            let len = end.saturating_sub(run.offset).min(run.len);
            let (mut power, mut base, mut exponent) = (1u64, 257u64, len);
            while exponent != 0 {
                if exponent & 1 != 0 { power = power.wrapping_mul(base); }
                base = base.wrapping_mul(base);
                exponent >>= 1;
            }
            hash = hash.wrapping_mul(power).wrapping_add(run.laidout.text_prefix_hash(len));
            if run.offset + run.len >= end {
                break;
            }
        }
        Some(hash)
    }

    /// Word around `offset` (double-tap / long-press selection).
    pub fn word_at(&self, offset: usize) -> (usize, usize) {
        let offset = self.source_boundary(offset);
        let Some(index) = self.runs.iter().position(|run| offset >= run.offset && offset < run.offset + run.len)
            .or_else(|| (offset == self.source_len).then(|| self.runs.len().checked_sub(1)).flatten())
        else { return (offset, offset) };
        let run = &self.runs[index];
        // A rich paragraph is one indexed run. Only legacy adjacent runs without
        // separators require joining for cross-run Unicode word boundaries.
        let joined_before = index > 0 && run.separator.is_none();
        let joined_after = self.runs.get(index + 1).is_some_and(|next| next.separator.is_none());
        if !joined_before && !joined_after {
            let (a, b) = run.laidout.word_at(offset - run.offset);
            return (run.offset + a, run.offset + b);
        }
        use unicode_segmentation::UnicodeSegmentation;
        let text = self.source_slice(0, self.source_len);
        text.split_word_bound_indices()
            .find(|(start, word)| offset >= *start && offset < *start + word.len())
            .or_else(|| text.split_word_bound_indices().next_back().filter(|_| offset == text.len()))
            .map(|(start, word)| (start, start + word.len()))
            .unwrap_or((offset, offset))
    }

    /// Selection highlight rects (absolute, from the current canvas origin).
    pub fn selection_rects(&self) -> Vec<Rect> {
        let Some((start, end)) = self.selected_range() else { return Vec::new() };
        let mut out = Vec::new();
        for run in &self.runs {
            let a = start.max(run.offset);
            let b = end.min(run.offset + run.len);
            if a >= b {
                continue;
            }
            for r in range_rects_visible(&run.laidout, a - run.offset, b - run.offset, self.origin + run.origin, self.viewport) {
                let mut rect = r.translate(self.origin + run.origin);
                if let Some(clip) = run.clip {
                    let abs = clip.translate(self.origin);
                    rect = rect.clip((abs.pos, abs.pos + abs.size));
                }
                if rect.size.x > 0.0 && rect.size.y > 0.0 {
                    out.push(rect);
                }
            }
        }
        out
    }
}

pub fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut i = index.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn rect_xywh(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { pos: dvec2(x, y), size: dvec2(w, h) }
}

#[cfg(test)]
mod selection_handle_tests {
    use super::*;

    fn append(canvas: &mut Canvas, text: &str, separator: Option<char>) {
        let layout = LaidoutText::from_rows(text.into(), Vec::new());
        canvas.add_run(Rc::new(layout), DVec2::default(), separator);
    }

    fn source(text: &str) -> Canvas {
        let mut canvas = Canvas::default();
        append(&mut canvas, text, None);
        canvas
    }

    #[test]
    fn touch_word_selection_keeps_hangul_and_emoji_clusters() {
        let text = "한글 👩\u{200d}💻 e\u{301}";
        let canvas = source(text);
        for word in ["한글", "👩\u{200d}💻", "e\u{301}"] {
            let offset = text.find(word).unwrap();
            let (a, b) = canvas.word_at(offset);
            assert_eq!(canvas.source_slice(a, b), word);
        }
    }

    #[test]
    fn crossing_endpoints_keeps_source_text_and_whitespace() {
        let text = "한글\n  code\t🙂\n";
        let mut canvas = source(text);
        let end = canvas.source_len();
        canvas.selection = Some((end, 0));
        assert_eq!(canvas.selected_text().as_deref(), Some(text));
        let hash = canvas.selection_source_hash();
        append(&mut canvas, "streamed suffix", None);
        assert_eq!(canvas.selection_source_hash(), hash);
        canvas.begin(DVec2::default());
        append(&mut canvas, "다글\n  code\t🙂\nstreamed suffix", None);
        assert_ne!(canvas.selection_source_hash(), hash);
    }

    #[test]
    fn retained_runs_preserve_separators_and_cross_run_words() {
        let mut canvas = source("e");
        append(&mut canvas, "\u{301}", None);
        append(&mut canvas, "한글", Some('\n'));
        append(&mut canvas, "🙂", Some('🚀'));
        canvas.selection = Some((0, canvas.source_len()));
        assert_eq!(canvas.selected_text().as_deref(), Some("e\u{301}\n한글🚀🙂"));
        assert_eq!(canvas.word_at(0), (0, "e\u{301}".len()));
        let expected = source("e\u{301}\n한글🚀🙂");
        assert_eq!(canvas.source_slice(0, canvas.source_len()), expected.source_slice(0, expected.source_len()));
        let mut expected = expected;
        expected.selection = canvas.selection;
        assert_eq!(canvas.selection_source_hash(), expected.selection_source_hash());
    }
}

#[cfg(test)]
mod range_rect_tests {
    use super::*;
    use makepad_widgets::makepad_draw::text::font::FontId;
    use makepad_widgets::makepad_draw::text::layouter::{Layouter, Settings};
    use makepad_widgets::makepad_draw::text::loader::{FontDefinition, FontFamilyDefinition};

    fn layout(layouter: &mut Layouter, text: &str, width: Option<f32>) -> Rc<LaidoutText> {
        let family = FontFamilyId::from("paseo-range-rect-test");
        if !layouter.is_font_family_known(family) {
            let font = FontId::from("paseo-range-rect-test");
            layouter.define_font(font, FontDefinition {
                data: Arc::new(include_bytes!("../../../resources/fonts/Jetendard-Regular.ttf").to_vec()),
                index: 0,
                ascender_fudge_in_ems: 0.0,
                descender_fudge_in_ems: 0.0,
                variations: Vec::new(),
            });
            layouter.define_font_family(family, FontFamilyDefinition { font_ids: vec![font] });
        }
        layouter.get_or_layout(BorrowedLayoutParams {
            text,
            spans: &[Span { style: Style { font_family_id: family, font_size_in_pts: 12.0, color: None }, len: text.len() }],
            options: LayoutOptions { wrap: width.is_some(), max_width_in_lpxs: width, ..LayoutOptions::default() },
        })
    }

    fn row_top(laidout: &LaidoutText, row: usize) -> f64 {
        (laidout.rows[row].origin_in_lpxs.y - laidout.rows[row].ascender_in_lpxs) as f64
    }

    /// An inline-code chip pads every rect of its range, so a row the range
    /// only touches at its wrap boundary must not get one.
    #[test]
    fn a_range_moved_to_the_next_row_leaves_no_fragment_behind() {
        let mut layouter = Layouter::new(Settings::default());
        let text = "before code after";
        let width = layout(&mut layouter, "before co", None).size_in_lpxs.width;
        let laidout = layout(&mut layouter, text, Some(width));
        let code = text.find("code").unwrap();
        assert_eq!(laidout.rows[0].text.as_str(), "before ");
        assert_eq!(laidout.rows[1].text.start_in_parent(), code);

        for rects in [
            range_rects(&laidout, code, code + 4),
            range_rects_visible(&laidout, code, code + 4, DVec2::default(), Some(rect_xywh(0.0, 0.0, 1000.0, 1000.0))),
        ] {
            assert_eq!(rects.len(), 1, "{rects:?}");
            assert_eq!(rects[0].pos.y, row_top(&laidout, 1));
            assert!(rects[0].size.x > 0.0);
        }

        // A range ending at the wrap stays on its own row.
        let rects = range_rects(&laidout, 0, code);
        assert_eq!(rects.len(), 1, "{rects:?}");
        assert_eq!(rects[0].pos.y, row_top(&laidout, 0));

        // A range crossing the wrap has glyphs on both rows.
        let rects = range_rects(&laidout, 3, code + 2);
        assert_eq!(rects.iter().map(|r| r.pos.y).collect::<Vec<_>>(), [row_top(&laidout, 0), row_top(&laidout, 1)]);
        assert!(rects.iter().all(|r| r.size.x > 0.0));
    }

    /// A tab-indented file in a Read sheet, code block or diff: each TAB is a blank advance to
    /// the next four-cell stop (Hangul takes two cells), the CR of a CRLF line takes no room,
    /// nothing falls back to the missing-glyph box, and selection still copies the source.
    #[test]
    fn tab_indented_source_keeps_columns_without_missing_glyphs() {
        let mut layouter = Layouter::new(Settings::default());
        let cell = layout(&mut layouter, "0", None).size_in_lpxs.width;
        let body = "<modelVersion>4.0.0</modelVersion>";
        let text = format!("<project>\n\t{body}\r\n\t\t<!-- 한\t글 -->");
        let laidout = layout(&mut layouter, &text, None);
        assert_eq!(laidout.rows.len(), 3);
        assert_eq!(laidout.rows[1].text.as_str(), format!("\t{body}\r"));
        assert!(laidout.rows.iter().all(|row| row.glyphs.iter().all(|glyph| glyph.id != 0)));
        let column = |row: usize, index: usize| laidout.rows[row].index_to_x_in_lpxs(index) / cell;
        let near = |actual: f32, expected: f32| (actual - expected).abs() < 0.02;

        // One leading tab: the tag starts at the first stop; the CR adds no width.
        assert!(near(column(1, 1), 4.0), "{}", column(1, 1));
        assert!(near(laidout.rows[1].width_in_lpxs / cell, 4.0 + body.len() as f32));
        // Two leading tabs, then a tab after `<!-- 한` (five cells and a two-cell syllable).
        let row = laidout.rows[2].text.as_str();
        assert!(near(column(2, row.find('<').unwrap()), 8.0));
        assert!(near(column(2, row.find('한').unwrap() + '한'.len_utf8()), 15.0));
        assert!(near(column(2, row.find('글').unwrap()), 16.0), "{}", column(2, row.find('글').unwrap()));

        // Hit testing and selection geometry follow the stops.
        assert_eq!(laidout.rows[1].x_in_lpxs_to_index(cell), 0);
        assert_eq!(laidout.rows[1].x_in_lpxs_to_index(4.0 * cell + 0.1), 1);
        let tab = text.find('\t').unwrap();
        let rects = range_rects(&laidout, tab, tab + 1);
        assert_eq!(rects.len(), 1, "{rects:?}");
        assert!((rects[0].size.x - 4.0 * cell as f64).abs() < 0.02, "{rects:?}");

        // Copying yields the original characters, TAB and CR included.
        let mut canvas = Canvas::default();
        canvas.add_run(laidout.clone(), DVec2::default(), None);
        canvas.selection = Some((0, canvas.source_len()));
        assert_eq!(canvas.selected_text().as_deref(), Some(text.as_str()));
    }
}
