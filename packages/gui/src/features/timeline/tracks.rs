//! Port of `chrome/composer_tracks.dart` + `stream_chrome.dart` subagent rows:
//! the floating track bar over the stream bottom (task progress, subagents,
//! workspace diff stat) and its panels (popover above the pill on wide
//! layouts, bottom sheet on compact), with subagent open / archive / detach /
//! archive-finished actions.

use super::canvas::*;
use super::i18n::{t, ta};
use super::json::*;
use super::paint::Draw;
use crate::common::paint::Painter;
use crate::common::provider_icon::ProviderIcon;
use crate::common::widgets::{STATUS_BUCKET_ORDER, StatusBucket, derive_status_bucket, paint_status_badge, status_dot_color, paint_status_ring};
use makepad_widgets::*;
use serde_json::Value;

/// composer/pill-styles.ts geometry.
pub const PILL_MIN_HEIGHT: f64 = 32.0;

pub fn pill_clearance(compact: bool) -> f64 {
    if compact { 8.0 } else { 10.0 }
}

/// resolveComposerTrackTailClearance.
pub fn tail_clearance(compact: bool) -> f64 {
    (if compact { 24.0 } else { 20.0 }) + PILL_MIN_HEIGHT + pill_clearance(compact)
}

/// resolveComposerTrackControlClearance.
pub fn control_clearance(compact: bool) -> f64 {
    pill_clearance(compact) * 2.0 + PILL_MIN_HEIGHT
}

/// subagents/select.ts SubagentRow.
#[derive(Clone, Debug)]
pub struct SubagentRow {
    pub paseo: bool,
    pub id: String,
    pub server_id: String,
    pub provider: String,
    pub status: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub subtitle: Option<String>,
    pub turn_open: bool,
    pub created_ms: i64,
    pub workspace_id: Option<String>,
    pub cwd: Option<String>,
    pub parent_agent_id: Option<String>,
    pub agent: Value,
}

/// track-presentation.ts resolveRowLabel.
pub fn row_label(title: Option<&str>) -> Option<String> {
    let normalized = title.unwrap_or("").trim();
    if normalized.is_empty() || normalized.eq_ignore_ascii_case("new agent") {
        return None;
    }
    Some(normalized.to_owned())
}

impl SubagentRow {
    pub fn key(&self) -> String {
        if self.paseo { format!("paseo:{}:{}", self.server_id, self.id) } else { format!("provider:{}", self.id) }
    }

    /// archive-finished.ts isFinishedSubagent.
    pub fn is_finished(&self) -> bool {
        let s = self.status.as_deref();
        if self.paseo { matches!(s, Some("idle" | "error")) } else { matches!(s, Some("completed" | "failed" | "canceled")) }
    }

    fn presentation_status(&self) -> Option<&str> {
        if self.paseo {
            if self.turn_open {
                return Some("running");
            }
            return if self.status.as_deref() == Some("running") { Some("idle") } else { self.status.as_deref() };
        }
        match self.status.as_deref() {
            Some("running") => Some("running"),
            Some("failed") => Some("error"),
            _ => Some("idle"),
        }
    }

    pub fn bucket(&self) -> StatusBucket {
        derive_status_bucket(self.presentation_status(), 0, false, None)
    }

    pub fn label(&self) -> Option<String> {
        row_label(self.description.as_deref()).or_else(|| row_label(self.title.as_deref()))
    }

    pub fn row_subtitle(&self) -> Option<String> {
        let described = row_label(self.description.as_deref()).is_some();
        if !self.paseo {
            return row_label(self.subtitle.as_deref()).or_else(|| if described { row_label(self.title.as_deref()) } else { None });
        }
        if described { row_label(self.title.as_deref()) } else { None }
    }
}

/// use-archive-finished ArchiveFinishedStatus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ArchiveFinished {
    #[default]
    Idle,
    Archiving { completed: usize, total: usize },
    Failed { failed: usize, total: usize },
}

