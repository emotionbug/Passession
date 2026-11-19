//! Semantic motion intents and the target platform's visual vocabulary.

use std::time::Duration;

use makepad_widgets::DVec2;

use super::{Spec, StyleFamily, ease, tokens};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    NavigateForward,
    NavigateBack,
    SwitchPeer,
    OpenPopover,
    ClosePopover,
    OpenSheet,
    CloseSheet,
    OpenDialog,
    CloseDialog,
    OpenExplorer,
    CloseExplorer,
    Hover,
    Press,
    Expand,
    Collapse,
    Appear,
    Disappear,
    Highlight,
    Reorder,
    ScrollTo,
}

#[derive(Clone, Copy)]
pub struct Style {
    pub spec: Spec,
    pub pattern: Pattern,
}

impl Intent {
    pub fn resolve(self) -> Style {
        self.resolve_for(StyleFamily::mobile_or_desktop())
    }

    pub fn resolve_for(self, family: StyleFamily) -> Style {
        use Intent::*;
        use Spec::Tween;
        use StyleFamily::*;
        use tokens::*;

        let (pattern, spec) = match self {
            NavigateForward | NavigateBack => {
                let back = self == NavigateBack;
                match family {
                    Material => (
                        Pattern::SharedAxis {
                            axis: Axis::X,
                            back,
                        },
                        Tween(MEDIUM_2, EMPHASIZED),
                    ),
                    Cupertino => (
                        Pattern::CupertinoPush { back },
                        Tween(if back { MEDIUM_1 } else { MEDIUM_2 }, STANDARD),
                    ),
                    Desktop => (
                        Pattern::DesktopNavigation { back },
                        Tween(if back { SHORT_4 } else { MEDIUM_1 }, STANDARD),
                    ),
                }
            }
            SwitchPeer => match family {
                Material => (Pattern::FadeThrough, Tween(MEDIUM_2, LINEAR)),
                Cupertino => (Pattern::Crossfade, Tween(SHORT_4, STANDARD)),
                Desktop => (Pattern::Crossfade, Tween(SHORT_3, STANDARD)),
            },
            OpenSheet | CloseSheet => match family {
                Material => (Pattern::Sheet, SPATIAL),
                Cupertino => (Pattern::Sheet, Tween(MEDIUM_2, STANDARD)),
                Desktop => (Pattern::Sheet, Tween(MEDIUM_1, STANDARD)),
            },
            OpenPopover | ClosePopover | OpenDialog | CloseDialog => {
                let close = matches!(self, ClosePopover | CloseDialog);
                match family {
                    Material => (
                        Pattern::FadeScale {
                            scale: 0.92,
                            offset: 0.0,
                        },
                        if close {
                            Tween(SHORT_3, EMPHASIZED_ACCELERATE)
                        } else {
                            Tween(MEDIUM_1, EMPHASIZED_DECELERATE)
                        },
                    ),
                    Cupertino => (
                        Pattern::FadeScale {
                            scale: 0.94,
                            offset: 0.0,
                        },
                        Tween(if close { SHORT_3 } else { SHORT_4 }, STANDARD),
                    ),
                    Desktop => (
                        Pattern::FadeScale {
                            scale: 0.98,
                            offset: 4.0,
                        },
                        Tween(if close { SHORT_2 } else { SHORT_3 }, STANDARD),
                    ),
                }
            }
            OpenExplorer | CloseExplorer => (
                Pattern::Explorer,
                Tween(Duration::from_millis(220), [0.25, 0.1, 0.25, 1.0]),
            ),
            Appear | Disappear => (
                Pattern::FadeScale {
                    scale: 1.0,
                    offset: 0.0,
                },
                match (family, self) {
                    (Material, Appear) => Tween(SHORT_4, EMPHASIZED_DECELERATE),
                    (Material, _) => Tween(SHORT_3, EMPHASIZED_ACCELERATE),
                    (Cupertino, _) => Tween(SHORT_4, STANDARD),
                    (Desktop, Appear) => Tween(SHORT_3, STANDARD),
                    (Desktop, _) => Tween(SHORT_2, STANDARD),
                },
            ),
            Hover | Press | Expand | Collapse | Highlight | Reorder | ScrollTo => (
                Pattern::Scalar,
                match (family, self) {
                    (Material, Hover | Press) => FAST_EFFECTS,
                    (Material, Expand | Collapse | Reorder) => FAST_SPATIAL,
                    (Material, ScrollTo) => SPATIAL,
                    (Material, Highlight) => Tween(MEDIUM_1, STANDARD_DECELERATE),
                    (Cupertino, Expand | Collapse | Reorder | ScrollTo) => SPATIAL,
                    (_, Press) => Tween(SHORT_1, STANDARD),
                    (_, Hover) => Tween(SHORT_2, STANDARD),
                    (_, Expand | Collapse) => Tween(SHORT_3, STANDARD),
                    (_, Reorder) => Tween(SHORT_4, STANDARD),
                    (_, ScrollTo) => Tween(MEDIUM_1, STANDARD),
                    (_, Highlight) => Tween(MEDIUM_2, STANDARD_DECELERATE),
                    _ => unreachable!(),
                },
            ),
        };
        Style { spec, pattern }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    SharedAxis { axis: Axis, back: bool },
    FadeThrough,
    Crossfade,
    FadeScale { scale: f64, offset: f64 },
    Sheet,
    CupertinoPush { back: bool },
    DesktopNavigation { back: bool },
    Explorer,
    Scalar,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visual {
    pub offset: DVec2,
    pub opacity: f64,
    pub scale: f64,
    pub edge_shadow: f64,
    pub dim: f64,
}

impl Default for Visual {
    fn default() -> Self {
        Self {
            offset: DVec2::default(),
            opacity: 1.0,
            scale: 1.0,
            edge_shadow: 0.0,
            dim: 0.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    pub incoming: Visual,
    pub outgoing: Visual,
    pub scrim: f64,
}

impl Style {
    pub fn sample(self, progress: f64, size: DVec2) -> Frame {
        self.pattern.sample(progress, size)
    }
}

impl Pattern {
    /// Navigation/peer progress is old → new; a single panel's progress is closed → open.
    pub fn sample(self, progress: f64, size: DVec2) -> Frame {
        let t = progress.clamp(0.0, 1.0);
        let remaining = 1.0 - t;
        let mut frame = Frame::default();
        match self {
            Self::SharedAxis { axis, back } => {
                let direction = if back { -1.0 } else { 1.0 };
                let incoming = 30.0 * remaining * direction;
                let outgoing = -30.0 * t * direction;
                match axis {
                    Axis::X => {
                        frame.incoming.offset.x = incoming;
                        frame.outgoing.offset.x = outgoing;
                    }
                    Axis::Y => {
                        frame.incoming.offset.y = incoming;
                        frame.outgoing.offset.y = outgoing;
                    }
                    Axis::Z => {
                        frame.incoming.scale = 1.0 - 0.12 * remaining * direction;
                        frame.outgoing.scale = 1.0 + 0.12 * t * direction;
                    }
                }
                frame.incoming.opacity = t;
                frame.outgoing.opacity = remaining;
            }
            Self::FadeThrough => {
                let incoming = ease(
                    ((t - 0.3) / 0.7).clamp(0.0, 1.0),
                    tokens::STANDARD_DECELERATE,
                );
                frame.outgoing.opacity = 1.0 - (t / 0.3).clamp(0.0, 1.0);
                frame.incoming.opacity = incoming;
                frame.incoming.scale = 0.92 + 0.08 * incoming;
            }
            Self::Crossfade => {
                frame.outgoing.opacity = remaining;
                frame.incoming.opacity = t;
            }
            Self::FadeScale { scale, offset } => {
                frame.incoming.opacity = t;
                frame.incoming.scale = 1.0 - (1.0 - scale) * remaining;
                frame.incoming.offset.y = offset * remaining;
                frame.scrim = t;
            }
            Self::Sheet => {
                frame.incoming.offset.y = size.y * (1.0 - progress);
                frame.scrim = ease(t, tokens::STANDARD_DECELERATE);
            }
            Self::CupertinoPush { back } => {
                if back {
                    frame.incoming.offset.x = -size.x * 0.3 * remaining;
                    frame.outgoing.offset.x = size.x * t;
                    frame.outgoing.edge_shadow = remaining;
                    frame.incoming.dim = 0.18 * remaining;
                } else {
                    frame.incoming.offset.x = size.x * remaining;
                    frame.incoming.edge_shadow = t;
                    frame.outgoing.offset.x = -size.x * 0.3 * t;
                    frame.outgoing.dim = 0.18 * t;
                }
            }
            Self::DesktopNavigation { back } => {
                let direction = if back { -1.0 } else { 1.0 };
                frame.incoming.offset.x = 16.0 * remaining * direction;
                frame.incoming.scale = 1.0 - 0.015 * remaining;
                frame.incoming.opacity = t;
                frame.outgoing.offset.x = -10.0 * t * direction;
                frame.outgoing.opacity = remaining;
            }
            Self::Explorer => {
                frame.incoming.offset.x = size.x * remaining;
                frame.scrim = t;
            }
            Self::Scalar => {}
        }
        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use makepad_widgets::dvec2;

    #[test]
    fn navigation_is_directional_and_resolves_by_family() {
        let size = dvec2(400.0, 800.0);
        let android = Intent::NavigateForward.resolve_for(StyleFamily::Material);
        assert_eq!(android.sample(0.0, size).incoming.offset.x, 30.0);
        let ios = Intent::NavigateForward.resolve_for(StyleFamily::Cupertino);
        assert_eq!(ios.sample(0.0, size).incoming.offset.x, 400.0);
        let back = Intent::NavigateBack.resolve_for(StyleFamily::Cupertino);
        assert_eq!(back.sample(0.0, size).incoming.offset.x, -120.0);
        assert_eq!(back.sample(1.0, size).outgoing.offset.x, 400.0);
        let desktop = Intent::NavigateForward.resolve_for(StyleFamily::Desktop);
        assert_eq!(desktop.sample(0.0, size).incoming.offset.x, 16.0);
        assert_eq!(desktop.sample(1.0, size).incoming, Visual::default());
    }

    #[test]
    fn peers_fade_through_and_close_preserves_open_geometry() {
        let size = dvec2(200.0, 300.0);
        let peer = Intent::SwitchPeer.resolve_for(StyleFamily::Material);
        let middle = peer.sample(0.3, size);
        assert_eq!(middle.outgoing.opacity, 0.0);
        assert_eq!(middle.incoming.opacity, 0.0);
        assert_eq!(peer.sample(1.0, size).incoming, Visual::default());
        let desktop = Intent::SwitchPeer
            .resolve_for(StyleFamily::Desktop)
            .sample(0.4, size);
        assert_eq!(desktop.outgoing.opacity, 0.6);
        assert_eq!(desktop.incoming.opacity, 0.4);
        for family in [
            StyleFamily::Material,
            StyleFamily::Cupertino,
            StyleFamily::Desktop,
        ] {
            for (open, close) in [
                (Intent::OpenSheet, Intent::CloseSheet),
                (Intent::OpenDialog, Intent::CloseDialog),
                (Intent::OpenPopover, Intent::ClosePopover),
            ] {
                assert_eq!(
                    open.resolve_for(family).sample(0.4, size),
                    close.resolve_for(family).sample(0.4, size)
                );
            }
        }
    }
}
