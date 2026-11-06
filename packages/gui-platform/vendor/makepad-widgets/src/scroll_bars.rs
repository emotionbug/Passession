use crate::{
    makepad_draw::*,
    makepad_draw::event::{TouchState, TAP_COUNT_DISTANCE},
    scroll_bar::*
};

live_design!{
    link widgets;
    use link::theme::*;
    use link::widgets::*;
    use makepad_draw::shader::std::*;
    
    pub ScrollBarsBase = {{ScrollBars}} {}
    
    pub ScrollBarsTabs = <ScrollBarsBase> {
        show_scroll_x: true,
        show_scroll_y: true,
        scroll_bar_x: <ScrollBarTabs> {}
        scroll_bar_y: <ScrollBarTabs> {}
    }
    
    pub ScrollBars = <ScrollBarsBase> {
        show_scroll_x: true,
        show_scroll_y: true,
        scroll_bar_x: <ScrollBar> {}
        scroll_bar_y: <ScrollBar> {}
    }
}

/// Shared pan and kinetic scrolling. Positive deltas move the viewport towards
/// the end of the content. Owners apply bounds and call `stop_at_edge` when hit.
/// `handle_hit` supports direct panning; call `frame` after input handling to
/// enable release inertia and retain its frame token until the matching event.
#[derive(Default)]
pub struct TouchScroll {
    motion: TouchMotion,
    velocity: TouchVelocity,
    next_frame: Option<NextFrame>,
    origin: DVec2,
}

// Makepad's 0.97 decay and 0.2px stop speed at 60Hz, expressed in seconds.
const SCROLL_DECAY: f64 = 1.8275524490825144;
const SCROLL_STOP_SPEED: f64 = 12.0;
// Keep real motion samples, rather than warming a velocity filter up from rest.
// 32 samples cover 100ms even at 240Hz without allocating during a gesture.
const VELOCITY_WINDOW: f64 = 0.1;
const RELEASE_IDLE_TIMEOUT: f64 = 0.1;

#[derive(Clone, Copy, Default)]
struct TouchSample {
    position: f64,
    time: f64,
}

#[derive(Default)]
struct TouchVelocity {
    samples: [TouchSample; 32],
    first: usize,
    len: usize,
    direction: f64,
    last_motion_time: f64,
}

impl TouchVelocity {
    fn reset(&mut self, position: f64, time: f64) {
        self.samples[0] = TouchSample { position, time };
        self.first = 0;
        self.len = 1;
        self.direction = 0.0;
        self.last_motion_time = time;
    }

    fn sample(&self, index: usize) -> TouchSample {
        self.samples[(self.first + index) % self.samples.len()]
    }

    fn clear(&mut self) {
        let last = self.sample(self.len - 1);
        self.reset(last.position, last.time);
    }

    fn push(&mut self, position: f64, time: f64) {
        let last = self.sample(self.len - 1);
        let delta = last.position - position;
        // Reversals and bounds must not retain momentum in the old direction.
        if delta * self.direction < 0.0 {
            self.clear();
        }
        if delta != 0.0 {
            self.direction = delta;
            self.last_motion_time = time;
        }
        if time <= last.time {
            // Batched/quantized timestamps replace the endpoint, retaining the
            // earlier timed sample instead of dropping this event's distance.
            let index = (self.first + self.len - 1) % self.samples.len();
            self.samples[index].position = position;
            return;
        }
        if self.len == self.samples.len() {
            self.first = (self.first + 1) % self.samples.len();
            self.len -= 1;
        }
        let index = (self.first + self.len) % self.samples.len();
        self.samples[index] = TouchSample { position, time };
        self.len += 1;
    }

    fn estimate(&self, release_time: f64) -> f64 {
        // UP is not a stationary MOVE. Normal delivery gaps must not damp a
        // flick, but a held finger must not release stale momentum.
        if self.len < 2 || release_time - self.last_motion_time >= RELEASE_IDLE_TIMEOUT {
            return 0.0;
        }
        let last = self.sample(self.len - 1);
        let cutoff = last.time - VELOCITY_WINDOW;
        let mut first = self.sample(0);
        for index in 1..self.len {
            let next = self.sample(index);
            if next.time > cutoff {
                if first.time < cutoff {
                    let fraction = (cutoff - first.time) / (next.time - first.time);
                    first.position += (next.position - first.position) * fraction;
                    first.time = cutoff;
                }
                break;
            }
            first = next;
        }
        (first.position - last.position) / (last.time - first.time)
    }
}

