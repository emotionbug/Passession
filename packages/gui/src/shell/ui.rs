//! Retained-hit immediate-mode drawing for shell chrome, menus and sheets: every
//! interactive rect is pushed with a string id during draw; events resolve the id under the
//! pointer from the last frame (Flutter `InkWell`/`GestureDetector` regions).

use std::collections::HashMap;

use makepad_widgets::*;

use crate::common::paint::{Font, HitMap, Painter, TextSpec, inset, rect};
use crate::theme::{ControlSize, Palette, Typography, control_icon_size, fade};

/// Per-frame drawing context.
pub struct Ui<'a> {
    pub painter: &'a mut Painter,
    pub draw_icon: &'a mut DrawIcon,
    pub hits: &'a mut HitMap<String>,
    pub tooltips: &'a mut HashMap<String, String>,
    pub hover: Option<&'a str>,
    pub pressed: Option<&'a str>,
    pub p: Palette,
    pub t: Typography,
    pub compact: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    Primary,
    Secondary,
    Outline,
    Ghost,
    Destructive,
}

impl<'a> Ui<'a> {
    pub fn is_hover(&self, id: &str) -> bool {
        self.hover == Some(id)
    }

    pub fn is_pressed(&self, id: &str) -> bool {
        self.pressed == Some(id)
    }

    pub fn hit(&mut self, r: Rect, id: impl Into<String>) {
        self.hits.push(r, id.into());
    }

    pub fn hit_tip(&mut self, r: Rect, id: &str, tooltip: &str) {
        self.hits.push(r, id.to_string());
        if !tooltip.is_empty() && !self.compact {
            self.tooltips.insert(id.to_string(), tooltip.to_string());
        }
    }

    pub fn spec(&self, font: Font, authored: f64, color: Vec4) -> TextSpec {
        TextSpec::new(font, self.t.ui_size(authored), color)
    }

    pub fn body(&self) -> TextSpec {
        self.spec(Font::Regular, 14.0, self.p.foreground)
    }

    pub fn small_muted(&self) -> TextSpec {
        self.spec(Font::Regular, 12.0, self.p.muted)
    }

    pub fn fill(&mut self, cx: &mut Cx2d, r: Rect, color: Vec4, radius: f64) {
        self.painter.fill(cx, r, color, radius);
    }

    pub fn text(&mut self, cx: &mut Cx2d, r: Rect, spec: TextSpec, text: &str) -> Rect {
        self.painter.text_in(cx, r, spec, text, 0.0)
    }

    pub fn text_centered(&mut self, cx: &mut Cx2d, r: Rect, spec: TextSpec, text: &str) -> Rect {
        self.painter.text_in(cx, r, spec, text, 0.5)
    }

    pub fn text_right(&mut self, cx: &mut Cx2d, r: Rect, spec: TextSpec, text: &str) -> Rect {
        self.painter.text_in(cx, r, spec, text, 1.0)
    }

    pub fn measure(&mut self, cx: &mut Cx, spec: TextSpec, text: &str) -> f64 {
        self.painter.measure(cx, spec, text).x
    }

    pub fn icon(&mut self, cx: &mut Cx2d, r: Rect, name: &str, size: f64, color: Vec4) {
        self.painter.icon(cx, r, name, size, color);
    }

