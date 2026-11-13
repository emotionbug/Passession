//! Painting of the Git Log tab ([`LogView`]): the desktop toolbar, branches
//! panel (or drawer), commit table and details, and the phone log and commit
//! screens. Hits follow the feature rule: a row registers before its inner
//! buttons, scrolled hits are clipped to their region, text that does not fit
//! is ellipsized.

use super::chrome::{
    ButtonSize, ButtonVariant, TOOLBAR_HEIGHT, button, button_height, diff_stat, label_trigger, measure_button, palette,
    toolbar_band, toolbar_button, toolbar_button_size, toolbar_icon_size, toolbar_trailing_padding, ui as ui_px,
};
use super::graph::GraphRow;
use super::json::{int, text};
use super::log::{Availability, CommitDetail, Detail, RefFilter, RefKind, RefLabel, Refs, is_hash_query};
use super::log_view::{Focus, LogHit, LogInputs, LogView, Screen, Split, ahead_behind, basename, dirname};
use super::model::now_ms;
use super::paint::{Font, GitPaint, hex, rect, with_alpha};
use super::text::{t, tr};
use crate::common::UiContext;
use crate::theme::Typography;
use chrono::{Local, TimeZone};
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashSet;

/// Desktop commit table: lane width and the widest graph drawn.
const LANE_WIDTH: f64 = 14.0;
const MAX_LANES: usize = 24;
/// The desktop graph takes at most this share of the subject column.
const GRAPH_SHARE: f64 = 0.35;
/// Phone rows: a narrow graph capped at six lanes.
const PHONE_LANE_WIDTH: f64 = 10.0;
const PHONE_MAX_LANES: usize = 6;
const PHONE_ROW_HEIGHT: f64 = 56.0;
const PHONE_FILE_ROW_HEIGHT: f64 = 48.0;
const BRANCHES_WIDTH: f64 = 240.0;
const BRANCHES_MIN_WIDTH: f64 = 160.0;

/// Lane colours by colour index.
fn lane_color(index: usize) -> Vec4 {
    const LANES: [&str; 8] = ["#4c8dff", "#3fb950", "#a371f7", "#d29922", "#f778ba", "#39c5cf", "#ff7b72", "#8b949e"];
    hex(LANES[index % LANES.len()])
}

/// "just now", "5m ago", … up to a week, then the local date.
pub(super) fn relative_time(ms: i64, now: i64) -> String {
    let minutes = (now - ms).max(0) / 60_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if minutes < 1 {
        t("workspace.git.log.time.justNow")
    } else if hours < 1 {
        tr("workspace.git.log.time.minutesAgo", &[("count", minutes.to_string())])
    } else if days < 1 {
        tr("workspace.git.log.time.hoursAgo", &[("count", hours.to_string())])
    } else if days < 7 {
        tr("workspace.git.log.time.daysAgo", &[("count", days.to_string())])
    } else {
        local_time(ms, "%Y-%m-%d")
    }
}

pub(super) fn local_time(ms: i64, format: &str) -> String {
    Local.timestamp_millis_opt(ms).single().map(|time| time.format(format).to_string()).unwrap_or_default()
}

/// The 8-digit abbreviation every hash column and title uses.
pub(super) fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Pill colours of a ref decoration: fill, text.
fn ref_colors(label: &RefLabel) -> (Vec4, Vec4) {
    let colors = palette();
    let tint = match label.kind {
        RefKind::Head => colors.merged,
        _ if label.is_head => colors.merged,
        RefKind::Local => colors.success,
        RefKind::Remote => colors.accent_bright,
        RefKind::Tag => colors.warning,
    };
    (with_alpha(tint, 0.16), tint)
}

/// The status letter of a changed file and its colour.
fn status_letter(file: &Value) -> (&'static str, Vec4) {
    let colors = palette();
    match text(file, "status") {
        "added" => ("A", colors.success),
        "deleted" => ("D", colors.danger),
        "renamed" => ("R", colors.accent_bright),
        _ => ("M", colors.warning),
    }
}

/// Name and directory shown for a changed file; renames read `old → new`.
fn file_label(file: &Value) -> (String, String) {
    let path = text(file, "path");
    let old = text(file, "oldPath");
    if !old.is_empty() && old != path {
        if dirname(old) == dirname(path) {
            return (format!("{} → {}", basename(old), basename(path)), dirname(path).to_owned());
        }
        return (format!("{old} → {path}"), String::new());
    }
    (basename(path).to_owned(), dirname(path).to_owned())
}

fn chip_height() -> f64 {
    ui_px(11.0) + 6.0
}

fn chip_width(cx: &mut Cx2d, paint: &mut GitPaint, label: &str) -> f64 {
    paint.measure(cx, Font::Medium, ui_px(11.0), label).x + 10.0
}

/// A compact ref pill at `x` centred on `center_y`, fitted to `max_width`; returns its width.
fn chip(cx: &mut Cx2d, paint: &mut GitPaint, x: f64, center_y: f64, label: &RefLabel, max_width: f64) -> f64 {
    if max_width < 20.0 {
        return 0.0;
    }
    let (fill, color) = ref_colors(label);
    let size = ui_px(11.0);
    let name = paint.ellipsize(cx, Font::Medium, size, &label.name, max_width - 10.0);
    let width = paint.measure(cx, Font::Medium, size, &name).x + 10.0;
    let height = chip_height();
    let area = rect(x, center_y - height / 2.0, width, height);
    let border = (label.is_head || label.kind == RefKind::Head).then_some((with_alpha(color, 0.7), 1.0));
    paint.rounded(cx, area, fill, 4.0, border);
    paint.text_centered(cx, Font::Medium, size, color, x + 5.0, area.pos.y, height, &name, width - 9.0);
    width
}

/// A muted `+n` pill for decorations that did not fit.
fn more_chip(cx: &mut Cx2d, paint: &mut GitPaint, x: f64, center_y: f64, count: usize) -> f64 {
    let colors = palette();
    let label = format!("+{count}");
    let size = ui_px(11.0);
    let width = paint.measure(cx, Font::Medium, size, &label).x + 10.0;
    let height = chip_height();
    paint.rounded(cx, rect(x, center_y - height / 2.0, width, height), colors.surface2, 4.0, None);
    paint.text_centered(cx, Font::Medium, size, colors.muted, x + 5.0, center_y - height / 2.0, height, &label, width - 9.0);
    width
}

/// A toolbar filter trigger: the plain label trigger while inactive; an
/// accent-tinted chip with an inline × while a filter is set. Returns the
/// trigger and the × rect.
#[allow(clippy::too_many_arguments)]
fn filter_chip(
    cx: &mut Cx2d,
    paint: &mut GitPaint,
    x: f64,
    center_y: f64,
    max_width: f64,
    label: &str,
    icon: &'static str,
    active: bool,
    highlighted: bool,
    clear_hovered: bool,
) -> (Rect, Option<Rect>) {
    let colors = palette();
    if !active {
        return (label_trigger(cx, paint, x, center_y, max_width, label, Some((icon, colors.muted)), true, highlighted), None);
    }
    let size = ui_px(12.0);
    let clear_width = 18.0;
    let text_max = (max_width - 4.0 - 18.0 - 2.0 - clear_width).max(0.0);
    let fitted = paint.ellipsize(cx, Font::Regular, size, label, text_max);
    let text_width = paint.measure(cx, Font::Regular, size, &fitted).x;
    let height = paint.measure(cx, Font::Regular, size, "Ag").y + 8.0;
    let width = 4.0 + 18.0 + text_width + 2.0 + clear_width;
    let area = rect(x, center_y - height / 2.0, width, height);
    // Accent fill; text and glyphs in the readable accent.
    let (accent, ink) = (colors.accent, colors.accent_bright);
    paint.rounded(cx, area, with_alpha(accent, if highlighted { 0.22 } else { 0.14 }), 6.0, Some((with_alpha(ink, 0.45), 1.0)));
    paint.icon(cx, icon, 14.0, ink, dvec2(x + 4.0 + 7.0, center_y));
    paint.text_centered(cx, Font::Regular, size, ink, x + 22.0, area.pos.y, height, &fitted, text_max + 1.0);
    let clear = rect(x + width - clear_width - 1.0, center_y - 8.0, 16.0, 16.0);
    if clear_hovered {
        paint.rounded(cx, clear, colors.interaction_highlight, 4.0, None);
    }
    paint.icon(cx, "x", 11.0, ink, clear.center());
    (area, Some(clear))
}

/// Lanes of one row from `left`: top and bottom edges, then the node.
/// Without a graph row (filtered lists) only the node is drawn. A row with
/// more lanes than `cap` folds the hidden ones into the last column: their
/// edges fade there and a commit on a hidden lane gets a hollow node.
/// Returns the lanes used.
#[allow(clippy::too_many_arguments)]
fn draw_graph(cx: &mut Cx2d, paint: &mut GitPaint, row: Option<&GraphRow>, left: f64, top: f64, height: f64, lane_width: f64, cap: usize, head: bool, background: Vec4) -> usize {
    let cap = cap.max(1);
    let x = |lane: usize| left + lane_width * (lane as f64 + 0.5);
    let (y0, yc, y1) = (top, top + height / 2.0, top + height);
    let stroke = if lane_width >= LANE_WIDTH { 1.6 } else { 1.4 };
    let radius = if lane_width >= LANE_WIDTH { 3.5 } else { 3.0 };
    let Some(row) = row else {
        let center = dvec2(x(0), yc);
        paint.disc(cx, center, radius, palette().muted);
        return 1;
    };
    let overflow = row.lanes > cap;
    let last = cap - 1;
    let hidden = |lane: usize| overflow && lane >= last;
    let column = |lane: usize| if overflow { lane.min(last) } else { lane };
    let edge_color = |from: usize, to: usize, color: usize| {
        let color = lane_color(color);
        if hidden(from) || hidden(to) { with_alpha(color, 0.35) } else { color }
    };
    for edge in &row.top {
        paint.segment(cx, dvec2(x(column(edge.from)), y0), dvec2(x(column(edge.to)), yc), stroke, edge_color(edge.from, edge.to, edge.color));
    }
    for edge in &row.bottom {
        paint.segment(cx, dvec2(x(column(edge.from)), yc), dvec2(x(column(edge.to)), y1), stroke, edge_color(edge.from, edge.to, edge.color));
    }
    let color = lane_color(row.color);
    let center = dvec2(x(column(row.lane)), yc);
    if head {
        paint.disc(cx, center, radius + 1.5, color);
        paint.disc(cx, center, radius - 0.5, background);
    } else if hidden(row.lane) {
        paint.disc(cx, center, radius, color);
        paint.disc(cx, center, radius - 1.3, background);
    } else {
        paint.disc(cx, center, radius, color);
    }
    row.lanes.clamp(1, cap)
}

