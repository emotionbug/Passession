//! Immediate-mode layout for the settings surface. Pages describe sections,
//! cards, rows and controls top-down; `Ui` measures text with Makepad's text
//! layouter, draws native quads/text/icons and registers hit zones whose
//! handlers the panel runs on tap. Geometry follows the original
//! `styles/settings.ts` metrics (16px rows, 8px card radius, 24px section gap).

use std::hash::{Hash, Hasher};

use makepad_widgets::*;

use super::draw::{Draws, Font, Icon};
use super::field::{FieldHost, FieldSpec, Layer};
use super::theme::{Palette, alpha};
use crate::features::settings::rpc::{Handler, handler};

pub fn key(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// A tappable rectangle registered during draw.
pub struct Zone {
    pub key: u64,
    pub rect: Rect,
    pub on: Option<Handler>,
    /// Hover-only zones (rows with no action) still highlight.
    pub layer: Layer,
    /// Tooltip shown while hovered: text and whether it waits the theme's
    /// 500 ms (buttons) or shows at once (info glyphs, `waitDuration: zero`).
    pub tip: Option<(String, bool)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Variant {
    Primary,
    Secondary,
    Outline,
    Ghost,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BSize {
    Xs,
    Sm,
    Md,
}

impl BSize {
    fn height(self) -> f64 {
        match self {
            BSize::Xs => 28.0,
            BSize::Sm => 32.0,
            BSize::Md => 40.0,
        }
    }
    fn icon(self) -> f64 {
        match self {
            BSize::Xs => 12.0,
            BSize::Sm => 14.0,
            BSize::Md => 16.0,
        }
    }
    fn font(self) -> f64 {
        if self == BSize::Xs { 12.0 } else { 14.0 }
    }
    fn padding(self) -> f64 {
        if self == BSize::Md { 16.0 } else { 12.0 }
    }
}

pub struct Button {
    pub key: String,
    pub label: String,
    pub icon: Option<Icon>,
    pub variant: Variant,
    pub size: BSize,
    pub enabled: bool,
    pub loading: bool,
    pub expand: bool,
    pub danger_text: bool,
    pub act: Option<Handler>,
    /// Flutter `PaseoButton(tooltip:)`, shown on hover after 500 ms.
    pub tip: Option<String>,
}

pub fn button(key: impl Into<String>, label: impl Into<String>, act: Option<Handler>) -> Button {
    Button {
        key: key.into(),
        label: label.into(),
        icon: None,
        variant: Variant::Outline,
        size: BSize::Sm,
        enabled: act.is_some(),
        loading: false,
        expand: false,
        danger_text: false,
        act,
        tip: None,
    }
}

impl Button {
    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }
    pub fn size(mut self, size: BSize) -> Self {
        self.size = size;
        self
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }
    pub fn tip(mut self, text: impl Into<String>) -> Self {
        self.tip = Some(text.into());
        self
    }
    pub fn danger_text(mut self) -> Self {
        self.danger_text = true;
        self
    }
    pub fn ctl(self) -> Ctl {
        Ctl::Button(self)
    }
}

/// Leading decoration for dropdown triggers and menu items.
#[derive(Clone, Debug)]
pub enum Lead {
    /// Flutter `ProviderIcon(provider:, size: 16, color: muted)` (Bot when the
    /// provider has no glyph).
    Provider(String),
    Icon(Icon, Option<Vec4>),
    Dot(Vec4),
    Swatch(Option<Vec4>),
    Monogram(char),
}

pub enum Ctl {
    Button(Button),
    Switch { key: String, on: bool, act: Option<Handler> },
    /// `tip`: Flutter `tooltip:` label, shown on hover after 500 ms.
    IconButton { key: String, icon: Icon, act: Option<Handler>, destructive: bool, size: f64, tip: Option<String> },
    Dropdown { key: String, label: String, lead: Option<Lead>, act: Option<Handler>, max_w: f64 },
    Segmented { key: String, items: Vec<(String, bool, Option<Handler>)> },
    Checkbox { key: String, on: bool, act: Option<Handler> },
    Label { text: String, color: Vec4, size: f64, font: Font, max_w: f64 },
    Badge { label: String, tone: Option<Vec4>, xs: bool, dot: Option<Vec4>, icon: Option<Icon> },
    Spinner(f64),
    Dot(Vec4, f64),
    Glyph(Icon, f64, Vec4),
    /// Provider glyph: provider id, host snapshot SVG, size, colour.
    Provider(String, Option<String>, f64, Vec4),
    Lead(Lead, f64),
    Keys(Vec<String>),
    Link { key: String, label: String, url: String, leading: bool },
    Field { spec: FieldSpec, w: f64, h: f64 },
    Gap(f64),
    Row(Vec<Ctl>, f64),
    Col(Vec<Ctl>, f64, bool),
}

pub fn label(text: impl Into<String>, color: Vec4) -> Ctl {
    Ctl::Label { text: text.into(), color, size: 14.0, font: Font::Regular, max_w: 320.0 }
}

pub fn small(text: impl Into<String>, color: Vec4) -> Ctl {
    Ctl::Label { text: text.into(), color, size: 12.0, font: Font::Regular, max_w: 320.0 }
}

pub fn icon_button(key: impl Into<String>, icon: Icon, act: Option<Handler>) -> Ctl {
    Ctl::IconButton { key: key.into(), icon, act, destructive: false, size: 32.0, tip: None }
}

pub fn danger_icon_button(key: impl Into<String>, icon: Icon, act: Option<Handler>) -> Ctl {
    Ctl::IconButton { key: key.into(), icon, act, destructive: true, size: 32.0, tip: None }
}

impl Ctl {
    /// Adds the Flutter `tooltip:` label to an icon button.
    pub fn tipped(self, text: impl Into<String>) -> Ctl {
        match self {
            Ctl::IconButton { key, icon, act, destructive, size, .. } => Ctl::IconButton { key, icon, act, destructive, size, tip: Some(text.into()) },
            other => other,
        }
    }
}

pub fn switch(key: impl Into<String>, on: bool, act: Option<Handler>) -> Ctl {
    Ctl::Switch { key: key.into(), on, act }
}

/// One settings row (Flutter `SettingsRow`).
pub struct Row {
    pub label: String,
    pub hint: Option<String>,
    pub error: Option<String>,
    pub accessory: Option<Ctl>,
    pub leading: Option<Ctl>,
    pub control: Option<Ctl>,
    pub on_tap: Option<(String, Handler)>,
    pub inline: bool,
    pub label_lines_one: bool,
    pub muted_label: bool,
    pub padding: f64,
}

pub fn row(label: impl Into<String>) -> Row {
    Row {
        label: label.into(),
        hint: None,
        error: None,
        accessory: None,
        leading: None,
        control: None,
        on_tap: None,
        inline: false,
        label_lines_one: false,
        muted_label: false,
        padding: 16.0,
    }
}

impl Row {
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        let hint = hint.into();
        self.hint = if hint.is_empty() { None } else { Some(hint) };
        self
    }
    pub fn hint_opt(mut self, hint: Option<String>) -> Self {
        self.hint = hint.filter(|h| !h.is_empty());
        self
    }
    pub fn error(mut self, error: Option<String>) -> Self {
        self.error = error.filter(|e| !e.is_empty());
        self
    }
    pub fn control(mut self, control: Ctl) -> Self {
        self.control = Some(control);
        self
    }
    pub fn leading(mut self, leading: Ctl) -> Self {
        self.leading = Some(leading);
        self
    }
    pub fn accessory(mut self, accessory: Ctl) -> Self {
        self.accessory = Some(accessory);
        self
    }
    pub fn tap(mut self, key: impl Into<String>, on: Handler) -> Self {
        self.on_tap = Some((key.into(), on));
        self
    }
    pub fn inline(mut self) -> Self {
        self.inline = true;
        self
    }
    pub fn one_line(mut self) -> Self {
        self.label_lines_one = true;
        self
    }
    pub fn padding(mut self, padding: f64) -> Self {
        self.padding = padding;
        self
    }
}

struct Card {
    area: Area,
    x: f64,
    y: f64,
    w: f64,
    rows: usize,
}

pub struct Ui<'u, 'a, 'b> {
    pub cx: &'u mut Cx2d<'a, 'b>,
    pub d: &'u mut Draws,
    pub zones: &'u mut Vec<Zone>,
    pub fields: &'u mut FieldHost,
    pub layer: Layer,
    pub pal: Palette,
    pub hovered: Option<u64>,
    pub pressed: Option<u64>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub compact: bool,
    /// Set when a spinner was drawn: the panel requests the next frame.
    pub animating: bool,
    /// Overlay stacking depth of what is drawn (0 page, 1.. overlay layers).
    pub depth: usize,
    /// Alerts drawn so far this frame (stable keys for their selectable titles).
    alert_seq: u32,
    card: Option<Card>,
    clip: Option<Rect>,
    typography: crate::theme::Typography,
}

