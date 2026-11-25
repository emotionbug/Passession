//! Stream row painters (timeline_item.dart TimelineItem + agent_panel.dart
//! turn footers): user bubbles, assistant Markdown (paced inline/worker
//! snapshots), Thinking and tool badges with inline details, overview tool
//! groups, speak messages, plan cards, task activity badges, compaction
//! markers, notifications/errors, unknown items as structured details, the
//! history-start slot, turn footers and permission cards.

use super::canvas::*;
use super::details::draw_details;
use super::format;
use super::i18n::{t, ta};
use super::images::{self, ImageState};
use super::indicator::Indicator;
use super::json::*;
use super::layout::{StreamRow, TurnTiming, parse_time};
use super::markdown::{self, FindMark, MdOpts};
use super::paint::{Draw, Look};
use super::permission::{self, AnswerField};
use super::tools::{
    self, ToolGlyph, build_tool_call_presentation, detail_blocks, detail_is_full_bleed,
};
use crate::common::transition::tokens;
use crate::common::widgets::{InlineMetrics, RowMetrics};
use makepad_widgets::*;
use paseo_gui_core::CoreHandle;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::rc::Rc;

pub const STREAM_METADATA_SIZE: f64 = 13.0;

/// Chat find's current match inside one row.
#[derive(Clone, Debug, PartialEq)]
pub struct FindTarget {
    pub query: String,
    pub row_seq: i64,
    pub occurrence: usize,
}

/// The running turn footer's facts (agent_panel.dart `_turnFooter(null)`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveFooter {
    pub live: bool,
    pub started_ms: Option<i64>,
    pub pending_delivery: bool,
}

/// Everything rows share for one frame.
pub struct RowContext {
    pub look: Look,
    pub host_id: String,
    pub agent_id: String,
    pub workspace_id: String,
    pub cwd: Option<String>,
    pub is_subagent: bool,
    pub capabilities: Value,
    pub expanded_tools: bool,
    pub auto_expand_reasoning: bool,
    pub core: CoreHandle,
    pub find: Option<FindTarget>,
    /// Host agents' titles (Paseo tool target agent titles).
    pub agent_titles: HashMap<String, String>,
    pub footer: LiveFooter,
    pub can_fork_cursor: bool,
    pub epoch: Option<String>,
}

impl RowContext {
    /// Compare only the context that can affect this row's drawing or hits.
    /// `core` is a command dispatcher, not a rendered value; resource arrivals
    /// and local interaction state are invalidated by the owning row widget.
    pub fn same_for(&self, other: &Self, item: &RowItem) -> bool {
        self.look == other.look
            && ContextInputs::from(self).same_for(&ContextInputs::from(other), item)
    }
}

// The production comparison runs on this borrowed projection so it can be
// tested without starting a core runtime (CoreHandle has no test constructor).
// `From` destructures RowContext exhaustively: a new field fails to compile
// until it is classified here.
#[derive(Clone, Copy)]
struct ContextInputs<'a> {
    host_id: &'a str,
    agent_id: &'a str,
    workspace_id: &'a str,
    cwd: Option<&'a str>,
    is_subagent: bool,
    capabilities: &'a Value,
    expanded_tools: bool,
    auto_expand_reasoning: bool,
    find: Option<&'a FindTarget>,
    agent_titles: &'a HashMap<String, String>,
    footer: &'a LiveFooter,
    can_fork_cursor: bool,
    epoch: Option<&'a str>,
}

impl<'a> From<&'a RowContext> for ContextInputs<'a> {
    fn from(ctx: &'a RowContext) -> Self {
        let RowContext {
            // Compared by RowContext::same_for.
            look: _,
            // The fixed runtime's command dispatcher, not a rendered value.
            core: _,
            host_id,
            agent_id,
            workspace_id,
            cwd,
            is_subagent,
            capabilities,
            expanded_tools,
            auto_expand_reasoning,
            find,
            agent_titles,
            footer,
            can_fork_cursor,
            epoch,
        } = ctx;
        Self {
            host_id,
            agent_id,
            workspace_id,
            cwd: cwd.as_deref(),
            is_subagent: *is_subagent,
            capabilities,
            expanded_tools: *expanded_tools,
            auto_expand_reasoning: *auto_expand_reasoning,
            find: find.as_ref(),
            agent_titles,
            footer,
            can_fork_cursor: *can_fork_cursor,
            epoch: epoch.as_deref(),
        }
    }
}

impl ContextInputs<'_> {
    fn same_for(&self, other: &Self, item: &RowItem) -> bool {
        if matches!(item.kind, RowKind::HistoryStart { .. } | RowKind::Inset(_)) {
            return true;
        }
        // Conservative identity boundary, including identities used by actions.
        if self.host_id != other.host_id
            || self.agent_id != other.agent_id
            || self.workspace_id != other.workspace_id
        {
            return false;
        }
        match &item.kind {
            RowKind::Footer { host: None, .. } => {
                self.is_subagent == other.is_subagent && self.footer == other.footer
            }
            RowKind::Footer { host: Some(_), .. } => {
                self.is_subagent == other.is_subagent
                    && self.can_fork_cursor == other.can_fork_cursor
                    && self.epoch == other.epoch
            }
            RowKind::Permission { .. } => self.cwd == other.cwd,
            RowKind::Entry { rows, index, .. } => {
                let row = &rows[*index];
                let seq = int_or(get(&row.entry, "seqStart"), i64::MIN);
                self.cwd == other.cwd
                    && effective_find(self.find, seq) == effective_find(other.find, seq)
                    && match row.kind() {
                        "user_message" => {
                            self.is_subagent == other.is_subagent
                                && self.capabilities == other.capabilities
                        }
                        "reasoning" => self.auto_expand_reasoning == other.auto_expand_reasoning,
                        "tool_call" => {
                            self.expanded_tools == other.expanded_tools
                                && self.agent_titles == other.agent_titles
                        }
                        _ => true,
                    }
            }
            RowKind::HistoryStart { .. } | RowKind::Inset(_) => true,
        }
    }
}

fn effective_find(find: Option<&FindTarget>, seq: i64) -> Option<(&str, usize)> {
    find.filter(|target| target.row_seq == seq)
        .map(|target| (target.query.as_str(), target.occurrence))
}

#[derive(Clone)]
pub enum RowKind {
    HistoryStart {
        loading: bool,
    },
    Entry {
        rows: Rc<Vec<StreamRow>>,
        index: usize,
        previous_tasks: Option<Rc<Value>>,
        footer_below: bool,
        streaming: bool,
        live: bool,
        gap_below: f64,
    },
    Footer {
        rows: Rc<Vec<StreamRow>>,
        host: Option<usize>,
        timing: Option<TurnTiming>,
    },
    Permission {
        key: String,
        request: Rc<Value>,
    },
    Inset(f64),
}

#[derive(Clone)]
pub struct RowItem {
    pub id: String,
    pub kind: RowKind,
}

impl RowItem {
    /// The projected row's start sequence (find, outline, reading anchor).
    pub fn seq(&self) -> Option<i64> {
        match &self.kind {
            RowKind::Entry { rows, index, .. } => {
                let entry = &rows[*index].entry;
                is_int(get(entry, "seqStart")).then(|| int_of(get(entry, "seqStart")))
            }
            _ => None,
        }
    }

    /// Exact rendered-input equality, independent of unrelated rows in the
    /// shared timeline snapshot. Rc identity is only an immutable fast path.
    pub fn same_content(&self, other: &Self) -> bool {
        if self.id != other.id {
            return false;
        }
        match (&self.kind, &other.kind) {
            (RowKind::HistoryStart { loading: a }, RowKind::HistoryStart { loading: b }) => a == b,
            (RowKind::Inset(a), RowKind::Inset(b)) => a == b,
            (
                RowKind::Permission {
                    key: ak,
                    request: a,
                },
                RowKind::Permission {
                    key: bk,
                    request: b,
                },
            ) => ak == bk && same_value(a, b),
            (
                RowKind::Entry {
                    rows: a,
                    index: ai,
                    previous_tasks: ap,
                    footer_below: af,
                    streaming: ast,
                    live: al,
                    gap_below: ag,
                },
                RowKind::Entry {
                    rows: b,
                    index: bi,
                    previous_tasks: bp,
                    footer_below: bf,
                    streaming: bst,
                    live: bl,
                    gap_below: bg,
                },
            ) => {
                af == bf
                    && ast == bst
                    && al == bl
                    && ag == bg
                    && match (ap, bp) {
                        (Some(a), Some(b)) => same_value(a, b),
                        (None, None) => true,
                        _ => false,
                    }
                    && ((ai == bi && Rc::ptr_eq(a, b)) || same_stream_row(&a[*ai], &b[*bi]))
            }
            (
                RowKind::Footer {
                    rows: a,
                    host: ah,
                    timing: at,
                },
                RowKind::Footer {
                    rows: b,
                    host: bh,
                    timing: bt,
                },
            ) => {
                at == bt
                    && match (ah, bh) {
                        (None, None) => true,
                        (Some(ah), Some(bh)) => {
                            (ah == bh && Rc::ptr_eq(a, b))
                                || (same_value(&a[*ah].entry, &b[*bh].entry)
                                    && same_response(a, *ah, b, *bh))
                        }
                        _ => false,
                    }
            }
            _ => false,
        }
    }
}

fn same_value(a: &Rc<Value>, b: &Rc<Value>) -> bool {
    Rc::ptr_eq(a, b) || a == b
}

fn same_stream_row(a: &StreamRow, b: &StreamRow) -> bool {
    same_value(&a.entry, &b.entry)
        && match (&a.group, &b.group) {
            (Some(a), Some(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b))
            }
            (None, None) => true,
            _ => false,
        }
        && match (&a.fold, &b.fold) {
            (Some(a), Some(b)) => {
                a.key == b.key
                    && a.open == b.open
                    && a.entries.len() == b.entries.len()
                    && a.entries
                        .iter()
                        .zip(&b.entries)
                        .all(|(a, b)| same_value(a, b))
            }
            (None, None) => true,
            _ => false,
        }
}