/// A branches-panel row (built once per refs / query / collapse change).
pub(super) enum RefRow {
    Section { key: &'static str, count: Option<usize>, open: bool },
    Head,
    Local(usize),
    Group { remote: String, count: usize, open: bool },
    Remote(usize),
    Tag(usize),
}

fn head_label(refs: &Refs) -> String {
    match (&refs.head_branch, &refs.head_sha) {
        (Some(branch), _) if !refs.detached => format!("HEAD ({branch})"),
        (_, Some(sha)) => tr("workspace.git.log.branches.detached", &[("sha", short(sha))]),
        _ => "HEAD".to_owned(),
    }
}

/// HEAD, Local, Remote (per remote) and Tags rows matching `query` (lowercase).
fn ref_rows(refs: &Refs, query: &str, collapsed: &HashSet<String>) -> Vec<RefRow> {
    let searching = !query.is_empty();
    let matches = |name: &str| !searching || name.to_lowercase().contains(query);
    let expanded = |key: &str| searching || !collapsed.contains(key);
    let mut rows = Vec::new();
    if matches(&head_label(refs)) {
        let open = expanded("head");
        rows.push(RefRow::Section { key: "head", count: None, open });
        if open {
            rows.push(RefRow::Head);
        }
    }
    let local: Vec<usize> = (0..refs.local.len()).filter(|&i| matches(&refs.local[i].name)).collect();
    if !local.is_empty() || !searching {
        let open = expanded("local");
        rows.push(RefRow::Section { key: "local", count: Some(local.len()), open });
        if open {
            rows.extend(local.into_iter().map(RefRow::Local));
        }
    }
    let remote: Vec<usize> = (0..refs.remote.len()).filter(|&i| matches(&refs.remote[i].name)).collect();
    if !remote.is_empty() || !searching {
        let open = expanded("remote");
        rows.push(RefRow::Section { key: "remote", count: Some(remote.len()), open });
        if open {
            // The host sorts remote branches grouped per remote.
            let mut index = 0;
            while index < remote.len() {
                let name = refs.remote[remote[index]].remote.clone();
                let end = remote[index..].iter().position(|&i| refs.remote[i].remote != name).map_or(remote.len(), |n| index + n);
                let open = expanded(&format!("remote:{name}"));
                rows.push(RefRow::Group { remote: name, count: end - index, open });
                if open {
                    rows.extend(remote[index..end].iter().copied().map(RefRow::Remote));
                }
                index = end;
            }
        }
    }
    let tags: Vec<usize> = (0..refs.tags.len()).filter(|&i| matches(&refs.tags[i])).collect();
    if !tags.is_empty() || !searching {
        let open = expanded("tags");
        rows.push(RefRow::Section { key: "tags", count: Some(tags.len()), open });
        if open {
            rows.extend(tags.into_iter().map(RefRow::Tag));
        }
    }
    rows
}

fn section_label(key: &str) -> String {
    t(match key {
        "head" => "workspace.git.log.branches.head",
        "local" => "workspace.git.log.branches.local",
        "remote" => "workspace.git.log.branches.remote",
        _ => "workspace.git.log.branches.tags",
    })
}

impl LogView {
    fn is_hovered(&self, hit: &LogHit) -> bool {
        self.hovered.as_ref() == Some(hit)
    }

    fn style_inputs(&mut self, cx: &mut Cx2d, inputs: &mut LogInputs<'_>) {
        let colors = palette();
        let size = ui_px(13.0);
        let search = t("workspace.git.log.search.placeholder");
        let branch = t("workspace.git.log.branches.search");
        let key = format!("{:?}{:?}{size}{search}{branch}", colors.foreground, colors.muted);
        if key == self.input_style {
            return;
        }
        self.input_style = key;
        let (foreground, muted, font_size) = (colors.foreground, colors.muted, Typography::pt(size));
        for (input, placeholder) in [(&mut *inputs.search, search), (&mut *inputs.branch, branch)] {
            input.apply_over(
                cx,
                live! {
                    draw_text: { color: (foreground), color_empty: (muted), text_style: { font_size: (font_size) } }
                },
            );
            input.set_empty_text(cx, placeholder);
        }
    }

    /// Draws the whole tab into `area`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, inputs: &mut LogInputs<'_>, area: Rect, compact: bool) {
        self.hits.clear();
        self.spinning = false;
        self.icons_pending = false;
        for scroller in [&mut self.list, &mut self.branches, &mut self.files, &mut self.info, &mut self.sheet] {
            scroller.area = Rect::default();
        }
        self.style_inputs(cx, inputs);
        let colors = palette();
        paint.fill(cx, area, colors.surface0);
        self.phone = compact || area.size.x < 600.0;
        if !self.phone {
            self.screen = Screen::Log;
        }
        let state = match self.state.availability(ui) {
            Availability::Disconnected => Some(("unplug", "workspace.git.log.states.disconnected")),
            Availability::Unsupported => Some(("git-graph", "workspace.git.log.states.unsupported")),
            Availability::Ready => None,
        };
        if let Some((icon, key)) = state {
            self.message(cx, paint, area, Some(icon), &t(key), false, Vec::new(), false);
            return;
        }
        if self.phone {
            match self.screen.clone() {
                Screen::Log => self.draw_phone_log(cx, ui, paint, inputs, area),
                Screen::Commit(sha) => self.draw_phone_commit(cx, ui, paint, area, &sha),
            }
        } else {
            self.draw_desktop(cx, ui, paint, inputs, area);
        }
    }

    /// A centred state: optional icon or spinner, wrapped copy, action buttons side by side.
    #[allow(clippy::too_many_arguments)]
    fn message(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect, icon: Option<&str>, copy: &str, error: bool, actions: Vec<(String, LogHit)>, spinner: bool) {
        let colors = palette();
        let center_x = area.pos.x + area.size.x / 2.0;
        let mut y = area.pos.y + (area.size.y * 0.2).min(64.0);
        if spinner {
            paint.spinner(cx, dvec2(center_x, y + 12.0), 24.0, colors.muted, cx.seconds_since_app_start());
            self.spinning = true;
            y += 24.0 + 12.0;
        } else if let Some(icon) = icon {
            paint.icon(cx, icon, 24.0, colors.muted, dvec2(center_x, y + 12.0));
            y += 24.0 + 12.0;
        }
        let inner = (area.size.x - 48.0).max(0.0);
        if !copy.is_empty() && inner > 0.0 {
            let size = ui_px(13.0);
            let width = paint.measure(cx, Font::Regular, size, copy).x.min(inner);
            let color = if error { colors.danger } else { colors.muted };
            y += paint.text_wrapped(cx, Font::Regular, size, color, dvec2(center_x - width / 2.0, y), copy, width + 1.0) + 12.0;
        }
        if actions.is_empty() {
            return;
        }
        let compact = self.phone;
        let height = button_height(ButtonSize::Xs, compact);
        if y + height > area.pos.y + area.size.y {
            return;
        }
        let gap = 8.0;
        let widths: Vec<f64> = actions.iter().map(|(label, _)| measure_button(cx, paint, label, false, ButtonSize::Xs, compact)).collect();
        let total = widths.iter().sum::<f64>() + gap * (widths.len() - 1) as f64;
        // Buttons that do not fit side by side stack.
        let stacked = total > inner.max(1.0);
        let mut x = center_x - total / 2.0;
        for ((label, hit), width) in actions.into_iter().zip(widths) {
            let frame = if stacked { rect(center_x - width / 2.0, y, width, height) } else { rect(x, y, width, height) };
            if frame.pos.y + height > area.pos.y + area.size.y {
                break;
            }
            let hovered = self.is_hovered(&hit);
            button(cx, paint, frame, &label, None, ButtonVariant::Secondary, ButtonSize::Xs, compact, true, hovered);
            self.hits.push(frame, hit);
            if stacked {
                y += height + gap;
            } else {
                x += width + gap;
            }
        }
    }

