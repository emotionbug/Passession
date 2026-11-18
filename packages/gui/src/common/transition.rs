//! Bounded navigation motion. The owner chooses what to compose from the current progress;
//! neither retargeting nor drawing should schedule an independent animation clock.

mod patterns;

pub use patterns::{Axis, Frame, Intent, Pattern, Style, StyleFamily, Visual};

use std::cell::Cell;
use std::time::Duration;

use makepad_widgets::{Cx, Event, NextFrame};

pub mod tokens {
    use super::{Duration, Spec};

    pub const SHORT_1: Duration = Duration::from_millis(50);
    pub const SHORT_2: Duration = Duration::from_millis(100);
    pub const SHORT_3: Duration = Duration::from_millis(150);
    pub const SHORT_4: Duration = Duration::from_millis(200);
    pub const MEDIUM_1: Duration = Duration::from_millis(250);
    pub const MEDIUM_2: Duration = Duration::from_millis(300);
    pub const MEDIUM_3: Duration = Duration::from_millis(350);
    pub const MEDIUM_4: Duration = Duration::from_millis(400);
    pub const LONG_1: Duration = Duration::from_millis(450);
    pub const LONG_2: Duration = Duration::from_millis(500);
    pub const HOVER_DWELL: Duration = SHORT_3;
    pub const COPY_FEEDBACK: Duration = Duration::from_millis(1500);
    pub const TIMESTAMP_PEEK: Duration = Duration::from_secs(3);

    pub const EMPHASIZED: [f64; 4] = [0.2, 0.0, 0.0, 1.0];
    pub const EMPHASIZED_DECELERATE: [f64; 4] = [0.05, 0.7, 0.1, 1.0];
    pub const EMPHASIZED_ACCELERATE: [f64; 4] = [0.3, 0.0, 0.8, 0.15];
    pub const STANDARD: [f64; 4] = [0.2, 0.0, 0.0, 1.0];
    pub const STANDARD_DECELERATE: [f64; 4] = [0.0, 0.0, 0.0, 1.0];
    pub const STANDARD_ACCELERATE: [f64; 4] = [0.3, 0.0, 1.0, 1.0];

    // AndroidX Compose Material3 StandardMotionTokens.
    pub const FAST_SPATIAL: Spec = Spec::Spring {
        damping: 0.9,
        stiffness: 1400.0,
    };
    pub const SPATIAL: Spec = Spec::Spring {
        damping: 0.9,
        stiffness: 700.0,
    };
    pub const SLOW_SPATIAL: Spec = Spec::Spring {
        damping: 0.9,
        stiffness: 300.0,
    };
    pub const FAST_EFFECTS: Spec = Spec::Spring {
        damping: 1.0,
        stiffness: 3800.0,
    };
    pub const EFFECTS: Spec = Spec::Spring {
        damping: 1.0,
        stiffness: 1600.0,
    };
    pub const SLOW_EFFECTS: Spec = Spec::Spring {
        damping: 1.0,
        stiffness: 800.0,
    };
}

#[derive(Clone, Copy)]
pub enum Spec {
    Tween(Duration, [f64; 4]),
    Spring { damping: f64, stiffness: f64 },
}

impl Spec {
    pub const IMMEDIATE: Self = Self::Tween(Duration::ZERO, tokens::STANDARD);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loop {
    StatusRing,
    Spinner,
    LoadingBar,
    ThinkingDots,
    Shimmer,
    CaretBlink,
    SkeletonPulse,
    IndeterminateRing,
}

#[derive(Clone, Copy)]
pub struct LoopStyle {
    pub cadence: Duration,
    pub period: Duration,
    ring_rotation: Duration,
    material_ring: bool,
}

impl Loop {
    /// Loop styles are data only. Schedule visible indicators with `common::clock`.
    pub const fn resolve(self) -> LoopStyle {
        self.resolve_for(StyleFamily::current())
    }

