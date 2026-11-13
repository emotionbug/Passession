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
use crate::common::overlay::{take_dismissal, TapTarget};
use makepad_widgets::*;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;

/// showModalBottomSheet's enter transition.
pub const OPEN_SECS: f64 = 0.25;

/// The sheet body painter: (cx, draw, x, y, width, fill height) → height.
pub type SheetBody<'a> = &'a dyn Fn(&mut Cx2d, &mut Draw, f64, f64, f64, f64) -> f64;

/// Paints the sheet over `rect`; `progress` is the open animation (0..1).
/// `bottom_inset` is the system navigation inset inside `rect`'s bottom edge:
/// the sheet's background covers it, the body ends above it.
/// Registers `Chrome("scrim")`, `Chrome("sheet:body")`, `Chrome("sheet:close")`
/// and the body's `ScrollY("sheet:y")`. Returns the sheet rect.
#[allow(clippy::too_many_arguments)]
pub fn paint_detail_sheet(
    cx: &mut Cx2d,
    d: &mut Draw,
    rect: Rect,
    bottom_inset: f64,
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
    let measurement = crate::common::widgets::RowMetrics::new(0.0, 0.0, 0.0)
        .layout(sheet, 16.0, 20.0, 36.0, 8.0);
    let size = d.look.ui(14.0);
    let font = if header.semibold { Font::Semibold } else { Font::Medium };
    let laid = d.kit.layout_clamped(cx, font, size, 1.3, &header.title, measurement.title.size.x, header.max_lines);
    let metrics = crate::common::widgets::RowMetrics::new(laidout_size(&laid).y, 0.0, 0.0);
    let header_height = metrics.height(60.0, 12.0, 36.0);
    let slots = metrics.layout(rect_xywh(sheet.pos.x, header_y, sheet.size.x, header_height), 16.0, 20.0, 36.0, 8.0);
    paint_glyph(cx, d, header.glyph, rect_xywh(slots.leading.pos.x, header_y + (header_height - 36.0) * 0.5, 20.0, 36.0), 20.0, p.foreground);
    d.kit.draw_laidout(cx, font, p.foreground, slots.title.pos, &laid);
    let close = rect_xywh(slots.trailing.pos.x, header_y + (header_height - 36.0) * 0.5, 36.0, 36.0);
    d.kit.icon(cx, "x", close, 20.0, p.muted);
    d.canvas.add_hit(close, HitTarget::Chrome("sheet:close".into()));
    d.kit.rect(cx, rect_xywh(sheet.pos.x, header_y + header_height, sheet.size.x, 1.0), p.border, 0.0);
    // The body scrolls inside the visible part of the 95% layout.
    let body_top = header_y + header_height + 1.0;
    let view = rect_xywh(sheet.pos.x, body_top, sheet.size.x, (rect.pos.y + rect.size.y - bottom_inset - body_top).max(1.0));
    let fill = (full - 24.0 - header_height - 1.0).max(0.0);
    let content = d.measure(|d| body(cx, d, view.pos.x, 0.0, view.size.x, fill));
    let key = "sheet:y".to_owned();
    let max_y = (content - view.size.y).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let pinned = pin_to_end && d.state.expanded.get("sheet:detached").copied() != Some(true);
    let sy = if pinned { max_y } else { d.state.scroll_y.get(&key).copied().unwrap_or(0.0).clamp(0.0, max_y) };
    d.state.scroll_y.insert(key.clone(), sy);
    // Enclosing regions register before their content: the deepest one wins.
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(key));
    }
    cx.begin_turtle(Walk { abs_pos: Some(view.pos), margin: Margin::default(), width: Size::Fixed(view.size.x), height: Size::Fixed(view.size.y) }, Layout::default());
    d.canvas.push_clip(view);
    body(cx, d, view.pos.x, view.pos.y - sy, view.size.x, fill);
    d.canvas.pop_clip();
    cx.end_turtle();
    if progress < 1.0 {
        d.animating = true;
    }
    sheet
}

/// Scrolls the sheet body; returns true when it moved.
pub fn scroll_sheet(state: &mut RowState, canvas: &Canvas, abs: DVec2, delta: DVec2) -> bool {
    let rel = abs - canvas.origin;
    // The deepest region that can still move takes the delta.
    for key in canvas.scroll_regions_at(rel, true) {
        let max = state.extent_y.get(key).copied().unwrap_or(0.0);
        let current = state.scroll_y.get(key).copied().unwrap_or(0.0);
        let next = (current + delta.y).clamp(0.0, max);
        if (next - current).abs() < 0.01 {
            continue;
        }
        state.scroll_y.insert(key.to_owned(), next);
        if key == "sheet:y" {
            state.expanded.insert("sheet:detached".into(), true);
        }
        return true;
    }
    false
}

