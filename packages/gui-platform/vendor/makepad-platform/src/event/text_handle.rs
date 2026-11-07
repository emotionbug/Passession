use crate::{area::Area, cx::Cx, event::{Event, NextFrame, TouchState}, makepad_math::{dvec2, DVec2, Rect}};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextHandleEndpoint { Caret, Anchor, Cursor }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextHandlePhase { Start, Move, End, Cancel }
#[derive(Clone, Copy, Debug)]
pub struct TextHandleAction {
    pub endpoint: TextHandleEndpoint,
    pub phase: TextHandlePhase,
    pub abs: DVec2,
    /// Content-signed scroll distance, integrated from frame time, not event rate.
    pub scroll: DVec2,
}
#[derive(Clone, Copy, Debug)]
pub struct TextHandleEvent {
    pub owner: Area,
    pub id: u64,
    pub action: TextHandleAction,
}
#[derive(Clone, Copy, Debug)]
pub struct TextHandleTarget {
    pub area: Area,
    pub endpoint: TextHandleEndpoint,
    /// Caret center relative to the painted handle's top-left.
    pub caret_offset: DVec2,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextHandleRegistration {
    pub id: u64,
    pub owner: Area,
    pub viewport: Rect,
    pub targets: [Option<TextHandleTarget>; 2],
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextHandleDrag {
    pub id: u64,
    pub owner: Area,
    pub uid: u64,
    pub endpoint: TextHandleEndpoint,
    pub abs: DVec2,
    pub offset: DVec2,
    pub start: DVec2,
    pub moved: bool,
    pub last_time: Option<f64>,
    pub frame: NextFrame,
}

impl TextHandleDrag {
    fn touch(&mut self, update: &super::TouchUpdateEvent) -> Option<TextHandlePhase> {
        if update.cancelled { return Some(TextHandlePhase::Cancel); }
        let touch = update.touches.iter().find(|touch| touch.uid == self.uid)?;
        let delta = touch.abs - self.start;
        self.moved |= delta.x * delta.x + delta.y * delta.y >= 9.0;
        if self.moved { self.abs = touch.abs + self.offset; }
        touch.handled.set(self.owner);
        touch.sweep_lock.set(self.owner);
        Some(if matches!(touch.state, TouchState::Stop) { TextHandlePhase::End } else { TextHandlePhase::Move })
    }
}

fn edge_speed(point: f64, start: f64, size: f64) -> f64 {
    if !point.is_finite() || !size.is_finite() || size <= 0.0 { return 0.0; }
    let edge = 28.0_f64.min(size * 0.25);
    if point < start + edge { -480.0 * ((start + edge - point) / edge).clamp(0.0, 1.0) }
    else if point > start + size - edge { 480.0 * ((point - start - size + edge) / edge).clamp(0.0, 1.0) }
    else { 0.0 }
}
fn frame_scroll(point: DVec2, viewport: Rect, elapsed: f64) -> DVec2 {
    let dt = if elapsed.is_finite() { elapsed.clamp(0.0, 0.05) } else { 0.0 };
    dvec2(edge_speed(point.x, viewport.pos.x, viewport.size.x), edge_speed(point.y, viewport.pos.y, viewport.size.y)) * dt
}

impl Cx {
    /// Registers actual painted instances: hit geometry follows final alignment,
    /// draw-list translation and clipping, rather than provisional draw positions.
    pub fn register_text_handles(&mut self, id: u64, owner: Area, viewport: Rect, targets: [Option<TextHandleTarget>; 2]) {
        if owner == Area::Empty || !owner.is_valid(self) || !self.has_key_focus(owner) { return; }
        if self.keyboard.text_handle_drag.is_some_and(|drag| drag.id != id) { return; }
        self.claim_text_selection(owner);
        self.keyboard.text_handles = Some(TextHandleRegistration {
            id, owner, viewport: viewport.translate(-owner.rect(self).pos), targets,
        });
    }

    pub fn text_handle_is_dragging(&self, id: u64) -> bool {
        self.keyboard.text_handle_drag.is_some_and(|drag| drag.id == id)
    }

    pub fn cancel_text_handles(&mut self, id: u64) {
        if self.keyboard.text_handles.is_some_and(|handles| handles.id == id) {
            self.keyboard.text_handles = None;
        }
        if self.keyboard.text_handle_drag.is_some_and(|drag| drag.id == id) {
            self.keyboard.text_handle_drag = None;
            self.text_handle_magnifier(None);
        }
    }

    pub(crate) fn text_handle_magnifier(&mut self, point: Option<DVec2>) {
        #[cfg(target_os = "android")]
        {
            let dpi = self.windows[crate::window::CxWindowPool::id_zero()].dpi_override.unwrap_or(self.os.dpi_factor);
            let source = point.and_then(|point| self.keyboard.text_handles
                .map(|registration| (point, self.text_handle_viewport(registration))));
            unsafe { crate::os::linux::android::android_jni::to_java_text_magnifier(source, dpi); }
        }
        #[cfg(not(target_os = "android"))]
        let _ = point;
    }

    fn text_handle_screen_rect(&self, area: Area) -> Rect {
        let mut rect = area.rect(self);
        if let Some(id) = area.draw_list_id() {
            let list = &self.draw_lists[id];
            if list.draw_list_has_clip {
                rect.pos += dvec2(list.draw_list_uniforms.view_shift.x as f64, list.draw_list_uniforms.view_shift.y as f64);
            }
        }
        rect
    }

    fn text_handle_viewport(&self, registration: TextHandleRegistration) -> Rect {
        let mut origin = registration.owner.rect(self).pos;
        if let Some(id) = registration.owner.draw_list_id() {
            let list = &self.draw_lists[id];
            if list.draw_list_has_clip {
                origin += dvec2(list.draw_list_uniforms.view_shift.x as f64, list.draw_list_uniforms.view_shift.y as f64);
            }
        }
        let clip = registration.owner.clipped_rect(self);
        registration.viewport.translate(origin).clip((clip.pos, clip.pos + clip.size))
    }

    /// Runs before ordinary pointer dispatch, including outside-owner dismissal.
    /// The owned touch stream is not sent to ancestor scrolling recognizers.
    pub(crate) fn route_text_handle_event(&mut self, event: &Event) -> bool {
        let leaving = matches!(event, Event::AppLostFocus | Event::Pause | Event::Background | Event::Shutdown | Event::MouseDown(_));
        let valid = self.keyboard.text_handles.is_some_and(|r| r.owner.is_valid(self)
            && self.has_key_focus(r.owner) && self.keyboard.text_selection == r.owner
            && { let v = self.text_handle_viewport(r); v.size.x > 0.0 && v.size.y > 0.0 });
        if leaving || !valid {
            if let Some(drag) = self.keyboard.text_handle_drag.take() {
                self.text_handle_magnifier(None);
                self.inner_call_event_handler(&Event::TextHandle(TextHandleEvent { owner: drag.owner, id: drag.id,
                    action: TextHandleAction { endpoint: drag.endpoint, phase: TextHandlePhase::Cancel, abs: drag.abs, scroll: DVec2::default() } }));
            }
            self.keyboard.text_handles = None;
            return false;
        }
        let registration = self.keyboard.text_handles.unwrap();
        let viewport = self.text_handle_viewport(registration);
        if let Some(mut drag) = self.keyboard.text_handle_drag {
            let mut phase = TextHandlePhase::Move;
            let mut scroll = DVec2::default();
            let consume = match event {
                Event::TouchUpdate(update) => {
                    let Some(next_phase) = drag.touch(update) else { return true; };
                    phase = next_phase;
                    true
                }
                Event::NextFrame(frame) if frame.set.contains(&drag.frame) => {
                    if drag.moved {
                        if let Some(previous) = drag.last_time { scroll = frame_scroll(drag.abs, viewport, frame.time - previous); }
                    }
                    drag.last_time = Some(frame.time);
                    drag.frame = self.new_next_frame();
                    false
                }
                Event::LongPress(long) if long.uid == drag.uid => return true,
                _ => return false,
            };
            if matches!(phase, TextHandlePhase::End | TextHandlePhase::Cancel) {
                self.keyboard.text_handle_drag = None;
                self.text_handle_magnifier(None);
            } else {
                self.keyboard.text_handle_drag = Some(drag);
                self.text_handle_magnifier(Some(dvec2(drag.abs.x.clamp(viewport.pos.x, viewport.pos.x + viewport.size.x), drag.abs.y.clamp(viewport.pos.y, viewport.pos.y + viewport.size.y))));
            }
            self.inner_call_event_handler(&Event::TextHandle(TextHandleEvent { owner: drag.owner, id: drag.id,
                action: TextHandleAction { endpoint: drag.endpoint, phase, abs: drag.abs, scroll } }));
            return consume;
        }
        let Event::TouchUpdate(update) = event else { return false; };
        if update.cancelled { return false; }
        let window = registration.owner.draw_list_id().and_then(|id| self.draw_lists[id].pass_id).and_then(|id| self.get_pass_window_id(id));
        if window != Some(update.window_id) { return false; }
        for touch in &update.touches {
            if !matches!(touch.state, TouchState::Start) { continue; }
            let target = registration.targets.iter().flatten().filter_map(|target| {
                if !target.area.is_valid(self) { return None; }
                let rect = target.area.clipped_rect(self);
                if rect.size.x <= 0.0 || rect.size.y <= 0.0 { return None; }
                let hit = Rect { pos: rect.pos - dvec2(12.0, 12.0), size: rect.size + dvec2(24.0, 24.0) };
                let center = rect.pos + rect.size * 0.5;
                let delta = touch.abs - center;
                hit.contains(touch.abs).then_some((*target, delta.x * delta.x + delta.y * delta.y))
            }).min_by(|a, b| a.1.total_cmp(&b.1)).map(|v| v.0);
            if let Some(target) = target {
                let rect = self.text_handle_screen_rect(target.area);
                let caret = rect.pos + target.caret_offset;
                let drag = TextHandleDrag { id: registration.id, owner: registration.owner, uid: touch.uid,
                    endpoint: target.endpoint, abs: caret, offset: caret - touch.abs,
                    start: touch.abs, moved: false,
                    last_time: None, frame: self.new_next_frame() };
                self.keyboard.text_handle_drag = Some(drag);
                touch.handled.set(drag.owner);
                touch.sweep_lock.set(drag.owner);
                self.hide_text_actions(drag.owner);
                self.text_handle_magnifier(Some(caret));
                self.inner_call_event_handler(&Event::TextHandle(TextHandleEvent { owner: drag.owner, id: drag.id,
                    action: TextHandleAction { endpoint: drag.endpoint, phase: TextHandlePhase::Start, abs: caret, scroll: DVec2::default() } }));
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edge_scroll_is_bounded_and_frame_timed() {
        let viewport = Rect { pos: dvec2(10.0, 20.0), size: dvec2(200.0, 100.0) };
        assert_eq!(frame_scroll(dvec2(110.0, 70.0), viewport, 0.016), DVec2::default());
        assert_eq!(frame_scroll(dvec2(-100.0, 500.0), viewport, 10.0), dvec2(-24.0, 24.0));
        assert_eq!(frame_scroll(dvec2(-100.0, 500.0), viewport, -1.0), DVec2::default());
        let half = frame_scroll(dvec2(210.0, 120.0), viewport, 0.008);
        assert_eq!(half * 2.0, frame_scroll(dvec2(210.0, 120.0), viewport, 0.016));
        assert_eq!(edge_speed(10.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn drag_keeps_touch_and_endpoint_identity_through_crossing_and_cancel() {
        use crate::event::{TouchPoint, TouchUpdateEvent, KeyModifiers};
        use std::cell::Cell;
        let mut drag = TextHandleDrag { id: 7, owner: Area::Empty, uid: 4,
            endpoint: TextHandleEndpoint::Anchor, abs: dvec2(10.0, 10.0),
            offset: dvec2(0.0, -10.0), start: dvec2(10.0, 20.0), moved: false,
            last_time: None, frame: NextFrame(1) };
        let mut event = TouchUpdateEvent { time: 0.0, cancelled: false,
            window_id: crate::window::CxWindowPool::id_zero(), modifiers: KeyModifiers::default(),
            touches: vec![TouchPoint { state: TouchState::Move, abs: dvec2(200.0, 20.0), time: 0.0, uid: 5,
                rotation_angle: 0.0, force: 0.0, radius: DVec2::default(),
                handled: Cell::new(Area::Empty), sweep_lock: Cell::new(Area::Empty) }] };
        assert_eq!(drag.touch(&event), None);
        assert_eq!(drag.abs, dvec2(10.0, 10.0));
        event.touches[0].uid = 4;
        event.touches[0].abs = dvec2(11.0, 21.0);
        assert_eq!(drag.touch(&event), Some(TextHandlePhase::Move));
        assert_eq!(drag.abs, dvec2(10.0, 10.0));
        event.touches[0].abs = dvec2(200.0, 20.0);
        drag.touch(&event);
        assert_eq!(drag.abs, dvec2(200.0, 10.0));
        assert_eq!(drag.endpoint, TextHandleEndpoint::Anchor);
        event.touches[0].state = TouchState::Stop;
        assert_eq!(drag.touch(&event), Some(TextHandlePhase::End));
        event.cancelled = true;
        event.touches.clear();
        assert_eq!(drag.touch(&event), Some(TextHandlePhase::Cancel));
    }
}