    /// A search box: surface2 field, search glyph, the text input and a clear glyph while not empty.
    #[allow(clippy::too_many_arguments)]
    fn search_field(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, input: &mut TextInput, frame: Rect, has_text: bool, clear: LogHit, focus: bool) {
        let colors = palette();
        let focused = cx.has_key_focus(input.area());
        paint.rounded(cx, frame, colors.surface2, 6.0, focused.then_some((colors.muted, 1.0)));
        let center_y = frame.pos.y + frame.size.y / 2.0;
        paint.icon(cx, "search", 12.0, colors.muted, dvec2(frame.pos.x + 13.0, center_y));
        let left = frame.pos.x + 26.0;
        let right = frame.pos.x + frame.size.x - if has_text { 24.0 } else { 8.0 };
        let line = paint.measure(cx, Font::Regular, ui_px(13.0), "Ag").y + 2.0;
        if right - left > 8.0 {
            let walk = Walk {
                abs_pos: Some(dvec2(left, center_y - line / 2.0)),
                margin: Margin::default(),
                width: Size::Fixed(right - left),
                height: Size::Fixed(line),
            };
            let _ = input.draw_walk(cx, &mut Scope::empty(), walk);
            if focus {
                cx.set_key_focus(input.area());
            }
        }
        if has_text {
            let button = rect(frame.pos.x + frame.size.x - 22.0, center_y - 9.0, 18.0, 18.0);
            if self.is_hovered(&clear) {
                paint.rounded(cx, button, colors.interaction_highlight, 4.0, None);
            }
            paint.icon(cx, "x", 12.0, colors.muted, button.center());
            self.hits.push(button, clear);
        }
    }

    fn branch_label(&self) -> String {
        match &self.state.filters.revision {
            None => t("workspace.git.log.filters.allBranches"),
            Some(RefFilter { name, kind: None }) if is_hash_query(name) => short(name),
            Some(RefFilter { name, .. }) => name.clone(),
        }
    }

    fn refresh_button(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, compact: bool, right: f64, center_y: f64) -> f64 {
        let size = toolbar_button_size(compact);
        // A pending page or Checkout spins in place of the refresh button.
        if self.state.page_loading || self.switching.is_some() {
            let colors = palette();
            paint.spinner(cx, dvec2(right - size / 2.0, center_y), toolbar_icon_size(compact), colors.extra_muted, cx.seconds_since_app_start());
            self.spinning = true;
            return right - size - 4.0;
        }
        let hovered = self.is_hovered(&LogHit::Refresh);
        let button = toolbar_button(cx, paint, compact, right, center_y, Some("rotate-cw"), false, true, hovered);
        self.hits.push(button, LogHit::Refresh);
        button.pos.x - 4.0
    }

    /// The size of a split this frame: the dragged size (else `default`)
    /// within `min..=max`, remembered as the next drag's origin.
    fn split_size(&mut self, split: Split, default: f64, min: f64, max: f64) -> f64 {
        let value = self.sizes.get(split).unwrap_or(default).clamp(min, max.max(min));
        *self.shown_sizes.slot(split) = Some(value);
        value
    }

    /// A slim error banner under a toolbar when a refresh or the refs failed
    /// while rows stay shown; returns the area below it.
    fn draw_banner(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect) -> Rect {
        let message = if self.state.refresh_failed() {
            self.state.page_error.clone().map(|error| tr("workspace.git.log.states.refreshFailed", &[("error", error)]))
        } else if self.state.loaded {
            self.state.refs_error.clone().map(|error| tr("workspace.git.log.states.refsFailed", &[("error", error)]))
        } else {
            None
        };
        let Some(message) = message else { return area };
        let colors = palette();
        let size = ui_px(12.0);
        let height = (paint.measure(cx, Font::Regular, size, "Ag").y + 12.0).max(28.0).min(area.size.y);
        let band = rect(area.pos.x, area.pos.y, area.size.x, height);
        paint.fill(cx, band, with_alpha(colors.danger, 0.10));
        paint.fill(cx, rect(band.pos.x, band.pos.y + height - 1.0, band.size.x, 1.0), colors.border);
        let center_y = band.pos.y + height / 2.0;
        paint.icon(cx, "circle-alert", 12.0, colors.danger, dvec2(band.pos.x + 14.0, center_y));
        let mut right = band.pos.x + band.size.x - 6.0;
        let mut actions = vec![(t("workspace.git.log.states.retry"), LogHit::PageRetry)];
        if self.state.refresh_failed() && self.state.filters.revision.is_some() {
            actions.push((t("workspace.git.log.states.showAllBranches"), LogHit::ShowAllBranches));
        }
        for (label, hit) in actions {
            let width = paint.measure(cx, Font::Medium, size, &label).x + 16.0;
            if right - width < band.pos.x + 80.0 {
                break;
            }
            let frame = rect(right - width, center_y - 11.0, width, 22.0);
            if self.is_hovered(&hit) {
                paint.rounded(cx, frame, colors.interaction_highlight, 4.0, None);
            }
            paint.text_centered(cx, Font::Medium, size, colors.accent_bright, frame.pos.x + 8.0, frame.pos.y, frame.size.y, &label, width - 15.0);
            self.hits.push(frame, hit);
            right = frame.pos.x - 4.0;
        }
        let x = band.pos.x + 26.0;
        paint.text_centered(cx, Font::Regular, size, colors.danger, x, band.pos.y, height, &message, (right - 6.0 - x).max(0.0));
        rect(area.pos.x, area.pos.y + height, area.size.x, (area.size.y - height).max(0.0))
    }

    // -----------------------------------------------------------------------
    // Desktop.

    fn draw_desktop(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, inputs: &mut LogInputs<'_>, area: Rect) {
        let colors = palette();
        let toolbar_h = TOOLBAR_HEIGHT.max(ui_px(14.0) * 1.4 + 12.0);
        let wide = area.size.x >= 900.0;
        self.wide = wide;
        if wide {
            self.drawer_open = false;
        }
        self.draw_toolbar(cx, paint, inputs.search, rect(area.pos.x, area.pos.y, area.size.x, toolbar_h), wide);
        let body = rect(area.pos.x, area.pos.y + toolbar_h, area.size.x, (area.size.y - toolbar_h).max(0.0));
        let body = self.draw_banner(cx, paint, body);
        let mut rest = body;
        let mut splitters = Vec::new();
        if wide && self.branches_open {
            let width = self.split_size(Split::Branches, BRANCHES_WIDTH, BRANCHES_MIN_WIDTH, (body.size.x * 0.4).min(480.0));
            let panel = rect(body.pos.x, body.pos.y, width, body.size.y);
            self.draw_branches(cx, paint, inputs.branch, panel);
            paint.fill(cx, rect(body.pos.x + width, body.pos.y, 1.0, body.size.y), colors.border);
            splitters.push((rect(body.pos.x + width - 3.0, body.pos.y, 7.0, body.size.y), Split::Branches, false));
            rest = rect(body.pos.x + width + 1.0, body.pos.y, body.size.x - width - 1.0, body.size.y);
        }
        let side = if wide { area.size.x >= 1100.0 } else { area.size.x >= 700.0 };
        if side {
            let (target, min) = if wide { (360.0, 280.0) } else { (300.0, 240.0) };
            let width = self.split_size(Split::Side, target.min(rest.size.x - 360.0).max(min), min, rest.size.x - 300.0);
            let list = rect(rest.pos.x, rest.pos.y, rest.size.x - width - 1.0, rest.size.y);
            self.draw_table(cx, ui, paint, list);
            paint.fill(cx, rect(list.pos.x + list.size.x, rest.pos.y, 1.0, rest.size.y), colors.border);
            self.draw_details(cx, ui, paint, rect(list.pos.x + list.size.x + 1.0, rest.pos.y, width, rest.size.y), true);
            splitters.push((rect(list.pos.x + list.size.x - 3.0, rest.pos.y, 7.0, rest.size.y), Split::Side, false));
        } else {
            let min = 160.0_f64.min(rest.size.y);
            let height = self.split_size(Split::Stacked, rest.size.y * 0.42, min, rest.size.y - 120.0);
            let list = rect(rest.pos.x, rest.pos.y, rest.size.x, rest.size.y - height - 1.0);
            self.draw_table(cx, ui, paint, list);
            paint.fill(cx, rect(rest.pos.x, list.pos.y + list.size.y, rest.size.x, 1.0), colors.border);
            self.draw_details(cx, ui, paint, rect(rest.pos.x, list.pos.y + list.size.y + 1.0, rest.size.x, height), false);
            splitters.push((rect(rest.pos.x, list.pos.y + list.size.y - 3.0, rest.size.x, 7.0), Split::Stacked, true));
        }
        // Dividers sit over the edges of the regions they separate.
        for (frame, split, horizontal) in splitters {
            let hit = LogHit::Splitter(split);
            if self.is_hovered(&hit) || self.dragging() == Some(split) {
                let line = if horizontal {
                    rect(frame.pos.x, frame.pos.y + 2.5, frame.size.x, 2.0)
                } else {
                    rect(frame.pos.x + 2.5, frame.pos.y, 2.0, frame.size.y)
                };
                paint.fill(cx, line, colors.border_accent);
            }
            self.hits.push(frame, hit);
        }
        if !wide && self.drawer_open {
            self.draw_drawer(cx, paint, inputs.branch, body);
        }
    }

    /// The branches panel as a drawer over the narrow layout's body.
    fn draw_drawer(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, input: &mut TextInput, body: Rect) {
        let colors = palette();
        paint.fill(cx, body, vec4(0.0, 0.0, 0.0, 0.22));
        self.hits.push(body, LogHit::DrawerBackdrop);
        let width = self.sizes.get(Split::Branches).unwrap_or(BRANCHES_WIDTH).clamp(BRANCHES_MIN_WIDTH, 480.0).min((body.size.x - 48.0).max(0.0));
        let panel = rect(body.pos.x, body.pos.y, width, body.size.y);
        self.hits.push(panel, LogHit::Drawer);
        self.draw_branches(cx, paint, input, panel);
        paint.fill(cx, rect(panel.pos.x + width, panel.pos.y, 1.0, panel.size.y), colors.border);
    }

