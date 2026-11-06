use crate::makepad_draw::*;

thread_local! {
    static DESKTOP_THUMB_COLOR: std::cell::Cell<Option<Vec4>> = const { std::cell::Cell::new(None) };
}

/// Set the current UI thread's desktop scrollbar foreground. Applications with
/// runtime themes call this when updating their palette, then redraw as usual.
pub fn set_desktop_scroll_bar_color(color: Vec4) {
    DESKTOP_THUMB_COLOR.with(|current| current.set(Some(color)));
}

live_design!{
    link widgets;
    use link::theme::*;
    use makepad_draw::shader::std::*;
    
    DrawScrollBar= {{DrawScrollBar}} {}
    
    pub ScrollBarBase= {{ScrollBar}} {}
    
    pub ScrollBar = <ScrollBarBase> {
        bar_size: 10.0,
        bar_side_margin: 3.0
        min_handle_size: 30.0

        draw_bg: {
            instance drag: 0.0
            instance hover: 0.0

            uniform size: 6.0
            uniform border_size: (THEME_BEVELING)
            uniform border_radius: 1.5

            uniform color: (THEME_COLOR_OUTSET)
            uniform color_hover: (THEME_COLOR_OUTSET_HOVER)
            uniform color_drag: (THEME_COLOR_OUTSET_DRAG)

            uniform border_color: (THEME_COLOR_U_HIDDEN)
            uniform border_color_hover: (THEME_COLOR_U_HIDDEN)
            uniform border_color_drag: (THEME_COLOR_U_HIDDEN)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                if self.is_vertical > 0.5 {
                    sdf.box(
                        1.,
                        self.rect_size.y * self.norm_scroll,
                        self.size,
                        self.rect_size.y * self.norm_handle,
                        self.border_radius
                    );
                }
                else {
                    sdf.box(
                        self.rect_size.x * self.norm_scroll,
                        1.,
                        self.rect_size.x * self.norm_handle,
                        self.size,
                        self.border_radius
                    );
                }

                sdf.fill_keep(mix(
                    self.color,
                    mix(
                        self.color_hover,
                        self.color_drag,
                        self.drag
                    ),
                    self.hover
                ));

                sdf.stroke(mix(
                    self.border_color,
                    mix(
                        self.border_color_hover,
                        self.border_color_drag,
                        self.drag
                    ),
                    self.hover
                ), self.border_size);

                return sdf.result * self.opacity
            }
        }

        animator: {
            hover = {
                default: off
                off = {
                    from: {all: Forward {duration: 0.1}}
                    apply: {
                        draw_bg: {drag: 0.0, hover: 0.0}
                    }
                }
                                
                on = {
                    cursor: Default,
                    from: {
                        all: Forward {duration: 0.1}
                        drag: Forward {duration: 0.01}
                    }
                    apply: {
                        draw_bg: {
                            drag: 0.0,
                            hover: [{time: 0.0, value: 1.0}],
                        }
                    }
                }
                                
                drag = {
                    cursor: Default,
                    from: {all: Snap}
                    apply: {
                        draw_bg: {
                            drag: 1.0,
                            hover: 1.0,
                        }
                    }
                }
            }
        }
    }

    pub ScrollBarTabs = <ScrollBar> {
        draw_bg: {
            instance drag: 0.0
            instance hover: 0.0

            uniform size: 6.0
            uniform border_size: 1.0
            uniform border_radius: 1.5

            uniform color: (THEME_COLOR_U_HIDDEN)
            uniform color_hover: (THEME_COLOR_OUTSET_HOVER)
            uniform color_drag: (THEME_COLOR_OUTSET_DRAG)

            uniform border_color: (THEME_COLOR_U_HIDDEN)
            uniform border_color_hover: (THEME_COLOR_U_HIDDEN)
            uniform border_color_drag: (THEME_COLOR_U_HIDDEN)

            fn pixel(self) -> vec4 {
                let sdf = Sdf2d::viewport(self.pos * self.rect_size);
                if self.is_vertical > 0.5 {
                    sdf.box(
                        1.,
                        self.rect_size.y * self.norm_scroll,
                        self.size,
                        self.rect_size.y * self.norm_handle,
                        self.border_radius
                    );
                }
                else {
                    sdf.box(
                        self.rect_size.x * self.norm_scroll,
                        1.,
                        self.rect_size.x * self.norm_handle,
                        self.size,
                        self.border_radius
                    );
                }

                sdf.fill_keep(mix(
                    self.color,
                    mix(
                        self.color_hover,
                        self.color_drag,
                        self.drag
                    ),
                    self.hover
                ));

                sdf.stroke(mix(
                    self.border_color,
                    mix(
                        self.border_color_hover,
                        self.border_color_drag,
                        self.drag
                    ),
                    self.hover
                ), self.border_size);

                return sdf.result * self.opacity
            }
        }
    }


}