    pub const fn resolve_for(self, family: StyleFamily) -> LoopStyle {
        use Loop::*;
        let (cadence, period) = match (self, family) {
            (StatusRing, _) => (125, 1250),
            // Cupertino's activity indicator steps its spokes eight times a second.
            (Spinner, StyleFamily::Cupertino) => (125, 1000),
            (Spinner, StyleFamily::Material) => (33, 1000),
            (Spinner, StyleFamily::Desktop) => (33, 900),
            (LoadingBar, _) => (33, 1400),
            // Dots advance one of six discrete states per cadence, so a cycle is
            // exactly six cadences and the phase stays linear.
            (ThinkingDots, StyleFamily::Material) => (150, 900),
            (ThinkingDots, StyleFamily::Cupertino) => (166, 996),
            (ThinkingDots, StyleFamily::Desktop) => (158, 948),
            (Shimmer, StyleFamily::Cupertino) => (33, 1500),
            (Shimmer, _) => (33, 1600),
            (CaretBlink, StyleFamily::Cupertino) => (530, 1060),
            (CaretBlink, _) => (500, 1000),
            (SkeletonPulse, _) => (33, 2000),
            (IndeterminateRing, StyleFamily::Material) => (33, 1333),
            (IndeterminateRing, _) => (33, 900),
        };
        LoopStyle {
            cadence: Duration::from_millis(cadence),
            period: Duration::from_millis(period),
            ring_rotation: Duration::from_millis(if matches!(family, StyleFamily::Material) {
                2222
            } else {
                period
            }),
            material_ring: matches!((self, family), (IndeterminateRing, StyleFamily::Material)),
        }
    }
}

impl LoopStyle {
    /// Linear position in the current cycle, 0..1. Loops shape their own
    /// output (rings, pulses) from this; a linear phase keeps discrete steps
    /// such as the thinking dots from skipping states.
    pub fn phase(self, elapsed_ms: f64) -> f64 {
        let period = self.period.as_secs_f64() * 1000.0;
        elapsed_ms.rem_euclid(period) / period
    }

    pub fn pulse(self, elapsed_ms: f64) -> f64 {
        1.0 - (self.phase(elapsed_ms) * 2.0 - 1.0).abs()
    }

    /// Normalized clockwise start and arc length in turns.
    pub fn ring(self, elapsed_ms: f64) -> (f64, f64) {
        let rotation_ms = self.ring_rotation.as_secs_f64() * 1000.0;
        let rotation = elapsed_ms.rem_euclid(rotation_ms) / rotation_ms;
        if !self.material_ring {
            return (rotation, 0.25);
        }
        let phase = self.phase(elapsed_ms);
        let curve = [0.4, 0.0, 0.2, 1.0];
        let head = 0.75 * ease((phase * 2.0).min(1.0), curve);
        let tail = 0.75 * ease((phase * 2.0 - 1.0).max(0.0), curve);
        (
            -0.5 + tail + rotation + phase * 0.25,
            (head - tail).max(0.001 / std::f64::consts::TAU),
        )
    }
}

pub struct Fling;

#[derive(Clone, Copy)]
pub struct FlingStyle {
    pub decay: f64,
    pub stop_speed: f64,
}

impl Fling {
    pub const fn resolve() -> FlingStyle {
        Self::resolve_for(StyleFamily::current())
    }

    pub const fn resolve_for(family: StyleFamily) -> FlingStyle {
        let (decay, stop_speed) = match family {
            StyleFamily::Material => (2.3, 12.0),
            StyleFamily::Cupertino => (2.0, 10.0),
            StyleFamily::Desktop => (1.82755, 12.0),
        };
        FlingStyle { decay, stop_speed }
    }
}

thread_local! {
    /// All windows consume the same Makepad frame stream. Coalesce the demand across them.
    static NEXT_FRAME: Cell<Option<NextFrame>> = const { Cell::new(None) };
}

pub(crate) fn begin_event(event: &Event) {
    if let Event::NextFrame(frame) = event {
        NEXT_FRAME.with(|slot| {
            if slot.get().is_some_and(|next| frame.set.contains(&next)) {
                slot.set(None);
            }
        });
    }
}

pub(crate) fn request_frame(cx: &mut Cx) -> NextFrame {
    NEXT_FRAME.with(|slot| {
        if let Some(frame) = slot.get() {
            return frame;
        }
        let frame = cx.new_next_frame();
        slot.set(Some(frame));
        frame
    })
}

pub(crate) fn frame_time(event: &Event, token: NextFrame) -> Option<f64> {
    match event {
        Event::NextFrame(frame) if frame.set.contains(&token) => Some(frame.time),
        _ => None,
    }
}

struct Motion {
    from: f64,
    to: f64,
    velocity: f64,
    spec: Spec,
    start: Option<f64>,
}

/// A scalar transition (typically position, scale, or opacity progress from 0 to 1).
#[derive(Default)]
pub(crate) struct Transition {
    value: f64,
    velocity: f64,
    motion: Option<Motion>,
    frame: Option<NextFrame>,
}

impl Transition {
    pub(crate) fn value(&self) -> f64 {
        self.value
    }

