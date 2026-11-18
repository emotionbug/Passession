use {
    std::collections::{HashSet, HashMap},
    crate::{
        area::Area,
        cx_api::CxOsApi,
        cx::Cx,
        window::WindowId,
        pass::{
            PassId,
            CxPassParent
        },
        event::{
            TimerEvent,
            DrawEvent,
            TriggerEvent,
            Event,
            KeyFocusEvent,
            NextFrameEvent,
            TouchState,
            finger::{TAP_COUNT_DISTANCE, TAP_COUNT_TIME},
            keyboard::TextSelectionOutsideTap,
        },
        studio::{AppToStudio,EventSample},
    }
};

impl Cx {
    fn uses_window_keyboard_routing(&self) -> bool {
        !matches!(self.os_type, crate::cx::OsType::Ios | crate::cx::OsType::Android(_)
            | crate::cx::OsType::OpenHarmony(_) | crate::cx::OsType::Web(_)
            | crate::cx::OsType::LinuxDirect)
    }

    pub(crate) fn area_window_id(&self, area: Area) -> Option<WindowId> {
        let draw_list_id = area.draw_list_id()?;
        let pass_id = self.draw_lists.checked_index(draw_list_id)?.pass_id?;
        self.get_pass_window_id(pass_id)
    }

    /// Raw input has exactly one native source. Derived lifecycle events broadcast.
    pub fn keyboard_event_is_for_window(&self, event: &Event, window_id: WindowId) -> bool {
        !event.is_keyboard_input() || !self.uses_window_keyboard_routing()
            || (self.keyboard_event_window.or(self.keyboard_window) == Some(window_id)
                && self.windows.is_created(window_id))
    }

    pub(crate) fn keyboard_event_is_for_area(&self, event: &Event, area: Area) -> bool {
        !self.uses_window_keyboard_routing() || self.area_window_id(area)
            .is_some_and(|window_id| self.keyboard_event_is_for_window(event, window_id))
    }

    /// Paseo: a native drag has one target window and reports its position in that
    /// window's coordinates. Desktop backends dispatch drag and drop events through
    /// `call_window_event_handler`; other windows must not
    /// hit-test them. A source-less drag event still reaches every window.
    pub fn drag_event_is_for_window(&self, event: &Event, window_id: WindowId) -> bool {
        !matches!(event, Event::Drag(_) | Event::Drop(_))
            || self.keyboard_event_window.is_none_or(|source| source == window_id)
    }

    /// Dispatch native or explicitly addressed synthetic input without changing wire payloads.
    pub fn call_window_event_handler(&mut self, window_id: WindowId, event: &Event) {
        if !self.windows.is_created(window_id) { return; }
        let previous = self.keyboard_event_window.replace(window_id);
        match event {
            Event::KeyDown(key) => self.keyboard.process_key_down(*key),
            Event::KeyUp(key) => self.keyboard.process_key_up(*key),
            _ => {}
        }
        self.call_event_handler(event);
        self.keyboard_event_window = previous;
    }

    /// Source-less legacy hosts cannot distinguish two live windows; never guess.
    /// Only the desktop stdin transports have such a host.
    #[cfg(any(test, not(any(target_os = "android", target_os = "ios", target_os = "tvos", target_env = "ohos"))))]
    pub(crate) fn call_single_window_event_handler(&mut self, event: &Event) {
        if let Some(window_id) = self.windows.single_created_window() {
            self.call_window_event_handler(window_id, event);
        }
    }

    /// Native focus is independent of editable focus and can change through OS Alt-Tab.
    pub fn set_window_keyboard_focus(&mut self, window_id: WindowId, focused: bool) {
        if !self.windows.is_valid(window_id)
            || (focused && !self.windows.is_created(window_id))
            || (focused && self.keyboard_window == Some(window_id))
            || (!focused && self.keyboard_window != Some(window_id))
        {
            return;
        }
        let pending = self.keyboard.next_key_focus;
        let owner = self.area_window_id(pending).or_else(|| {
            (pending == Area::Empty).then_some(self.keyboard_window).flatten()
        });
        if let Some(owner) = owner.filter(|id| self.windows.is_valid(*id)) {
            let previous = self.keyboard.prev_key_focus;
            let previous = if self.area_window_id(previous) == Some(owner) {
                previous
            } else { Area::Empty };
            self.windows[owner].keyboard_focus = pending;
            self.windows[owner].keyboard_previous_focus = previous;
        }
        self.keyboard_window = focused.then_some(window_id);
        self.keyboard.keys_down.clear();
        let (focus, previous) = if focused {
            let window = &self.windows[window_id];
            (window.keyboard_focus, window.keyboard_previous_focus)
        } else { (Area::Empty, Area::Empty) };
        let belongs = |area: Area| area.is_valid(self)
            && self.area_window_id(area) == Some(window_id);
        let focus = if belongs(focus) { focus } else { Area::Empty };
        let previous = if belongs(previous) { previous } else { Area::Empty };
        self.keyboard.set_key_focus(focus);
        self.inner_key_focus_change();
        // Revert-focus must not jump back into a different native window.
        self.keyboard.prev_key_focus = previous;
    }

