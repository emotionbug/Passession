//! Port of `chrome/chat_outline.dart` (chat-outline/rail.web.tsx): a 36px
//! rail 8px from the transcript's left edge spanning its middle 80%, one tick
//! per user prompt; dock-style magnification after a hover dwell with a
//! 260x48 preview; hidden for fewer than two prompts, on Android/iOS and in
//! panels narrower than 918px. Prompts come from
//! `agent.timeline.list_prompts.request`.

use super::canvas::*;
use super::json::*;
use super::paint::{Draw, MOBILE};
use crate::common::clock;
use crate::common::transition::{Intent, Spec, Transition, tokens};
use makepad_widgets::makepad_draw::makepad_platform::makepad_shader_compiler::ShaderTy;
use makepad_widgets::*;
use serde_json::Value;
use std::time::Instant;

const RAIL_WIDTH: f64 = 36.0;
const SLOT_HEIGHT: f64 = 8.0;
pub const MIN_PANEL_WIDTH: f64 = 918.0;
const RESTING_H: f64 = 2.0;
const MAGNIFIED_H: f64 = 4.0;
const RESTING_W: f64 = 10.0;
const ACTIVE_W: f64 = 18.0;
const MAGNIFIED_W: f64 = 26.0;
const PREVIEW_W: f64 = 260.0;
const PREVIEW_H: f64 = 48.0;
const PREVIEW_GAP: f64 = 4.0;
const MAGNIFY_RADIUS: i64 = 3;

/// promptTickMagnification: a raised cosine, 1 under the pointer easing to 0
/// three slots away.
pub fn magnification(slot_distance: i64) -> f64 {
    let distance = slot_distance.abs();
    if distance >= MAGNIFY_RADIUS {
        return 0.0;
    }
    (1.0 + (std::f64::consts::PI * distance as f64 / MAGNIFY_RADIUS as f64).cos()) / 2.0
}

/// resolveActivePromptSeq: the last indexed prompt at or before the anchor.
pub fn active_prompt_seq(prompts: &[Value], anchor_seq: Option<i64>) -> Option<i64> {
    let anchor = anchor_seq?;
    let mut active = None;
    for prompt in prompts {
        let seq = int_of(get(prompt, "seq"));
        if seq > anchor {
            break;
        }
        active = Some(seq);
    }
    active
}

pub fn supported() -> bool {
    !MOBILE
}

/// Hover-intent state of the rail.
#[derive(Default)]
pub struct Rail {
    pub prompts: Vec<Value>,
    /// The prompt list request in flight (agent id it was asked for).
    pub loading: bool,
    pub loaded_for: Option<(String, String)>,
    hovered: Option<usize>,
    candidate: Option<usize>,
    intent_deadline: Option<Instant>,
    motion_anchor: Option<DVec2>,
    ticks: Vec<Tick>,
    moving: Vec<usize>,
    area: Area,
    rail: Option<Rect>,
    slot: f64,
}

#[derive(Default)]
struct Tick {
    width: Transition,
    height: Transition,
    target: Option<DVec2>,
    area: Area,
    listed: bool,
}

impl Tick {
    fn retarget(&mut self, cx: &mut Cx, target: DVec2, spec: Spec) {
        match self.target.replace(target) {
            None => {
                self.width.snap(target.x);
                self.height.snap(target.y);
            }
            Some(previous) => {
                if previous.x != target.x {
                    self.width.retarget(cx, target.x, spec);
                }
                if previous.y != target.y {
                    self.height.retarget(cx, target.y, spec);
                }
            }
        }
    }

    fn is_moving(&self) -> bool {
        self.width.is_moving() || self.height.is_moving()
    }

    fn rect(&self, rail: Rect, slot: f64, index: usize) -> Rect {
        let h = self.height.value();
        rect_xywh(
            rail.pos.x + 4.0,
            rail.pos.y + slot * index as f64 + (slot - h) / 2.0,
            self.width.value(),
            h,
        )
    }

    fn advance(&mut self, cx: &mut Cx, event: &Event, rail: Rect, slot: f64, index: usize) {
        let width = self.width.advance(cx, event);
        let height = self.height.advance(cx, event);
        if !width && !height {
            return;
        }
        let rect = self.rect(rail, slot, index);
        for (field, value) in [
            (live_id!(rect_pos), rect.pos),
            (live_id!(rect_size), rect.size),
        ] {
            if let Some(write) = self
                .area
                .get_write_ref(cx, field, ShaderTy::Vec2, "outline tick")
            {
                write.buffer[..2].copy_from_slice(&[value.x as f32, value.y as f32]);
            }
        }
    }
}

pub enum RailEvent {
    Jump(i64),
}

impl Rail {
    pub fn visible(&self, width: f64) -> bool {
        self.prompts.len() >= 2 && supported() && width >= MIN_PANEL_WIDTH
    }

    fn schedule_activation(&mut self, cx: &mut Cx, now: Instant) {
        clock::cancel(cx, self.area);
        let deadline = now + tokens::HOVER_DWELL;
        self.intent_deadline = Some(deadline);
        clock::register_until(cx, self.area, deadline);
    }

