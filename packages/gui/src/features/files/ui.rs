//! Pane chrome shared by the explorer and file views (Flutter `pane_chrome.dart` plus the
//! `PaseoButton` / `PaseoSegmentedControl` geometry they use), drawn immediate-mode with the
//! runtime [`Palette`].

use makepad_widgets::*;

use crate::common::paint::Font;
use crate::theme::{Palette, Typography, fade, with_alpha};

use super::kit::{Kit, rect, spec};

/// Original `WORKSPACE_SECONDARY_HEADER_HEIGHT`.
pub const TOOLBAR_HEIGHT: f64 = 36.0;
/// Original `WORKSPACE_PANE_TRAILING_GLYPH_RAIL`.
pub const TRAILING_GLYPH_RAIL: f64 = 8.0;
/// Original `TREE_INDENT_PER_LEVEL`.
pub const TREE_INDENT: f64 = 12.0;

pub fn tree_row_padding_left(depth: usize) -> f64 {
    12.0 + depth as f64 * TREE_INDENT
}

/// Original small `iconButtonChromeFrameSize`: 20 desktop, 32 compact.
pub fn toolbar_button_size(compact: bool) -> f64 {
    if compact { 32.0 } else { 20.0 }
}

/// Original `paneContentToolbarIconSize`: 14 desktop, 18 compact.
pub fn toolbar_icon_size(compact: bool) -> f64 {
    if compact { 18.0 } else { 14.0 }
}

/// Original `paneContentToolbarTrailingPadding` for a bare trailing glyph.
pub fn toolbar_trailing_padding(compact: bool) -> f64 {
    TRAILING_GLYPH_RAIL - (toolbar_button_size(compact) - toolbar_icon_size(compact)) / 2.0
}

/// Pointer state of one interactive item.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interaction {
    pub hovered: bool,
    pub pressed: bool,
    pub enabled: bool,
    pub selected: bool,
}

impl Interaction {
    pub fn new(enabled: bool) -> Interaction {
        Interaction { enabled, ..Interaction::default() }
    }
}

/// Original `PaneContentToolbar`: a 36px row with a bottom border, surface0 in panes and
/// surfaceSidebar in the explorer.
pub fn toolbar(cx: &mut Cx2d, kit: &mut Kit, area: Rect, sidebar: bool, p: &Palette) {
    kit.fill(cx, area, if sidebar { p.sidebar } else { p.surface0 }, 0.0);
    kit.hline(cx, area.pos.x, area.pos.y + area.size.y - 1.0, area.size.x, p.border);
}

/// Original `ToolbarButton`: highlight while hovered, pressed or selected, 50% when disabled.
pub fn icon_button(cx: &mut Cx2d, kit: &mut Kit, frame: Rect, icon: &str, state: Interaction, compact: bool, p: &Palette) {
    let highlighted = state.selected || (state.enabled && (state.hovered || state.pressed));
    if highlighted {
        kit.fill(cx, frame, p.interaction_highlight, 6.0);
    }
    let color = if state.enabled { p.muted } else { fade(p.muted, 0.5) };
    kit.icon(cx, frame, icon, toolbar_icon_size(compact), color);
}

