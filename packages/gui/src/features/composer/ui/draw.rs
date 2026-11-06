//! The composer's one custom shader: a rounded, cover- or contain-fitted image quad for
//! attachment thumbnails and the image preview. Boxes, rings, text and glyphs use the shared
//! `common::paint::Painter`.

use makepad_widgets::*;

live_design! {
    use link::shaders::*;

    pub DrawComposerImage = {{DrawComposerImage}} {
        texture image: texture2d
        fn pixel(self) -> vec4 {
            // The exact rounded-rect distance of theme.rs `DrawBox` (Sdf2d::box doubles the
            // radius and leaves square fills nearly transparent).
            let size = self.rect_size;
            let p = self.pos * size - size * 0.5;
            let r = min(self.radius, min(size.x, size.y) * 0.5);
            let q = abs(p) - size * 0.5 + vec2(r, r);
            let d = length(max(q, vec2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
            let outer = clamp(0.5 - d, 0.0, 1.0);
            let color = sample2d(self.image, self.pos * self.uv_scale + self.uv_offset);
            let a = color.a * self.opacity * outer;
            return vec4(color.rgb * a, a);
        }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawComposerImage {
    #[deref]
    pub draw_super: DrawQuad,
    #[live]
    pub radius: f32,
    #[live(1.0)]
    pub opacity: f32,
    #[live(vec2(1.0, 1.0))]
    pub uv_scale: Vec2,
    #[live]
    pub uv_offset: Vec2,
}

impl DrawComposerImage {
    /// Draw `texture` (pixel `size`) into `rect`: cover-fitted and centred, or letterboxed
    /// (`contain`) for the full preview.
    pub fn paint(&mut self, cx: &mut Cx2d, rect: Rect, texture: &Texture, size: (usize, usize), radius: f64, contain: bool) {
        self.draw_vars.set_texture(0, texture);
        let image_aspect = size.0.max(1) as f64 / size.1.max(1) as f64;
        let rect_aspect = rect.size.x.max(1.0) / rect.size.y.max(1.0);
        self.radius = radius as f32;
        self.opacity = 1.0;
        if contain {
            let mut target = rect;
            if image_aspect > rect_aspect {
                target.size.y = rect.size.x / image_aspect;
                target.pos.y += (rect.size.y - target.size.y) * 0.5;
            } else {
                target.size.x = rect.size.y * image_aspect;
                target.pos.x += (rect.size.x - target.size.x) * 0.5;
            }
            self.uv_scale = vec2(1.0, 1.0);
            self.uv_offset = vec2(0.0, 0.0);
            self.draw_abs(cx, target);
            return;
        }
        if image_aspect > rect_aspect {
            let scale = (rect_aspect / image_aspect) as f32;
            self.uv_scale = vec2(scale, 1.0);
            self.uv_offset = vec2((1.0 - scale) * 0.5, 0.0);
        } else {
            let scale = (image_aspect / rect_aspect) as f32;
            self.uv_scale = vec2(1.0, scale);
            self.uv_offset = vec2(0.0, (1.0 - scale) * 0.5);
        }
        self.draw_abs(cx, rect);
    }
}