#[derive(Copy, Clone, Debug, Live, LiveHook)]
#[live_ignore]
pub enum ScrollAxis {
    #[pick] Horizontal,
    Vertical
}
#[derive(Live, LiveHook, LiveRegister)]
pub struct ScrollBar {
    #[live] draw_bg: DrawScrollBar,
    #[live] pub bar_size: f64,
    #[live] pub min_handle_size: f64, //minimum size of the handle in pixels
    #[live] bar_side_margin: f64,
    #[live(ScrollAxis::Horizontal)] pub axis: ScrollAxis,
    
    #[live] use_vertical_finger_scroll: bool,
    #[live] smoothing: Option<f64>,
    /// Desktop auto-hide; touch platforms retain the upstream scrollbar behavior.
    #[live(true)] fade_when_idle: bool,
    #[rust] fade_timer: Timer,
    #[rust] fade_frame: NextFrame,
    #[rust] fade_started: Option<std::time::Instant>,
    #[rust] hovered: bool,
    #[rust] opacity: f32,
    #[rust] theme_color: Option<Vec4>,
    
    #[animator] animator: Animator,
    
    #[rust] next_frame: NextFrame,
    #[rust(false)] visible: bool,
    #[rust] view_total: f64, // the total view area
    #[rust] view_visible: f64, // the visible view area
    #[rust] scroll_size: f64, // the size of the scrollbar
    #[rust] scroll_pos: f64, // scrolling position non normalised
    
    #[rust] scroll_target: f64,
    #[rust] scroll_delta: f64,
    #[rust] drag_point: Option<f64>, // the point in pixels where we are dragging
}

#[derive(Live, LiveHook, LiveRegister)]
#[repr(C)]
pub struct DrawScrollBar {
    #[deref] draw_super: DrawQuad,
    #[live] is_vertical: f32,
    #[live] norm_handle: f32,
    #[live] norm_scroll: f32,
    #[live(1.0)] opacity: f32,
}

#[derive(Clone, PartialEq, Debug)]
pub enum ScrollBarAction {
    None,
    Scroll {scroll_pos: f64, view_total: f64, view_visible: f64},
    ScrollDone
}

impl ScrollBar {
    fn fades(&self) -> bool {
        self.fade_when_idle && !cfg!(any(target_os = "android", target_os = "ios"))
    }

    fn set_opacity(&mut self, cx: &mut Cx, opacity: f32) {
        if self.opacity != opacity {
            self.opacity = opacity;
            self.draw_bg.apply_over(cx, live! { opacity: (opacity) });
        }
    }

    fn stop_fade(&mut self, cx: &mut Cx) {
        if self.fade_timer.0 != 0 {
            cx.stop_timer(self.fade_timer);
        }
        self.fade_timer = Timer::default();
        self.fade_frame = NextFrame::default();
        self.fade_started = None;
    }

    pub(crate) fn show_thumb(&mut self, cx: &mut Cx) {
        if !self.fades() || !self.visible {
            return;
        }
        self.stop_fade(cx);
        self.set_opacity(cx, 1.0);
        if !self.hovered && self.drag_point.is_none() {
            // One wake at the end of the hold, then frames only during the fade.
            self.fade_timer = cx.start_timeout(0.6);
        }
    }

