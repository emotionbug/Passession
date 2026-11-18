//! tool_call_details.dart `showToolCallSheet` / `_showDetailSheet`: the
//! compact-layout detail sheet — 50% scrim, surface2 sheet with 16px top
//! radii, the 32x4 handle, a header row (20px glyph,
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
use crate::common::overlay::{self, DrawLayerScrim, TapTarget, take_dismissal};
use crate::common::transition::{Intent, Transition};
use makepad_widgets::*;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
pub struct SheetMotion {
    pub open: bool,
    progress: Transition,
    pub offset: DVec2,
    pub scrim: Area,
    cover: Area,
}

impl SheetMotion {
    pub fn set_open(&mut self, cx: &mut Cx, open: bool) {
        if !open && self.open && self.cover.is_valid(cx) {
            self.cover.set_rect(cx, &Rect::default());
            crate::common::clock::finish_draw(cx);
        }
        self.open = open;
        self.progress.retarget(
            cx,
            if open { 1.0 } else { 0.0 },
            self.intent().resolve().spec,
        );
    }

    pub fn is_moving(&self) -> bool {
        self.progress.is_moving()
    }

    pub fn advance(&mut self, cx: &mut Cx, event: &Event) -> bool {
        self.progress.advance(cx, event)
    }

    fn intent(&self) -> Intent {
        if self.open {
            Intent::OpenSheet
        } else {
            Intent::CloseSheet
        }
    }

    pub fn mark_cover(&mut self, cx: &mut Cx2d, bounds: Rect, opaque: bool) {
        self.cover = if opaque {
            crate::common::clock::occlude(cx, Self::cover_rect(bounds))
        } else {
            Area::Empty
        };
        self.place_cover(cx, bounds);
    }

    fn cover_rect(bounds: Rect) -> Rect {
        let start = bounds.size.y * 0.4 + 16.0;
        rect_xywh(
            bounds.pos.x,
            bounds.pos.y + start,
            bounds.size.x,
            bounds.size.y - start,
        )
    }

    fn place_cover(&self, cx: &mut Cx, bounds: Rect) {
        if self.cover.is_valid(cx) {
            let rect = if self.open && !self.is_moving() {
                Self::cover_rect(bounds)
            } else {
                Rect::default()
            };
            self.cover.set_rect(cx, &rect);
        }
    }

    pub fn finish_open(&self, cx: &mut Cx, bounds: Rect) {
        if self.open && !self.is_moving() && self.cover.is_valid(cx) {
            self.place_cover(cx, bounds);
            crate::common::clock::finish_draw(cx);
        }
    }

    pub fn compose(&mut self, cx: &mut Cx, list: &DrawList2d, bounds: Rect) {
        let size = dvec2(bounds.size.x, bounds.size.y * 0.6);
        let frame = self.intent().resolve().sample(self.progress.value(), size);
        self.offset = frame.incoming.offset;
        overlay::place_layer(
            cx,
            list,
            Rect {
                pos: self.offset,
                size,
            },
            1.0,
        );
        overlay::update_scrim(cx, self.scrim, (0.5 * frame.scrim) as f32);
    }

    pub fn draw_scrim(&mut self, cx: &mut Cx2d, draw: &mut DrawLayerScrim, bounds: Rect) {
        let frame = self
            .intent()
            .resolve()
            .sample(self.progress.value(), bounds.size);
        draw.color = vec4(0.0, 0.0, 0.0, (0.5 * frame.scrim) as f32);
        draw.draw_abs(cx, bounds);
        self.scrim = draw.area();
    }
}

/// The header copy button's key in the sheet's `copied` state.
const COPY_KEY: &str = "sheet:copy";

/// The sheet body painter: (cx, draw, x, y, width, fill height) → height.
pub type SheetBody<'a> = &'a dyn Fn(&mut Cx2d, &mut Draw, f64, f64, f64, f64) -> f64;