    /// One row: branches toggle, search, Branch / User / Path filters, refresh.
    fn draw_toolbar(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, search: &mut TextInput, band: Rect, wide: bool) {
        toolbar_band(cx, paint, band, false);
        let center_y = band.pos.y + (band.size.y - 1.0) / 2.0;
        let mut x = band.pos.x + 8.0;
        let hovered = self.is_hovered(&LogHit::BranchesToggle);
        let open = if wide { self.branches_open } else { self.drawer_open };
        let icon = if open { "panel-left-close" } else { "panel-left-open" };
        let button = toolbar_button(cx, paint, false, x + toolbar_button_size(false), center_y, Some(icon), open, true, hovered);
        self.hits.push(button, LogHit::BranchesToggle);
        x = button.pos.x + button.size.x + 8.0;
        let right = self.refresh_button(cx, paint, false, band.pos.x + band.size.x - toolbar_trailing_padding(false, false), center_y);
        let available = (right - 8.0 - x).max(0.0);
        let field_width = (available * 0.4).clamp(120.0_f64.min(available), 260.0_f64.min(available));
        let focus = std::mem::take(&mut self.focus_search);
        if field_width >= 60.0 {
            let has_text = !self.search_text.is_empty();
            let field = rect(x, center_y - 12.0, field_width, 24.0);
            self.search_field(cx, paint, search, field, has_text, LogHit::SearchClear, focus);
            x += field_width + 8.0;
        }
        let path = self.state.filters.path.clone();
        let mut remaining = (right - 8.0 - x).max(0.0);
        let branch = tr("workspace.git.log.filters.branch", &[("name", self.branch_label())]);
        let share = if path.is_some() { 0.4 } else { 0.55 };
        let max = remaining * share;
        if max >= 48.0 {
            let open = matches!(self.overlay.purpose(), Some(super::log_view::Purpose::Branch));
            let highlighted = open || self.is_hovered(&LogHit::BranchChip);
            let active = self.state.filters.revision.is_some();
            let (trigger, clear) = filter_chip(cx, paint, x, center_y, max, &branch, "git-branch", active, highlighted, self.is_hovered(&LogHit::ClearBranch));
            self.hits.push(trigger, LogHit::BranchChip);
            if let Some(clear) = clear {
                self.hits.push(clear, LogHit::ClearBranch);
            }
            x = trigger.pos.x + trigger.size.x + 4.0;
            remaining = (right - 8.0 - x).max(0.0);
        }
        let author = self.state.filters.author.clone();
        let user = tr("workspace.git.log.filters.user", &[("name", author.clone().unwrap_or_else(|| t("workspace.git.log.filters.allUsers")))]);
        let max = if path.is_some() { remaining * 0.6 } else { remaining };
        if max >= 48.0 {
            let open = matches!(self.overlay.purpose(), Some(super::log_view::Purpose::Author));
            let highlighted = open || self.is_hovered(&LogHit::AuthorChip);
            let (trigger, clear) = filter_chip(cx, paint, x, center_y, max, &user, "user", author.is_some(), highlighted, self.is_hovered(&LogHit::ClearAuthor));
            self.hits.push(trigger, LogHit::AuthorChip);
            if let Some(clear) = clear {
                self.hits.push(clear, LogHit::ClearAuthor);
            }
            x = trigger.pos.x + trigger.size.x + 4.0;
            remaining = (right - 8.0 - x).max(0.0);
        }
        if let Some(path) = path
            && remaining >= 48.0
        {
            let label = tr("workspace.git.log.filters.path", &[("name", path.clone())]);
            let hit = LogHit::PathChip(path);
            let highlighted = self.is_hovered(&hit);
            let (trigger, clear) = filter_chip(cx, paint, x, center_y, remaining, &label, "file-clock", true, highlighted, self.is_hovered(&LogHit::ClearPath));
            self.hits.push(trigger, hit);
            if let Some(clear) = clear {
                self.hits.push(clear, LogHit::ClearPath);
            }
        }
    }

    /// Whether `filter` selects this branches-panel row.
    pub(super) fn ref_selected(&self, kind: RefKind, name: &str) -> bool {
        self.state
            .filters
            .revision
            .as_ref()
            .is_some_and(|f| f.name == name && f.kind.is_none_or(|k| k == kind))
    }

    /// The branches panel: search, then HEAD, Local, Remote (per remote) and Tags.
    fn draw_branches(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, input: &mut TextInput, area: Rect) {
        let colors = palette();
        paint.fill(cx, area, colors.sidebar);
        let field = rect(area.pos.x + 8.0, area.pos.y + 8.0, area.size.x - 16.0, 26.0);
        let has_text = !self.branch_query.is_empty();
        self.search_field(cx, paint, input, field, has_text, LogHit::BranchSearchClear, false);
        let list = rect(area.pos.x, field.pos.y + field.size.y + 6.0, area.size.x, (area.pos.y + area.size.y - field.pos.y - field.size.y - 6.0).max(0.0));
        // Moved out while drawing (no per-frame copy of every ref).
        let Some(refs) = self.state.refs.take() else {
            match self.state.refs_error.clone() {
                Some(error) => {
                    let copy = tr("workspace.git.log.states.refsFailed", &[("error", error)]);
                    let retry = vec![(t("workspace.git.log.states.retry"), LogHit::PageRetry)];
                    self.message(cx, paint, list, Some("circle-alert"), &copy, true, retry, false);
                }
                None => self.message(cx, paint, list, None, "", false, Vec::new(), true),
            }
            return;
        };
        self.draw_ref_rows(cx, paint, &refs, list);
        self.state.refs = Some(refs);
    }

    fn draw_ref_rows(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, refs: &Refs, list: Rect) {
        let colors = palette();
        let query = self.branch_query.trim().to_lowercase();
        let version = self.state.refs_version;
        let stale = self
            .ref_rows_key
            .as_ref()
            .is_none_or(|(refs_version, built_query, collapsed)| *refs_version != version || *built_query != query || *collapsed != self.collapsed_version);
        if stale {
            self.ref_rows = ref_rows(refs, &query, &self.collapsed);
            self.ref_rows_key = Some((version, query, self.collapsed_version));
        }
        if self.ref_rows.is_empty() {
            self.message(cx, paint, list, None, &t("workspace.git.log.branches.empty"), false, Vec::new(), false);
            return;
        }
        let size = ui_px(13.0);
        let row_h = (paint.measure(cx, Font::Regular, size, "Ag").y + 8.0).max(24.0);
        self.branches.set(list, self.ref_rows.len() as f64 * row_h + 8.0);
        let offset = self.branches.offset;
        // Moved out while drawing (rows borrow nothing from `self`).
        let rows = std::mem::take(&mut self.ref_rows);
        let first = ((offset / row_h).floor() as usize).min(rows.len());
        paint.begin_clip(cx, list);
        for (index, row) in rows.iter().enumerate().skip(first) {
            let top = list.pos.y + index as f64 * row_h - offset;
            if top > list.pos.y + list.size.y {
                break;
            }
            let band = rect(list.pos.x, top, list.size.x, row_h);
            let center_y = top + row_h / 2.0;
            match row {
                RefRow::Section { key, count, open } => {
                    let hit = LogHit::Section((*key).to_owned());
                    self.hits.push_clipped(band, list, hit.clone());
                    if self.is_hovered(&hit) {
                        paint.fill(cx, band, colors.sidebar_hover);
                    }
                    let chevron = if *open { "chevron-down" } else { "chevron-right" };
                    paint.icon(cx, chevron, 12.0, colors.muted, dvec2(band.pos.x + 14.0, center_y));
                    let label = section_label(key);
                    let used = paint.text_centered(cx, Font::Medium, ui_px(12.0), colors.muted, band.pos.x + 24.0, top, row_h, &label, band.size.x - 64.0);
                    if let Some(count) = count {
                        paint.text_centered(cx, Font::Regular, ui_px(11.0), colors.muted, band.pos.x + 24.0 + used + 6.0, top, row_h, &count.to_string(), 40.0);
                    }
                }
                RefRow::Head => {
                    let selected = self.ref_selected(RefKind::Head, "HEAD");
                    let hit = LogHit::Ref { kind: RefKind::Head, name: "HEAD".to_owned() };
                    let label = head_label(refs);
                    self.branch_row(cx, paint, band, list, hit, selected, 24.0, "crosshair", colors.muted, &label, Font::Regular, None, None, false);
                }
                RefRow::Local(i) => {
                    let branch = &refs.local[*i];
                    let selected = self.ref_selected(RefKind::Local, &branch.name);
                    let hit = LogHit::Ref { kind: RefKind::Local, name: branch.name.clone() };
                    let menu = LogHit::RefMenu { kind: RefKind::Local, name: branch.name.clone() };
                    let (icon, color, font) = if branch.current { ("circle-dot", colors.accent_bright, Font::Medium) } else { ("git-branch", colors.muted, Font::Regular) };
                    let trailing = if branch.gone {
                        Some((t("workspace.git.log.branches.gone"), colors.danger))
                    } else {
                        ahead_behind(branch.ahead, branch.behind).map(|label| (label, colors.muted))
                    };
                    let busy = self.switching.as_deref() == Some(branch.name.as_str());
                    self.branch_row(cx, paint, band, list, hit, selected, 24.0, icon, color, &branch.name, font, trailing, Some(menu), busy);
                }
                RefRow::Group { remote, count, open } => {
                    let hit = LogHit::Section(format!("remote:{remote}"));
                    self.hits.push_clipped(band, list, hit.clone());
                    if self.is_hovered(&hit) {
                        paint.fill(cx, band, colors.sidebar_hover);
                    }
                    let chevron = if *open { "chevron-down" } else { "chevron-right" };
                    paint.icon(cx, chevron, 12.0, colors.muted, dvec2(band.pos.x + 28.0, center_y));
                    paint.icon(cx, "cloud", 13.0, colors.muted, dvec2(band.pos.x + 44.0, center_y));
                    let used = paint.text_centered(cx, Font::Regular, size, colors.foreground, band.pos.x + 54.0, top, row_h, remote, band.size.x - 94.0);
                    paint.text_centered(cx, Font::Regular, ui_px(11.0), colors.muted, band.pos.x + 54.0 + used + 6.0, top, row_h, &count.to_string(), 36.0);
                }
                RefRow::Remote(i) => {
                    let branch = &refs.remote[*i];
                    let selected = self.ref_selected(RefKind::Remote, &branch.name);
                    let hit = LogHit::Ref { kind: RefKind::Remote, name: branch.name.clone() };
                    let menu = LogHit::RefMenu { kind: RefKind::Remote, name: branch.name.clone() };
                    let busy = self.switching.as_deref() == Some(branch.branch.as_str()) && !refs.local.iter().any(|b| b.name == branch.branch);
                    self.branch_row(cx, paint, band, list, hit, selected, 52.0, "git-branch", colors.muted, &branch.branch, Font::Regular, None, Some(menu), busy);
                }
                RefRow::Tag(i) => {
                    let tag = &refs.tags[*i];
                    let selected = self.ref_selected(RefKind::Tag, tag);
                    let hit = LogHit::Ref { kind: RefKind::Tag, name: tag.clone() };
                    self.branch_row(cx, paint, band, list, hit, selected, 24.0, "tag", colors.muted, tag, Font::Regular, None, None, false);
                }
            }
        }
        paint.end_clip(cx);
        self.ref_rows = rows;
    }