/// panels/agent-facts-line.ts buildAgentFactsLine: model · thinking · mode ·
/// context usage, each resolved as the composer's controls resolve it, over
/// the panel's providers snapshot.
pub fn facts_line(providers: &[Value], agent: &Value) -> Option<String> {
    use crate::features::composer::controls_model::{format_agent_mode_label, format_thinking_option_label, model_label};
    let runtime = get(agent, "runtimeInfo");
    let runtime_model = str_of(get(runtime, "model")).trim().to_owned();
    let configured = str_of(get(agent, "model")).trim().to_owned();
    if runtime_model.is_empty() && configured.is_empty() {
        return None;
    }
    let provider = str_of(get(agent, "provider"));
    let model = super::composer_bridge::model_entry(providers, provider, &runtime_model)
        .or_else(|| super::composer_bridge::model_entry(providers, provider, &configured));
    let mut parts = vec![match &model {
        Some(m) => model_label(m),
        None if !runtime_model.is_empty() => runtime_model.clone(),
        None => configured.clone(),
    }];
    let thinking_id = get(runtime, "thinkingOptionId").as_str().or_else(|| get(agent, "thinkingOptionId").as_str()).unwrap_or("").to_owned();
    let options = model.as_ref().map(|m| list(get(m, "thinkingOptions")).to_vec()).unwrap_or_default();
    let thinking = options.iter().find(|o| get(o, "id") == thinking_id.as_str()).or_else(|| {
        if thinking_id.is_empty() { options.iter().find(|o| is_true(get(o, "isDefault"))) } else { None }
    });
    match thinking {
        Some(option) => parts.push(format_thinking_option_label(option, &t("agentControls.thinking.extraHigh"))),
        None if !thinking_id.is_empty() => parts.push(thinking_id),
        None => {}
    }
    let modes = list(get(agent, "availableModes"));
    if let Some(mode) = modes.iter().find(|m| get(m, "id") == get(agent, "currentModeId")).or(modes.first()) {
        parts.push(format_agent_mode_label(mode));
    }
    let usage = get(agent, "lastUsage");
    if let (Some(used), Some(max)) = (get(usage, "contextWindowUsedTokens").as_f64(), get(usage, "contextWindowMaxTokens").as_f64())
        && used >= 0.0
        && max > 0.0
    {
        let tokens = |v: f64| {
            if v >= 1_000_000.0 {
                format!("{}m", (v / 1_000_000.0).round())
            } else if v >= 1000.0 {
                format!("{}k", (v / 1000.0).round())
            } else {
                format!("{}", v.round())
            }
        };
        let pct = (used / max * 100.0).clamp(0.0, 100.0).round();
        parts.push(format!("{} / {} tokens ({pct}%)", tokens(used), tokens(max)));
    }
    Some(parts.join(" · "))
}

/// Which track panel is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackPanel {
    Tasks,
    Subagents,
}

pub struct TrackModel<'a> {
    pub tasks: &'a [Value],
    pub rows: &'a [SubagentRow],
    pub archive: ArchiveFinished,
    pub additions: i64,
    pub deletions: i64,
    pub can_archive_finished: bool,
    pub can_detach: bool,
    pub host_id: &'a str,
    pub providers: &'a [Value],
    pub host_label: &'a dyn Fn(&str) -> String,
}

pub fn visible(model: &TrackModel) -> bool {
    model.additions > 0 || model.deletions > 0 || !model.rows.is_empty() || !model.tasks.is_empty() || model.archive != ArchiveFinished::Idle
}

/// track-presentation.ts buildSubagentPillPresentation.
fn subagent_segments(rows: &[SubagentRow]) -> Vec<(Option<StatusBucket>, String)> {
    let buckets: Vec<StatusBucket> = rows.iter().map(SubagentRow::bucket).collect();
    let mut segments = Vec::new();
    for bucket in STATUS_BUCKET_ORDER {
        if bucket == StatusBucket::Done {
            continue;
        }
        let count = buckets.iter().filter(|b| **b == bucket).count();
        if count == 0 {
            continue;
        }
        let c = count.to_string();
        let text = match bucket {
            StatusBucket::Running => ta("subagents.pillLabelWorking", &[("count", &c)]),
            StatusBucket::Failed => ta("subagents.pillLabelFailed", &[("count", &c)]),
            StatusBucket::NeedsInput => {
                if count == 1 { t("subagents.pillLabelNeedsInputOne") } else { ta("subagents.pillLabelNeedsInputMany", &[("count", &c)]) }
            }
            _ => ta("subagents.pillLabelReadyToReview", &[("count", &c)]),
        };
        segments.push((Some(bucket), text));
    }
    if segments.is_empty() {
        let label = if rows.len() == 1 { t("subagents.pillLabelOne") } else { ta("subagents.pillLabelMany", &[("count", &rows.len().to_string())]) };
        segments.push((None, label));
    }
    segments
}