    pub(crate) fn is_moving(&self) -> bool {
        self.motion.is_some()
    }

    /// Cancel motion on resize, when handed to a gesture, or when motion is disabled.
    /// Returns whether the presentation changed (including changing input ownership).
    pub(crate) fn snap(&mut self, value: f64) -> bool {
        let changed = self.value != value || self.is_moving();
        self.value = value;
        self.velocity = 0.0;
        self.motion = None;
        self.frame = None;
        changed
    }

    /// Start at the last sampled position (and spring velocity), preserving a pending frame.
    pub(crate) fn retarget(&mut self, cx: &mut Cx, target: f64, spec: Spec) -> bool {
        let changed = self.set_target(target, spec);
        self.request_frame(cx);
        changed
    }

    #[cfg(test)]
    pub(crate) fn scheduled_frame(&self) -> Option<NextFrame> {
        self.frame
    }

    /// Process this transition's own NextFrame only. True requests a composition redraw.
    pub(crate) fn advance(&mut self, cx: &mut Cx, event: &Event) -> bool {
        self.advance_when_ready(cx, event, true)
    }

    /// Keep the shared wake armed while a newly composed backing surface has not painted.
    pub(crate) fn advance_when_ready(&mut self, cx: &mut Cx, event: &Event, ready: bool) -> bool {
        let Event::NextFrame(_) = event else {
            return false;
        };
        let Some(next) = self.frame else {
            return false;
        };
        let Some(time) = frame_time(event, next) else {
            // A dormant owner can miss its wake while a window is hidden.
            if NEXT_FRAME.with(Cell::get) != Some(next) {
                self.frame = None;
                self.request_frame(cx);
            }
            return false;
        };
        self.frame = None;
        let changed = ready && self.sample(time);
        self.request_frame(cx);
        changed
    }

    fn request_frame(&mut self, cx: &mut Cx) {
        if self.is_moving() && self.frame.is_none() {
            self.frame = Some(request_frame(cx));
        }
    }

    fn set_target(&mut self, target: f64, spec: Spec) -> bool {
        if matches!(spec, Spec::Tween(duration, _) if duration.is_zero())
            || (self.value == target && self.velocity == 0.0)
        {
            return self.snap(target);
        }
        if self
            .motion
            .as_ref()
            .is_some_and(|motion| motion.to == target)
        {
            return false;
        }
        self.motion = Some(Motion {
            from: self.value,
            to: target,
            velocity: self.velocity,
            spec,
            start: None,
        });
        true
    }