#[derive(Default)]
enum TouchMotion {
    #[default]
    Idle,
    Drag { start: DVec2, last: DVec2, panning: bool },
    Fling { time: Option<f64>, velocity: f64 },
}

impl TouchScroll {
    pub fn reset(&mut self) {
        self.motion = TouchMotion::Idle;
        self.next_frame = None;
    }

    pub fn begin(&mut self, abs: DVec2, time: f64, is_touch: bool) {
        self.reset();
        self.origin = abs;
        self.velocity.reset(abs.y, time);
        self.motion = TouchMotion::Drag { start: abs, last: abs, panning: !is_touch };
    }

    pub fn origin(&self) -> DVec2 {
        self.origin
    }

    pub fn is_dragging(&self) -> bool {
        matches!(self.motion, TouchMotion::Drag { .. })
    }

    pub fn is_panning(&self) -> bool {
        matches!(self.motion, TouchMotion::Drag { panning: true, .. })
    }

    pub fn is_flinging(&self) -> bool {
        matches!(self.motion, TouchMotion::Fling { .. })
    }

    pub fn move_to(&mut self, abs: DVec2, time: f64) -> Option<f64> {
        let TouchMotion::Drag { start, last, panning } = self.motion else {
            return None;
        };
        let displacement = abs - start;
        let pan = panning || (displacement.y.abs() >= TAP_COUNT_DISTANCE && displacement.y.abs() >= displacement.x.abs());
        self.velocity.push(abs.y, time);
        self.motion = TouchMotion::Drag { start, last: abs, panning: pan };
        pan.then_some(if panning { last.y - abs.y } else { start.y - abs.y })
    }

    pub fn release(&mut self, time: f64) -> bool {
        let motion = std::mem::take(&mut self.motion);
        let TouchMotion::Drag { panning, .. } = motion else {
            self.motion = motion;
            return false;
        };
        let velocity = self.velocity.estimate(time);
        if panning && velocity.abs() > SCROLL_STOP_SPEED {
            self.motion = TouchMotion::Fling { time: None, velocity };
        }
        panning
    }

    pub fn stop_at_edge(&mut self) {
        if let TouchMotion::Drag { .. } = &self.motion {
            // Preserve pan ownership so reversing at a bound takes effect immediately.
            self.velocity.clear();
        } else {
            self.reset();
        }
    }

    pub fn apply_delta(&mut self, position: &mut f64, delta: f64, max: f64) -> bool {
        let next = (*position + delta).clamp(0.0, max.max(0.0));
        let changed = next != *position;
        *position = next;
        if (delta < 0.0 && next == 0.0) || (delta > 0.0 && next == max.max(0.0)) {
            self.stop_at_edge();
        }
        changed
    }

    fn advance(&mut self, time: f64) -> Option<f64> {
        let TouchMotion::Fling { time: previous, velocity } = self.motion else {
            return None;
        };
        // Android touch timestamps use uptime; frames use the platform timer epoch.
        let Some(previous) = previous else {
            self.motion = TouchMotion::Fling { time: Some(time), velocity };
            return None;
        };
        let remaining = (velocity.abs() / SCROLL_STOP_SPEED).ln() / SCROLL_DECAY;
        let elapsed = (time - previous).max(0.0);
        let decay = (-SCROLL_DECAY * elapsed.min(remaining)).exp();
        let delta = velocity * (1.0 - decay) / SCROLL_DECAY;
        self.motion = if elapsed >= remaining {
            TouchMotion::Idle
        } else {
            TouchMotion::Fling { time: Some(time), velocity: velocity * decay }
        };
        Some(delta)
    }

    pub fn frame(&mut self, cx: &mut Cx, event: &Event) -> Option<f64> {
        let new_press = matches!(event, Event::MouseDown(_))
            || matches!(event, Event::TouchUpdate(update) if update.touches.iter().any(|touch| matches!(touch.state, TouchState::Start)));
        if matches!(event, Event::Pause | Event::AppLostFocus | Event::LongPress(_))
            || matches!(event, Event::TouchUpdate(update) if update.cancelled)
            || (self.is_flinging() && new_press)
        {
            self.reset();
            return None;
        }
        let delta = if let Some(frame) = self.next_frame.and_then(|next| next.is_event(event)) {
            self.next_frame = None;
            self.advance(frame.time)
        } else {
            None
        };
        self.schedule(cx);
        delta
    }