fn same_response(a: &[StreamRow], ah: usize, b: &[StreamRow], bh: usize) -> bool {
    let mut a = response_sources(a, ah);
    let mut b = response_sources(b, bh);
    loop {
        match (a.next(), b.next()) {
            (Some(a), Some(b)) if same_value(a, b) => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Same response boundary and assistant sources as ResponseCopyCache::text,
/// without allocating clipboard text during every cache-validity check.
fn response_sources(rows: &[StreamRow], host: usize) -> impl Iterator<Item = &Rc<Value>> {
    let mut first = host;
    while first > 0
        && super::layout::continues_response(Some(&rows[first - 1].entry), Some(&rows[first].entry))
    {
        first -= 1;
    }
    super::layout::response_messages(&rows[first..=host])
}

fn glyph_icon(glyph: ToolGlyph) -> &'static str {
    match glyph {
        ToolGlyph::SquareTerminal => "square-terminal",
        ToolGlyph::Eye => "eye",
        ToolGlyph::Pencil => "pencil",
        ToolGlyph::Search => "search",
        ToolGlyph::Bot => "bot",
        ToolGlyph::Sparkles => "sparkles",
        ToolGlyph::Brain => "brain",
        ToolGlyph::MicVocal => "mic-vocal",
        ToolGlyph::Paseo => "",
        ToolGlyph::Wrench => "wrench",
        ToolGlyph::Plus => "plus",
        ToolGlyph::CircleDot => "circle-dot",
        ToolGlyph::Check => "check",
        ToolGlyph::CheckSquare => "square-check",
        ToolGlyph::Summary => "list-tree",
    }
}

fn draw_glyph(cx: &mut Cx2d, d: &mut Draw, glyph: ToolGlyph, r: Rect, size: f64, color: Vec4) {
    if glyph == ToolGlyph::Paseo {
        let s = size;
        d.kit.paseo_logo(
            cx,
            rect_xywh(r.center().x - s * 0.5, r.center().y - s * 0.5, s, s),
            color,
        );
    } else {
        d.kit.icon(cx, glyph_icon(glyph), r, size, color);
    }
}

/// Paints one row at (x, y) in `w`; returns its height and the answer field
/// of a question permission (drawn by the row widget as a native TextInput).
pub fn draw_row(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    item: &RowItem,
    x: f64,
    y: f64,
    w: f64,
) -> (f64, Option<AnswerField>) {
    let look = ctx.look;
    match &item.kind {
        RowKind::Inset(h) => (*h, None),
        RowKind::HistoryStart { loading } => {
            // HistoryStartSlot: a 48px slot with a spinner while paging.
            if *loading {
                d.spinner(
                    cx,
                    rect_xywh(x + w * 0.5 - 9.0, y + 15.0, 18.0, 18.0),
                    look.palette.muted,
                );
            }
            (48.0, None)
        }
        RowKind::Footer { rows, host, timing } => (
            footer(cx, d, ctx, &item.id, rows, *host, *timing, x, y, w),
            None,
        ),
        RowKind::Permission { key, request } => {
            let inset = look.list_inset;
            let cw = (w - 2.0 * inset).min(look.typo.content_max_width) - 16.0;
            let cx0 = x + (w - cw) * 0.5;
            let mut opts = md_opts(ctx, &item.id, None);
            let (h, field) = permission::draw(cx, d, key, request, cx0, y, cw, &mut opts);
            (h + 8.0, field)
        }
        RowKind::Entry {
            rows,
            index,
            previous_tasks,
            footer_below,
            streaming,
            live,
            gap_below,
        } => {
            let row = &rows[*index];
            (
                entry(
                    cx,
                    d,
                    ctx,
                    &item.id,
                    row,
                    previous_tasks.as_deref(),
                    *footer_below,
                    *streaming,
                    *live,
                    *gap_below,
                    x,
                    y,
                    w,
                ),
                None,
            )
        }
    }
}

fn md_opts<'a>(ctx: &'a RowContext, key: &'a str, find: Option<&'a mut FindMark>) -> MdOpts<'a> {
    MdOpts {
        font_size: ctx.look.content(),
        key,
        find,
        core: Some(&ctx.core),
        host_id: &ctx.host_id,
        workspace_root: ctx.cwd.as_deref(),
        syntax_theme: ctx.look.syntax.as_str(),
        counter: 0,
    }
}

/// Outer geometry (listContentContainer + streamItemWrapper): badges bleed
/// 13px so their icon rail lines up with message text.
fn content_box(look: &Look, x: f64, w: f64, badge: bool) -> (f64, f64) {
    let inset = if badge {
        look.list_inset - 5.0
    } else {
        look.list_inset
    };
    let max = look.typo.content_max_width + if badge { 10.0 } else { 0.0 };
    let outer = (w - 2.0 * inset).min(max).max(1.0);
    let ox = x + (w - outer) * 0.5;
    if badge {
        (ox, outer)
    } else {
        (ox + 8.0, (outer - 16.0).max(1.0))
    }
}

#[allow(clippy::too_many_arguments)]
fn entry(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    row: &StreamRow,
    previous_tasks: Option<&Value>,
    footer_below: bool,
    streaming: bool,
    live: bool,
    gap_below: f64,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let item = get(&row.entry, "item");
    let kind = str_of(get(item, "type"));
    let look = ctx.look;
    let seq = int_or(get(&row.entry, "seqStart"), i64::MIN);
    let mut find = ctx
        .find
        .as_ref()
        .filter(|f| f.row_seq == seq)
        .and_then(|f| {
            super::markdown::model::find_pattern(&f.query).map(|pattern| FindMark {
                pattern,
                occurrence: f.occurrence,
                seen: 0,
            })
        });
    match kind {
        "user_message" => {
            let (cx0, cw) = content_box(&look, x, w, false);
            user_message(cx, d, ctx, id, &row.entry, item, cx0, y, cw, find.as_mut()) + gap_below
        }
        "assistant_message" => {
            let (cx0, cw) = content_box(&look, x, w, false);
            // Preparation alone advances capture and publication. The final
            // paint pass uses the same complete source/layout snapshot.
            // draw_worker will request another frame only if work still
            // remains after preparing and publishing this frame's layout.
            if !d.state.managed_preparation || d.state.preparing {
                let options = markdown::worker::ParseOptions {
                    root: ctx.cwd.as_deref(),
                    linkify: true,
                    find: find.as_ref().map(|mark| &mark.pattern),
                };
                d.state.markdown_worker.update(
                    (id, &ctx.host_id, &ctx.agent_id),
                    row.entry.clone(),
                    live,
                    options,
                    d.now,
                    |bytes| markdown::view::admit_history_inline(cx, bytes),
                );
            }
            let top = 12.0;
            let mut opts = md_opts(ctx, id, find.as_mut());
            let h = markdown::view::draw_worker(cx, d, &mut opts, cx0, y + top, cw);
            top + h + if footer_below { 0.0 } else { 12.0 } + gap_below
        }
        "reasoning" => {
            let (cx0, cw) = content_box(&look, x, w, true);
            let text = str_of(get(item, "text"));
            let reveal = d
                .state
                .reveals
                .entry("thinking".into())
                .or_insert_with(|| super::reveal::Reveal::new(text));
            if reveal.sync(text, streaming, true) {
                reveal.arm(d.now);
                reveal.step(d.now);
                d.animating = true;
            }
            let visible = reveal.visible(text, streaming);
            let meaningful = !visible.trim().is_empty();
            let skeleton = !meaningful && streaming;
            let badge = Badge {
                key: format!("thinking:{id}"),
                glyph: ToolGlyph::Brain,
                label: t("agentStream.thinking"),
                secondary: None,
                loading: streaming,
                error: false,
                can_open: meaningful || skeleton,
                initially_expanded: ctx.auto_expand_reasoning,
                force_inline: ctx.auto_expand_reasoning,
                open_file: None,
                image: None,
                borderless: false,
                sheet: format!("thinking:{id}"),
                tap: None,
            };
            draw_badge(cx, d, ctx, &badge, cx0, y, cw, |cx, d, x, y, w| {
                let detail = json!({"type": "unknown", "input": visible, "output": null});
                let blocks = detail_blocks(&detail, Some("thinking"), None, &|k| t(k));
                draw_details(
                    cx,
                    d,
                    &blocks,
                    false,
                    skeleton,
                    &format!("{id}:thinking"),
                    x,
                    y,
                    w,
                    Some(400.0),
                    None,
                )
            }) + gap_below
        }
        "tool_call" if row.group.is_some() => {
            let (cx0, cw) = content_box(&look, x, w, true);
            overview_group(
                cx,
                d,
                ctx,
                id,
                row.group.as_deref().unwrap_or(&[]),
                cx0,
                y,
                cw,
            ) + gap_below
        }
        "turn_summary" => {
            let Some(fold) = row.fold.as_deref() else {
                return gap_below;
            };
            let (cx0, cw) = content_box(&look, x, w, true);
            turn_summary(cx, d, ctx, fold, cx0, y, cw) + gap_below
        }
        "tool_call" => {
            let target_title =
                tools::paseo_tool_target_agent_id(str_of(get(item, "name")), get(item, "detail"))
                    .and_then(|agent| ctx.agent_titles.get(&agent).cloned());
            let presentation =
                build_tool_call_presentation(item, ctx.cwd.as_deref(), target_title.as_deref());
            let detail = get(item, "detail");
            let speak_input = get(detail, "input");
            if str_of(get(item, "name")) == "speak"
                && get(detail, "type") == "unknown"
                && speak_input.as_str().is_some_and(|s| !s.trim().is_empty())
            {
                let (cx0, cw) = content_box(&look, x, w, false);
                return 12.0
                    + speak(cx, d, str_of(speak_input), cx0, y + 12.0, cw)
                    + 12.0
                    + gap_below;
            }
            if presentation.is_plan() {
                let (cx0, cw) = content_box(&look, x, w, false);
                let title = match presentation.plan_outcome {
                    Some("rejected") => t("agentStream.permission.rejectedPlan"),
                    Some("approved") => t("agentStream.permission.approvedPlan"),
                    Some("canceled") => t("agentStream.permission.canceledPlan"),
                    _ => t("agentStream.permission.plan"),
                };
                // The outcome resets local expansion: collapsed when rejected/canceled.
                let plan_key = format!(
                    "plan:{}:{}",
                    str_of(get(item, "callId")),
                    presentation.plan_outcome.unwrap_or("")
                );
                let toggle = format!("{plan_key}:plan");
                let open = !matches!(presentation.plan_outcome, Some("rejected" | "canceled"));
                d.state.expanded.entry(toggle).or_insert(open);
                let mut opts = md_opts(ctx, id, None);
                return 12.0
                    + permission::plan_card(
                        cx,
                        d,
                        &plan_key,
                        &title,
                        "",
                        str_of(get(detail, "text")),
                        cx0,
                        y + 12.0,
                        cw,
                        &mut opts,
                        None,
                    )
                    + 12.0
                    + gap_below;
            }
            let (cx0, cw) = content_box(&look, x, w, true);
            let call = str_of(get(item, "callId")).to_owned();
            let badge = Badge {
                key: format!("tool:{call}:{}", ctx.expanded_tools),
                glyph: presentation.icon,
                label: presentation.display_name.clone(),
                secondary: presentation.summary.clone(),
                loading: presentation.is_loading(),
                error: presentation.is_error(),
                can_open: presentation.can_open_details
                    || tools::tool_call_error_text("failed", get(item, "error")).is_some()
                        && !get(item, "error").is_null(),
                initially_expanded: ctx.expanded_tools,
                force_inline: false,
                open_file: presentation.open_file_path.clone(),
                image: tools::read_image_path(&presentation.detail, &presentation.status).map(
                    |path| BadgeImage {
                        path,
                        row: id.to_owned(),
                    },
                ),
                borderless: false,
                sheet: format!("tool:{id}"),
                tap: None,
            };
            let tool_name = presentation.tool_name.clone();
            let error = presentation.error_text.clone();
            let skeleton = presentation.is_loading_details;
            let detail = presentation.detail.clone();
            let max = if row.group.is_some() { 200.0 } else { 400.0 };
            draw_badge(cx, d, ctx, &badge, cx0, y, cw, |cx, d, x, y, w| {
                let blocks = detail_blocks(&detail, Some(&tool_name), error.as_deref(), &|k| t(k));
                draw_details(
                    cx,
                    d,
                    &blocks,
                    detail_is_full_bleed(&detail),
                    skeleton,
                    &format!("{id}:detail"),
                    x,
                    y,
                    w,
                    Some(max),
                    None,
                )
            }) + gap_below
        }
        "todo" => {
            let (cx0, cw) = content_box(&look, x, w, true);
            todo(cx, d, ctx, id, item, previous_tasks, cx0, y, cw) + gap_below
        }
        "compaction" => {
            let (cx0, cw) = content_box(&look, x, w, false);
            compaction(cx, d, item, cx0, y, cw) + gap_below
        }
        "error" | "notification" => {
            let (cx0, cw) = content_box(&look, x, w, false);
            let (level, message) = if kind == "error" {
                ("error", str_of(get(item, "message")).to_owned())
            } else {
                (
                    str_or(get(item, "level"), "info"),
                    str_of(get(item, "message")).to_owned(),
                )
            };
            let message = if kind == "error" && message.is_empty() {
                t("agentStream.unknownError")
            } else {
                message
            };
            notification(cx, d, level, &message, cx0, y, cw) + gap_below
        }
        _ => {
            // Server-authored content without an executable client renderer:
            // the item's fields as structured, selectable details.
            let (cx0, cw) = content_box(&look, x, w, false);
            let text = pretty(item);
            let blocks = vec![tools::DetailBlock::JsonBox(text)];
            draw_details(
                cx,
                d,
                &blocks,
                false,
                false,
                &format!("{id}:unknown"),
                cx0,
                y,
                cw,
                None,
                None,
            ) + gap_below
        }
    }
}

