//! Immediate-mode drawing kit for the files surfaces: the shell [`Painter`] (boxes, labels,
//! Lucide icons), multi-span rich text (syntax-highlighted source rows, Markdown inlines), and
//! textured quads (images, material file icons).

use std::cell::RefCell;
use std::rc::Rc;

use makepad_widgets::makepad_draw::cx_draw::CxDraw;
use makepad_widgets::makepad_draw::shader::draw_text::FontFamily as DrawTextFontFamily;
use makepad_widgets::makepad_draw::text::color::Color as TextColor;
use makepad_widgets::makepad_draw::text::font_family::FontFamilyId;
use makepad_widgets::makepad_draw::text::fonts::Fonts;
use makepad_widgets::makepad_draw::text::layouter::{
    BorrowedLayoutParams, LaidoutText, LayoutOptions, Span, Style,
};
use makepad_widgets::*;

use crate::common::paint::{Font, Painter, TextSpec};
use crate::theme::Typography;

// Makepad resolves a `#[live]` field's DSL class in the module that defines its Rust type,
// so `DrawFilesImage` and `Kit` are defined here, next to their structs.
live_design! {
    use link::theme::*;
    use link::shaders::*;
    use crate::common::paint::PaseoPainter;

    pub DrawFilesImage = {{DrawFilesImage}} {
        texture image: texture2d
        // Exact rounded-rect coverage: `Sdf2d::box` doubles the radius and leaves square
        // images nearly transparent.
        fn pixel(self) -> vec4 {
            let size = self.rect_size;
            let p = self.pos * size - size * 0.5;
            let r = min(self.radius, min(size.x, size.y) * 0.5);
            let q = abs(p) - size * 0.5 + vec2(r, r);
            let d = length(max(q, vec2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
            let cover = clamp(0.5 - d, 0.0, 1.0) * self.opacity;
            let color = sample2d(self.image, self.pos);
            return vec4(color.rgb * color.a, color.a) * cover;
        }
    }

    pub FilesKit = {{Kit}} {
        painter: <PaseoPainter> {}
        image: <DrawFilesImage> {}
        text_bold: { text_style: <THEME_FONT_BOLD> { line_spacing: 1.2 } }
        text_italic: { text_style: <THEME_FONT_ITALIC> { line_spacing: 1.2 } }
        text_bold_italic: { text_style: <THEME_FONT_BOLD_ITALIC> { line_spacing: 1.2 } }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawFilesImage {
    #[deref]
    draw_super: DrawQuad,
    #[live(1.0)]
    pub opacity: f32,
    #[live(0.0)]
    pub radius: f32,
}

impl DrawFilesImage {
    /// A texture in `rect` in a draw call of its own. The texture is bound before the call is
    /// opened: Makepad copies the slots into a new call, and a call with an empty slot accepts
    /// any later texture, so binding afterwards draws the next textures black.
    pub fn draw_texture(&mut self, cx: &mut Cx2d, rect: Rect, texture: &Texture, radius: f64, opacity: f32) {
        self.draw_vars.set_texture(0, texture);
        self.new_draw_call(cx);
        self.radius = radius as f32;
        self.opacity = opacity;
        self.draw_abs(cx, rect);
    }
}

/// Fonts of rich spans (Markdown and source text).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum RichFont {
    Regular,
    Medium,
    Bold,
    Italic,
    BoldItalic,
    Mono,
}

/// One styled stretch (in bytes) of a rich text.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RichSpan {
    pub len: usize,
    pub font: RichFont,
    pub size: f64,
    pub color: Vec4,
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct Kit {
    #[live]
    pub painter: Painter,
    #[live]
    pub image: DrawFilesImage,
    #[live]
    pub text_bold: DrawText,
    #[live]
    pub text_italic: DrawText,
    #[live]
    pub text_bold_italic: DrawText,
}

/// The font family a DrawText's style resolved to. Makepad 1.0.0 keeps `FontFamily::id`
/// private while `Fonts::get_or_layout` needs it for multi-span layout; `FontFamily` is one
/// `LiveId` (u64) field, which the size check pins for the pinned version (same technique as the
/// timeline canvas).
fn family_id(dt: &DrawText) -> FontFamilyId {
    const _: () = assert!(std::mem::size_of::<DrawTextFontFamily>() == std::mem::size_of::<u64>());
    // SAFETY: `FontFamily` (makepad-draw 1.0.0 shader/draw_text.rs) is
    // `struct FontFamily { #[rust] id: LiveId }` with `LiveId(pub u64)`.
    let raw: u64 = unsafe { std::mem::transmute_copy(&dt.text_style.font_family) };
    raw.into()
}

fn text_color(color: Vec4) -> TextColor {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    TextColor { r: byte(color.x), g: byte(color.y), b: byte(color.z), a: byte(color.w) }
}

impl Kit {
    fn rich_drawer(&mut self, font: RichFont) -> &mut DrawText {
        match font {
            RichFont::Regular => &mut self.painter.text_regular,
            RichFont::Medium => &mut self.painter.text_medium,
            RichFont::Bold => &mut self.text_bold,
            RichFont::Italic => &mut self.text_italic,
            RichFont::BoldItalic => &mut self.text_bold_italic,
            RichFont::Mono => &mut self.painter.text_mono,
        }
    }

    /// One laid-out paragraph with styled spans; `line_height` is the row height ratio relative
    /// to the font's natural height (1.0 keeps it).
    pub fn layout_rich(
        &mut self,
        cx: &mut Cx,
        text: &str,
        spans: &[RichSpan],
        max_width: Option<f64>,
        line_spacing: f64,
    ) -> Rc<LaidoutText> {
        CxDraw::lazy_construct_fonts(cx);
        let layout_spans: Vec<Span> = spans
            .iter()
            .map(|span| Span {
                style: Style {
                    font_family_id: family_id(self.rich_drawer(span.font)),
                    font_size_in_pts: Typography::pt(span.size),
                    color: Some(text_color(span.color)),
                },
                len: span.len,
            })
            .collect();
        let fonts = cx.get_global::<Rc<RefCell<Fonts>>>().clone();
        let mut fonts = fonts.borrow_mut();
        fonts.get_or_layout(BorrowedLayoutParams {
            text,
            spans: &layout_spans,
            options: LayoutOptions {
                first_row_indent_in_lpxs: 0.0,
                first_row_min_line_spacing_below_in_lpxs: 0.0,
                max_width_in_lpxs: max_width.map(|w| w.max(1.0) as f32),
                wrap: max_width.is_some(),
                align: 0.0,
                line_spacing_scale: line_spacing.max(0.5) as f32,
            },
        })
    }

    /// Draws a rich layout at `pos`; glyph colours come from its spans.
    pub fn draw_rich(&mut self, cx: &mut Cx2d, pos: DVec2, laidout: &LaidoutText) {
        let size = laidout_size(laidout);
        let dt = &mut self.painter.text_regular;
        dt.color = vec4(1.0, 1.0, 1.0, 1.0);
        dt.draw_walk_laidout(
            cx,
            Walk {
                abs_pos: Some(pos),
                margin: Margin::default(),
                width: Size::Fixed(size.x.max(1.0)),
                height: Size::Fixed(size.y.max(1.0)),
            },
            laidout,
        );
    }

    /// A plain one-style layout (no wrap unless `max_width`).
    pub fn layout(&mut self, cx: &mut Cx, spec: TextSpec, text: &str, max_width: Option<f64>) -> Rc<LaidoutText> {
        self.painter.layout(cx, spec, text, max_width)
    }

    pub fn draw_laidout(&mut self, cx: &mut Cx2d, spec: TextSpec, pos: DVec2, laidout: &LaidoutText) {
        let size = laidout_size(laidout);
        let dt = match spec.font {
            Font::Regular => &mut self.painter.text_regular,
            Font::Medium => &mut self.painter.text_medium,
            Font::Mono => &mut self.painter.text_mono,
        };
        dt.text_style.font_size = Typography::pt(spec.size);
        dt.color = spec.color;
        dt.draw_walk_laidout(
            cx,
            Walk {
                abs_pos: Some(pos),
                margin: Margin::default(),
                width: Size::Fixed(size.x.max(1.0)),
                height: Size::Fixed(size.y.max(1.0)),
            },
            laidout,
        );
    }

    /// A texture in `rect` (new draw call: each texture needs its own).
    pub fn texture(&mut self, cx: &mut Cx2d, rect: Rect, texture: &Texture, radius: f64, opacity: f32) {
        self.image.draw_texture(cx, rect, texture, radius, opacity);
    }

    // Painter shortcuts.
    pub fn fill(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4, radius: f64) {
        self.painter.fill(cx, rect, color, radius);
    }

    pub fn outline(&mut self, cx: &mut Cx2d, rect: Rect, fill: Vec4, border: Vec4, width: f64, radius: f64) {
        self.painter.outline(cx, rect, fill, border, width, radius);
    }

    pub fn hline(&mut self, cx: &mut Cx2d, x: f64, y: f64, width: f64, color: Vec4) {
        self.painter.hline(cx, x, y, width, color);
    }

    pub fn vline(&mut self, cx: &mut Cx2d, x: f64, y: f64, height: f64, color: Vec4) {
        self.painter.vline(cx, x, y, height, color);
    }

    pub fn icon(&mut self, cx: &mut Cx2d, rect: Rect, name: &str, size: f64, color: Vec4) {
        self.painter.icon(cx, rect, name, size, color);
    }

    pub fn text_in(&mut self, cx: &mut Cx2d, rect: Rect, spec: TextSpec, text: &str, align_x: f64) -> Rect {
        self.painter.text_in(cx, rect, spec, text, align_x)
    }

    pub fn measure(&mut self, cx: &mut Cx, spec: TextSpec, text: &str) -> DVec2 {
        self.painter.measure(cx, spec, text)
    }

    pub fn paragraph(&mut self, cx: &mut Cx2d, pos: DVec2, max_width: f64, spec: TextSpec, text: &str) -> DVec2 {
        self.painter.paragraph(cx, pos, max_width, spec, text)
    }

    pub fn paragraph_height(&mut self, cx: &mut Cx, max_width: f64, spec: TextSpec, text: &str) -> f64 {
        self.painter.paragraph_height(cx, max_width, spec, text)
    }

    pub fn dot(&mut self, cx: &mut Cx2d, center: DVec2, diameter: f64, color: Vec4) {
        self.painter.dot(cx, center, diameter, color);
    }

    /// The 14px spinner arc (Flutter `CircularProgressIndicator(strokeWidth: 1.5)`), rotated by
    /// `phase` turns.
    pub fn spinner(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4, phase: f64) {
        self.painter.ring(cx, rect, color, 1.5, 0.7, phase);
    }
}

pub fn laidout_size(laidout: &LaidoutText) -> DVec2 {
    dvec2(laidout.size_in_lpxs.width as f64, laidout.size_in_lpxs.height as f64)
}

pub fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
    Rect { pos: dvec2(x, y), size: dvec2(w.max(0.0), h.max(0.0)) }
}

pub fn spec(font: Font, size: f64, color: Vec4) -> TextSpec {
    TextSpec::new(font, size, color)
}

/// `#rrggbb` (syntax theme colours).
pub fn hex_color(hex: &str) -> Vec4 {
    let digits = hex.trim_start_matches('#');
    let value = u32::from_str_radix(digits, 16).unwrap_or(0xffffff);
    let (r, g, b, a) = match digits.len() {
        8 => (value >> 24, (value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff),
        _ => ((value >> 16) & 0xff, (value >> 8) & 0xff, value & 0xff, 0xff),
    };
    vec4(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a as f32 / 255.0)
}

/// Pointer coordinates of `hit` moved by `-shift`. A parent turtle's alignment (a centred
/// modal, an aligned view) moves a widget's instances after it drew, so the hit rects it
/// recorded while drawing lag its real position by `shift` (its area's current origin minus
/// the origin it drew at).
pub fn unshift_hit(hit: Hit, shift: DVec2) -> Hit {
    if shift == DVec2::default() {
        return hit;
    }
    match hit {
        Hit::FingerDown(mut e) => {
            e.abs -= shift;
            Hit::FingerDown(e)
        }
        Hit::FingerMove(mut e) => {
            e.abs -= shift;
            e.abs_start -= shift;
            Hit::FingerMove(e)
        }
        Hit::FingerUp(mut e) => {
            e.abs -= shift;
            e.abs_start -= shift;
            Hit::FingerUp(e)
        }
        Hit::FingerHoverIn(mut e) => {
            e.abs -= shift;
            Hit::FingerHoverIn(e)
        }
        Hit::FingerHoverOver(mut e) => {
            e.abs -= shift;
            Hit::FingerHoverOver(e)
        }
        Hit::FingerHoverOut(mut e) => {
            e.abs -= shift;
            Hit::FingerHoverOut(e)
        }
        Hit::FingerScroll(mut e) => {
            e.abs -= shift;
            Hit::FingerScroll(e)
        }
        Hit::FingerLongPress(mut e) => {
            e.abs -= shift;
            Hit::FingerLongPress(e)
        }
        other => other,
    }
}

/// `rect` in drawn coordinates moved to where it is on screen (see [`unshift_hit`]).
pub fn shifted(rect: Rect, shift: DVec2) -> Rect {
    Rect { pos: rect.pos + shift, size: rect.size }
}