    fn handle_fade(&mut self, cx: &mut Cx, event: &Event) {
        if !self.fades() || !self.visible {
            return;
        }
        if self.fade_timer.is_event(event).is_some() {
            self.fade_timer = Timer::default();
            self.fade_started = Some(std::time::Instant::now());
            self.fade_frame = cx.new_next_frame();
        }
        if self.fade_frame.is_event(event).is_some() {
            if let Some(started) = self.fade_started {
                let opacity = fade_opacity(started.elapsed().as_secs_f64());
                self.set_opacity(cx, opacity);
                if opacity > 0.0 {
                    self.fade_frame = cx.new_next_frame();
                } else {
                    self.fade_frame = NextFrame::default();
                    self.fade_started = None;
                }
            }
        }
    }

    /*
    pub fn with_bar_size(self, bar_size: f32) -> Self {Self {bar_size, ..self}}
    pub fn with_smoothing(self, s: f32) -> Self {Self {smoothing: Some(s), ..self}}
    pub fn with_use_vertical_finger_scroll(self, use_vertical_finger_scroll: bool) -> Self {Self {use_vertical_finger_scroll, ..self}}
    */
    // reads back normalized scroll position info
    pub fn get_normalized_scroll_pos(&self) -> (f64, f64) {
        // computed handle size normalized
        let vy = self.view_visible / self.view_total;
        if !self.visible {
            return (0.0, 0.0);
        }
        let norm_handle = vy.max(self.min_handle_size / self.scroll_size);
        let norm_scroll = (1. - norm_handle) * ((self.scroll_pos / self.view_total) / (1. - vy));
        return (norm_scroll, norm_handle)
    }
    
    // sets the scroll pos from finger position
    pub fn set_scroll_pos_from_finger(&mut self,finger: f64) -> bool {
        let vy = self.view_visible / self.view_total;
        let norm_handle = vy.max(self.min_handle_size / self.scroll_size);
        
        let new_scroll_pos = (
            (self.view_total * (1. - vy) * (finger / self.scroll_size)) / (1. - norm_handle)
        ).max(0.).min(self.view_total - self.view_visible);
        //log!("SCROLL POS {} {}", new_scroll_pos, self.view_total - self.view_visible);
        // lets snap new_scroll_pos
        let changed = self.scroll_pos != new_scroll_pos;
        self.scroll_pos = new_scroll_pos;
        self.scroll_target = new_scroll_pos;
        changed
    }
    
    // writes the norm_scroll value into the shader
    pub fn update_shader_scroll_pos(&mut self, cx: &mut Cx) {
        self.show_thumb(cx);
        let (norm_scroll, _) = self.get_normalized_scroll_pos();
        self.draw_bg.apply_over(cx, live!{
            norm_scroll: (norm_scroll)
        });
        //self.draw_bg.set_norm_scroll(cx, norm_scroll);
    }
    
    // turns scroll_pos into an event on this.event
    pub fn make_scroll_action(&mut self) -> ScrollBarAction {
        ScrollBarAction::Scroll {
            scroll_pos: self.scroll_pos,
            view_total: self.view_total,
            view_visible: self.view_visible
        }
    }
    
    pub fn move_towards_scroll_target(&mut self, cx: &mut Cx) -> bool {
        if self.smoothing.is_none() {
            return false;
        }
        if (self.scroll_target - self.scroll_pos).abs() < 0.01 {
            return false
        }
        if self.scroll_pos > self.scroll_target { // go back
            self.scroll_pos = self.scroll_pos + (self.smoothing.unwrap() * self.scroll_delta).min(-1.);
            if self.scroll_pos <= self.scroll_target { // hit the target
                self.scroll_pos = self.scroll_target;
                self.update_shader_scroll_pos(cx);
                return false;
            }
        }
        else { // go forward
            self.scroll_pos = self.scroll_pos + (self.smoothing.unwrap() * self.scroll_delta).max(1.);
            if self.scroll_pos > self.scroll_target { // hit the target
                self.scroll_pos = self.scroll_target;
                self.update_shader_scroll_pos(cx);
                return false;
            }
        }
        self.update_shader_scroll_pos(cx);
        true
    }
    
    pub fn get_scroll_pos(&self) -> f64 {
        return self.scroll_pos;
    }
    