    pub fn schedule(&mut self, cx: &mut Cx) {
        if self.is_flinging() && self.next_frame.is_none() {
            self.next_frame = Some(cx.new_next_frame());
        }
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, hit: &Hit) -> Option<f64> {
        self.handle_axis_event(cx, event, hit, Vec2Index::Y)
    }

    pub fn handle_axis_event(&mut self, cx: &mut Cx, event: &Event, hit: &Hit, axis: Vec2Index) -> Option<f64> {
        let pan = self.handle_axis_hit(hit, axis);
        let fling = self.frame(cx, event);
        pan.or(fling)
    }

    pub fn handle_hit(&mut self, hit: &Hit) -> Option<f64> {
        self.handle_axis_hit(hit, Vec2Index::Y)
    }

    fn handle_axis_hit(&mut self, hit: &Hit, axis: Vec2Index) -> Option<f64> {
        let point = |abs: DVec2| match axis { Vec2Index::X => dvec2(abs.y, abs.x), Vec2Index::Y => abs };
        match hit {
            Hit::FingerDown(e) => {
                self.reset();
                if e.device.is_touch() {
                    self.begin(point(e.abs), e.time, true);
                    self.origin = e.abs;
                }
            }
            Hit::FingerMove(e) if e.device.is_touch() => return self.move_to(point(e.abs), e.time),
            Hit::FingerUp(e) => { self.release(e.time); }
            Hit::FingerLongPress(_) | Hit::FingerScroll(_) => self.reset(),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod touch_scroll_tests {
    use super::*;
    use makepad_draw::event::{DigitId, FingerLongPressEvent};

    fn down(device: DigitDevice) -> Hit {
        Hit::FingerDown(FingerDownEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, 100.0),
            digit_id: DigitId::default(), device, tap_count: 1,
            modifiers: KeyModifiers::default(), time: 0.0, rect: Rect::default(),
        })
    }

    fn moved(y: f64, device: DigitDevice) -> Hit {
        Hit::FingerMove(FingerMoveEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, y),
            digit_id: DigitId::default(), device, tap_count: 1,
            modifiers: KeyModifiers::default(), time: 0.1,
            abs_start: dvec2(20.0, 100.0), rect: Rect::default(), is_over: true,
        })
    }

    #[test]
    fn touch_pan_waits_for_slop_then_tracks_reversal() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        assert_eq!(scroll.handle_hit(&moved(96.0, DigitDevice::Touch { uid: 1 })), None);
        assert_eq!(scroll.handle_hit(&moved(80.0, DigitDevice::Touch { uid: 1 })), Some(20.0));
        assert_eq!(scroll.handle_hit(&moved(98.0, DigitDevice::Touch { uid: 1 })), Some(-18.0));
    }

    #[test]
    fn long_press_yields_to_selection_until_next_touch() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        scroll.handle_hit(&Hit::FingerLongPress(FingerLongPressEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, 100.0),
            capture_time: 0.0, time: 0.6, digit_id: DigitId::default(),
            device: DigitDevice::Touch { uid: 1 }, rect: Rect::default(),
        }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Touch { uid: 1 })), None);
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Touch { uid: 1 })), Some(50.0));
    }

    #[test]
    fn mouse_drag_remains_selection_not_panning() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Mouse { button: MouseButton::PRIMARY }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Mouse { button: MouseButton::PRIMARY })), None);
    }

    #[test]
    fn touch_motion_preserves_subdivided_drag_and_release_velocity() {
        let mut coalesced = TouchScroll::default();
        coalesced.begin(dvec2(20.0, 100.0), 0.0, true);
        assert_eq!(coalesced.move_to(dvec2(20.0, 80.0), 0.1), Some(20.0));
        let mut subdivided = TouchScroll::default();
        subdivided.begin(dvec2(20.0, 100.0), 0.0, true);
        let mut distance = 0.0;
        for step in 1..=10 {
            let delta = subdivided.move_to(dvec2(20.0, 100.0 - step as f64 * 2.0), step as f64 * 0.01);
            assert_eq!(delta.is_some(), step >= 3);
            distance += delta.unwrap_or(0.0);
        }
        assert_eq!(distance, 20.0);
        coalesced.release(0.1);
        subdivided.release(0.1);
        coalesced.advance(0.1);
        subdivided.advance(0.1);
        assert!((coalesced.advance(0.2).unwrap() - subdivided.advance(0.2).unwrap()).abs() < 1e-9);
    }

    #[test]
    fn touch_motion_fling_is_independent_of_frame_rate_and_clock_epoch() {
        let run = |fps: usize| {
            let mut s = TouchScroll::default();
            s.begin(dvec2(20.0, 200.0), 50000.0, true);
            s.move_to(dvec2(20.0, 100.0), 50000.1);
            assert!(s.release(50000.1));
            assert_eq!(s.advance(2.0), None);
            let mut distance = 0.0;
            for frame in 1..=fps * 4 {
                distance += s.advance(2.0 + frame as f64 / fps as f64).unwrap_or(0.0);
            }
            assert!(!s.is_flinging());
            distance
        };
        let at_30hz = run(30);
        assert!(at_30hz > 100.0);
        assert!((at_30hz - run(120)).abs() < 1e-9);
    }

    #[test]
    fn touch_motion_edge_keeps_pan_ownership_and_reversal() {
        let mut s = TouchScroll::default();
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        assert_eq!(s.move_to(dvec2(20.0, 120.0), 0.1), Some(-20.0));
        s.stop_at_edge();
        assert!(s.is_panning());
        assert_eq!(s.move_to(dvec2(20.0, 115.0), 0.2), Some(5.0));
        s.release(0.2);
        s.advance(1.0);
        assert!(s.advance(1.1).unwrap() > 0.0);
        s.stop_at_edge();
        assert_eq!(s.advance(1.2), None);
    }

    #[test]
    fn touch_motion_hold_tap_and_new_touch_cancel_inertia() {
        let mut s = TouchScroll::default();
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        assert_eq!(s.move_to(dvec2(20.0, 98.0), 0.01), None);
        assert!(!s.release(0.02));
        assert!(!s.is_flinging());
        s.begin(dvec2(20.0, 100.0), 1.0, true);
        s.move_to(dvec2(20.0, 50.0), 1.1);
        assert!(s.release(1.6));
        assert!(!s.is_flinging());
        s.begin(dvec2(20.0, 100.0), 2.0, true);
        s.move_to(dvec2(20.0, 50.0), 2.1);
        s.release(2.1);
        assert!(s.is_flinging());
        s.begin(dvec2(20.0, 50.0), 2.2, true);
        assert_eq!(s.advance(3.0), None);
        s.reset();
        assert_eq!(s.move_to(dvec2(20.0, 0.0), 2.3), None);
    }

    #[test]
    fn touch_motion_bounds_stop_fling_but_allow_drag_reversal() {
        let mut s = TouchScroll::default();
        let mut offset = 15.0;
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        let delta = s.move_to(dvec2(20.0, 80.0), 0.1).unwrap();
        assert!(s.apply_delta(&mut offset, delta, 20.0));
        assert_eq!(offset, 20.0);
        assert!(s.is_panning());
        let delta = s.move_to(dvec2(20.0, 90.0), 0.2).unwrap();
        assert!(s.apply_delta(&mut offset, delta, 20.0));
        assert_eq!(offset, 10.0);
        s.release(0.2);
        s.advance(1.0);
        let delta = s.advance(3.0).unwrap();
        s.apply_delta(&mut offset, delta, 20.0);
        assert_eq!(offset, 0.0);
        assert!(!s.is_flinging());
    }

    #[test]
    fn touch_motion_horizontal_axis_uses_same_slop_and_keeps_origin() {
        let mut s = TouchScroll::default();
        let down = down(DigitDevice::Touch { uid: 1 });
        s.handle_axis_hit(&down, Vec2Index::X);
        let mut movement = moved(100.0, DigitDevice::Touch { uid: 1 });
        if let Hit::FingerMove(e) = &mut movement {
            e.abs.x = 16.0;
        }
        assert_eq!(s.handle_axis_hit(&movement, Vec2Index::X), None);
        if let Hit::FingerMove(e) = &mut movement {
            e.abs.x = 10.0;
        }
        assert_eq!(s.handle_axis_hit(&movement, Vec2Index::X), Some(10.0));
        assert_eq!(s.origin(), dvec2(20.0, 100.0));
    }

    #[test]
    fn touch_motion_reversal_cannot_fling_in_previous_direction() {
        let mut s = TouchScroll::default();
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        s.move_to(dvec2(20.0, 0.0), 0.1);
        assert_eq!(s.move_to(dvec2(20.0, 5.0), 0.11), Some(-5.0));
        s.release(0.11);
        s.advance(1.0);
        assert!(s.advance(1.1).unwrap() < 0.0);
    }

    #[test]
    fn release_uses_recent_speed_not_the_gesture_average() {
        let mut accelerated = TouchScroll::default();
        accelerated.begin(dvec2(20.0, 1000.0), 0.0, true);
        accelerated.move_to(dvec2(20.0, 500.0), 0.5);
        accelerated.move_to(dvec2(20.0, 300.0), 0.6);
        accelerated.release(0.616);
        assert!(first_tenth_second(&mut accelerated) > 160.0);

        let mut slowed = TouchScroll::default();
        slowed.begin(dvec2(20.0, 1000.0), 0.0, true);
        slowed.move_to(dvec2(20.0, 500.0), 0.5);
        slowed.move_to(dvec2(20.0, 499.0), 0.6);
        slowed.release(0.616);
        assert!(!slowed.is_flinging());
    }

    fn first_tenth_second(scroll: &mut TouchScroll) -> f64 {
        assert_eq!(scroll.advance(10.0), None);
        scroll.advance(10.1).unwrap_or(0.0)
    }

    #[test]
    fn short_flick_keeps_speed_across_normal_up_delivery_gaps() {
        let mut distances = Vec::new();
        for gap in [0.0, 0.016, 0.032, 0.064] {
            let mut s = TouchScroll::default();
            s.begin(dvec2(20.0, 100.0), 50000.0, true);
            for step in 1..=4 {
                s.move_to(dvec2(20.0, 100.0 - step as f64 * 8.0), 50000.0 + step as f64 * 0.008);
            }
            s.release(50000.032 + gap);
            let distance = first_tenth_second(&mut s);
            // A 1000px/s flick should still cover most of 100px in its first
            // 100ms, not lose its speed to filter startup or UP latency.
            assert!(distance > 80.0 && distance < 100.0, "{distance}");
            distances.push(distance);
        }
        for distance in &distances[1..] {
            assert!((distance - distances[0]).abs() < 1e-9);
        }
    }

    #[test]
    fn batched_moves_with_equal_timestamps_keep_their_distance() {
        let mut coalesced = TouchScroll::default();
        let mut batched = TouchScroll::default();
        for s in [&mut coalesced, &mut batched] {
            s.begin(dvec2(20.0, 100.0), 0.0, true);
        }
        assert_eq!(coalesced.move_to(dvec2(20.0, 80.0), 0.016), Some(20.0));
        assert_eq!(batched.move_to(dvec2(20.0, 90.0), 0.016), Some(10.0));
        assert_eq!(batched.move_to(dvec2(20.0, 80.0), 0.016), Some(10.0));
        coalesced.release(0.032);
        batched.release(0.032);
        let distance = first_tenth_second(&mut coalesced);
        assert!(distance > 100.0);
        assert!((distance - first_tenth_second(&mut batched)).abs() < 1e-9);
    }

    #[test]
    fn sustained_flick_is_independent_of_move_rate_after_history_wraps() {
        let run = |steps: usize| {
            let mut s = TouchScroll::default();
            s.begin(dvec2(20.0, 400.0), 0.0, true);
            let mut dragged = 0.0;
            for step in 1..=steps {
                let time = 0.25 * step as f64 / steps as f64;
                dragged += s.move_to(dvec2(20.0, 400.0 - time * 1000.0), time).unwrap_or(0.0);
            }
            assert!((dragged - 250.0).abs() < 1e-9);
            s.release(0.266);
            first_tenth_second(&mut s)
        };
        let distance = run(8);
        assert!((distance - run(32)).abs() < 1e-9);
        assert!((distance - run(64)).abs() < 1e-9);
    }

    #[test]
    fn slow_drag_remains_one_to_one_without_a_fling() {
        let mut s = TouchScroll::default();
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        let mut dragged = 0.0;
        for step in 1..=20 {
            dragged += s.move_to(dvec2(20.0, 100.0 - step as f64), step as f64 * 0.25).unwrap_or(0.0);
        }
        assert_eq!(dragged, 20.0);
        assert!(s.release(5.016));
        assert!(!s.is_flinging());
    }

    #[test]
    fn held_release_cancels_fast_flick_even_with_stationary_move_events() {
        for stationary_moves in [false, true] {
            let mut s = TouchScroll::default();
            s.begin(dvec2(20.0, 100.0), 0.0, true);
            s.move_to(dvec2(20.0, 0.0), 0.025);
            if stationary_moves {
                for step in 1..=7 {
                    assert_eq!(s.move_to(dvec2(20.0, 0.0), 0.025 + step as f64 * 0.016), Some(0.0));
                }
            }
            assert!(s.release(0.145));
            assert!(!s.is_flinging());
        }
    }

    #[test]
    fn bounds_discard_velocity_before_release_without_losing_pan() {
        let mut s = TouchScroll::default();
        s.begin(dvec2(20.0, 100.0), 0.0, true);
        let delta = s.move_to(dvec2(20.0, 0.0), 0.032).unwrap();
        let mut position = 90.0;
        assert!(s.apply_delta(&mut position, delta, 100.0));
        assert!(s.is_panning());
        assert!(s.release(0.048));
        assert!(!s.is_flinging());
    }

    #[test]
    fn cancelled_touch_cannot_fling_regardless_of_adapter_event_order() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let event = Event::TouchUpdate(makepad_draw::event::TouchUpdateEvent {
            window_id: WindowId(0, 0),
            time: 0.048,
            modifiers: KeyModifiers::default(),
            touches: Vec::new(),
            cancelled: true,
        });
        for release_first in [false, true] {
            let mut s = TouchScroll::default();
            s.begin(dvec2(20.0, 100.0), 0.0, true);
            s.move_to(dvec2(20.0, 0.0), 0.032);
            if release_first {
                s.release(0.048);
                assert!(s.is_flinging());
            }
            assert_eq!(s.frame(&mut cx, &event), None);
            assert!(!s.release(0.048));
            assert!(!s.is_dragging());
            assert!(!s.is_flinging());
            assert!(s.next_frame.is_none());
        }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct ScrollBars {
    #[live] show_scroll_x: bool,
    #[live] show_scroll_y: bool,
    #[live] scroll_bar_x: ScrollBar,
    #[live] scroll_bar_y: ScrollBar,
    #[rust] nav_scroll_index: Option<NavScrollIndex>,
    #[rust] scroll: DVec2,
    #[rust] area: Area,
    #[rust] touch_scroll: TouchScroll,
}