    /// Original `PaseoChromeButton` / icon-button chrome: `extent` square, `radius` corners,
    /// muted glyph, `interactionHighlight` while hovered/pressed.
    #[allow(clippy::too_many_arguments)]
    pub fn icon_button(&mut self, cx: &mut Cx2d, r: Rect, id: &str, icon: &str, icon_size: f64, radius: f64, tooltip: &str) {
        self.icon_button_with(cx, r, id, icon, icon_size, radius, tooltip, None, true);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn icon_button_with(
        &mut self,
        cx: &mut Cx2d,
        r: Rect,
        id: &str,
        icon: &str,
        icon_size: f64,
        radius: f64,
        tooltip: &str,
        color: Option<Vec4>,
        enabled: bool,
    ) {
        let active = enabled && (self.is_hover(id) || self.is_pressed(id));
        if active {
            let highlight = self.p.interaction_highlight;
            self.fill(cx, r, highlight, radius);
        }
        let color = color.unwrap_or(self.p.muted);
        self.icon(cx, r, icon, icon_size, if enabled { color } else { fade(color, 0.5) });
        if enabled {
            self.hit_tip(r, id, tooltip);
        }
    }

    /// Natural width of a [`button`](Self::button).
    pub fn button_width(&mut self, cx: &mut Cx, label: &str, icon: Option<&str>, size: ControlSize) -> f64 {
        let pad = match size {
            ControlSize::Xs | ControlSize::Sm => 12.0,
            ControlSize::Md => 16.0,
            ControlSize::Lg => 24.0,
        };
        let spec = self.spec(Font::Regular, if size == ControlSize::Xs { 12.0 } else { 14.0 }, self.p.foreground);
        let text = if label.is_empty() { 0.0 } else { self.measure(cx, spec, label) };
        let icon_w = if icon.is_some() { control_icon_size(size) + if label.is_empty() { 0.0 } else { 8.0 } } else { 0.0 };
        text + icon_w + pad * 2.0
    }

    /// Original `PaseoButton`.
    #[allow(clippy::too_many_arguments)]
    pub fn button(
        &mut self,
        cx: &mut Cx2d,
        r: Rect,
        id: &str,
        label: &str,
        icon: Option<&str>,
        variant: Variant,
        size: ControlSize,
        enabled: bool,
    ) {
        let p = self.p;
        let hovered = self.is_hover(id) && enabled;
        let (bg, border, fg) = match variant {
            Variant::Primary => (p.accent, p.accent, p.on_tertiary),
            Variant::Secondary => (p.surface3, p.surface3, p.foreground),
            Variant::Outline => (Vec4::default(), p.border_accent, p.foreground),
            Variant::Ghost => (Vec4::default(), Vec4::default(), if hovered { p.foreground } else { p.muted }),
            Variant::Destructive => (p.destructive, p.destructive, if p.is_dark { vec4(1.0, 1.0, 1.0, 1.0) } else { p.surface0 }),
        };
        let opacity = if !enabled { 0.5 } else if self.is_pressed(id) { 0.85 } else { 1.0 };
        let radius = match size {
            ControlSize::Xs | ControlSize::Sm => 12.0,
            _ => 16.0,
        }
        .min(r.size.y * 0.5);
        self.painter.outline(cx, r, fade(bg, opacity), fade(border, opacity), 1.0, radius);
        let fg = fade(fg, opacity);
        let spec = self.spec(Font::Regular, if size == ControlSize::Xs { 12.0 } else { 14.0 }, fg);
        let icon_size = control_icon_size(size);
        let text_w = if label.is_empty() { 0.0 } else { self.measure(cx, spec, label) };
        let content = text_w + if icon.is_some() { icon_size + if label.is_empty() { 0.0 } else { 8.0 } } else { 0.0 };
        let mut x = r.pos.x + ((r.size.x - content) * 0.5).max(8.0);
        if let Some(icon) = icon {
            self.icon(cx, rect(x, r.pos.y + (r.size.y - icon_size) * 0.5, icon_size, icon_size), icon, icon_size, fg);
            x += icon_size + 8.0;
        }
        if !label.is_empty() {
            let w = (r.pos.x + r.size.x - x - 8.0).max(0.0);
            self.text(cx, rect(x, r.pos.y, w, r.size.y), spec, label);
        }
        if enabled {
            self.hit(r, id);
        }
    }

    /// Original `MenuItem` row: 40/28 tall, 4px inset, 6px radius, 16px leading slot,
    /// surface2 when hovered/pressed/active, 0.5 disabled / 0.72 muted opacity.
    #[allow(clippy::too_many_arguments)]
    pub fn menu_row(
        &mut self,
        cx: &mut Cx2d,
        r: Rect,
        id: &str,
        label: &str,
        lead: RowLead,
        hint: Option<&str>,
        enabled: bool,
        muted: bool,
        destructive: bool,
        checked: bool,
        active: bool,
    ) {
        let p = self.p;
        let body = inset(r, 4.0, 0.0);
        if enabled && (self.is_hover(id) || self.is_pressed(id) || active) {
            self.fill(cx, body, p.surface2, 6.0);
        }
        let muted = muted && enabled;
        let opacity = if !enabled { 0.5 } else if muted { 0.72 } else { 1.0 };
        let fg = fade(if muted { p.muted } else if destructive { p.danger } else { p.foreground }, opacity);
        let mut x = body.pos.x + 8.0;
        let mid = body.pos.y + body.size.y * 0.5;
        let glyph_color = fade(if destructive { p.danger } else { p.muted }, opacity);
        match lead {
            RowLead::None => {}
            RowLead::Dot(color) => {
                self.painter.dot(cx, dvec2(x + 8.0, mid), 8.0, fade(color, opacity));
                x += 24.0;
            }
            RowLead::Glyph(icon) => {
                self.icon(cx, rect(x, mid - 8.0, 16.0, 16.0), icon, 16.0, glyph_color);
                x += 24.0;
            }
            RowLead::Svg(svg) => {
                crate::common::provider_icon::ProviderIcon::paint(self.painter, self.draw_icon, cx, rect(x, mid - 8.0, 16.0, 16.0), "", Some(svg), glyph_color);
                x += 24.0;
            }
        }
        let mut right = body.pos.x + body.size.x - 8.0;
        if checked {
            self.icon(cx, rect(right - 16.0, mid - 8.0, 16.0, 16.0), "check", 16.0, fade(p.muted, opacity));
            right -= 24.0;
        }
        if let Some(hint) = hint.filter(|h| !h.is_empty()) {
            let spec = self.spec(Font::Regular, 12.0, fade(p.muted, opacity));
            let w = self.measure(cx, spec, hint).min(body.size.x * 0.45);
            self.text(cx, rect(right - w, body.pos.y, w, body.size.y), spec, hint);
            right -= w + 8.0;
        }
        let spec = self.spec(Font::Regular, 14.0, fg);
        self.text(cx, rect(x, body.pos.y, (right - x).max(0.0), body.size.y), spec, label);
        if enabled {
            self.hit(r, id);
        }
    }

    /// Original `MenuSeparator`: 1px borderAccent with 4px above and below (9px tall).
    pub fn menu_separator(&mut self, cx: &mut Cx2d, x: f64, y: f64, width: f64) {
        let color = self.p.border_accent;
        self.painter.hline(cx, x, y + 4.0, width, color);
    }

    /// The 36x4 surface2 drag handle of bottom sheets.
    pub fn sheet_handle(&mut self, cx: &mut Cx2d, center_x: f64, y: f64) {
        let color = self.p.surface2;
        self.fill(cx, rect(center_x - 18.0, y + 10.0, 36.0, 4.0), color, 2.0);
    }

    /// 8px status dot.
    pub fn dot(&mut self, cx: &mut Cx2d, center: DVec2, diameter: f64, color: Vec4) {
        self.painter.dot(cx, center, diameter, color);
    }
}

/// The leading 16px slot of a [`Ui::menu_row`].
#[derive(Clone, Copy, Debug)]
pub enum RowLead<'a> {
    None,
    /// A Lucide glyph by name.
    Glyph(&'a str),
    /// An SVG document filled with the glyph colour (brand marks).
    Svg(&'a str),
    /// An 8px status dot.
    Dot(Vec4),
}

/// Hover/press bookkeeping for a widget that owns one capture area.
#[derive(Default)]
pub struct Pointer {
    pub hover: Option<String>,
    pub pressed: Option<String>,
    pub down_pos: Option<DVec2>,
    pub dragging: bool,
}

/// What a pointer event resolved to.
#[derive(Clone, Debug)]
pub enum Press {
    /// Primary click released over the same item.
    Click(String, KeyModifiers),
    /// Right click (desktop) or long press (touch) on an item, with the pointer position.
    Secondary(String, DVec2),
}

impl Pointer {
    /// Resolves hover and clicks on `area` against `hits`. Returns redraw-needed and an
    /// optional press.
    pub fn handle(&mut self, cx: &mut Cx, event: &Event, area: Area, hits: &HitMap<String>) -> (bool, Option<Press>) {
        let hit = event.hits(cx, area);
        self.handle_hit(cx, hit, hits)
    }

    /// [`Pointer::handle`] for a hit the caller already resolved (the overlay layer hits its
    /// area through its own sweep lock).
    pub fn handle_hit(&mut self, cx: &mut Cx, hit: Hit, hits: &HitMap<String>) -> (bool, Option<Press>) {
        let mut redraw = false;
        let mut press = None;
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hover = hits.at(e.abs);
                cx.set_cursor(if hover.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
                if hover != self.hover {
                    self.hover = hover;
                    redraw = true;
                }
            }
            Hit::FingerHoverOut(_) => {
                if self.hover.take().is_some() {
                    redraw = true;
                }
            }
            Hit::FingerDown(fe) => {
                let id = hits.at(fe.abs);
                if fe.mouse_button().is_some_and(|b| b.is_secondary()) {
                    if let Some(id) = id {
                        press = Some(Press::Secondary(id, fe.abs));
                    }
                } else {
                    self.pressed = id;
                    self.down_pos = Some(fe.abs);
                    redraw = true;
                }
            }
            Hit::FingerLongPress(lp) => {
                if let Some(id) = self.pressed.take() {
                    press = Some(Press::Secondary(id, lp.abs));
                    redraw = true;
                }
            }
            Hit::FingerUp(fe) => {
                let pressed = self.pressed.take();
                self.down_pos = None;
                if let Some(id) = pressed
                    && fe.is_over
                    && !fe.has_long_press_occurred
                    && (!fe.device.is_touch() || fe.was_tap())
                    && hits.at(fe.abs).as_deref() == Some(id.as_str())
                {
                    press = Some(Press::Click(id, fe.modifiers));
                }
                if !fe.device.has_hovers() {
                    self.hover = None;
                }
                redraw = true;
            }
            _ => {}
        }
        (redraw, press)
    }
}
