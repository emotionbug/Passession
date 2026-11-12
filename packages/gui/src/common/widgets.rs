//! Paseo primitives (Flutter `ui/controls.dart`, `ui/common.dart` building blocks). Each one
//! repaints from the runtime [`Palette`] on every draw, so theme switches apply without
//! reloading DSL. Use them in DSL with `use crate::common::widgets::*;`.

use makepad_widgets::*;
use serde_json::Value;

use crate::common::paint::{Font, HitMap, Painter, TextSpec, rect};
use crate::common::{UiContext, UiEffect, clock};
use crate::theme::{self, ControlSize, Palette, Typography, control_height, control_icon_size, fade};

/// Measured line extents shared by custom-painted list rows. Measure once per
/// style per draw; virtualized lists use `height` for culling and advancing too.
#[derive(Clone, Copy, Debug)]
pub struct RowMetrics {
    pub title_height: f64,
    pub subtitle_height: f64,
    pub gap: f64,
}

impl RowMetrics {
    pub fn new(title_height: f64, subtitle_height: f64, gap: f64) -> Self {
        Self {
            title_height: title_height.max(0.0),
            subtitle_height: subtitle_height.max(0.0),
            gap: if subtitle_height > 0.0 { gap.max(0.0) } else { 0.0 },
        }
    }

    pub fn measure(painter: &mut Painter, cx: &mut Cx, title: TextSpec, subtitle: Option<TextSpec>, gap: f64) -> Self {
        let title_height = painter.line_height(cx, title);
        let subtitle_height = subtitle.map_or(0.0, |spec| painter.line_height(cx, spec));
        Self::new(title_height, subtitle_height, gap)
    }

    pub fn height(self, minimum: f64, padding_y: f64, leading_height: f64) -> f64 {
        minimum.max((self.title_height + self.gap + self.subtitle_height).max(leading_height) + padding_y * 2.0)
    }