pub enum ScrollBarsAction {
    ScrollX(f64),
    ScrollY(f64),
    None
}

impl ScrollBars {
    
    pub fn set_scroll_x(&mut self, _cx: &mut Cx, value: f64) {
        self.scroll.x = value;
    }
    
    pub fn set_scroll_y(&mut self, _cx: &mut Cx, value: f64) {
        self.scroll.y = value;
    }
    
    pub fn get_scroll_pos(&self) -> DVec2 {
        self.scroll
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope:&mut Scope)->Vec<ScrollBarsAction> {
        let mut actions = Vec::new();
        self.handle_main_event(cx, event, scope, &mut actions);
        self.handle_scroll_event(cx, event, scope, &mut actions);
        actions
    }
    
    pub fn handle_main_event(&mut self, cx: &mut Cx, event: &Event, _scope:&mut Scope, actions: &mut Vec<ScrollBarsAction> ) {
        if let Event::Trigger(te) = event{
            if let Some(triggers) = te.triggers.get(&self.area){
                if let Some(trigger) = triggers.iter().find(|t| t.id == live_id!(scroll_focus_nav)){
                    let self_rect = self.area.rect(cx);
                    self.scroll_into_view(
                        cx,
                        trigger.from.rect(cx)
                        .translate(-self_rect.pos + self.scroll)
                        .add_margin(dvec2(5.0,5.0))
                    );
                }
            }
        }
        
        if self.show_scroll_x {
            let mut ret_x = None;
            self.scroll_bar_x.handle_event_with(cx, event, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_x = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollX(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(x) = ret_x {self.scroll.x = x; self.redraw(cx);}
        }
        if self.show_scroll_y {
            let mut ret_y = None;
            self.scroll_bar_y.handle_event_with(cx, event, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_y = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollY(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(y) = ret_y {self.scroll.y = y; self.redraw(cx);}
        }
    }
    
    pub fn handle_scroll_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope, actions: &mut Vec<ScrollBarsAction> ) {
        // Run after children: the deepest scroll region owns each touch move.
        // Overloaded capture preserves child taps and their long-press handling.
        let can_pan = self.show_scroll_y
            && self.area.is_valid(cx)
            && self.scroll_bar_y.get_scroll_view_total() > self.scroll_bar_y.get_scroll_view_visible()
            && !self.scroll_bar_x.is_area_captured(cx)
            && !self.scroll_bar_y.is_area_captured(cx);
        if !can_pan {
            self.touch_scroll.reset();
        }
        if can_pan && matches!(event, Event::TouchUpdate(_) | Event::LongPress(_)) {
            let child_scrolled = if let Event::TouchUpdate(e) = event {
                e.touches.iter().any(|t| matches!(t.state, TouchState::Move) && !t.handled.get().is_empty())
            } else { false };
            let hit = event.hits_with_capture_overload(cx, self.area, true);
            if child_scrolled {
                self.touch_scroll.reset();
            } else if let Some(dy) = self.touch_scroll.handle_hit(&hit) {
                if let Event::TouchUpdate(e) = event {
                    for t in &e.touches {
                        if matches!(t.state, TouchState::Move) {
                            t.handled.set(self.area);
                        }
                    }
                }
                self.apply_touch_scroll(cx, dy, actions);
            }
        }
        if let Event::Scroll(e) = event {
            if self.area.rect(cx).contains(e.abs) {
                self.touch_scroll.reset();
            }
        }
        if let Some(dy) = self.touch_scroll.frame(cx, event) {
            self.apply_touch_scroll(cx, dy, actions);
        }
        
        if self.show_scroll_x {
            let mut ret_x = None;
            self.scroll_bar_x.handle_scroll_event(cx, event, self.area, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_x = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollX(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(x) = ret_x {self.scroll.x = x; self.redraw(cx);}
        }
        if self.show_scroll_y {
            let mut ret_y = None;
            self.scroll_bar_y.handle_scroll_event(cx, event, self.area, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_y = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollY(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(y) = ret_y {self.scroll.y = y; self.redraw(cx);}
        }
    }

    fn apply_touch_scroll(&mut self, cx: &mut Cx, dy: f64, actions: &mut Vec<ScrollBarsAction>) {
        if self.set_scroll_pos(cx, self.scroll + dvec2(0.0, dy)) {
            actions.push(ScrollBarsAction::ScrollY(self.scroll.y));
            self.redraw(cx);
        }
        let max = (self.scroll_bar_y.get_scroll_view_total() - self.scroll_bar_y.get_scroll_view_visible()).max(0.0);
        if (dy < 0.0 && self.scroll.y == 0.0) || (dy > 0.0 && self.scroll.y == max) {
            self.touch_scroll.stop_at_edge();
        }
    }
    
    pub fn set_scroll_pos(&mut self, cx: &mut Cx, pos: DVec2) -> bool {
        //let view_area = Area::DrawList(DrawListArea{draw_list_id:draw_list_id, redraw_id:cx.redraw_id});
        let mut changed = false;
        if self.show_scroll_x {
            if self.scroll_bar_x.set_scroll_pos(cx, pos.x) {
                changed = true;
            }
            let scroll_pos = self.scroll_bar_x.get_scroll_pos();
            self.set_scroll_x(cx, scroll_pos);
        }
        if self.show_scroll_y {
            if self.scroll_bar_y.set_scroll_pos(cx, pos.y) {
                changed = true;
            }
            let scroll_pos = self.scroll_bar_y.get_scroll_pos();
            self.set_scroll_y(cx, scroll_pos);
        }
        changed
    }
    
    pub fn set_scroll_pos_no_clip(&mut self, cx: &mut Cx, pos: DVec2) -> bool {
        let mut changed = false;
        if self.show_scroll_x {
            if self.scroll_bar_x.set_scroll_pos_no_clip(cx, pos.x) {
                changed = true;
            }
            self.set_scroll_x(cx, pos.x);
        }
        if self.show_scroll_y {
            if self.scroll_bar_y.set_scroll_pos_no_clip(cx, pos.y) {
                changed = true;
            }
            self.set_scroll_y(cx, pos.y);
        }
        changed
    }
    
    pub fn get_scroll_view_total(&mut self) -> DVec2 {
        DVec2 {
            x: if self.show_scroll_x {
                self.scroll_bar_x.get_scroll_view_total()
            }else {0.},
            y: if self.show_scroll_y {
                self.scroll_bar_y.get_scroll_view_total()
            }else {0.}
        }
    }
    
    pub fn get_scroll_view_visible(&mut self) -> DVec2 {
        DVec2 {
            x: if self.show_scroll_x {
                self.scroll_bar_x.get_scroll_view_visible()
            }else {0.},
            y: if self.show_scroll_y {
                self.scroll_bar_y.get_scroll_view_visible()
            }else {0.}
        }
    }
    
    pub fn get_viewport_rect(&mut self, _cx: &mut Cx) -> Rect {
        let pos = self.get_scroll_pos();
        let size = self.get_scroll_view_visible();
        Rect {pos, size}
    }
    
    pub fn scroll_into_view(&mut self, cx: &mut Cx, rect: Rect) {
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x, rect.size.x, true);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y, rect.size.y, true);
        }
    }
    
    pub fn scroll_into_view_no_smooth(&mut self, cx: &mut Cx, rect: Rect) {
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x, rect.size.x, false);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y, rect.size.y, false);
        }
    }
    
    pub fn scroll_into_view_abs(&mut self, cx: &mut Cx, rect: Rect) {
        let self_rect = self.area.rect(cx);
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x - self_rect.pos.x, rect.size.x, true);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y - self_rect.pos.y, rect.size.y, true);
        }
    }
    
    pub fn set_scroll_target(&mut self, cx: &mut Cx, pos: DVec2) {
        if self.show_scroll_x {
            self.scroll_bar_x.set_scroll_target(cx, pos.x);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.set_scroll_target(cx, pos.y);
        }
    }
    
    
    // all in one scrollbar api
    
    pub fn begin(&mut self, cx: &mut Cx2d, walk: Walk, layout: Layout) {
        cx.begin_turtle(walk, layout.with_scroll(self.scroll));
        self.begin_nav_area(cx);
    }
    
    pub fn end(&mut self, cx: &mut Cx2d) {
        self.draw_scroll_bars(cx);
        // this needs to be a rect_area
        cx.end_turtle_with_area(&mut self.area);
        self.end_nav_area(cx);
    }
    
    
    pub fn end_with_shift(&mut self, cx: &mut Cx2d) {
        self.draw_scroll_bars(cx);
        // this needs to be a rect_area
        cx.end_turtle_with_area(&mut self.area);
        self.end_nav_area(cx);
    }
    // separate API
    
    pub fn begin_nav_area(&mut self, cx: &mut Cx2d) {
        self. nav_scroll_index = Some(cx.add_begin_scroll());
    }
    
    pub fn end_nav_area(&mut self, cx: &mut Cx2d) {
        if !self.area.is_valid(cx) {
            error!("Call set area before end_nav_area");
            return
        }
        cx.add_end_scroll(self.nav_scroll_index.take().unwrap(), self.area);
    }
    
    pub fn draw_scroll_bars(&mut self, cx: &mut Cx2d) {
        // lets ask the turtle our actual bounds
        let view_total = cx.turtle().used();
        let mut rect_now = cx.turtle().rect();
        
        if rect_now.size.y.is_nan() {
            rect_now.size.y = view_total.y;
        }
        if rect_now.size.x.is_nan() {
            rect_now.size.x = view_total.x;
        }
        
        if self.show_scroll_x {
            let scroll_pos = self.scroll_bar_x.draw_scroll_bar(cx, ScrollAxis::Horizontal, rect_now, view_total);
            self.set_scroll_x(cx, scroll_pos);
        }
        if self.show_scroll_y {
            //println!("SET SCROLLBAR {} {}", rect_now.h, view_total.y);
            let scroll_pos = self.scroll_bar_y.draw_scroll_bar(cx, ScrollAxis::Vertical, rect_now, view_total);
            self.set_scroll_y(cx, scroll_pos);
        }
    }
    
    pub fn set_area(&mut self, area: Area) {
        self.area = area;
    }
    
    pub fn area(&self) -> Area {
        self.area
    }
    
    pub fn redraw(&self, cx: &mut Cx) {
        self.area.redraw(cx);
    }
}