use crate::makepad_draw::*;
use std::sync::atomic::{AtomicU64, Ordering};

live_design! {
    link widgets;
    use makepad_draw::shader::std::*;
    pub DrawTextSelectionHandle = {{DrawTextSelectionHandle}} {
        fn pixel(self) -> vec4 {
            let sdf = Sdf2d::viewport(self.pos * self.rect_size);
            sdf.circle(9.0, 10.0, 8.0);
            sdf.fill(#4c9ffe);
            return sdf.result;
        }
    }
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
struct DrawTextSelectionHandle {
    #[deref] draw_super: DrawQuad,
}

/// Shared touch-only renderer. Gesture capture, frame timing and native magnifier
/// lifecycle live in the platform registry so parent scroll views cannot steal it.
///
/// Rust-only consumers must explicitly import
/// `makepad_widgets::text_selection_handles::DrawTextSelectionHandle` in a reachable
/// `live_design!` document. TextInput already retains that dependency. A wildcard
/// `link::widgets::*` import alone does not expand unused shader identifiers.
pub struct TextSelectionHandles {
    id: u64,
    anchor: DrawTextSelectionHandle,
    cursor: DrawTextSelectionHandle,
    dragging: bool,
    range_drag: bool,
}

impl TextSelectionHandles {
    pub fn new(cx: &mut Cx) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        // Bare LiveNew::new initializes Rust fields but does not apply the shader
        // document. Manual draw objects must instantiate their registered design.
        let mut handles = Self { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), anchor: DrawTextSelectionHandle::new_local(cx),
            cursor: DrawTextSelectionHandle::new_local(cx), dragging: false, range_drag: false };
        handles.anchor.draw_vars.set_draw_call_group(LiveId(handles.id));
        handles.cursor.draw_vars.set_draw_call_group(LiveId(handles.id));
        handles
    }

    pub fn is_dragging(&self) -> bool { self.dragging }

    pub fn cancel(&mut self, cx: &mut Cx) {
        cx.cancel_text_handles(self.id);
        self.dragging = false;
        self.range_drag = false;
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, owner: Area) -> Option<TextHandleAction> {
        self.dragging = cx.text_handle_is_dragging(self.id);
        let Event::TextHandle(event) = event else { return None; };
        if event.id != self.id || event.owner != owner { return None; }
        self.dragging = matches!(event.action.phase, TextHandlePhase::Start | TextHandlePhase::Move);
        if event.action.phase == TextHandlePhase::Start {
            self.range_drag = event.action.endpoint != TextHandleEndpoint::Caret;
        }
        Some(event.action)
    }

    /// Coordinates are absolute at draw time; registered painted areas acquire
    /// final turtle alignment/clipping. Endpoints retain identity when crossed.
    pub fn draw(&mut self, cx: &mut Cx2d, owner: Area, viewport: Rect,
        anchor: Rect, cursor: Rect, collapsed: bool, editable: bool) {
        if !cx.has_key_focus(owner) || (collapsed && !editable && !self.dragging) {
            self.cancel(cx);
            return;
        }
        let range = !collapsed || (self.dragging && self.range_drag);
        let anchor_first = anchor.pos.y < cursor.pos.y || (anchor.pos.y == cursor.pos.y && anchor.pos.x <= cursor.pos.x);
        let mut targets = [None, None];
        if range {
            targets[0] = Self::draw_one(&mut self.anchor, cx, owner, viewport, anchor, TextHandleEndpoint::Anchor, if anchor_first { -9.0 } else { 9.0 });
        }
        targets[1] = Self::draw_one(&mut self.cursor, cx, owner, viewport, cursor,
            if range { TextHandleEndpoint::Cursor } else { TextHandleEndpoint::Caret },
            if !range { 0.0 } else if anchor_first { 9.0 } else { -9.0 });
        cx.register_text_handles(self.id, owner, viewport, targets);
    }

    fn draw_one(draw: &mut DrawTextSelectionHandle, cx: &mut Cx2d, owner: Area, viewport: Rect,
        caret: Rect, endpoint: TextHandleEndpoint, orientation: f64) -> Option<TextHandleTarget> {
        if caret.size.x <= 0.0 || caret.size.y <= 0.0 { return None; }
        let point = dvec2(caret.pos.x + caret.size.x * 0.5, caret.pos.y + caret.size.y * 0.5);
        // An offscreen endpoint must not leave a detached knob at the clip edge.
        if !viewport.contains(point) { return None; }
        let rect = Rect { pos: dvec2(point.x + orientation - 9.0, caret.pos.y + caret.size.y - 1.0), size: dvec2(18.0, 20.0) };
        let origin = owner.rect(cx).pos;
        cx.begin_pass_sized_turtle(Layout::default());
        draw.draw_abs(cx, rect);
        let area = draw.area();
        cx.end_pass_sized_turtle_with_visible_anchor(owner, origin, area, point - rect.pos);
        Some(TextHandleTarget { area, endpoint, caret_offset: point - rect.pos })
    }
}