    /// Side slots reserve space before text. Narrow rows sacrifice text width,
    /// never let it overlap a trailing action. The caller paints custom icons
    /// or metadata in the slots and registers `bounds` with its existing hits.
    pub fn layout(self, bounds: Rect, padding_x: f64, leading_width: f64, trailing_width: f64, slot_gap: f64) -> RowLayout {
        let padding = padding_x.max(0.0).min(bounds.size.x.max(0.0) * 0.5);
        let left = bounds.pos.x + padding;
        let right = bounds.pos.x + bounds.size.x.max(0.0) - padding;
        let trailing_width = trailing_width.max(0.0).min(right - left);
        let trailing = rect(right - trailing_width, bounds.pos.y, trailing_width, bounds.size.y);
        let content_right = (trailing.pos.x - if trailing_width > 0.0 { slot_gap.max(0.0) } else { 0.0 }).max(left);
        let leading_width = leading_width.max(0.0).min(content_right - left);
        let leading = rect(left, bounds.pos.y, leading_width, bounds.size.y);
        let text_left = (left + leading_width + if leading_width > 0.0 { slot_gap.max(0.0) } else { 0.0 }).min(content_right);
        let text_width = content_right - text_left;
        let text_height = self.title_height + self.gap + self.subtitle_height;
        let top = bounds.pos.y + (bounds.size.y - text_height) * 0.5;
        RowLayout {
            bounds,
            leading,
            title: rect(text_left, top, text_width, self.title_height),
            subtitle: rect(text_left, top + self.title_height + self.gap, text_width, self.subtitle_height),
            trailing,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RowLayout {
    pub bounds: Rect,
    pub leading: Rect,
    pub title: Rect,
    pub subtitle: Rect,
    pub trailing: Rect,
}

/// A shared baseline for adjacent labels that may use different fallback fonts.
/// Centering each label independently (or giving both the same top) misaligns
/// Latin and Hangul; reserve the union above and below their actual baselines.
#[derive(Clone, Copy, Debug, Default)]
pub struct InlineMetrics {
    above: f64,
    below: f64,
}

impl InlineMetrics {
    pub fn from_layout(layout: &makepad_widgets::makepad_draw::text::layouter::LaidoutText) -> Self {
        let above = layout.rows.first().map_or(0.0, |row| row.origin_in_lpxs.y as f64);
        Self { above, below: (layout.size_in_lpxs.height as f64 - above).max(0.0) }
    }

    pub fn union(self, other: Self) -> Self {
        Self { above: self.above.max(other.above), below: self.below.max(other.below) }
    }

    pub fn height(self) -> f64 {
        self.above + self.below
    }

    pub fn top(self, bounds: Rect, label: Self) -> f64 {
        bounds.pos.y + (bounds.size.y - self.height()) * 0.5 + self.above - label.above
    }
}

#[cfg(test)]
mod row_tests {
    use super::*;

    #[test]
    fn scaled_subtitle_stays_inside_the_row_hit_and_advance() {
        let metrics = RowMetrics::new(28.0, 24.0, 2.0);
        let height = metrics.height(48.0, 7.0, 14.0);
        let first = metrics.layout(rect(10.0, 50.0, 280.0, height), 12.0, 14.0, 32.0, 8.0);
        let second = metrics.layout(rect(10.0, 50.0 + height, 280.0, height), 12.0, 14.0, 32.0, 8.0);
        assert_eq!(height, 68.0);
        assert_eq!(first.title.pos.y, 57.0);
        assert_eq!(first.subtitle.pos.y, 87.0);
        assert_eq!(first.subtitle.pos.y + first.subtitle.size.y, 111.0);
        assert_eq!(second.bounds.pos.y, first.bounds.pos.y + first.bounds.size.y);
        let mut hits = HitMap::default();
        hits.push(first.bounds, 1);
        hits.push(second.bounds, 2);
        assert_eq!(hits.at(dvec2(first.subtitle.pos.x, 110.0)), Some(1));
        assert_eq!(hits.at(dvec2(first.subtitle.pos.x, 118.5)), Some(2));
    }

    #[test]
    fn narrow_rows_reserve_trailing_actions_without_text_overlap() {
        let metrics = RowMetrics::new(20.0, 0.0, 0.0);
        for width in [0.0, 12.0, 24.0, 40.0, 64.0, 96.0] {
            let slots = metrics.layout(rect(10.0, 0.0, width, 32.0), 12.0, 16.0, 24.0, 8.0);
            assert!(slots.title.size.x >= 0.0);
            assert!(slots.leading.pos.x + slots.leading.size.x <= slots.title.pos.x);
            assert!(slots.title.pos.x + slots.title.size.x <= slots.trailing.pos.x);
            assert!(slots.trailing.pos.x + slots.trailing.size.x <= 10.0 + width);
            if width >= 48.0 {
                assert_eq!(slots.trailing.size.x, 24.0);
            }
        }
    }

    #[test]
    fn absent_slots_and_subtitle_do_not_consume_label_space() {
        let metrics = RowMetrics::new(20.0, 0.0, 6.0);
        assert_eq!(metrics.height(28.0, 4.0, 16.0), 28.0);
        let slots = metrics.layout(rect(0.0, 0.0, 120.0, 28.0), 8.0, 0.0, 0.0, 8.0);
        assert_eq!(slots.title.pos, dvec2(8.0, 4.0));
        assert_eq!(slots.title.size, dvec2(104.0, 20.0));
        assert_eq!(slots.subtitle.size.y, 0.0);
    }

    #[test]
    fn adjacent_fallback_labels_share_a_baseline_without_escaping_the_row() {
        for scale in [1.0, 1.5, 2.0] {
            let latin = InlineMetrics { above: 11.0 * scale, below: 4.0 * scale };
            let hangul = InlineMetrics { above: 17.0 * scale, below: 3.0 * scale };
            let joined = latin.union(hangul);
            let height = RowMetrics::new(joined.height(), 0.0, 0.0).height(30.0, 4.0, 22.0);
            let bounds = rect(0.0, 40.0, 240.0, height);
            let latin_top = joined.top(bounds, latin);
            let hangul_top = joined.top(bounds, hangul);
            assert_eq!(latin_top + latin.above, hangul_top + hangul.above);
            assert!(latin_top >= bounds.pos.y + 4.0);
            assert!(hangul_top >= bounds.pos.y + 4.0);
            assert!(latin_top + latin.height() <= bounds.pos.y + bounds.size.y - 4.0);
            assert!(hangul_top + hangul.height() <= bounds.pos.y + bounds.size.y - 4.0);
        }
    }
}

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::paint::PaseoPainter;

    pub SurfaceBase = {{Surface}} {}
    pub Surface = <SurfaceBase> {
        show_bg: true,
        draw_bg: {
            instance border_color: vec4(0.0, 0.0, 0.0, 0.0)
            instance border_width: 0.0
            instance radius: 0.0
            // Exact rounded-rect distance (see `DrawBox` in theme.rs: `Sdf2d::box` doubles the
            // radius and leaves square boxes nearly transparent).
            fn pixel(self) -> vec4 {
                let size = self.rect_size;
                let p = self.pos * size - size * 0.5;
                let r = min(self.radius, min(size.x, size.y) * 0.5);
                let q = abs(p) - size * 0.5 + vec2(r, r);
                let d = length(max(q, vec2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
                let outer = clamp(0.5 - d, 0.0, 1.0);
                let fill = vec4(self.color.rgb * self.color.a, self.color.a);
                let bw = self.border_width;
                if bw > 0.0 {
                    let border = vec4(self.border_color.rgb * self.border_color.a, self.border_color.a);
                    let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
                    return mix(border, fill, inner) * outer;
                }
                return fill * outer;
            }
        }
    }

    pub PLabel = {{PLabel}} {
        width: Fit, height: Fit,
        draw_regular: { text_style: <PASEO_FONT_REGULAR> {} }
        draw_medium: { text_style: <PASEO_FONT_MEDIUM> {} }
        draw_mono: { text_style: <PASEO_FONT_MONO> {} }
    }

    pub PButton = {{PButton}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub PIconButton = {{PIconButton}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub PIcon = {{PIcon}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub PSwitch = {{PSwitch}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub PCheckbox = {{PCheckbox}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub PSegmented = {{PSegmented}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }

    pub Divider = {{Divider}} {
        width: Fill, height: 1,
        painter: <PaseoPainter> {}
    }

    pub StatusMark = {{StatusMark}} {
        width: 14, height: 14,
        painter: <PaseoPainter> {}
    }

    pub Spinner = {{Spinner}} {
        width: 16, height: 16,
        painter: <PaseoPainter> {}
    }

    pub PTextFieldBase = {{PTextField}} {}
    pub PTextField = <PTextFieldBase> {
        width: Fill, height: Fit,
        flow: Down,
        input = <TextInput> {
            width: Fill, height: Fit,
            padding: { left: 12, right: 12, top: 6, bottom: 6 }
            margin: 0,
            empty_text: "",
            draw_text: { text_style: <PASEO_FONT_REGULAR> {} }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Roles

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum SurfaceRole {
    #[pick]
    Transparent,
    Surface0,
    Surface1,
    Surface2,
    Surface3,
    Surface4,
    Sidebar,
    Selected,
    Accent,
    Danger,
    /// `cardTheme`: surface1 with a border.
    Card,
    Dialog,
    Background,
}

impl SurfaceRole {
    pub fn color(self, p: &Palette) -> Vec4 {
        match self {
            Self::Transparent => Vec4::default(),
            Self::Surface0 => p.surface0,
            Self::Surface1 | Self::Card => p.surface1,
            Self::Surface2 => p.surface2,
            Self::Surface3 => p.surface3,
            Self::Surface4 => p.surface4,
            Self::Sidebar => p.sidebar,
            Self::Selected => p.selected,
            Self::Accent => p.accent,
            Self::Danger => p.destructive,
            Self::Dialog => p.dialog,
            Self::Background => p.background,
        }
    }
}

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum BorderRole {
    #[pick]
    None,
    Border,
    BorderAccent,
    Danger,
}

impl BorderRole {
    pub fn color(self, p: &Palette) -> Option<Vec4> {
        match self {
            Self::None => None,
            Self::Border => Some(p.border),
            Self::BorderAccent => Some(p.border_accent),
            Self::Danger => Some(p.destructive),
        }
    }
}

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum TextRole {
    #[pick]
    Body,
    Title,
    Muted,
    ExtraMuted,
    Small,
    SmallMuted,
    Mono,
    Danger,
    Success,
    Warning,
    Accent,
    OnAccent,
    FieldError,
}

impl TextRole {
    fn color(self, p: &Palette) -> Vec4 {
        match self {
            Self::Body | Self::Title | Self::Small | Self::Mono => p.foreground,
            Self::Muted | Self::SmallMuted => p.muted,
            Self::ExtraMuted => p.extra_muted,
            Self::Danger => p.destructive,
            Self::Success => p.success,
            Self::Warning => p.warning,
            Self::Accent => p.accent_bright,
            Self::OnAccent => p.on_tertiary,
            Self::FieldError => p.field_error,
        }
    }

    /// Default authored px when `size` is 0.
    fn default_size(self) -> f64 {
        match self {
            Self::Title => 16.0,
            Self::Small | Self::SmallMuted | Self::FieldError => 12.0,
            Self::Mono => 12.0,
            _ => 14.0,
        }
    }
}

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum FontWeight {
    #[pick]
    Regular,
    Medium,
    Semibold,
    Bold,
}

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum ButtonVariant {
    Primary,
    #[pick]
    Secondary,
    Outline,
    Ghost,
    Destructive,
}

// ---------------------------------------------------------------------------------------------
// Surface

#[derive(Clone, Debug, DefaultNone)]
pub enum SurfaceAction {
    Clicked(KeyModifiers),
    /// Right click / long press with the pointer position (context menus).
    SecondaryClicked(DVec2),
    HoverChanged(bool),
    None,
}

/// View with a palette-driven background/border and optional hover/press highlight.
#[derive(Live, LiveHook, Widget)]
pub struct Surface {
    #[deref]
    view: View,
    #[live]
    role: SurfaceRole,
    #[live]
    hover_role: SurfaceRole,
    #[live]
    border_role: BorderRole,
    #[live(0.0)]
    radius: f64,
    #[live(1.0)]
    border_width: f64,
    #[live(false)]
    interactive: bool,
    #[rust]
    hovered: bool,
    #[rust]
    pressed: bool,
}

impl Widget for Surface {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if !self.interactive {
            return;
        }
        let uid = self.widget_uid();
        match event.hits(cx, self.view.area()) {
            Hit::FingerHoverIn(_) => {
                cx.set_cursor(MouseCursor::Hand);
                self.hovered = true;
                cx.widget_action(uid, &scope.path, SurfaceAction::HoverChanged(true));
                self.redraw(cx);
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = false;
                cx.widget_action(uid, &scope.path, SurfaceAction::HoverChanged(false));
                self.redraw(cx);
            }
            Hit::FingerDown(fe) => {
                if fe.mouse_button().is_some_and(|b| b.is_secondary()) {
                    cx.widget_action(uid, &scope.path, SurfaceAction::SecondaryClicked(fe.abs));
                } else {
                    self.pressed = true;
                    self.redraw(cx);
                }
            }
            Hit::FingerLongPress(lp) => {
                self.pressed = false;
                cx.widget_action(uid, &scope.path, SurfaceAction::SecondaryClicked(lp.abs));
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                let was_pressed = std::mem::take(&mut self.pressed);
                if was_pressed && fe.is_over && !fe.has_long_press_occurred && (!fe.device.is_touch() || fe.was_tap()) {
                    cx.widget_action(uid, &scope.path, SurfaceAction::Clicked(fe.modifiers));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let mut fill = self.role.color(&p);
        if self.interactive && (self.hovered || self.pressed) {
            fill = match self.hover_role {
                SurfaceRole::Transparent => p.sidebar_hover_over(fill),
                role => role.color(&p),
            };
        }
        let border = self.border_role.color(&p);
        self.view.draw_bg.color = fill;
        let b = border.unwrap_or_default();
        self.view.draw_bg.set_var_instance(cx, &[live_id!(border_color)], &[b.x, b.y, b.z, b.w]);
        let width = if border.is_some() { self.border_width as f32 } else { 0.0 };
        self.view.draw_bg.set_var_instance(cx, &[live_id!(border_width)], &[width]);
        self.view.draw_bg.set_var_instance(cx, &[live_id!(radius)], &[self.radius as f32]);
        self.view.draw_walk(cx, scope, walk)
    }
}

impl Palette {
    /// Hover fill for an interactive surface without an explicit hover role.
    pub fn sidebar_hover_over(&self, base: Vec4) -> Vec4 {
        if base.w <= 0.0 { self.sidebar_hover } else { self.interaction_highlight_over(base) }
    }

    fn interaction_highlight_over(&self, base: Vec4) -> Vec4 {
        let h = self.interaction_highlight;
        let a = h.w;
        vec4(base.x * (1.0 - a) + h.x * a, base.y * (1.0 - a) + h.y * a, base.z * (1.0 - a) + h.z * a, base.w)
    }
}

impl SurfaceRef {
    pub fn clicked(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|s| matches!(actions.find_widget_action(s.widget_uid()).cast_ref(), SurfaceAction::Clicked(_)))
    }

    pub fn secondary_clicked(&self, actions: &Actions) -> Option<DVec2> {
        let s = self.borrow()?;
        match actions.find_widget_action(s.widget_uid()).cast_ref() {
            SurfaceAction::SecondaryClicked(pos) => Some(*pos),
            _ => None,
        }
    }

    pub fn set_role(&self, cx: &mut Cx, role: SurfaceRole) {
        if let Some(mut s) = self.borrow_mut()
            && s.role != role
        {
            s.role = role;
            s.redraw(cx);
        }
    }

    pub fn rect(&self, cx: &Cx) -> Rect {
        self.borrow().map(|s| s.view.area().rect(cx)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// PLabel

#[derive(Live, LiveHook, Widget)]
pub struct PLabel {
    #[redraw]
    #[live]
    draw_regular: DrawText,
    #[live]
    draw_medium: DrawText,
    #[live]
    draw_mono: DrawText,
    #[walk]
    walk: Walk,
    #[live]
    align: Align,
    #[live]
    text: ArcStringMut,
    #[live]
    role: TextRole,
    /// Authored px at the 14px base; 0 = the role's default.
    #[live(0.0)]
    size: f64,
    #[live]
    weight: FontWeight,
    /// Wrap at the available width (Flutter default `Text`), else one line.
    #[live(true)]
    wrap: bool,
    /// One line with an ellipsis (`maxLines: 1, overflow: ellipsis`).
    #[live(false)]
    ellipsis: bool,
    /// Content-size text (`contentFontSize`) instead of UI size.
    #[live(false)]
    content: bool,
    #[rust]
    color_override: Option<Vec4>,
}

impl PLabel {
    fn spec(&self) -> (Font, f64, Vec4) {
        let p = theme::current_palette();
        let t = theme::current_typography();
        let authored = if self.size > 0.0 { self.size } else { self.role.default_size() };
        let px = if self.content { t.content_font_size } else { t.ui_size(authored) };
        let font = if self.role == TextRole::Mono {
            Font::Mono
        } else if self.weight == FontWeight::Regular && self.role != TextRole::Title {
            Font::Regular
        } else {
            Font::Medium
        };
        (font, px, self.color_override.unwrap_or_else(|| self.role.color(&p)))
    }
}

impl Widget for PLabel {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let (font, px, color) = self.spec();
        let text = self.text.as_ref().to_string();
        let drawer = match font {
            Font::Regular => &mut self.draw_regular,
            Font::Medium => &mut self.draw_medium,
            Font::Mono => &mut self.draw_mono,
        };
        drawer.text_style.font_size = Typography::pt(px);
        drawer.color = color;
        if self.ellipsis {
            let laid = drawer.layout(cx, 0.0, 0.0, None, false, Align::default(), &text);
            let height = laid.size_in_lpxs.height as f64;
            let rect = cx.walk_turtle(Walk { height: Size::Fixed(height), ..walk });
            let shown = fit_text(drawer, cx, &text, rect.size.x);
            drawer.draw_abs(cx, rect.pos, &shown);
        } else if !self.wrap {
            let laid = drawer.layout(cx, 0.0, 0.0, None, false, Align::default(), &text);
            drawer.draw_walk_laidout(cx, walk, &laid);
        } else {
            drawer.draw_walk(cx, walk, self.align, &text);
        }
        DrawStep::done()
    }

    fn text(&self) -> String {
        self.text.as_ref().to_string()
    }

    fn set_text(&mut self, cx: &mut Cx, v: &str) {
        if self.text.as_ref() != v {
            self.text.as_mut_empty().push_str(v);
            self.redraw(cx);
        }
    }
}

/// Truncates `text` with "…" to `max_width` using `drawer`'s current style.
fn fit_text(drawer: &DrawText, cx: &mut Cx, text: &str, max_width: f64) -> String {
    let line = text.lines().next().unwrap_or("");
    let width = |s: &str, cx: &mut Cx| drawer.layout(cx, 0.0, 0.0, None, false, Align::default(), s).size_in_lpxs.width as f64;
    if width(line, cx) <= max_width + 0.5 {
        return line.to_string();
    }
    let mut end = line.len();
    while end > 0 {
        end -= 1;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}…", line[..end].trim_end());
        if width(&candidate, cx) <= max_width + 0.5 {
            return candidate;
        }
    }
    "…".to_string()
}

impl PLabelRef {
    pub fn set_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut label) = self.borrow_mut()
            && label.color_override != color
        {
            label.color_override = color;
            label.redraw(cx);
        }
    }

    pub fn set_role(&self, cx: &mut Cx, role: TextRole) {
        if let Some(mut label) = self.borrow_mut()
            && label.role != role
        {
            label.role = role;
            label.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PButton

#[derive(Clone, Debug, DefaultNone)]
pub enum PButtonAction {
    Clicked(KeyModifiers),
    None,
}

/// Original `components/ui/button.tsx`: 1px bordered pill, 8px icon gap, 0.85 pressed and 0.5
/// disabled opacity, normal-weight text.
#[derive(Live, LiveHook, Widget)]
pub struct PButton {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live]
    text: ArcStringMut,
    #[live]
    icon: ArcStringMut,
    #[live]
    variant: ButtonVariant,
    #[live]
    size: ControlSize,
    #[live(true)]
    enabled: bool,
    #[live(false)]
    busy: bool,
    /// Fill the available width with centred content (`expand: true`).
    #[live(false)]
    expand: bool,
    #[rust]
    area: Area,
    #[rust]
    hovered: bool,
    #[rust]
    pressed: bool,
}

impl PButton {
    fn colors(&self, p: &Palette) -> (Vec4, Vec4, Vec4) {
        let disabled = !self.enabled || self.busy;
        match self.variant {
            ButtonVariant::Primary => (p.accent, p.accent, p.on_tertiary),
            ButtonVariant::Secondary => (p.surface3, p.surface3, p.foreground),
            ButtonVariant::Outline => (Vec4::default(), p.border_accent, p.foreground),
            ButtonVariant::Ghost => (
                Vec4::default(),
                Vec4::default(),
                if self.hovered && !disabled { p.foreground } else { p.muted },
            ),
            ButtonVariant::Destructive => (
                p.destructive,
                p.destructive,
                if p.is_dark { vec4(1.0, 1.0, 1.0, 1.0) } else { p.surface0 },
            ),
        }
    }
}

impl Widget for PButton {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let disabled = !self.enabled || self.busy;
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(_) => {
                cx.set_cursor(if disabled { MouseCursor::Default } else { MouseCursor::Hand });
                self.hovered = true;
                self.redraw(cx);
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = false;
                self.redraw(cx);
            }
            Hit::FingerDown(fe) if !disabled && fe.is_primary_hit() => {
                self.pressed = true;
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                if std::mem::take(&mut self.pressed) && fe.is_over && !disabled && (!fe.device.is_touch() || fe.was_tap()) {
                    cx.widget_action(self.widget_uid(), &scope.path, PButtonAction::Clicked(fe.modifiers));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let t = theme::current_typography();
        let height = control_height(self.size);
        let (pad, radius) = match self.size {
            ControlSize::Xs | ControlSize::Sm => (12.0, 12.0),
            ControlSize::Md => (16.0, 16.0),
            ControlSize::Lg => (24.0, 16.0),
        };
        let icon_size = control_icon_size(self.size);
        let spec = TextSpec::new(Font::Regular, t.ui_size(if self.size == ControlSize::Xs { 12.0 } else { 14.0 }), p.foreground);
        let label = self.text.as_ref().to_string();
        let has_icon = !self.icon.as_ref().is_empty() || self.busy;
        let text_w = if label.is_empty() { 0.0 } else { self.painter.measure(cx, spec, &label).x };
        let content_w = text_w + if has_icon { icon_size + if label.is_empty() { 0.0 } else { 8.0 } } else { 0.0 };
        let natural = dvec2(content_w + pad * 2.0, height);
        let walk = Walk {
            width: if self.expand { Size::Fill } else if matches!(walk.width, Size::Fit) { Size::Fixed(natural.x) } else { walk.width },
            height: match walk.height {
                Size::Fit => Size::Fixed(natural.y),
                Size::Fixed(h) if cfg!(target_os = "android") => Size::Fixed(h.max(natural.y)),
                height => height,
            },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let (bg, border, fg) = self.colors(&p);
        let opacity = if !self.enabled || self.busy { 0.5 } else if self.pressed { 0.85 } else { 1.0 };
        self.area = self.painter.capture_box(cx, r, fade(bg, opacity), fade(border, opacity), 1.0, radius.min(r.size.y * 0.5));
        let fg = fade(fg, opacity);
        let mut x = r.pos.x + ((r.size.x - content_w) * 0.5).max(pad.min(r.size.x * 0.5));
        if has_icon {
            let icon_rect = rect(x, r.pos.y + (r.size.y - icon_size) * 0.5, icon_size, icon_size);
            if self.busy {
                self.painter.ring(cx, icon_rect, fg, 1.5, 0.25, clock::phase(900));
                clock::register(cx, self.area);
            } else {
                let icon = self.icon.as_ref().to_string();
                self.painter.icon(cx, icon_rect, &icon, icon_size, fg);
            }
            x += icon_size + 8.0;
        }
        if !label.is_empty() {
            let avail = (r.pos.x + r.size.x - pad - x).max(0.0);
            self.painter.text_in(cx, rect(x, r.pos.y, avail, r.size.y), TextSpec { color: fg, ..spec }, &label, 0.0);
        }
        DrawStep::done()
    }

    fn text(&self) -> String {
        self.text.as_ref().to_string()
    }

    fn set_text(&mut self, cx: &mut Cx, v: &str) {
        if self.text.as_ref() != v {
            self.text.as_mut_empty().push_str(v);
            self.redraw(cx);
        }
    }
}

impl PButtonRef {
    pub fn clicked(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|b| matches!(actions.find_widget_action(b.widget_uid()).cast_ref(), PButtonAction::Clicked(_)))
    }

    pub fn set_enabled(&self, cx: &mut Cx, enabled: bool) {
        if let Some(mut b) = self.borrow_mut()
            && b.enabled != enabled
        {
            b.enabled = enabled;
            b.redraw(cx);
        }
    }

    pub fn set_busy(&self, cx: &mut Cx, busy: bool) {
        if let Some(mut b) = self.borrow_mut()
            && b.busy != busy
        {
            b.busy = busy;
            b.redraw(cx);
        }
    }

    pub fn set_icon(&self, cx: &mut Cx, icon: &str) {
        if let Some(mut b) = self.borrow_mut()
            && b.icon.as_ref() != icon
        {
            b.icon.as_mut_empty().push_str(icon);
            b.redraw(cx);
        }
    }

    pub fn set_variant(&self, cx: &mut Cx, variant: ButtonVariant) {
        if let Some(mut b) = self.borrow_mut()
            && b.variant != variant
        {
            b.variant = variant;
            b.redraw(cx);
        }
    }

    pub fn rect(&self, cx: &Cx) -> Rect {
        self.borrow().map(|b| b.area.rect(cx)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// PIconButton

/// Original `icon-button-chrome`: square frame (26 desktop / 32 compact by default), 6px radius,
/// muted glyph, `interactionHighlight` fill while hovered/pressed, 0.5 opacity when disabled.
#[derive(Live, LiveHook, Widget)]
pub struct PIconButton {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live]
    icon: ArcStringMut,
    /// 0 = control default (32 compact, 26 desktop).
    #[live(0.0)]
    extent: f64,
    #[live(16.0)]
    icon_size: f64,
    #[live(6.0)]
    radius: f64,
    #[live(true)]
    enabled: bool,
    /// Glyph uses the foreground colour instead of muted.
    #[live(false)]
    strong: bool,
    #[live]
    tooltip: ArcStringMut,
    #[rust]
    area: Area,
    #[rust]
    hovered: bool,
    #[rust]
    pressed: bool,
}

impl Widget for PIconButton {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(_) => {
                cx.set_cursor(if self.enabled { MouseCursor::Hand } else { MouseCursor::Default });
                self.hovered = true;
                self.redraw(cx);
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = false;
                self.redraw(cx);
            }
            Hit::FingerDown(fe) if self.enabled && fe.is_primary_hit() => {
                self.pressed = true;
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                if std::mem::take(&mut self.pressed) && fe.is_over && self.enabled && (!fe.device.is_touch() || fe.was_tap()) {
                    cx.widget_action(self.widget_uid(), &scope.path, PButtonAction::Clicked(fe.modifiers));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let extent = if self.extent > 0.0 {
            self.extent
        } else if crate::common::is_compact_layout(cx) {
            32.0
        } else {
            26.0
        };
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(extent) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(extent) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let fill = if self.enabled && (self.hovered || self.pressed) { p.interaction_highlight } else { Vec4::default() };
        self.area = self.painter.capture_box(cx, r, fill, Vec4::default(), 0.0, self.radius);
        let color = if self.strong { p.foreground } else { p.muted };
        let icon = self.icon.as_ref().to_string();
        self.painter.icon(cx, r, &icon, self.icon_size, if self.enabled { color } else { fade(color, 0.5) });
        DrawStep::done()
    }
}

impl PIconButtonRef {
    pub fn clicked(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|b| matches!(actions.find_widget_action(b.widget_uid()).cast_ref(), PButtonAction::Clicked(_)))
    }

    /// Trigger rect for anchored menus.
    pub fn anchor(&self, cx: &Cx) -> Rect {
        self.borrow().map(|b| b.area.rect(cx)).unwrap_or_default()
    }

    pub fn set_icon(&self, cx: &mut Cx, icon: &str) {
        if let Some(mut b) = self.borrow_mut()
            && b.icon.as_ref() != icon
        {
            b.icon.as_mut_empty().push_str(icon);
            b.redraw(cx);
        }
    }

    pub fn set_enabled(&self, cx: &mut Cx, enabled: bool) {
        if let Some(mut b) = self.borrow_mut()
            && b.enabled != enabled
        {
            b.enabled = enabled;
            b.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PIcon

#[derive(Live, LiveHook, Widget)]
pub struct PIcon {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live]
    icon: ArcStringMut,
    #[live(16.0)]
    size: f64,
    #[live]
    color_role: TextRole,
    #[rust]
    color_override: Option<Vec4>,
}

impl Widget for PIcon {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(self.size) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(self.size) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let icon = self.icon.as_ref().to_string();
        let color = self.color_override.unwrap_or_else(|| self.color_role.color(&p));
        self.painter.icon(cx, r, &icon, self.size, color);
        DrawStep::done()
    }
}

impl PIconRef {
    pub fn set_icon(&self, cx: &mut Cx, icon: &str) {
        if let Some(mut i) = self.borrow_mut()
            && i.icon.as_ref() != icon
        {
            i.icon.as_mut_empty().push_str(icon);
            i.redraw(cx);
        }
    }

    pub fn set_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut i) = self.borrow_mut() {
            i.color_override = color;
            i.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PSwitch / PCheckbox

#[derive(Clone, Debug, DefaultNone)]
pub enum ToggleAction {
    Changed(bool),
    None,
}

/// Original `components/ui/switch.tsx`: 34x20 track, 16px thumb, surface3 → accent track.
#[derive(Live, LiveHook, Widget)]
pub struct PSwitch {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live(false)]
    value: bool,
    #[live(true)]
    enabled: bool,
    #[rust]
    area: Area,
}

impl Widget for PSwitch {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(_) if self.enabled => cx.set_cursor(MouseCursor::Hand),
            Hit::FingerUp(fe) if fe.is_over && self.enabled && fe.is_primary_hit() && (!fe.device.is_touch() || fe.was_tap()) => {
                self.value = !self.value;
                cx.widget_action(self.widget_uid(), &scope.path, ToggleAction::Changed(self.value));
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        // hitSlop: 4px horizontal / 6px vertical around the 34x20 track.
        let walk = Walk { width: Size::Fixed(42.0), height: Size::Fixed(32.0), ..walk };
        let r = cx.walk_turtle(walk);
        let opacity = if self.enabled { 1.0 } else { 0.5 };
        let track = rect(r.pos.x + 4.0, r.pos.y + 6.0, 34.0, 20.0);
        self.area = self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        self.painter.fill(cx, track, fade(if self.value { p.accent } else { p.surface3 }, opacity), 10.0);
        let thumb_x = if self.value { track.pos.x + 16.0 } else { track.pos.x + 2.0 };
        let thumb = if self.value { p.on_tertiary } else { vec4(1.0, 1.0, 1.0, 1.0) };
        self.painter.dot(cx, dvec2(thumb_x + 8.0, track.pos.y + 10.0), 16.0, fade(thumb, opacity));
        DrawStep::done()
    }
}

impl PSwitchRef {
    pub fn changed(&self, actions: &Actions) -> Option<bool> {
        let s = self.borrow()?;
        match actions.find_widget_action(s.widget_uid()).cast_ref() {
            ToggleAction::Changed(v) => Some(*v),
            _ => None,
        }
    }

    pub fn set_value(&self, cx: &mut Cx, value: bool) {
        if let Some(mut s) = self.borrow_mut()
            && s.value != value
        {
            s.value = value;
            s.redraw(cx);
        }
    }

    pub fn set_enabled(&self, cx: &mut Cx, enabled: bool) {
        if let Some(mut s) = self.borrow_mut() {
            s.enabled = enabled;
            s.redraw(cx);
        }
    }
}

/// 16px rounded checkbox with a Lucide check, accent when checked.
#[derive(Live, LiveHook, Widget)]
pub struct PCheckbox {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live(false)]
    value: bool,
    #[live(true)]
    enabled: bool,
    #[live]
    text: ArcStringMut,
    #[rust]
    area: Area,
}

impl Widget for PCheckbox {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(_) if self.enabled => cx.set_cursor(MouseCursor::Hand),
            Hit::FingerUp(fe) if fe.is_over && self.enabled && fe.is_primary_hit() && (!fe.device.is_touch() || fe.was_tap()) => {
                self.value = !self.value;
                cx.widget_action(self.widget_uid(), &scope.path, ToggleAction::Changed(self.value));
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let t = theme::current_typography();
        let spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.foreground);
        let label = self.text.as_ref().to_string();
        let label_w = if label.is_empty() { 0.0 } else { self.painter.measure(cx, spec, &label).x + 8.0 };
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(16.0 + label_w) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(28.0) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let opacity = if self.enabled { 1.0 } else { 0.5 };
        self.area = self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        let boxr = rect(r.pos.x, r.pos.y + (r.size.y - 16.0) * 0.5, 16.0, 16.0);
        if self.value {
            self.painter.fill(cx, boxr, fade(p.accent, opacity), 4.0);
            self.painter.icon(cx, boxr, "check", 12.0, fade(p.on_tertiary, opacity));
        } else {
            self.painter.outline(cx, boxr, Vec4::default(), fade(p.border_accent, opacity), 1.0, 4.0);
        }
        if !label.is_empty() {
            self.painter.text_in(cx, rect(boxr.pos.x + 24.0, r.pos.y, r.size.x - 24.0, r.size.y), TextSpec { color: fade(p.foreground, opacity), ..spec }, &label, 0.0);
        }
        DrawStep::done()
    }
}

impl PCheckboxRef {
    pub fn changed(&self, actions: &Actions) -> Option<bool> {
        let s = self.borrow()?;
        match actions.find_widget_action(s.widget_uid()).cast_ref() {
            ToggleAction::Changed(v) => Some(*v),
            _ => None,
        }
    }

    pub fn set_value(&self, cx: &mut Cx, value: bool) {
        if let Some(mut s) = self.borrow_mut()
            && s.value != value
        {
            s.value = value;
            s.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// PSegmented

#[derive(Clone, Debug, DefaultNone)]
pub enum SegmentAction {
    Changed(String),
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub value: String,
    pub label: String,
    pub icon: Option<String>,
    pub enabled: bool,
}

/// Original `components/ui/segmented-control.tsx`: transparent row, 4px gap, selected segment
/// surface3, hover surface2, muted → foreground label.
#[derive(Live, LiveHook, Widget)]
pub struct PSegmented {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live]
    size: ControlSize,
    #[live(false)]
    hide_labels: bool,
    #[live(false)]
    expand: bool,
    #[rust]
    segments: Vec<Segment>,
    #[rust]
    selected: String,
    #[rust]
    hits: HitMap<usize>,
    #[rust]
    area: Area,
    #[rust]
    hovered: Option<usize>,
}

impl Widget for PSegmented {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hovered = self.hits.at(e.abs);
                if hovered != self.hovered {
                    self.hovered = hovered;
                    self.redraw(cx);
                }
                cx.set_cursor(if hovered.is_some() { MouseCursor::Hand } else { MouseCursor::Default });
            }
            Hit::FingerHoverOut(_) => {
                self.hovered = None;
                self.redraw(cx);
            }
            Hit::FingerUp(fe) if fe.is_over && (!fe.device.is_touch() || fe.was_tap()) => {
                if let Some(index) = self.hits.at(fe.abs) {
                    let segment = &self.segments[index];
                    if segment.enabled && segment.value != self.selected {
                        self.selected = segment.value.clone();
                        cx.widget_action(self.widget_uid(), &scope.path, SegmentAction::Changed(self.selected.clone()));
                        self.redraw(cx);
                    }
                }
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let t = theme::current_typography();
        let (height, pad, radius, font, icon) = match self.size {
            ControlSize::Xs => (24.0, 8.0, 12.0, 12.0, 12.0),
            ControlSize::Sm => (28.0, 8.0, 12.0, 14.0, 14.0),
            _ => (38.0, 12.0, 16.0, 14.0, 16.0),
        };
        let outer = control_height(self.size);
        let height = height + outer - match self.size {
            ControlSize::Xs => 28.0,
            ControlSize::Sm => 32.0,
            _ => 44.0,
        };
        let spec = TextSpec::new(Font::Regular, t.ui_size(font), p.muted);
        let widths: Vec<f64> = self
            .segments
            .iter()
            .map(|s| {
                let label = if self.hide_labels { 0.0 } else { self.painter.measure(cx, spec, &s.label).x };
                let icon_w = if s.icon.is_some() { icon + if label > 0.0 { 4.0 } else { 0.0 } } else { 0.0 };
                label + icon_w + pad * 2.0
            })
            .collect();
        let natural: f64 = widths.iter().sum::<f64>() + 4.0 * widths.len().saturating_sub(1) as f64;
        let walk = Walk {
            width: if self.expand { Size::Fill } else if matches!(walk.width, Size::Fit) { Size::Fixed(natural) } else { walk.width },
            height: Size::Fixed(outer),
            ..walk
        };
        let r = cx.walk_turtle(walk);
        self.area = self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        self.hits.clear();
        let count = self.segments.len().max(1) as f64;
        let even = (r.size.x - 4.0 * (count - 1.0)) / count;
        let mut x = r.pos.x;
        for (index, segment) in self.segments.iter().enumerate() {
            let w = if self.expand { even } else { widths[index] };
            let seg = rect(x, r.pos.y + (outer - height) * 0.5, w, height);
            let selected = segment.value == self.selected;
            let bg = if selected {
                p.surface3
            } else if self.hovered == Some(index) && segment.enabled {
                p.surface2
            } else {
                Vec4::default()
            };
            let opacity = if segment.enabled { 1.0 } else { 0.5 };
            self.painter.fill(cx, seg, bg, radius);
            let fg = fade(if selected { p.foreground } else { p.muted }, opacity);
            let label_w = if self.hide_labels { 0.0 } else { self.painter.measure(cx, spec, &segment.label).x };
            let icon_w = if segment.icon.is_some() { icon + if label_w > 0.0 { 4.0 } else { 0.0 } } else { 0.0 };
            let mut cx_pos = seg.pos.x + ((w - label_w - icon_w) * 0.5).max(pad);
            if let Some(name) = &segment.icon {
                self.painter.icon(cx, rect(cx_pos, seg.pos.y + (height - icon) * 0.5, icon, icon), name, icon, fg);
                cx_pos += icon_w;
            }
            if !self.hide_labels {
                let avail = (seg.pos.x + w - pad - cx_pos).max(0.0);
                self.painter.text_in(cx, rect(cx_pos, seg.pos.y, avail, height), TextSpec { color: fg, ..spec }, &segment.label, 0.0);
            }
            self.hits.push(seg, index);
            x += w + 4.0;
        }
        DrawStep::done()
    }
}

impl PSegmentedRef {
    pub fn set_options(&self, cx: &mut Cx, segments: Vec<Segment>, selected: &str) {
        if let Some(mut s) = self.borrow_mut()
            && (s.segments != segments || s.selected != selected)
        {
            s.segments = segments;
            s.selected = selected.to_string();
            s.redraw(cx);
        }
    }

    pub fn changed(&self, actions: &Actions) -> Option<String> {
        let s = self.borrow()?;
        match actions.find_widget_action(s.widget_uid()).cast_ref() {
            SegmentAction::Changed(v) => Some(v.clone()),
            _ => None,
        }
    }

    pub fn selected(&self) -> String {
        self.borrow().map(|s| s.selected.clone()).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// Divider

#[derive(Live, LiveHook, Widget)]
pub struct Divider {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live(false)]
    vertical: bool,
    #[live]
    border_role: BorderRole,
}

impl Widget for Divider {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let walk = if self.vertical { Walk { width: Size::Fixed(1.0), ..walk } } else { walk };
        let r = cx.walk_turtle(walk);
        let color = self.border_role.color(&p).unwrap_or(p.border);
        self.painter.fill(cx, r, color, 0.0);
        DrawStep::done()
    }
}

// ---------------------------------------------------------------------------------------------
// Status marks (Flutter `chat/chrome/status_mark.dart`)

/// `SidebarStateBucket`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusBucket {
    NeedsInput,
    Failed,
    Running,
    Attention,
    Done,
}

/// `STATUS_BUCKET_ORDER`.
pub const STATUS_BUCKET_ORDER: [StatusBucket; 5] = [
    StatusBucket::NeedsInput,
    StatusBucket::Failed,
    StatusBucket::Attention,
    StatusBucket::Running,
    StatusBucket::Done,
];

/// `deriveAgentStateBucket`.
pub fn derive_status_bucket(
    status: Option<&str>,
    pending_permission_count: usize,
    requires_attention: bool,
    attention_reason: Option<&str>,
) -> StatusBucket {
    if pending_permission_count > 0 || attention_reason == Some("permission") {
        return StatusBucket::NeedsInput;
    }
    if status == Some("error") || attention_reason == Some("error") {
        return StatusBucket::Failed;
    }
    if status == Some("running") {
        return StatusBucket::Running;
    }
    if requires_attention {
        return StatusBucket::Attention;
    }
    StatusBucket::Done
}

/// Bucket of a daemon agent row.
pub fn agent_status_bucket(agent: &Value) -> StatusBucket {
    let pending = agent.get("pendingPermissions").and_then(Value::as_array).map_or(0, Vec::len);
    let pending = pending.max(crate::common::json::int(agent, "pendingPermissionCount", 0).max(0) as usize);
    derive_status_bucket(
        agent.get("status").and_then(Value::as_str),
        pending,
        crate::common::json::boolean(agent, "requiresAttention", false),
        agent.get("attentionReason").and_then(Value::as_str),
    )
}

/// `statusDotColor` (the louder statusDot band).
pub fn status_dot_color(p: &Palette, bucket: StatusBucket, show_done_as_inactive: bool) -> Option<Vec4> {
    let dark = p.is_dark;
    match bucket {
        StatusBucket::NeedsInput => Some(theme::argb(if dark { 0xffdb932e } else { 0xffb37824 })),
        StatusBucket::Failed => Some(theme::argb(if dark { 0xfff7796d } else { 0xfff12e2f })),
        StatusBucket::Running => Some(p.running),
        StatusBucket::Attention => Some(theme::argb(if dark { 0xff35c264 } else { 0xff299f51 })),
        StatusBucket::Done => show_done_as_inactive.then_some(p.border),
    }
}

pub const STATUS_FILLED_DOT_SIZE: f64 = 6.0;
pub const STATUS_DOT_SIZE: f64 = STATUS_FILLED_DOT_SIZE + 2.0;
pub const STATUS_ALERT_SIZE: f64 = 12.0;
pub const STATUS_RING_GAP: f64 = 1.5;
pub const STATUS_RING_STROKE: f64 = 1.5;
pub const STATUS_RING_SIZE: f64 = STATUS_FILLED_DOT_SIZE + (STATUS_RING_GAP + STATUS_RING_STROKE) * 2.0;
pub const STATUS_RING_FRAME_SIZE: f64 = STATUS_RING_SIZE + 2.0;
const RING_PERIOD_MS: u128 = 900;

/// `StatusRing`: a 6px dot inside a 1.5px ring whose quarter turns every 900ms.
pub fn paint_status_ring(painter: &mut Painter, cx: &mut Cx2d, center: DVec2, p: &Palette, backdrop: Option<Vec4>) {
    if let Some(backdrop) = backdrop {
        painter.dot(cx, center, STATUS_RING_FRAME_SIZE, backdrop);
    }
    let size = STATUS_RING_SIZE;
    let r = rect(center.x - size * 0.5, center.y - size * 0.5, size, size);
    painter.ring(cx, r, theme::with_alpha(p.running, 0.3), STATUS_RING_STROKE, 1.0, 0.0);
    // Flutter arcs start at -π/2 (12 o'clock); the shader's angle 0 is 3 o'clock.
    painter.ring(cx, r, theme::with_alpha(p.running, 0.9), STATUS_RING_STROKE, 0.25, clock::phase(RING_PERIOD_MS) - 0.25);
    painter.dot(cx, center, STATUS_FILLED_DOT_SIZE, p.running);
}

/// `StatusIcon` overlay drawn at the bottom-right corner of an icon of `icon_rect`.
/// Returns true when it animates (the caller registers its area with the clock).
pub fn paint_status_badge(
    painter: &mut Painter,
    cx: &mut Cx2d,
    icon_rect: Rect,
    bucket: Option<StatusBucket>,
    p: &Palette,
    backdrop: Vec4,
) -> bool {
    let right = icon_rect.pos.x + icon_rect.size.x;
    let bottom = icon_rect.pos.y + icon_rect.size.y;
    match bucket {
        Some(StatusBucket::Running) => {
            let offset = -2.0 - (STATUS_RING_FRAME_SIZE - STATUS_DOT_SIZE) * 0.5;
            let center = dvec2(right - offset - STATUS_RING_FRAME_SIZE * 0.5, bottom - offset - STATUS_RING_FRAME_SIZE * 0.5);
            paint_status_ring(painter, cx, center, p, Some(backdrop));
            true
        }
        Some(bucket @ (StatusBucket::Failed | StatusBucket::Attention)) => {
            let color = status_dot_color(p, bucket, false).unwrap_or(p.running);
            let center = dvec2(right + 2.0 - STATUS_DOT_SIZE * 0.5, bottom + 2.0 - STATUS_DOT_SIZE * 0.5);
            painter.dot(cx, center, STATUS_DOT_SIZE, backdrop);
            painter.dot(cx, center, STATUS_DOT_SIZE - 2.0, color);
            false
        }
        Some(StatusBucket::NeedsInput) => {
            let fill = status_dot_color(p, StatusBucket::NeedsInput, false).unwrap_or(p.warning);
            let center = dvec2(right + 3.0 - STATUS_ALERT_SIZE * 0.5, bottom + 3.0 - STATUS_ALERT_SIZE * 0.5);
            painter.dot(cx, center, STATUS_ALERT_SIZE - 2.0, fill);
            painter.icon(
                cx,
                rect(center.x - STATUS_ALERT_SIZE * 0.5, center.y - STATUS_ALERT_SIZE * 0.5, STATUS_ALERT_SIZE, STATUS_ALERT_SIZE),
                "circle-alert",
                STATUS_ALERT_SIZE,
                p.surface0,
            );
            false
        }
        _ => false,
    }
}

/// Standalone status dot/ring widget (`set_status`).
#[derive(Live, LiveHook, Widget)]
pub struct StatusMark {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live(false)]
    show_done_as_inactive: bool,
    #[rust]
    bucket: Option<StatusBucket>,
    #[rust]
    area: Area,
}

impl Widget for StatusMark {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let r = cx.walk_turtle(walk);
        self.area = self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        let center = dvec2(r.pos.x + r.size.x * 0.5, r.pos.y + r.size.y * 0.5);
        match self.bucket {
            Some(StatusBucket::Running) => {
                paint_status_ring(&mut self.painter, cx, center, &p, None);
                clock::register(cx, self.area);
            }
            Some(bucket) => {
                if let Some(color) = status_dot_color(&p, bucket, self.show_done_as_inactive) {
                    self.painter.dot(cx, center, STATUS_FILLED_DOT_SIZE, color);
                }
            }
            None => {}
        }
        DrawStep::done()
    }
}

impl StatusMarkRef {
    pub fn set_status(&self, cx: &mut Cx, bucket: Option<StatusBucket>) {
        if let Some(mut s) = self.borrow_mut()
            && s.bucket != bucket
        {
            s.bucket = bucket;
            s.redraw(cx);
        }
    }
}

/// Indeterminate spinner (Flutter `CircularProgressIndicator` / `LoadingSpinner`), ticking on
/// the indicator clock only while drawn.
#[derive(Live, LiveHook, Widget)]
pub struct Spinner {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[live(1.5)]
    stroke: f64,
    #[live]
    color_role: TextRole,
    #[rust]
    area: Area,
}

impl Widget for Spinner {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let r = cx.walk_turtle(walk);
        self.area = self.painter.capture_box(cx, r, Vec4::default(), Vec4::default(), 0.0, 0.0);
        let color = match self.color_role {
            TextRole::Body => p.muted,
            role => role.color(&p),
        };
        self.painter.ring(cx, r, color, self.stroke, 0.3, clock::phase(900));
        clock::register(cx, self.area);
        DrawStep::done()
    }
}

// ---------------------------------------------------------------------------------------------
// PTextField

#[derive(Clone, Debug, DefaultNone)]
pub enum PTextFieldAction {
    Changed(String),
    Returned(String),
    Escaped,
    Focus(bool),
    None,
}

/// Original themed `InputDecoration`: surface2 fill, transparent border that turns
/// `borderAccent` while focused/hovered, 44/32 tall, 8/6 radius. Wraps Makepad `TextInput`
/// (IME, selection, clipboard, undo) and reports its focus scope to the shortcut router.
#[derive(Live, LiveHook, Widget)]
pub struct PTextField {
    #[deref]
    view: View,
    #[live]
    placeholder: ArcStringMut,
    #[live(false)]
    password: bool,
    #[live(false)]
    mono: bool,
    /// The sidebar search look (`session_sidebar.dart`): sidebar fill, 6px divider border
    /// (muted while focused), 8px padding, 70% white hint on dark palettes.
    #[live(false)]
    bare: bool,
    /// The select search row look (`showPaseoSelect`): no fill, no border, 12px vertical padding.
    #[live(false)]
    plain: bool,
    /// `TextField(minLines/maxLines)` multi-line entry: Enter inserts a newline (Makepad
    /// inserts one on Shift+Enter, so plain Enter is passed on as Shift+Enter).
    #[live(false)]
    multiline: bool,
    /// Focus scope reported while focused (`editable`, `message-input`, `terminal`).
    #[live]
    focus_scope: ArcStringMut,
    #[rust]
    theme_epoch: u64,
    #[rust]
    focused: bool,
    #[rust]
    applied_placeholder: String,
}

impl Widget for PTextField {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        let input = self.view.text_input(id!(input));
        if self.focused && !input.area().is_valid(cx) {
            input.release_focus(cx);
            self.focused = false;
            if let Some(ui) = scope.data.get_mut::<UiContext>() {
                ui.push(UiEffect::SetFocusScope { owner: self.widget_uid(), scope: None });
            }
        }
        if self.multiline
            && let Event::KeyDown(key) = event
            && key.key_code == KeyCode::ReturnKey
            && !key.modifiers.shift
        {
            let mut newline = *key;
            newline.modifiers.shift = true;
            self.view.handle_event(cx, &Event::KeyDown(newline), scope);
        } else {
            self.view.handle_event(cx, event, scope);
        }
        if let Event::Actions(actions) = event {
            let input = self.view.text_input(id!(input));
            let uid = self.widget_uid();
            if let Some(text) = input.changed(actions) {
                cx.widget_action(uid, &scope.path, PTextFieldAction::Changed(text));
            }
            if let Some((text, _)) = input.returned(actions) {
                cx.widget_action(uid, &scope.path, PTextFieldAction::Returned(text));
            }
            if input.escaped(actions) {
                cx.widget_action(uid, &scope.path, PTextFieldAction::Escaped);
            }
            for action in actions.filter_widget_actions(input.widget_uid()) {
                let focus = match action.cast_ref::<TextInputAction>() {
                    TextInputAction::KeyFocus => Some(true),
                    TextInputAction::KeyFocusLost => Some(false),
                    _ => None,
                };
                if let Some(focus) = focus {
                    self.focused = focus;
                    cx.widget_action(uid, &scope.path, PTextFieldAction::Focus(focus));
                    let scope_name = if self.focus_scope.as_ref().is_empty() { "editable".to_string() } else { self.focus_scope.as_ref().to_string() };
                    if let Some(ui) = scope.data.get_mut::<UiContext>() {
                        ui.push(UiEffect::SetFocusScope { owner: uid, scope: focus.then_some(scope_name) });
                    }
                    self.apply_theme(cx, true);
                }
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.apply_theme(cx, false);
        self.view.draw_walk(cx, scope, walk)
    }

    fn text(&self) -> String {
        self.view.text_input(id!(input)).text()
    }

    fn set_text(&mut self, cx: &mut Cx, v: &str) {
        let input = self.view.text_input(id!(input));
        if input.text() != v {
            input.set_text(cx, v);
        }
    }
}

impl PTextField {
    fn apply_theme(&mut self, cx: &mut Cx, force: bool) {
        let input = self.view.text_input(id!(input));
        if self.applied_placeholder != self.placeholder.as_ref() {
            self.applied_placeholder = self.placeholder.as_ref().to_string();
            input.set_empty_text(cx, self.applied_placeholder.clone());
        }
        if input.is_password() != self.password {
            input.set_is_password(cx, self.password);
        }
        let now = theme::epoch();
        if !force && self.theme_epoch == now {
            return;
        }
        self.theme_epoch = now;
        let p = theme::current_palette();
        let t = theme::current_typography();
        let compact = t.mobile;
        let radius = if compact && !self.bare { 8.0 } else { 6.0 };
        let (pad_v, pad_h) = if self.plain {
            (12.0, 0.0)
        } else if self.bare {
            (8.0, 8.0)
        } else {
            (((if compact { 44.0 } else { 32.0 }) - 20.0 - 2.0) / 2.0, if compact { 16.0 } else { 12.0 })
        };
        let clear = vec4(0.0, 0.0, 0.0, 0.0);
        let (fill, border, hover, active, hint) = if self.plain {
            (clear, clear, clear, clear, p.muted)
        } else if self.bare {
            let hint = if p.is_dark { theme::argb(0xb3ffffff) } else { p.muted };
            let border = if self.focused { p.muted } else { p.border };
            (p.sidebar, border, border, p.muted, hint)
        } else {
            let border = if self.focused { p.border_accent } else { clear };
            (p.surface2, border, p.border_accent, p.border_accent, p.muted)
        };
        let font_size = t.ui_pt(14.0) as f64;
        input.apply_over(
            cx,
            live! {
                padding: { left: (pad_h), right: (pad_h), top: (pad_v), bottom: (pad_v) }
                draw_bg: {
                    color: (fill), color_hover: (fill), color_focus: (fill),
                    color_down: (fill), color_empty: (fill), color_disabled: (fade(fill, 0.5)),
                    // `Sdf2d::box` draws twice the radius it is given.
                    border_radius: (radius * 0.5), border_size: 1.0,
                    border_color_1: (border), border_color_2: (border),
                    border_color_1_hover: (hover), border_color_2_hover: (hover),
                    border_color_1_focus: (active), border_color_2_focus: (active),
                    border_color_1_down: (active), border_color_2_down: (active),
                    border_color_1_empty: (border), border_color_2_empty: (border),
                    border_color_1_disabled: (border), border_color_2_disabled: (border),
                }
                draw_text: {
                    color: (p.foreground), color_hover: (p.foreground), color_focus: (p.foreground),
                    color_down: (p.foreground), color_disabled: (fade(p.foreground, 0.5)),
                    color_empty: (hint), color_empty_hover: (hint), color_empty_focus: (hint),
                    text_style: { font_size: (font_size) }
                }
                draw_cursor: { color: (p.foreground) }
            },
        );
        input.apply_over(cx, &theme::text_selection(theme::with_alpha(p.accent, 0.35)));
        input.redraw(cx);
    }
}

impl PTextFieldRef {
    fn input(&self) -> Option<TextInputRef> {
        self.borrow().map(|f| f.view.text_input(id!(input)))
    }

    pub fn changed(&self, actions: &Actions) -> Option<String> {
        let f = self.borrow()?;
        match actions.find_widget_action(f.widget_uid()).cast_ref() {
            PTextFieldAction::Changed(text) => Some(text.clone()),
            _ => None,
        }
    }

    pub fn returned(&self, actions: &Actions) -> Option<String> {
        let f = self.borrow()?;
        match actions.find_widget_action(f.widget_uid()).cast_ref() {
            PTextFieldAction::Returned(text) => Some(text.clone()),
            _ => None,
        }
    }

    pub fn escaped(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|f| matches!(actions.find_widget_action(f.widget_uid()).cast_ref(), PTextFieldAction::Escaped))
    }

    pub fn focus_changed(&self, actions: &Actions) -> Option<bool> {
        let f = self.borrow()?;
        match actions.find_widget_action(f.widget_uid()).cast_ref() {
            PTextFieldAction::Focus(focus) => Some(*focus),
            _ => None,
        }
    }

    pub fn set_key_focus(&self, cx: &mut Cx) {
        if let Some(input) = self.input() {
            input.set_key_focus(cx);
        }
    }

    /// Stops native input and cursor blinking when the owning modal closes,
    /// including after the field's last painted area has expired.
    pub fn release_focus(&self, cx: &mut Cx) {
        if let Some(input) = self.input() {
            input.release_focus(cx);
        }
    }

    pub fn set_placeholder(&self, cx: &mut Cx, text: &str) {
        if let Some(mut f) = self.borrow_mut()
            && f.placeholder.as_ref() != text
        {
            f.placeholder.as_mut_empty().push_str(text);
            f.redraw(cx);
        }
    }

    pub fn set_password(&self, cx: &mut Cx, password: bool) {
        if let Some(mut f) = self.borrow_mut()
            && f.password != password
        {
            f.password = password;
            f.redraw(cx);
        }
    }

    pub fn select_all(&self, cx: &mut Cx) {
        if let Some(input) = self.input()
            && let Some(mut inner) = input.borrow_mut()
        {
            inner.select_all(cx);
        }
    }

    pub fn set_read_only(&self, cx: &mut Cx, read_only: bool) {
        if let Some(input) = self.input() {
            input.set_is_read_only(cx, read_only);
        }
    }

    /// Switches to the select search row look ([`PTextField::plain`]).
    pub fn set_plain(&self, cx: &mut Cx) {
        if let Some(mut f) = self.borrow_mut()
            && !f.plain
        {
            f.plain = true;
            f.apply_theme(cx, true);
        }
    }

    /// Multi-line entry (see [`PTextField::multiline`]); the field then fills the height its
    /// parent turtle gives it (the prompt sizes it to 4–12 lines).
    pub fn set_multiline(&self, cx: &mut Cx, multiline: bool) {
        if let Some(mut f) = self.borrow_mut() {
            f.multiline = multiline;
            if multiline {
                f.view.apply_over(cx, live! { height: Fill, input = { height: Fill } });
            }
        }
    }
}