#[allow(clippy::too_many_arguments)]
fn pill(cx: &mut Cx2d, d: &mut Draw, painter: &mut Painter, x: f64, y: f64, max_w: f64, segments: &[(Option<StatusBucket>, String)], open: bool, id: &str) -> f64 {
    let p = d.look.palette;
    let size = d.look.ui(12.0);
    let mut widths = Vec::new();
    let mut total = 24.0;
    for (i, (bucket, text)) in segments.iter().enumerate() {
        let w = d.kit.measure(cx, Font::Regular, size, 1.3, text, None).x + if bucket.is_some() { 14.0 } else { 0.0 };
        widths.push(w);
        total += w + if i > 0 { 16.0 } else { 0.0 };
    }
    let w = total.min(max_w);
    let hovered = d.state.hover_hit.as_deref() == Some(id);
    let r = rect_xywh(x, y, w, PILL_MIN_HEIGHT);
    d.kit.bordered(cx, r, if hovered || open { p.surface2 } else { p.surface1 }, 16.0, p.border_accent, 1.0);
    cx.begin_turtle(Walk { abs_pos: Some(r.pos), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(PILL_MIN_HEIGHT) }, Layout::default());
    let mut sx = x + 12.0;
    for ((bucket, text), sw) in segments.iter().zip(&widths) {
        if let Some(bucket) = bucket {
            let center = dvec2(sx + 3.0, y + PILL_MIN_HEIGHT * 0.5);
            if *bucket == StatusBucket::Running {
                paint_status_ring(painter, cx, center, &p, None);
                d.animating = true;
            } else if let Some(color) = status_dot_color(&p, *bucket, true) {
                d.kit.rect(cx, rect_xywh(center.x - 3.0, center.y - 3.0, 6.0, 6.0), color, 3.0);
            }
            sx += 14.0;
        }
        let s = d.kit.measure(cx, Font::Regular, size, 1.3, text, None);
        d.kit.label_line(cx, Font::Regular, size, 1.3, if open { p.foreground } else { p.muted }, dvec2(sx, y + (PILL_MIN_HEIGHT - s.y) * 0.5), text, (x + w - 12.0 - sx).max(8.0));
        sx += sw - if bucket.is_some() { 14.0 } else { 0.0 } + 16.0;
    }
    cx.end_turtle();
    d.canvas.add_hit(r, HitTarget::Chrome(id.to_owned()));
    w
}

/// The track bar resting `pill_clearance` above `bottom`; returns pill rects.
#[allow(clippy::too_many_arguments)]
pub fn draw_bar(cx: &mut Cx2d, d: &mut Draw, painter: &mut Painter, model: &TrackModel, open: Option<TrackPanel>, x: f64, bottom: f64, w: f64) -> Vec<(TrackPanel, Rect)> {
    let p = d.look.palette;
    let outer = (w - 32.0).min(d.look.typo.content_max_width);
    let bx = x + (w - outer) * 0.5;
    let y = bottom - pill_clearance(d.look.compact) - PILL_MIN_HEIGHT;
    let mut cursor = bx;
    let mut out = Vec::new();
    let diff = model.additions > 0 || model.deletions > 0;
    let diff_w = if diff {
        let a = format!("+{}", model.additions);
        let r = format!("-{}", model.deletions);
        let size = d.look.ui(12.0);
        d.kit.measure(cx, Font::Mono, size, 1.3, &a, None).x + d.kit.measure(cx, Font::Mono, size, 1.3, &r, None).x + 6.0 + 24.0
    } else {
        0.0
    };
    let pills = (!model.tasks.is_empty()) as usize + (!model.rows.is_empty() || model.archive != ArchiveFinished::Idle) as usize;
    let avail = (outer - diff_w - 4.0 * pills as f64).max(80.0);
    let each = if pills > 0 { avail / pills as f64 } else { avail };
    if !model.tasks.is_empty() {
        let completed = model.tasks.iter().filter(|task| is_true(get(task, "completed")) || get(task, "status") == "completed").count();
        let label = ta("message.todo.tasksProgress", &[("completed", &completed.to_string()), ("total", &model.tasks.len().to_string())]);
        let pw = pill(cx, d, painter, cursor, y, each, &[(None, label)], open == Some(TrackPanel::Tasks), "track:tasks");
        out.push((TrackPanel::Tasks, rect_xywh(cursor, y, pw, PILL_MIN_HEIGHT)));
        cursor += pw + 4.0;
    }
    if !model.rows.is_empty() || model.archive != ArchiveFinished::Idle {
        let segments = subagent_segments(model.rows);
        let pw = pill(cx, d, painter, cursor, y, each, &segments, open == Some(TrackPanel::Subagents), "track:subagents");
        out.push((TrackPanel::Subagents, rect_xywh(cursor, y, pw, PILL_MIN_HEIGHT)));
        cursor += pw + 4.0;
    }
    if diff {
        let r = rect_xywh(cursor, y, diff_w, PILL_MIN_HEIGHT);
        let hovered = d.state.hover_hit.as_deref() == Some("track:diff");
        d.kit.bordered(cx, r, if hovered { p.surface2 } else { p.surface1 }, 16.0, p.border_accent, 1.0);
        let size = d.look.ui(12.0);
        let a = format!("+{}", model.additions);
        let s = d.kit.label(cx, Font::Mono, size, 1.3, p.added, dvec2(cursor + 12.0, y + 8.0), &a, None);
        d.kit.label(cx, Font::Mono, size, 1.3, p.removed, dvec2(cursor + 18.0 + s.x, y + 8.0), &format!("-{}", model.deletions), None);
        d.canvas.add_hit(r, HitTarget::Chrome("track:diff".into()));
    }
    out
}