    /// One ref row: highlight, icon, name, trailing note (a spinner while a
    /// Checkout to it runs) and the ⋯ menu button (shown while hovered or
    /// selected), registered after the row.
    #[allow(clippy::too_many_arguments)]
    fn branch_row(
        &mut self,
        cx: &mut Cx2d,
        paint: &mut GitPaint,
        band: Rect,
        clip: Rect,
        hit: LogHit,
        selected: bool,
        indent: f64,
        icon: &str,
        icon_color: Vec4,
        label: &str,
        font: Font,
        trailing: Option<(String, Vec4)>,
        menu: Option<LogHit>,
        busy: bool,
    ) {
        let colors = palette();
        let center_y = band.pos.y + band.size.y / 2.0;
        self.hits.push_clipped(band, clip, hit.clone());
        let hovered = self.is_hovered(&hit) || menu.as_ref().is_some_and(|m| self.is_hovered(m));
        if selected {
            paint.fill(cx, band, colors.selected);
        } else if hovered {
            paint.fill(cx, band, colors.sidebar_hover);
        }
        paint.icon(cx, icon, 13.0, icon_color, dvec2(band.pos.x + indent + 6.0, center_y));
        let mut right = band.pos.x + band.size.x - 8.0;
        if let Some(menu) = menu.filter(|_| hovered || selected) {
            let button = rect(right - 20.0, center_y - 10.0, 20.0, 20.0);
            if self.is_hovered(&menu) {
                paint.rounded(cx, button, colors.interaction_highlight, 4.0, None);
            }
            paint.icon(cx, "ellipsis", 13.0, colors.muted, button.center());
            self.hits.push_clipped(button, clip, menu);
            right = button.pos.x - 4.0;
        }
        if busy {
            paint.spinner(cx, dvec2(right - 6.0, center_y), 12.0, colors.muted, cx.seconds_since_app_start());
            self.spinning = true;
            right -= 18.0;
        } else if let Some((note, color)) = trailing {
            let size = ui_px(11.0);
            let width = paint.measure(cx, Font::Regular, size, &note).x;
            if right - width > band.pos.x + indent + 60.0 {
                paint.text_centered(cx, Font::Regular, size, color, right - width, band.pos.y, band.size.y, &note, width + 1.0);
                right -= width + 6.0;
            }
        }
        let x = band.pos.x + indent + 16.0;
        paint.text_centered(cx, font, ui_px(13.0), colors.foreground, x, band.pos.y, band.size.y, label, (right - x).max(0.0));
    }

    /// Loading, error and empty states of the commit list; true when one was drawn.
    fn list_state(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, body: Rect, row_h: f64) -> bool {
        if !self.state.loaded {
            if let Some(error) = self.state.page_error.clone() {
                let mut actions = vec![(t("workspace.git.log.states.retry"), LogHit::PageRetry)];
                // A deleted branch or an unborn HEAD fails every retry.
                if self.state.filters.revision.is_some() {
                    actions.push((t("workspace.git.log.states.showAllBranches"), LogHit::ShowAllBranches));
                }
                self.message(cx, paint, body, Some("circle-alert"), &error, true, actions, false);
            } else {
                // Skeleton rows while the first page loads.
                let colors = palette();
                let count = ((body.size.y / row_h).ceil() as usize).min(12);
                for index in 0..count {
                    let top = body.pos.y + index as f64 * row_h;
                    let center_y = top + row_h / 2.0;
                    paint.disc(cx, dvec2(body.pos.x + 13.0, center_y), 3.5, colors.surface2);
                    let width = (body.size.x - 60.0) * (0.35 + 0.08 * (index % 5) as f64);
                    paint.rounded(cx, rect(body.pos.x + 28.0, center_y - 5.0, width.max(0.0), 10.0), colors.surface2, 4.0, None);
                }
                self.spinning = true;
            }
            return true;
        }
        if self.state.commits.is_empty() {
            if self.state.filters.is_active() {
                let actions = vec![(t("workspace.git.log.filters.clearAll"), LogHit::ClearFilters)];
                self.message(cx, paint, body, Some("git-commit-horizontal"), &t("workspace.git.log.states.noMatches"), false, actions, false);
            } else {
                self.message(cx, paint, body, Some("git-commit-horizontal"), &t("workspace.git.log.states.empty"), false, Vec::new(), false);
            }
            return true;
        }
        false
    }

    /// The footer height after the last row: paging spinner, later-page error, or more to load.
    fn footer_height(&self, phone: bool) -> f64 {
        if self.state.page_loading || self.state.page_error_at_end() || self.state.has_more {
            if phone { 48.0 } else { 32.0 }
        } else {
            0.0
        }
    }

    /// The page row after the last commit: a spinner or the error with retry.
    fn draw_footer(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, band: Rect, clip: Rect) {
        let colors = palette();
        let center_y = band.pos.y + band.size.y / 2.0;
        if let Some(error) = self.state.page_error.clone().filter(|_| self.state.page_error_at_end()) {
            let label = t("workspace.git.log.states.retry");
            let size = ui_px(12.0);
            let label_w = paint.measure(cx, Font::Medium, size, &label).x + 16.0;
            let retry = rect(band.pos.x + band.size.x - 8.0 - label_w, center_y - 11.0, label_w, 22.0);
            paint.text_centered(cx, Font::Regular, size, colors.danger, band.pos.x + 12.0, band.pos.y, band.size.y, &error, (retry.pos.x - band.pos.x - 20.0).max(0.0));
            if self.is_hovered(&LogHit::PageRetry) {
                paint.rounded(cx, retry, colors.interaction_highlight, 4.0, None);
            }
            paint.text_centered(cx, Font::Medium, size, colors.accent_bright, retry.pos.x + 8.0, retry.pos.y, retry.size.y, &label, label_w - 15.0);
            self.hits.push_clipped(retry, clip, LogHit::PageRetry);
        } else if self.state.page_loading {
            paint.spinner(cx, dvec2(band.pos.x + band.size.x / 2.0, center_y), 16.0, colors.muted, cx.seconds_since_app_start());
            self.spinning = true;
        }
    }

    /// Asks for the next page while the end is within two screens.
    fn maybe_page(&mut self, ui: &UiContext, rows_end: f64) {
        let view = self.list.area.size.y;
        if self.state.has_more && rows_end - (self.list.offset + view) < view * 2.0 {
            self.state.load_more(ui);
        }
    }