// ── Standalone window-level sheets ──────────────────────────────────────

struct Standalone {
    id: u64,
    title: String,
    detail: Value,
    opened: f64,
    /// Drawn fully open (the open animation finished).
    settled: bool,
    state: RowState,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    tap: TapTarget<HitTarget>,
}

#[derive(Default)]
struct Layer {
    kit: Option<ChatKit>,
    list: Option<DrawList2d>,
    canvas: Canvas,
    area: Area,
    sheets: Vec<Standalone>,
    next_frame: NextFrame,
    painted_top: Option<u64>,
}

thread_local! {
    static LAYER: RefCell<HashMap<String, Layer>> = RefCell::new(HashMap::new());
}

/// Opens the tool-call detail sheet for `display_name` / `detail` over the
/// window `window_id` (Flutter `showToolCallSheet` with the tool's glyph).
/// Returns its id.
pub fn show_tool_call_sheet(cx: &mut Cx, window_id: &str, display_name: &str, detail: &Value) -> u64 {
    let id = LAYER.with(|layer| {
        let mut layers = layer.borrow_mut();
        let layer = layers.entry(window_id.to_owned()).or_default();
        if let Some(parent) = layer.sheets.last_mut() {
            parent.touch_scroll.reset();
            parent.tap.cancel();
        }
        let id = crate::common::overlay::next_id();
        layer.sheets.push(Standalone {
            id,
            title: display_name.to_owned(),
            detail: detail.clone(),
            opened: now_seconds(),
            settled: false,
            state: RowState::default(),
            touch_scroll: Default::default(),
            tap: TapTarget::default(),
        });
        id
    });
    cx.redraw_all();
    id
}

/// Whether the sheet finished opening and was drawn fully open.
pub fn tool_call_sheet_settled(id: u64) -> bool {
    LAYER.with(|layers| layers.borrow().values().any(|layer| layer.sheets.iter().any(|s| s.id == id && s.settled)))
}

pub fn close_tool_call_sheet(cx: &mut Cx, id: u64) {
    LAYER.with(|layers| {
        for layer in layers.borrow_mut().values_mut() {
            layer.sheets.retain(|sheet| sheet.id != id);
        }
    });
    cx.redraw_all();
}

fn top_index(layer: &Layer) -> Option<usize> {
    layer.sheets.len().checked_sub(1)
}

/// Shell hook, before content dispatch: taps (close, scrim), wheel scrolling
/// and Escape for the window's top standalone sheet. Returns true while one
/// is open (the content below is modal-locked).
pub fn handle_tool_call_sheets(cx: &mut Cx, event: &Event, window_id: &str) -> bool {
    LAYER.with(|cell| {
        let mut layers = cell.borrow_mut();
        let Some(layer) = layers.get_mut(window_id) else { return false };
        if layer.next_frame.is_event(event).is_some() {
            cx.redraw_area(layer.area);
        }
        let Some(index) = top_index(layer) else { return false };
        if take_dismissal(event) {
            layer.sheets.remove(index);
            cx.redraw_all();
            return true;
        }
        if matches!(event, Event::BackPressed { .. } | Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, .. })) {
            return true;
        }
        if matches!(event, Event::TextHandle(_) | Event::Pause | Event::AppLostFocus)
            || matches!(event, Event::TouchUpdate(update) if update.cancelled)
        {
            layer.sheets[index].touch_scroll.reset();
            layer.sheets[index].tap.cancel();
        }
        // A newly pushed/revealed owner cannot reuse the previous owner's hit map.
        if layer.painted_top != Some(layer.sheets[index].id) {
            return true;
        }
        let mut close = false;
        if let Event::Scroll(se) = event {
            layer.sheets[index].touch_scroll.reset();
            layer.sheets[index].tap.cancel();
            if scroll_sheet(&mut layer.sheets[index].state, &layer.canvas, se.abs, se.scroll) {
                cx.redraw_area(layer.area);
            }
        }
        let hit = event.hits(cx, layer.area);
        let scroll_start = match &hit {
            Hit::FingerDown(e) => layer.canvas.scroll_region_at(e.abs - layer.canvas.origin, true).is_some(),
            _ => true,
        };
        if !scroll_start {
            layer.sheets[index].touch_scroll.reset();
        }
        if scroll_start && let Some(delta) = layer.sheets[index].touch_scroll.handle_event(cx, event, &hit) {
            layer.sheets[index].tap.cancel();
            let origin = layer.sheets[index].touch_scroll.origin();
            if scroll_sheet(&mut layer.sheets[index].state, &layer.canvas, origin, dvec2(0.0, delta)) {
                cx.redraw_area(layer.area);
            } else {
                layer.sheets[index].touch_scroll.stop_at_edge();
            }
        }
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hit = layer.canvas.hit_at(e.abs - layer.canvas.origin).map(super::chat_row::hover_key);
                let state = &mut layer.sheets[index].state;
                if state.hover_hit != hit {
                    state.hover_hit = hit.clone();
                    cx.redraw_area(layer.area);
                }
                cx.set_cursor(if matches!(hit.as_deref(), Some("sheet:close")) { MouseCursor::Hand } else { MouseCursor::Default });
            }
            Hit::FingerDown(e) => {
                layer.sheets[index].tap.press(layer.canvas.hit_at(e.abs - layer.canvas.origin).cloned());
            }
            Hit::FingerLongPress(_) => layer.sheets[index].tap.cancel(),
            Hit::FingerUp(e) => {
                let target = layer.canvas.hit_at(e.abs - layer.canvas.origin).cloned();
                let released = layer.sheets[index].tap.release(target, e.is_over && e.was_tap() && !e.has_long_press_occurred);
                match released {
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
            _ => {}
        }
        if close {
            layer.sheets.remove(index);
            cx.redraw_all();
        }
        true
    })
}