    pub fn set_scroll_pos_no_action(&mut self, cx: &mut Cx, scroll_pos: f64) -> bool {
        let scroll_pos = scroll_pos.min(self.view_total - self.view_visible).max(0.);
        if self.scroll_pos != scroll_pos {
            self.scroll_pos = scroll_pos;
            self.scroll_target = scroll_pos;
            self.update_shader_scroll_pos(cx);
            return true
        };
        return false
    }
    pub fn set_scroll_pos(&mut self, cx: &mut Cx, scroll_pos: f64) -> bool {
        let scroll_pos = scroll_pos.min(self.view_total - self.view_visible).max(0.);
        if self.scroll_pos != scroll_pos {
            self.scroll_pos = scroll_pos;
            self.scroll_target = scroll_pos;
            self.update_shader_scroll_pos(cx);
            self.next_frame = cx.new_next_frame();
            return true
        };
        return false
    }
    
    
    pub fn set_scroll_pos_no_clip(&mut self, cx: &mut Cx, scroll_pos: f64) -> bool {
        if self.scroll_pos != scroll_pos {
            self.scroll_pos = scroll_pos;
            self.scroll_target = scroll_pos;
            self.update_shader_scroll_pos(cx);
            self.next_frame = cx.new_next_frame();
            return true
        };
        return false
    }
    
    pub fn get_scroll_target(&mut self) -> f64 {
        return self.scroll_target
    }
    
    pub fn set_scroll_view_total(&mut self, _cx: &mut Cx, view_total: f64) {
        self.view_total = view_total;
    }
    
    pub fn get_scroll_view_total(&self) -> f64 {
        return self.view_total;
    }
    
    pub fn get_scroll_view_visible(&self) -> f64 {
        return self.view_visible;
    }
    
    
    pub fn set_scroll_target(&mut self, cx: &mut Cx, scroll_pos_target: f64) -> bool {
        // clamp scroll_pos to
        
        let new_target = scroll_pos_target.min(self.view_total - self.view_visible).max(0.);
        if self.scroll_target != new_target {
            self.scroll_target = new_target;
            self.scroll_delta = new_target - self.scroll_pos;
            self.next_frame = cx.new_next_frame();
            return true
        };
        return false
    }
    
    pub fn scroll_into_view(&mut self, cx: &mut Cx, pos: f64, size: f64, smooth: bool) {
        if pos < self.scroll_pos { // scroll up
            let scroll_to = pos;
            if !smooth || self.smoothing.is_none() {
                self.set_scroll_pos(cx, scroll_to);
            }
            else {
                self.set_scroll_target(cx, scroll_to);
            }
        }
        else if pos + size > self.scroll_pos + self.view_visible { // scroll down
            let scroll_to = (pos + size) - self.view_visible;
            if pos + size > self.view_total { // resize _view_total if need be
                self.view_total = pos + size;
            }
            if !smooth || self.smoothing.is_none() {
                self.set_scroll_pos(cx, scroll_to);
            }
            else {
                self.set_scroll_target(cx, scroll_to);
            }
        }
    }
    
    pub fn handle_scroll_event(&mut self, cx: &mut Cx, event: &Event, scroll_area: Area, dispatch_action: &mut dyn FnMut(&mut Cx, ScrollBarAction)) {
        if let Event::Scroll(e) = event {
            if scroll_area.rect(cx).contains(e.abs) {
                if !match self.axis {
                    ScrollAxis::Horizontal => e.handled_x.get(),
                    ScrollAxis::Vertical => e.handled_y.get()
                } {
                    let scroll = match self.axis {
                        ScrollAxis::Horizontal => if self.use_vertical_finger_scroll {e.scroll.y}else {e.scroll.x},
                        ScrollAxis::Vertical => e.scroll.y
                    };
                    if !self.smoothing.is_none() && e.is_mouse {
                        let scroll_pos_target = self.get_scroll_target();
                        if self.set_scroll_target(cx, scroll_pos_target + scroll) {
                            match self.axis {
                                ScrollAxis::Horizontal => e.handled_x.set(true),
                                ScrollAxis::Vertical => e.handled_y.set(true)
                            }
                        };
                        self.move_towards_scroll_target(cx); // take the first step now
                        return dispatch_action(cx, self.make_scroll_action());
                    }
                    else {
                        let scroll_pos = self.get_scroll_pos();
                        if self.set_scroll_pos(cx, scroll_pos + scroll) {
                            match self.axis {
                                ScrollAxis::Horizontal => e.handled_x.set(true),
                                ScrollAxis::Vertical => e.handled_y.set(true)
                            }
                        }
                        return dispatch_action(cx, self.make_scroll_action());
                    }
                }
            }
        }
    }
    pub fn is_area_captured(&self, cx:&Cx)->bool{
        cx.fingers.is_area_captured(self.draw_bg.area())
    }
    