/// Panel rows (ComposerTrackRow): returns the content height.
#[allow(clippy::too_many_arguments)]
fn panel_rows(cx: &mut Cx2d, d: &mut Draw, painter: &mut Painter, icon: &mut DrawIcon, model: &TrackModel, panel: TrackPanel, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let compact = d.look.compact;
    let row_h = if compact { 40.0 } else { 32.0 };
    let base = d.look.ui(14.0);
    let small = d.look.ui(12.0);
    let mut cy = y;
    match panel {
        TrackPanel::Tasks => {
            for task in model.tasks {
                let r = rect_xywh(x + 4.0, cy, w - 8.0, row_h);
                super::rows::task_rows(cx, d, std::slice::from_ref(task), r.pos.x + 8.0, cy + (row_h - base * 1.4) * 0.5, r.size.x - 16.0);
                cy += row_h;
            }
        }
        TrackPanel::Subagents => {
            let finished = model.rows.iter().filter(|r| r.is_finished()).count();
            if model.can_archive_finished && (finished > 0 || model.archive != ArchiveFinished::Idle) {
                let archiving = matches!(model.archive, ArchiveFinished::Archiving { .. });
                let r = rect_xywh(x + 4.0, cy, w - 8.0, row_h);
                let hovered = !archiving && d.state.hover_hit.as_deref() == Some("archive-finished");
                if hovered {
                    d.kit.rect(cx, r, p.surface2, 6.0);
                }
                d.kit.icon(cx, "archive", rect_xywh(r.pos.x + 8.0, cy, 14.0, row_h), 14.0, if hovered { p.foreground } else { p.muted });
                let label = t("subagents.archiveFinishedAction");
                let s = d.kit.label_line(cx, Font::Regular, base, 1.3, p.foreground, dvec2(r.pos.x + 30.0, cy + (row_h - base * 1.3) * 0.5), &label, (r.size.x - 120.0).max(20.0));
                let trailing = match model.archive {
                    ArchiveFinished::Archiving { completed, total } => Some(format!("{completed}/{total}")),
                    ArchiveFinished::Failed { failed, total } => Some(ta("subagents.archiveFinishedRetry", &[("failed", &failed.to_string()), ("total", &total.to_string())])),
                    ArchiveFinished::Idle => None,
                };
                if let Some(trailing) = trailing {
                    d.kit.label(cx, Font::Regular, small, 1.3, p.muted, dvec2(r.pos.x + 38.0 + s.x, cy + (row_h - small * 1.3) * 0.5), &trailing, None);
                }
                if !archiving {
                    d.canvas.add_hit(r, HitTarget::Chrome("archive-finished".into()));
                }
                cy += row_h;
                if !model.rows.is_empty() {
                    d.kit.rect(cx, rect_xywh(x, cy + 4.0, w, 1.0), p.border_accent, 0.0);
                    cy += 9.0;
                }
            }
            for row in model.rows {
                let key = row.key();
                let open_id = format!("subagent-open:{key}");
                let hovered_row = d.state.hover_hit.as_deref().is_some_and(|h| h.ends_with(&key));
                let facts: Vec<String> = if row.paseo {
                    let mut lines = Vec::new();
                    if row.server_id != model.host_id {
                        lines.push((model.host_label)(&row.server_id));
                    }
                    if let Some(f) = facts_line(model.providers, &row.agent) {
                        lines.push(f);
                    }
                    lines
                } else {
                    row.row_subtitle().into_iter().collect()
                };
                let h = row_h.max(base * 1.4 + facts.len() as f64 * small * 1.4 + if compact { 16.0 } else { 8.0 });
                let r = rect_xywh(x + 4.0, cy, w - 8.0, h);
                if hovered_row {
                    d.kit.rect(cx, r, p.surface2, 6.0);
                }
                d.canvas.add_hit(r, HitTarget::Chrome(open_id));
                let icon_rect = rect_xywh(r.pos.x + 8.0, cy + (h - 14.0) * 0.5, 14.0, 14.0);
                if !d.dry() {
                    let svg = super::composer_bridge::provider_svg(model.providers, &row.provider);
                    ProviderIcon::paint(painter, icon, cx, icon_rect, &row.provider, svg.as_deref(), p.muted);
                    if paint_status_badge(painter, cx, icon_rect, Some(row.bucket()), &p, if hovered_row { p.surface2 } else { p.surface1 }) {
                        d.animating = true;
                    }
                }
                let actions_visible = compact || d.look.mobile || hovered_row;
                let actions_w = if row.paseo { if model.can_detach { 64.0 } else { 32.0 } } else { 0.0 };
                let text_x = r.pos.x + 30.0;
                let text_w = (r.size.x - 38.0 - actions_w).max(20.0);
                let label = row.label().unwrap_or_else(|| t("common.states.loading"));
                cx.begin_turtle(Walk { abs_pos: Some(dvec2(text_x, cy)), margin: Margin::default(), width: Size::Fixed(text_w), height: Size::Fixed(h) }, Layout::default());
                let mut ty = cy + (h - (base * 1.4 + facts.len() as f64 * small * 1.4)) * 0.5;
                d.kit.label_line(cx, Font::Regular, base, 1.4, p.foreground, dvec2(text_x, ty), &label, text_w);
                ty += base * 1.4;
                for line in &facts {
                    d.kit.label_line(cx, Font::Regular, small, 1.4, p.muted, dvec2(text_x, ty), line, text_w);
                    ty += small * 1.4;
                }
                cx.end_turtle();
                if row.paseo && actions_visible {
                    let mut ax = r.pos.x + r.size.x - 8.0 - 28.0;
                    let archive = rect_xywh(ax, cy + (h - 28.0) * 0.5, 28.0, 28.0);
                    let archive_id = format!("subagent-archive:{key}");
                    let over = d.state.hover_hit.as_deref() == Some(archive_id.as_str());
                    if over {
                        d.kit.rect(cx, archive, p.surface3, 6.0);
                    }
                    d.kit.icon(cx, "archive", archive, 16.0, if over { p.foreground } else { p.muted });
                    d.canvas.add_hit(archive, HitTarget::Chrome(archive_id));
                    if model.can_detach {
                        ax -= 32.0;
                        let detach = rect_xywh(ax, archive.pos.y, 28.0, 28.0);
                        let detach_id = format!("subagent-detach:{key}");
                        let over = d.state.hover_hit.as_deref() == Some(detach_id.as_str());
                        if over {
                            d.kit.rect(cx, detach, p.surface3, 6.0);
                        }
                        d.kit.icon(cx, "unlink", detach, 16.0, if over { p.foreground } else { p.muted });
                        d.canvas.add_hit(detach, HitTarget::Chrome(detach_id));
                    }
                }
                cy += h;
            }
        }
    }
    cy - y
}

