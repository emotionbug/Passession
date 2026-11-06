//! Per-draw styling snapshot: the runtime palette/typography and platform/layout facts the
//! composer geometry depends on (Flutter `_native`, `isCompactLayout`, narrow < 720).

use crate::common::UiContext;
use crate::theme::{Palette, Typography};
use makepad_widgets::*;

#[derive(Clone)]
pub struct Look {
    pub p: Palette,
    pub t: Typography,
    pub compact: bool,
    pub native: bool,
    pub ios: bool,
    /// Window narrower than 720 (Flutter `MediaQuery.sizeOf(context).width < 720`).
    pub narrow: bool,
    pub window: DVec2,
    /// `preferences.usage.displayAs == "remaining"` (usage cards show what is left).
    pub usage_remaining: bool,
}

impl Look {
    pub fn new(cx: &Cx, ui: &UiContext, window: DVec2) -> Self {
        let (native, ios) = match cx.os_type() {
            OsType::Ios => (true, true),
            OsType::Android(_) => (true, false),
            _ => (false, false),
        };
        Self {
            p: crate::theme::palette(ui),
            t: crate::theme::typography(ui),
            compact: ui.compact,
            native,
            ios,
            narrow: window.x > 0.0 && window.x < 720.0,
            window,
            usage_remaining: ui
                .preference("usage")
                .and_then(|usage| usage.get("displayAs"))
                .and_then(serde_json::Value::as_str)
                == Some("remaining"),
        }
    }

    /// Material bodyMedium / bodySmall at the user's UI scale.
    pub fn body(&self) -> f64 {
        self.t.ui_size(14.0)
    }

    pub fn small(&self) -> f64 {
        self.t.ui_size(12.0)
    }

    pub fn content(&self) -> f64 {
        self.t.content_font_size
    }

    /// index.tsx resolveComposerButtonIconSize: 20 on native, 16 on desktop.
    pub fn glyph(&self) -> f64 {
        if self.native { 20.0 } else { 16.0 }
    }

    pub fn red500(&self) -> Vec4 {
        vec4(0.937, 0.267, 0.267, 1.0)
    }

    pub fn red600(&self) -> Vec4 {
        vec4(0.863, 0.149, 0.149, 1.0)
    }

    pub fn amber500(&self) -> Vec4 {
        vec4(0.961, 0.620, 0.043, 1.0)
    }

    pub fn white(&self) -> Vec4 {
        vec4(1.0, 1.0, 1.0, 1.0)
    }

    pub fn transparent(&self) -> Vec4 {
        vec4(0.0, 0.0, 0.0, 0.0)
    }

    pub fn shadow(&self) -> Vec4 {
        if self.p.is_dark { vec4(0.0, 0.0, 0.0, 0.2) } else { vec4(0.0, 0.0, 0.0, 0.04) }
    }
}
