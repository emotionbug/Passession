//! Motion timing shared by Makepad widgets and the application.

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StyleFamily {
    Material,
    Cupertino,
    Desktop,
}

impl StyleFamily {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Cupertino
        } else {
            Self::mobile_or_desktop()
        }
    }

    /// Use desktop styling on every desktop, including macOS. Used by app navigation
    /// (ported from the web/Electron client) and the existing desktop scrollbar.
    pub const fn mobile_or_desktop() -> Self {
        if cfg!(target_os = "android") {
            Self::Material
        } else if cfg!(target_os = "ios") {
            Self::Cupertino
        } else {
            Self::Desktop
        }
    }
}

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

    pub const EMPHASIZED: [f64; 4] = STANDARD;
    pub const EMPHASIZED_DECELERATE: [f64; 4] = [0.05, 0.7, 0.1, 1.0];
    pub const EMPHASIZED_ACCELERATE: [f64; 4] = [0.3, 0.0, 0.8, 0.15];
    pub const LINEAR: [f64; 4] = [0.0, 0.0, 1.0, 1.0];
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Spec {
    Tween(Duration, [f64; 4]),
    Spring { damping: f64, stiffness: f64 },
}

pub struct Sample {
    pub value: f64,
    pub velocity: f64,
    pub settled: bool,
}

impl Spec {
    pub const IMMEDIATE: Self = Self::Tween(Duration::ZERO, tokens::STANDARD);

    pub const fn is_immediate(self) -> bool {
        matches!(self, Self::Tween(duration, _) if duration.is_zero())
    }

    pub fn sample(self, from: f64, to: f64, velocity: f64, elapsed: f64) -> Sample {
        let (value, velocity, settled) = match self {
            Self::Tween(duration, curve) => {
                let t = if duration.is_zero() {
                    1.0
                } else {
                    elapsed / duration.as_secs_f64()
                };
                (from + (to - from) * ease(t, curve), 0.0, t >= 1.0)
            }
            Self::Spring { damping, stiffness } => {
                let (delta, velocity) = spring(from - to, velocity, elapsed, damping, stiffness);
                (
                    to + delta,
                    velocity,
                    delta.abs() < 0.001 && velocity.abs() < 0.01,
                )
            }
        };
        if settled {
            Sample {
                value: to,
                velocity: 0.0,
                settled,
            }
        } else {
            Sample {
                value,
                velocity,
                settled,
            }
        }
    }
}

/// Scalar motion anchored in the render clock by its first frame.
pub struct Motion {
    from: f64,
    to: f64,
    velocity: f64,
    spec: Spec,
    start: Option<f64>,
}

impl Motion {
    pub fn new(from: f64, to: f64, velocity: f64, spec: Spec) -> Self {
        Self {
            from,
            to,
            velocity,
            spec,
            start: None,
        }
    }

    pub fn target(&self) -> f64 {
        self.to
    }

    pub fn sample(&mut self, now: f64) -> Sample {
        let start = *self.start.get_or_insert(now);
        self.spec
            .sample(self.from, self.to, self.velocity, (now - start).max(0.0))
    }
}

/// Analytic unit-mass damped spring. Sample at the frame time rather than integrating steps.
fn spring(x: f64, velocity: f64, t: f64, damping: f64, stiffness: f64) -> (f64, f64) {
    let w = stiffness.sqrt();
    let decay = damping * w;
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
    let envelope = (-decay * t).exp();
    (envelope * position, envelope * (slope - decay * position))
}

pub fn ease(t: f64, [x1, y1, x2, y2]: [f64; 4]) -> f64 {
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

pub(crate) struct CaretStyle {
    pub half_period: Duration,
    pub fade: Spec,
}

impl CaretStyle {
    pub const fn resolve(family: StyleFamily) -> Self {
        Self {
            half_period: tokens::LONG_2,
            fade: match family {
                StyleFamily::Cupertino => Spec::Tween(tokens::SHORT_1, tokens::LINEAR),
                _ => Spec::IMMEDIATE,
            },
        }
    }
}

pub(crate) struct ScrollbarStyle {
    pub hold: Duration,
    pub fade: Spec,
}

impl ScrollbarStyle {
    pub const fn resolve(family: StyleFamily) -> Self {
        use tokens::*;
        match family {
            StyleFamily::Material | StyleFamily::Cupertino => Self {
                hold: MEDIUM_2,
                fade: Spec::Tween(MEDIUM_1, LINEAR),
            },
            StyleFamily::Desktop => Self {
                hold: Duration::from_millis(600),
                fade: Spec::Tween(MEDIUM_2, LINEAR),
            },
        }
    }
}

/// Returns the allowed top-edge pull distance and how it returns to rest.
/// A desktop list opts in by supplying a positive `max_pull_down`.
pub(crate) fn pulldown_style(
    family: StyleFamily,
    max_pull_down: Option<f64>,
) -> Option<(f64, Spec)> {
    use tokens::*;
    let (limit, spec) = match family {
        StyleFamily::Material => return None,
        // Flutter's BouncingScrollPhysics spring, normalized to unit mass.
        StyleFamily::Cupertino => (
            max_pull_down.unwrap_or(80.0),
            Spec::Spring {
                damping: 1.1,
                stiffness: 200.0,
            },
        ),
        StyleFamily::Desktop => (max_pull_down?, Spec::Tween(MEDIUM_4, STANDARD_DECELERATE)),
    };
    (limit > 0.0).then_some((limit, spec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use StyleFamily::*;

    #[test]
    fn caret_motion_is_smooth_only_in_cupertino() {
        for family in [Material, Cupertino, Desktop] {
            let style = CaretStyle::resolve(family);
            assert_eq!(style.half_period, Duration::from_millis(500));
            let sample = style.fade.sample(1.0, 0.0, 0.0, 0.025);
            if family == Cupertino {
                assert_eq!(sample.value, 0.5);
                assert!(!sample.settled);
            } else {
                assert_eq!(sample.value, 0.0);
                assert!(sample.settled);
            }
        }
    }

    #[test]
    fn scrollbar_motion_has_a_shorter_hold_and_fade_on_phones() {
        for (family, hold, fade) in [
            (Material, 300, 250),
            (Cupertino, 300, 250),
            (Desktop, 600, 300),
        ] {
            let style = ScrollbarStyle::resolve(family);
            assert_eq!(style.hold, Duration::from_millis(hold));
            let end = Duration::from_millis(fade).as_secs_f64();
            assert_eq!(style.fade.sample(1.0, 0.0, 0.0, end / 2.0).value, 0.5);
            let sample = style.fade.sample(1.0, 0.0, 0.0, end);
            assert_eq!(sample.value, 0.0);
            assert!(sample.settled);
        }
    }

    #[test]
    fn pulldown_clamps_material_and_default_desktop() {
        assert_eq!(pulldown_style(Material, None), None);
        assert_eq!(pulldown_style(Material, Some(80.0)), None);
        assert_eq!(pulldown_style(Desktop, None), None);
        assert_eq!(pulldown_style(Cupertino, Some(0.0)), None);
        let (limit, tween) = pulldown_style(Desktop, Some(42.0)).unwrap();
        assert_eq!(limit, 42.0);
        assert!(matches!(tween, Spec::Tween(..)));
        let (limit, spring) = pulldown_style(Cupertino, None).unwrap();
        assert_eq!(limit, 80.0);
        assert!(matches!(spring, Spec::Spring { .. }));
        assert!(spring.sample(limit, 0.0, 0.0, 0.10).velocity < 0.0);
    }
}
