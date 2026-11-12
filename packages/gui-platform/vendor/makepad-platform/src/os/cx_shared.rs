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
    /// window's coordinates. Backends that know the window (Windows, X11) dispatch drag
    /// and drop events through `call_window_event_handler`; other windows must not
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
        if self.route_text_handle_event(event) {
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
    }

    fn dismiss_text_selection_for_event(&mut self, event: &Event) {
        let owner = self.keyboard.text_selection;
        if owner == Area::Empty { return; }
        match event {
            Event::MouseDown(_) => {}
            Event::TouchUpdate(update) if update.touches.iter().any(|touch| matches!(touch.state, TouchState::Start)) => {}
            Event::AppLostFocus | Event::Pause | Event::Background | Event::Shutdown if cfg!(target_os = "android") => {}
            _ => return,
        }
        let valid = owner.is_valid(self);
        let rect = if valid { owner.clipped_rect(self) } else { Default::default() };
        let window = if valid {
            owner.draw_list_id()
                .and_then(|id| self.draw_lists[id].pass_id)
                .and_then(|id| self.get_pass_window_id(id))
        } else {
            None
        };
        let outside = match event {
            Event::MouseDown(event) => window != Some(event.window_id) || !rect.contains(event.abs),
            Event::TouchUpdate(event) => event.touches.iter().any(|touch| {
                matches!(touch.state, TouchState::Start)
                    && (window != Some(event.window_id) || !rect.contains(touch.abs))
            }),
            _ => false,
        };
        let leaving = cfg!(target_os = "android")
            && matches!(event, Event::AppLostFocus | Event::Pause | Event::Background | Event::Shutdown);
        if outside || leaving {
            self.clear_text_selection(owner);
            self.clear_key_focus_if(owner);
            // Deliver loss before the pointer so the old owner cannot erase a
            // new selection or swallow the outside target's normal action.
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

        self.call_event_handler(&Event::Draw(draw_event));
        self.in_draw_event = false;
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
