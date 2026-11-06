//! tool_call_details.dart `showToolCallSheet` / `_showDetailSheet`: the
//! compact-layout detail sheet — 50% scrim, surface2 sheet with 16px top
//! radii sliding up over 250ms, the 32x4 handle, a header row (20px glyph,
//! title, close) over a rule, and the detail body scrolling inside the 95%
//! layout (overview sheets pinned to the newest call until scrolled).
//!
//! `paint_detail_sheet` is shared by ChatPanel's in-panel sheet and the
//! window-level standalone sheet below, which the desktop smoke driver opens
//! without a chat (`show_tool_call_sheet`). The shell hosts the standalone
//! layer: `handle_tool_call_sheets` before its content dispatch and
//! `draw_tool_call_sheets` in its last overlay pass.

use super::canvas::*;
use super::details::draw_details;
use super::i18n::t;
use super::paint::{Draw, Look, MOBILE, RowState, now_seconds};
use super::rows::{SheetHeader, paint_glyph};
use super::tools::{self, detail_blocks, detail_is_full_bleed};
use makepad_widgets::*;
use serde_json::Value;
use std::cell::RefCell;

/// showModalBottomSheet's enter transition.
pub const OPEN_SECS: f64 = 0.25;

/// The sheet body painter: (cx, draw, x, y, width, fill height) → height.
pub type SheetBody<'a> = &'a dyn Fn(&mut Cx2d, &mut Draw, f64, f64, f64, f64) -> f64;

/// Paints the sheet over `rect`; `progress` is the open animation (0..1).
/// Registers `Chrome("scrim")`, `Chrome("sheet:body")`, `Chrome("sheet:close")`
/// and the body's `ScrollY("sheet:y")`. Returns the sheet rect.
pub fn paint_detail_sheet(
    cx: &mut Cx2d,
    d: &mut Draw,
    rect: Rect,
    header: &SheetHeader,
    progress: f64,
    pin_to_end: bool,
    body: SheetBody,
) -> Rect {
    let p = d.look.palette;
    let eased = 1.0 - (1.0 - progress.clamp(0.0, 1.0)).powi(3);
    d.kit.rect(cx, rect, with_alpha(rgba(0xff000000), (0.5 * eased) as f32), 0.0);
    d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
    // DraggableScrollableSheet: laid out at 95%, resting at the 60% snap.
    let full = rect.size.y * 0.95;
    let rest = rect.size.y * 0.6;
    let top = rect.pos.y + rect.size.y - rest * eased;
    let sheet = rect_xywh(rect.pos.x, top, rect.size.x, full);
    d.kit.rect_full(cx, sheet, p.surface2, vec4(16.0, 16.0, 0.0, 0.0), TRANSPARENT, 0.0);
    d.canvas.add_hit(sheet, HitTarget::Chrome("sheet:body".into()));
    // Gorhom handle: 32x4 zinc-600 with 10px insets.
    d.kit.rect(cx, rect_xywh(sheet.center().x - 16.0, sheet.pos.y + 10.0, 32.0, 4.0), rgba(0xff52525b), 2.0);
    let header_y = sheet.pos.y + 24.0;
    paint_glyph(cx, d, header.glyph, rect_xywh(sheet.pos.x + 16.0, header_y + 12.0, 20.0, 36.0), 20.0, p.foreground);
    let size = d.look.ui(14.0);
    let font = if header.semibold { Font::Semibold } else { Font::Medium };
    let laid = d.kit.layout_clamped(cx, font, size, 1.3, &header.title, (sheet.size.x - 16.0 - 28.0 - 52.0).max(1.0), header.max_lines);
    let th = laidout_size(&laid).y;
    d.kit.draw_laidout(cx, font, p.foreground, dvec2(sheet.pos.x + 44.0, header_y + 12.0 + (36.0 - th) * 0.5), &laid);
    let close = rect_xywh(sheet.pos.x + sheet.size.x - 16.0 - 36.0, header_y + 12.0, 36.0, 36.0);
    d.kit.icon(cx, "x", close, 20.0, p.muted);
    d.canvas.add_hit(close, HitTarget::Chrome("sheet:close".into()));
    d.kit.rect(cx, rect_xywh(sheet.pos.x, header_y + 60.0, sheet.size.x, 1.0), p.border, 0.0);
    // The body scrolls inside the visible part of the 95% layout.
    let body_top = header_y + 61.0;
    let view = rect_xywh(sheet.pos.x, body_top, sheet.size.x, (rect.pos.y + rect.size.y - body_top).max(1.0));
    let fill = (full - 85.0).max(0.0);
    let content = d.measure(|d| body(cx, d, view.pos.x, 0.0, view.size.x, fill));
    let key = "sheet:y".to_owned();
    let max_y = (content - view.size.y).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let pinned = pin_to_end && d.state.expanded.get("sheet:detached").copied() != Some(true);
    let sy = if pinned { max_y } else { d.state.scroll_y.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_y) };
    d.state.scroll_y.insert(key.clone(), sy);
    cx.begin_turtle(Walk { abs_pos: Some(view.pos), margin: Margin::default(), width: Size::Fixed(view.size.x), height: Size::Fixed(view.size.y) }, Layout::default());
    d.canvas.push_clip(view);
    body(cx, d, view.pos.x, view.pos.y - sy, view.size.x, fill);
    d.canvas.pop_clip();
    cx.end_turtle();
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(key));
    }
    if progress < 1.0 {
        d.animating = true;
    }
    sheet
}