    /// The commit table: header, then rows of graph + subject with ref chips | author | date | hash.
    fn draw_table(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, area: Rect) {
        let colors = palette();
        let small = ui_px(12.0);
        let size = ui_px(13.0);
        let header_h = (paint.measure(cx, Font::Medium, small, "Ag").y + 8.0).max(24.0);
        let row_h = (paint.measure(cx, Font::Regular, size, "Ag").y + 8.0).max(26.0);
        self.row_height = row_h;
        let hash_w = paint.measure(cx, Font::Mono, small, "00000000").x + 20.0;
        let date_w = if area.size.x >= 420.0 { (paint.measure(cx, Font::Regular, small, "0000-00-00").x + 24.0).max(96.0) } else { 0.0 };
        let author_w = if area.size.x >= 560.0 { (area.size.x * 0.18).clamp(100.0, 180.0) } else { 0.0 };
        let subject_w = (area.size.x - hash_w - date_w - author_w).max(0.0);
        let lane_cap = ((subject_w * GRAPH_SHARE / LANE_WIDTH).floor() as usize).clamp(2, MAX_LANES);
        let header = rect(area.pos.x, area.pos.y, area.size.x, header_h);
        paint.fill(cx, rect(header.pos.x, header.pos.y + header_h - 1.0, header.size.x, 1.0), colors.border);
        let columns = [
            (area.pos.x + 12.0, subject_w - 16.0, "workspace.git.log.columns.subject"),
            (area.pos.x + subject_w, author_w - 8.0, "workspace.git.log.columns.author"),
            (area.pos.x + subject_w + author_w, date_w - 8.0, "workspace.git.log.columns.date"),
            (area.pos.x + subject_w + author_w + date_w, hash_w - 8.0, "workspace.git.log.columns.hash"),
        ];
        for (x, width, key) in columns {
            if width > 0.0 {
                paint.text_centered(cx, Font::Medium, small, colors.muted, x, header.pos.y, header_h - 1.0, &t(key), width);
            }
        }
        let body = rect(area.pos.x, area.pos.y + header_h, area.size.x, (area.size.y - header_h).max(0.0));
        if self.list_state(cx, paint, body, row_h) {
            self.list.set(body, 0.0);
            return;
        }
        let count = self.state.commits.len();
        let footer = self.footer_height(false);
        let content = count as f64 * row_h + footer;
        self.list.set(body, content);
        let offset = self.list.offset;
        let now = now_ms();
        let head = self.state.refs.as_ref().and_then(|r| r.head_sha.clone());
        paint.begin_clip(cx, body);
        let first = (offset / row_h).floor() as usize;
        for index in first..count {
            let top = body.pos.y + index as f64 * row_h - offset;
            if top > body.pos.y + body.size.y {
                break;
            }
            let band = rect(body.pos.x, top, body.size.x, row_h);
            self.hits.push_clipped(band, body, LogHit::Row(index));
            let commit = &self.state.commits[index];
            let date_cell = rect(band.pos.x + subject_w + author_w, top, date_w, row_h);
            if date_w > 0.0 && commit.author_ms.is_some() {
                self.hits.push_clipped(date_cell, body, LogHit::Date(index));
            }
            let selected = self.state.selected.as_deref() == Some(commit.sha.as_str());
            let hovered = self.is_hovered(&LogHit::Row(index)) || self.is_hovered(&LogHit::Date(index));
            let background = if selected {
                colors.selected
            } else if hovered {
                colors.sidebar_hover
            } else {
                colors.surface0
            };
            if selected || background != colors.surface0 {
                paint.fill(cx, band, background);
            }
            let center_y = top + row_h / 2.0;
            let is_head = head.as_deref() == Some(commit.sha.as_str());
            let graph_left = band.pos.x + 6.0;
            let lanes = draw_graph(cx, paint, self.state.rows.get(index), graph_left, top, row_h, LANE_WIDTH, lane_cap, is_head, background);
            let mut x = graph_left + lanes as f64 * LANE_WIDTH + 6.0;
            let subject_right = band.pos.x + subject_w - 8.0;
            let chips_limit = x + ((subject_right - x) * 0.5).max(0.0);
            for (position, label) in commit.refs.iter().enumerate() {
                let width = chip_width(cx, paint, &label.name);
                let remaining = commit.refs.len() - position;
                let fits = x + width.min(120.0) <= chips_limit;
                // The first decoration is shortened rather than hidden behind `+n`.
                if !fits && (position > 0 || chips_limit - x < 48.0) {
                    x += more_chip(cx, paint, x, center_y, remaining) + 4.0;
                    break;
                }
                x += chip(cx, paint, x, center_y, label, width.min(chips_limit - x).min(160.0)) + 4.0;
                if !fits {
                    if remaining > 1 {
                        x += more_chip(cx, paint, x, center_y, remaining - 1) + 4.0;
                    }
                    break;
                }
            }
            paint.text_centered(cx, Font::Regular, size, colors.foreground, x + 2.0, top, row_h, &commit.subject, (subject_right - x - 2.0).max(0.0));
            if author_w > 0.0 {
                paint.text_centered(cx, Font::Regular, small, colors.muted, band.pos.x + subject_w, top, row_h, &commit.author_name, author_w - 8.0);
            }
            if date_w > 0.0 {
                let date = commit.author_ms.map(|ms| relative_time(ms, now)).unwrap_or_default();
                paint.text_centered(cx, Font::Regular, small, colors.muted, date_cell.pos.x, top, row_h, &date, date_w - 8.0);
            }
            // Always 8 digits: the column is measured for them (git's `%h` grows in big repositories).
            let hash = commit.sha.get(..8).unwrap_or(&commit.sha);
            paint.text_centered(cx, Font::Mono, small, colors.muted, band.pos.x + subject_w + author_w + date_w, top, row_h, hash, hash_w - 8.0);
        }
        if footer > 0.0 {
            let top = body.pos.y + count as f64 * row_h - offset;
            if top < body.pos.y + body.size.y {
                self.draw_footer(cx, paint, rect(body.pos.x, top, body.size.x, footer), body);
            }
        }
        paint.end_clip(cx);
        self.maybe_page(ui, count as f64 * row_h);
    }

    /// The selected commit's changed files and message, side by side with the
    /// table (`side`, files above the message) or below it (files beside it).
    fn draw_details(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, area: Rect, side: bool) {
        let Some(sha) = self.state.selected.clone() else {
            if !self.state.commits.is_empty() {
                self.message(cx, paint, area, Some("git-commit-horizontal"), &t("workspace.git.log.details.select"), false, Vec::new(), false);
            }
            return;
        };
        // A keyboard selection asks once its pause ends.
        if !self.detail_waiting {
            self.state.ensure_detail(ui, &sha);
        }
        // Moved out while drawing (no per-frame copy of the file list).
        let Some(detail) = self.state.details.remove(&sha) else {
            self.message(cx, paint, area, None, "", false, Vec::new(), true);
            return;
        };
        match &detail {
            Detail::Loading => self.message(cx, paint, area, None, "", false, Vec::new(), true),
            Detail::Failed(error) => {
                let retry = vec![(t("workspace.git.log.states.retry"), LogHit::DetailRetry(sha.clone()))];
                self.message(cx, paint, area, Some("circle-alert"), error, true, retry, false);
            }
            Detail::Missing => self.message(cx, paint, area, Some("circle-alert"), &t("workspace.git.log.details.missing"), false, Vec::new(), false),
            Detail::Ready(commit) => {
                let colors = palette();
                if side {
                    let files_h = (area.size.y * 0.55).round();
                    self.draw_files(cx, paint, commit, rect(area.pos.x, area.pos.y, area.size.x, files_h));
                    paint.fill(cx, rect(area.pos.x, area.pos.y + files_h, area.size.x, 1.0), colors.border);
                    self.draw_info(cx, paint, commit, rect(area.pos.x, area.pos.y + files_h + 1.0, area.size.x, area.size.y - files_h - 1.0));
                } else {
                    let files_w = (area.size.x * 0.5).round();
                    self.draw_files(cx, paint, commit, rect(area.pos.x, area.pos.y, files_w, area.size.y));
                    paint.fill(cx, rect(area.pos.x + files_w, area.pos.y, 1.0, area.size.y), colors.border);
                    self.draw_info(cx, paint, commit, rect(area.pos.x + files_w + 1.0, area.pos.y, area.size.x - files_w - 1.0, area.size.y));
                }
            }
        }
        self.state.details.insert(sha, detail);
    }

    /// Lists `commit`'s file paths for keyboard moves (once per commit).
    fn list_files(&mut self, commit: &CommitDetail) {
        if self.listed_for.as_deref() != Some(commit.sha.as_str()) {
            self.listed_files = commit.files.iter().map(|f| text(f, "path").to_owned()).collect();
            self.listed_for = Some(commit.sha.clone());
        }
    }

    /// "Changed files (n)" and the 24px file rows.
    fn draw_files(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, commit: &CommitDetail, area: Rect) {
        let colors = palette();
        let small = ui_px(12.0);
        let header_h = (paint.measure(cx, Font::Medium, small, "Ag").y + 12.0).max(28.0);
        let title = tr("workspace.git.log.details.changedFiles", &[("count", commit.files.len().to_string())]);
        paint.text_centered(cx, Font::Medium, small, colors.muted, area.pos.x + 12.0, area.pos.y, header_h, &title, area.size.x - 24.0);
        let list = rect(area.pos.x, area.pos.y + header_h, area.size.x, (area.size.y - header_h).max(0.0));
        let row_h = (paint.measure(cx, Font::Regular, ui_px(13.0), "Ag").y + 6.0).max(24.0);
        self.file_row_height = row_h;
        self.list_files(commit);
        if commit.files.is_empty() {
            paint.text_centered(cx, Font::Regular, small, colors.muted, list.pos.x + 12.0, list.pos.y, row_h, &t("workspace.git.log.details.noFiles"), list.size.x - 24.0);
            self.files.set(list, 0.0);
            return;
        }
        self.files.set(list, commit.files.len() as f64 * row_h + 8.0);
        let offset = self.files.offset;
        let first = (offset / row_h).floor() as usize;
        paint.begin_clip(cx, list);
        for (index, file) in commit.files.iter().enumerate().skip(first) {
            let top = list.pos.y + index as f64 * row_h - offset;
            if top > list.pos.y + list.size.y {
                break;
            }
            let path = text(file, "path");
            let band = rect(list.pos.x, top, list.size.x, row_h);
            let hit = LogHit::File(path.to_owned());
            self.hits.push_clipped(band, list, hit.clone());
            let selected = self.selected_file.as_deref() == Some(path);
            if selected && self.focus == Focus::Files {
                paint.fill(cx, band, colors.selected);
            } else if selected || self.is_hovered(&hit) {
                paint.fill(cx, band, colors.sidebar_hover);
            }
            self.file_line(cx, paint, band, file, band.pos.x + 10.0);
        }
        paint.end_clip(cx);
    }