    #[allow(dead_code)]
    pub (crate) fn repaint_windows(&mut self) {
        for pass_id in self.passes.id_iter() {
            match self.passes[pass_id].parent {
                CxPassParent::Window(_) => {
                    self.passes[pass_id].paint_dirty = true;
                    self.passes[pass_id].damage_epoch = self.passes[pass_id].damage_epoch.wrapping_add(1);
                },
                _ => ()
            }
        }
    }
    
    #[allow(unused)]
    pub (crate) fn any_passes_dirty(&self) -> bool {
        for pass_id in self.passes.id_iter() {
            if self.passes[pass_id].paint_dirty {
                return true
            }
        }
        false
    }
    
    pub (crate) fn compute_pass_repaint_order(&mut self, passes_todo: &mut Vec<PassId>) {
        passes_todo.clear();
        
        // we need this because we don't mark the entire deptree of passes dirty every small paint
        loop { // loop untill we don't propagate anymore
            let mut altered = false;
            for pass_id in self.passes.id_iter(){
                if self.demo_time_repaint {
                    if self.passes[pass_id].main_draw_list_id.is_some(){
                        self.passes[pass_id].paint_dirty = true;
                    }
                }
                if self.passes[pass_id].paint_dirty {
                    let other = match self.passes[pass_id].parent {
                        CxPassParent::Pass(parent_pass_id) => {
                            Some(parent_pass_id)
                        }
                        _ => None
                    };
                    if let Some(other) = other {
                        if !self.passes[other].paint_dirty {
                            self.passes[other].paint_dirty = true;
                            altered = true;
                        }
                    }
                }
            }
            if !altered {
                break
            }
        }
        
        for pass_id in self.passes.id_iter(){
            if self.passes[pass_id].paint_dirty {
                let mut inserted = false;
                match self.passes[pass_id].parent {
                    CxPassParent::Window(_) | CxPassParent::Xr=> {
                    },
                    CxPassParent::Pass(dep_of_pass_id) => {
                        if pass_id == dep_of_pass_id {
                            panic!()
                        }
                        for insert_before in 0..passes_todo.len() {
                            if passes_todo[insert_before] == dep_of_pass_id {
                                passes_todo.insert(insert_before, pass_id);
                                inserted = true;
                                break;
                            }
                        }
                    },
                    CxPassParent::None => { // we need to be first
                        passes_todo.insert(0, pass_id);
                        inserted = true;
                    },
                }
                if !inserted {
                    passes_todo.push(pass_id);
                }
            }
        }
        self.demo_time_repaint = false;
    }
    
    pub (crate) fn need_redrawing(&self) -> bool {
        self.new_draw_event.will_redraw() 
    }
    
    
    
    
    // event handler wrappers
    
    
    pub (crate) fn inner_call_event_handler(&mut self, event: &Event) {
        self.event_id += 1;
        let trace_start = self.frame_trace.as_ref().map(|_| std::time::Instant::now());
        let timer_owner = self.frame_trace.as_mut().and_then(|trace| trace.timer_owner(event));
        if Cx::has_studio_web_socket(){
            let start = self.seconds_since_app_start();
            let mut event_handler = self.event_handler.take().unwrap();
            event_handler(self, event);
            self.event_handler = Some(event_handler);
            let end = self.seconds_since_app_start();
            Cx::send_studio_message(AppToStudio::EventSample(EventSample{
                event_u32: event.to_u32(),
                start: start,
                event_meta: if let Event::Timer(TimerEvent{timer_id,..}) = event{*timer_id}else{0},
                end: end
            }))
        }
        else{
            let mut event_handler = self.event_handler.take().unwrap();
            event_handler(self, event);
            self.event_handler = Some(event_handler);
        }
        if let (Some(trace), Some(start)) = (&mut self.frame_trace, trace_start) {
            trace.event(event, timer_owner, start.elapsed());
        }

        // Reset widget query invalidation after all views have processed it.
        // We wait until event_id is at least 1 events past the invalidation event
        // to ensure the cache clear has propagated through the widget hierarchy
        // during the previous event cycle.
        if let Some(event_id) = self.widget_query_invalidation_event {
            if self.event_id > event_id + 1 {
                self.widget_query_invalidation_event = None;
            }
        }
    }
    
