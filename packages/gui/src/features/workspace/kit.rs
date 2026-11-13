//! Live components shared by the workspace, schedules, logs and command-center features
//! (Flutter `ui/controls.dart` PaseoButton variants, `SettingsCard`/`SettingsRow`,
//! `WorkspaceAlert`, `HostStatusDot`, `StatusBadge`, field triggers, form fields).
//!
//! Every component repaints from the runtime [`Palette`] (`theme::current_palette()`), so a
//! theme switch applies without reloading DSL: text is `PLabel`, containers are `Surface`,
//! and the button, input, dot, badge, project icon and alert frame below resolve their
//! colours on draw. Icons are Lucide names (`common::icons`).

use makepad_widgets::*;

use crate::common::paint::{Font, Painter, TextSpec, rect};
use crate::common::widgets::{PLabelWidgetExt, PLabelWidgetRefExt, TextRole};
use crate::common::{UiContext, UiEffect, clock, overlay};
use crate::theme::{self, ControlSize, Palette, control_height, control_icon_size, fade, with_alpha};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::common::paint::PaseoPainter;

    // ---- Text (authored px at the 14px UI base) -------------------------------------

    pub WaText = <PLabel> { width: Fill, role: Body }
    pub WaTextFit = <WaText> { width: Fit }
    pub WaLine = <WaText> { ellipsis: true }
    pub WaLineFit = <WaTextFit> {}
    pub WaMedium = <WaText> { weight: Medium }
    pub WaMuted = <WaText> { role: Muted }
    pub WaMutedLine = <WaMuted> { ellipsis: true }
    pub WaSmall = <WaText> { role: SmallMuted }
    pub WaSmallLine = <WaSmall> { ellipsis: true }
    pub WaSmallFit = <WaSmall> { width: Fit }
    pub WaError = <WaText> { role: FieldError }
    pub WaDangerText = <WaText> { role: Danger }
    pub WaTitle = <WaText> { role: Title }
    pub WaCode = <WaText> { role: Mono }
    pub WaIcon = {{WaIcon}} {
        width: Fit, height: Fit
        size: 16., role: Muted
        painter: <PaseoPainter> {}
    }

    // ---- Buttons -------------------------------------------------------------------

    pub WaButtonBase = {{WaButton}} {
        width: Fit, height: Fit,
        painter: <PaseoPainter> {}
    }
    // `PaseoButton` (original `components/ui/button.tsx`): bordered pill per variant.
    pub WaButton = <WaButtonBase> { kind: Secondary, size: Sm }
    pub WaButtonPrimary = <WaButton> { kind: Primary }
    pub WaButtonOutline = <WaButton> { kind: Outline }
    pub WaButtonGhost = <WaButton> { kind: Ghost }
    pub WaButtonDanger = <WaButton> { kind: Danger }
    pub WaButtonSmall = <WaButtonGhost> { size: Xs }
    pub WaButtonOutlineSmall = <WaButtonOutline> { size: Xs }
    pub WaButtonPrimarySmall = <WaButtonPrimary> { size: Xs }
    // Original `icon-button-chrome`: 26px (32 compact) square, muted glyph.
    pub WaIconButton = <WaButtonBase> { kind: Icon }
    // `_FieldTrigger` / `SelectFieldTrigger`: surface2 field with a trailing chevron.
    pub WaFieldButton = <WaButtonBase> { kind: Field, width: Fill, trailing: "chevron-down" }
    // Desktop new-workspace badge trigger (28px, borderless, muted value).
    pub WaBadgeButton = <WaButtonBase> { kind: Badge }
    pub WaLinkButton = <WaButtonBase> { kind: Link }
    pub WaGlyphButton = <WaButtonBase> { kind: Glyph }
    pub WaMenuButton = <WaButtonBase> { kind: Menu, icon: "ellipsis-vertical" }
    pub WaToolButton = <WaButtonBase> { kind: Tool }
    pub WaFilterButton = <WaButtonBase> { kind: Filter, trailing: "chevron-down" }
    pub WaSubmitButton = <WaButtonBase> { kind: Submit, margin: { left: 4. } }
    pub WaRoundButton = <WaButtonBase> { kind: Round }
    pub WaToolbarButton = <WaButtonBase> { kind: Toolbar }
    // `FieldTrigger` (`new-workspace/form/field-trigger.tsx`): desktop badge, compact field.
    pub WaTrigger = {{WaTrigger}} {
        width: Fit, height: Fit
        painter: <PaseoPainter> {}
    }

    // ---- Inputs --------------------------------------------------------------------

    pub WaInputBase = {{WaInput}} {}
    // Themed `InputDecoration` (`PTextField` geometry): surface2, accent border on focus.
    pub WaInput = <WaInputBase> {
        width: Fill, height: Fit, flow: Down
        input = <TextInput> {
            width: Fill, height: Fit
            margin: 0.
            empty_text: ""
            // The original fields' 1.4 line height: a 20px line at 14px.
            draw_text: { text_style: <PASEO_FONT_REGULAR> { line_spacing: 1.43 } }
        }
    }
    pub WaInputMono = <WaInput> {
        mono: true
        input = { draw_text: { text_style: <PASEO_FONT_MONO> { line_spacing: 1.43 } } }
    }
    // `SettingsTextAreaCard`: multi-line input on a bordered settings card.
    pub WaTextArea = <WaInput> { look: Area }
    // Transparent inline field (search rows, composer prompt); padding from the instance.
    pub WaBareInput = <WaInput> { look: Bare }

    pub WaToggle = <PSwitch> {}

    // ---- Containers ----------------------------------------------------------------

    pub WaCard = <Surface> {
        width: Fill, height: Fit, flow: Down
        role: Card, border_role: Border, radius: 8.
    }
    pub WaRule = <Divider> {}
    // `SettingsRow`: label, hint and trailing control.
    pub WaRow = <View> {
        width: Fill, height: Fit, flow: Right, spacing: 12.
        padding: 16.
        align: { y: 0.5 }
        body = <View> {
            width: Fill, height: Fit, flow: Down, spacing: 4.
            label = <WaLine> {}
            hint = <View> { width: Fill, height: Fit, text = <WaSmall> {} }
        }
        controls = <View> { width: Fit, height: Fit, flow: Right, spacing: 4., align: { y: 0.5 } }
    }
    // `FormField`: muted small label, 8px gap, control, optional hint/error.
    pub WaField = <View> {
        width: Fill, height: Fit, flow: Down, spacing: 8.
        label = <WaSmallLine> {}
    }
    pub WaSectionTitle = <WaText> { margin: { top: 0., bottom: 8. } }

    // `WorkspaceAlert`: transparent, danger/warning border at half alpha, 16px radius, a
    // 14px (16px large) circle-x / triangle-alert icon, title in the tone, muted description
    // and actions indented under the title.
    pub WaFrameBase = {{WaFrame}} {
        painter: <PaseoPainter> {}
    }
    pub WaAlert = <WaFrameBase> {
        width: Fill, height: Fit, flow: Down, spacing: 2.
        padding: { left: 16., right: 16., top: 12., bottom: 12. }
        tone: Danger
        head = <View> {
            width: Fill, height: Fit, flow: Right, spacing: 12.
            icon = <WaIcon> { size: 14., icon: "circle-x" }
            title = <View> { width: Fill, height: Fit, text = <WaMedium> {} }
            lead = <View> { width: Fill, height: Fit, visible: false, text = <WaMuted> {} }
        }
        body = <View> {
            width: Fill, height: Fit, flow: Down
            padding: { left: 26. }
            description = <View> { width: Fill, height: Fit, text = <WaMuted> {} }
            actions = <View> { width: Fill, height: Fit, flow: RightWrap, spacing: 8., margin: { top: 8. } }
        }
    }
    pub WaWarningAlert = <WaAlert> { tone: Warning }

    // `errorCard`: red-100 fill, red-800 text in every theme (the original is constant).
    pub WaErrorCard = <View> {
        width: Fill, height: Fit, padding: 12., show_bg: true
        draw_bg: { color: #fee2e2 }
        text = <WaText> {}
    }

    // `HostStatusDot`: 8px dot.
    pub WaStatusDot = {{WaDot}} {
        width: 14., height: 14.
        painter: <PaseoPainter> {}
    }

    // `ProjectIconSquare`: identity fill with the white initial.
    pub WaProjectIcon = {{WaProjectIcon}} {
        width: 16., height: 16.
        painter: <PaseoPainter> {}
    }

    // `StatusBadge`.
    pub WaBadge = {{WaBadge}} {
        width: Fit, height: Fit
        painter: <PaseoPainter> {}
    }

    // A non-virtualized list of `template` instances for short dynamic lists inside a
    // scroll view (setup commands, scripts, schedule rows).
    pub WaRepeat = {{WaRepeat}} {
        width: Fill, height: Fit, flow: Down
    }

    pub WaSpinner = <Spinner> {
        width: 20., height: 20.
    }

    // Makepad `Modal` with the adaptive dialog barrier (`0x8c000000`) whose content may
    // yield draw steps (a `PortalList` inside a sheet; `Modal` swallows them in `draw_all`).
    pub WaModal = {{WaModal}} {
        width: Fill
        height: Fill
        flow: Overlay
        align: { x: 0.5, y: 0.5 }
        scroll_body: body
        draw_bg: {
            fn pixel(self) -> vec4 {
                return vec4(0., 0., 0., 0.0)
            }
        }
        bg_view: <View> {
            width: Fill
            height: Fill
            show_bg: true
            draw_bg: {
                fn pixel(self) -> vec4 {
                    return vec4(0., 0., 0., 0.55)
                }
            }
        }
        content: <ScrollYView> {
            flow: Overlay
            width: Fit
            height: Fit
        }
    }

    // `showAdaptiveSheet` dialog + `SheetContents` header: surface1, 1px surface2 border,
    // 12px radius; 24/16/16/16 header with the medium base title and the 16px close glyph,
    // a surface2 rule under it. Sheet bodies pad 24px; footers add `WaSheetRule` above them.
    pub WaSheetRule = <Surface> { width: Fill, height: 1., role: Surface2 }
    pub WaSheet = <Surface> {
        width: 520., height: Fit, flow: Down
        role: Surface1, border_role: Border, radius: 12.
        header = <View> {
            width: Fill, height: Fit, flow: Right, spacing: 8.
            padding: { left: 24., right: 16., top: 16., bottom: 16. }
            title = <WaMedium> { margin: { top: 6. } }
            close = <WaIconButton> { icon: "x", radius: 8., width: 32., height: 32. }
        }
        <WaSheetRule> {}
    }
}