impl<'u, 'a, 'b> Ui<'u, 'a, 'b> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &'u mut Cx2d<'a, 'b>,
        d: &'u mut Draws,
        zones: &'u mut Vec<Zone>,
        fields: &'u mut FieldHost,
        layer: Layer,
        pal: Palette,
        hovered: Option<u64>,
        pressed: Option<u64>,
        compact: bool,
    ) -> Self {
        Self { cx, d, zones, fields, layer, pal, hovered, pressed, x: 0.0, y: 0.0, w: 0.0, compact, animating: false, depth: 0, alert_seq: 0, card: None, clip: None, typography: crate::theme::current_typography() }
    }

    // ---------------------------------------------------------------- primitives

    pub fn rect(&mut self, r: Rect, fill: Vec4, border: Option<Vec4>, radius: f64) -> Area {
        let d = &mut self.d.rect;
        match border {
            Some(border) => d.paint_bordered(self.cx, r, fill, border, 1.0, radius),
            None => d.paint(self.cx, r, fill, radius),
        }
        d.draw_vars.area
    }

    /// Top corners rounded, bottom square (compact bottom sheets): a rounded
    /// box plus a square box over its lower part. Only for opaque fills.
    pub fn rect_top(&mut self, r: Rect, fill: Vec4, radius: f64) {
        let d = &mut self.d.rect;
        d.paint(self.cx, r, fill, radius);
        if radius > 0.0 && r.size.y > radius {
            let lower = Rect { pos: dvec2(r.pos.x, r.pos.y + radius), size: dvec2(r.size.x, r.size.y - radius) };
            d.paint(self.cx, lower, fill, 0.0);
        }
    }



    pub fn hline(&mut self, x: f64, y: f64, w: f64) {
        let color = self.pal.border;
        self.rect(Rect { pos: dvec2(x, y), size: dvec2(w, 1.0) }, color, None, 0.0);
    }

    /// A Lucide glyph of `size` at `pos` (spinners are a 3/4 ring).
    pub fn icon(&mut self, icon: Icon, pos: DVec2, size: f64, color: Vec4) {
        let rect = Rect { pos, size: dvec2(size, size) };
        if icon == Icon::Spinner {
            // Flutter `SettingsSpinner`: indeterminate CircularProgressIndicator, 1.5px stroke.
            let (start, sweep) = spinner_arc(spinner_clock());
            self.d.painter.ring(self.cx, rect, color, 1.5, sweep, start);
            self.animating = true;
            return;
        }
        self.d.painter.icon(self.cx, rect, icon.name(), size, color);
    }

    /// Provider glyph (built-in, host snapshot SVG, vendored catalog, else Bot).
    pub fn provider_glyph(&mut self, provider: &str, snapshot_svg: Option<&str>, pos: DVec2, size: f64, color: Vec4) {
        let rect = Rect { pos, size: dvec2(size, size) };
        crate::common::provider_icon::ProviderIcon::paint(&mut self.d.painter, &mut self.d.provider_icon, self.cx, rect, provider, snapshot_svg, color);
    }

    /// Measures `text` wrapped at `max_w` (unwrapped when `None`).
    pub fn measure(&mut self, font: Font, size: f64, text: &str, max_w: Option<f64>) -> DVec2 {
        let size = self.typography.ui_size(size);
        if text.is_empty() {
            return dvec2(0.0, (size * 1.35).round());
        }
        let cx: &mut Cx = &mut *self.cx;
        let d = match font {
            Font::Regular => &mut self.d.text,
            Font::Medium => &mut self.d.medium,
            Font::Code => &mut self.d.code,
        };
        d.text_style.font_size = (size * 0.75) as f32;
        let laid = d.layout(cx, 0.0, 0.0, max_w.map(|w| w.max(1.0) as f32), max_w.is_some(), Align::default(), text);
        let scale = d.font_scale as f64;
        dvec2(laid.size_in_lpxs.width as f64 * scale, laid.size_in_lpxs.height as f64 * scale)
    }

    /// Draws `text` at `pos`, wrapped at `max_w`; returns its size.
    pub fn text(&mut self, font: Font, size: f64, color: Vec4, pos: DVec2, max_w: Option<f64>, text: &str) -> DVec2 {
        if text.is_empty() {
            return self.measure(font, size, text, max_w);
        }
        let measured = self.measure(font, size, text, max_w);
        let cx: &mut Cx = &mut *self.cx;
        let d = match font {
            Font::Regular => &mut self.d.text,
            Font::Medium => &mut self.d.medium,
            Font::Code => &mut self.d.code,
        };
        d.text_style.font_size = (self.typography.ui_size(size) * 0.75) as f32;
        d.color = color;
        let laid = d.layout(cx, 0.0, 0.0, max_w.map(|w| w.max(1.0) as f32), max_w.is_some(), Align::default(), text);
        d.draw_walk_laidout(self.cx, Walk::fixed(measured.x.max(1.0), measured.y.max(1.0)).with_abs_pos(pos), &laid);
        measured
    }

    /// Flutter `SelectableText`: `text` laid out like [`Ui::text`] in a
    /// read-only native text widget, so it can be selected and copied.
    /// `key` identifies the widget across frames.
    #[allow(clippy::too_many_arguments)]
    pub fn selectable(&mut self, key: &str, font: Font, size: f64, color: Vec4, pos: DVec2, max_w: Option<f64>, text: &str) -> DVec2 {
        if text.is_empty() {
            return self.measure(font, size, text, max_w);
        }
        let measured = self.measure(font, size, text, max_w);
        let spec = FieldSpec::selectable(format!("sel.{key}"), text, font, size, color);
        let pal = self.pal;
        let layer = self.layer;
        // One extra pixel so the widget wraps exactly where the measurement did.
        self.fields.draw(self.cx, &pal, (layer, self.depth), spec, Rect { pos, size: dvec2(measured.x + 1.0, measured.y) });
        measured
    }

    /// [`Ui::para`] as selectable text.
    pub fn selectable_para(&mut self, key: &str, text: &str, size: f64, color: Vec4) {
        let s = self.selectable(key, Font::Regular, size, color, dvec2(self.x, self.y), Some(self.w), text);
        self.y += s.y;
    }

    /// Single-line text truncated with an ellipsis to `max_w`.
    pub fn text_line(&mut self, font: Font, size: f64, color: Vec4, pos: DVec2, max_w: f64, text: &str) -> DVec2 {
        let full = self.measure(font, size, text, None);
        if full.x <= max_w || max_w <= 0.0 {
            return self.text(font, size, color, pos, None, text);
        }
        let chars: Vec<char> = text.chars().collect();
        let (mut lo, mut hi) = (0usize, chars.len());
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            let candidate: String = chars[..mid].iter().collect::<String>() + "…";
            if self.measure(font, size, &candidate, None).x <= max_w { lo = mid } else { hi = mid - 1 }
        }
        let clipped: String = chars[..lo].iter().collect::<String>() + "…";
        self.text(font, size, color, pos, None, &clipped)
    }

    // ---------------------------------------------------------------- zones

    pub fn state(&self, key_text: &str) -> (bool, bool) {
        let k = key(key_text);
        (self.hovered == Some(k), self.pressed == Some(k))
    }

    /// Registers a tappable rectangle (clipped to the current scroll region);
    /// `on: None` registers hover only.
    pub fn zone(&mut self, key_text: &str, rect: Rect, on: Option<Handler>) {
        let rect = match self.clip {
            Some(clip) => {
                let x0 = rect.pos.x.max(clip.pos.x);
                let y0 = rect.pos.y.max(clip.pos.y);
                let x1 = (rect.pos.x + rect.size.x).min(clip.pos.x + clip.size.x);
                let y1 = (rect.pos.y + rect.size.y).min(clip.pos.y + clip.size.y);
                if x1 <= x0 || y1 <= y0 {
                    return;
                }
                Rect { pos: dvec2(x0, y0), size: dvec2(x1 - x0, y1 - y0) }
            }
            None => rect,
        };
        self.zones.push(Zone { key: key(key_text), rect, on, layer: self.layer, tip: None });
    }

    /// Flutter `Tooltip(triggerMode: tap, waitDuration: zero)`: hover shows
    /// `text` at once (panel hover handling), a tap shows it for 1.5 s.
    pub fn tip_zone(&mut self, key_text: &str, rect: Rect, text: String) {
        let shown = text.clone();
        let on = handler(move |s, _| {
            s.tip = Some(crate::features::settings::state::Tip { text: shown.clone(), anchor: s.anchor, tapped: true });
        });
        self.zone(key_text, rect, Some(on));
        self.set_last_tip(key_text, text, false);
    }

    /// [`Ui::set_last_tip`] with the 500 ms button wait (custom-drawn buttons).
    pub fn set_last_tip_pub(&mut self, key_text: &str, text: String) {
        self.set_last_tip(key_text, text, true);
    }

    /// Attaches a tooltip to the zone just registered for `key_text` (none
    /// when the zone was clipped away).
    fn set_last_tip(&mut self, key_text: &str, text: String, wait: bool) {
        let k = key(key_text);
        if let Some(zone) = self.zones.last_mut()
            && zone.key == k
        {
            zone.tip = Some((text, wait));
        }
    }

    pub fn set_clip(&mut self, clip: Option<Rect>) {
        self.clip = clip;
    }

    // ---------------------------------------------------------------- controls

    pub fn measure_ctl(&mut self, c: &Ctl) -> DVec2 {
        match c {
            Ctl::Button(b) => {
                let text = if b.label.is_empty() { DVec2::default() } else { self.measure(Font::Regular, b.size.font(), &b.label, None) };
                let icon = if b.icon.is_some() || b.loading { b.size.icon() } else { 0.0 };
                let gap = if text.x > 0.0 && icon > 0.0 { 8.0 } else { 0.0 };
                let pad = if b.label.is_empty() { (b.size.height() - icon) / 2.0 } else { b.size.padding() };
                dvec2(text.x + icon + gap + pad * 2.0, b.size.height().max(text.y + 8.0))
            }
            Ctl::Switch { .. } => dvec2(36.0, 20.0),
            Ctl::IconButton { size, .. } => dvec2(*size, *size),
            Ctl::Checkbox { .. } => dvec2(22.0, 22.0),
            Ctl::Dropdown { label, lead, max_w, .. } => {
                let text = self.measure(Font::Regular, 14.0, label, None);
                let lead_w = if lead.is_some() { 16.0 + 8.0 } else { 0.0 };
                dvec2(12.0 + lead_w + text.x.min(*max_w) + 8.0 + 14.0 + 12.0, 32.0_f64.max(text.y + 8.0))
            }
            Ctl::Segmented { items, .. } => {
                let mut w = 4.0;
                let mut h = 28.0_f64;
                for (text, _, _) in items {
                    let text = self.measure(Font::Regular, 14.0, text, None);
                    w += text.x + 16.0;
                    h = h.max(text.y + 8.0);
                }
                dvec2(w, h)
            }
            Ctl::Label { text, size, font, max_w, .. } => {
                let m = self.measure(*font, *size, text, Some(*max_w));
                dvec2(m.x.min(*max_w), m.y)
            }
            Ctl::Badge { label, xs, dot, icon, .. } => {
                let text = self.measure(Font::Regular, 12.0, label, None);
                let extra = if dot.is_some() { 12.0 } else { 0.0 } + if icon.is_some() { 20.0 } else { 0.0 };
                let (minimum, padding): (f64, f64) = if *xs { (18.0, 4.0) } else { (22.0, 8.0) };
                dvec2(text.x.min(184.0) + extra + if *xs { 12.0 } else { 16.0 }, minimum.max(text.y + padding))
            }
            Ctl::Spinner(size) | Ctl::Lead(_, size) => dvec2(*size, *size),
            Ctl::Dot(_, size) => dvec2(*size, *size),
            Ctl::Glyph(_, size, _) => dvec2(*size, *size),
            Ctl::Provider(_, _, size, _) => dvec2(*size, *size),
            Ctl::Keys(steps) => {
                let mut w = 0.0;
                let mut h = 20.0_f64;
                for (i, step) in steps.iter().enumerate() {
                    let text = self.measure(Font::Regular, 12.0, step, None);
                    w += text.x + 8.0 + if i > 0 { 4.0 } else { 0.0 };
                    h = h.max(text.y + 4.0);
                }
                dvec2(w, h)
            }
            Ctl::Link { label, .. } => {
                let text = self.measure(Font::Regular, 12.0, label, None);
                dvec2(text.x + 18.0, 16.0_f64.max(text.y))
            }
            Ctl::Field { spec, w, h } => {
                let font = spec.plain.map(|plain| plain.font).unwrap_or(Font::Regular);
                let padding = if spec.bare || spec.plain.is_some() { 0.0 } else { 18.0 };
                let minimum = self.measure(font, spec.font_px, "Ag", None).y + padding;
                dvec2(*w, h.max(minimum))
            }
            Ctl::Gap(w) => dvec2(*w, 0.0),
            Ctl::Row(items, gap) => {
                let mut w = 0.0f64;
                let mut h = 0.0f64;
                for (i, item) in items.iter().enumerate() {
                    let s = self.measure_ctl(item);
                    w += s.x + if i > 0 { *gap } else { 0.0 };
                    h = h.max(s.y);
                }
                dvec2(w, h)
            }
            Ctl::Col(items, gap, _) => {
                let mut w = 0.0f64;
                let mut h = 0.0f64;
                for (i, item) in items.iter().enumerate() {
                    let s = self.measure_ctl(item);
                    h += s.y + if i > 0 { *gap } else { 0.0 };
                    w = w.max(s.x);
                }
                dvec2(w, h)
            }
        }
    }

    pub fn lead(&mut self, lead: Lead, pos: DVec2, size: f64) {
        match lead {
            Lead::Provider(provider) => {
                let muted = self.pal.muted;
                self.provider_glyph(&provider, None, pos, size, muted);
            }
            Lead::Icon(icon, color) => {
                let color = color.unwrap_or(self.pal.muted);
                self.icon(icon, pos, size, color);
            }
            Lead::Dot(color) => {
                let d = (size * 0.5).min(8.0);
                self.rect(Rect { pos: pos + dvec2((size - d) / 2.0, (size - d) / 2.0), size: dvec2(d, d) }, color, None, d / 2.0);
            }
            Lead::Swatch(color) => {
                let border = self.pal.border;
                let fill = color.unwrap_or(self.pal.transparent);
                self.rect(Rect { pos, size: dvec2(size, size) }, fill, Some(border), size / 2.0);
            }
            Lead::Monogram(c) => {
                let muted = self.pal.muted;
                self.rect(Rect { pos, size: dvec2(size, size) }, self.pal.transparent, Some(muted), 4.0);
                let text = c.to_uppercase().to_string();
                let m = self.measure(Font::Medium, size * 0.62, &text, None);
                self.text(Font::Medium, size * 0.62, muted, pos + dvec2((size - m.x) / 2.0, (size - m.y) / 2.0), None, &text);
            }
        }
    }

    fn button_colors(&self, b: &Button, hovered: bool) -> (Vec4, Option<Vec4>, Vec4) {
        let p = &self.pal;
        let (bg, border, fg) = match b.variant {
            Variant::Primary => (p.accent, Some(p.accent), p.on_accent),
            Variant::Secondary => (p.surface3, Some(p.surface3), p.foreground),
            Variant::Outline => (if hovered { p.interaction } else { p.transparent }, Some(p.border_accent), p.foreground),
            Variant::Ghost => (
                if hovered { p.interaction } else { p.transparent },
                None,
                if hovered && b.enabled { p.foreground } else { p.muted },
            ),
        };
        let fg = if b.danger_text { p.danger } else { fg };
        (bg, border, fg)
    }

    /// Draws `c` with its top-left at `pos`; returns its size.
    pub fn draw_ctl(&mut self, c: Ctl, pos: DVec2) -> DVec2 {
        let size = self.measure_ctl(&c);
        match c {
            Ctl::Button(b) => {
                let (hovered, pressed) = self.state(&b.key);
                let enabled = b.enabled && !b.loading && b.act.is_some();
                let (mut bg, border, mut fg) = self.button_colors(&b, hovered && enabled);
                let fade = if !enabled { 0.5 } else if pressed { 0.85 } else { 1.0 };
                bg = alpha(bg, fade);
                fg = alpha(fg, fade);
                let rect = Rect { pos, size };
                let radius = if b.size == BSize::Md { 16.0 } else { 12.0 };
                self.rect(rect, bg, border.map(|c| alpha(c, fade)), radius);
                let icon_size = b.size.icon();
                let text_w = if b.label.is_empty() { 0.0 } else { self.measure(Font::Regular, b.size.font(), &b.label, None).x };
                let has_icon = b.icon.is_some() || b.loading;
                let content = text_w + if has_icon { icon_size } else { 0.0 } + if has_icon && text_w > 0.0 { 8.0 } else { 0.0 };
                let mut x = pos.x + (size.x - content) / 2.0;
                if has_icon {
                    let icon = if b.loading { Icon::Spinner } else { b.icon.unwrap_or(Icon::Circle) };
                    self.icon(icon, dvec2(x, pos.y + (size.y - icon_size) / 2.0), icon_size, fg);
                    x += icon_size + if text_w > 0.0 { 8.0 } else { 0.0 };
                }
                if text_w > 0.0 {
                    let th = self.measure(Font::Regular, b.size.font(), &b.label, None).y;
                    self.text(Font::Regular, b.size.font(), fg, dvec2(x, pos.y + (size.y - th) / 2.0), None, &b.label);
                }
                if enabled || b.tip.is_some() {
                    self.zone(&b.key, rect, if enabled { b.act } else { None });
                }
                if let Some(text) = b.tip {
                    self.set_last_tip(&b.key, text, true);
                }
            }
            Ctl::Switch { key: k, on, act } => {
                let enabled = act.is_some();
                let fade = if enabled { 1.0 } else { 0.5 };
                let track = if on { self.pal.accent } else { self.pal.surface3 };
                let rect = Rect { pos, size };
                self.rect(rect, alpha(track, fade), None, 10.0);
                let knob_x = if on { pos.x + 18.0 } else { pos.x + 2.0 };
                let knob = if on { self.pal.on_accent } else { vec4(1.0, 1.0, 1.0, 1.0) };
                self.rect(Rect { pos: dvec2(knob_x, pos.y + 2.0), size: dvec2(16.0, 16.0) }, alpha(knob, fade), None, 8.0);
                if enabled {
                    self.zone(&k, Rect { pos: pos - dvec2(4.0, 6.0), size: size + dvec2(8.0, 12.0) }, act);
                }
            }
            Ctl::IconButton { key: k, icon, act, destructive, size: s, tip } => {
                let (hovered, pressed) = self.state(&k);
                let enabled = act.is_some();
                let rect = Rect { pos, size };
                if enabled && (hovered || pressed) {
                    let fill = if pressed { self.pal.surface3 } else { self.pal.interaction };
                    self.rect(rect, fill, None, 8.0);
                }
                let color = if destructive { self.pal.danger } else if hovered && enabled { self.pal.foreground } else { self.pal.muted };
                let glyph = if s >= 32.0 { 14.0 } else { 12.0 };
                self.icon(icon, pos + dvec2((s - glyph) / 2.0, (s - glyph) / 2.0), glyph, alpha(color, if enabled { 1.0 } else { 0.5 }));
                if enabled || tip.is_some() {
                    self.zone(&k, rect, act);
                }
                // Flutter `PaseoButton(tooltip:)`: a Material tooltip after the 500 ms theme wait.
                if let Some(text) = tip {
                    self.set_last_tip(&k, text, true);
                }
            }
            Ctl::Checkbox { key: k, on, act } => {
                let enabled = act.is_some();
                let fade = if enabled { 1.0 } else { 0.5 };
                let rect = Rect { pos, size };
                let border = if on { self.pal.accent } else { self.pal.border };
                let fill = if on { self.pal.accent } else { self.pal.transparent };
                self.rect(rect, alpha(fill, fade), Some(alpha(border, fade)), 2.0);
                if on {
                    let c = self.pal.on_accent;
                    self.icon(Icon::Check, pos + dvec2(4.0, 4.0), 14.0, alpha(c, fade));
                }
                if enabled {
                    self.zone(&k, rect, act);
                }
            }
            Ctl::Dropdown { key: k, label: text, lead, act, max_w } => {
                let (hovered, _) = self.state(&k);
                let enabled = act.is_some();
                let fade = if enabled { 1.0 } else { 0.5 };
                let rect = Rect { pos, size };
                let fill = if hovered && enabled { self.pal.interaction } else { self.pal.transparent };
                self.rect(rect, fill, Some(alpha(self.pal.border_accent, fade)), 12.0);
                let mut x = pos.x + 12.0;
                if let Some(lead) = lead {
                    self.lead(lead, dvec2(x, pos.y + (size.y - 16.0) / 2.0), 16.0);
                    x += 24.0;
                }
                let th = self.measure(Font::Regular, 14.0, &text, None).y;
                let fg = alpha(self.pal.foreground, fade);
                let tw = self.text_line(Font::Regular, 14.0, fg, dvec2(x, pos.y + (size.y - th) / 2.0), max_w, &text).x.min(max_w);
                let muted = alpha(self.pal.muted, fade);
                self.icon(Icon::ChevronDown, dvec2(x + tw + 8.0, pos.y + (size.y - 14.0) / 2.0), 14.0, muted);
                if enabled {
                    self.zone(&k, rect, act);
                }
            }
            Ctl::Segmented { key: k, items } => {
                let rect = Rect { pos, size };
                let bg = self.pal.surface2;
                self.rect(rect, bg, None, 10.0);
                let mut x = pos.x + 2.0;
                for (i, (text, selected, act)) in items.into_iter().enumerate() {
                    let tw = self.measure(Font::Regular, 14.0, &text, None);
                    let w = tw.x + 16.0;
                    let seg = Rect { pos: dvec2(x, pos.y + 2.0), size: dvec2(w, size.y - 4.0) };
                    if selected {
                        let fill = if self.pal.dark { self.pal.surface3 } else { self.pal.surface0 };
                        self.rect(seg, fill, None, 8.0);
                    }
                    let enabled = act.is_some();
                    let color = if selected { self.pal.foreground } else { self.pal.muted };
                    self.text(Font::Regular, 14.0, alpha(color, if enabled || selected { 1.0 } else { 0.5 }), dvec2(x + 8.0, pos.y + (size.y - tw.y) / 2.0), None, &text);
                    if enabled && !selected {
                        self.zone(&format!("{k}.{i}"), seg, act);
                    }
                    x += w;
                }
            }
            Ctl::Label { text, color, size: s, font, max_w } => {
                self.text(font, s, color, pos, Some(max_w), &text);
            }
            Ctl::Badge { label: text, tone, xs, dot, icon } => {
                let rect = Rect { pos, size };
                let tint = if self.pal.dark { 0.16 } else { 0.12 };
                match tone {
                    Some(color) => {
                        self.rect(rect, alpha(color, tint), None, size.y / 2.0);
                    }
                    None => {
                        let (s3, b) = (self.pal.surface3, self.pal.border);
                        self.rect(rect, s3, Some(b), size.y / 2.0);
                    }
                }
                let mut x = pos.x + if xs { 6.0 } else { 8.0 };
                if let Some(dot) = dot {
                    self.rect(Rect { pos: dvec2(x, pos.y + (size.y - 6.0) / 2.0), size: dvec2(6.0, 6.0) }, dot, None, 3.0);
                    x += 12.0;
                }
                if let Some(icon) = icon {
                    let muted = self.pal.muted;
                    self.icon(icon, dvec2(x, pos.y + (size.y - 14.0) / 2.0), 14.0, muted);
                    x += 20.0;
                }
                let color = tone.unwrap_or(self.pal.muted);
                let th = self.measure(Font::Regular, 12.0, &text, None).y;
                self.text_line(Font::Regular, 12.0, color, dvec2(x, pos.y + (size.y - th) / 2.0), 184.0, &text);
            }
            Ctl::Spinner(s) => {
                let muted = self.pal.muted;
                self.icon(Icon::Spinner, pos, s, muted);
            }
            Ctl::Dot(color, s) => {
                self.rect(Rect { pos, size: dvec2(s, s) }, color, None, s / 2.0);
            }
            Ctl::Glyph(icon, s, color) => self.icon(icon, pos, s, color),
            Ctl::Provider(id, svg, s, color) => self.provider_glyph(&id, svg.as_deref(), pos, s, color),
            Ctl::Lead(lead, s) => self.lead(lead, pos, s),
            Ctl::Keys(steps) => {
                let mut x = pos.x;
                for step in steps {
                    let tw = self.measure(Font::Regular, 12.0, &step, None);
                    let fill = alpha(self.pal.surface3, 0.5);
                    self.rect(Rect { pos: dvec2(x, pos.y), size: dvec2(tw.x + 8.0, size.y) }, fill, None, 6.0);
                    let muted = self.pal.muted;
                    self.text(Font::Regular, 12.0, muted, dvec2(x + 4.0, pos.y + (size.y - tw.y) / 2.0), None, &step);
                    x += tw.x + 12.0;
                }
            }
            Ctl::Link { key: k, label: text, url, leading } => {
                let muted = self.pal.muted;
                let tw = self.measure(Font::Regular, 12.0, &text, None).x;
                let (icon_x, text_x) = if leading { (pos.x, pos.x + 18.0) } else { (pos.x + tw + 4.0, pos.x) };
                let icon_size = if leading { 14.0 } else { 12.0 };
                self.icon(Icon::ArrowUpRight, dvec2(icon_x, pos.y + (size.y - icon_size) / 2.0), icon_size, muted);
                self.text(Font::Regular, 12.0, muted, dvec2(text_x, pos.y), None, &text);
                let open = crate::features::settings::rpc::handler(move |_, env| env.open_url(&url));
                self.zone(&k, Rect { pos, size }, Some(open));
            }
            Ctl::Field { spec, .. } => {
                let pal = self.pal;
                let layer = self.layer;
                self.fields.draw(self.cx, &pal, (layer, self.depth), spec, Rect { pos, size });
            }
            Ctl::Gap(_) => {}
            Ctl::Row(items, gap) => {
                let mut x = pos.x;
                let h = size.y;
                for item in items {
                    let s = self.measure_ctl(&item);
                    self.draw_ctl(item, dvec2(x, pos.y + (h - s.y) / 2.0));
                    x += s.x + gap;
                }
            }
            Ctl::Col(items, gap, end) => {
                let mut y = pos.y;
                let w = size.x;
                for item in items {
                    let s = self.measure_ctl(&item);
                    let x = if end { pos.x + w - s.x } else { pos.x };
                    self.draw_ctl(item, dvec2(x, y));
                    y += s.y + gap;
                }
            }
        }
        size
    }

    // ---------------------------------------------------------------- layout

    pub fn gap(&mut self, h: f64) {
        self.y += h;
    }

    /// Section header (Flutter `SettingsSection` title row).
    /// `info`: `(zone key, tooltip text)` for the trailing info glyph.
    pub fn section_title(&mut self, title: &str, info: Option<(String, String)>, trailing: Option<Ctl>) {
        let muted = self.pal.muted;
        let trailing_size = trailing.as_ref().map(|t| self.measure_ctl(t)).unwrap_or_default();
        let title_size = self.measure(Font::Regular, 12.0, title, Some((self.w - 4.0 - trailing_size.x - 30.0).max(40.0)));
        let h = title_size.y.max(trailing_size.y);
        let tx = self.x + 4.0;
        self.text(Font::Regular, 12.0, muted, dvec2(tx, self.y + (h - title_size.y) / 2.0), Some(title_size.x + 1.0), title);
        if let Some((k, text)) = info {
            let ix = tx + title_size.x + 8.0;
            let iy = self.y + (h - 14.0) / 2.0;
            self.icon(Icon::Info, dvec2(ix, iy), 14.0, muted);
            self.tip_zone(&k, Rect { pos: dvec2(ix, iy), size: dvec2(14.0, 14.0) }, text);
        }
        if let Some(trailing) = trailing {
            let pos = dvec2(self.x + self.w - trailing_size.x, self.y + (h - trailing_size.y) / 2.0);
            self.draw_ctl(trailing, pos);
        }
        self.y += h + 12.0;
    }

    pub fn card_begin(&mut self) {
        let fill = self.pal.surface1;
        let border = self.pal.border;
        let area = self.rect(Rect { pos: dvec2(self.x, self.y), size: dvec2(self.w, 1.0) }, fill, Some(border), 8.0);
        self.card = Some(Card { area, x: self.x, y: self.y, w: self.w, rows: 0 });
    }

    pub fn card_end(&mut self) {
        if let Some(card) = self.card.take() {
            let h = (self.y - card.y).max(2.0);
            card.area.set_rect(self.cx, &Rect { pos: dvec2(card.x, card.y), size: dvec2(card.w, h) });
        }
    }

    /// Inside a card: the 1px row border before every row but the first.
    pub fn card_row_separator(&mut self) {
        if let Some(card) = &mut self.card {
            card.rows += 1;
            if card.rows > 1 {
                let (x, w) = (card.x, card.w);
                let y = self.y;
                self.hline(x, y, w);
                self.y += 1.0;
            }
        }
    }

    /// Flutter `SettingsRow`: label/hint/error with a control beside it, or
    /// under it when the control leaves less than 160px for the label.
    pub fn row(&mut self, r: Row) {
        self.card_row_separator();
        let pad = r.padding;
        let x0 = self.x + pad;
        let inner_w = self.w - pad * 2.0;
        let control_size = r.control.as_ref().map(|c| self.measure_ctl(c)).unwrap_or_default();
        let leading_size = r.leading.as_ref().map(|c| self.measure_ctl(c)).unwrap_or_default();
        let lead_w = if r.leading.is_some() { leading_size.x + 8.0 } else { 0.0 };
        let inline_control = r.control.is_some() && (r.inline || control_size.x <= inner_w - 160.0 - 12.0);
        let label_w = if inline_control { (inner_w - control_size.x - 12.0 - lead_w).max(60.0) } else { inner_w - lead_w };
        let accessory_size = r.accessory.as_ref().map(|c| self.measure_ctl(c)).unwrap_or_default();
        let label_color = if r.muted_label { self.pal.muted } else { self.pal.foreground };
        let title_max = if r.accessory.is_some() { (label_w - accessory_size.x - 8.0).max(40.0) } else { label_w };
        let title_size = if r.label_lines_one {
            let m = self.measure(Font::Regular, 14.0, &r.label, None);
            dvec2(m.x.min(title_max), m.y)
        } else {
            self.measure(Font::Regular, 14.0, &r.label, Some(title_max))
        };
        let mut text_h = title_size.y.max(accessory_size.y);
        let hint_size = r.hint.as_ref().map(|h| self.measure(Font::Regular, 12.0, h, Some(label_w)));
        let error_size = r.error.as_ref().map(|e| self.measure(Font::Regular, 12.0, e, Some(label_w)));
        if let Some(s) = hint_size {
            text_h += 4.0 + s.y;
        }
        if let Some(s) = error_size {
            text_h += 4.0 + s.y;
        }
        let block_h = text_h.max(leading_size.y);
        let content_h = if inline_control { block_h.max(control_size.y) } else if r.control.is_some() { block_h + 12.0 + control_size.y } else { block_h };
        let total = content_h + pad * 2.0;
        let rect = Rect { pos: dvec2(self.x, self.y), size: dvec2(self.w, total) };
        if let Some((k, _)) = &r.on_tap {
            let (hovered, pressed) = self.state(k);
            if hovered || pressed {
                let fill = if pressed { self.pal.surface3 } else { self.pal.surface2 };
                self.rect(rect, fill, None, 0.0);
            }
        }
        let top = self.y + pad;
        let text_top = if inline_control { top + (content_h - block_h) / 2.0 } else { top };
        if let Some(leading) = r.leading {
            self.draw_ctl(leading, dvec2(x0, text_top + (block_h - leading_size.y) / 2.0));
        }
        let tx = x0 + lead_w;
        let mut ty = text_top;
        if r.label_lines_one {
            self.text_line(Font::Regular, 14.0, label_color, dvec2(tx, ty), title_max, &r.label);
        } else {
            self.text(Font::Regular, 14.0, label_color, dvec2(tx, ty), Some(title_max), &r.label);
        }
        if let Some(accessory) = r.accessory {
            self.draw_ctl(accessory, dvec2(tx + title_size.x + 8.0, ty + (title_size.y - accessory_size.y) / 2.0));
        }
        ty += title_size.y.max(accessory_size.y);
        if let (Some(hint), Some(s)) = (&r.hint, hint_size) {
            let muted = self.pal.muted;
            self.text(Font::Regular, 12.0, muted, dvec2(tx, ty + 4.0), Some(label_w), hint);
            ty += 4.0 + s.y;
        }
        if let (Some(error), Some(_)) = (&r.error, error_size) {
            let danger = self.pal.danger;
            self.text(Font::Regular, 12.0, danger, dvec2(tx, ty + 4.0), Some(label_w), error);
        }
        if let Some(control) = r.control {
            let pos = if inline_control {
                dvec2(x0 + inner_w - control_size.x, top + (content_h - control_size.y) / 2.0)
            } else {
                dvec2(x0, top + block_h + 12.0)
            };
            self.draw_ctl(control, pos);
        }
        if let Some((k, on)) = r.on_tap {
            self.zone(&k, rect, Some(on));
        }
        self.y += total;
    }

    /// Flutter `SettingsEmptyCard`.
    pub fn empty_card(&mut self, text: &str, action: Option<Ctl>, roomy: bool) {
        let pad_v = if roomy { 24.0 } else { 16.0 };
        let inner = self.w - 32.0;
        let ts = self.measure(Font::Regular, 14.0, text, Some(inner));
        let action_size = action.as_ref().map(|a| self.measure_ctl(a)).unwrap_or_default();
        let h = pad_v * 2.0 + ts.y + if action.is_some() { 12.0 + action_size.y } else { 0.0 };
        let (s1, b, muted) = (self.pal.surface1, self.pal.border, self.pal.muted);
        self.rect(Rect { pos: dvec2(self.x, self.y), size: dvec2(self.w, h) }, s1, Some(b), 8.0);
        self.text(Font::Regular, 14.0, muted, dvec2(self.x + (self.w - ts.x) / 2.0, self.y + pad_v), Some(inner), text);
        if let Some(action) = action {
            let pos = dvec2(self.x + (self.w - action_size.x) / 2.0, self.y + pad_v + ts.y + 12.0);
            self.draw_ctl(action, pos);
        }
        self.y += h;
    }

    /// Flutter `SettingsInlineAlert`: bordered 16px-radius box, status glyph,
    /// medium title and muted description.
    pub fn alert(&mut self, success: bool, title: &str, description: Option<&str>, action: Option<Ctl>) {
        let accent = if success { self.pal.success } else { self.pal.danger };
        let border = if success { self.pal.border } else { alpha(accent, 0.5) };
        let inner = self.w - 32.0 - 26.0;
        let ts = self.measure(Font::Medium, 14.0, title, Some(inner));
        let ds = description.filter(|d| !d.is_empty()).map(|d| self.measure(Font::Regular, 14.0, d, Some(inner)));
        let action_size = action.as_ref().map(|a| self.measure_ctl(a)).unwrap_or_default();
        let h = 24.0 + ts.y + ds.map(|s| s.y + 2.0).unwrap_or(0.0) + if action.is_some() { 8.0 + action_size.y } else { 0.0 };
        let transparent = self.pal.transparent;
        self.rect(Rect { pos: dvec2(self.x, self.y), size: dvec2(self.w, h) }, transparent, Some(border), 16.0);
        let icon = if success { Icon::CircleCheck } else { Icon::CircleX };
        self.icon(icon, dvec2(self.x + 16.0, self.y + 12.0 + (ts.y - 14.0).max(0.0) / 2.0), 14.0, accent);
        // Flutter `SettingsAlert`: the title is a `SelectableText`.
        self.alert_seq += 1;
        let key = format!("alert.{}", self.alert_seq);
        self.selectable(&key, Font::Medium, 14.0, accent, dvec2(self.x + 42.0, self.y + 12.0), Some(inner), title);
        let mut y = self.y + 12.0 + ts.y;
        if let (Some(d), Some(s)) = (description, ds) {
            let muted = self.pal.muted;
            self.text(Font::Regular, 14.0, muted, dvec2(self.x + 42.0, y + 2.0), Some(inner), d);
            y += 2.0 + s.y;
        }
        if let Some(action) = action {
            self.draw_ctl(action, dvec2(self.x + 42.0, y + 8.0));
        }
        self.y += h;
    }

    /// Flutter `_UsageAlert` (original `alert.tsx`, sm, error): a danger-tinted
    /// 8px box, 12px padding, medium 14px danger title, 4px gap, selectable
    /// 12px muted description and the action 8px below.
    pub fn tinted_alert(&mut self, key: &str, title: &str, description: &str, action: Option<Ctl>) {
        let danger = self.pal.danger;
        let muted = self.pal.muted;
        let tint = if self.pal.dark { 0.16 } else { 0.12 };
        let inner = self.w - 24.0;
        let ts = self.measure(Font::Medium, 14.0, title, Some(inner));
        let ds = self.measure(Font::Regular, 12.0, description, Some(inner));
        let action_size = action.as_ref().map(|a| self.measure_ctl(a)).unwrap_or_default();
        let h = 24.0 + ts.y + 4.0 + ds.y + if action.is_some() { 4.0 + 8.0 + action_size.y } else { 0.0 };
        let (x, y) = (self.x, self.y);
        self.rect(Rect { pos: dvec2(x, y), size: dvec2(self.w, h) }, alpha(danger, tint), None, 8.0);
        self.text(Font::Medium, 14.0, danger, dvec2(x + 12.0, y + 12.0), Some(inner), title);
        self.selectable(key, Font::Regular, 12.0, muted, dvec2(x + 12.0, y + 12.0 + ts.y + 4.0), Some(inner), description);
        if let Some(action) = action {
            self.draw_ctl(action, dvec2(x + 12.0, y + 12.0 + ts.y + 4.0 + ds.y + 4.0 + 8.0));
        }
        self.y += h;
    }

    /// The original warning `Alert` (plugins trusted-code notice etc.).
    pub fn warning(&mut self, title: &str, description: Option<&str>) {
        let accent = self.pal.warning;
        let inner = self.w - 32.0 - 26.0;
        let ts = self.measure(Font::Medium, 14.0, title, Some(inner));
        let ds = description.filter(|d| !d.is_empty()).map(|d| self.measure(Font::Regular, 14.0, d, Some(inner)));
        let h = 24.0 + ts.y + ds.map(|s| s.y + 2.0).unwrap_or(0.0);
        let transparent = self.pal.transparent;
        self.rect(Rect { pos: dvec2(self.x, self.y), size: dvec2(self.w, h) }, transparent, Some(alpha(accent, 0.5)), 16.0);
        self.icon(Icon::TriangleAlert, dvec2(self.x + 16.0, self.y + 12.0), 14.0, accent);
        self.text(Font::Medium, 14.0, accent, dvec2(self.x + 42.0, self.y + 12.0), Some(inner), title);
        if let (Some(d), Some(_)) = (description, ds) {
            let muted = self.pal.muted;
            self.text(Font::Regular, 14.0, muted, dvec2(self.x + 42.0, self.y + 14.0 + ts.y), Some(inner), d);
        }
        self.y += h;
    }

    /// A full-width field with an optional label above (Flutter `PaseoFormField`).
    pub fn form_field(&mut self, label: Option<&str>, hint: Option<&str>, error: Option<&str>, spec: FieldSpec, height: f64) {
        if let Some(label) = label {
            let muted = self.pal.muted;
            let s = self.text(Font::Regular, 14.0, muted, dvec2(self.x, self.y), Some(self.w), label);
            self.y += s.y + 8.0;
        }
        let w = self.w;
        let size = self.draw_ctl(Ctl::Field { spec, w, h: height }, dvec2(self.x, self.y));
        self.y += size.y;
        if let Some(hint) = hint {
            let muted = self.pal.muted;
            let s = self.text(Font::Regular, 12.0, muted, dvec2(self.x, self.y + 6.0), Some(self.w), hint);
            self.y += 6.0 + s.y;
        }
        if let Some(error) = error {
            let color = self.pal.field_error;
            let s = self.text(Font::Regular, 12.0, color, dvec2(self.x, self.y + 6.0), Some(self.w), error);
            self.y += 6.0 + s.y;
        }
    }

    /// Paragraph text advancing the cursor.
    pub fn para(&mut self, text: &str, size: f64, color: Vec4) {
        let s = self.text(Font::Regular, size, color, dvec2(self.x, self.y), Some(self.w), text);
        self.y += s.y;
    }

    /// Flutter `Wrap(spacing, runSpacing, crossAxisAlignment: center)` at the
    /// cursor across the full width; advances past the last run.
    pub fn wrap(&mut self, items: Vec<Ctl>, spacing: f64, run_spacing: f64) {
        let sizes: Vec<DVec2> = items.iter().map(|c| self.measure_ctl(c)).collect();
        let mut runs: Vec<Vec<usize>> = vec![Vec::new()];
        let mut run_w = 0.0;
        for (i, size) in sizes.iter().enumerate() {
            let run = runs.last_mut().expect("one run");
            let needed = if run.is_empty() { size.x } else { run_w + spacing + size.x };
            if !run.is_empty() && needed > self.w {
                runs.push(vec![i]);
                run_w = size.x;
            } else {
                run.push(i);
                run_w = needed;
            }
        }
        let mut items: Vec<Option<Ctl>> = items.into_iter().map(Some).collect();
        let mut y = self.y;
        for (r, run) in runs.iter().enumerate() {
            let h = run.iter().map(|&i| sizes[i].y).fold(0.0, f64::max);
            let mut x = self.x;
            for &i in run {
                if let Some(c) = items[i].take() {
                    self.draw_ctl(c, dvec2(x, y + (h - sizes[i].y) / 2.0));
                }
                x += sizes[i].x + spacing;
            }
            y += h + if r + 1 < runs.len() { run_spacing } else { 0.0 };
        }
        self.y = y;
    }

    /// Lays out one control at the cursor and advances.
    pub fn place(&mut self, c: Ctl) -> DVec2 {
        let pos = dvec2(self.x, self.y);
        let s = self.draw_ctl(c, pos);
        self.y += s.y;
        s
    }

    /// Multiline text height for a field showing `text`.
    pub fn multiline_height(&mut self, text: &str, min: f64) -> f64 {
        let height = self.measure(Font::Regular, 14.0, text, Some((self.w - 24.0).max(1.0))).y;
        (height + 24.0).clamp(min, 360.0)
    }
}