    fn inner_key_focus_change(&mut self) {
        if let Some((prev, focus)) = self.keyboard.cycle_key_focus_changed(){
            self.clear_text_selection(prev);
            self.inner_call_event_handler(&Event::KeyFocus(KeyFocusEvent {
                prev,
                focus
            }));
        }
    }
    
    pub fn handle_triggers(&mut self) {
        // post op events like signals, triggers and key-focus
        let mut counter = 0;
        while self.triggers.len() != 0 {
            counter += 1;
            let mut triggers = HashMap::new();
            std::mem::swap(&mut self.triggers, &mut triggers);
            self.inner_call_event_handler(&Event::Trigger(TriggerEvent {
                triggers: triggers,
            }));
            self.inner_key_focus_change();
            if counter > 100 {
                crate::error!("Trigger feedback loop detected");
                break
            }
        }
    }
    
    pub fn handle_actions(&mut self) {
        // post op events like signals, triggers and key-focus
        let mut counter = 0;
        while self.new_actions.len() != 0 {
            counter += 1;
            let mut actions = Vec::new();
            std::mem::swap(&mut self.new_actions, &mut actions);
            self.inner_call_event_handler(&Event::Actions(actions));
            self.inner_key_focus_change();
            if counter > 100 {
                crate::error!("Action feedback loop detected");
                crate::error!("New actions {:#?}", self.new_actions);
                break
            }
        }
    }
    
    pub (crate) fn call_event_handler(&mut self, event: &Event) {
        if let Event::WindowClosed(closed) = event {
            let window_id = closed.window_id;
            if self.windows.is_valid(window_id) {
                self.set_window_keyboard_focus(window_id, false);
                let window = &mut self.windows[window_id];
                window.is_created = false;
                window.keyboard_focus = Area::Empty;
                window.keyboard_previous_focus = Area::Empty;
            }
        }
        if event.is_keyboard_input() && self.uses_window_keyboard_routing()
            && self.keyboard_event_window.is_none()
        {
            if let Some(window_id) = self.keyboard_window {
                self.call_window_event_handler(window_id, event);
            }
            return;
        }
        self.pointer_event_starting(event);
        if self.route_text_handle_event(event) {
            self.keyboard.text_selection_outside_tap = None;
            self.inner_key_focus_change();
            self.handle_triggers();
            self.handle_actions();
            return;
        }
        self.dismiss_text_selection_for_event(event);
        self.inner_call_event_handler(event);
        self.inner_key_focus_change();
        self.handle_triggers();
        self.handle_actions();
        self.pointer_event_finished(event);
    }

    fn dismiss_text_selection_for_event(&mut self, event: &Event) {
        let owner = self.keyboard.text_selection;
        let leaving = matches!(event, Event::AppLostFocus | Event::Pause | Event::Background | Event::Shutdown);
        if leaving || matches!(event, Event::MouseDown(_) | Event::Scroll(_)) {
            self.keyboard.text_selection_outside_tap = None;
        }
        if let Event::LongPress(long) = event {
            if self.keyboard.text_selection_outside_tap.is_some_and(|tap| {
                tap.window_id == long.window_id && tap.uid == long.uid
            }) {
                self.keyboard.text_selection_outside_tap = None;
            }
            return;
        }
        if owner == Area::Empty {
            self.keyboard.text_selection_outside_tap = None;
            return;
        }
        if !matches!(event, Event::MouseDown(_) | Event::TouchUpdate(_))
            && !(leaving && cfg!(target_os = "android"))
        {
            return;
        }
        let valid = owner.is_valid(self);
        let rect = if valid { owner.clipped_rect(self) } else { Default::default() };
        let window = if valid { self.area_window_id(owner) } else { None };
        let outside = match event {
            Event::MouseDown(event) => window != Some(event.window_id) || !rect.contains(event.abs),
            Event::TouchUpdate(update) => {
                // Match FingerUpEvent::was_tap: slop cancellation is sticky,
                // including a drag that returns to its starting position.
                let pending = self.keyboard.text_selection_outside_tap.take();
                if update.cancelled || update.touches.len() != 1 { return; }
                let touch = &update.touches[0];
                if let Some(tap) = pending {
                    if tap.owner != owner || tap.window_id != update.window_id
                        || tap.uid != touch.uid || matches!(touch.state, TouchState::Start)
                        || update.time - tap.time >= TAP_COUNT_TIME
                        || (touch.abs - tap.abs_start).length() >= TAP_COUNT_DISTANCE
                    {
                        return;
                    }
                    if !matches!(touch.state, TouchState::Stop) {
                        self.keyboard.text_selection_outside_tap = Some(tap);
                        return;
                    }
                    window != Some(update.window_id) || !rect.contains(touch.abs)
                } else {
                    if matches!(touch.state, TouchState::Start)
                        && (window != Some(update.window_id) || !rect.contains(touch.abs))
                    {
                        self.keyboard.text_selection_outside_tap = Some(TextSelectionOutsideTap {
                            owner, window_id: update.window_id, uid: touch.uid,
                            abs_start: touch.abs, time: update.time,
                        });
                    }
                    return;
                }
            }
            _ => false,
        };
        if outside || (leaving && cfg!(target_os = "android")) {
            self.clear_text_selection(owner);
            self.clear_key_focus_if(owner);
            // Deliver loss before the completed tap/click so the old owner
            // cannot erase a new selection or swallow the target's action.
            self.inner_key_focus_change();
        }
    }

