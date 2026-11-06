//! Port of `chrome/chat_outline.dart` (chat-outline/rail.web.tsx): a 36px
//! rail 8px from the transcript's left edge spanning its middle 80%, one tick
//! per user prompt; dock-style magnification after a 150ms hover intent with a
//! 260x48 preview; hidden for fewer than two prompts, on Android/iOS and in
//! panels narrower than 918px. Prompts come from
//! `agent.timeline.list_prompts.request`.

use super::canvas::*;
use super::json::*;
use super::paint::{Draw, MOBILE};
use makepad_widgets::*;
use serde_json::Value;

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
/// AnimatedContainer 140ms easeOut.
const EASE_SECS: f64 = 0.14;

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
    active: bool,
    intent_deadline: Option<f64>,
    motion_anchor: Option<DVec2>,
    /// Per tick: (width, height) as currently animated, and the last frame time.
    anim: Vec<(f64, f64)>,
    anim_at: f64,
    rail: Option<Rect>,
    slot: f64,
}

pub enum RailEvent {
    Jump(i64),
}

impl Rail {
    pub fn visible(&self, width: f64) -> bool {
        self.prompts.len() >= 2 && supported() && width >= MIN_PANEL_WIDTH
    }

    fn schedule_activation(&mut self, now: f64) {
        if self.candidate.is_some() {
            self.intent_deadline = Some(now + 0.15);
        }
    }

    fn point_at(&mut self, index: usize, now: f64) {
        self.candidate = Some(index);
        if self.active {
            self.hovered = Some(index);
            return;
        }
        if self.intent_deadline.is_none() {
            self.schedule_activation(now);
        }
    }

    /// hover-intent.ts: a pointer crossing the rail horizontally restarts the delay.
    fn moved(&mut self, point: DVec2, now: f64) {
        let Some(anchor) = self.motion_anchor else { return };
        if self.active {
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
        self.schedule_activation(now);
    }

    pub fn leave(&mut self) {
        self.intent_deadline = None;
        self.candidate = None;
        self.motion_anchor = None;
        self.active = false;
        self.hovered = None;
    }

    /// Pointer hover over the panel; returns true when a redraw is needed.
    pub fn hover(&mut self, point: Option<DVec2>, now: f64) -> bool {
        let Some(rail) = self.rail else { return false };
        match point.filter(|p| rail.contains(*p)) {
            Some(p) => {
                if self.motion_anchor.is_none() {
                    self.motion_anchor = Some(p);
                } else {
                    self.moved(p, now);
                }
                let index = (((p.y - rail.pos.y) / self.slot.max(0.001)) as usize).min(self.prompts.len().saturating_sub(1));
                let before = self.hovered;
                self.point_at(index, now);
                before != self.hovered || self.intent_deadline.is_some()
            }
            None => {
                let had = self.hovered.is_some() || self.candidate.is_some();
                if had || self.motion_anchor.is_some() {
                    self.leave();
                }
                had
            }
        }
    }

    pub fn tap(&self, point: DVec2) -> Option<RailEvent> {
        let rail = self.rail?;
        if !rail.contains(point) {
            return None;
        }
        let index = (((point.y - rail.pos.y) / self.slot.max(0.001)) as usize).min(self.prompts.len().saturating_sub(1));
        self.prompts.get(index).map(|p| RailEvent::Jump(int_of(get(p, "seq"))))
    }

    pub fn contains(&self, point: DVec2) -> bool {
        self.rail.is_some_and(|r| r.contains(point))
    }

    /// Paints the rail inside `area` (the transcript rect); `active_seq` is the
    /// prompt being read. Returns true while animating.
    pub fn draw(&mut self, cx: &mut Cx2d, d: &mut Draw, area: Rect, active_seq: Option<i64>) -> bool {
        if !self.visible(area.size.x) {
            self.rail = None;
            return false;
        }
        let p = d.look.palette;
        let now = d.now;
        if let Some(deadline) = self.intent_deadline
            && now >= deadline
        {
            self.intent_deadline = None;
            self.active = true;
            if self.candidate.is_some() {
                self.hovered = self.candidate;
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
        let dt = if self.anim_at > 0.0 { (now - self.anim_at).max(0.0) } else { EASE_SECS };
        self.anim_at = now;
        self.anim.resize(count, (RESTING_W, RESTING_H));
        let mut animating = self.intent_deadline.is_some();
        let attention = self.hovered;
        for (i, prompt) in self.prompts.iter().enumerate() {
            let active = Some(int_of(get(prompt, "seq"))) == active_seq;
            let m = attention.map(|a| magnification(i as i64 - a as i64)).unwrap_or(0.0);
            let resting = if active { ACTIVE_W } else { RESTING_W };
            let target = (resting + m * (MAGNIFIED_W - resting), RESTING_H + m * (MAGNIFIED_H - RESTING_H));
            let current = &mut self.anim[i];
            // easeOut approach: close the remaining gap proportionally to elapsed time.
            let k = (dt / EASE_SECS).clamp(0.0, 1.0);
            let k = 1.0 - (1.0 - k).powi(2);
            current.0 += (target.0 - current.0) * k;
            current.1 += (target.1 - current.1) * k;
            if (current.0 - target.0).abs() > 0.05 || (current.1 - target.1).abs() > 0.05 {
                animating = true;
            } else {
                *current = target;
            }
            let (w, h) = *current;
            let y = rail_top + slot * i as f64 + (slot - h) / 2.0;
            let color = if attention == Some(i) {
                p.foreground
            } else if active {
                p.extra_muted
            } else {
                p.border_accent
            };
            d.kit.rect(cx, rect_xywh(rail.pos.x + 4.0, y, w, h), color, h / 2.0);
        }
        d.canvas.add_hit(rail, HitTarget::Chrome("outline".into()));
        if let Some(index) = attention.filter(|a| *a < count) {
            let text = str_of(get(&self.prompts[index], "preview"));
            let x = area.pos.x + 8.0 + RAIL_WIDTH + PREVIEW_GAP;
            let y = rail_top + slot * index as f64 + slot / 2.0 - PREVIEW_H / 2.0;
            let r = rect_xywh(x, y, PREVIEW_W, PREVIEW_H);
            let shadow = if p.is_dark { rgba(0x33000000) } else { rgba(0x0a000000) };
            d.kit.rect(cx, r.translate(dvec2(0.0, 4.0)), shadow, 8.0);
            d.kit.bordered(cx, r, p.surface2, 8.0, p.border, 1.0);
            let size = d.look.ui(12.0);
            cx.begin_turtle(Walk { abs_pos: Some(r.pos), margin: Margin::default(), width: Size::Fixed(PREVIEW_W), height: Size::Fixed(PREVIEW_H) }, Layout::default());
            let laid = d.kit.layout_clamped(cx, Font::Regular, size, 1.3, text, PREVIEW_W - 24.0, 2);
            let th = laidout_size(&laid).y;
            d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 12.0, y + (PREVIEW_H - th) / 2.0), &laid);
            cx.end_turtle();
        }
        animating
    }
}
