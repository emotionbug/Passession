//! The native text editing surfaces of the files feature.

pub mod buffer;
pub mod field;
pub mod view;

use std::time::Instant;

use makepad_widgets::{Area, Cx, Cx2d, LiveId, Rect, Vec4, live_id};

use crate::common::{clock, paint::Painter};

/// Focus/edits start with a solid caret; clock ticks only change its GPU instance.
#[derive(Default)]
struct Blink {
    area: Area,
    start: Option<Instant>,
}

impl Blink {
    fn draw(&mut self, cx: &mut Cx2d, painter: &mut Painter, rect: Rect, color: Vec4) {
        let now = Instant::now();
        let start = *self.start.get_or_insert(now);
        let brush = &mut painter.draw_box;
        brush.opacity = clock::caret_opacity(start, now);
        brush.paint(cx, rect, color, 0.0);
        brush.opacity = 1.0;
        self.area = brush.area();
        clock::blink_caret(cx, self.area, start);
    }

    fn disarm(&mut self, cx: &mut Cx) {
        clock::stop_caret_blink(cx, std::mem::take(&mut self.area));
        self.start = None;
    }

    fn restart(&mut self, cx: &mut Cx) {
        let start = Instant::now();
        self.start = Some(start);
        if self.area.is_attached(cx) {
            clock::write_instance(cx, self.area, live_id!(opacity), 1.0);
            clock::stop_caret_blink(cx, self.area);
            clock::blink_caret(cx, self.area, start);
        }
    }
}