// ---------------------------------------------------------------------------------------------
// WaButton

/// The `icon` name drawing the phone `MobileMenuIcon` (three bars) instead of a glyph.
pub const MOBILE_MENU: &str = "mobile-menu";

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum WaButtonKind {
    #[pick]
    Secondary,
    Primary,
    Outline,
    Ghost,
    Danger,
    Field,
    Badge,
    Link,
    Icon,
    /// Bare 14px glyph with 4px padding and a tooltip (`_glyphButton`).
    Glyph,
    /// Composer `sendButton`: 28px accent circle, or a 12px-padded pill when labelled.
    Submit,
    /// `ComposerHoverGlyph`: 28px round muted glyph, surface2 while hovered.
    Round,
    /// `control.tsx` toolbarControl: 28px, 8px inset, 4px gaps, surface2 hover, surface0 press.
    Toolbar,
    /// `ScheduleKebabMenu` trigger: 4px padding and radius, surface2 + foreground on hover.
    Menu,
    /// `HostFilter` pill: surface1, 1px border, 6px radius, 12/6 insets, 6px gaps, medium label.
    Filter,
    /// Pane `ToolbarButton`: 20px (32 compact) frame, 14px (18) muted glyph, 6px radius, the
    /// interaction highlight while hovered, pressed or selected.
    Tool,
    /// Command Center scope chip: 28px, borderAccent, surface2, 6px radius, 8px padding, muted
    /// 14px glyph, medium label and a muted 12px trailing glyph.
    Chip,
}

/// Field control size (`PaseoControlSize` sm 32px / md 44px). `Adaptive` is the forms'
/// `isCompactLayout ? md : sm`.
#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum WaFieldSize {
    Adaptive,
    Sm,
    #[pick]
    Md,
}

impl WaFieldSize {
    /// Whether this resolves to the 44px md geometry.
    pub fn is_md(self, compact: bool) -> bool {
        match self {
            Self::Adaptive => compact,
            Self::Sm => false,
            Self::Md => true,
        }
    }
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaButtonAction {
    Clicked(KeyModifiers),
    None,
}

/// One button primitive for every `PaseoButton` variant, field trigger, badge trigger, link and
/// icon button these features draw. Pill variants follow `PButton` (original `button.tsx`:
/// 1px bordered, 8px icon gap, 0.85 pressed / 0.5 disabled opacity); `Icon` follows
/// `icon-button-chrome`; `Field` follows the themed `InputDecoration` with a trailing chevron.
/// Unlike `PButton` it can hide itself and take a per-instance text colour.
#[derive(Live, LiveHook, Widget)]
pub struct WaButton {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[live]
    text: ArcStringMut,
    /// Leading Lucide icon.
    #[live]
    icon: ArcStringMut,
    /// Trailing Lucide icon (`chevron-down` on field triggers).
    #[live]
    trailing: ArcStringMut,
    #[live]
    kind: WaButtonKind,
    #[live]
    size: ControlSize,
    #[live(true)]
    enabled: bool,
    #[live(false)]
    busy: bool,
    /// Icon buttons: glyph in the foreground colour instead of muted.
    #[live(false)]
    strong: bool,
    /// `_pillOrGhost`: a full-radius borderAccent outline around the button.
    #[live(false)]
    pill: bool,
    /// Corner radius override (0: the kind's).
    #[live(0.0)]
    radius: f64,
    /// Field triggers: sm/md geometry.
    #[live]
    field_size: WaFieldSize,
    /// Extra space before the content (a widget overlaid on the leading edge).
    #[live(0.0)]
    lead_space: f64,
    #[live]
    tooltip: ArcStringMut,
    #[rust]
    color: Option<Vec4>,
    /// Leading icon tint only (active provider features).
    #[rust]
    icon_color: Option<Vec4>,
    /// A host status dot in the leading slot instead of the icon.
    #[rust]
    dot: Option<super::host::Connection>,
    /// Toolbar toggles (`ToolbarButton.selected`).
    #[rust]
    selected: bool,
    #[area]
    #[rust]
    area: Area,
    #[rust]
    hovered: bool,
    #[rust]
    pressed: bool,
}

struct ButtonGeometry {
    height: f64,
    pad: f64,
    radius: f64,
    icon: f64,
    font: f64,
}

impl WaButton {
    /// `compact` is the layout width (icon chrome, adaptive fields), `mobile` the platform
    /// (composer glyph sizes).
    fn geometry(&self, compact: bool, mobile: bool) -> ButtonGeometry {
        match self.kind {
            WaButtonKind::Icon => {
                let extent = if compact { 32.0 } else { 26.0 };
                ButtonGeometry { height: extent, pad: (extent - 16.0) * 0.5, radius: 6.0, icon: 16.0, font: 14.0 }
            }
            WaButtonKind::Field => {
                let md = self.field_size.is_md(compact);
                ButtonGeometry {
                    height: if md { 44.0 } else { 32.0 },
                    pad: if md { 16.0 } else { 12.0 },
                    radius: if md { 8.0 } else { 6.0 },
                    icon: 16.0,
                    font: 14.0,
                }
            }
            WaButtonKind::Glyph => ButtonGeometry { height: 22.0, pad: 4.0, radius: 4.0, icon: 14.0, font: 12.0 },
            WaButtonKind::Submit => ButtonGeometry { height: 28.0, pad: 12.0, radius: 14.0, icon: if mobile { 20.0 } else { 16.0 }, font: 14.0 },
            WaButtonKind::Round => ButtonGeometry { height: 28.0, pad: 6.0, radius: 14.0, icon: if mobile { 20.0 } else { 16.0 }, font: 14.0 },
            WaButtonKind::Toolbar => ButtonGeometry { height: 28.0, pad: 8.0, radius: 14.0, icon: if mobile { 20.0 } else { 16.0 }, font: 14.0 },
            WaButtonKind::Menu => ButtonGeometry { height: 22.0, pad: 4.0, radius: 4.0, icon: 14.0, font: 14.0 },
            WaButtonKind::Chip => ButtonGeometry { height: 28.0, pad: 8.0, radius: 6.0, icon: 14.0, font: 14.0 },
            WaButtonKind::Tool => {
                let (extent, glyph) = if compact { (32.0, 18.0) } else { (20.0, 14.0) };
                ButtonGeometry { height: extent, pad: (extent - glyph) * 0.5, radius: 6.0, icon: glyph, font: 12.0 }
            }
            WaButtonKind::Filter => ButtonGeometry { height: 32.0, pad: 12.0, radius: 6.0, icon: 14.0, font: 14.0 },
            WaButtonKind::Badge => ButtonGeometry { height: 28.0, pad: 8.0, radius: 14.0, icon: 14.0, font: 14.0 },
            WaButtonKind::Link => ButtonGeometry { height: 24.0, pad: 4.0, radius: 4.0, icon: 12.0, font: 12.0 },
            _ => {
                let (pad, radius) = match self.size {
                    ControlSize::Xs | ControlSize::Sm => (12.0, 12.0),
                    ControlSize::Md => (16.0, 16.0),
                    ControlSize::Lg => (24.0, 16.0),
                };
                ButtonGeometry {
                    height: control_height(self.size),
                    pad,
                    radius,
                    icon: control_icon_size(self.size),
                    font: if self.size == ControlSize::Xs { 12.0 } else { 14.0 },
                }
            }
        }
    }