// ---- UserMessage ----

#[allow(clippy::too_many_arguments)]
fn user_message(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    entry: &Value,
    item: &Value,
    x: f64,
    y: f64,
    w: f64,
    find: Option<&mut FindMark>,
) -> f64 {
    let look = ctx.look;
    let p = look.palette;
    let text = str_of(get(item, "text"));
    let images = list(get(item, "images"));
    let attachments = list(get(item, "attachments"));
    let has_text = !text.trim().is_empty();
    let pending = is_true(get(entry, "pending"));
    let size = look.content();
    let height = if look.mobile {
        1.3
    } else {
        (size * 1.4).round() / size
    };
    let max_inner = (w - 32.0).max(1.0);
    let laid = has_text.then(|| {
        d.kit
            .layout(cx, Font::Regular, size, height, text, Some(max_inner), 0.0)
    });
    let copy_text = laid.as_ref().map(|layout| layout.text.clone());
    // Attachment pills.
    let pills: Vec<(String, Option<String>)> = attachments
        .iter()
        .map(|a| {
            let (label, secondary) = super::composer_bridge::attachment_label(a);
            (label, secondary)
        })
        .collect();
    let pill_h = 32.0;
    let image_size = 48.0;
    let mut inner_w = laid.as_ref().map(|l| laidout_size(l).x).unwrap_or(0.0);
    let images_w = images.len() as f64 * (image_size + 8.0) - 8.0;
    if !images.is_empty() {
        inner_w = inner_w.max(images_w.min(max_inner));
    }
    // (pill width, secondary width incl. its gap). A long name is clipped to one
    // line with an ellipsis; the pill never grows taller than `pill_h`.
    let pill_widths: Vec<(f64, f64)> = pills
        .iter()
        .map(|(label, secondary)| {
            let l = d
                .kit
                .measure(cx, Font::Regular, look.ui(13.0), 1.3, label, None)
                .x;
            let s = secondary
                .as_ref()
                .map(|s| {
                    d.kit
                        .measure(cx, Font::Regular, look.ui(12.0), 1.3, s, None)
                        .x
                        + 6.0
                })
                .unwrap_or(0.0);
            ((28.0 + l + s + 10.0).min(max_inner), s)
        })
        .collect();
    for (pw, _) in &pill_widths {
        inner_w = inner_w.max(*pw);
    }
    // Wrapped layout heights.
    let wrap_rows = |widths: &[f64], max: f64| -> usize {
        let mut rows = 0;
        let mut cursor = f64::MAX;
        for w in widths {
            if cursor + w > max {
                rows += 1;
                cursor = 0.0;
            }
            cursor += w + 8.0;
        }
        rows
    };
    let image_rows = if images.is_empty() {
        0
    } else {
        wrap_rows(&vec![image_size; images.len()], inner_w.max(image_size))
    };
    let pill_rows = wrap_rows(
        &pill_widths.iter().map(|(pw, _)| *pw).collect::<Vec<_>>(),
        inner_w.max(1.0),
    );
    let mut bubble_h = 32.0;
    if image_rows > 0 {
        bubble_h += image_rows as f64 * (image_size + 8.0) - 8.0
            + if has_text || !attachments.is_empty() {
                8.0
            } else {
                0.0
            };
    }
    if pill_rows > 0 {
        bubble_h += pill_rows as f64 * (pill_h + 8.0) - 8.0 + if has_text { 8.0 } else { 0.0 };
    }
    if let Some(l) = &laid {
        bubble_h += laidout_size(l).y;
    }
    let bubble_w = inner_w + 32.0;
    let bx = x + w - bubble_w;
    // surface3, radii (16, 2, 16, 16).
    d.kit.rect_full(
        cx,
        rect_xywh(bx, y, bubble_w, bubble_h),
        p.surface3,
        vec4(16.0, 2.0, 16.0, 16.0),
        TRANSPARENT,
        0.0,
    );
    let mut cy = y + 16.0;
    if image_rows > 0 {
        let mut ix = bx + 16.0;
        for (index, image) in images.iter().enumerate() {
            if ix > bx + 16.0 && ix + image_size > bx + 16.0 + inner_w {
                ix = bx + 16.0;
                cy += image_size + 8.0;
            }
            let r = rect_xywh(ix, cy, image_size, image_size);
            d.kit.bordered(cx, r, p.surface2, 8.0, p.border, 1.0);
            match super::composer_bridge::image_texture(cx, image, true) {
                Some(texture) => d.kit.texture(cx, r, &texture, 8.0),
                None => d.kit.icon(cx, "image", r, 16.0, p.muted),
            }
            d.canvas.add_hit(r, HitTarget::UserImage { index });
            ix += image_size + 8.0;
        }
        cy += image_size
            + if has_text || !attachments.is_empty() {
                16.0
            } else {
                8.0
            };
    }
    if pill_rows > 0 {
        let mut px = bx + 16.0;
        for ((label, secondary), (pw, sw)) in pills.iter().zip(&pill_widths) {
            if px > bx + 16.0 && px + pw > bx + 16.0 + inner_w {
                px = bx + 16.0;
                cy += pill_h + 8.0;
            }
            let r = rect_xywh(px, cy, *pw, pill_h);
            d.kit.bordered(cx, r, p.surface2, 8.0, p.border, 1.0);
            d.kit.icon(
                cx,
                "paperclip",
                rect_xywh(px + 8.0, cy, 14.0, pill_h),
                14.0,
                p.muted,
            );
            let lsize = d.kit.label_line(
                cx,
                Font::Regular,
                look.ui(13.0),
                1.3,
                p.foreground,
                dvec2(px + 28.0, cy + 8.0),
                label,
                (pw - 38.0 - sw).max(1.0),
            );
            if let Some(secondary) = secondary {
                d.kit.label(
                    cx,
                    Font::Regular,
                    look.ui(12.0),
                    1.3,
                    p.muted,
                    dvec2(px + 34.0 + lsize.x, cy + 9.0),
                    secondary,
                    None,
                );
            }
            px += pw + 8.0;
        }
        cy += pill_h + if has_text { 8.0 } else { 0.0 };
    }
    if let Some(laid) = laid {
        let pos = dvec2(bx + 16.0, cy);
        if let Some(find) = find {
            for (seen, m) in find.pattern.find_iter(text).enumerate() {
                if seen == find.occurrence {
                    for r in range_rects(&laid, m.start(), m.end()) {
                        if !d.dry() {
                            d.state.find_y.get_or_insert(r.pos.y + pos.y);
                        }
                        d.kit.rect(cx, r.translate(pos), p.warning, 2.0);
                    }
                }
            }
        }
        d.kit
            .draw_laidout(cx, Font::Regular, p.foreground, pos, &laid);
        d.canvas.add_run(laid, pos, Some('\n'));
    }
    let mut h = bubble_h;
    // trailingRow: timestamp, rewind menu and copy; always on compact/native
    // layouts, on hover otherwise; hidden while pending.
    if has_text {
        let show = !pending && (look.compact || look.mobile || d.state.hovered);
        let ry = y + bubble_h + 8.0;
        if show {
            let mut rx = x + w + 4.0;
            let copy_key = format!("copy:{id}");
            rx -= 22.0;
            let copy = rect_xywh(rx, ry + 1.0, 22.0, 22.0);
            let hovered = d.state.hover_hit.as_deref() == Some(copy_key.as_str());
            d.copy_icon(
                cx,
                &copy_key,
                copy,
                14.0,
                if hovered { p.foreground } else { p.muted },
            );
            if let Some(text) = copy_text {
                d.canvas.add_hit(
                    copy,
                    HitTarget::Copy {
                        key: copy_key,
                        text,
                    },
                );
            }
            let modes = rewind_modes(&ctx.capabilities);
            if !ctx.is_subagent && !get(item, "messageId").is_null() && !modes.is_empty() {
                rx -= 8.0 + 24.0;
                let r = rect_xywh(rx, ry, 24.0, 24.0);
                let open = d.state.menu_open.contains("rewind");
                let busy = d.state.menu_busy.contains("rewind");
                let color = if open || d.state.hover_hit.as_deref() == Some("rewind") {
                    p.foreground
                } else {
                    p.muted
                };
                d.kit.icon(
                    cx,
                    "undo-2",
                    r,
                    16.0,
                    if busy { with_alpha(color, 0.5) } else { color },
                );
                d.canvas.add_hit(r, HitTarget::RewindMenu);
            }
            if let Some(ms) = parse_time(get(entry, "timestamp")) {
                let stamp = format::message_timestamp(ms);
                let meta = look.ui(STREAM_METADATA_SIZE);
                let size = d.kit.measure(cx, Font::Regular, meta, 1.3, &stamp, None);
                rx -= 8.0 + size.x;
                d.kit.label(
                    cx,
                    Font::Regular,
                    meta,
                    1.3,
                    p.muted,
                    dvec2(rx, ry + (24.0 - size.y) * 0.5),
                    &stamp,
                    None,
                );
            }
        } else if is_true(get(entry, "queued")) {
            // Accepted by the provider; the agent reads it after its current step.
            let note = t("composer.delivery.queued");
            let meta = look.ui(STREAM_METADATA_SIZE);
            let size = d.kit.measure(cx, Font::Regular, meta, 1.3, &note, None);
            d.kit.label(
                cx,
                Font::Regular,
                meta,
                1.3,
                p.muted,
                dvec2(x + w - size.x, ry + (24.0 - size.y) * 0.5),
                &note,
                None,
            );
        }
        h += 8.0 + 24.0;
    }
    h
}