/// Shell hook, in the window's last overlay pass: draws its standalone sheets.
pub fn draw_tool_call_sheets(cx: &mut Cx2d, window_id: &str, window: Rect) {
    LAYER.with(|cell| {
        let mut layers = cell.borrow_mut();
        let Some(layer) = layers.get_mut(window_id) else { return };
        if top_index(layer).is_none() {
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
        let now = now_seconds();
        let mut animating = false;
        for sheet in &mut layer.sheets {
            // Lower sheets paint normally, but only the top sheet contributes input.
            layer.canvas.begin(window.pos);
            let progress = ((now - sheet.opened) / OPEN_SECS).clamp(0.0, 1.0);
            let glyph = tools::resolve_tool_call_icon(&sheet.title, Some(&sheet.detail), false);
            let header = SheetHeader { glyph, title: sheet.title.clone(), max_lines: 1, semibold: true };
            let detail = sheet.detail.clone();
            let tool_name = sheet.title.clone();
            let mut d = Draw { kit: &mut *kit, canvas: &mut layer.canvas, look, state: &mut sheet.state, now, area: layer.area, animating: false };
            // An overlay root: it reaches the window's bottom edge and owns the system-bar padding.
            let bottom_inset = crate::shell::platform::safe_insets().bottom;
            paint_detail_sheet(cx, &mut d, window, bottom_inset, &header, progress, false, &|cx, d, x, y, w, fill| {
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
        layer.painted_top = layer.sheets.last().map(|sheet| sheet.id);
        if animating || layer.sheets.iter().any(|s| !s.settled) {
            layer.next_frame = cx.new_next_frame();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sheet(id: u64) -> Standalone {
        Standalone {
            id, title: String::new(), detail: Value::Null, opened: 0.0,
            settled: false, state: RowState::default(), touch_scroll: Default::default(),
            tap: TapTarget::default(),
        }
    }

    #[test]
    fn dismissal_closes_only_top_sheet_in_its_window_and_ignores_repeats() {
        LAYER.with(|layers| {
            *layers.borrow_mut() = HashMap::from([
                ("main".into(), Layer { sheets: vec![sheet(1), sheet(2)], ..Default::default() }),
                ("other".into(), Layer { sheets: vec![sheet(3)], ..Default::default() }),
            ]);
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let escape = KeyEvent { key_code: KeyCode::Escape, ..Default::default() };
        assert!(handle_tool_call_sheets(&mut cx, &Event::KeyDown(escape), "main"));
        assert!(handle_tool_call_sheets(&mut cx, &Event::KeyDown(KeyEvent { is_repeat: true, ..escape }), "main"));
        LAYER.with(|layers| {
            let layers = layers.borrow();
            assert_eq!(layers["main"].sheets.iter().map(|sheet| sheet.id).collect::<Vec<_>>(), vec![1]);
            assert_eq!(layers["other"].sheets.iter().map(|sheet| sheet.id).collect::<Vec<_>>(), vec![3]);
        });
        let back = Event::BackPressed { handled: Cell::new(false) };
        assert!(handle_tool_call_sheets(&mut cx, &back, "main"));
        assert!(!handle_tool_call_sheets(&mut cx, &back, "main"));
        assert!(!take_dismissal(&back));
        LAYER.with(|layers| layers.borrow_mut().clear());
    }
}