/// Opens above the pill on wide layouts (12px offset, 280–620px wide, at most
/// 440 tall) and as a bottom sheet on compact layouts.
#[allow(clippy::too_many_arguments)]
pub fn draw_panel(cx: &mut Cx2d, d: &mut Draw, painter: &mut Painter, icon: &mut DrawIcon, model: &TrackModel, panel: TrackPanel, anchor: Rect, area: Rect) {
    let p = d.look.palette;
    let title = t(match panel {
        TrackPanel::Tasks => "message.todo.title",
        TrackPanel::Subagents => "subagents.title",
    });
    // Scrim closes the panel.
    d.canvas.add_hit(area, HitTarget::Chrome("track-close".into()));
    if d.look.compact {
        d.kit.rect(cx, area, rgba(0x73000000), 0.0);
        let w = area.size.x;
        let content = d.measure(|d| panel_rows(cx, d, painter, icon, model, panel, area.pos.x, 0.0, w));
        let max_h = area.size.y * 0.8;
        let head = 24.0 + 16.0 + d.look.ui(14.0) * 1.3;
        let h = (head + content + 4.0).min(max_h);
        let top = area.pos.y + area.size.y - h;
        let sheet = rect_xywh(area.pos.x, top, w, h + 12.0);
        d.kit.rect_full(cx, sheet, p.surface0, vec4(12.0, 12.0, 0.0, 0.0), p.border, 1.0);
        d.canvas.add_hit(sheet, HitTarget::Chrome("track-panel".into()));
        d.kit.rect(cx, rect_xywh(area.pos.x + w * 0.5 - 18.0, top + 10.0, 36.0, 4.0), p.surface2, 2.0);
        d.kit.label_line(cx, Font::Medium, d.look.ui(14.0), 1.3, p.foreground, dvec2(area.pos.x + 12.0, top + 28.0), &title, w - 24.0);
        let body = rect_xywh(area.pos.x, top + head, w, h - head);
        let key = "track-panel:y".to_owned();
        let max_y = (content + 4.0 - body.size.y).max(0.0);
        d.state.extent_y.insert(key.clone(), max_y);
        let sy = d.state.scroll_y.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_y);
        cx.begin_turtle(Walk { abs_pos: Some(body.pos), margin: Margin::default(), width: Size::Fixed(body.size.x), height: Size::Fixed(body.size.y.max(1.0)) }, Layout::default());
        d.canvas.push_clip(body);
        panel_rows(cx, d, painter, icon, model, panel, area.pos.x, body.pos.y - sy, w);
        d.canvas.pop_clip();
        cx.end_turtle();
        if max_y > 0.0 {
            d.canvas.add_hit(body, HitTarget::ScrollY(key));
        }
        return;
    }
    let max_w = (area.size.x - 16.0).clamp(0.0, 620.0);
    let w = 280.0f64.min(max_w).max(max_w.min(420.0));
    let content = d.measure(|d| panel_rows(cx, d, painter, icon, model, panel, 0.0, 0.0, w));
    let h = (content + 8.0).min((area.size.y - 16.0).clamp(0.0, 440.0));
    let mut y = anchor.pos.y - 12.0 - h;
    if y < area.pos.y + 8.0 {
        y = anchor.pos.y + anchor.size.y + 12.0;
    }
    let x = anchor.pos.x.clamp(area.pos.x + 8.0, (area.pos.x + area.size.x - w - 8.0).max(area.pos.x + 8.0));
    let r = rect_xywh(x, y, w, h);
    let shadow = if d.look.dark() { rgba(0x33000000) } else { rgba(0x0a000000) };
    d.kit.rect(cx, r.translate(dvec2(0.0, 4.0)), shadow, 8.0);
    d.kit.bordered(cx, r, p.surface1, 8.0, p.border_accent, 1.0);
    d.canvas.add_hit(r, HitTarget::Chrome("track-panel".into()));
    let key = "track-panel:y".to_owned();
    let max_y = (content + 8.0 - h).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let sy = d.state.scroll_y.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_y);
    cx.begin_turtle(Walk { abs_pos: Some(r.pos), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(h.max(1.0)) }, Layout::default());
    d.canvas.push_clip(r);
    panel_rows(cx, d, painter, icon, model, panel, x, y + 4.0 - sy, w);
    d.canvas.pop_clip();
    cx.end_turtle();
    if max_y > 0.0 {
        d.canvas.add_hit(r, HitTarget::ScrollY(key));
    }
}