    /// (fill, border, content) for the current state.
    fn colors(&self, p: &Palette) -> (Vec4, Vec4, Vec4) {
        let disabled = !self.enabled || self.busy;
        let lit = (self.hovered || self.pressed) && !disabled;
        let clear = Vec4::default();
        let (fill, border, content) = match self.kind {
            WaButtonKind::Primary => (p.accent, p.accent, p.on_tertiary),
            WaButtonKind::Secondary => (p.surface3, p.surface3, p.foreground),
            WaButtonKind::Outline => (clear, p.border_accent, p.foreground),
            WaButtonKind::Ghost => (clear, clear, if lit { p.foreground } else { p.muted }),
            WaButtonKind::Danger => (p.destructive, p.destructive, if p.is_dark { vec4(1.0, 1.0, 1.0, 1.0) } else { p.surface0 }),
            WaButtonKind::Field => (p.surface2, if lit { p.muted } else { clear }, p.foreground),
            WaButtonKind::Badge => (if lit { p.surface2 } else { clear }, clear, if lit { p.foreground } else { p.muted }),
            WaButtonKind::Link => (clear, clear, if lit { p.foreground } else { p.muted }),
            WaButtonKind::Glyph => (clear, clear, p.muted),
            WaButtonKind::Submit => (p.accent, clear, p.on_tertiary),
            WaButtonKind::Round => (if lit { p.surface2 } else { clear }, clear, if lit { p.foreground } else { p.muted }),
            WaButtonKind::Menu => (if lit { p.surface2 } else { clear }, clear, if lit { p.foreground } else { p.muted }),
            WaButtonKind::Chip => (p.surface2, p.border_accent, p.foreground),
            WaButtonKind::Tool => {
                let highlight = if p.is_dark { theme::argb(0x14ffffff) } else { theme::argb(0x0f000000) };
                (if lit || (self.selected && !disabled) { highlight } else { clear }, clear, p.muted)
            }
            WaButtonKind::Filter => (
                if self.pressed && !disabled { p.surface3 } else if lit { p.surface2 } else { p.surface1 },
                p.border,
                p.foreground,
            ),
            WaButtonKind::Toolbar => (
                if self.pressed && !disabled { p.surface0 } else if lit { p.surface2 } else { clear },
                clear,
                p.muted,
            ),
            WaButtonKind::Icon => (
                if lit { p.interaction_highlight } else { clear },
                clear,
                if self.strong || lit { p.foreground } else { p.muted },
            ),
        };
        (fill, border, self.color.unwrap_or(content))
    }
}

impl Widget for WaButton {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if !self.visible {
            return;
        }
        let disabled = !self.enabled || self.busy;
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(fe) => {
                cx.set_cursor(if disabled { MouseCursor::Default } else { MouseCursor::Hand });
                self.hovered = true;
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.as_ref().is_empty()
                {
                    overlay::tooltip(ui, Some(self.tooltip.as_ref()), fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerHoverOut(fe) => {
                self.hovered = false;
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.as_ref().is_empty()
                {
                    overlay::tooltip(ui, None, fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerDown(fe) if !disabled && fe.is_primary_hit() => {
                self.pressed = true;
                // A press dismisses the hover tooltip (the click may open an overlay that
                // swallows the hover-out).
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.as_ref().is_empty()
                {
                    overlay::tooltip(ui, None, fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                if std::mem::take(&mut self.pressed) && fe.is_over && !disabled && (!fe.device.is_touch() || fe.was_tap()) {
                    cx.widget_action(self.widget_uid(), &scope.path, WaButtonAction::Clicked(fe.modifiers));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let t = theme::current_typography();
        let g = self.geometry(crate::common::is_compact_layout(cx), t.mobile);
        let disabled = !self.enabled || self.busy;
        let font = if matches!(self.kind, WaButtonKind::Submit | WaButtonKind::Filter | WaButtonKind::Chip) { Font::Medium } else { Font::Regular };
        let spec = TextSpec::new(font, t.ui_size(g.font), p.foreground);
        let label = self.text.as_ref().to_string();
        let icon = self.icon.as_ref().to_string();
        let trailing = self.trailing.as_ref().to_string();
        let has_icon = !icon.is_empty() || self.busy || self.dot.is_some();
        let text_w = if label.is_empty() { 0.0 } else { self.painter.measure(cx, spec, &label).x };
        let gap = match self.kind {
            WaButtonKind::Link | WaButtonKind::Toolbar => 4.0,
            WaButtonKind::Filter | WaButtonKind::Chip => 6.0,
            _ => 8.0,
        };
        let trail_size = if self.kind == WaButtonKind::Chip { 12.0 } else { g.icon };
        let lead_w = if has_icon { g.icon + if label.is_empty() { 0.0 } else { gap } } else { 0.0 };
        let trail_w = if trailing.is_empty() { 0.0 } else { gap + trail_size };
        let content_w = self.lead_space + lead_w + text_w + trail_w;
        let square = matches!(
            self.kind,
            WaButtonKind::Icon
                | WaButtonKind::Glyph
                | WaButtonKind::Submit
                | WaButtonKind::Round
                | WaButtonKind::Toolbar
                | WaButtonKind::Menu
                | WaButtonKind::Tool
        ) && label.is_empty()
            && trailing.is_empty();
        let natural_w = if square { g.height } else { content_w + g.pad * 2.0 };
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(natural_w) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(g.height) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let (fill, border, content) = self.colors(&p);
        let opacity = if disabled { 0.5 } else if self.pressed { 0.85 } else { 1.0 };
        let base_radius = if self.radius > 0.0 { self.radius } else { g.radius };
        let (border, radius) = if self.pill { (p.border_accent, r.size.y * 0.5) } else { (border, base_radius.min(r.size.y * 0.5)) };
        let border_w = if border.w > 0.0 { 1.0 } else { 0.0 };
        // The pill outline is a foreground decoration: it keeps full opacity.
        let border = if self.pill { border } else { fade(border, opacity) };
        self.area = self.painter.capture_box(cx, r, fade(fill, opacity), border, border_w, radius);
        let content = fade(content, opacity);
        // Field triggers start at the padding; square icon buttons centre the glyph exactly
        // (a 20px mobile glyph in a 28px circle); every other kind centres its content but
        // keeps at least the padding.
        let start = if self.kind == WaButtonKind::Field {
            r.pos.x + g.pad
        } else if square {
            r.pos.x + (r.size.x - content_w) * 0.5
        } else {
            r.pos.x + ((r.size.x - content_w) * 0.5).max(g.pad.min(r.size.x * 0.5))
        };
        let mut x = start + self.lead_space;
        if has_icon {
            let icon_rect = rect(x, r.pos.y + (r.size.y - g.icon) * 0.5, g.icon, g.icon);
            if self.busy {
                let ring = if self.kind == WaButtonKind::Submit { rect(icon_rect.pos.x + (g.icon - 14.0) * 0.5, icon_rect.pos.y + (g.icon - 14.0) * 0.5, 14.0, 14.0) } else { icon_rect };
                self.painter.ring(cx, ring, content, if self.kind == WaButtonKind::Submit { 2.0 } else { 1.5 }, 0.25, clock::phase(900));
                clock::register(cx, self.area);
            } else if let Some(status) = self.dot {
                self.painter.dot(cx, icon_rect.center(), 8.0, fade(status_color(&p, status), opacity));
            } else if icon == MOBILE_MENU {
                // Original MobileMenuIcon: 16/16/8 x 1.5 lines in a 16x12 box.
                let left = icon_rect.pos.x + (icon_rect.size.x - 16.0) * 0.5;
                let top = icon_rect.pos.y + (icon_rect.size.y - 12.0) * 0.5;
                for (index, width) in [16.0, 16.0, 8.0].into_iter().enumerate() {
                    self.painter.fill(cx, rect(left, top + index as f64 * 5.25, width, 1.5), content, 1.0);
                }
            } else {
                let base = if self.kind == WaButtonKind::Field { fade(p.muted, opacity) } else { content };
                let tint = self.icon_color.map(|c| fade(c, opacity)).unwrap_or(base);
                self.painter.icon(cx, icon_rect, &icon, g.icon, tint);
            }
            x += g.icon + if label.is_empty() { 0.0 } else { gap };
        }
        let right = r.pos.x + r.size.x - g.pad;
        if !trailing.is_empty() {
            let trail_rect = rect(right - trail_size, r.pos.y + (r.size.y - trail_size) * 0.5, trail_size, trail_size);
            let muted = fade(p.muted, opacity);
            self.painter.icon(cx, trail_rect, &trailing, trail_size, muted);
        }
        if !label.is_empty() {
            let avail = (right - trail_w - x).max(0.0);
            self.painter.text_in(cx, rect(x, r.pos.y, avail, r.size.y), TextSpec { color: content, ..spec }, &label, 0.0);
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

impl WaButtonRef {
    pub fn clicked(&self, actions: &Actions) -> bool {
        self.clicked_with(actions).is_some()
    }

    /// The modifiers of a click this frame.
    pub fn clicked_with(&self, actions: &Actions) -> Option<KeyModifiers> {
        let b = self.borrow()?;
        match actions.find_widget_action(b.widget_uid()).cast_ref() {
            WaButtonAction::Clicked(modifiers) => Some(*modifiers),
            _ => None,
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

    pub fn set_trailing(&self, cx: &mut Cx, icon: &str) {
        if let Some(mut b) = self.borrow_mut()
            && b.trailing.as_ref() != icon
        {
            b.trailing.as_mut_empty().push_str(icon);
            b.redraw(cx);
        }
    }

    /// Text and icon colour; `None` restores the variant's.
    pub fn set_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut b) = self.borrow_mut()
            && b.color != color
        {
            b.color = color;
            b.redraw(cx);
        }
    }

    pub fn set_dot(&self, cx: &mut Cx, dot: Option<super::host::Connection>) {
        if let Some(mut b) = self.borrow_mut()
            && b.dot != dot
        {
            b.dot = dot;
            b.redraw(cx);
        }
    }

    pub fn set_selected(&self, cx: &mut Cx, selected: bool) {
        if let Some(mut b) = self.borrow_mut()
            && b.selected != selected
        {
            b.selected = selected;
            b.redraw(cx);
        }
    }

    pub fn set_pill(&self, cx: &mut Cx, pill: bool) {
        if let Some(mut b) = self.borrow_mut()
            && b.pill != pill
        {
            b.pill = pill;
            b.redraw(cx);
        }
    }

    pub fn set_icon_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut b) = self.borrow_mut()
            && b.icon_color != color
        {
            b.icon_color = color;
            b.redraw(cx);
        }
    }

    pub fn set_kind(&self, cx: &mut Cx, kind: WaButtonKind) {
        if let Some(mut b) = self.borrow_mut()
            && b.kind != kind
        {
            b.kind = kind;
            b.redraw(cx);
        }
    }

    /// Hover tooltip (desktop only; the shell never shows tooltips on compact layouts).
    pub fn set_tooltip(&self, cx: &mut Cx, text: &str) {
        if let Some(mut b) = self.borrow_mut()
            && b.tooltip.as_ref() != text
        {
            b.tooltip.as_mut_empty().push_str(text);
            b.redraw(cx);
        }
    }

    /// Trigger rect for anchored menus.
    pub fn rect(&self, cx: &Cx) -> Rect {
        self.borrow().map(|b| b.area.rect(cx)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// WaTrigger

/// The 16px leading slot of a [`WaTrigger`].
#[derive(Clone, Debug, PartialEq)]
pub enum TriggerLead {
    /// A muted 14px Lucide glyph.
    Icon(&'static str),
    /// `ProjectIconSquare` at 16px: identity fill and initial.
    Project { fill: Vec4, initial: String },
    /// `HostStatusDot`.
    Status(super::host::Connection),
}

/// Original `FieldTrigger`. Desktop `badge`: borderless 28px pill (≤240px, 8px inset, 16px
/// radius, 4px gap, muted value, surface2 hover / surface0 press, tooltip). Compact `field`:
/// 56px surface1 box (12px inset, 12px radius, 8px gap) with the muted small label 2px over
/// the foreground value (6px icon gap), surface2 hover/press. 16px icon box, extra-muted 14px
/// chevron 1px down, 0.6 opacity disabled.
#[derive(Live, LiveHook, Widget)]
pub struct WaTrigger {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[live(true)]
    enabled: bool,
    #[rust]
    label: String,
    #[rust]
    value: String,
    #[rust]
    tooltip: String,
    #[rust]
    lead: Option<TriggerLead>,
    #[area]
    #[rust]
    area: Area,
    #[rust]
    hovered: bool,
    #[rust]
    pressed: bool,
    /// Cap of a Fit desktop badge below [`TRIGGER_MAX_W`], so a row can share its width.
    #[rust]
    max_width: Option<f64>,
}

/// Widest a Fit desktop trigger badge grows for a long value.
const TRIGGER_MAX_W: f64 = 240.0;

impl WaTrigger {
    /// Width the desktop badge wants for its value, before any cap: 8px
    /// insets, the 16px lead, 4px gaps, the value and the 14px chevron.
    fn badge_width(&mut self, cx: &mut Cx) -> f64 {
        let spec = TextSpec::new(Font::Regular, theme::current_typography().ui_size(14.0), Vec4::default());
        8.0 * 2.0 + 16.0 + 4.0 + self.painter.measure(cx, spec, &self.value).x + 4.0 + 14.0
    }
}

impl Widget for WaTrigger {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if !self.visible {
            return;
        }
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(fe) => {
                cx.set_cursor(if self.enabled { MouseCursor::Hand } else { MouseCursor::Default });
                self.hovered = true;
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.is_empty()
                {
                    overlay::tooltip(ui, Some(&self.tooltip), fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerHoverOut(fe) => {
                self.hovered = false;
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.is_empty()
                {
                    overlay::tooltip(ui, None, fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerDown(fe) if self.enabled && fe.is_primary_hit() => {
                self.pressed = true;
                if let Some(ui) = scope.data.get_mut::<UiContext>()
                    && !self.tooltip.is_empty()
                {
                    overlay::tooltip(ui, None, fe.abs);
                }
                self.redraw(cx);
            }
            Hit::FingerUp(fe) => {
                if std::mem::take(&mut self.pressed) && fe.is_over && self.enabled && (!fe.device.is_touch() || fe.was_tap()) {
                    cx.widget_action(self.widget_uid(), &scope.path, WaButtonAction::Clicked(fe.modifiers));
                }
                self.redraw(cx);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let t = theme::current_typography();
        let field = crate::common::is_compact_layout(cx);
        let opacity = if self.enabled { 1.0 } else { 0.6 };
        let lit = self.enabled && (self.hovered || self.pressed);
        let rest = if field { p.surface1 } else { Vec4::default() };
        let fill = if !self.enabled {
            rest
        } else if self.pressed {
            if field { p.surface2 } else { p.surface0 }
        } else if lit {
            p.surface2
        } else {
            rest
        };
        let value_spec = TextSpec::new(Font::Regular, t.ui_size(14.0), fade(if field { p.foreground } else { p.muted }, opacity));
        let (height, pad, radius, gap) = if field { (56.0, 12.0, 12.0, 8.0) } else { (28.0, 8.0, 16.0, 4.0) };
        let natural = self.badge_width(cx).min(self.max_width.unwrap_or(TRIGGER_MAX_W).min(TRIGGER_MAX_W));
        let walk = Walk {
            width: if field { Size::Fill } else if matches!(walk.width, Size::Fit) { Size::Fixed(natural) } else { walk.width },
            height: Size::Fixed(height),
            ..walk
        };
        let r = cx.walk_turtle(walk);
        self.area = self.painter.capture_box(cx, r, fade(fill, opacity), Vec4::default(), 0.0, radius);
        let chevron = rect(r.pos.x + r.size.x - pad - 14.0, r.pos.y + (r.size.y - 14.0) * 0.5 + 1.0, 14.0, 14.0);
        self.painter.icon(cx, chevron, "chevron-down", 14.0, fade(p.muted, opacity));
        let (row_y, value_x_gap) = if field {
            let label_spec = TextSpec::new(Font::Regular, t.ui_size(12.0), fade(p.muted, opacity));
            let label_h = self.painter.line_height(cx, label_spec);
            let value_h = 20.0f64.max(self.painter.line_height(cx, value_spec));
            let top = r.pos.y + (r.size.y - label_h - 2.0 - value_h) * 0.5;
            let label = self.label.clone();
            self.painter.text_in(cx, rect(r.pos.x + pad, top, r.size.x - pad * 2.0 - 14.0 - gap, label_h), label_spec, &label, 0.0);
            (top + label_h + 2.0 + (value_h - 16.0) * 0.5, 6.0)
        } else {
            (r.pos.y + (r.size.y - 16.0) * 0.5, gap)
        };
        let lead = rect(r.pos.x + pad, row_y, 16.0, 16.0);
        match &self.lead {
            Some(TriggerLead::Icon(name)) => self.painter.icon(cx, lead, name, 14.0, fade(p.muted, opacity)),
            Some(TriggerLead::Project { fill, initial }) => {
                self.painter.fill(cx, lead, fade(*fill, opacity), 4.0);
                let spec = TextSpec::new(Font::Medium, 10.0, fade(vec4(1.0, 1.0, 1.0, 1.0), opacity));
                let initial = initial.clone();
                self.painter.text_in(cx, lead, spec, &initial, 0.5);
            }
            Some(TriggerLead::Status(status)) => {
                self.painter.dot(cx, lead.center(), 8.0, fade(status_color(&p, *status), opacity));
            }
            None => {}
        }
        let value_x = lead.pos.x + 16.0 + value_x_gap;
        let value_w = (chevron.pos.x - gap - value_x).max(0.0);
        let value = self.value.clone();
        self.painter.text_in(cx, rect(value_x, row_y - 2.0, value_w, 20.0), value_spec, &value, 0.0);
        DrawStep::done()
    }
}

impl WaTriggerRef {
    pub fn clicked(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|b| matches!(actions.find_widget_action(b.widget_uid()).cast_ref(), WaButtonAction::Clicked(_)))
    }

    /// Label (compact field only), value, tooltip (desktop badge only) and leading slot.
    pub fn set(&self, cx: &mut Cx, label: &str, value: &str, tooltip: &str, lead: TriggerLead) {
        if let Some(mut b) = self.borrow_mut()
            && (b.label != label || b.value != value || b.tooltip != tooltip || b.lead.as_ref() != Some(&lead))
        {
            b.label = label.to_owned();
            b.value = value.to_owned();
            b.tooltip = tooltip.to_owned();
            b.lead = Some(lead);
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

    /// Width the desktop badge wants for its current value, before any cap.
    pub fn badge_width(&self, cx: &mut Cx) -> f64 {
        self.borrow_mut().map(|mut b| b.badge_width(cx)).unwrap_or(0.0)
    }

    /// Caps a Fit desktop badge below its 240px maximum; `None` restores it.
    pub fn set_max_width(&self, cx: &mut Cx, max_width: Option<f64>) {
        if let Some(mut b) = self.borrow_mut()
            && b.max_width != max_width
        {
            b.max_width = max_width;
            b.redraw(cx);
        }
    }

    /// Trigger rect for anchored menus.
    pub fn rect(&self, cx: &Cx) -> Rect {
        self.borrow().map(|b| b.area.rect(cx)).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------------------
// WaInput

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum WaInputLook {
    #[pick]
    Field,
    Area,
    Bare,
}

/// Makepad `TextInput` (IME, selection, clipboard, undo) themed from the palette on every
/// theme change, reporting its focus scope to the shortcut router like `PTextField`.
#[derive(Live, LiveHook, Widget)]
pub struct WaInput {
    #[deref]
    view: View,
    #[live]
    look: WaInputLook,
    #[live(false)]
    mono: bool,
    /// Extra right padding for an inline action laid over the field (`suffixIcon`).
    #[live(0.0)]
    trailing_space: f64,
    #[live]
    field_size: WaFieldSize,
    /// The composer prompt's surface4 hint instead of muted.
    #[live(false)]
    faint_hint: bool,
    /// Typed content (the composer prompt) uses the content font size, not the 14px UI size.
    #[live(false)]
    content: bool,
    /// Multi-line fields keep 12px vertical padding (`paseoInputDecoration(multiline)`).
    #[live(false)]
    multiline: bool,
    /// The (theme epoch, md) the styling was applied for.
    #[rust]
    applied: Option<(u64, bool)>,
    /// The single-line text height the input measured, for the exact control height.
    #[rust]
    line_height: Option<f64>,
    /// (theme epoch, empty) the text colour was applied for.
    #[rust]
    empty_applied: Option<(u64, bool)>,
}

impl Widget for WaInput {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event {
            let input = self.view.text_input(id!(input));
            for action in actions.filter_widget_actions(input.widget_uid()) {
                let focus = match action.cast_ref::<TextInputAction>() {
                    TextInputAction::KeyFocus => Some(true),
                    TextInputAction::KeyFocusLost => Some(false),
                    _ => None,
                };
                if let Some(focus) = focus
                    && let Some(ui) = scope.data.get_mut::<UiContext>()
                {
                    ui.push(UiEffect::SetFocusScope {
                        owner: self.widget_uid(),
                        scope: focus.then(|| "editable".to_string()),
                    });
                }
            }
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.apply_theme(cx);
        self.apply_empty(cx);
        let step = self.view.draw_walk(cx, scope, walk);
        self.settle_height(cx);
        step
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

impl WaInput {
    /// The muted placeholder (`hintStyle`). Makepad's TextInput only reaches `color_empty`
    /// through its animator, which starts in the non-empty state, so the base colour follows
    /// the emptiness instead.
    fn apply_empty(&mut self, cx: &mut Cx) {
        let input = self.view.text_input(id!(input));
        let empty = input.text().is_empty();
        let key = (theme::epoch(), empty);
        if self.empty_applied == Some(key) {
            return;
        }
        self.empty_applied = Some(key);
        let p = theme::current_palette();
        // The shader mixes `color_focus` over the empty colour, so a focused empty field
        // would show its hint in the text colour.
        let color = if empty { if self.faint_hint { p.surface4 } else { p.muted } } else { p.foreground };
        input.apply_over(cx, live! { draw_text: { color: (color), color_focus: (color), color_hover: (color), color_down: (color) } });
    }

    /// Vertical padding giving the 32 / 44px control (`paseoInputDecoration`).
    fn field_pad(&self, md: bool) -> f64 {
        let target = if md { 44.0 } else { 32.0 };
        ((target - 2.0 - self.line_height.unwrap_or(20.0)) / 2.0).max(2.0)
    }

    /// After a draw, learns the input's real line height and re-pads single-line fields once
    /// so they reach the control height exactly.
    fn settle_height(&mut self, cx: &mut Cx2d) {
        if self.look != WaInputLook::Field || self.multiline || self.line_height.is_some() {
            return;
        }
        let Some((_, md)) = self.applied else { return };
        let input = self.view.text_input(id!(input));
        let height = input.area().rect(cx).size.y;
        if height <= 0.0 || input.text().contains('\n') {
            return;
        }
        let pad = self.field_pad(md);
        self.line_height = Some((height - 2.0 * pad - 2.0).max(8.0));
        let pad = self.field_pad(md);
        input.apply_over(cx, live! { padding: { top: (pad), bottom: (pad) } });
        input.redraw(cx);
    }

    fn apply_theme(&mut self, cx: &mut Cx) {
        let md = self.field_size.is_md(crate::common::is_compact_layout(cx));
        let key = (theme::epoch(), md);
        if self.applied == Some(key) {
            return;
        }
        self.applied = Some(key);
        self.empty_applied = None;
        let p = theme::current_palette();
        let t = theme::current_typography();
        let input = self.view.text_input(id!(input));
        let font_size = f64::from(if self.content {
            theme::Typography::pt(t.content_font_size)
        } else {
            t.ui_pt(if self.mono { 12.0 } else { 14.0 })
        });
        let clear = Vec4::default();
        // `Sdf2d::box` (the TextInput shader) draws twice the radius it is given.
        let (fill, border, active, radius) = match self.look {
            // Focus border on the surface2 fill: muted (border_accent is ~1.1:1 against it).
            WaInputLook::Field => (p.surface2, clear, p.muted, if md { 8.0 } else { 6.0 }),
            WaInputLook::Area => (p.surface1, p.border, p.border, 8.0),
            WaInputLook::Bare => (clear, clear, clear, 0.0),
        };
        input.apply_over(
            cx,
            live! {
                draw_bg: {
                    color: (fill), color_hover: (fill), color_focus: (fill),
                    color_down: (fill), color_empty: (fill), color_disabled: (fade(fill, 0.5)),
                    border_radius: (radius * 0.5), border_size: 1.0,
                    border_color_1: (border), border_color_2: (border),
                    border_color_1_hover: (active), border_color_2_hover: (active),
                    border_color_1_focus: (active), border_color_2_focus: (active),
                    border_color_1_down: (active), border_color_2_down: (active),
                    border_color_1_empty: (border), border_color_2_empty: (border),
                    border_color_1_disabled: (border), border_color_2_disabled: (border),
                }
                draw_text: {
                    color: (p.foreground), color_hover: (p.foreground), color_focus: (p.foreground),
                    color_down: (p.foreground), color_disabled: (fade(p.foreground, 0.5)),
                    color_empty: (p.muted), color_empty_hover: (p.muted), color_empty_focus: (p.muted),
                    text_style: { font_size: (font_size) }
                }
                draw_cursor: { color: (p.foreground) }
            },
        );
        input.apply_over(cx, &theme::text_selection(with_alpha(p.accent, 0.35)));
        match self.look {
            WaInputLook::Field => {
                let pad_v = if self.multiline { 12.0 } else { self.field_pad(md) };
                let pad_h = if md { 16.0 } else { 12.0 };
                let pad_r = pad_h + self.trailing_space;
                input.apply_over(cx, live! { padding: { left: (pad_h), right: (pad_r), top: (pad_v), bottom: (pad_v) } });
            }
            // The card's 16/12 padding plus the field's 16/12 content padding.
            WaInputLook::Area => {
                input.apply_over(cx, live! { padding: { left: 32., right: 32., top: 24., bottom: 24. } });
            }
            WaInputLook::Bare => {}
        }
        input.redraw(cx);
    }
}

impl WaInputRef {
    /// The wrapped Makepad input.
    pub fn input(&self) -> TextInputRef {
        self.borrow().map(|f| f.view.text_input(id!(input))).unwrap_or_else(|| WidgetRef::empty().as_text_input())
    }

    pub fn changed(&self, actions: &Actions) -> Option<String> {
        self.input().changed(actions)
    }

    pub fn returned(&self, actions: &Actions) -> Option<(String, KeyModifiers)> {
        self.input().returned(actions)
    }

    pub fn escaped(&self, actions: &Actions) -> bool {
        self.input().escaped(actions)
    }

    pub fn key_down_unhandled(&self, actions: &Actions) -> Option<KeyEvent> {
        self.input().key_down_unhandled(actions)
    }

    pub fn set_key_focus(&self, cx: &mut Cx) {
        self.input().set_key_focus(cx);
    }

    pub fn key_focus(&self, cx: &Cx) -> bool {
        self.input().key_focus(cx)
    }

    pub fn set_empty_text(&self, cx: &mut Cx, text: String) {
        let input = self.input();
        if input.empty_text() != text {
            input.set_empty_text(cx, text);
        }
    }

    pub fn set_is_read_only(&self, cx: &mut Cx, read_only: bool) {
        self.input().set_is_read_only(cx, read_only);
    }

    pub fn select_all(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.input().borrow_mut() {
            inner.select_all(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WaFrame (`WorkspaceAlert`)

#[derive(Live, LiveHook, Clone, Copy, Debug, PartialEq, Eq)]
#[live_ignore]
pub enum WaTone {
    #[pick]
    Danger,
    Warning,
}

/// A view outlined in its tone at half alpha with a 16px radius; icon and title take the tone.
#[derive(Live, LiveHook, Widget)]
pub struct WaFrame {
    #[deref]
    view: View,
    #[live]
    painter: Painter,
    #[live]
    tone: WaTone,
    /// The `large` alert (24/16 insets from the instance, 16px icon).
    #[live(false)]
    large: bool,
    #[rust]
    painted: Option<(WaTone, u64)>,
}

impl Widget for WaFrame {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        let p = theme::current_palette();
        let (accent, icon) = match self.tone {
            WaTone::Danger => (p.danger, "circle-x"),
            WaTone::Warning => (p.warning, "triangle-alert"),
        };
        let key = (self.tone, theme::epoch());
        if self.painted != Some(key) {
            self.painted = Some(key);
            let size = if self.large { 16.0 } else { 14.0 };
            let glyph = self.view.wa_icon(id!(head.icon));
            glyph.set_icon(cx, icon);
            glyph.set_color(cx, Some(accent));
            glyph.set_size(cx, size);
            self.view.widget(id!(body)).apply_over(cx, live! { padding: { left: (size + 12.0) } });
            self.view.plabel(id!(head.title.text)).set_color(cx, Some(accent));
        }
        let actions = self.view.view(id!(body.actions));
        let has_actions = actions.child_count() > 0;
        actions.set_visible(cx, has_actions);
        let has_description = self.view.widget(id!(body.description)).visible();
        self.view.widget(id!(body)).set_visible(cx, has_actions || has_description);
        let step = self.view.draw_walk(cx, scope, walk);
        if step.is_done() {
            let r = self.view.area().rect(cx);
            self.painter.outline(cx, r, Vec4::default(), with_alpha(accent, accent.w * 0.5), 1.0, 16.0);
        }
        step
    }
}

// ---------------------------------------------------------------------------------------------
// WaIcon

/// A Lucide icon in a palette role or an explicit colour. Unlike `PIcon` its setters are
/// change-guarded (safe to call while presenting) and it can hide itself.
#[derive(Live, LiveHook, Widget)]
pub struct WaIcon {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[live]
    icon: ArcStringMut,
    #[live(16.0)]
    size: f64,
    #[live]
    role: TextRole,
    #[rust]
    color: Option<Vec4>,
}

impl Widget for WaIcon {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(self.size) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(self.size) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let color = self.color.unwrap_or_else(|| text_color(&p, self.role));
        let icon = self.icon.as_ref().to_string();
        self.painter.icon(cx, r, &icon, self.size, color);
        DrawStep::done()
    }
}

/// `TextRole` colours (`common::widgets` keeps its mapping private).
pub fn text_color(p: &Palette, role: TextRole) -> Vec4 {
    match role {
        TextRole::Body | TextRole::Title | TextRole::Small | TextRole::Mono => p.foreground,
        TextRole::Muted | TextRole::SmallMuted => p.muted,
        TextRole::ExtraMuted => p.extra_muted,
        TextRole::Danger => p.danger,
        TextRole::Success => p.success,
        TextRole::Warning => p.warning,
        TextRole::Accent => p.accent_bright,
        TextRole::OnAccent => p.on_tertiary,
        TextRole::FieldError => p.field_error,
    }
}

impl WaIconRef {
    pub fn set_icon(&self, cx: &mut Cx, icon: &str) {
        if let Some(mut i) = self.borrow_mut()
            && i.icon.as_ref() != icon
        {
            i.icon.as_mut_empty().push_str(icon);
            i.redraw(cx);
        }
    }

    pub fn set_color(&self, cx: &mut Cx, color: Option<Vec4>) {
        if let Some(mut i) = self.borrow_mut()
            && i.color != color
        {
            i.color = color;
            i.redraw(cx);
        }
    }

    pub fn set_size(&self, cx: &mut Cx, size: f64) {
        if let Some(mut i) = self.borrow_mut()
            && i.size != size
        {
            i.size = size;
            i.redraw(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WaDot, WaProjectIcon, WaBadge

/// `HostStatusDot`: an 8px dot centred in its box.
#[derive(Live, LiveHook, Widget)]
pub struct WaDot {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[live(8.0)]
    diameter: f64,
    #[rust]
    status: Option<super::host::Connection>,
}

impl Widget for WaDot {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let r = cx.walk_turtle(walk);
        let color = status_color(&p, self.status.unwrap_or(super::host::Connection::Disconnected));
        self.painter.dot(cx, r.center(), self.diameter, color);
        DrawStep::done()
    }
}

/// Status dot color for a host connection (`HostStatusDot`).
pub fn status_color(p: &Palette, status: super::host::Connection) -> Vec4 {
    use super::host::Connection;
    match status {
        Connection::Connected => p.success,
        Connection::Connecting => p.warning,
        _ => p.danger,
    }
}

/// Paints a `WaStatusDot`.
pub fn set_status_dot(cx: &mut Cx, dot: &WidgetRef, status: super::host::Connection) {
    if let Some(mut inner) = dot.borrow_mut::<WaDot>()
        && inner.status != Some(status)
    {
        inner.status = Some(status);
        inner.redraw(cx);
    }
}

/// `ProjectIconSquare`: identity fill (radius a quarter of the box) with the white initial.
#[derive(Live, LiveHook, Widget)]
pub struct WaProjectIcon {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[rust]
    fill: Option<Vec4>,
    #[rust]
    initial: String,
}

impl Widget for WaProjectIcon {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let t = theme::current_typography();
        let r = cx.walk_turtle(walk);
        let fill = self.fill.unwrap_or_else(|| super::identity::rgb(0x7a6aa8));
        let side = r.size.x.min(r.size.y);
        self.painter.fill(cx, r, fill, (side * 0.25).round());
        let initial = if self.initial.is_empty() { "?".to_string() } else { self.initial.clone() };
        // 16px icons carry the 10px semibold initial, larger ones the base medium one.
        let px = if side <= 16.0 { 10.0 } else { t.ui_size(14.0) };
        let spec = TextSpec::new(Font::Medium, px, vec4(1.0, 1.0, 1.0, 1.0));
        self.painter.text_in(cx, r, spec, &initial, 0.5);
        DrawStep::done()
    }
}

/// Paints a `WaProjectIcon` for `name` under its identity `view_key`.
pub fn set_project_icon(cx: &mut Cx, icon: &WidgetRef, name: &str, view_key: &str) {
    let fill = super::identity::rgb(super::identity::identity_fill(view_key));
    let initial = super::identity::project_initial(name);
    if let Some(mut inner) = icon.borrow_mut::<WaProjectIcon>()
        && (inner.fill != Some(fill) || inner.initial != initial)
    {
        inner.fill = Some(fill);
        inner.initial = initial;
        inner.redraw(cx);
    }
}

/// `StatusBadge` variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeVariant {
    Success,
    Muted,
    Error,
    Warning,
}

/// `StatusBadge`: a full-radius pill, 8/3 insets (6/1 `xs`), 12px text; tones tint their
/// fill (16% dark, 12% light) with a clear border, muted is surface3 with a divider border.
#[derive(Live, LiveHook, Widget)]
pub struct WaBadge {
    #[redraw]
    #[live]
    painter: Painter,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[live(false)]
    xs: bool,
    #[rust]
    label: String,
    #[rust]
    variant: Option<BadgeVariant>,
}

impl Widget for WaBadge {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let t = theme::current_typography();
        let tone = match self.variant.unwrap_or(BadgeVariant::Muted) {
            BadgeVariant::Success => Some(p.success),
            BadgeVariant::Muted => None,
            BadgeVariant::Error => Some(p.danger),
            BadgeVariant::Warning => Some(p.warning),
        };
        let spec = TextSpec::new(Font::Regular, t.ui_size(12.0), tone.unwrap_or(p.muted));
        let size = self.painter.measure(cx, spec, &self.label);
        let (pad_x, pad_y) = if self.xs { (6.0, 1.0) } else { (8.0, 3.0) };
        let walk = Walk {
            width: if matches!(walk.width, Size::Fit) { Size::Fixed(size.x + pad_x * 2.0 + 2.0) } else { walk.width },
            height: if matches!(walk.height, Size::Fit) { Size::Fixed(size.y + pad_y * 2.0 + 2.0) } else { walk.height },
            ..walk
        };
        let r = cx.walk_turtle(walk);
        let tint = if p.is_dark { 0.16 } else { 0.12 };
        let (fill, border) = match tone {
            Some(tone) => (with_alpha(tone, tint), Vec4::default()),
            None => (p.surface3, p.border),
        };
        self.painter.outline(cx, r, fill, border, if border.w > 0.0 { 1.0 } else { 0.0 }, r.size.y * 0.5);
        let label = self.label.clone();
        self.painter.text_in(cx, r, spec, &label, 0.5);
        DrawStep::done()
    }
}

pub fn set_badge(cx: &mut Cx, badge: &WidgetRef, label: &str, variant: BadgeVariant) {
    if let Some(mut inner) = badge.borrow_mut::<WaBadge>()
        && (inner.label != label || inner.variant != Some(variant))
    {
        inner.label = label.to_owned();
        inner.variant = Some(variant);
        inner.redraw(cx);
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers

/// Shows `text` in the `text` label of the wrapper view `slot` and hides the wrapper when
/// `text` is empty (labels cannot hide themselves).
pub fn set_optional(cx: &mut Cx, slot: &WidgetRef, text: &str) {
    slot.set_visible(cx, !text.is_empty());
    if !text.is_empty() {
        slot.widget(id!(text)).set_text(cx, text);
    }
}

/// Fills a `WaErrorCard` (original red-800 text) and hides it when `text` is empty.
pub fn set_error_card(cx: &mut Cx, card: &WidgetRef, text: &str) {
    card.set_visible(cx, !text.is_empty());
    if !text.is_empty() {
        let label = card.plabel(id!(text));
        label.set_text(cx, text);
        label.set_color(cx, Some(theme::argb(0xff991b1b)));
    }
}

/// Fills a `WaAlert`: with a title the description sits indented below it; without one the
/// description takes the title's place beside the icon.
pub fn set_alert(cx: &mut Cx, alert: &WidgetRef, title: &str, description: &str) {
    let has_title = !title.is_empty();
    set_optional(cx, &alert.widget(id!(head.title)), title);
    set_optional(cx, &alert.widget(id!(head.lead)), if has_title { "" } else { description });
    set_optional(cx, &alert.widget(id!(body.description)), if has_title { description } else { "" });
}

/// Sets an input's text only when it differs, so the caret, selection and any IME
/// composition survive redraws driven by unrelated state.
pub fn sync_input(cx: &mut Cx, input: &WaInputRef, text: &str) {
    if input.text() != text {
        input.set_text(cx, text);
    }
}

/// Multi-line inputs: Makepad's TextInput returns on Enter; a text area inserts the newline
/// (Flutter `TextInputType.multiline`) and reports the new text.
pub fn insert_newline(cx: &mut Cx, input: &WaInputRef) -> String {
    let input = input.input();
    let mut text = input.text();
    let selection = input.selection();
    let start = selection.start().index.min(text.len());
    let end = selection.end().index.min(text.len()).max(start);
    text.replace_range(start..end, "\n");
    input.set_text(cx, &text);
    input.set_cursor(
        cx,
        makepad_widgets::makepad_draw::text::selection::Cursor { index: start + 1, prefer_next_row: false },
        false,
    );
    text
}

/// Trims `text` to at most `max` characters with an ellipsis.
pub fn ellipsize(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------------------------
// WaModal

/// Makepad `Modal` (overlay draw list, sweep lock, Escape / back / background-tap dismissal)
/// that forwards its content's draw steps, so a `PortalList` inside a sheet is driven by the
/// owner's draw loop.
#[derive(Live, Widget)]
pub struct WaModal {
    #[live]
    #[find]
    content: View,
    #[live]
    #[area]
    bg_view: View,
    #[redraw]
    #[rust(DrawList2d::new(cx))]
    draw_list: DrawList2d,
    #[live]
    draw_bg: DrawQuad,
    #[layout]
    layout: Layout,
    #[walk]
    walk: Walk,
    #[rust]
    opened: bool,
    #[rust]
    draw_state: DrawStateWrap<bool>,
    #[rust]
    window_modal: Option<overlay::WindowModal>,
    #[rust]
    window_id: Option<String>,
    #[rust]
    nav_ready: bool,
    #[rust]
    preferred_sheet_width: Option<f64>,
    #[rust]
    sheet_height: f64,
    #[live]
    scroll_body: LiveId,
    #[rust]
    preferred_body_height: Option<f64>,
    #[rust]
    sheet_chrome_height: Option<f64>,
    #[rust]
    applied_body_height: Option<f64>,
    /// A text field inside the content holds key focus: Escape ends its edit first.
    #[rust]
    field_focus: bool,
}

impl LiveHook for WaModal {
    fn after_apply(&mut self, cx: &mut Cx, _apply: &mut Apply, _index: usize, _nodes: &[LiveNode]) {
        self.draw_list.redraw(cx);
    }
}

impl Widget for WaModal {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.sync_window(scope);
        // Focus transitions and their actions synchronize retained fields even when
        // their modal is closed or covered; they are not interactive input.
        if matches!(event, Event::KeyFocus(_) | Event::KeyFocusLost(_) | Event::Actions(_)) {
            if let Event::KeyFocus(focus) | Event::KeyFocusLost(focus) = event {
                let content = self.content.area().rect(cx);
                self.field_focus = self.opened && focus.focus != Area::Empty && content.contains(focus.focus.rect(cx).pos);
            }
            self.content.handle_event(cx, event, scope);
            return;
        }
        if !self.opened {
            return;
        }
        let area = self.draw_bg.area();
        if !modal_is_top(self.widget_uid(), self.window_id.as_deref()) {
            cx.sweep_unlock(area);
            return;
        }
        // Claim Escape before dispatching to a searchable field. Otherwise its
        // Escaped action and the raw key can dismiss different modal layers.
        if let Event::KeyDown(key) = event
            && key.key_code == KeyCode::Escape
        {
            if MODAL_STACK.with(|stack| stack.borrow_mut().claim_escape(self.window_id.as_deref(), event)) {
                if self.field_focus {
                    // The focused field gets this Escape (cancelling its edit); the next
                    // one dismisses the modal.
                    self.field_focus = false;
                    self.content.handle_event(cx, event, scope);
                    cx.set_key_focus(Area::Empty);
                } else {
                    cx.widget_action(self.content.widget_uid(), &scope.path, ModalAction::Dismissed);
                    self.close(cx);
                }
            }
            return;
        }
        // Key-up is deliberately not a dismissal: it may follow a picker closing.
        cx.sweep_unlock(area);
        self.content.handle_event(cx, event, scope);
        cx.sweep_lock(area);
        let consumed_hit = event.hits_with_sweep_area(cx, area, area);
        let should_close = event.back_pressed()
            || match consumed_hit {
                Hit::FingerUp(fe) => {
                    let content = self.content.area().rect(cx);
                    !content.contains(fe.abs_start)
                        && !content.contains(fe.abs)
                        && (!fe.device.is_touch() || fe.was_tap())
                }
                _ => false,
            };
        if should_close {
            let widget_uid = self.content.widget_uid();
            cx.widget_action(widget_uid, &scope.path, ModalAction::Dismissed);
            self.close(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.sync_window(scope);
        if self.draw_state.begin(cx, true) {
            if self.opened && let Some(ui) = scope.data.get::<UiContext>() {
                let owner = self.widget_uid().0;
                overlay::WindowModal::sync(&mut self.window_modal, ui, owner, true);
            }
            self.draw_list.begin_overlay_reuse(cx);
            self.nav_ready = true;
            // Overlay draw order alone does not escape the owning pane's turtle
            // clip. Finish this pass-sized scope independently of its ancestors.
            if self.opened {
                cx.begin_pass_sized_turtle(self.layout);
            } else {
                cx.begin_turtle(walk, self.layout);
            }
            let mut layout = self.layout;
            if self.opened && !self.content.widget(id!(sheet)).is_empty() {
                let insets = crate::shell::platform::safe_insets();
                layout.padding = Padding {
                    left: insets.left + 12.0,
                    right: insets.right + 12.0,
                    top: insets.top + 12.0,
                    bottom: insets.bottom_occlusion() + 12.0,
                };
            }
            self.draw_bg.begin(cx, self.walk, self.layout);
            if self.opened {
                let _ = self.bg_view.draw_walk(cx, scope, Walk::size(Size::Fill, Size::Fill));
            }
            cx.begin_turtle(Walk::size(Size::Fill, Size::Fill), layout);
        }
        if self.opened {
            let sheet = self.content.widget(id!(sheet));
            let body = sheet.widget(&[self.scroll_body]);
            let mut content_walk = self.content.walk(cx);
            if !sheet.is_empty() {
                // Keep authored desktop width; constrain the viewport, not the form's
                // natural height, so even a short keyboard viewport can scroll to its footer.
                let preferred = *self.preferred_sheet_width.get_or_insert_with(|| {
                    match sheet.walk(cx).width {
                        Size::Fixed(width) => width,
                        _ => 520.0,
                    }
                });
                if matches!(sheet.walk(cx).width, Size::Fixed(_)) {
                    sheet.apply_over(cx, live! { width: Fill });
                }
                let insets = crate::shell::platform::safe_insets();
                let size = cx.current_pass_size();
                let available = dvec2(
                    (size.x - insets.left - insets.right - 24.0).max(0.0),
                    (size.y - insets.top - insets.bottom_occlusion() - 24.0).max(0.0),
                );
                if let Size::Fixed(height) = body.walk(cx).height {
                    // A select can author a new preferred height when its options change.
                    if self.applied_body_height.is_none_or(|applied| (applied - height).abs() > 0.5) {
                        self.preferred_body_height = Some(height);
                    }
                    let preferred = self.preferred_body_height.unwrap_or(height);
                    let constrained = self.sheet_chrome_height
                        .map(|chrome| preferred.min((available.y - chrome).max(0.0)))
                        .unwrap_or(preferred);
                    if (constrained - height).abs() > 0.5 {
                        body.apply_over(cx, live! { height: (constrained) });
                    }
                    self.applied_body_height = Some(constrained);
                }
                content_walk.width = Size::Fixed(preferred.min(available.x).max(0.0));
                content_walk.height = Size::Fixed(
                    if self.sheet_height > 0.0 { self.sheet_height.min(available.y) } else { available.y }.max(0.0),
                );
            }
            self.content.draw_walk(cx, scope, content_walk)?;
            if !sheet.is_empty() {
                let height = sheet.area().rect(cx).size.y;
                if self.preferred_body_height.is_some() {
                    let chrome = (height - body.area().rect(cx).size.y).max(0.0);
                    if self.sheet_chrome_height.is_none_or(|old| (old - chrome).abs() > 0.5) {
                        self.sheet_chrome_height = Some(chrome);
                        cx.redraw_list_in_draw(self.draw_list.draw_list_id());
                    }
                }
                if (height - self.sheet_height).abs() > 0.5 {
                    self.sheet_height = height;
                    // Ordinary redraw() is ignored during drawing; measurement needs
                    // another frame before the user can interact with this geometry.
                    cx.redraw_list_in_draw(self.draw_list.draw_list_id());
                }
            }
        }
        cx.end_turtle();
        self.draw_bg.end(cx);
        if self.opened {
            cx.end_pass_sized_turtle();
        } else {
            cx.end_turtle();
        }
        self.draw_list.end(cx);
        self.draw_state.end();
        DrawStep::done()
    }
}

impl WaModal {
    fn sync_window(&mut self, scope: &mut Scope) {
        let Some(ui) = scope.data.get::<UiContext>() else { return };
        if self.window_id.as_deref() == Some(ui.window_id.as_str()) {
            return;
        }
        self.window_id = Some(ui.window_id.clone());
        MODAL_STACK.with(|stack| {
            if let Some(entry) = stack.borrow_mut().entries.iter_mut().find(|entry| entry.uid == self.widget_uid()) {
                entry.window_id.clone_from(&self.window_id);
            }
        });
    }

    fn clear_content_focus(&self, cx: &mut Cx) {
        use makepad_widgets::makepad_draw::nav::{CxNavTree, CxNavTreeRc};

        fn clear(cx: &mut Cx, tree: &CxNavTree, root: DrawListId, stack: &ModalStack) -> bool {
            for item in &tree[root].nav_list {
                match item {
                    NavItem::Stop(stop) if cx.clear_key_focus_if(stop.area) => return true,
                    // A nested modal owns its own focus, even if its draw list is
                    // linked into this modal's navigation subtree.
                    NavItem::Child(child) if !stack.entries.iter().any(|entry| entry.draw_list == *child) => {
                        if clear(cx, tree, *child, stack) {
                            return true;
                        }
                    }
                    _ => {}
                }
            }
            false
        }

        if self.nav_ready && cx.has_global::<CxNavTreeRc>() {
            let tree = cx.get_global::<CxNavTreeRc>().clone();
            MODAL_STACK.with(|stack| {
                clear(cx, &tree.0.borrow(), self.draw_list.draw_list_id(), &stack.borrow());
            });
        }
    }

    pub fn open(&mut self, cx: &mut Cx) {
        self.opened = true;
        let uid = self.widget_uid();
        MODAL_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            stack.remove(uid);
            stack.entries.push(ModalEntry {
                uid,
                window_id: self.window_id.clone(),
                draw_list: self.draw_list.draw_list_id(),
            });
        });
        self.draw_bg.redraw(cx);
        cx.sweep_lock(self.draw_bg.area());
    }

    pub fn close(&mut self, cx: &mut Cx) {
        self.clear_content_focus(cx);
        self.field_focus = false;
        self.window_modal = None;
        self.content.handle_event(cx, &Event::Actions(vec![Box::new(ModalAction::Dismissed)]), &mut Scope::empty());
        let uid = self.widget_uid();
        MODAL_STACK.with(|stack| stack.borrow_mut().remove(uid));
        self.opened = false;
        self.draw_bg.redraw(cx);
        cx.sweep_unlock(self.draw_bg.area());
    }
}

impl Drop for WaModal {
    fn drop(&mut self) {
        let uid = self.widget_uid();
        let _ = MODAL_STACK.try_with(|stack| stack.borrow_mut().remove(uid));
    }
}

struct ModalEntry {
    uid: WidgetUid,
    window_id: Option<String>,
    draw_list: DrawListId,
}

#[derive(Default)]
struct ModalStack {
    // Preserve open order even for widgets opened before their first scoped draw.
    entries: Vec<ModalEntry>,
    last_escape: std::collections::BTreeMap<String, f64>,
}

impl ModalStack {
    fn remove(&mut self, uid: WidgetUid) {
        self.entries.retain(|entry| entry.uid != uid);
    }

    fn is_top(&self, uid: WidgetUid, window_id: Option<&str>) -> bool {
        let Some(window_id) = window_id else { return false };
        self.entries.iter().rev()
            .find(|entry| entry.window_id.as_deref() == Some(window_id))
            .is_some_and(|entry| entry.uid == uid)
    }

    fn claim_escape(&mut self, window_id: Option<&str>, event: &Event) -> bool {
        let Some(window_id) = window_id else { return false };
        let Event::KeyDown(key) = event else { return false };
        if key.key_code != KeyCode::Escape || key.is_repeat {
            return false;
        }
        if let Some(time) = self.last_escape.get_mut(window_id) {
            if *time == key.time {
                return false;
            }
            *time = key.time;
        } else {
            self.last_escape.insert(window_id.to_owned(), key.time);
        }
        true
    }
}

thread_local! {
    /// Modal ordering is independent for each UiContext window.
    static MODAL_STACK: std::cell::RefCell<ModalStack> = std::cell::RefCell::new(ModalStack::default());
}

fn modal_is_top(uid: WidgetUid, window_id: Option<&str>) -> bool {
    MODAL_STACK.with(|stack| stack.borrow().is_top(uid, window_id))
}

impl WaModalRef {
    pub fn is_open(&self) -> bool {
        self.borrow().is_some_and(|inner| inner.opened)
    }

    pub fn accepts_input(&self) -> bool {
        self.borrow().is_some_and(|inner| {
            inner.opened && modal_is_top(inner.widget_uid(), inner.window_id.as_deref())
        })
    }

    pub fn open(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.open(cx);
        }
    }

    pub fn close(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.close(cx);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WaRepeat

/// See the `WaRepeat` DSL component. The owner sets the item count and fills each item
/// (`item(cx, i)`) before the list draws; items keep their identity by index so inputs and
/// hover state survive redraws.
#[derive(Live, LiveHook, Widget)]
pub struct WaRepeat {
    #[redraw]
    #[rust]
    area: Area,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[live]
    template: Option<LivePtr>,
    #[rust]
    items: Vec<WidgetRef>,
    #[rust]
    count: usize,
}

impl Widget for WaRepeat {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        for (index, item) in self.items.iter().take(self.count).enumerate() {
            scope.with_id(LiveId(index as u64), |scope| item.handle_event(cx, event, scope));
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        for item in self.items.iter().take(self.count) {
            item.draw_all(cx, scope);
        }
        cx.end_turtle_with_area(&mut self.area);
        DrawStep::done()
    }
}

impl WaRepeat {
    fn ensure(&mut self, cx: &mut Cx, count: usize) {
        while self.items.len() < count {
            self.items.push(WidgetRef::new_from_ptr(cx, self.template));
        }
    }
}

impl WaRepeatRef {
    /// Shows `count` items (creating missing ones from the template).
    pub fn set_count(&self, cx: &mut Cx, count: usize) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.ensure(cx, count);
        if inner.count != count {
            inner.count = count;
            inner.area.redraw(cx);
        }
    }

    /// Item `index`, created when missing. Dropping items past the count keeps their state
    /// for when the list grows again.
    pub fn item(&self, cx: &mut Cx, index: usize) -> WidgetRef {
        let Some(mut inner) = self.borrow_mut() else { return WidgetRef::empty() };
        inner.ensure(cx, index + 1);
        inner.items[index].clone()
    }

    /// The shown items, for matching their actions.
    pub fn items(&self) -> Vec<WidgetRef> {
        self.borrow().map(|inner| inner.items.iter().take(inner.count).cloned().collect()).unwrap_or_default()
    }

    /// Drops every item (a different list identity).
    pub fn reset(&self, cx: &mut Cx) {
        let Some(mut inner) = self.borrow_mut() else { return };
        inner.items.clear();
        inner.count = 0;
        inner.area.redraw(cx);
    }
}

/// The index of the first item in `repeat` whose `path` button was clicked.
pub fn clicked_in(repeat: &WaRepeatRef, path: &[LiveId], actions: &Actions) -> Option<usize> {
    repeat.items().iter().position(|item| item.wa_button(path).clicked(actions))
}

/// The index of the first item whose `path` view (a `cursor: Hand` tap area without a hover
/// fill, Flutter `GestureDetector`) received a primary tap (`FingerUp` over it).
pub fn tapped_in(repeat: &WaRepeatRef, path: &[LiveId], actions: &Actions) -> Option<usize> {
    repeat
        .items()
        .iter()
        .position(|item| item.view(path).finger_up(actions).is_some_and(|up| row_tapped(&up)))
}

fn row_tapped(up: &FingerUpEvent) -> bool {
    up.is_over && up.device.is_primary_hit() && (!up.device.is_touch() || up.was_tap())
}

#[cfg(test)]
mod modal_input_tests {
    use super::*;

    fn escape(time: f64, is_repeat: bool) -> Event {
        Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, time, is_repeat, ..Default::default() })
    }

    #[test]
    fn escape_dismisses_only_one_layer_until_a_new_press() {
        let mut stack = ModalStack::default();
        assert!(stack.claim_escape(Some("main"), &escape(1.0, false)));
        // A lower modal sees the same dispatch after the upper modal was removed.
        assert!(!stack.claim_escape(Some("main"), &escape(1.0, false)));
        assert!(!stack.claim_escape(Some("main"), &escape(2.0, true)));
        assert!(!stack.claim_escape(Some("main"), &Event::KeyUp(KeyEvent {
            key_code: KeyCode::Escape, time: 3.0, ..Default::default()
        })));
        assert!(stack.claim_escape(Some("main"), &escape(4.0, false)));
    }

    #[test]
    fn windows_have_independent_order_and_escape_claims() {
        let mut pool = CxDrawListPool::default();
        let lists = [pool.alloc(), pool.alloc(), pool.alloc()];
        let mut stack = ModalStack::default();
        for (index, window) in ["main", "main", "other"].iter().enumerate() {
            stack.entries.push(ModalEntry {
                uid: WidgetUid(index as u64 + 1),
                window_id: Some((*window).to_owned()),
                draw_list: lists[index].id(),
            });
        }
        assert!(!stack.is_top(WidgetUid(1), Some("main")));
        assert!(stack.is_top(WidgetUid(2), Some("main")));
        assert!(stack.is_top(WidgetUid(3), Some("other")));
        assert!(stack.claim_escape(Some("main"), &escape(1.0, false)));
        assert!(stack.claim_escape(Some("other"), &escape(1.0, false)));
        assert!(!stack.claim_escape(Some("main"), &escape(1.0, false)));
        stack.remove(WidgetUid(2));
        assert!(stack.is_top(WidgetUid(1), Some("main")));
        assert!(stack.is_top(WidgetUid(3), Some("other")));
    }

    #[test]
    fn row_release_requires_a_real_touch_tap() {
        let up = FingerUpEvent {
            window_id: CxWindowPool::id_zero(),
            abs: dvec2(0.0, 0.0), abs_start: dvec2(0.0, 0.0),
            capture_time: 0.0, time: 0.1, digit_id: live_id_num!(touch, 1).into(),
            device: DigitDevice::Touch { uid: 1 },
            has_long_press_occurred: false, tap_cancelled: false, tap_count: 1,
            modifiers: KeyModifiers::default(), rect: Rect::default(),
            is_over: true, is_sweep: false,
        };
        assert!(row_tapped(&up));
        assert!(!row_tapped(&FingerUpEvent { tap_cancelled: true, ..up.clone() }));
        assert!(!row_tapped(&FingerUpEvent { has_long_press_occurred: true, ..up.clone() }));
        assert!(!row_tapped(&FingerUpEvent { abs: dvec2(100.0, 0.0), ..up.clone() }));
        assert!(!row_tapped(&FingerUpEvent { time: 10.0, ..up.clone() }));
        assert!(!row_tapped(&FingerUpEvent { is_over: false, ..up }));
    }
}