/// rewind-menu.tsx modes the agent supports.
pub fn rewind_modes(capabilities: &Value) -> Vec<&'static str> {
    let mut modes = Vec::new();
    if is_true(get(capabilities, "supportsRewindConversation")) {
        modes.push("conversation");
    }
    if is_true(get(capabilities, "supportsRewindFiles")) {
        modes.push("files");
    }
    if is_true(get(capabilities, "supportsRewindBoth")) {
        modes.push("both");
    }
    modes
}

// ---- SpeakMessage / Notification / Compaction ----

fn speak(cx: &mut Cx2d, d: &mut Draw, message: &str, x: f64, y: f64, w: f64) -> f64 {
    let look = d.look;
    let p = look.palette;
    let base = look.ui(14.0);
    let header = t("message.speak.header");
    let hs = d.kit.measure(cx, Font::Regular, base, 1.3, &header, None);
    d.kit
        .icon(cx, "mic-vocal", rect_xywh(x, y, 12.0, hs.y), 12.0, p.muted);
    d.kit.label(
        cx,
        Font::Regular,
        base,
        1.3,
        p.muted,
        dvec2(x + 20.0, y),
        &header,
        None,
    );
    let size = look.content();
    let laid = d.kit.layout(
        cx,
        Font::Regular,
        size,
        (size * 1.4).round() / size,
        message,
        Some(w),
        0.0,
    );
    let pos = dvec2(x, y + hs.y + 8.0);
    d.kit
        .draw_laidout(cx, Font::Regular, p.foreground, pos, &laid);
    let h = hs.y + 8.0 + laidout_size(&laid).y;
    d.canvas.add_run(laid, pos, Some('\n'));
    h
}

/// message.tsx Notification: tinted 6px card, 12x10 padding, 16px level glyph
/// in a 20px rail, base 14/20 text.
pub fn notification(
    cx: &mut Cx2d,
    d: &mut Draw,
    level: &str,
    message: &str,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let p = d.look.palette;
    let (icon, color) = match level {
        "warning" => ("triangle-alert", p.warning),
        "error" => ("circle-x", p.danger),
        _ => ("info", p.running),
    };
    let bg = with_alpha(color, 0.1);
    let size = d.look.ui(14.0);
    let laid = d.kit.layout(
        cx,
        Font::Regular,
        size,
        d.look.typo.system_size(20.0) / size,
        message,
        Some((w - 52.0).max(1.0)),
        0.0,
    );
    let h = laidout_size(&laid).y.max(20.0) + 20.0;
    d.kit.rect(cx, rect_xywh(x, y, w, h), bg, 6.0);
    d.kit.icon(
        cx,
        icon,
        rect_xywh(x + 12.0, y + 10.0, 20.0, 20.0),
        16.0,
        color,
    );
    let pos = dvec2(x + 40.0, y + 10.0);
    d.kit
        .draw_laidout(cx, Font::Regular, p.foreground, pos, &laid);
    d.canvas.add_run(laid, pos, Some('\n'));
    h
}

fn compaction(cx: &mut Cx2d, d: &mut Draw, item: &Value, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let status = str_or(get(item, "status"), "completed");
    let label = format::compaction_label(
        status,
        get(item, "trigger").as_str(),
        get(item, "preTokens").as_f64(),
    );
    let text_size = d.look.ui(13.0);
    let size = d
        .kit
        .measure(cx, Font::Regular, text_size, 1.3, &label, None);
    let glyph = 16.0;
    let mid_w = glyph + 8.0 + size.x;
    let row_h = size.y.max(16.0);
    let cy = y + 12.0;
    let mx = x + 16.0 + ((w - 32.0) - mid_w) * 0.5;
    let line_y = cy + row_h * 0.5;
    d.kit.rect(
        cx,
        rect_xywh(x + 16.0, line_y, (mx - 8.0 - x - 16.0).max(0.0), 1.0),
        p.border,
        0.0,
    );
    d.kit.rect(
        cx,
        rect_xywh(
            mx + mid_w + 8.0,
            line_y,
            (x + w - 16.0 - mx - mid_w - 8.0).max(0.0),
            1.0,
        ),
        p.border,
        0.0,
    );
    let glyph_color = p.muted;
    if status == "loading" {
        d.spinner(
            cx,
            rect_xywh(mx, cy + (row_h - 16.0) * 0.5, 16.0, 16.0),
            glyph_color,
        );
    } else {
        d.kit.icon(
            cx,
            "scissors",
            rect_xywh(mx, cy, glyph, row_h),
            12.0,
            glyph_color,
        );
    }
    d.kit.label(
        cx,
        Font::Regular,
        text_size,
        1.3,
        p.muted,
        dvec2(mx + glyph + 8.0, cy + (row_h - size.y) * 0.5),
        &label,
        None,
    );
    row_h + 24.0
}

// ---- TodoListCard ----

fn task_status(task: &Value) -> &'static str {
    if is_true(get(task, "completed")) || get(task, "status") == "completed" {
        "completed"
    } else if get(task, "status") == "in_progress" {
        "in_progress"
    } else {
        "pending"
    }
}

fn task_key(task: &Value, index: usize) -> String {
    match get(task, "id").as_str() {
        Some(id) => id.to_owned(),
        None => format!("{index}:{}", str_of(get(task, "text"))),
    }
}

/// stream.ts deriveTaskActivities.
pub fn todo_activities(
    item: &Value,
    previous: Option<&Value>,
) -> Vec<(&'static str, String, usize)> {
    let tasks = list(get(item, "items"));
    let before = previous.map(|p| list(get(p, "items"))).unwrap_or(&[]);
    if before.is_empty() {
        return if tasks.is_empty() {
            Vec::new()
        } else {
            vec![("created", String::new(), tasks.len())]
        };
    }
    let by_key: HashMap<String, &Value> = before
        .iter()
        .enumerate()
        .map(|(i, t)| (task_key(t, i), t))
        .collect();
    let mut out = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        let text = str_of(get(task, "text")).to_owned();
        let Some(prior) = by_key.get(&task_key(task, i)) else {
            out.push(("added", text, 0));
            continue;
        };
        let (from, to) = (task_status(prior), task_status(task));
        if from == to {
            continue;
        }
        if to == "completed" {
            out.push(("completed", text.clone(), 0));
        }
        if to == "in_progress" {
            out.push(("started", text, 0));
        }
    }
    out
}

/// components/task-list-row.tsx rows (16px glyph, 8px from base text). Text
/// wraps instead of the original's one-line truncation so the whole task stays
/// readable on narrow sheets; the glyph stays on the first line.
pub fn task_rows(cx: &mut Cx2d, d: &mut Draw, tasks: &[Value], x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let mut cy = y;
    for (i, task) in tasks.iter().enumerate() {
        if i > 0 {
            cy += 4.0;
        }
        let status = task_status(task);
        let running = status == "in_progress";
        let completed = status == "completed";
        let text = if running && !str_of(get(task, "activeForm")).is_empty() {
            str_of(get(task, "activeForm"))
        } else {
            str_of(get(task, "text"))
        };
        let color = if running { p.foreground } else { p.muted };
        let tx = x + 24.0;
        let avail = (w - 24.0).max(1.0);
        let laid = d
            .kit
            .layout(cx, Font::Regular, base, 1.4, text, Some(avail), 0.0);
        let size = laidout_size(&laid);
        let row_h = size.y.max(16.0);
        let first_line = (base * 1.4).clamp(16.0, row_h);
        let icon = if completed {
            "circle-check"
        } else if running {
            "circle-dot"
        } else {
            "circle"
        };
        d.kit.icon(
            cx,
            icon,
            rect_xywh(x, cy, 16.0, first_line),
            16.0,
            if running { p.running } else { p.muted },
        );
        let ty = cy + (row_h - size.y) * 0.5;
        cx.begin_turtle(
            Walk {
                abs_pos: Some(dvec2(tx, cy)),
                margin: Margin::default(),
                width: Size::Fixed(avail),
                height: Size::Fixed(row_h),
            },
            Layout::default(),
        );
        d.kit
            .draw_laidout(cx, Font::Regular, color, dvec2(tx, ty), &laid);
        cx.end_turtle();
        if completed {
            for line in &laid.rows {
                let mid = (line.origin_in_lpxs.y
                    - (line.ascender_in_lpxs + line.descender_in_lpxs) * 0.5)
                    as f64;
                d.kit.rect(
                    cx,
                    rect_xywh(tx, ty + mid, (line.width_in_lpxs as f64).min(avail), 1.0),
                    p.extra_muted,
                    0.0,
                );
            }
        }
        cy += row_h;
    }
    cy - y
}

