//! Pane chrome shared by the git views (original files/pane_chrome.dart and
//! ui/controls.dart geometry): toolbars, toolbar buttons, label triggers,
//! diff stats, change glyphs and buttons.

use super::json::{int, text};
use super::model::{ChangeKind, TreeRow, change_kind, format_diff_count};
use super::paint::{Font, GitPaint, rect, with_alpha};
use crate::common::paint::TextSpec;
use crate::common::widgets::RowMetrics;
use crate::theme::{Palette, Typography, current_palette, current_typography};
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashSet;

/// Height of a `ChangedFilesTree` row.
pub const TREE_ROW_HEIGHT: f64 = 28.0;

pub fn tree_row_metrics(cx: &mut Cx2d, paint: &mut GitPaint) -> RowMetrics {
    RowMetrics::measure(&mut paint.painter, cx, TextSpec::new(Font::Regular, ui(14.0), palette().foreground), None, 0.0)
}

/// Original `FileTreeRow` of `ChangedFilesTree` in its 28px `band`: 12px plus
/// 12px per depth, a 16px chevron (folders) or material file icon, the name
/// at 76% opacity (full while hovered), and on the right the subtree stat or
/// the file stat and change glyph 8px apart. Returns false while the file
/// icon is still decoding.
#[allow(clippy::too_many_arguments)]
pub fn file_tree_row(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    band: Rect,
    metrics: RowMetrics,
    row: &TreeRow,
    files: &[Value],
    collapsed: &HashSet<String>,
    hovered: bool,
    selected: bool,
) -> bool {
    let colors = palette();
    if hovered || selected {
        paint.fill(cx, band, colors.sidebar_hover);
    }
    let center_y = band.pos.y + band.size.y / 2.0;
    let (depth, label) = match row {
        TreeRow::Folder { display, depth, .. } => (*depth, display.as_str()),
        TreeRow::File { index, depth } => {
            let path = text(&files[*index], "path");
            (*depth, path.rsplit('/').next().unwrap_or(path))
        }
    };
    let indent = (4.0 + depth as f64 * 12.0).min(band.size.x);
    let content = rect(band.pos.x + indent, band.pos.y, band.size.x - indent, band.size.y);
    let right = band.pos.x + band.size.x - 8.0;
    // Stats return their measured width; do not format/shape them a second time
    // just to reserve the trailing slot.
    paint.begin_clip(cx, content);
    let trailing_width = match row {
        TreeRow::Folder { additions, deletions, .. } => diff_stat(cx, paint, right, center_y, *additions, *deletions, true),
        TreeRow::File { index, .. } => {
            let file = &files[*index];
            change_icon(cx, paint, dvec2(right - 7.0, center_y), change_kind(file));
            22.0 + diff_stat(cx, paint, right - 22.0, center_y, int(file, "additions"), int(file, "deletions"), true)
        }
    };
    let slots = metrics.layout(content, 8.0, 16.0, trailing_width, 8.0);
    let mut icon_ready = true;
    if slots.leading.size.x >= 16.0 {
        match row {
            TreeRow::Folder { path, .. } => {
                let chevron = if collapsed.contains(path) { "chevron-right" } else { "chevron-down" };
                paint.painter.icon(cx, slots.leading, chevron, 16.0, colors.extra_muted);
            }
            TreeRow::File { .. } => {
                icon_ready = paint.file_icon(cx, rect(slots.leading.pos.x, center_y - 8.0, 16.0, 16.0), label);
            }
        }
    }
    let alpha = if hovered { 1.0 } else { 0.76 };
    paint.painter.text_in(cx, slots.title, TextSpec::new(Font::Regular, ui(14.0), with_alpha(colors.foreground, alpha)), label, 0.0);
    paint.end_clip(cx);
    icon_ready
}

/// Original `paneToolbarHeight`.
pub const TOOLBAR_HEIGHT: f64 = 36.0;
/// Original `paneTrailingGlyphRail`.
pub const TRAILING_RAIL: f64 = 8.0;

pub fn palette() -> Palette {
    current_palette()
}

pub fn typography() -> Typography {
    current_typography()
}

/// `typography.uiSize(px)`.
pub fn ui(px: f64) -> f64 {
    current_typography().ui_size(px)
}

/// Original `paneToolbarButtonSize`: 20 desktop, 32 compact.
pub fn toolbar_button_size(compact: bool) -> f64 {
    if compact { 32.0 } else { 20.0 }
}

/// Original `paneToolbarIconSize`: 14 desktop, 18 compact.
pub fn toolbar_icon_size(compact: bool) -> f64 {
    if compact { 18.0 } else { 14.0 }
}