    pub fn handle_event_with(&mut self, cx: &mut Cx, event: &Event, dispatch_action: &mut dyn FnMut(&mut Cx, ScrollBarAction)) {
        self.handle_fade(cx, event);
        if self.visible {
            self.animator_handle_event(cx, event);
            if self.next_frame.is_event(event).is_some() {
                if self.move_towards_scroll_target(cx) {
                    self.next_frame = cx.new_next_frame();
                }
                return dispatch_action(cx, self.make_scroll_action());
            }
            
            match event.hits(cx, self.draw_bg.area()) {
                Hit::FingerDown(fe) if fe.is_primary_hit() => {
                    self.animator_play(cx, id!(hover.drag));
                    let rel = fe.abs - fe.rect.pos;
                    let rel = match self.axis {
                        ScrollAxis::Horizontal => rel.x,
                        ScrollAxis::Vertical => rel.y
                    };
                    let (norm_scroll, norm_handle) = self.get_normalized_scroll_pos();
                    let bar_start = norm_scroll * self.scroll_size;
                    let bar_size = norm_handle * self.scroll_size;
                    if rel < bar_start || rel > bar_start + bar_size { // clicked outside
                        self.drag_point = Some(bar_size * 0.5);
                        if self.set_scroll_pos_from_finger(rel - self.drag_point.unwrap()){
                            dispatch_action(cx, self.make_scroll_action());
                        }
                    }
                    else { // clicked on
                        self.drag_point = Some(rel - bar_start); // store the drag delta
                    }
                    self.show_thumb(cx);
                },
                Hit::FingerHoverIn(_) => {
                    self.hovered = true;
                    self.show_thumb(cx);
                    self.animator_play(cx, id!(hover.on));
                },
                Hit::FingerHoverOut(_) => {
                    self.hovered = false;
                    self.show_thumb(cx);
                    self.animator_play(cx, id!(hover.off));
                },
                Hit::FingerUp(fe) if fe.is_primary_hit() => {
                    self.drag_point = None;
                    self.hovered = fe.is_over && fe.device.has_hovers();
                    self.show_thumb(cx);
                    if fe.is_over && fe.device.has_hovers() {
                        self.animator_play(cx, id!(hover.on));
                    }
                    else {
                        self.animator_play(cx, id!(hover.off));
                    }
                    return;
                },
                Hit::FingerMove(fe) => {
                    self.show_thumb(cx);
                    let rel = fe.abs - fe.rect.pos;
                    // helper called by event code to scroll from a finger
                    if self.drag_point.is_none() {
                        // state should never occur.
                        //println!("Invalid state in scrollbar, fingerMove whilst drag_point is none")
                    }
                    else {
                        match self.axis {
                            ScrollAxis::Horizontal => {
                                if self.set_scroll_pos_from_finger(rel.x - self.drag_point.unwrap()){
                                    dispatch_action(cx, self.make_scroll_action());
                                }
                            },
                            ScrollAxis::Vertical => {
                                if self.set_scroll_pos_from_finger(rel.y - self.drag_point.unwrap()){
                                    dispatch_action(cx, self.make_scroll_action());
                                }
                            }
                        }
                    }
                 },
                _ => ()
            };
        }
    }
    