/// Scrolls the sheet body; returns true when it moved.
pub fn scroll_sheet(state: &mut RowState, canvas: &Canvas, abs: DVec2, delta: DVec2) -> bool {
    let rel = abs - canvas.origin;
    let Some(key) = canvas.scroll_region_at(rel, true).map(str::to_owned) else { return false };
    let max = state.extent_y.get(&key).copied().unwrap_or(0.0);
    let current = state.scroll_y.get(&key).copied().unwrap_or(0.0);
    let next = (current + delta.y).clamp(0.0, max);
    if (next - current).abs() < 0.01 {
        return false;
    }
    state.scroll_y.insert(key.clone(), next);
    if key == "sheet:y" {
        state.expanded.insert("sheet:detached".into(), true);
    }
    true
}

// ── Standalone window-level sheets ──────────────────────────────────────

struct Standalone {
    id: u64,
    window: String,
    title: String,
    detail: Value,
    opened: f64,
    /// Drawn fully open (the open animation finished).
    settled: bool,
    state: RowState,
}

#[derive(Default)]
struct Layer {
    kit: Option<ChatKit>,
    list: Option<DrawList2d>,
    canvas: Canvas,
    area: Area,
    sheets: Vec<Standalone>,
    next_id: u64,
    next_frame: NextFrame,
    touch_scroll: Option<(DVec2, bool)>,
}

thread_local! {
    static LAYER: RefCell<Layer> = RefCell::new(Layer::default());
}

/// Opens the tool-call detail sheet for `display_name` / `detail` over the
/// window `window_id` (Flutter `showToolCallSheet` with the tool's glyph).
/// Returns its id.
pub fn show_tool_call_sheet(cx: &mut Cx, window_id: &str, display_name: &str, detail: &Value) -> u64 {
    let id = LAYER.with(|layer| {
        let mut layer = layer.borrow_mut();
        layer.next_id += 1;
        let id = layer.next_id;
        layer.sheets.push(Standalone {
            id,
            window: window_id.to_owned(),
            title: display_name.to_owned(),
            detail: detail.clone(),
            opened: now_seconds(),
            settled: false,
            state: RowState::default(),
        });
        id
    });
    cx.redraw_all();
    id
}

/// Whether the sheet finished opening and was drawn fully open.
pub fn tool_call_sheet_settled(id: u64) -> bool {
    LAYER.with(|layer| layer.borrow().sheets.iter().any(|s| s.id == id && s.settled))
}

pub fn close_tool_call_sheet(cx: &mut Cx, id: u64) {
    LAYER.with(|layer| layer.borrow_mut().sheets.retain(|s| s.id != id));
    cx.redraw_all();
}

fn top_index(layer: &Layer, window_id: &str) -> Option<usize> {
    layer.sheets.iter().rposition(|s| s.window == window_id)
}