    /// Status letter, file icon, name, muted directory and the diff stat in one line of `band`.
    fn file_line(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, band: Rect, file: &Value, x: f64) {
        let colors = palette();
        let center_y = band.pos.y + band.size.y / 2.0;
        let (letter, color) = status_letter(file);
        paint.text_centered(cx, Font::Mono, ui_px(12.0), color, x, band.pos.y, band.size.y, letter, 12.0);
        let path = text(file, "path");
        if !paint.file_icon(cx, rect(x + 14.0, center_y - 8.0, 16.0, 16.0), basename(path)) {
            self.icons_pending = true;
        }
        let right = band.pos.x + band.size.x - 10.0;
        let stat = diff_stat(cx, paint, right, center_y, int(file, "additions"), int(file, "deletions"), true);
        let text_x = x + 36.0;
        let max = (right - stat - 8.0 - text_x).max(0.0);
        let (name, dir) = file_label(file);
        let size = ui_px(13.0);
        let used = paint.text_centered(cx, Font::Regular, size, colors.foreground, text_x, band.pos.y, band.size.y, &name, max);
        if !dir.is_empty() && max - used > 40.0 {
            paint.text_centered(cx, Font::Regular, ui_px(12.0), colors.muted, text_x + used + 6.0, band.pos.y, band.size.y, &dir, max - used - 6.0);
        }
    }

    /// Subject, body, author/committer, hash with copy, parents and refs (scrolls).
    fn draw_info(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, commit: &CommitDetail, area: Rect) {
        let offset = self.info.offset;
        paint.begin_clip(cx, area);
        let end = self.commit_text(cx, paint, commit, area, area.pos.y + 12.0 - offset, false);
        paint.end_clip(cx);
        self.info.set(area, end + offset - area.pos.y + 12.0);
    }

    /// The commit's message and metadata from `y` inside `area` (hits clipped
    /// to it); returns the y below the last line.
    fn commit_text(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, commit: &CommitDetail, area: Rect, mut y: f64, phone: bool) -> f64 {
        let colors = palette();
        let pad = if phone { 16.0 } else { 12.0 };
        let x = area.pos.x + pad;
        let width = (area.size.x - pad * 2.0).max(1.0);
        let (title, body, meta) = if phone { (ui_px(16.0), ui_px(14.0), ui_px(13.0)) } else { (ui_px(14.0), ui_px(13.0), ui_px(12.0)) };
        y += paint.text_wrapped(cx, Font::Medium, title, colors.foreground, dvec2(x, y), &commit.subject, width);
        if !commit.body.is_empty() {
            y += 8.0;
            y += paint.text_wrapped(cx, Font::Regular, body, with_alpha(colors.foreground, 0.85), dvec2(x, y), &commit.body, width);
        }
        y += 14.0;
        let line = paint.measure(cx, Font::Regular, meta, "Ag").y;
        let author = format!("{} <{}>", commit.author_name, commit.author_email);
        paint.text_fit(cx, Font::Regular, meta, colors.foreground, dvec2(x, y), &author, width);
        y += line + 2.0;
        let authored = commit.author_ms.map(|ms| local_time(ms, "%Y-%m-%d %H:%M")).unwrap_or_default();
        paint.text_fit(cx, Font::Regular, meta, colors.muted, dvec2(x, y), &authored, width);
        y += line + 6.0;
        let other_committer = commit.committer_name != commit.author_name || commit.committer_email != commit.author_email;
        if other_committer || commit.committer_ms != commit.author_ms {
            let committed = commit.committer_ms.map(|ms| local_time(ms, "%Y-%m-%d %H:%M")).unwrap_or_default();
            let copy = tr(
                "workspace.git.log.details.committedBy",
                &[("name", format!("{} <{}>", commit.committer_name, commit.committer_email)), ("date", committed)],
            );
            y += paint.text_wrapped(cx, Font::Regular, meta, colors.muted, dvec2(x, y), &copy, width) + 6.0;
        }
        // Full hash with its copy button.
        let row_h = (line + 8.0).max(if phone { 32.0 } else { 22.0 });
        let center_y = y + row_h / 2.0;
        let label_w = paint.text_centered(cx, Font::Regular, meta, colors.muted, x, y, row_h, &t("workspace.git.log.details.hash"), width * 0.3) + 8.0;
        let copy_size = if phone { 32.0 } else { 20.0 };
        let copy = rect(x + width - copy_size, center_y - copy_size / 2.0, copy_size, copy_size);
        paint.text_centered(cx, Font::Mono, meta, colors.foreground, x + label_w, y, row_h, &commit.sha, (copy.pos.x - 4.0 - x - label_w).max(0.0));
        let copy_hit = LogHit::CopyHash(commit.sha.clone());
        if self.is_hovered(&copy_hit) {
            paint.rounded(cx, copy, colors.interaction_highlight, 4.0, None);
        }
        paint.icon(cx, "copy", if phone { 16.0 } else { 12.0 }, colors.muted, copy.center());
        self.hits.push_clipped(copy, area, copy_hit);
        y += row_h + 4.0;
        // Parents: short hashes that select a loaded commit.
        if !commit.parents.is_empty() {
            let key = if commit.parents.len() == 1 { "workspace.git.log.details.parent" } else { "workspace.git.log.details.parents" };
            let mut cursor = x + paint.text_centered(cx, Font::Regular, meta, colors.muted, x, y, row_h, &t(key), width * 0.3) + 8.0;
            for parent in &commit.parents {
                let label = short(parent);
                let label_w = paint.measure(cx, Font::Mono, meta, &label).x;
                if cursor + label_w > x + width {
                    y += row_h;
                    cursor = x;
                }
                let loaded = self.state.index_of(parent).is_some();
                let frame = rect(cursor - 4.0, y + (row_h - line - 6.0) / 2.0, label_w + 8.0, line + 6.0);
                let hit = LogHit::Parent(parent.clone());
                if loaded && self.is_hovered(&hit) {
                    paint.rounded(cx, frame, colors.interaction_highlight, 4.0, None);
                }
                paint.text_centered(cx, Font::Mono, meta, if loaded { colors.accent_bright } else { colors.muted }, cursor, y, row_h, &label, label_w + 1.0);
                if loaded {
                    let target = if phone { rect(frame.pos.x, y, frame.size.x, row_h) } else { frame };
                    self.hits.push_clipped(target, area, hit);
                }
                cursor += label_w + 12.0;
            }
            y += row_h + 4.0;
        }
        // Refs, wrapped (the listed row's are newer than the cached details').
        let refs: &[RefLabel] = match self.state.commit(&commit.sha) {
            Some(listed) => &listed.refs,
            None => &commit.refs,
        };
        if !refs.is_empty() {
            let chip_h = chip_height() + 4.0;
            let mut cursor = x;
            for label in refs {
                let w = chip_width(cx, paint, &label.name).min(width);
                if cursor > x && cursor + w > x + width {
                    cursor = x;
                    y += chip_h + 4.0;
                }
                cursor += chip(cx, paint, cursor, y + chip_h / 2.0, label, width) + 4.0;
            }
            y += chip_h + 4.0;
        }
        y
    }

    // -----------------------------------------------------------------------
    // Phone.

    /// The filters the phone top bar does not show (message text while the
    /// search is closed, user, path) as one line with a clear-all button;
    /// returns the area below it.
    fn draw_filter_summary(&mut self, cx: &mut Cx2d, paint: &mut GitPaint, area: Rect) -> Rect {
        let filters = &self.state.filters;
        let mut parts = Vec::new();
        if !self.search_open && !filters.text.is_empty() {
            parts.push(tr("workspace.git.log.filters.text", &[("query", filters.text.clone())]));
        }
        if let Some(author) = &filters.author {
            parts.push(tr("workspace.git.log.filters.user", &[("name", author.clone())]));
        }
        if let Some(path) = &filters.path {
            parts.push(tr("workspace.git.log.filters.path", &[("name", path.clone())]));
        }
        if parts.is_empty() {
            return area;
        }
        let colors = palette();
        let height = 36.0_f64.min(area.size.y);
        let band = rect(area.pos.x, area.pos.y, area.size.x, height);
        paint.fill(cx, band, with_alpha(colors.accent, 0.08));
        paint.fill(cx, rect(band.pos.x, band.pos.y + height - 1.0, band.size.x, 1.0), colors.border);
        let center_y = band.pos.y + height / 2.0;
        paint.icon(cx, "list-filter", 14.0, colors.accent_bright, dvec2(band.pos.x + 18.0, center_y));
        let clear = rect(band.pos.x + band.size.x - 6.0 - 32.0, center_y - 16.0, 32.0, 32.0);
        if self.pressed.as_ref() == Some(&LogHit::ClearFilters) {
            paint.rounded(cx, clear, colors.interaction_highlight, 6.0, None);
        }
        paint.icon(cx, "x", 16.0, colors.muted, clear.center());
        let x = band.pos.x + 32.0;
        paint.text_centered(cx, Font::Regular, ui_px(13.0), colors.foreground, x, band.pos.y, height - 1.0, &parts.join(" · "), (clear.pos.x - 4.0 - x).max(0.0));
        self.hits.push(clear, LogHit::ClearFilters);
        rect(area.pos.x, area.pos.y + height, area.size.x, (area.size.y - height).max(0.0))
    }