#[allow(clippy::too_many_arguments)]
fn todo(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    item: &Value,
    previous: Option<&Value>,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let tasks: Vec<Value> = list(get(item, "items")).to_vec();
    let activities = todo_activities(item, previous);
    let mut cy = y;
    for (index, (kind, task, count)) in activities.iter().enumerate() {
        if index > 0 {
            cy += 4.0;
        }
        let glyph = match *kind {
            "added" => ToolGlyph::Plus,
            "started" => ToolGlyph::CircleDot,
            "completed" => ToolGlyph::Check,
            _ => ToolGlyph::CheckSquare,
        };
        let label = if *kind == "created" {
            ta(
                "message.todo.activity.created",
                &[("count", &count.to_string())],
            )
        } else {
            t(&format!("message.todo.activity.{kind}"))
        };
        let badge = Badge {
            key: format!("todo:{id}:{index}"),
            glyph,
            label,
            secondary: (!task.is_empty()).then(|| task.clone()),
            loading: false,
            error: false,
            can_open: true,
            initially_expanded: false,
            force_inline: true,
            open_file: None,
            image: None,
            borderless: false,
            sheet: String::new(),
            tap: None,
        };
        let tasks = tasks.clone();
        cy += draw_badge(cx, d, ctx, &badge, x, cy, w, |cx, d, x, y, w| {
            if tasks.is_empty() {
                let empty = t("message.todo.empty");
                8.0 + d
                    .kit
                    .label(
                        cx,
                        Font::Regular,
                        d.look.ui(14.0),
                        1.4,
                        d.look.palette.muted,
                        dvec2(x + 8.0, y + 8.0),
                        &empty,
                        Some(w - 16.0),
                    )
                    .y
                    + 8.0
            } else {
                8.0 + task_rows(cx, d, &tasks, x + 8.0, y + 8.0, w - 16.0) + 8.0
            }
        });
    }
    cy - y
}

// ---- ExpandableBadge ----

pub struct Badge {
    pub key: String,
    pub glyph: ToolGlyph,
    pub label: String,
    pub secondary: Option<String>,
    pub loading: bool,
    pub error: bool,
    pub can_open: bool,
    pub initially_expanded: bool,
    /// Inline even on compact layouts (Thinking with autoExpandReasoning, tasks).
    pub force_inline: bool,
    pub open_file: Option<String>,
    pub borderless: bool,
    /// Sheet id opened on compact layouts.
    pub sheet: String,
    /// What a tap on the row does instead of expanding it; the row then stays
    /// expanded as `initially_expanded` says.
    pub tap: Option<HitTarget>,
    /// An image the call read, shown below the row.
    pub image: Option<BadgeImage>,
}

/// An image file shown under a badge; `row` is the timeline row the lightbox
/// opens it from.
pub struct BadgeImage {
    pub path: String,
    pub row: String,
}

/// Most a badge's image is tall.
const BADGE_IMAGE_MAX_HEIGHT: f64 = 200.0;

/// [`BadgeImage`] fit in the badge's text column: loading reserves the full
/// height, an image that cannot be read or decoded takes no space (the row
/// still opens the file), and tapping one opens the lightbox.
#[allow(clippy::too_many_arguments)]
fn badge_image(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    image: &BadgeImage,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    const TOP: f64 = 2.0;
    const BOTTOM: f64 = 6.0;
    let owner = d.result_owner(cx);
    let state = images::fitted_image(
        cx,
        Some(&ctx.core),
        &ctx.host_id,
        ctx.cwd.as_deref(),
        &image.path,
        &image.row,
        owner,
        w,
    );
    match state {
        ImageState::Loading => TOP + BADGE_IMAGE_MAX_HEIGHT + BOTTOM,
        ImageState::Ready {
            texture,
            width,
            height,
        } => {
            let aspect = width as f64 / height.max(1) as f64;
            let h = BADGE_IMAGE_MAX_HEIGHT.min(w / aspect);
            let r = rect_xywh(x, y + TOP, h * aspect, h);
            d.kit.texture(cx, r, &texture, 6.0);
            d.canvas.add_hit(
                r,
                HitTarget::Image {
                    key: image.path.clone(),
                },
            );
            TOP + h + BOTTOM
        }
        ImageState::Failed | ImageState::Error(_) => 0.0,
    }
}

/// message.tsx ExpandableBadge: muted single-line row (22px rail, 12px glyph,
/// label, ellipsized secondary) expanding into an attached bordered detail
/// panel; compact layouts open the tool-call sheet instead.
#[allow(clippy::too_many_arguments)]
pub fn draw_badge(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    badge: &Badge,
    x: f64,
    y: f64,
    w: f64,
    details: impl Fn(&mut Cx2d, &mut Draw, f64, f64, f64) -> f64,
) -> f64 {
    let look = ctx.look;
    let p = look.palette;
    let inline = badge.force_inline || !look.compact;
    let interactive = badge.can_open;
    let expanded = interactive
        && inline
        && if badge.tap.is_some() {
            badge.initially_expanded
        } else {
            d.state.is_expanded(&badge.key, badge.initially_expanded)
        };
    // Hovering the open-file glyph still hovers its badge.
    let file_key = badge
        .open_file
        .as_ref()
        .map(|path| super::permission::hit_key(&HitTarget::OpenFile(path.clone())));
    let over_file = file_key.is_some() && d.state.hover_hit == file_key;
    let hovered = over_file
        || d.state
            .hover_hit
            .as_deref()
            .is_some_and(|h| h.contains(&badge.key));
    // Touch layouts cannot hover: the glyph stays visible there.
    let show_file = badge.open_file.is_some() && (hovered || look.mobile);
    let active = hovered || expanded;
    let base = look.ui(14.0);
    let label_color = if badge.loading {
        with_alpha(p.foreground, 0.72)
    } else if active {
        p.foreground
    } else {
        p.muted
    };
    let secondary_color = if active { p.foreground } else { p.muted };
    let lx = x + 8.0 + 26.0;
    let avail = (w - 8.0 - 26.0 - 8.0 - if show_file { 26.0 } else { 0.0 }).max(1.0);
    let label = d
        .kit
        .layout_clamped(cx, Font::Regular, base, 1.3, &badge.label, avail, 1);
    let label_size = laidout_size(&label);
    let secondary_x = lx + label_size.x.min(avail) + 8.0;
    let secondary_w = (lx + avail - secondary_x).max(0.0);
    let secondary_text = badge
        .secondary
        .as_ref()
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "));
    let secondary = secondary_text
        .as_ref()
        .filter(|text| !text.is_empty() && secondary_w > 0.0)
        .map(|text| {
            d.kit
                .layout_clamped(cx, Font::Regular, base, 1.3, text, secondary_w, 1)
        });
    let label_metrics = InlineMetrics::from_layout(&label);
    let secondary_metrics = secondary
        .as_ref()
        .map_or(InlineMetrics::default(), |layout| {
            InlineMetrics::from_layout(layout)
        });
    let metrics = label_metrics.union(secondary_metrics);
    let header_h = RowMetrics::new(metrics.height(), 0.0, 0.0).height(30.0, 4.0, 22.0);
    let header = rect_xywh(x, y, w, header_h);
    let label_pos = dvec2(lx, metrics.top(header, label_metrics));
    let secondary_pos = dvec2(secondary_x, metrics.top(header, secondary_metrics));
    let attached = expanded && !badge.borderless;

    let body_h = if expanded {
        d.measure(|d| details(cx, d, x + 1.0, y + header_h, w - 2.0))
    } else {
        0.0
    };
    // Header background / border.
    if expanded {
        let radii = if attached {
            vec4(8.0, 8.0, 0.0, 0.0)
        } else {
            vec4(8.0, 8.0, 8.0, 8.0)
        };
        d.kit.rect_full(
            cx,
            rect_xywh(x, y, w, header_h),
            p.surface1,
            radii,
            if attached { p.border } else { TRANSPARENT },
            if attached { 1.0 } else { 0.0 },
        );
    }
    if attached {
        d.kit.rect_full(
            cx,
            rect_xywh(x, y + header_h - 1.0, w, body_h + 1.0),
            TRANSPARENT,
            vec4(0.0, 0.0, 8.0, 8.0),
            p.border,
            1.0,
        );
    }
    // Glyph rail.
    let rail = rect_xywh(x + 8.0, y + 4.0, 22.0, header_h - 8.0);
    if interactive && active {
        d.kit.icon(
            cx,
            if expanded {
                "chevron-down"
            } else {
                "chevron-right"
            },
            Rect {
                pos: rail.pos - dvec2(2.0, 0.0),
                ..rail
            },
            15.6,
            p.foreground,
        );
    } else if badge.error {
        d.kit
            .icon(cx, "triangle-alert", rail, 12.0, with_alpha(p.danger, 0.8));
    } else {
        draw_glyph(
            cx,
            d,
            badge.glyph,
            Rect {
                pos: rail.pos - dvec2(0.5, 0.0),
                ..rail
            },
            12.0,
            if active { p.foreground } else { p.muted },
        );
    }
    // Adjacent labels share a baseline; fallback-font ascent/descent contributes
    // to the row height, hit box and attached detail offset.
    let clip = rect_xywh(lx, y, avail, header_h);
    cx.begin_turtle(
        Walk {
            abs_pos: Some(clip.pos),
            margin: Margin::default(),
            width: Size::Fixed(clip.size.x),
            height: Size::Fixed(clip.size.y),
        },
        Layout::default(),
    );
    d.kit
        .draw_laidout(cx, Font::Regular, label_color, label_pos, &label);
    let mut end_x = lx + label_size.x.min(avail);
    if let Some(secondary) = &secondary {
        d.kit
            .draw_laidout(cx, Font::Regular, secondary_color, secondary_pos, secondary);
        end_x = (secondary_x + laidout_size(secondary).x).min(lx + avail);
    }
    if badge.loading {
        // Shimmer: a bright peak sweeping across the labels.
        let width = end_x - lx;
        if !d.dry() && width > 0.0 {
            d.indicator(
                cx,
                Indicator::Shimmer {
                    rect: rect_xywh(lx, y, width, header_h),
                    color: vec4(1.0, 1.0, 1.0, if look.dark() { 0.9 } else { 0.55 }),
                    label,
                    label_pos,
                    secondary,
                    secondary_pos,
                },
            );
        }
    }
    cx.end_turtle();
    if let (Some(path), true) = (&badge.open_file, show_file) {
        let r = rect_xywh(
            (end_x + 4.0).min(x + w - 30.0),
            y + (header_h - 22.0) * 0.5,
            22.0,
            22.0,
        );
        d.kit.icon(
            cx,
            "file-symlink",
            r,
            14.0,
            if over_file { p.foreground } else { p.muted },
        );
        d.canvas.add_hit(r, HitTarget::OpenFile(path.clone()));
    }
    if interactive {
        let target = if let Some(tap) = &badge.tap {
            tap.clone()
        } else if inline {
            HitTarget::Toggle(badge.key.clone())
        } else {
            HitTarget::OpenSheet(badge.sheet.clone())
        };
        // Registered first so the open-file glyph above wins.
        let header = rect_xywh(x, y, w, header_h);
        d.canvas.add_hit_under(header, target);
    }
    let mut height = if expanded {
        let h = details(cx, d, x + 1.0, y + header_h, w - 2.0);
        header_h + h.max(body_h)
    } else {
        header_h
    };
    if let Some(image) = &badge.image {
        height += badge_image(
            cx,
            d,
            ctx,
            image,
            lx,
            y + height,
            (x + w - 8.0 - lx).max(1.0),
        );
    }
    height
}