/// Paints the sheet at its settled geometry over `rect`.
/// `bottom_inset` is the system navigation inset inside `rect`'s bottom edge:
/// the sheet's background covers it, the body ends above it.
/// Registers `Chrome("sheet:body")`, `Chrome("sheet:close")`,
/// the header's `Copy` when it has text, and the body's `ScrollY("sheet:y")`.
/// Returns the sheet rect.
#[allow(clippy::too_many_arguments)]
pub fn paint_detail_sheet(
    cx: &mut Cx2d,
    d: &mut Draw,
    rect: Rect,
    bottom_inset: f64,
    header: &SheetHeader,
    pin_to_end: bool,
    body: SheetBody,
) -> Rect {
    let p = d.look.palette;
    d.canvas.add_hit(rect, HitTarget::Chrome("scrim".into()));
    // DraggableScrollableSheet: laid out at 95%, resting at the 60% snap.
    let full = rect.size.y * 0.95;
    let rest = rect.size.y * 0.6;
    let top = rect.pos.y + rect.size.y - rest;
    let sheet = rect_xywh(rect.pos.x, top, rect.size.x, full);
    d.kit.rect_full(
        cx,
        sheet,
        p.surface2,
        vec4(16.0, 16.0, 0.0, 0.0),
        TRANSPARENT,
        0.0,
    );
    d.canvas
        .add_hit(sheet, HitTarget::Chrome("sheet:body".into()));
    // Gorhom handle: 32x4 zinc-600 with 10px insets.
    d.kit.rect(
        cx,
        rect_xywh(sheet.center().x - 16.0, sheet.pos.y + 10.0, 32.0, 4.0),
        rgba(0xff52525b),
        2.0,
    );
    let header_y = sheet.pos.y + 24.0;
    // Trailing actions are 36px each: copy (when there is text) and close.
    let actions = if header.copy.is_some() { 72.0 } else { 36.0 };
    let measurement = crate::common::widgets::RowMetrics::new(0.0, 0.0, 0.0)
        .layout(sheet, 16.0, 20.0, actions, 8.0);
    let size = d.look.ui(14.0);
    let font = if header.semibold {
        Font::Semibold
    } else {
        Font::Medium
    };
    let laid = d.kit.layout_clamped(
        cx,
        font,
        size,
        1.3,
        &header.title,
        measurement.title.size.x,
        header.max_lines,
    );
    let metrics = crate::common::widgets::RowMetrics::new(laidout_size(&laid).y, 0.0, 0.0);
    let header_height = metrics.height(60.0, 12.0, 36.0);
    let slots = metrics.layout(
        rect_xywh(sheet.pos.x, header_y, sheet.size.x, header_height),
        16.0,
        20.0,
        actions,
        8.0,
    );
    paint_glyph(
        cx,
        d,
        header.glyph,
        rect_xywh(
            slots.leading.pos.x,
            header_y + (header_height - 36.0) * 0.5,
            20.0,
            36.0,
        ),
        20.0,
        p.foreground,
    );
    d.kit
        .draw_laidout(cx, font, p.foreground, slots.title.pos, &laid);
    if let Some(text) = &header.copy {
        let copy = rect_xywh(
            slots.trailing.pos.x,
            header_y + (header_height - 36.0) * 0.5,
            36.0,
            36.0,
        );
        d.copy_icon(cx, COPY_KEY, copy, 18.0, p.muted);
        d.canvas.add_hit(
            copy,
            HitTarget::Copy {
                key: COPY_KEY.into(),
                text: text.clone().into(),
            },
        );
    }
    let close = rect_xywh(
        slots.trailing.pos.x + actions - 36.0,
        header_y + (header_height - 36.0) * 0.5,
        36.0,
        36.0,
    );
    d.kit.icon(cx, "x", close, 20.0, p.muted);
    d.canvas
        .add_hit(close, HitTarget::Chrome("sheet:close".into()));
    d.kit.rect(
        cx,
        rect_xywh(sheet.pos.x, header_y + header_height, sheet.size.x, 1.0),
        p.border,
        0.0,
    );
    // The body scrolls inside the visible part of the 95% layout.
    let body_top = header_y + header_height + 1.0;
    let view = rect_xywh(
        sheet.pos.x,
        body_top,
        sheet.size.x,
        (rect.pos.y + rect.size.y - bottom_inset - body_top).max(1.0),
    );
    let fill = (full - 24.0 - header_height - 1.0).max(0.0);
    let content = d.measure(|d| body(cx, d, view.pos.x, 0.0, view.size.x, fill));
    let key = "sheet:y".to_owned();
    let max_y = (content - view.size.y).max(0.0);
    d.state.extent_y.insert(key.clone(), max_y);
    let pinned = pin_to_end && d.state.expanded.get("sheet:detached").copied() != Some(true);
    let sy = if pinned {
        max_y
    } else {
        d.state
            .scroll_y
            .get(&key)
            .copied()
            .unwrap_or(0.0)
            .clamp(0.0, max_y)
    };
    d.state.scroll_y.insert(key.clone(), sy);
    // Enclosing regions register before their content: the deepest one wins.
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(key.clone()));
    }
    cx.begin_turtle(
        Walk {
            abs_pos: Some(view.pos),
            margin: Margin::default(),
            width: Size::Fixed(view.size.x),
            height: Size::Fixed(view.size.y),
        },
        Layout::default(),
    );
    d.canvas.push_clip(view);
    body(cx, d, view.pos.x, view.pos.y - sy, view.size.x, fill);
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
    sheet
}

