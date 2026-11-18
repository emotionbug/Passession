//! Paint-only timeline effects. ChatRow retains these in siblings of its text list,
//! so a clock wakeup composes the row without measuring or painting its Markdown.

use std::rc::Rc;
use std::time::{Duration, Instant};

use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::*;

use crate::common::clock;
use crate::theme::with_alpha;

use super::canvas::{ChatKit, Font, rect_xywh};
use super::format;

pub(super) enum Indicator {
    Copy {
        rect: Rect,
        size: f64,
        color: Vec4,
        expiry: Instant,
    },
    Dots {
        rect: Rect,
        color: Vec4,
    },
    Elapsed {
        rect: Rect,
        started_ms: i64,
        size: f64,
        color: Vec4,
        note: Option<(String, f64)>,
    },
    Stamp {
        rect: Rect,
        size: f64,
        color: Vec4,
        primary: String,
        stamp: String,
        expiry: Instant,
    },
    Shimmer {
        rect: Rect,
        color: Vec4,
        duration: f64,
        peak: f64,
        travel: f64,
        label: Rc<LaidoutText>,
        label_pos: DVec2,
        secondary: Option<Rc<LaidoutText>>,
        secondary_pos: DVec2,
    },
}

impl Indicator {
    fn rect(&self) -> Rect {
        match self {
            Self::Copy { rect, .. }
            | Self::Dots { rect, .. }
            | Self::Elapsed { rect, .. }
            | Self::Stamp { rect, .. }
            | Self::Shimmer { rect, .. } => *rect,
        }
    }

    pub(super) fn paint(
        &self,
        cx: &mut Cx2d,
        kit: &mut ChatKit,
        clip: Option<Rect>,
        area: &mut Area,
    ) {
        if !area.is_empty() {
            clock::cancel(cx, *area);
        }
        let rect = self.rect();
        let clipped = clip.map_or(rect, |clip| rect.clip((clip.pos, clip.pos + clip.size)));
        let mut paint_bounds = rect;
        if clipped.size.x <= 0.0 || clipped.size.y <= 0.0 {
            return;
        }
        if let Some(clip) = clip {
            cx.begin_turtle(Walk::abs_rect(clip), Layout::default());
        }
        let now = Instant::now();
        let mut until = None;
        let mut every = None;
        match self {
            Self::Copy {
                size,
                color,
                expiry,
                ..
            } => {
                let copied = now < *expiry;
                kit.icon(
                    cx,
                    if copied { "check" } else { "copy" },
                    rect,
                    *size,
                    *color,
                );
                until = copied.then_some(*expiry);
            }
            Self::Dots { color, .. } => {
                const PERIOD: Duration = Duration::from_nanos(950_000_000 / 6);
                draw_dots(cx, kit, rect.pos, *color, clock::step(PERIOD, 6));
                every = Some(PERIOD);
            }
            Self::Elapsed {
                started_ms,
                size,
                color,
                note,
                ..
            } => {
                let elapsed = (clock::now_ms() as i64).saturating_sub(*started_ms);
                let text = format::duration(elapsed);
                let s = kit.label(cx, Font::Regular, *size, 1.3, *color, rect.pos, &text, None);
                paint_bounds.size = s;
                if let Some((text, size)) = note {
                    let dx = s.x + 12.0;
                    let note_size = kit.label(
                        cx,
                        Font::Regular,
                        *size,
                        1.3,
                        *color,
                        rect.pos + dvec2(dx, 0.0),
                        text,
                        Some((rect.size.x - dx).max(1.0)),
                    );
                    paint_bounds.size = dvec2(dx + note_size.x, s.y.max(note_size.y));
                }
                until = Some(now + duration_delay(elapsed));
            }
            Self::Stamp {
                size,
                color,
                primary,
                stamp,
                expiry,
                ..
            } => {
                let revealing = now < *expiry;
                paint_bounds.size = kit.label(
                    cx,
                    Font::Regular,
                    *size,
                    1.3,
                    *color,
                    rect.pos,
                    if revealing { stamp } else { primary },
                    Some(rect.size.x),
                );
                until = revealing.then_some(*expiry);
            }
            Self::Shimmer {
                color,
                duration,
                peak,
                travel,
                label,
                label_pos,
                secondary,
                secondary_pos,
                ..
            } => {
                let phase = (super::paint::now_seconds() % duration) / duration;
                let left = rect.pos.x - peak + travel * phase;
                let x = left.max(rect.pos.x);
                let width = (left + peak).min(rect.pos.x + rect.size.x) - x;
                if width > 0.0 {
                    let sweep = rect_xywh(x, rect.pos.y, width, rect.size.y);
                    cx.begin_turtle(Walk::abs_rect(sweep), Layout::default());
                    kit.draw_laidout(cx, Font::Regular, *color, *label_pos, label);
                    if let Some(secondary) = secondary {
                        kit.draw_laidout(cx, Font::Regular, *color, *secondary_pos, secondary);
                    }
                    cx.end_turtle();
                }
                every = Some(clock::MOTION_PERIOD);
            }
        }
        if every.is_some() || until.is_some() {
            let bounds = clip.map_or(paint_bounds, |clip| {
                paint_bounds.clip((clip.pos, clip.pos + clip.size))
            });
            cx.add_rect_area(area, bounds);
            if let Some(period) = every {
                clock::register_every(cx, *area, period);
            }
            if let Some(when) = until {
                clock::register_until(cx, *area, when);
            }
        }
        if clip.is_some() {
            cx.end_turtle();
        }
    }
}