/// Shell hook, before content dispatch: taps (close, scrim), wheel scrolling
/// and Escape for the window's top standalone sheet. Returns true while one
/// is open (the content below is modal-locked).
pub fn handle_tool_call_sheets(cx: &mut Cx, event: &Event, window_id: &str) -> bool {
    LAYER.with(|cell| {
        let mut layer = cell.borrow_mut();
        let layer = &mut *layer;
        if layer.next_frame.is_event(event).is_some() {
            cx.redraw_area(layer.area);
        }
        let Some(index) = top_index(layer, window_id) else { return false };
        let mut close = false;
        if let Event::KeyDown(ke) = event {
            close |= ke.key_code == KeyCode::Escape;
        }
        if let Event::Scroll(se) = event
            && scroll_sheet(&mut layer.sheets[index].state, &layer.canvas, se.abs, se.scroll)
        {
            cx.redraw_area(layer.area);
        }
        if let Event::BackPressed { handled } = event
            && !handled.get()
        {
            handled.set(true);
            close = true;
        }
        match event.hits(cx, layer.area) {
            Hit::FingerDown(e) => {
                layer.touch_scroll = e.device.is_touch().then_some((e.abs, false));
            }
            Hit::FingerMove(e) => {
                if let Some((last, dragging)) = layer.touch_scroll {
                    let dragging = dragging || e.move_distance() >= 5.0;
                    layer.touch_scroll = Some((e.abs, dragging));
                    if dragging && scroll_sheet(&mut layer.sheets[index].state, &layer.canvas, e.abs_start, last - e.abs) {
                        cx.redraw_area(layer.area);
                    }
                }
            }
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hit = layer.canvas.hit_at(e.abs - layer.canvas.origin).map(super::chat_row::hover_key);
                let state = &mut layer.sheets[index].state;
                if state.hover_hit != hit {
                    state.hover_hit = hit.clone();
                    cx.redraw_area(layer.area);
                }
                cx.set_cursor(if matches!(hit.as_deref(), Some("sheet:close")) { MouseCursor::Hand } else { MouseCursor::Default });
            }
            Hit::FingerUp(e) if e.is_over && e.was_tap() => {
                layer.touch_scroll = None;
                let target = layer.canvas.hit_at(e.abs - layer.canvas.origin).cloned();
                match target {
                    Some(HitTarget::Chrome(id)) if id == "sheet:close" || id == "scrim" => close = true,
                    Some(HitTarget::Copy { key, text }) => {
                        cx.copy_to_clipboard(&text);
                        layer.sheets[index].state.copied.insert(key, now_seconds());
                        cx.redraw_area(layer.area);
                    }
                    Some(HitTarget::Toggle(key)) => {
                        layer.sheets[index].state.toggle(&key);
                        cx.redraw_area(layer.area);
                    }
                    _ => {}
                }
            }
            Hit::FingerUp(_) => layer.touch_scroll = None,
            _ => {}
        }
        if close {
            layer.touch_scroll = None;
            layer.sheets.remove(index);
            cx.redraw_all();
        }
        true
    })
}

/// Shell hook, in the window's last overlay pass: draws its standalone sheets.
pub fn draw_tool_call_sheets(cx: &mut Cx2d, window_id: &str, window: Rect) {
    LAYER.with(|cell| {
        let mut layer = cell.borrow_mut();
        let layer = &mut *layer;
        if top_index(layer, window_id).is_none() {
            return;
        }
        if layer.kit.is_none() {
            // Reuse the canvas DSL already expanded for ChatPanel. A standalone,
            // otherwise unreferenced module is registered but not expanded by Makepad.
            layer.kit = Some(ChatKit::new_local(cx));
        }
        if layer.list.is_none() {
            layer.list = Some(DrawList2d::new(cx));
        }
        let (Some(kit), Some(list)) = (layer.kit.as_mut(), layer.list.as_mut()) else { return };
        let palette = crate::theme::current_palette();
        let typo = crate::theme::current_typography();
        let look = Look {
            palette,
            typo,
            compact: crate::common::is_compact_width(window.size.x),
            mobile: MOBILE,
            list_inset: if window.size.x < 720.0 { 12.0 } else { 16.0 },
            syntax: paseo_highlight::SyntaxThemeId::One,
        };
        list.begin_always(cx);
        // The layer's capture area is its own full-window turtle (stable across
        // redraws, unlike the first instance of a primitive drawn many times).
        cx.begin_turtle(Walk::abs_rect(window), Layout::default());
        kit.begin_layer(cx);
        layer.canvas.begin(window.pos);
        let now = now_seconds();
        let mut animating = false;
        for sheet in layer.sheets.iter_mut().filter(|s| s.window == window_id) {
            let progress = ((now - sheet.opened) / OPEN_SECS).clamp(0.0, 1.0);
            let glyph = tools::resolve_tool_call_icon(&sheet.title, Some(&sheet.detail), false);
            let header = SheetHeader { glyph, title: sheet.title.clone(), max_lines: 1, semibold: true };
            let detail = sheet.detail.clone();
            let tool_name = sheet.title.clone();
            let mut d = Draw { kit: &mut *kit, canvas: &mut layer.canvas, look, state: &mut sheet.state, now, area: layer.area, animating: false };
            paint_detail_sheet(cx, &mut d, window, &header, progress, false, &|cx, d, x, y, w, fill| {
                let blocks = detail_blocks(&detail, Some(&tool_name), None, &|k| t(k));
                let skeleton = tools::is_pending_tool_call_detail(&detail, "completed", &Value::Null);
                draw_details(cx, d, &blocks, detail_is_full_bleed(&detail), skeleton, "standalone:sheet:detail", x, y, w, None, Some(fill))
            });
            animating |= d.animating;
            // Settled once a frame was drawn with the open animation finished.
            if progress >= 1.0 {
                sheet.settled = true;
            }
        }
        cx.end_turtle_with_area(&mut layer.area);
        list.end(cx);
        if animating || layer.sheets.iter().any(|s| s.window == window_id && !s.settled) {
            layer.next_frame = cx.new_next_frame();
        }
    });
}