// ---- Turn summaries ----

/// turn-collapse/summary-line.tsx TurnSummaryLine: a folded turn's row,
/// styled like a tool-call group and labelled with what its calls did.
/// Tapping it shows or folds the turn's rows below it.
fn turn_summary(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    fold: &super::layout::TurnFold,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let calls: Vec<Rc<Value>> = fold
        .entries
        .iter()
        .filter(|entry| super::layout::stream_kind(Some(entry)) == "tool_call")
        .cloned()
        .collect();
    let (summary, _) = tools::build_overview_summary(&calls);
    let label = format::overview_summary(&summary);
    let label = if label.is_empty() {
        t("agentStream.turnSummary.details")
    } else {
        label
    };
    let tap = HitTarget::FoldTurn(fold.key.clone());
    let badge = Badge {
        // The hover test matches the badge key inside the hovered hit's key.
        key: super::permission::hit_key(&tap),
        glyph: ToolGlyph::Summary,
        label,
        secondary: None,
        loading: false,
        error: false,
        can_open: true,
        initially_expanded: fold.open,
        force_inline: true,
        open_file: None,
        image: None,
        borderless: true,
        sheet: String::new(),
        tap: Some(tap),
    };
    draw_badge(cx, d, ctx, &badge, x, y, w, |_, _, _, _, _| 0.0)
}

// ---- Overview groups ----

#[allow(clippy::too_many_arguments)]
fn overview_group(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    calls: &[Rc<Value>],
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let (summary, loading) = tools::build_overview_summary(calls);
    let label = format::overview_summary(&summary);
    let group_id = calls
        .first()
        .map(|c| str_of(get(get(c, "item"), "callId")).to_owned())
        .unwrap_or_default();
    let badge = Badge {
        key: format!("group:{group_id}"),
        glyph: ToolGlyph::Wrench,
        label,
        secondary: None,
        loading,
        error: false,
        can_open: true,
        initially_expanded: false,
        force_inline: false,
        open_file: None,
        image: None,
        borderless: true,
        sheet: format!("group:{id}"),
        tap: None,
    };
    let calls: Vec<Rc<Value>> = calls.to_vec();
    draw_badge(cx, d, ctx, &badge, x, y, w, |cx, d, x, y, w| {
        // 400px scroll pinned to the latest call, 4 top.
        let key = format!("{id}:group:y");
        let content = d.measure(|d| embedded_calls(cx, d, ctx, id, &calls, x, y + 4.0, w));
        let h = (content + 4.0).min(400.0);
        let max_y = (content + 4.0 - h).max(0.0);
        d.state.extent_y.insert(key.clone(), max_y);
        // Pinned to end unless the reader scrolled up inside it.
        let pinned = d.state.expanded.get(&format!("{key}:detached")).copied() != Some(true);
        let sy = if pinned {
            max_y
        } else {
            d.state
                .scroll_y
                .get(&key)
                .copied()
                .unwrap_or(max_y)
                .clamp(0.0, max_y)
        };
        d.state.scroll_y.insert(key.clone(), sy);
        let view = rect_xywh(x, y, w, h);
        // Registered before the calls so their own detail regions win.
        if max_y > 0.0 {
            d.canvas.add_hit(view, HitTarget::ScrollY(key.clone()));
        }
        cx.begin_turtle(
            Walk {
                abs_pos: Some(view.pos),
                margin: Margin::default(),
                width: Size::Fixed(w),
                height: Size::Fixed(h.max(1.0)),
            },
            Layout::default(),
        );
        d.canvas.push_clip(view);
        embedded_calls(cx, d, ctx, id, &calls, x, y + 4.0 - sy, w);
        d.canvas.pop_clip();
        cx.end_turtle();
        super::scrollbar::draw(
            cx,
            d,
            view,
            None,
            None,
            Some(super::scrollbar::Axis {
                key: &key,
                offset: sy,
                max: max_y,
            }),
        );
        h
    })
}

/// Calls inside an overview group (or its sheet): no stream wrapper, 4px
/// between calls of one sequence, 16 after the last; details capped at 200.
#[allow(clippy::too_many_arguments)]
pub fn embedded_calls(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    calls: &[Rc<Value>],
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let mut cy = y;
    for (i, call) in calls.iter().enumerate() {
        let item = get(call, "item");
        let presentation = build_tool_call_presentation(item, ctx.cwd.as_deref(), None);
        let call_id = str_of(get(item, "callId")).to_owned();
        let badge = Badge {
            key: format!("tool:{call_id}:embedded"),
            glyph: presentation.icon,
            label: presentation.display_name.clone(),
            secondary: presentation.summary.clone(),
            loading: presentation.is_loading(),
            error: presentation.is_error(),
            can_open: presentation.can_open_details,
            initially_expanded: false,
            force_inline: true,
            open_file: presentation.open_file_path.clone(),
            image: tools::read_image_path(&presentation.detail, &presentation.status).map(|path| {
                BadgeImage {
                    path,
                    row: id.to_owned(),
                }
            }),
            borderless: false,
            sheet: String::new(),
            tap: None,
        };
        let detail = presentation.detail.clone();
        let tool = presentation.tool_name.clone();
        let error = presentation.error_text.clone();
        let skeleton = presentation.is_loading_details;
        cy += draw_badge(cx, d, ctx, &badge, x, cy, w, |cx, d, x, y, w| {
            let blocks = detail_blocks(&detail, Some(&tool), error.as_deref(), &|k| t(k));
            draw_details(
                cx,
                d,
                &blocks,
                detail_is_full_bleed(&detail),
                skeleton,
                &format!("{id}:{call_id}"),
                x,
                y,
                w,
                Some(200.0),
                None,
            )
        });
        cy += if i + 1 == calls.len() { 16.0 } else { 4.0 };
    }
    cy - y
}

// ---- Turn footers ----