/// Original `paneToolbarTrailingPadding`.
pub fn toolbar_trailing_padding(compact: bool, framed: bool) -> f64 {
    if framed {
        TRAILING_RAIL
    } else {
        TRAILING_RAIL - (toolbar_button_size(compact) - toolbar_icon_size(compact)) / 2.0
    }
}

/// Original `FilePaneToolbar`: a 36px band with a 1px bottom border.
pub fn toolbar_band(cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, sidebar: bool) {
    let colors = palette();
    paint.fill(cx, area, if sidebar { colors.sidebar } else { colors.surface0 });
    paint.fill(
        cx,
        rect(area.pos.x, area.pos.y + area.size.y - 1.0, area.size.x, 1.0),
        colors.border,
    );
}

/// Original `PaneToolbarButton`: hitbox with the interaction highlight while
/// hovered or selected, 50% opacity when disabled. Returns the button rect.
#[allow(clippy::too_many_arguments)]
pub fn toolbar_button(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    compact: bool,
    right: f64,
    center_y: f64,
    icon: Option<&str>,
    selected: bool,
    enabled: bool,
    hovered: bool,
) -> Rect {
    let colors = palette();
    let size = toolbar_button_size(compact);
    let area = rect(right - size, center_y - size / 2.0, size, size);
    if selected || (enabled && hovered) {
        paint.rounded(cx, area, colors.interaction_highlight, 6.0, None);
    }
    if let Some(icon) = icon {
        let color = if enabled { colors.extra_muted } else { with_alpha(colors.extra_muted, 0.5) };
        paint.icon(cx, icon, toolbar_icon_size(compact), color, area.center());
    }
    area
}

/// Original `_ToolbarLabelTrigger`: 4px padding, small muted label (foreground
/// with the interaction highlight while hovered or open), optional leading
/// glyph, trailing 12px extraMuted chevron. Returns the trigger rect.
#[allow(clippy::too_many_arguments)]
pub fn label_trigger(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    x: f64,
    center_y: f64,
    max_width: f64,
    label: &str,
    leading: Option<(&str, Vec4)>,
    chevron: bool,
    highlighted: bool,
) -> Rect {
    let colors = palette();
    let size = ui(12.0);
    let leading_width = if leading.is_some() { 14.0 + 4.0 } else { 0.0 };
    let chevron_width = if chevron { 4.0 + 12.0 } else { 0.0 };
    let text_max = (max_width - 8.0 - leading_width - chevron_width).max(0.0);
    let fitted = paint.ellipsize(cx, Font::Regular, size, label, text_max);
    let text_width = paint.measure(cx, Font::Regular, size, &fitted).x;
    let height = paint.measure(cx, Font::Regular, size, "Ag").y + 8.0;
    let width = 8.0 + leading_width + text_width + chevron_width;
    let area = rect(x, center_y - height / 2.0, width, height);
    if highlighted {
        paint.rounded(cx, area, colors.interaction_highlight, 6.0, None);
    }
    let mut cursor = x + 4.0;
    if let Some((icon, color)) = leading {
        paint.icon(cx, icon, 14.0, color, dvec2(cursor + 7.0, center_y));
        cursor += leading_width;
    }
    let color = if highlighted { colors.foreground } else { colors.muted };
    paint.text_centered(cx, Font::Regular, size, color, cursor, area.pos.y, height, &fitted, text_max + 1.0);
    cursor += text_width;
    if chevron {
        paint.icon(cx, "chevron-down", 12.0, colors.extra_muted, dvec2(cursor + 4.0 + 6.0, center_y));
    }
    area
}

/// Original `DiffStat`: `+a` in success and `-d` in danger, 4px apart, small.
/// Draws right-aligned ending at `right` when `right_align`, else from `x`.
pub fn diff_stat(cx: &mut Cx2d, paint: &mut GitPaint, x: f64, center_y: f64, additions: i64, deletions: i64, right_align: bool) -> f64 {
    let colors = palette();
    let size = ui(12.0);
    let plus = format!("+{}", format_diff_count(additions));
    let minus = format!("-{}", format_diff_count(deletions));
    let plus_width = paint.measure(cx, Font::Regular, size, &plus).x;
    let minus_width = paint.measure(cx, Font::Regular, size, &minus).x;
    let width = plus_width + 4.0 + minus_width;
    let start = if right_align { x - width } else { x };
    let line = paint.measure(cx, Font::Regular, size, "Ag").y;
    let top = center_y - line / 2.0;
    paint.text(cx, Font::Regular, size, colors.success, dvec2(start, top), &plus);
    paint.text(cx, Font::Regular, size, colors.danger, dvec2(start + plus_width + 4.0, top), &minus);
    width
}