    fn point_at(&mut self, cx: &mut Cx, index: usize, now: Instant) {
        self.candidate = Some(index);
        if self.hovered.is_some() {
            self.hovered = Some(index);
        } else if self.intent_deadline.is_none() {
            self.schedule_activation(cx, now);
        }
    }

    /// hover-intent.ts: a pointer crossing the rail horizontally restarts the delay.
    fn moved(&mut self, cx: &mut Cx, point: DVec2, now: Instant) {
        let Some(anchor) = self.motion_anchor else {
            return;
        };
        if self.hovered.is_some() {
            return;
        }
        let (dx, dy) = (point.x - anchor.x, point.y - anchor.y);
        if dy.abs() >= dx.abs() {
            self.motion_anchor = Some(point);
            return;
        }
        if dx.abs() < 4.0 {
            return;
        }
        self.motion_anchor = Some(point);
        self.schedule_activation(cx, now);
    }

    fn leave(&mut self, cx: &mut Cx) {
        if self.intent_deadline.take().is_some() {
            clock::cancel(cx, self.area);
        }
        self.candidate = None;
        self.motion_anchor = None;
        self.hovered = None;
    }

    /// Pointer hover over the panel; returns true when a redraw is needed.
    pub fn hover(&mut self, cx: &mut Cx, point: Option<DVec2>) -> bool {
        let Some(rail) = self.rail.filter(|_| self.area.is_attached(cx)) else {
            return false;
        };
        let now = Instant::now();
        match point.filter(|p| rail.contains(*p)) {
            Some(p) => {
                if self.motion_anchor.is_none() {
                    self.motion_anchor = Some(p);
                } else {
                    self.moved(cx, p, now);
                }
                let index = (((p.y - rail.pos.y) / self.slot.max(0.001)) as usize)
                    .min(self.prompts.len().saturating_sub(1));
                let before = self.hovered;
                self.point_at(cx, index, now);
                before != self.hovered
            }
            None => {
                let had = self.hovered.is_some();
                self.leave(cx);
                had
            }
        }
    }

    pub fn tap(&self, point: DVec2) -> Option<RailEvent> {
        let rail = self.rail?;
        if !rail.contains(point) {
            return None;
        }
        let index = (((point.y - rail.pos.y) / self.slot.max(0.001)) as usize)
            .min(self.prompts.len().saturating_sub(1));
        self.prompts
            .get(index)
            .map(|p| RailEvent::Jump(int_of(get(p, "seq"))))
    }

    pub fn contains(&self, point: DVec2) -> bool {
        self.rail.is_some_and(|r| r.contains(point))
    }

    pub fn hide(&mut self, cx: &mut Cx) {
        self.leave(cx);
        self.area = Area::Empty;
        self.rail = None;
        self.moving.clear();
        self.ticks.clear();
    }

    pub fn advance(&mut self, cx: &mut Cx, event: &Event) {
        if self.moving.is_empty() || !matches!(event, Event::NextFrame(_)) {
            return;
        }
        let Some(rail) = self.rail.filter(|_| self.area.is_attached(cx)) else {
            self.hide(cx);
            return;
        };
        self.moving.retain(|&index| {
            let tick = &mut self.ticks[index];
            tick.advance(cx, event, rail, self.slot, index);
            tick.listed = tick.is_moving();
            tick.listed
        });
    }