/// A spinner in an icon slot (`TreeLoadingIcon`, toolbar refresh while refreshing).
pub fn spinner(cx: &mut Cx2d, kit: &mut Kit, frame: Rect, size: f64, color: Vec4, phase: f64) {
    let r = rect(
        frame.pos.x + (frame.size.x - size) / 2.0,
        frame.pos.y + (frame.size.y - size) / 2.0,
        size,
        size,
    );
    kit.spinner(cx, r, color, phase);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonVariant {
    Primary,
    Outline,
    Ghost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ButtonSize {
    Xs,
    Sm,
}

impl ButtonSize {
    pub fn height(self) -> f64 {
        match self {
            ButtonSize::Xs => 28.0,
            ButtonSize::Sm => 32.0,
        }
    }

    fn font(self, t: &Typography) -> f64 {
        match self {
            ButtonSize::Xs => t.ui_size(12.0),
            ButtonSize::Sm => t.ui_size(14.0),
        }
    }

    fn icon(self) -> f64 {
        match self {
            ButtonSize::Xs => 12.0,
            ButtonSize::Sm => 14.0,
        }
    }
}

/// The width a `PaseoButton` needs (12px padding, 8px icon gap).
pub fn button_width(cx: &mut Cx, kit: &mut Kit, label: &str, icon: Option<&str>, size: ButtonSize, t: &Typography) -> f64 {
    let text = kit.measure(cx, spec(Font::Regular, size.font(t), Vec4::default()), label).x;
    let icon = if icon.is_some() { size.icon() + if label.is_empty() { 0.0 } else { 8.0 } } else { 0.0 };
    (24.0 + text + icon).ceil()
}

/// Original `PaseoButton` (outline / ghost / primary at xs/sm).
#[allow(clippy::too_many_arguments)]
pub fn button(
    cx: &mut Cx2d,
    kit: &mut Kit,
    frame: Rect,
    label: &str,
    icon: Option<&str>,
    variant: ButtonVariant,
    size: ButtonSize,
    state: Interaction,
    p: &Palette,
    t: &Typography,
) {
    let (background, border, foreground) = match variant {
        ButtonVariant::Primary => (p.accent, p.accent, p.on_tertiary),
        ButtonVariant::Outline => (Vec4::default(), p.border_accent, p.foreground),
        ButtonVariant::Ghost => (
            Vec4::default(),
            Vec4::default(),
            if state.hovered && state.enabled { p.foreground } else { p.muted },
        ),
    };
    let opacity = if state.enabled { 1.0 } else { 0.5 };
    let hover = state.enabled && (state.hovered || state.pressed);
    let fill = if hover && variant != ButtonVariant::Primary { p.surface2 } else { background };
    if border.w > 0.0 {
        kit.outline(cx, frame, fade(fill, opacity), fade(border, opacity), 1.0, 12.0);
    } else {
        kit.fill(cx, frame, fade(fill, opacity), 12.0);
    }
    let fg = fade(foreground, opacity);
    let font = size.font(t);
    let text_w = if label.is_empty() { 0.0 } else { kit.measure(cx, spec(Font::Regular, font, fg), label).x };
    let icon_w = if icon.is_some() { size.icon() + if label.is_empty() { 0.0 } else { 8.0 } } else { 0.0 };
    let mut x = frame.pos.x + ((frame.size.x - text_w - icon_w) / 2.0).max(12.0);
    if let Some(icon) = icon {
        kit.icon(cx, rect(x, frame.pos.y, size.icon(), frame.size.y), icon, size.icon(), fg);
        x += icon_w;
    }
    if !label.is_empty() {
        kit.text_in(cx, rect(x, frame.pos.y, frame.pos.x + frame.size.x - 12.0 - x, frame.size.y), spec(Font::Regular, font, fg), label, 0.0);
    }
}

/// Original `PaseoSegmentedControl` xs: 24px segments 8px padded, 12px radius, 4px apart.
/// Returns each segment's rect.
#[allow(clippy::too_many_arguments)] // independent drawing inputs
pub fn segmented(
    cx: &mut Cx2d,
    kit: &mut Kit,
    right: f64,
    center_y: f64,
    labels: &[&str],
    selected: usize,
    hovered: Option<usize>,
    p: &Palette,
    t: &Typography,
) -> Vec<Rect> {
    let font = t.ui_size(12.0);
    let mut height: f64 = 24.0;
    let widths: Vec<f64> = labels
        .iter()
        .map(|label| {
            let measured = kit.measure(cx, spec(Font::Regular, font, p.foreground), label);
            height = height.max(measured.y + 8.0);
            measured.x + 16.0
        })
        .collect();
    let total: f64 = widths.iter().sum::<f64>() + 4.0 * (labels.len().saturating_sub(1)) as f64;
    let mut x = right - total;
    let mut rects = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        let r = rect(x, center_y - height * 0.5, widths[index], height);
        let fill = if index == selected {
            p.surface3
        } else if hovered == Some(index) {
            p.surface2
        } else {
            Vec4::default()
        };
        kit.fill(cx, r, fill, 12.0);
        let color = if index == selected { p.foreground } else { p.muted };
        kit.text_in(cx, r, spec(Font::Regular, font, color), label, 0.5);
        rects.push(r);
        x += widths[index] + 4.0;
    }
    rects
}

pub fn segmented_width(cx: &mut Cx, kit: &mut Kit, labels: &[&str], t: &Typography) -> f64 {
    let font = t.ui_size(12.0);
    labels.iter().map(|label| kit.measure(cx, spec(Font::Regular, font, Vec4::default()), label).x + 16.0).sum::<f64>()
        + 4.0 * labels.len().saturating_sub(1) as f64
}

/// The sidebar row hover fill (original `surfaceSidebarHover`).
pub fn sidebar_hover(p: &Palette) -> Vec4 {
    if p.is_dark { p.surface1 } else { p.surface3 }
}

/// A rotating disclosure chevron (original `TreeChevron`).
pub fn chevron(cx: &mut Cx2d, kit: &mut Kit, slot: Rect, expanded: bool, p: &Palette) {
    kit.icon(cx, slot, if expanded { "chevron-down" } else { "chevron-right" }, 16.0, p.muted);
}

/// A translucent scrim over `area` (dialogs drawn inside a panel).
pub fn scrim(cx: &mut Cx2d, kit: &mut Kit, area: Rect) {
    kit.fill(cx, area, with_alpha(vec4(0.0, 0.0, 0.0, 1.0), 0.55), 0.0);
}

/// Original md card shadow approximation: a soft dark rect behind a card.
pub fn card(cx: &mut Cx2d, kit: &mut Kit, frame: Rect, p: &Palette, radius: f64) {
    let shadow = if p.is_dark { with_alpha(vec4(0.0, 0.0, 0.0, 1.0), 0.2) } else { with_alpha(vec4(0.0, 0.0, 0.0, 1.0), 0.04) };
    kit.fill(cx, rect(frame.pos.x, frame.pos.y + 4.0, frame.size.x, frame.size.y), shadow, radius);
    kit.outline(cx, frame, p.surface1, p.border, 1.0, radius);
}

/// Human file size (Flutter `_fileSize`).
pub fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