pub fn measure_diff_stat(cx: &mut Cx2d, paint: &mut GitPaint, additions: i64, deletions: i64) -> f64 {
    let size = ui(12.0);
    paint.measure(cx, Font::Regular, size, &format!("+{}", format_diff_count(additions))).x
        + 4.0
        + paint.measure(cx, Font::Regular, size, &format!("-{}", format_diff_count(deletions))).x
}

/// Original `FileChangeIcon`: square-plus / square-minus / square-dot, 14px.
pub fn change_icon(cx: &mut Cx2d, paint: &mut GitPaint, center: DVec2, kind: ChangeKind) {
    let colors = palette();
    let (icon, color) = match kind {
        ChangeKind::Added => ("square-plus", colors.success),
        ChangeKind::Deleted => ("square-minus", colors.danger),
        ChangeKind::Modified => ("square-dot", colors.warning),
    };
    paint.icon(cx, icon, 14.0, color, center);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonVariant {
    Primary,
    Secondary,
    Ghost,
    Destructive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonSize {
    Xs,
    Sm,
    Md,
}

fn button_metrics(size: ButtonSize, compact: bool) -> (f64, f64, f64) {
    // (height, horizontal padding, text px)
    match (size, compact) {
        (ButtonSize::Xs, false) => (24.0, 8.0, 12.0),
        (ButtonSize::Xs, true) => (28.0, 10.0, 13.0),
        (ButtonSize::Sm, false) => (28.0, 10.0, 13.0),
        (ButtonSize::Sm, true) => (32.0, 12.0, 14.0),
        (ButtonSize::Md, false) => (32.0, 12.0, 14.0),
        (ButtonSize::Md, true) => (40.0, 14.0, 15.0),
    }
}

/// The width a [`button`] needs.
pub fn measure_button(cx: &mut Cx2d, paint: &mut GitPaint, label: &str, icon: bool, size: ButtonSize, compact: bool) -> f64 {
    let (_, padding, text) = button_metrics(size, compact);
    let text_width = paint.measure(cx, Font::Medium, ui(text), label).x;
    padding * 2.0 + text_width + if icon { 16.0 + 6.0 } else { 0.0 }
}

pub fn button_height(size: ButtonSize, compact: bool) -> f64 {
    button_metrics(size, compact).0
}

/// Original `PaseoButton`.
#[allow(clippy::too_many_arguments)]
pub fn button(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    area: Rect,
    label: &str,
    icon: Option<&str>,
    variant: ButtonVariant,
    size: ButtonSize,
    compact: bool,
    enabled: bool,
    hovered: bool,
) {
    let colors = palette();
    let (_, padding, text) = button_metrics(size, compact);
    let opacity = if enabled { 1.0 } else { 0.5 };
    let (background, border, foreground) = match variant {
        ButtonVariant::Primary => (colors.primary, None, colors.on_primary),
        ButtonVariant::Destructive => (colors.destructive, None, vec4(1.0, 1.0, 1.0, 1.0)),
        ButtonVariant::Secondary => (colors.surface2, Some((colors.border, 1.0)), colors.foreground),
        ButtonVariant::Ghost => (vec4(0.0, 0.0, 0.0, 0.0), None, colors.muted),
    };
    let background = if hovered && enabled && variant == ButtonVariant::Ghost {
        colors.interaction_highlight
    } else {
        background
    };
    paint.rounded(
        cx,
        area,
        with_alpha(background, opacity),
        6.0,
        border.map(|(c, w)| (with_alpha(c, opacity), w)),
    );
    if hovered && enabled && variant != ButtonVariant::Ghost {
        paint.rounded(cx, area, colors.interaction_highlight, 6.0, None);
    }
    let foreground = if hovered && enabled && variant == ButtonVariant::Ghost { colors.foreground } else { foreground };
    let foreground = with_alpha(foreground, opacity);
    let text_px = ui(text);
    let text_width = paint.measure(cx, Font::Medium, text_px, label).x;
    let content = text_width + if icon.is_some() { 22.0 } else { 0.0 };
    let mut x = area.pos.x + ((area.size.x - content) / 2.0).max(padding.min(area.size.x / 2.0));
    let center_y = area.pos.y + area.size.y / 2.0;
    if let Some(icon) = icon {
        paint.icon(cx, icon, 16.0, foreground, dvec2(x + 8.0, center_y));
        x += 22.0;
    }
    paint.text_centered(
        cx,
        Font::Medium,
        text_px,
        foreground,
        x,
        area.pos.y,
        area.size.y,
        label,
        (area.pos.x + area.size.x - x - padding / 2.0).max(0.0),
    );
}