/// Scrolls the sheet body; returns true when it moved.
pub fn scroll_sheet(state: &mut RowState, canvas: &Canvas, abs: DVec2, delta: DVec2) -> bool {
    let rel = abs - canvas.origin;
    if delta.x.abs() > delta.y.abs() {
        for key in canvas.scroll_regions_at(rel, false) {
            let max = state.extent_x.get(key).copied().unwrap_or(0.0);
            let current = state.scroll_x.get(key).copied().unwrap_or(0.0);
            let next = (current + delta.x).clamp(0.0, max);
            if (next - current).abs() >= 0.01 {
                state.scroll_x.insert(key.to_owned(), next);
                return true;
            }
        }
    }
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
    motion: SheetMotion,
    canvas: Canvas,
    state: RowState,
    paint: Option<DrawList2d>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    bar_drag: Option<super::scrollbar::BarDrag>,
    tap: TapTarget<HitTarget>,
}

#[derive(Default)]
struct Layer {
    kit: Option<ChatKit>,
    list: Option<DrawList2d>,
    scrim: Option<DrawLayerScrim>,
    window: Rect,
    area: Area,
    sheets: Vec<Standalone>,
    painted_top: Option<u64>,
}

thread_local! {
    static LAYER: RefCell<HashMap<String, Layer>> = RefCell::new(HashMap::new());
}

/// Opens the tool-call detail sheet for `display_name` / `detail` over the
/// window `window_id` (Flutter `showToolCallSheet` with the tool's glyph).
/// Returns its id.
pub fn show_tool_call_sheet(
    cx: &mut Cx,
    window_id: &str,
    display_name: &str,
    detail: &Value,
) -> u64 {
    LAYER.with(|layer| {
        let mut layers = layer.borrow_mut();
        let layer = layers.entry(window_id.to_owned()).or_default();
        if let Some(parent) = layer.sheets.last_mut() {
            parent.touch_scroll.reset();
            parent.bar_drag = None;
            parent.state.dragging_bar = None;
            parent.tap.cancel();
        }
        let id = crate::common::overlay::next_id();
        let mut motion = SheetMotion::default();
        motion.set_open(cx, true);
        layer.sheets.push(Standalone {
            id,
            title: display_name.to_owned(),
            detail: detail.clone(),
            motion,
            canvas: Canvas::default(),
            state: RowState::default(),
            paint: None,
            touch_scroll: crate::common::widgets::touch_scroll(),
            bar_drag: None,
            tap: TapTarget::default(),
        });
        // The layer's list exists from the first show so it can be repainted before its first
        // paint: any list redraw re-runs the shell's overlay pass, which draws this layer.
        layer
            .list
            .get_or_insert_with(|| DrawList2d::new(cx))
            .redraw(cx);
        id
    })
}

/// Whether the sheet finished opening and was drawn fully open.
pub fn tool_call_sheet_settled(id: u64) -> bool {
    LAYER.with(|layers| {
        layers.borrow().values().any(|layer| {
            layer
                .sheets
                .iter()
                .any(|s| s.id == id && s.motion.open && !s.motion.is_moving() && s.paint.is_some())
        })
    })
}

pub fn close_tool_call_sheet(cx: &mut Cx, id: u64) {
    LAYER.with(|layers| {
        for layer in layers.borrow_mut().values_mut() {
            if let Some(index) = layer.sheets.iter().position(|sheet| sheet.id == id) {
                close_standalone(cx, layer, index);
            }
        }
    });
}

/// Repaints the window's sheet layer: its list, which holds `layer.area` once painted.
fn redraw_layer(cx: &mut Cx, layer: &Layer) {
    if let Some(list) = &layer.list {
        list.redraw(cx);
    }
}