    /// Paints the rail inside `area` (the transcript rect); `active_seq` is the
    /// prompt being read. Intent waits a single deadline; motion ticks only while changing.
    pub fn draw(&mut self, cx: &mut Cx2d, d: &mut Draw, area: Rect, active_seq: Option<i64>) {
        if !self.visible(area.size.x) {
            self.hide(cx);
            return;
        }
        let p = d.look.palette;
        self.area = d.area;
        if let Some(deadline) = self.intent_deadline {
            if Instant::now() >= deadline {
                self.intent_deadline = None;
                self.hovered = self.candidate;
            } else {
                clock::register_until(cx, self.area, deadline);
            }
        }
        let top = area.size.y * 0.1;
        let height = area.size.y * 0.8;
        let count = self.prompts.len();
        let slot = SLOT_HEIGHT.min(height / count as f64);
        let rail_h = slot * count as f64;
        let rail_top = area.pos.y + top + (height - rail_h) / 2.0;
        let rail = rect_xywh(area.pos.x + 8.0, rail_top, RAIL_WIDTH, rail_h);
        self.rail = Some(rail);
        self.slot = slot;
        self.ticks.resize_with(count, Tick::default);
        self.moving.retain(|&index| index < count);
        let attention = self.hovered.filter(|&index| index < count);
        let spec = Intent::Hover.resolve().spec;
        for (i, prompt) in self.prompts.iter().enumerate() {
            let active = Some(int_of(get(prompt, "seq"))) == active_seq;
            let m = attention
                .map(|a| magnification(i as i64 - a as i64))
                .unwrap_or(0.0);
            let resting = if active { ACTIVE_W } else { RESTING_W };
            let tick = &mut self.ticks[i];
            tick.retarget(
                cx,
                dvec2(
                    resting + m * (MAGNIFIED_W - resting),
                    RESTING_H + m * (MAGNIFIED_H - RESTING_H),
                ),
                spec,
            );
            if !tick.listed && tick.is_moving() {
                tick.listed = true;
                self.moving.push(i);
            }
            let color = if attention == Some(i) {
                p.foreground
            } else if active {
                p.extra_muted
            } else {
                p.border_accent
            };
            d.kit
                .rect
                .paint(cx, tick.rect(rail, slot, i), color, MAGNIFIED_H / 2.0);
            tick.area = d.kit.rect.area();
        }
        d.canvas.add_hit(rail, HitTarget::Chrome("outline".into()));
        if let Some(index) = attention {
            let text = str_of(get(&self.prompts[index], "preview"));
            let x = area.pos.x + 8.0 + RAIL_WIDTH + PREVIEW_GAP;
            let y = rail_top + slot * index as f64 + slot / 2.0 - PREVIEW_H / 2.0;
            let r = rect_xywh(x, y, PREVIEW_W, PREVIEW_H);
            let shadow = if p.is_dark {
                rgba(0x33000000)
            } else {
                rgba(0x0a000000)
            };
            d.kit.rect(cx, r.translate(dvec2(0.0, 4.0)), shadow, 8.0);
            d.kit.bordered(cx, r, p.surface2, 8.0, p.border, 1.0);
            let size = d.look.ui(12.0);
            cx.begin_turtle(
                Walk {
                    abs_pos: Some(r.pos),
                    margin: Margin::default(),
                    width: Size::Fixed(PREVIEW_W),
                    height: Size::Fixed(PREVIEW_H),
                },
                Layout::default(),
            );
            let laid =
                d.kit
                    .layout_clamped(cx, Font::Regular, size, 1.3, text, PREVIEW_W - 24.0, 2);
            let th = laidout_size(&laid).y;
            d.kit.draw_laidout(
                cx,
                Font::Regular,
                p.foreground,
                dvec2(x + 12.0, y + (PREVIEW_H - th) / 2.0),
                &laid,
            );
            cx.end_turtle();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::transition;
    use crate::theme::DrawBox;
    use makepad_widgets::makepad_draw::CxDraw;

    #[test]
    fn outline_hover_updates_tick_instances_without_redrawing_the_preview() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        makepad_widgets::makepad_draw::live_design(&mut cx);
        crate::theme::live_design(&mut cx);
        DrawBox::register_main_module(&mut cx);
        cx.live_expand();
        let mut tick = DrawBox::new_local(&mut cx);
        let pass = Pass::new(&mut cx);
        pass.set_size(&mut cx, dvec2(200.0, 100.0));
        let mut list = DrawList2d::new(&mut cx);
        let mut rail = Rail {
            rail: Some(rect_xywh(8.0, 20.0, 36.0, 8.0)),
            slot: SLOT_HEIGHT,
            ticks: vec![Tick::default()],
            ..Default::default()
        };
        let rest = dvec2(RESTING_W, RESTING_H);
        let target = dvec2(MAGNIFIED_W, MAGNIFIED_H);
        let spec = Intent::Hover.resolve().spec;
        rail.ticks[0].retarget(&mut cx, rest, spec);
        {
            let event = DrawEvent::default();
            let mut draw = CxDraw::new(&mut cx, &event);
            draw.begin_pass(&pass, Some(1.0));
            list.begin_always(&mut draw);
            let mut cx = Cx2d::new(&mut draw);
            cx.begin_pass_sized_turtle(Layout::flow_overlay());
            cx.add_rect_area(&mut rail.area, rail.rail.unwrap());
            tick.paint(
                &mut cx,
                rail.ticks[0].rect(rail.rail.unwrap(), rail.slot, 0),
                vec4(1.0, 1.0, 1.0, 1.0),
                MAGNIFIED_H / 2.0,
            );
            rail.ticks[0].area = tick.area();
            cx.end_pass_sized_turtle();
            list.end(&mut cx);
            cx.end_pass(&pass);
        }

        rail.ticks[0].retarget(&mut cx, target, spec);
        rail.ticks[0].listed = true;
        rail.moving.push(0);
        cx.new_draw_event = DrawEvent::default();
        for (time, expected) in [(1.0, Some(rest)), (1.04, None), (2.0, Some(target))] {
            let event = Event::NextFrame(NextFrameEvent {
                frame: 0,
                time,
                set: [rail.ticks[0].width.scheduled_frame().unwrap()]
                    .into_iter()
                    .collect(),
            });
            transition::begin_event(&event);
            rail.advance(&mut cx, &event);
            let actual = tick.area().rect(&cx).size;
            if let Some(expected) = expected {
                assert_eq!(actual, expected);
            } else {
                assert!((RESTING_W..MAGNIFIED_W).contains(&actual.x));
                assert!((RESTING_H..MAGNIFIED_H).contains(&actual.y));
            }
            assert!(!cx.new_draw_event.will_redraw());
        }
        assert!(rail.moving.is_empty());
        assert!(rail.ticks[0].width.scheduled_frame().is_none());
    }
}
