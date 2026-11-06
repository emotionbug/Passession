use crate::{
    makepad_draw::*,
    makepad_draw::event::TouchState,
    scroll_bar::*
};

live_design!{
    link widgets;
    use link::theme::*;
    use link::widgets::*;
    use makepad_draw::shader::std::*;
    
    pub ScrollBarsBase = {{ScrollBars}} {}
    
    pub ScrollBarsTabs = <ScrollBarsBase> {
        show_scroll_x: true,
        show_scroll_y: true,
        scroll_bar_x: <ScrollBarTabs> {}
        scroll_bar_y: <ScrollBarTabs> {}
    }
    
    pub ScrollBars = <ScrollBarsBase> {
        show_scroll_x: true,
        show_scroll_y: true,
        scroll_bar_x: <ScrollBar> {}
        scroll_bar_y: <ScrollBar> {}
    }
}

/// A touch starts as a tap, becomes a vertical pan after slop, or yields to
/// long-press selection. Mouse selection and scrollbar drags are separate.
#[derive(Default)]
pub struct TouchScroll {
    last: Option<DVec2>,
    panning: bool,
}

impl TouchScroll {
    pub fn reset(&mut self) {
        self.last = None;
        self.panning = false;
    }

    pub fn handle_hit(&mut self, hit: &Hit) -> Option<f64> {
        match hit {
            Hit::FingerDown(e) => {
                self.reset();
                if e.device.is_touch() {
                    self.last = Some(e.abs);
                }
            }
            Hit::FingerMove(e) if e.device.is_touch() => {
                let last = self.last?;
                if !self.panning && (e.abs.y - e.abs_start.y).abs() < 10.0 {
                    return None;
                }
                self.panning = true;
                self.last = Some(e.abs);
                return Some(last.y - e.abs.y);
            }
            Hit::FingerLongPress(_) | Hit::FingerUp(_) => self.reset(),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod touch_scroll_tests {
    use super::*;
    use makepad_draw::event::{DigitId, FingerLongPressEvent};

    fn down(device: DigitDevice) -> Hit {
        Hit::FingerDown(FingerDownEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, 100.0),
            digit_id: DigitId::default(), device, tap_count: 1,
            modifiers: KeyModifiers::default(), time: 0.0, rect: Rect::default(),
        })
    }

    fn moved(y: f64, device: DigitDevice) -> Hit {
        Hit::FingerMove(FingerMoveEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, y),
            digit_id: DigitId::default(), device, tap_count: 1,
            modifiers: KeyModifiers::default(), time: 0.1,
            abs_start: dvec2(20.0, 100.0), rect: Rect::default(), is_over: true,
        })
    }

    #[test]
    fn touch_pan_waits_for_slop_then_tracks_reversal() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        assert_eq!(scroll.handle_hit(&moved(96.0, DigitDevice::Touch { uid: 1 })), None);
        assert_eq!(scroll.handle_hit(&moved(80.0, DigitDevice::Touch { uid: 1 })), Some(20.0));
        assert_eq!(scroll.handle_hit(&moved(98.0, DigitDevice::Touch { uid: 1 })), Some(-18.0));
    }

    #[test]
    fn long_press_yields_to_selection_until_next_touch() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        scroll.handle_hit(&Hit::FingerLongPress(FingerLongPressEvent {
            window_id: WindowId(0, 0), abs: dvec2(20.0, 100.0),
            capture_time: 0.0, time: 0.6, digit_id: DigitId::default(),
            device: DigitDevice::Touch { uid: 1 }, rect: Rect::default(),
        }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Touch { uid: 1 })), None);
        scroll.handle_hit(&down(DigitDevice::Touch { uid: 1 }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Touch { uid: 1 })), Some(50.0));
    }

    #[test]
    fn mouse_drag_remains_selection_not_panning() {
        let mut scroll = TouchScroll::default();
        scroll.handle_hit(&down(DigitDevice::Mouse { button: MouseButton::PRIMARY }));
        assert_eq!(scroll.handle_hit(&moved(50.0, DigitDevice::Mouse { button: MouseButton::PRIMARY })), None);
    }
}

#[derive(Live, LiveHook, LiveRegister)]
pub struct ScrollBars {
    #[live] show_scroll_x: bool,
    #[live] show_scroll_y: bool,
    #[live] scroll_bar_x: ScrollBar,
    #[live] scroll_bar_y: ScrollBar,
    #[rust] nav_scroll_index: Option<NavScrollIndex>,
    #[rust] scroll: DVec2,
    #[rust] area: Area,
    #[rust] touch_scroll: TouchScroll,
}

pub enum ScrollBarsAction {
    ScrollX(f64),
    ScrollY(f64),
    None
}

impl ScrollBars {
    
    pub fn set_scroll_x(&mut self, _cx: &mut Cx, value: f64) {
        self.scroll.x = value;
    }
    