#[allow(clippy::too_many_arguments)]
fn footer(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    id: &str,
    rows: &Rc<Vec<StreamRow>>,
    host: Option<usize>,
    timing: Option<TurnTiming>,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let look = ctx.look;
    let p = look.palette;
    let outer = (w - 2.0 * look.list_inset).min(look.typo.content_max_width);
    let fx = x + (w - outer) * 0.5 + 8.0;
    let fw = outer - 16.0;
    let top = y + 13.0;
    let slot = 24.0;
    let mut cursor = fx;
    match host {
        None => {
            let f = &ctx.footer;
            if f.live {
                d.indicator(
                    cx,
                    Indicator::Dots {
                        rect: rect_xywh(cursor - 2.0, top + 5.0, 14.0, 14.0),
                        color: p.foreground,
                    },
                );
            } else {
                d.kit.icon(
                    cx,
                    "refresh-cw-off",
                    rect_xywh(cursor - 2.0, top, 18.0, slot),
                    18.0,
                    p.muted,
                );
            }
            cursor += 14.0 + 12.0;
            if !ctx.is_subagent {
                let r = rect_xywh(cursor, top, 24.0, slot);
                let busy = d.state.menu_busy.contains("fork:live");
                let color = if d.state.menu_open.contains("fork:live")
                    || d.state.hover_hit.as_deref()
                        == Some(&format!("{:?}", HitTarget::ForkMenu { completed: false }))
                {
                    p.foreground
                } else {
                    p.muted
                };
                d.kit.icon(
                    cx,
                    "split",
                    r,
                    16.0,
                    if busy { with_alpha(color, 0.5) } else { color },
                );
                d.canvas
                    .add_hit(r, HitTarget::ForkMenu { completed: false });
                cursor += 24.0 + 12.0;
            }
            if let (true, Some(start)) = (f.live, f.started_ms) {
                d.indicator(
                    cx,
                    Indicator::Elapsed {
                        rect: rect_xywh(cursor, top + 4.0, (fx + fw - cursor).max(1.0), slot - 4.0),
                        started_ms: start,
                        size: look.ui(13.0),
                        color: p.muted,
                        note: f
                            .pending_delivery
                            .then(|| (t("composer.delivery.agentRunning"), look.ui(12.0))),
                    },
                );
            } else if !f.live || f.pending_delivery {
                let note = t(if f.live {
                    "composer.delivery.agentRunning"
                } else {
                    "composer.delivery.stateUnavailable"
                });
                d.kit.label(
                    cx,
                    Font::Regular,
                    look.ui(12.0),
                    1.3,
                    p.muted,
                    dvec2(cursor, top + 4.0),
                    &note,
                    Some((fx + fw - cursor).max(1.0)),
                );
            }
        }
        Some(host) => {
            // CompletedTurnFooter: copy, fork menu, "Worked for …".
            let copy_key = format!("copy:{id}");
            let r = rect_xywh(cursor - 4.0, top + 1.0, 22.0, 22.0);
            let hovered = d.state.hover_hit.as_deref() == Some(copy_key.as_str());
            d.copy_icon(
                cx,
                &copy_key,
                r,
                14.0,
                if hovered { p.foreground } else { p.muted },
            );
            if !d.dry() {
                let text = d.state.response_copy.text(rows, host);
                d.canvas.add_hit(
                    r,
                    HitTarget::Copy {
                        key: copy_key,
                        text,
                    },
                );
            }
            cursor += 18.0 + 8.0;
            let entry = &rows[host].entry;
            let can_fork = !ctx.is_subagent
                && ((ctx.can_fork_cursor
                    && ctx.epoch.is_some()
                    && !get(entry, "seqEnd").is_null())
                    || !get(get(entry, "item"), "messageId").is_null());
            if can_fork {
                let r = rect_xywh(cursor, top, 24.0, slot);
                let key = format!("fork:{id}");
                let busy = d.state.menu_busy.contains(&key);
                let color = if d.state.menu_open.contains(&key)
                    || d.state.hover_hit.as_deref()
                        == Some(&format!("{:?}", HitTarget::ForkMenu { completed: true }))
                {
                    p.foreground
                } else {
                    p.muted
                };
                d.kit.icon(
                    cx,
                    "split",
                    r,
                    16.0,
                    if busy { with_alpha(color, 0.5) } else { color },
                );
                d.canvas.add_hit(r, HitTarget::ForkMenu { completed: true });
                cursor += 24.0 + 8.0;
            }
            if let Some(timing) = timing {
                let duration = timing
                    .started_at
                    .map(|start| {
                        ta(
                            "message.turnFooter.workedFor",
                            &[("duration", &format::duration(timing.completed_at - start))],
                        )
                    })
                    .unwrap_or_default();
                let stamp = format::message_timestamp(timing.completed_at);
                let primary = if duration.is_empty() {
                    stamp.clone()
                } else {
                    duration.clone()
                };
                let reveal_key = format!("stamp:{id}");
                let swap = !duration.is_empty() && !stamp.is_empty();
                let expiry = swap
                    .then(|| d.state.copy_expiry(&reveal_key, tokens::TIMESTAMP_PEEK))
                    .flatten();
                let hovered = swap && d.state.hover_hit.as_deref() == Some(reveal_key.as_str());
                let pos = dvec2(cursor, top + 4.0);
                let max = (fx + fw - cursor).max(1.0);
                let size = look.ui(13.0);
                let s = if let (Some(expiry), false) = (expiry, hovered) {
                    // Keep the input anchor stable as the temporary label swaps.
                    let a = d
                        .kit
                        .measure(cx, Font::Regular, size, 1.3, &stamp, Some(max));
                    let b = d
                        .kit
                        .measure(cx, Font::Regular, size, 1.3, &primary, Some(max));
                    d.indicator(
                        cx,
                        Indicator::Stamp {
                            rect: rect_xywh(pos.x, pos.y, max, slot - 4.0),
                            size,
                            color: p.muted,
                            primary,
                            stamp,
                            expiry,
                        },
                    );
                    dvec2(a.x.max(b.x), a.y.max(b.y))
                } else {
                    d.kit.label(
                        cx,
                        Font::Regular,
                        size,
                        1.3,
                        p.muted,
                        pos,
                        if hovered { &stamp } else { &primary },
                        Some(max),
                    )
                };
                if swap {
                    d.canvas.add_hit(
                        rect_xywh(cursor, top, s.x, slot),
                        HitTarget::Toggle(reveal_key),
                    );
                }
            }
        }
    }
    13.0 + slot + 32.0
}

// ---- Compact detail sheets ----

/// The header of a compact-layout detail sheet (tool_call_details.dart
/// `_showDetailSheet`): the badge glyph, its title, two-line medium titles
/// for overview groups, one-line semibold ones otherwise.
pub struct SheetHeader {
    pub glyph: ToolGlyph,
    pub title: String,
    pub max_lines: usize,
    pub semibold: bool,
    /// What the header's copy button puts on the clipboard; None hides it.
    pub copy: Option<String>,
}

/// Draws a badge glyph (Paseo logo or Lucide icon).
pub fn paint_glyph(cx: &mut Cx2d, d: &mut Draw, glyph: ToolGlyph, r: Rect, size: f64, color: Vec4) {
    draw_glyph(cx, d, glyph, r, size, color);
}

/// `kind` is the badge's sheet key prefix: `tool`, `thinking` or `group`.
pub fn sheet_header(ctx: &RowContext, item: &RowItem, kind: &str) -> Option<SheetHeader> {
    let RowKind::Entry { rows, index, .. } = &item.kind else {
        return None;
    };
    let row = rows.get(*index)?;
    let entry_item = get(&row.entry, "item");
    match kind {
        "thinking" => {
            let text = str_of(get(entry_item, "text"));
            Some(SheetHeader {
                glyph: ToolGlyph::Brain,
                title: t("agentStream.thinking"),
                max_lines: 1,
                semibold: true,
                copy: (!text.trim().is_empty()).then(|| text.to_owned()),
            })
        }
        "group" => {
            let calls = row.group.as_deref()?;
            let (summary, _) = tools::build_overview_summary(calls);
            Some(SheetHeader {
                glyph: ToolGlyph::Wrench,
                title: format::overview_summary(&summary),
                max_lines: 2,
                semibold: false,
                copy: None,
            })
        }
        _ => {
            let target_title = tools::paseo_tool_target_agent_id(
                str_of(get(entry_item, "name")),
                get(entry_item, "detail"),
            )
            .and_then(|agent| ctx.agent_titles.get(&agent).cloned());
            let presentation = build_tool_call_presentation(
                entry_item,
                ctx.cwd.as_deref(),
                target_title.as_deref(),
            );
            let copy = tools::detail_copy_text(&detail_blocks(
                &presentation.detail,
                Some(&presentation.tool_name),
                presentation.error_text.as_deref(),
                &|k| t(k),
            ));
            Some(SheetHeader {
                glyph: presentation.icon,
                title: presentation.display_name,
                max_lines: 1,
                semibold: true,
                copy,
            })
        }
    }
}

/// The sheet body (ToolCallDetailsContent filling `fill`, or the group's
/// calls inside a 13/8 inset); returns its height.
#[allow(clippy::too_many_arguments)]
pub fn sheet_body(
    cx: &mut Cx2d,
    d: &mut Draw,
    ctx: &RowContext,
    item: &RowItem,
    kind: &str,
    x: f64,
    y: f64,
    w: f64,
    fill: f64,
) -> f64 {
    let RowKind::Entry {
        rows,
        index,
        streaming,
        ..
    } = &item.kind
    else {
        return 0.0;
    };
    let Some(row) = rows.get(*index) else {
        return 0.0;
    };
    let entry_item = get(&row.entry, "item");
    let id = item.id.as_str();
    match kind {
        "thinking" => {
            let text = str_of(get(entry_item, "text"));
            let detail = json!({"type": "unknown", "input": text, "output": null});
            let status = if *streaming { "executing" } else { "completed" };
            let skeleton = tools::is_pending_tool_call_detail(&detail, status, &Value::Null);
            let blocks = detail_blocks(&detail, Some("thinking"), None, &|k| t(k));
            draw_details(
                cx,
                d,
                &blocks,
                false,
                skeleton,
                &format!("{id}:sheet:thinking"),
                x,
                y,
                w,
                None,
                Some(fill),
            )
        }
        "group" => {
            let calls: Vec<Rc<Value>> = row.group.clone().unwrap_or_default();
            8.0 + embedded_calls(
                cx,
                d,
                ctx,
                &format!("{id}:sheet"),
                &calls,
                x + 13.0,
                y + 8.0,
                (w - 26.0).max(1.0),
            ) + 8.0
        }
        _ => {
            let presentation = build_tool_call_presentation(entry_item, ctx.cwd.as_deref(), None);
            let blocks = detail_blocks(
                &presentation.detail,
                Some(&presentation.tool_name),
                presentation.error_text.as_deref(),
                &|k| t(k),
            );
            draw_details(
                cx,
                d,
                &blocks,
                detail_is_full_bleed(&presentation.detail),
                presentation.is_loading_details,
                &format!("{id}:sheet:detail"),
                x,
                y,
                w,
                None,
                Some(fill),
            )
        }
    }
}

#[cfg(test)]
mod cache_input_tests {
    use super::*;

    fn stream_row(seq: i64, kind: &str, text: &str, turn: &str) -> StreamRow {
        StreamRow {
            entry: Rc::new(json!({
                "seqStart": seq, "seqEnd": seq, "turnId": turn,
                "item": {"type": kind, "text": text}
            })),
            group: None,
            fold: None,
        }
    }

    fn item(kind: RowKind) -> RowItem {
        RowItem {
            id: "row".into(),
            kind,
        }
    }

    fn entry(rows: Rc<Vec<StreamRow>>, index: usize) -> RowItem {
        item(RowKind::Entry {
            rows,
            index,
            previous_tasks: None,
            footer_below: false,
            streaming: false,
            live: false,
            gap_below: 12.0,
        })
    }

    fn completed(rows: Rc<Vec<StreamRow>>, host: usize) -> RowItem {
        item(RowKind::Footer {
            rows,
            host: Some(host),
            timing: None,
        })
    }