// ------------------------------------------------------------ spinner

/// Seconds since the first spinner frame (the animation phase source).
fn spinner_clock() -> f64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

/// Flutter `Curves.fastOutSlowIn`: `Cubic(0.4, 0.0, 0.2, 1.0)`.
fn fast_out_slow_in(t: f64) -> f64 {
    let (x1, y1, x2, y2) = (0.4, 0.0, 0.2, 1.0);
    let bezier = |a: f64, b: f64, m: f64| 3.0 * a * (1.0 - m) * (1.0 - m) * m + 3.0 * b * (1.0 - m) * m * m + m * m * m;
    let (mut lo, mut hi) = (0.0, 1.0);
    // Flutter's `Cubic.transform` bisects to 0.001 as well.
    loop {
        let mid = (lo + hi) / 2.0;
        let x = bezier(x1, x2, mid);
        if (t - x).abs() < 0.001 || hi - lo < 1e-6 {
            return bezier(y1, y2, mid);
        }
        if x < t { lo = mid } else { hi = mid }
    }
}

/// Flutter indeterminate `CircularProgressIndicator` at `t` seconds, as
/// `(start, sweep)` in turns of the settings `DrawRing` (0 = bottom,
/// clockwise). Flutter: 1333 ms head/tail cycle (head over the first half,
/// tail over the second, both `fastOutSlowIn`), 2222 ms rotation, a quarter
/// turn offset per cycle, arcs of up to 3/4 turn starting at the top.
pub fn spinner_arc(t: f64) -> (f64, f64) {
    use std::f64::consts::PI;
    let path = (t / 1.333).fract();
    let head = fast_out_slow_in((path / 0.5).min(1.0));
    let tail = if path < 0.5 { 0.0 } else { fast_out_slow_in((path - 0.5) / 0.5) };
    let rotation = (t / 2.222).fract();
    let start = -PI / 2.0 + tail * 1.5 * PI + rotation * 2.0 * PI + path * 0.5 * PI;
    let sweep = (head * 1.5 * PI - tail * 1.5 * PI).max(0.001);
    // Flutter angles start at +x; the ring's start at +y (a quarter turn later).
    ((start - PI / 2.0) / (2.0 * PI), sweep / (2.0 * PI))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_follows_flutter_head_and_tail() {
        // Cycle start: no sweep yet, the arc starts at the top (half a ring turn from +y).
        let (start, sweep) = spinner_arc(0.0);
        assert!(sweep < 0.001);
        assert!((start + 0.5).abs() < 1e-9);
        // Head done, tail not started: the full 3/4-turn arc.
        let (_, sweep) = spinner_arc(1.333 * 0.5);
        assert!((sweep - 0.75).abs() < 0.01);
        // Tail catches up by the end of the cycle.
        let (_, sweep) = spinner_arc(1.333 * 0.999);
        assert!(sweep < 0.01);
    }
}