    fn sample(&mut self, now: f64) -> bool {
        let Some(motion) = &mut self.motion else {
            return false;
        };
        let start = *motion.start.get_or_insert(now);
        let elapsed = (now - start).max(0.0);
        let (value, velocity, settled) = match motion.spec {
            Spec::Tween(duration, curve) => {
                let t = elapsed / duration.as_secs_f64();
                (
                    motion.from + (motion.to - motion.from) * ease(t, curve),
                    0.0,
                    t >= 1.0,
                )
            }
            Spec::Spring { damping, stiffness } => {
                let (delta, velocity) = spring(
                    motion.from - motion.to,
                    motion.velocity,
                    elapsed,
                    damping,
                    stiffness,
                );
                (
                    motion.to + delta,
                    velocity,
                    delta.abs() < 0.001 && velocity.abs() < 0.01,
                )
            }
        };
        if settled {
            let to = motion.to;
            return self.snap(to);
        }
        self.velocity = velocity;
        let changed = self.value != value;
        self.value = value;
        changed
    }
}

/// Analytic unit-mass damped spring. Sample at the frame time rather than integrating steps.
fn spring(x: f64, velocity: f64, t: f64, damping: f64, stiffness: f64) -> (f64, f64) {
    let w = stiffness.sqrt();
    let decay = damping * w;
    let envelope = (-decay * t).exp();
    let b = velocity + decay * x;
    let (position, slope) = if damping < 1.0 {
        let frequency = w * (1.0 - damping * damping).sqrt();
        let (sin, cos) = (frequency * t).sin_cos();
        let position = x * cos + b / frequency * sin;
        (position, -x * frequency * sin + b * cos)
    } else if damping == 1.0 {
        (x + b * t, b)
    } else {
        let frequency = w * (damping * damping - 1.0).sqrt();
        let first = -decay + frequency;
        let second = -decay - frequency;
        let a = (velocity - second * x) / (first - second);
        let left = a * (first * t).exp();
        let right = (x - a) * (second * t).exp();
        return (left + right, first * left + second * right);
    };
    (envelope * position, envelope * (slope - decay * position))
}

fn ease(t: f64, [x1, y1, x2, y2]: [f64; 4]) -> f64 {
    if t <= 0.0 || t >= 1.0 {
        return t.clamp(0.0, 1.0);
    }
    if x1 == y1 && x2 == y2 {
        return t;
    }
    let bezier = |a: f64, b: f64, s: f64| {
        let inv = 1.0 - s;
        3.0 * a * s * inv * inv + 3.0 * b * s * s * inv + s * s * s
    };
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if bezier(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bezier(y1, y2, (lo + hi) * 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHARED_AXIS: Spec = Spec::Tween(tokens::MEDIUM_2, tokens::EMPHASIZED);

    #[test]
    fn rapid_reversal_keeps_its_sampled_position_and_settles() {
        let mut t = Transition::default();
        assert!(!t.set_target(0.0, SHARED_AXIS));
        assert!(t.set_target(1.0, SHARED_AXIS));
        assert!(!t.sample(0.0));
        assert!(t.sample(0.08));
        let value = t.value();
        assert!(!t.set_target(1.0, SHARED_AXIS));
        assert!(t.set_target(0.0, SHARED_AXIS));
        assert!(!t.sample(0.08));
        assert_eq!(t.value(), value);
        t.sample(0.15);
        let value = t.value();
        assert!(t.set_target(1.0, SHARED_AXIS));
        assert!(!t.sample(0.15));
        assert_eq!(t.value(), value);
        assert!(t.sample(0.46));
        assert_eq!(t.value(), 1.0);
        assert!(!t.sample(0.47));
        assert!(!t.is_moving());
        assert!(t.frame.is_none());
    }

    #[test]
    fn spring_retarget_preserves_velocity() {
        let mut t = Transition::default();
        t.set_target(1.0, tokens::SPATIAL);
        t.sample(0.0);
        t.sample(0.03);
        let velocity = t.velocity;
        assert!(velocity > 0.0);
        t.set_target(0.0, tokens::SPATIAL);
        t.sample(0.03);
        assert!((t.velocity - velocity).abs() < 1e-10);
        assert!(t.sample(0.58));
        assert_eq!(t.value(), 0.0);
        assert!(!t.is_moving());
    }

    #[test]
    fn snap_cancels_motion_and_immediate_does_not_arm_it() {
        let mut t = Transition::default();
        t.set_target(1.0, SHARED_AXIS);
        t.sample(0.0);
        t.sample(0.15);
        assert!(t.is_moving());
        assert!(t.snap(1.0));
        assert!(!t.sample(0.32));
        assert!(!t.is_moving());
        assert!(t.set_target(0.0, Spec::IMMEDIATE));
        assert_eq!(t.value(), 0.0);
        assert!(!t.is_moving());
        assert!(t.frame.is_none());
    }

    #[test]
    fn simultaneous_transitions_coalesce_and_release_the_shared_wake() {
        use makepad_widgets::NextFrameEvent;
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let mut a = Transition::default();
        let mut b = Transition::default();
        a.retarget(&mut cx, 1.0, SHARED_AXIS);
        b.retarget(&mut cx, 1.0, SHARED_AXIS);
        assert_eq!(a.scheduled_frame(), b.scheduled_frame());
        for time in [0.0, 0.15, 0.31] {
            let event = Event::NextFrame(NextFrameEvent {
                frame: 0,
                time,
                set: [a.scheduled_frame().unwrap()].into(),
            });
            begin_event(&event);
            a.advance(&mut cx, &event);
            b.advance(&mut cx, &event);
            assert_eq!(a.scheduled_frame(), b.scheduled_frame());
        }
        assert!(a.scheduled_frame().is_none());
        assert!(NEXT_FRAME.with(Cell::get).is_none());
    }

    #[test]
    fn material_ring_grows_and_shrinks_without_changing_clock_cadence() {
        let ring = Loop::IndeterminateRing.resolve_for(StyleFamily::Material);
        let (start, sweep) = ring.ring(0.0);
        assert_eq!(start, -0.5);
        assert!(sweep < 0.001);
        assert!((ring.ring(666.5).1 - 0.75).abs() < 1e-7);
        assert!(ring.ring(1333.0 * 0.999).1 < 0.01);
        assert_eq!(ring.cadence, Duration::from_millis(33));
        assert_eq!(Loop::SkeletonPulse.resolve().pulse(1000.0), 1.0);
    }
}