    /// The top bar (Branch chip or the expanded search, user filter, search toggle,
    /// refresh), the filter summary and 56px rows.
    fn draw_phone_log(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, inputs: &mut LogInputs<'_>, area: Rect) {
        let colors = palette();
        let bar_h = 48.0;
        let bar = rect(area.pos.x, area.pos.y, area.size.x, bar_h);
        toolbar_band(cx, paint, bar, false);
        let center_y = bar.pos.y + (bar_h - 1.0) / 2.0;
        let mut right = self.refresh_button(cx, paint, true, bar.pos.x + bar.size.x - 6.0, center_y);
        let hovered = self.is_hovered(&LogHit::SearchToggle);
        let icon = if self.search_open { "x" } else { "search" };
        let toggle = toolbar_button(cx, paint, true, right, center_y, Some(icon), self.search_open, true, hovered);
        self.hits.push(toggle, LogHit::SearchToggle);
        right = toggle.pos.x - 4.0;
        let open = matches!(self.overlay.purpose(), Some(super::log_view::Purpose::Author));
        let user = toolbar_button(cx, paint, true, right, center_y, Some("user"), open || self.state.filters.author.is_some(), true, false);
        self.hits.push(user, LogHit::AuthorChip);
        right = user.pos.x - 4.0;
        let left = bar.pos.x + 8.0;
        if self.search_open {
            let focus = std::mem::take(&mut self.focus_search);
            let field = rect(left, center_y - 16.0, (right - 4.0 - left).max(0.0), 32.0);
            // The bar's × closes and clears; no second × inside the field.
            self.search_field(cx, paint, inputs.search, field, false, LogHit::SearchClear, focus);
        } else {
            let label = tr("workspace.git.log.filters.branch", &[("name", self.branch_label())]);
            let open = matches!(self.overlay.purpose(), Some(super::log_view::Purpose::Branch));
            let active = self.state.filters.revision.is_some();
            let pressed_clear = self.pressed.as_ref() == Some(&LogHit::ClearBranch);
            let (trigger, clear) = filter_chip(cx, paint, left, center_y, (right - 8.0 - left).max(0.0), &label, "git-branch", active, open, pressed_clear);
            // A tall tap target around the compact chip; its × registers last.
            let target = rect(trigger.pos.x, bar.pos.y + 4.0, trigger.size.x, bar_h - 9.0);
            self.hits.push(target, LogHit::BranchChip);
            if let Some(clear) = clear {
                let target = rect(clear.pos.x - 6.0, bar.pos.y + 4.0, clear.size.x + 10.0, bar_h - 9.0);
                self.hits.push(target, LogHit::ClearBranch);
            }
        }
        let below = rect(area.pos.x, area.pos.y + bar_h, area.size.x, (area.size.y - bar_h).max(0.0));
        let below = self.draw_filter_summary(cx, paint, below);
        let body = self.draw_banner(cx, paint, below);
        let row_h = PHONE_ROW_HEIGHT.max(paint.measure(cx, Font::Regular, ui_px(14.0), "Ag").y + paint.measure(cx, Font::Regular, ui_px(12.0), "Ag").y + 20.0);
        self.row_height = row_h;
        if self.list_state(cx, paint, body, row_h) {
            self.list.set(body, 0.0);
            return;
        }
        let count = self.state.commits.len();
        let footer = self.footer_height(true);
        let content = count as f64 * row_h + footer;
        self.list.set(body, content);
        let offset = self.list.offset;
        let now = now_ms();
        let head = self.state.refs.as_ref().and_then(|r| r.head_sha.clone());
        let title = ui_px(14.0);
        let meta = ui_px(12.0);
        let title_h = paint.measure(cx, Font::Regular, title, "Ag").y;
        let meta_h = paint.measure(cx, Font::Regular, meta, "Ag").y;
        paint.begin_clip(cx, body);
        let first = (offset / row_h).floor() as usize;
        for index in first..count {
            let top = body.pos.y + index as f64 * row_h - offset;
            if top > body.pos.y + body.size.y {
                break;
            }
            let band = rect(body.pos.x, top, body.size.x, row_h);
            // The whole row is the tap target.
            self.hits.push_clipped(band, body, LogHit::Row(index));
            let commit = &self.state.commits[index];
            let selected = self.state.selected.as_deref() == Some(commit.sha.as_str());
            let pressed = self.pressed.as_ref() == Some(&LogHit::Row(index));
            let background = if pressed || selected { colors.selected } else { colors.surface0 };
            if background != colors.surface0 {
                paint.fill(cx, band, background);
            }
            let is_head = head.as_deref() == Some(commit.sha.as_str());
            let graph_left = band.pos.x + 6.0;
            let lanes = draw_graph(cx, paint, self.state.rows.get(index), graph_left, top, row_h, PHONE_LANE_WIDTH, PHONE_MAX_LANES, is_head, background);
            let x0 = graph_left + lanes as f64 * PHONE_LANE_WIDTH + 8.0;
            let right = band.pos.x + band.size.x - 12.0;
            let block = title_h + 4.0 + meta_h;
            let line1 = top + (row_h - block) / 2.0;
            let line2 = line1 + title_h + 4.0;
            let mut x = x0;
            // The first decoration, then a count of the rest.
            if let Some(label) = commit.refs.first() {
                let max = ((right - x) * 0.45).min(140.0);
                x += chip(cx, paint, x, line1 + title_h / 2.0, label, max) + 4.0;
                if commit.refs.len() > 1 {
                    x += more_chip(cx, paint, x, line1 + title_h / 2.0, commit.refs.len() - 1) + 4.0;
                }
            }
            paint.text_centered(cx, Font::Regular, title, colors.foreground, x, line1, title_h, &commit.subject, (right - x).max(0.0));
            let date = commit.author_ms.map(|ms| relative_time(ms, now)).unwrap_or_default();
            let meta_line = format!("{} · {} · {}", commit.author_name, date, short(&commit.sha));
            paint.text_centered(cx, Font::Regular, meta, colors.muted, x0, line2, meta_h, &meta_line, (right - x0).max(0.0));
            paint.fill(cx, rect(x0, top + row_h - 1.0, band.size.x - (x0 - band.pos.x), 1.0), with_alpha(colors.border, 0.6));
        }
        if footer > 0.0 {
            let top = body.pos.y + count as f64 * row_h - offset;
            if top < body.pos.y + body.size.y {
                self.draw_footer(cx, paint, rect(body.pos.x, top, body.size.x, footer), body);
            }
        }
        paint.end_clip(cx);
        self.maybe_page(ui, count as f64 * row_h);
    }

    /// The phone commit screen: back + short hash, message and metadata, then 48px file rows.
    fn draw_phone_commit(&mut self, cx: &mut Cx2d, ui: &mut UiContext, paint: &mut GitPaint, area: Rect, sha: &str) {
        let colors = palette();
        let bar_h = 48.0;
        let bar = rect(area.pos.x, area.pos.y, area.size.x, bar_h);
        toolbar_band(cx, paint, bar, false);
        let center_y = bar.pos.y + (bar_h - 1.0) / 2.0;
        let hovered = self.is_hovered(&LogHit::Back);
        let back = toolbar_button(cx, paint, true, bar.pos.x + 6.0 + toolbar_button_size(true), center_y, Some("arrow-left"), false, true, hovered);
        self.hits.push(back, LogHit::Back);
        let title = short(sha);
        let x = back.pos.x + back.size.x + 8.0;
        paint.text_centered(cx, Font::Mono, ui_px(15.0), colors.foreground, x, bar.pos.y, bar_h - 1.0, &title, (bar.pos.x + bar.size.x - 12.0 - x).max(0.0));
        let body = rect(area.pos.x, area.pos.y + bar_h, area.size.x, (area.size.y - bar_h).max(0.0));
        self.state.ensure_detail(ui, sha);
        let Some(detail) = self.state.details.remove(sha) else {
            self.message(cx, paint, body, None, "", false, Vec::new(), true);
            return;
        };
        match &detail {
            Detail::Loading => self.message(cx, paint, body, None, "", false, Vec::new(), true),
            Detail::Failed(error) => {
                let retry = vec![(t("workspace.git.log.states.retry"), LogHit::DetailRetry(sha.to_owned()))];
                self.message(cx, paint, body, Some("circle-alert"), error, true, retry, false);
            }
            Detail::Missing => self.message(cx, paint, body, Some("circle-alert"), &t("workspace.git.log.details.missing"), false, Vec::new(), false),
            Detail::Ready(commit) => {
                let offset = self.sheet.offset;
                paint.begin_clip(cx, body);
                let mut y = self.commit_text(cx, paint, commit, body, body.pos.y + 16.0 - offset, true);
                y += 8.0;
                paint.fill(cx, rect(body.pos.x, y, body.size.x, 1.0), colors.border);
                y += 1.0;
                let header_h = 40.0;
                let header = tr("workspace.git.log.details.changedFiles", &[("count", commit.files.len().to_string())]);
                paint.text_centered(cx, Font::Medium, ui_px(13.0), colors.muted, body.pos.x + 16.0, y, header_h, &header, body.size.x - 32.0);
                y += header_h;
                self.list_files(commit);
                self.file_row_height = PHONE_FILE_ROW_HEIGHT;
                if commit.files.is_empty() {
                    paint.text_centered(cx, Font::Regular, ui_px(13.0), colors.muted, body.pos.x + 16.0, y, PHONE_FILE_ROW_HEIGHT, &t("workspace.git.log.details.noFiles"), body.size.x - 32.0);
                    y += PHONE_FILE_ROW_HEIGHT;
                }
                let start = y;
                let first = ((((body.pos.y - start) / PHONE_FILE_ROW_HEIGHT).floor()).max(0.0) as usize).min(commit.files.len());
                for (index, file) in commit.files.iter().enumerate().skip(first) {
                    let top = start + index as f64 * PHONE_FILE_ROW_HEIGHT;
                    if top > body.pos.y + body.size.y {
                        break;
                    }
                    let band = rect(body.pos.x, top, body.size.x, PHONE_FILE_ROW_HEIGHT);
                    let path = text(file, "path");
                    let hit = LogHit::File(path.to_owned());
                    self.hits.push_clipped(band, body, hit.clone());
                    if self.pressed.as_ref() == Some(&hit) {
                        paint.fill(cx, band, colors.selected);
                    }
                    self.file_line(cx, paint, band, file, band.pos.x + 16.0);
                }
                y = start + commit.files.len() as f64 * PHONE_FILE_ROW_HEIGHT + 16.0;
                paint.end_clip(cx);
                self.sheet.set(body, y + offset - body.pos.y);
            }
        }
        self.state.details.insert(sha.to_owned(), detail);
    }
}