    // helpers
    
    /*
    pub (crate) fn call_all_keys_up(&mut self) {
        let keys_down = self.keyboard.all_keys_up();
        for key_event in keys_down {
            self.call_event_handler(&Event::KeyUp(key_event))
        }
    }*/ 
    
    pub (crate) fn call_draw_event(&mut self) {
        let mut draw_event = DrawEvent::default();
        std::mem::swap(&mut draw_event, &mut self.new_draw_event);
        self.in_draw_event = true;
        let started = std::time::Instant::now();

        self.call_event_handler(&Event::Draw(draw_event));
        self.in_draw_event = false;
        // Paseo: what the frame moved under a still pointer is hovered (`crate::pointer`).
        self.rehover_after_draw(started);
    }

    pub (crate) fn call_next_frame_event(&mut self, time: f64) {
        let mut set = HashSet::default();
        std::mem::swap(&mut set, &mut self.new_next_frames);

        self.performance_stats.process_frame_data(time);

        self.call_event_handler(&Event::NextFrame(NextFrameEvent {set, time: time, frame: self.repaint_id}));
    }
}

#[cfg(test)]
mod window_keyboard_tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, rc::Rc};
    use crate::{
        area::RectArea,
        draw_list::DrawList,
        event::{Hit, KeyCode, KeyEvent, TextInputEvent, WindowClosedEvent},
        live_traits::LiveNew,
        pass::Pass,
        window::WindowHandle,
    };

    fn window(cx: &mut Cx) -> WindowHandle {
        let handle = WindowHandle::new(cx);
        cx.windows[handle.window_id()].is_created = true;
        handle
    }

    fn editor(cx: &mut Cx, window: WindowId) -> (Pass, DrawList, Area) {
        let pass = Pass::new(cx);
        cx.passes[pass.pass_id()].parent = CxPassParent::Window(window);
        let list = DrawList::new(cx);
        cx.draw_lists[list.id()].pass_id = Some(pass.pass_id());
        cx.draw_lists[list.id()].redraw_id = 1;
        let area = Area::Rect(RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        (pass, list, area)
    }

    fn escape() -> Event {
        Event::KeyDown(KeyEvent { key_code: KeyCode::Escape, ..Default::default() })
    }

    #[test]
    fn escape_changes_only_source_window_but_lifecycle_reaches_both() {
        let modals = Rc::new(RefCell::new(Vec::<(WindowId, Vec<u8>, usize)>::new()));
        let observed = modals.clone();
        let mut cx = Cx::new(Box::new(move |cx, event| {
            for (window, stack, signals) in observed.borrow_mut().iter_mut() {
                if !cx.keyboard_event_is_for_window(event, *window) { continue; }
                match event {
                    Event::KeyDown(key) if key.key_code == KeyCode::Escape => { stack.pop(); }
                    Event::Signal => *signals += 1,
                    _ => {}
                }
            }
        }));
        let first = window(&mut cx);
        let second = window(&mut cx);
        let first_id = first.window_id();
        let second_id = second.window_id();
        modals.borrow_mut().extend([(first_id, vec![1, 2], 0), (second_id, vec![3, 4], 0)]);
        cx.set_window_keyboard_focus(first_id, true);
        // The originating native window wins even if editable/native focus is stale.
        cx.call_window_event_handler(second_id, &escape());
        assert_eq!(modals.borrow()[0].1, vec![1, 2]);
        assert_eq!(modals.borrow()[1].1, vec![3]);
        cx.call_window_event_handler(second_id, &Event::Signal);
        assert_eq!(modals.borrow()[0].2, 1);
        assert_eq!(modals.borrow()[1].2, 1);
        assert_eq!(cx.keyboard_event_window, None);
    }

    #[test]
    fn inactive_drop_target_is_exclusive_without_changing_keyboard_focus() {
        use std::sync::{Arc, Mutex};
        use crate::event::{DragEvent, DragItem, DragResponse, DropEvent};
        let targets = Rc::new(RefCell::new(Vec::<(WindowId, usize)>::new()));
        let observed = targets.clone();
        let mut cx = Cx::new(Box::new(move |cx, event| {
            for (window, drops) in observed.borrow_mut().iter_mut() {
                if !cx.drag_event_is_for_window(event, *window) { continue; }
                match event {
                    Event::Drag(drag) => *drag.response.lock().unwrap() = DragResponse::Copy,
                    Event::Drop(drop) => {
                        *drops += 1;
                        *drop.handled.lock().unwrap() = true;
                    }
                    _ => {}
                }
            }
        }));
        let focused = window(&mut cx);
        let target = window(&mut cx);
        targets.borrow_mut().extend([(focused.window_id(), 0), (target.window_id(), 0)]);
        cx.set_window_keyboard_focus(focused.window_id(), true);
        let response = Arc::new(Mutex::new(DragResponse::None));
        let handled = Arc::new(Mutex::new(false));
        let items = Arc::new(vec![DragItem::FilePath { path: "/tmp/a.txt".into(), internal_id: None }]);
        cx.call_window_event_handler(target.window_id(), &Event::Drag(DragEvent {
            modifiers: Default::default(), handled: Arc::new(Mutex::new(false)),
            abs: Default::default(), items: items.clone(), response: response.clone(),
        }));
        assert_eq!(*response.lock().unwrap(), DragResponse::Copy);
        cx.call_window_event_handler(target.window_id(), &Event::Drop(DropEvent {
            modifiers: Default::default(), handled: handled.clone(), abs: Default::default(), items,
        }));
        assert!(*handled.lock().unwrap());
        assert_eq!(*targets.borrow(), vec![(focused.window_id(), 0), (target.window_id(), 1)]);
        assert_eq!(cx.keyboard_window, Some(focused.window_id()));
        assert_eq!(cx.keyboard_event_window, None);
    }

    #[test]
    fn os_focus_switch_restores_typing_and_tracks_inactive_area_remaps() {
        let editors = Rc::new(RefCell::new(Vec::<(Area, String)>::new()));
        let observed = editors.clone();
        let mut cx = Cx::new(Box::new(move |cx, event| {
            for (area, text) in observed.borrow_mut().iter_mut() {
                if let Hit::TextInput(input) = event.hits(cx, *area) {
                    text.push_str(&input.input);
                }
            }
        }));
        let first = window(&mut cx);
        let second = window(&mut cx);
        let (_first_pass, first_list, first_area) = editor(&mut cx, first.window_id());
        let (_second_pass, _second_list, second_area) = editor(&mut cx, second.window_id());
        editors.borrow_mut().extend([(first_area, String::new()), (second_area, String::new())]);
        let type_text = |cx: &mut Cx, id, input: &str| {
            cx.call_window_event_handler(id, &Event::TextInput(TextInputEvent {
                input: input.into(), replace_last: false, was_paste: false,
            }));
        };
        cx.set_window_keyboard_focus(first.window_id(), true);
        cx.set_key_focus(first_area);
        cx.call_event_handler(&Event::Signal);
        type_text(&mut cx, first.window_id(), "a");
        cx.set_window_keyboard_focus(second.window_id(), true);
        cx.set_key_focus(second_area);
        cx.call_event_handler(&Event::Signal);
        type_text(&mut cx, second.window_id(), "b");

        let remapped = Area::Rect(RectArea {
            draw_list_id: first_list.id(), rect_id: 0, redraw_id: 2,
        });
        cx.draw_lists[first_list.id()].redraw_id = 2;
        cx.update_area_refs(first_area, remapped);
        editors.borrow_mut()[0].0 = remapped;
        cx.set_window_keyboard_focus(first.window_id(), true);
        // An old window's late focus loss cannot clear the newer OS owner.
        cx.set_window_keyboard_focus(second.window_id(), false);
        assert!(cx.has_key_focus(remapped));
        type_text(&mut cx, second.window_id(), "wrong-window");
        type_text(&mut cx, first.window_id(), "c");
        assert_eq!(editors.borrow()[0].1, "ac");
        assert_eq!(editors.borrow()[1].1, "b");

        cx.set_window_keyboard_focus(second.window_id(), true);
        cx.clear_key_focus_if(remapped);
        cx.set_window_keyboard_focus(first.window_id(), true);
        assert!(!cx.has_key_focus(remapped));
        type_text(&mut cx, first.window_id(), "closed-editor");
        assert_eq!(editors.borrow()[0].1, "ac");
    }

    #[test]
    fn closed_or_recycled_source_cannot_dispatch_or_change_held_keys() {
        let key_count = Rc::new(Cell::new(0));
        let observed = key_count.clone();
        let mut cx = Cx::new(Box::new(move |_, event| {
            if matches!(event, Event::KeyDown(_)) { observed.set(observed.get() + 1); }
        }));
        let original = window(&mut cx);
        let old_id = original.window_id();
        cx.set_window_keyboard_focus(old_id, true);
        cx.call_window_event_handler(old_id, &escape());
        assert_eq!(key_count.get(), 1);
        cx.call_event_handler(&Event::WindowClosed(WindowClosedEvent { window_id: old_id }));
        cx.set_window_keyboard_focus(old_id, true);
        assert_eq!(cx.keyboard_window, None);
        cx.call_window_event_handler(old_id, &escape());
        assert_eq!(key_count.get(), 1);
        assert!(!cx.keyboard.is_key_down(KeyCode::Escape));

        drop(original);
        let replacement = window(&mut cx);
        let new_id = replacement.window_id();
        assert_eq!(old_id.0, new_id.0);
        assert_ne!(old_id.1, new_id.1);
        cx.call_window_event_handler(old_id, &escape());
        cx.set_window_keyboard_focus(new_id, true);
        cx.set_window_keyboard_focus(old_id, false);
        assert_eq!(cx.keyboard_window, Some(new_id));
        cx.call_window_event_handler(new_id, &escape());
        assert_eq!(key_count.get(), 2);
    }

    #[test]
    fn legacy_input_requires_one_live_window_not_first_or_focused_window() {
        let key_count = Rc::new(Cell::new(0));
        let observed = key_count.clone();
        let mut cx = Cx::new(Box::new(move |_, event| {
            if matches!(event, Event::KeyDown(_)) { observed.set(observed.get() + 1); }
        }));
        let first = window(&mut cx);
        cx.call_single_window_event_handler(&escape());
        assert_eq!(key_count.get(), 1);
        let second = window(&mut cx);
        cx.set_window_keyboard_focus(second.window_id(), true);
        cx.call_single_window_event_handler(&escape());
        assert_eq!(key_count.get(), 1);
        cx.call_event_handler(&Event::WindowClosed(WindowClosedEvent { window_id: first.window_id() }));
        cx.call_single_window_event_handler(&escape());
        assert_eq!(key_count.get(), 2);
    }
}