#[derive(Default)]
pub(crate) struct Indicators {
    layers: Vec<IndicatorLayer>,
    len: usize,
}

struct IndicatorLayer {
    list: DrawList2d,
    area: Area,
    clip: Option<Rect>,
    paint: Indicator,
    dirty: bool,
}

impl Indicators {
    pub(super) fn begin(&mut self) {
        self.len = 0;
    }

    pub(super) fn push(&mut self, cx: &mut Cx2d, paint: Indicator, clip: Option<Rect>) {
        if let Some(layer) = self.layers.get_mut(self.len) {
            layer.paint = paint;
            layer.clip = clip;
            layer.dirty = true;
        } else {
            self.layers.push(IndicatorLayer {
                list: DrawList2d::new(cx),
                area: Area::Empty,
                clip,
                paint,
                dirty: true,
            });
        }
        self.len += 1;
    }

    pub(super) fn draw(&mut self, cx: &mut Cx2d, kit: &mut ChatKit) {
        for layer in self.layers.drain(self.len..) {
            clock::cancel(cx, layer.area);
        }
        for layer in &mut self.layers {
            let repaint = layer.dirty || cx.draw_event.draw_list_will_redraw(cx, layer.list.id());
            if layer.list.begin_maybe(cx, repaint).is_ok() {
                layer.paint.paint(cx, kit, layer.clip, &mut layer.area);
                layer.list.end(cx);
                layer.dirty = false;
            }
        }
    }
}

fn duration_delay(elapsed_ms: i64) -> Duration {
    let next = match elapsed_ms {
        ..0 => 1_000i64.saturating_sub(elapsed_ms),
        0..3_600_000 => 1_000 - elapsed_ms % 1_000,
        _ => 60_000 - elapsed_ms % 60_000,
    };
    Duration::from_millis(next as u64)
}

fn draw_dots(cx: &mut Cx2d, kit: &mut ChatKit, pos: DVec2, color: Vec4, step: usize) {
    const STATES: [[f32; 6]; 6] = [
        [1.0, 0.0, 0.78, 0.0, 0.56, 0.34],
        [0.78, 1.0, 0.56, 0.0, 0.34, 0.0],
        [0.56, 0.78, 0.34, 1.0, 0.0, 0.0],
        [0.34, 0.56, 0.0, 0.78, 0.0, 1.0],
        [0.0, 0.34, 0.0, 0.56, 1.0, 0.78],
        [0.0, 0.0, 1.0, 0.34, 0.78, 0.56],
    ];
    const SIZE: f64 = 14.0;
    const GAP: f64 = 2.0;
    const DOT: f64 = (SIZE - GAP * 2.0) / 3.0;
    let ox = pos.x + (SIZE - (DOT * 2.0 + GAP)) * 0.5;
    let oy = pos.y + (SIZE - (DOT * 3.0 + GAP)) * 0.5;
    for (index, alpha) in STATES[step].iter().enumerate() {
        let dx = (index % 2) as f64 * (DOT + GAP);
        let dy = (index / 2) as f64 * (DOT + GAP);
        kit.rect(
            cx,
            rect_xywh(ox + dx, oy + dy, DOT, DOT),
            with_alpha(color, color.w * alpha),
            DOT * 0.5,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_clock_elapsed_follows_visible_boundaries() {
        for (elapsed, delay) in [
            (-17, 1017),
            (0, 1000),
            (1255, 745),
            (3_599_999, 1),
            (3_600_000, 60_000),
            (3_622_001, 37_999),
        ] {
            assert_eq!(duration_delay(elapsed), Duration::from_millis(delay as u64));
            assert_ne!(format::duration(elapsed), format::duration(elapsed + delay));
            assert_eq!(
                format::duration(elapsed),
                format::duration(elapsed + delay - 1)
            );
        }
    }
}