    pub fn set_scroll_y(&mut self, _cx: &mut Cx, value: f64) {
        self.scroll.y = value;
    }
    
    pub fn get_scroll_pos(&self) -> DVec2 {
        self.scroll
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope:&mut Scope)->Vec<ScrollBarsAction> {
        let mut actions = Vec::new();
        self.handle_main_event(cx, event, scope, &mut actions);
        self.handle_scroll_event(cx, event, scope, &mut actions);
        actions
    }
    
    pub fn handle_main_event(&mut self, cx: &mut Cx, event: &Event, _scope:&mut Scope, actions: &mut Vec<ScrollBarsAction> ) {
        if let Event::Trigger(te) = event{
            if let Some(triggers) = te.triggers.get(&self.area){
                if let Some(trigger) = triggers.iter().find(|t| t.id == live_id!(scroll_focus_nav)){
                    let self_rect = self.area.rect(cx);
                    self.scroll_into_view(
                        cx,
                        trigger.from.rect(cx)
                        .translate(-self_rect.pos + self.scroll)
                        .add_margin(dvec2(5.0,5.0))
                    );
                }
            }
        }
        
        if self.show_scroll_x {
            let mut ret_x = None;
            self.scroll_bar_x.handle_event_with(cx, event, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_x = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollX(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(x) = ret_x {self.scroll.x = x; self.redraw(cx);}
        }
        if self.show_scroll_y {
            let mut ret_y = None;
            self.scroll_bar_y.handle_event_with(cx, event, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_y = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollY(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(y) = ret_y {self.scroll.y = y; self.redraw(cx);}
        }
    }
    
    pub fn handle_scroll_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope, actions: &mut Vec<ScrollBarsAction> ) {
        // Run after children: the deepest scroll region owns each touch move.
        // Overloaded capture preserves child taps and their long-press handling.
        if self.show_scroll_y
            && self.scroll_bar_y.get_scroll_view_total() > self.scroll_bar_y.get_scroll_view_visible()
            && !self.scroll_bar_x.is_area_captured(cx)
            && !self.scroll_bar_y.is_area_captured(cx)
            && matches!(event, Event::TouchUpdate(_) | Event::LongPress(_))
        {
            let child_scrolled = if let Event::TouchUpdate(e) = event {
                e.touches.iter().any(|t| matches!(t.state, TouchState::Move) && !t.handled.get().is_empty())
            } else { false };
            let hit = event.hits_with_capture_overload(cx, self.area, true);
            if child_scrolled {
                self.touch_scroll.reset();
            } else if let Some(dy) = self.touch_scroll.handle_hit(&hit) {
                if let Event::TouchUpdate(e) = event {
                    for t in &e.touches {
                        if matches!(t.state, TouchState::Move) {
                            t.handled.set(self.area);
                        }
                    }
                }
                let pos = self.scroll + dvec2(0.0, dy);
                if self.set_scroll_pos(cx, pos) {
                    actions.push(ScrollBarsAction::ScrollY(self.scroll.y));
                    self.redraw(cx);
                }
            }
        }
        
        if self.show_scroll_x {
            let mut ret_x = None;
            self.scroll_bar_x.handle_scroll_event(cx, event, self.area, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_x = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollX(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(x) = ret_x {self.scroll.x = x; self.redraw(cx);}
        }
        if self.show_scroll_y {
            let mut ret_y = None;
            self.scroll_bar_y.handle_scroll_event(cx, event, self.area, &mut | _cx, action | {
                match action {
                    ScrollBarAction::Scroll {scroll_pos, ..} => {
                        ret_y = Some(scroll_pos);
                        actions.push(ScrollBarsAction::ScrollY(scroll_pos))
                    }
                    _ => ()
                }
            });
            if let Some(y) = ret_y {self.scroll.y = y; self.redraw(cx);}
        }
    }
    
    pub fn set_scroll_pos(&mut self, cx: &mut Cx, pos: DVec2) -> bool {
        //let view_area = Area::DrawList(DrawListArea{draw_list_id:draw_list_id, redraw_id:cx.redraw_id});
        let mut changed = false;
        if self.show_scroll_x {
            if self.scroll_bar_x.set_scroll_pos(cx, pos.x) {
                changed = true;
            }
            let scroll_pos = self.scroll_bar_x.get_scroll_pos();
            self.set_scroll_x(cx, scroll_pos);
        }
        if self.show_scroll_y {
            if self.scroll_bar_y.set_scroll_pos(cx, pos.y) {
                changed = true;
            }
            let scroll_pos = self.scroll_bar_y.get_scroll_pos();
            self.set_scroll_y(cx, scroll_pos);
        }
        changed
    }
    
    pub fn set_scroll_pos_no_clip(&mut self, cx: &mut Cx, pos: DVec2) -> bool {
        let mut changed = false;
        if self.show_scroll_x {
            if self.scroll_bar_x.set_scroll_pos_no_clip(cx, pos.x) {
                changed = true;
            }
            self.set_scroll_x(cx, pos.x);
        }
        if self.show_scroll_y {
            if self.scroll_bar_y.set_scroll_pos_no_clip(cx, pos.y) {
                changed = true;
            }
            self.set_scroll_y(cx, pos.y);
        }
        changed
    }
    
    pub fn get_scroll_view_total(&mut self) -> DVec2 {
        DVec2 {
            x: if self.show_scroll_x {
                self.scroll_bar_x.get_scroll_view_total()
            }else {0.},
            y: if self.show_scroll_y {
                self.scroll_bar_y.get_scroll_view_total()
            }else {0.}
        }
    }
    
    pub fn get_scroll_view_visible(&mut self) -> DVec2 {
        DVec2 {
            x: if self.show_scroll_x {
                self.scroll_bar_x.get_scroll_view_visible()
            }else {0.},
            y: if self.show_scroll_y {
                self.scroll_bar_y.get_scroll_view_visible()
            }else {0.}
        }
    }
    
    pub fn get_viewport_rect(&mut self, _cx: &mut Cx) -> Rect {
        let pos = self.get_scroll_pos();
        let size = self.get_scroll_view_visible();
        Rect {pos, size}
    }
    
    pub fn scroll_into_view(&mut self, cx: &mut Cx, rect: Rect) {
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x, rect.size.x, true);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y, rect.size.y, true);
        }
    }
    
    pub fn scroll_into_view_no_smooth(&mut self, cx: &mut Cx, rect: Rect) {
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x, rect.size.x, false);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y, rect.size.y, false);
        }
    }
    
    pub fn scroll_into_view_abs(&mut self, cx: &mut Cx, rect: Rect) {
        let self_rect = self.area.rect(cx);
        if self.show_scroll_x {
            self.scroll_bar_x.scroll_into_view(cx, rect.pos.x - self_rect.pos.x, rect.size.x, true);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.scroll_into_view(cx, rect.pos.y - self_rect.pos.y, rect.size.y, true);
        }
    }
    
    pub fn set_scroll_target(&mut self, cx: &mut Cx, pos: DVec2) {
        if self.show_scroll_x {
            self.scroll_bar_x.set_scroll_target(cx, pos.x);
        }
        if self.show_scroll_y {
            self.scroll_bar_y.set_scroll_target(cx, pos.y);
        }
    }
    
    
    // all in one scrollbar api
    
    pub fn begin(&mut self, cx: &mut Cx2d, walk: Walk, layout: Layout) {
        cx.begin_turtle(walk, layout.with_scroll(self.scroll));
        self.begin_nav_area(cx);
    }
    
    pub fn end(&mut self, cx: &mut Cx2d) {
        self.draw_scroll_bars(cx);
        // this needs to be a rect_area
        cx.end_turtle_with_area(&mut self.area);
        self.end_nav_area(cx);
    }
    
    
    pub fn end_with_shift(&mut self, cx: &mut Cx2d) {
        self.draw_scroll_bars(cx);
        // this needs to be a rect_area
        cx.end_turtle_with_area(&mut self.area);
        self.end_nav_area(cx);
    }
    // separate API
    
    pub fn begin_nav_area(&mut self, cx: &mut Cx2d) {
        self. nav_scroll_index = Some(cx.add_begin_scroll());
    }
    
    pub fn end_nav_area(&mut self, cx: &mut Cx2d) {
        if !self.area.is_valid(cx) {
            error!("Call set area before end_nav_area");
            return
        }
        cx.add_end_scroll(self.nav_scroll_index.take().unwrap(), self.area);
    }
    
    pub fn draw_scroll_bars(&mut self, cx: &mut Cx2d) {
        // lets ask the turtle our actual bounds
        let view_total = cx.turtle().used();
        let mut rect_now = cx.turtle().rect();
        
        if rect_now.size.y.is_nan() {
            rect_now.size.y = view_total.y;
        }
        if rect_now.size.x.is_nan() {
            rect_now.size.x = view_total.x;
        }
        
        if self.show_scroll_x {
            let scroll_pos = self.scroll_bar_x.draw_scroll_bar(cx, ScrollAxis::Horizontal, rect_now, view_total);
            self.set_scroll_x(cx, scroll_pos);
        }
        if self.show_scroll_y {
            //println!("SET SCROLLBAR {} {}", rect_now.h, view_total.y);
            let scroll_pos = self.scroll_bar_y.draw_scroll_bar(cx, ScrollAxis::Vertical, rect_now, view_total);
            self.set_scroll_y(cx, scroll_pos);
        }
    }
    
    pub fn set_area(&mut self, area: Area) {
        self.area = area;
    }
    
    pub fn area(&self) -> Area {
        self.area
    }
    
    pub fn redraw(&self, cx: &mut Cx) {
        self.area.redraw(cx);
    }
}