    pub fn draw_scroll_bar(&mut self, cx: &mut Cx2d, axis: ScrollAxis, view_rect: Rect, view_total: DVec2) -> f64 {
        
        self.axis = axis;
        self.draw_bg.opacity = if self.fades() { self.opacity } else { 1.0 };
        if !cfg!(any(target_os = "android", target_os = "ios")) {
            let theme_color = DESKTOP_THUMB_COLOR.with(|current| current.get());
            if self.theme_color != theme_color {
                self.theme_color = theme_color;
                if let Some(foreground) = theme_color {
                    let color = Vec4 { w: foreground.w * 0.54, ..foreground };
                    let color_hover = Vec4 { w: foreground.w * 0.72, ..foreground };
                    let color_drag = Vec4 { w: foreground.w * 0.90, ..foreground };
                    self.draw_bg.apply_over(cx, live! {
                        color: (color), color_hover: (color_hover), color_drag: (color_drag)
                    });
                }
            }
        }
        
        match self.axis {
            ScrollAxis::Horizontal => {
                self.visible = view_total.x > view_rect.size.x + 0.1;
                self.scroll_size = if view_total.y > view_rect.size.y + 0.1 {
                    view_rect.size.x - self.bar_size
                }
                else {
                    view_rect.size.x
                } -self.bar_side_margin * 2.;
                self.view_total = view_total.x;
                self.view_visible = view_rect.size.x;
                self.scroll_pos = self.scroll_pos.min(self.view_total - self.view_visible).max(0.);
                
                if self.visible {
                    let (norm_scroll, norm_handle) = self.get_normalized_scroll_pos();
                    self.draw_bg.is_vertical = 0.0;
                    self.draw_bg.norm_scroll = norm_scroll as f32;
                    self.draw_bg.norm_handle = norm_handle as f32;
                    let scroll = cx.turtle().scroll();
                    self.draw_bg.draw_rel(
                        cx,
                        Rect {
                            pos: dvec2(self.bar_side_margin, view_rect.size.y - self.bar_size) + scroll,
                            size: dvec2(self.scroll_size, self.bar_size),
                        }
                    );
                }
            },
            ScrollAxis::Vertical => {
                // compute if we need a horizontal one
                self.visible = view_total.y > view_rect.size.y + 0.1;
                self.scroll_size = if view_total.x > view_rect.size.x + 0.1 {
                    view_rect.size.y - self.bar_size
                }
                else {
                    view_rect.size.y
                } -self.bar_side_margin * 2.;
                self.view_total = view_total.y;
                self.view_visible = view_rect.size.y;
                self.scroll_pos = self.scroll_pos.min(self.view_total - self.view_visible).max(0.);
                if self.visible {
                    let (norm_scroll, norm_handle) = self.get_normalized_scroll_pos();
                    self.draw_bg.is_vertical = 1.0;
                    self.draw_bg.norm_scroll = norm_scroll as f32;
                    self.draw_bg.norm_handle = norm_handle as f32;
                    let scroll = cx.turtle().scroll();
                    self.draw_bg.draw_rel(
                        cx,
                        Rect {
                            pos: dvec2(view_rect.size.x - self.bar_size, self.bar_side_margin) + scroll,
                            size: dvec2(self.bar_size, self.scroll_size)
                        }
                    );
                }
            }
        }
        if !self.visible && self.fades() {
            self.stop_fade(cx);
            self.hovered = false;
            self.opacity = 0.0;
        }
        
        
        // see if we need to clamp
        let clamped_pos = self.scroll_pos.min(self.view_total - self.view_visible).max(0.);
        if clamped_pos != self.scroll_pos {
            self.scroll_pos = clamped_pos;
            self.scroll_target = clamped_pos;
            // ok so this means we 'scrolled' this can give a problem for virtual viewport widgets
            self.next_frame = cx.new_next_frame();
        }
        
        self.scroll_pos
    }
}

fn fade_opacity(elapsed: f64) -> f32 {
    (1.0 - elapsed / 0.3).clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod paseo_scrollbar_tests {
    use super::fade_opacity;

    #[test]
    fn fade_reaches_zero_and_stays_there() {
        assert_eq!(fade_opacity(0.0), 1.0);
        assert!((fade_opacity(0.15) - 0.5).abs() < f32::EPSILON);
        assert_eq!(fade_opacity(0.3), 0.0);
        assert_eq!(fade_opacity(2.0), 0.0);
    }
}