#[cfg(test)]
mod outside_text_selection_tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, rc::Rc};
    use crate::{
        area::RectArea,
        draw_list::{CxRectArea, DrawList},
        event::{LongPressEvent, MouseButton, MouseDownEvent, ScrollEvent, TouchPoint, TouchUpdateEvent,
            TextHandleEndpoint, TextHandleTarget},
        live_traits::LiveNew,
        makepad_math::{dvec2, Rect},
        pass::Pass,
        window::WindowHandle,
    };

    fn editor(cx: &mut Cx, window: WindowId, x: f64) -> (Pass, DrawList, Area) {
        let pass = Pass::new(cx);
        cx.passes[pass.pass_id()].parent = CxPassParent::Window(window);
        let list = DrawList::new(cx);
        cx.draw_lists[list.id()].pass_id = Some(pass.pass_id());
        cx.draw_lists[list.id()].redraw_id = 1;
        cx.draw_lists[list.id()].rect_areas.push(CxRectArea {
            rect: Rect { pos: dvec2(x, 0.0), size: dvec2(100.0, 100.0) },
            draw_clip: (dvec2(-1000.0, -1000.0), dvec2(1000.0, 1000.0)),
        });
        let area = Area::Rect(RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        (pass, list, area)
    }

    fn focus(cx: &mut Cx, area: Area) {
        cx.set_key_focus(area);
        cx.inner_key_focus_change();
        cx.claim_text_selection(area);
    }

    fn touch(window_id: WindowId, state: TouchState, x: f64, time: f64) -> Event {
        Event::TouchUpdate(TouchUpdateEvent {
            window_id, time, cancelled: false, modifiers: Default::default(),
            touches: vec![TouchPoint {
                state, abs: dvec2(x, 50.0), time, uid: 1, rotation_angle: 0.0,
                force: 0.0, radius: Default::default(),
                handled: Cell::new(Area::Empty), sweep_lock: Cell::new(Area::Empty),
            }],
        })
    }

    #[test]
    fn outside_pan_preserves_editor_even_after_returning_to_origin() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let window = WindowHandle::new(&mut cx);
        let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
        focus(&mut cx, owner);
        for (state, x, time) in [
            (TouchState::Start, 150.0, 0.0),
            (TouchState::Move, 150.0 + TAP_COUNT_DISTANCE, 0.1),
            (TouchState::Move, 150.0, 0.2),
            (TouchState::Stop, 150.0, 0.3),
        ] {
            cx.call_event_handler(&touch(window.window_id(), state, x, time));
            assert!(cx.has_key_focus(owner));
            assert_eq!(cx.keyboard.text_selection, owner);
        }
    }

    #[test]
    fn outside_tap_blurs_once_before_dispatching_release_once() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let mut cx = Cx::new(Box::new(move |cx, event| {
            match event {
                Event::KeyFocus(change) if change.focus == Area::Empty => observed.borrow_mut().push("blur"),
                Event::TouchUpdate(update) if matches!(update.touches[0].state, TouchState::Stop) => {
                    assert_eq!(cx.keyboard.key_focus, Area::Empty);
                    observed.borrow_mut().push("release");
                }
                _ => {}
            }
        }));
        let window = WindowHandle::new(&mut cx);
        let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
        focus(&mut cx, owner);
        cx.call_event_handler(&touch(window.window_id(), TouchState::Start, 150.0, 0.0));
        assert!(cx.has_key_focus(owner));
        cx.call_event_handler(&touch(window.window_id(), TouchState::Move, 151.0, 0.1));
        cx.call_event_handler(&touch(window.window_id(), TouchState::Stop, 151.0, 0.2));
        assert_eq!(cx.keyboard.text_selection, Area::Empty);
        assert_eq!(&*events.borrow(), &["blur", "release"]);
    }

    #[test]
    fn outside_tap_cannot_clear_new_or_pending_editor_focus() {
        for cycle_before_release in [false, true] {
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let window = WindowHandle::new(&mut cx);
            let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
            let (_other_pass, _other_list, other) = editor(&mut cx, window.window_id(), 120.0);
            focus(&mut cx, owner);
            cx.call_event_handler(&touch(window.window_id(), TouchState::Start, 150.0, 0.0));
            cx.set_key_focus(other);
            if cycle_before_release { cx.inner_key_focus_change(); }
            cx.claim_text_selection(other);
            cx.call_event_handler(&touch(window.window_id(), TouchState::Stop, 150.0, 0.1));
            assert!(cx.has_key_focus(other));
            assert_eq!(cx.keyboard.text_selection, other);
        }
    }

    #[test]
    fn cancelled_long_press_scroll_and_multitouch_do_not_blur() {
        for cancellation in 0..6 {
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let window = WindowHandle::new(&mut cx);
            let id = window.window_id();
            let (_pass, _list, owner) = editor(&mut cx, id, 0.0);
            focus(&mut cx, owner);
            cx.call_event_handler(&touch(id, TouchState::Start, 150.0, 0.0));
            let mut cancelled = touch(id, TouchState::Move, 150.0, 0.1);
            if let Event::TouchUpdate(update) = &mut cancelled {
                match cancellation {
                    0 => update.cancelled = true,
                    1 => {
                        let mut second = update.touches[0].clone();
                        second.uid = 2;
                        second.state = TouchState::Start;
                        update.touches.push(second);
                    }
                    2 => update.time = TAP_COUNT_TIME,
                    3 => update.touches.clear(),
                    _ => {}
                }
            }
            if cancellation == 4 {
                cancelled = Event::LongPress(LongPressEvent { window_id: id, uid: 1, abs: dvec2(150.0, 50.0), time: 0.1 });
            } else if cancellation == 5 {
                cancelled = Event::Scroll(ScrollEvent {
                    window_id: id, abs: dvec2(150.0, 50.0), scroll: dvec2(0.0, 10.0),
                    modifiers: Default::default(), handled_x: Cell::new(false), handled_y: Cell::new(false),
                    is_mouse: false, time: 0.1,
                });
            }
            cx.call_event_handler(&cancelled);
            cx.call_event_handler(&touch(id, TouchState::Stop, 150.0,
                if cancellation == 2 { TAP_COUNT_TIME + 0.1 } else { 0.2 }));
            assert!(cx.has_key_focus(owner), "cancellation {cancellation}");
            assert_eq!(cx.keyboard.text_selection, owner);
            assert!(cx.keyboard.text_selection_outside_tap.is_none());
        }
    }

    #[test]
    fn inside_tap_and_outside_release_inside_editor_do_not_blur() {
        for (start, stop) in [(50.0, 50.0), (101.0, 99.0)] {
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let window = WindowHandle::new(&mut cx);
            let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
            focus(&mut cx, owner);
            cx.call_event_handler(&touch(window.window_id(), TouchState::Start, start, 0.0));
            cx.call_event_handler(&touch(window.window_id(), TouchState::Stop, stop, 0.1));
            assert!(cx.has_key_focus(owner));
            assert_eq!(cx.keyboard.text_selection, owner);
        }
    }

    #[test]
    fn pending_outside_tap_follows_redrawn_owner() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let window = WindowHandle::new(&mut cx);
        let (_pass, list, owner) = editor(&mut cx, window.window_id(), 0.0);
        focus(&mut cx, owner);
        cx.call_event_handler(&touch(window.window_id(), TouchState::Start, 150.0, 0.0));
        let remapped = Area::Rect(RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 2 });
        cx.draw_lists[list.id()].redraw_id = 2;
        cx.update_area_refs(owner, remapped);
        cx.call_event_handler(&touch(window.window_id(), TouchState::Stop, 150.0, 0.1));
        assert_eq!(cx.keyboard.key_focus, Area::Empty);
        assert_eq!(cx.keyboard.text_selection, Area::Empty);
    }

    #[test]
    fn outside_mouse_down_still_blurs_immediately() {
        let mut cx = Cx::new(Box::new(|_, _| {}));
        let window = WindowHandle::new(&mut cx);
        let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
        focus(&mut cx, owner);
        cx.call_event_handler(&Event::MouseDown(MouseDownEvent {
            window_id: window.window_id(), abs: dvec2(150.0, 50.0), button: MouseButton::PRIMARY,
            modifiers: Default::default(), handled: Cell::new(Area::Empty), time: 0.0,
        }));
        assert_eq!(cx.keyboard.key_focus, Area::Empty);
        assert_eq!(cx.keyboard.text_selection, Area::Empty);
    }

    #[test]
    fn selection_handle_outside_editor_keeps_its_route_and_focus() {
        let phases = Rc::new(RefCell::new(Vec::new()));
        let observed = phases.clone();
        let mut cx = Cx::new(Box::new(move |_, event| {
            if let Event::TextHandle(handle) = event { observed.borrow_mut().push(handle.action.phase); }
        }));
        let window = WindowHandle::new(&mut cx);
        let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
        let (_handle_pass, _handle_list, handle) = editor(&mut cx, window.window_id(), 120.0);
        focus(&mut cx, owner);
        cx.register_text_handles(1, owner, Rect { pos: dvec2(0.0, 0.0), size: dvec2(100.0, 100.0) },
            [Some(TextHandleTarget { area: handle, endpoint: TextHandleEndpoint::Cursor, caret_offset: Default::default() }), None]);
        cx.call_event_handler(&touch(window.window_id(), TouchState::Start, 150.0, 0.0));
        assert!(cx.keyboard.text_selection_outside_tap.is_none());
        cx.call_event_handler(&touch(window.window_id(), TouchState::Stop, 150.0, 0.1));
        assert!(cx.has_key_focus(owner));
        assert_eq!(cx.keyboard.text_selection, owner);
        assert_eq!(&*phases.borrow(), &[crate::event::TextHandlePhase::Start, crate::event::TextHandlePhase::End]);
    }

    #[cfg(target_os = "android")]
    #[test]
    fn lifecycle_blur_is_not_deferred_with_the_outside_tap() {
        for event in [Event::AppLostFocus, Event::Pause, Event::Background, Event::Shutdown] {
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let window = WindowHandle::new(&mut cx);
            let (_pass, _list, owner) = editor(&mut cx, window.window_id(), 0.0);
            focus(&mut cx, owner);
            cx.call_event_handler(&touch(window.window_id(), TouchState::Start, 150.0, 0.0));
            cx.call_event_handler(&event);
            assert_eq!(cx.keyboard.key_focus, Area::Empty);
            assert_eq!(cx.keyboard.text_selection, Area::Empty);
            assert!(cx.keyboard.text_selection_outside_tap.is_none());
        }
    }
}
