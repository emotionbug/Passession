//! Paint-only timeline effects. ChatRow retains these in siblings of its text list,
//! so a clock wakeup composes the row without measuring or painting its Markdown.

use std::rc::Rc;
use std::time::{Duration, Instant};

use makepad_widgets::makepad_draw::text::layouter::LaidoutText;
use makepad_widgets::*;

use crate::common::clock;
use crate::common::layer::place_viewport;
use crate::common::transition::Loop;

use super::canvas::{ChatKit, Font, rect_xywh};
use super::format;
use super::paint::MOBILE;

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
        shimmer_list: Option<&mut Option<DrawList2d>>,
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
                *area = kit.thinking_dots.paint(cx, rect, *color);
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
                label,
                label_pos,
                secondary,
                secondary_pos,
                ..
            } => {
                // The parent clock anchor stays visible even when the moving
                // clip on the child list is entirely outside the label.
                let list = shimmer_list
                    .expect("shimmer has a retained indicator layer")
                    .get_or_insert_with(|| DrawList2d::new(cx));
                list.begin_always(cx);
                kit.draw_laidout(cx, Font::Regular, *color, *label_pos, label);
                if let Some(secondary) = secondary {
                    kit.draw_laidout(cx, Font::Regular, *color, *secondary_pos, secondary);
                }
                list.end(cx);
                place_viewport(
                    cx,
                    list.id(),
                    shimmer_clip(rect, clock::now_ms()),
                    DVec2::default(),
                );
                cx.add_rect_area(area, rect);
                clock::sample(cx, *area, Loop::Shimmer.resolve().cadence, sample_shimmer);
            }
        }
        if let Some(when) = until {
            let bounds = clip.map_or(paint_bounds, |clip| {
                paint_bounds.clip((clip.pos, clip.pos + clip.size))
            });
            cx.add_rect_area(area, bounds);
            clock::register_until(cx, *area, when);
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
    shimmer: Option<DrawList2d>,
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
                shimmer: None,
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
                layer.paint.paint(
                    cx,
                    kit,
                    layer.clip,
                    &mut layer.area,
                    Some(&mut layer.shimmer),
                );
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

fn shimmer_clip(rect: Rect, now_ms: u128) -> Rect {
    let width = rect.size.x;
    let (peak, travel) = if MOBILE {
        let peak = (width * 0.28).clamp(32.0, 120.0);
        (peak, width + 2.0 * peak)
    } else {
        let peak = (width * 0.22).clamp(42.0, 120.0);
        (peak, width + peak)
    };
    let phase = Loop::Shimmer.resolve().phase(now_ms as f64);
    let left = rect.pos.x - peak + travel * phase;
    crate::common::paint::intersect(rect_xywh(left, rect.pos.y, peak, rect.size.y), rect)
}

fn sample_shimmer(cx: &mut Cx, area: Area, now_ms: u128) -> bool {
    let Some(owner) = area.draw_list_id() else {
        return false;
    };
    let items = &cx.draw_lists[owner].draw_items;
    if items.len() == 0 {
        return false;
    }
    let Some(list) = items[0].sub_list() else {
        return false;
    };
    place_viewport(
        cx,
        list,
        shimmer_clip(area.rect(cx), now_ms),
        DVec2::default(),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeline_indicators_keep_full_dots_bounds_and_move_only_the_shimmer_clip() {
        use super::super::canvas::DrawThinkingDots;
        use makepad_widgets::makepad_draw::CxDraw;

        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        crate::features::files::live_design(&mut cx);
        super::super::canvas::live_design(&mut cx);
        DrawThinkingDots::register_main_module(&mut cx);
        cx.live_expand();
        let mut dots = DrawThinkingDots::new_local(&mut cx);
        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(200.0, 100.0));
        let mut row = DrawList2d::new(&mut cx);
        let mut effect = DrawList2d::new(&mut cx);
        let mut shimmer = DrawList2d::new(&mut cx);
        let bounds = rect_xywh(15.0, 20.0, 100.0, 20.0);
        let dots_bounds = rect_xywh(0.0, 20.0, 14.0, 14.0);
        let mut anchor = Area::Empty;
        let dots_area;
        {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(&mut cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            row.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            dots_area = dots.paint(&mut cx, dots_bounds, vec4(1.0, 1.0, 1.0, 1.0));
            effect.begin_always(&mut cx);
            shimmer.begin_always(&mut cx);
            shimmer.end(&mut cx);
            cx.add_rect_area(&mut anchor, bounds);
            effect.end(&mut cx);
            cx.end_pass_sized_turtle();
            row.end(&mut cx);
            cx.end_pass(&pass);
        }
        assert_eq!(dots_area.clipped_rect(&cx), dots_bounds);
        clock::cancel(&mut cx, dots_area);

        let period = Loop::Shimmer.resolve().period.as_millis();
        let id = shimmer.id();
        cx.new_draw_event = DrawEvent::default();
        assert!(sample_shimmer(&mut cx, anchor, 0));
        let start = cx.draw_lists[id].draw_list_uniforms.view_clip;
        assert_eq!(start.x, start.z);
        assert!(sample_shimmer(&mut cx, anchor, period / 2));
        let middle = cx.draw_lists[id].draw_list_uniforms.view_clip;
        assert!(start.x < middle.x && middle.x < middle.z);
        assert!(middle.z <= (bounds.pos.x + bounds.size.x) as f32);
        assert!(sample_shimmer(&mut cx, anchor, period));
        assert_eq!(cx.draw_lists[id].draw_list_uniforms.view_clip, start);
        assert_eq!(anchor.clipped_rect(&cx), bounds);
        assert!(!cx.new_draw_event.will_redraw());
    }

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