fn top_index(layer: &Layer) -> Option<usize> {
    layer.sheets.len().checked_sub(1)
}

fn close_standalone(cx: &mut Cx, layer: &mut Layer, index: usize) {
    let sheet = &mut layer.sheets[index];
    sheet.motion.set_open(cx, false);
    sheet.tap.cancel();
    sheet.touch_scroll.reset();
    sheet.bar_drag = None;
    if !sheet.motion.is_moving() {
        layer.sheets.remove(index);
        redraw_layer(cx, layer);
    }
}

/// Shell hook, before content dispatch: taps (close, scrim), wheel scrolling
/// and Escape for the window's top standalone sheet. Returns true while one
/// is open (the content below is modal-locked).
pub fn handle_tool_call_sheets(cx: &mut Cx, event: &Event, window_id: &str) -> bool {
    LAYER.with(|cell| {
        let mut layers = cell.borrow_mut();
        let Some(layer) = layers.get_mut(window_id) else {
            return false;
        };
        let mut removed = false;
        layer.sheets.retain_mut(|sheet| {
            if sheet.motion.advance(cx, event)
                && let Some(paint) = &sheet.paint
            {
                sheet.motion.compose(cx, paint, layer.window);
                sheet.motion.finish_open(cx, layer.window);
            }
            let visible = sheet.motion.open || sheet.motion.is_moving();
            removed |= !visible;
            visible
        });
        if removed {
            redraw_layer(cx, layer);
        }
        let Some(index) = top_index(layer) else {
            return false;
        };
        if !layer.sheets[index].motion.open {
            return true;
        }
        if take_dismissal(event) {
            close_standalone(cx, layer, index);
            return true;
        }
        if matches!(
            event,
            Event::BackPressed { .. }
                | Event::KeyDown(KeyEvent {
                    key_code: KeyCode::Escape,
                    ..
                })
        ) {
            return true;
        }
        if matches!(
            event,
            Event::TextHandle(_) | Event::Pause | Event::AppLostFocus
        ) || matches!(event, Event::TouchUpdate(update) if update.cancelled)
        {
            layer.sheets[index].touch_scroll.reset();
            layer.sheets[index].bar_drag = None;
            layer.sheets[index].state.dragging_bar = None;
            layer.sheets[index].tap.cancel();
        }
        // A newly pushed/revealed owner cannot reuse the previous owner's hit map.
        if layer.painted_top != Some(layer.sheets[index].id) {
            return true;
        }
        let sheet = &mut layer.sheets[index];
        let area = layer.area;
        let offset = sheet.motion.offset;
        let origin = sheet.canvas.origin + offset;
        let redraw = |cx: &mut Cx, sheet: &Standalone| {
            if let Some(paint) = &sheet.paint {
                paint.redraw(cx);
            }
            cx.redraw_area(area);
        };
        if let Event::Scroll(se) = event {
            sheet.touch_scroll.reset();
            sheet.tap.cancel();
            if scroll_sheet(&mut sheet.state, &sheet.canvas, se.abs - offset, se.scroll) {
                redraw(cx, sheet);
            }
        }
        let hit = event.hits(cx, area);
        if let Hit::FingerDown(e) = &hit {
            let rel = e.abs - origin;
            if let Some(target) = sheet.canvas.hit_at(rel)
                && let Some(drag) = super::scrollbar::BarDrag::press(target, rel, &mut sheet.state)
            {
                sheet.bar_drag = Some(drag);
                sheet.touch_scroll.reset();
                sheet.tap.cancel();
                redraw(cx, sheet);
                return true;
            }
        }
        if let Some(drag) = &sheet.bar_drag {
            match &hit {
                Hit::FingerMove(e) => {
                    if let (DigitDevice::Touch { uid }, Event::TouchUpdate(update)) =
                        (&e.device, event)
                        && let Some(touch) = update.touches.iter().find(|touch| touch.uid == *uid)
                    {
                        touch.handled.set(area);
                    }
                    drag.drag_to(e.abs - origin, &mut sheet.state);
                    redraw(cx, sheet);
                    return true;
                }
                Hit::FingerUp(_) => {
                    sheet.bar_drag = None;
                    sheet.state.dragging_bar = None;
                    redraw(cx, sheet);
                    return true;
                }
                _ => {}
            }
        }
        let scroll_start = match &hit {
            Hit::FingerDown(e) => sheet
                .canvas
                .scroll_region_at(e.abs - origin, true)
                .is_some(),
            _ => true,
        };
        if !scroll_start {
            sheet.touch_scroll.reset();
        }
        if scroll_start && let Some(delta) = sheet.touch_scroll.handle_event(cx, event, &hit) {
            sheet.tap.cancel();
            if scroll_sheet(
                &mut sheet.state,
                &sheet.canvas,
                sheet.touch_scroll.origin() - offset,
                dvec2(0.0, delta),
            ) {
                redraw(cx, sheet);
            } else {
                sheet.touch_scroll.stop_at_edge();
            }
        }
        let tap_target = |abs: DVec2| {
            sheet.canvas.hit_at(abs - origin).cloned().or_else(|| {
                layer
                    .window
                    .contains(abs)
                    .then(|| HitTarget::Chrome("scrim".into()))
            })
        };
        let mut close = false;
        match hit {
            Hit::FingerHoverIn(e) | Hit::FingerHoverOver(e) => {
                let hit = sheet
                    .canvas
                    .hit_at(e.abs - origin)
                    .map(super::chat_row::hover_key);
                if sheet.state.hover_hit != hit {
                    sheet.state.hover_hit = hit.clone();
                    redraw(cx, sheet);
                }
                cx.set_cursor(if matches!(hit.as_deref(), Some("sheet:close")) {
                    MouseCursor::Hand
                } else {
                    MouseCursor::Default
                });
            }
            Hit::FingerDown(e) => sheet.tap.press(tap_target(e.abs)),
            Hit::FingerLongPress(_) => sheet.tap.cancel(),
            Hit::FingerUp(e) => {
                let released = sheet.tap.release(
                    tap_target(e.abs),
                    e.is_over && e.was_tap() && !e.has_long_press_occurred,
                );
                match released {
                    Some(HitTarget::Chrome(id)) if id == "sheet:close" || id == "scrim" => {
                        close = true
                    }
                    Some(target @ HitTarget::MermaidExport { .. }) => {
                        super::mermaid::tap(&mut sheet.state, &target);
                        redraw(cx, sheet);
                    }
                    Some(HitTarget::Copy { key, text }) => {
                        cx.copy_to_clipboard(&text);
                        sheet.state.copied.insert(key, std::time::Instant::now());
                        redraw(cx, sheet);
                    }
                    Some(HitTarget::Toggle(key)) => {
                        sheet.state.toggle(&key);
                        redraw(cx, sheet);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        if close {
            close_standalone(cx, layer, index);
        }
        true
    })
}

/// Shell hook, in the window's last overlay pass: draws its standalone sheets.
pub fn draw_tool_call_sheets(cx: &mut Cx2d, window_id: &str, window: Rect) {
    LAYER.with(|cell| {
        let mut layers = cell.borrow_mut();
        let Some(layer) = layers.get_mut(window_id) else {
            return;
        };
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
        let scrim = layer
            .scrim
            .get_or_insert_with(|| DrawLayerScrim::new_local(cx));
        let (Some(kit), Some(list)) = (layer.kit.as_mut(), layer.list.as_mut()) else {
            return;
        };
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
        layer.window = window;
        list.begin_always(cx);
        // The layer's capture area is its own full-window turtle (stable across
        // redraws, unlike the first instance of a primitive drawn many times).
        cx.begin_turtle(Walk::abs_rect(window), Layout::default());
        let now = now_seconds();
        for sheet in &mut layer.sheets {
            // Each sheet owns its own cover and paint order.
            let paint = sheet.paint.get_or_insert_with(|| DrawList2d::new(cx));
            sheet.motion.draw_scrim(cx, scrim, window);
            if paint.begin(cx, Walk::abs_rect(window)).is_err() {
                sheet.motion.compose(cx, paint, window);
                continue;
            }
            sheet
                .motion
                .mark_cover(cx, window, look.palette.surface2.w >= 1.0);
            kit.begin_layer(cx);
            sheet.canvas.begin(window.pos);
            let glyph = tools::resolve_tool_call_icon(&sheet.title, Some(&sheet.detail), false);
            let detail = sheet.detail.clone();
            let tool_name = sheet.title.clone();
            let header = SheetHeader {
                glyph,
                title: sheet.title.clone(),
                max_lines: 1,
                semibold: true,
                copy: tools::detail_copy_text(&detail_blocks(
                    &detail,
                    Some(&tool_name),
                    None,
                    &|k| t(k),
                )),
            };
            let mut d = Draw {
                kit: &mut *kit,
                canvas: &mut sheet.canvas,
                look,
                state: &mut sheet.state,
                now,
                area: layer.area,
                owner: None,
                animating: false,
                indicators: None,
            };
            // An overlay root: it reaches the window's bottom edge and owns the system-bar padding.
            let bottom_inset = crate::shell::platform::safe_insets().bottom;
            paint_detail_sheet(
                cx,
                &mut d,
                window,
                bottom_inset,
                &header,
                false,
                &|cx, d, x, y, w, fill| {
                    let blocks = detail_blocks(&detail, Some(&tool_name), None, &|k| t(k));
                    let skeleton =
                        tools::is_pending_tool_call_detail(&detail, "completed", &Value::Null);
                    draw_details(
                        cx,
                        d,
                        &blocks,
                        detail_is_full_bleed(&detail),
                        skeleton,
                        "standalone:sheet:detail",
                        x,
                        y,
                        w,
                        None,
                        Some(fill),
                    )
                },
            );
            paint.end(cx);
            sheet.motion.compose(cx, paint, window);
        }
        cx.end_turtle_with_area(&mut layer.area);
        list.end(cx);
        layer.painted_top = layer.sheets.last().map(|sheet| sheet.id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn diagram_horizontal_wheel_moves_inner_region_not_sheet() {
        let mut state = RowState::default();
        state.extent_x.insert("diagram".into(), 200.0);
        state.extent_y.insert("sheet:y".into(), 400.0);
        let mut canvas = Canvas::default();
        let rect = rect_xywh(0.0, 0.0, 300.0, 200.0);
        canvas.add_hit(rect, HitTarget::ScrollY("sheet:y".into()));
        canvas.add_hit(rect, HitTarget::ScrollX("diagram".into()));
        assert!(scroll_sheet(
            &mut state,
            &canvas,
            dvec2(20.0, 20.0),
            dvec2(45.0, 0.0)
        ));
        assert_eq!(state.scroll_x.get("diagram"), Some(&45.0));
        assert!(!state.scroll_y.contains_key("sheet:y"));
    }

    fn sheet(id: u64) -> Standalone {
        Standalone {
            id,
            title: String::new(),
            detail: Value::Null,
            motion: SheetMotion {
                open: true,
                ..Default::default()
            },
            canvas: Canvas::default(),
            state: RowState::default(),
            paint: None,
            touch_scroll: crate::common::widgets::touch_scroll(),
            bar_drag: None,
            tap: TapTarget::default(),
        }
    }

    #[test]
    fn dismissal_closes_only_top_sheet_in_its_window_and_ignores_repeats() {
        LAYER.with(|layers| {
            *layers.borrow_mut() = HashMap::from([
                (
                    "main".into(),
                    Layer {
                        sheets: vec![sheet(1), sheet(2)],
                        ..Default::default()
                    },
                ),
                (
                    "other".into(),
                    Layer {
                        sheets: vec![sheet(3)],
                        ..Default::default()
                    },
                ),
            ]);
        });
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let escape = KeyEvent {
            key_code: KeyCode::Escape,
            ..Default::default()
        };
        assert!(handle_tool_call_sheets(
            &mut cx,
            &Event::KeyDown(escape),
            "main"
        ));
        assert!(handle_tool_call_sheets(
            &mut cx,
            &Event::KeyDown(KeyEvent {
                is_repeat: true,
                ..escape
            }),
            "main"
        ));
        LAYER.with(|layers| {
            let layers = layers.borrow();
            assert_eq!(
                layers["main"]
                    .sheets
                    .iter()
                    .map(|sheet| sheet.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert_eq!(
                layers["other"]
                    .sheets
                    .iter()
                    .map(|sheet| sheet.id)
                    .collect::<Vec<_>>(),
                vec![3]
            );
        });
        let back = Event::BackPressed {
            handled: Cell::new(false),
        };
        assert!(handle_tool_call_sheets(&mut cx, &back, "main"));
        assert!(!handle_tool_call_sheets(&mut cx, &back, "main"));
        assert!(!take_dismissal(&back));
        LAYER.with(|layers| layers.borrow_mut().clear());
    }
}