    fn inputs<'a>(
        capabilities: &'a Value,
        titles: &'a HashMap<String, String>,
        footer: &'a LiveFooter,
    ) -> ContextInputs<'a> {
        ContextInputs {
            host_id: "host",
            agent_id: "agent",
            workspace_id: "workspace",
            cwd: Some("/project"),
            is_subagent: false,
            capabilities,
            expanded_tools: false,
            auto_expand_reasoning: false,
            find: None,
            agent_titles: titles,
            footer,
            can_fork_cursor: false,
            epoch: None,
        }
    }

    #[test]
    fn entry_equality_ignores_other_rows_but_detects_same_length_replacement() {
        let first = stream_row(1, "assistant_message", "first", "a");
        let rows = Rc::new(vec![first.clone()]);
        let original = entry(rows.clone(), 0);
        let appended = Rc::new(vec![
            first.clone(),
            stream_row(2, "assistant_message", "other", "b"),
        ]);
        assert!(original.same_content(&entry(appended, 0)));
        let reconstructed = Rc::new(vec![stream_row(1, "assistant_message", "first", "a")]);
        assert!(original.same_content(&entry(reconstructed, 0)));
        let replaced = Rc::new(vec![stream_row(1, "assistant_message", "other", "a")]);
        assert!(!original.same_content(&entry(replaced, 0)));
        let prepended = Rc::new(vec![stream_row(0, "user_message", "older", "old"), first]);
        assert!(original.same_content(&entry(prepended, 1)));
    }

    #[test]
    fn entry_equality_includes_group_members_previous_tasks_and_flags() {
        let earlier = stream_row(1, "tool_call", "first", "a").entry;
        let latest = stream_row(2, "tool_call", "last", "a").entry;
        let grouped = StreamRow {
            entry: latest.clone(),
            group: Some(vec![earlier.clone(), latest.clone()]),
            fold: None,
        };
        let original = entry(Rc::new(vec![grouped.clone()]), 0);
        let mut changed = grouped.clone();
        changed.group.as_mut().unwrap()[0] = stream_row(1, "tool_call", "changed", "a").entry;
        assert!(!original.same_content(&entry(Rc::new(vec![changed]), 0)));
        let mut changed = grouped.clone();
        changed.group.as_mut().unwrap().remove(0);
        assert!(!original.same_content(&entry(Rc::new(vec![changed]), 0)));
        let reconstructed = StreamRow {
            entry: Rc::new((*latest).clone()),
            group: Some(vec![
                Rc::new((*earlier).clone()),
                Rc::new((*latest).clone()),
            ]),
            fold: None,
        };
        assert!(original.same_content(&entry(Rc::new(vec![reconstructed]), 0)));

        for field in 0..5 {
            let mut changed = original.clone();
            let RowKind::Entry {
                previous_tasks,
                footer_below,
                streaming,
                live,
                gap_below,
                ..
            } = &mut changed.kind
            else {
                unreachable!()
            };
            match field {
                0 => *previous_tasks = Some(Rc::new(json!({"items": []}))),
                1 => *footer_below = true,
                2 => *streaming = true,
                3 => *live = true,
                _ => *gap_below = 16.0,
            }
            assert!(!original.same_content(&changed));
        }
        let mut previous = original.clone();
        let RowKind::Entry { previous_tasks, .. } = &mut previous.kind else {
            unreachable!()
        };
        *previous_tasks = Some(Rc::new(json!({"items": [{"text": "old"}]})));
        let mut changed = previous.clone();
        let RowKind::Entry { previous_tasks, .. } = &mut changed.kind else {
            unreachable!()
        };
        *previous_tasks = Some(Rc::new(json!({"items": [{"text": "new"}]})));
        assert!(!previous.same_content(&changed));
    }

    #[test]
    fn footer_equality_tracks_response_sources_boundaries_actions_and_timing() {
        let rows = Rc::new(vec![
            stream_row(1, "user_message", "question", "a"),
            stream_row(2, "assistant_message", "first", "a"),
            stream_row(3, "assistant_message", "last", "a"),
        ]);
        let original = completed(rows.clone(), 2);
        let mut appended = (*rows).clone();
        appended.push(stream_row(4, "user_message", "another question", "b"));
        appended.push(stream_row(5, "assistant_message", "another response", "b"));
        assert!(original.same_content(&completed(Rc::new(appended), 2)));

        let mut changed = (*rows).clone();
        changed[1] = stream_row(2, "assistant_message", "changed", "a");
        assert!(!original.same_content(&completed(Rc::new(changed), 2)));
        let mut boundary = (*rows).clone();
        boundary.insert(2, stream_row(10, "user_message", "new boundary", "b"));
        assert!(!original.same_content(&completed(Rc::new(boundary), 3)));
        let mut prepended = (*rows).clone();
        prepended.insert(
            0,
            stream_row(0, "assistant_message", "older response", "old"),
        );
        assert!(original.same_content(&completed(Rc::new(prepended), 3)));
        for field in ["seqEnd", "messageId"] {
            let mut changed = (*rows).clone();
            let host = Rc::make_mut(&mut changed[2].entry);
            if field == "seqEnd" {
                host[field] = Value::Null;
            } else {
                host["item"][field] = json!("message");
            }
            assert!(!original.same_content(&completed(Rc::new(changed), 2)));
        }
        let mut timed = original.clone();
        let RowKind::Footer { timing, .. } = &mut timed.kind else {
            unreachable!()
        };
        *timing = Some(TurnTiming {
            started_at: Some(10),
            completed_at: 20,
        });
        assert!(!original.same_content(&timed));
        let mut later = timed.clone();
        let RowKind::Footer { timing, .. } = &mut later.kind else {
            unreachable!()
        };
        timing.as_mut().unwrap().completed_at = 30;
        assert!(!timed.same_content(&later));
        assert!(!original.same_content(&item(RowKind::Footer {
            rows,
            host: None,
            timing: None,
        })));
    }

    #[test]
    fn permission_history_and_inset_inputs_are_exact() {
        let permission = item(RowKind::Permission {
            key: "request".into(),
            request: Rc::new(json!({"status": "pending", "question": "Allow?"})),
        });
        let rebuilt = item(RowKind::Permission {
            key: "request".into(),
            request: Rc::new(json!({"status": "pending", "question": "Allow?"})),
        });
        assert!(permission.same_content(&rebuilt));
        let changed = item(RowKind::Permission {
            key: "request".into(),
            request: Rc::new(json!({"status": "approved", "question": "Allow?"})),
        });
        assert!(!permission.same_content(&changed));
        let mut changed_key = rebuilt;
        let RowKind::Permission { key, .. } = &mut changed_key.kind else {
            unreachable!()
        };
        *key = "other".into();
        assert!(!permission.same_content(&changed_key));
        assert!(
            !item(RowKind::HistoryStart { loading: false })
                .same_content(&item(RowKind::HistoryStart { loading: true }))
        );
        assert!(!item(RowKind::Inset(12.0)).same_content(&item(RowKind::Inset(16.0))));
        let mut renamed = permission.clone();
        renamed.id = "other".into();
        assert!(!permission.same_content(&renamed));
    }

    #[test]
    fn moving_find_invalidates_only_old_and_new_targets() {
        let capabilities = Value::Null;
        let titles = HashMap::new();
        let footer = LiveFooter::default();
        let first = FindTarget {
            query: "match".into(),
            row_seq: 1,
            occurrence: 0,
        };
        let second = FindTarget {
            row_seq: 2,
            ..first.clone()
        };
        let old = ContextInputs {
            find: Some(&first),
            ..inputs(&capabilities, &titles, &footer)
        };
        let new = ContextInputs {
            find: Some(&second),
            ..old
        };
        let rows: Rc<Vec<StreamRow>> = Rc::new(
            (1..=3)
                .map(|seq| stream_row(seq, "assistant_message", "match", "a"))
                .collect(),
        );
        assert!(!old.same_for(&new, &entry(rows.clone(), 0)));
        assert!(!old.same_for(&new, &entry(rows.clone(), 1)));
        assert!(old.same_for(&new, &entry(rows.clone(), 2)));
        let occurrence = FindTarget {
            occurrence: 1,
            ..first.clone()
        };
        let next = ContextInputs {
            find: Some(&occurrence),
            ..old
        };
        assert!(!old.same_for(&next, &entry(rows.clone(), 0)));
        let query = FindTarget {
            query: "different".into(),
            ..first
        };
        let next = ContextInputs {
            find: Some(&query),
            ..old
        };
        assert!(!old.same_for(&next, &entry(rows, 0)));
    }

    #[test]
    fn context_dependencies_are_scoped_to_their_rows() {
        let capabilities = Value::Null;
        let updated_capabilities = json!({"rewind": true});
        let titles = HashMap::new();
        let updated_titles: HashMap<String, String> =
            HashMap::from([("agent".into(), "renamed".into())]);
        let footer = LiveFooter::default();
        let live_footer = LiveFooter {
            live: true,
            started_ms: Some(100),
            pending_delivery: true,
        };
        let old = inputs(&capabilities, &titles, &footer);
        let rows = Rc::new(vec![
            stream_row(1, "user_message", "question", "a"),
            stream_row(2, "assistant_message", "answer", "a"),
            stream_row(3, "tool_call", "", "a"),
            stream_row(4, "reasoning", "thinking", "a"),
        ]);
        let user = entry(rows.clone(), 0);
        let assistant = entry(rows.clone(), 1);
        let tool = entry(rows.clone(), 2);
        let reasoning = entry(rows.clone(), 3);
        let completed = completed(rows.clone(), 1);
        let live = item(RowKind::Footer {
            rows,
            host: None,
            timing: None,
        });
        let permission = item(RowKind::Permission {
            key: "request".into(),
            request: Rc::new(json!({})),
        });
        let changed = ContextInputs {
            footer: &live_footer,
            ..old
        };
        assert!(!old.same_for(&changed, &live));
        assert!(old.same_for(&changed, &completed));
        assert!(old.same_for(&changed, &assistant));
        let changed = ContextInputs {
            can_fork_cursor: true,
            epoch: Some("epoch"),
            ..old
        };
        assert!(!old.same_for(&changed, &completed));
        assert!(old.same_for(&changed, &live));
        assert!(old.same_for(&changed, &assistant));
        let changed = ContextInputs {
            capabilities: &updated_capabilities,
            ..old
        };
        assert!(!old.same_for(&changed, &user));
        assert!(old.same_for(&changed, &assistant));
        let changed = ContextInputs {
            is_subagent: true,
            ..old
        };
        assert!(!old.same_for(&changed, &user));
        assert!(!old.same_for(&changed, &completed));
        assert!(!old.same_for(&changed, &live));
        let changed = ContextInputs {
            expanded_tools: true,
            ..old
        };
        assert!(!old.same_for(&changed, &tool));
        assert!(old.same_for(&changed, &assistant));
        let changed = ContextInputs {
            agent_titles: &updated_titles,
            ..old
        };
        assert!(!old.same_for(&changed, &tool));
        let changed = ContextInputs {
            auto_expand_reasoning: true,
            ..old
        };
        assert!(!old.same_for(&changed, &reasoning));
        assert!(old.same_for(&changed, &tool));
        let changed = ContextInputs {
            cwd: Some("/other"),
            ..old
        };
        assert!(!old.same_for(&changed, &assistant));
        assert!(!old.same_for(&changed, &permission));
        for changed in [
            ContextInputs {
                host_id: "other",
                ..old
            },
            ContextInputs {
                agent_id: "other",
                ..old
            },
            ContextInputs {
                workspace_id: "other",
                ..old
            },
        ] {
            assert!(!old.same_for(&changed, &assistant));
            assert!(!old.same_for(&changed, &permission));
        }
    }
}
