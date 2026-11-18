use super::metal_surface::{self, Acquire, SurfaceKey, SurfacePresenter};

use {
    std::{
        rc::Rc,
        cell::RefCell,
        time::Instant
    },
    makepad_objc_sys::{
        msg_send,
        sel,
        sel_impl,
    },
    crate::{
        makepad_live_id::*,
        makepad_math::*,
        os::{
            cx_native::EventFlow,
            apple::{
                apple_sys::*,
                macos::{
                    macos_event::MacosEvent,
                    macos_app::{
                        MacosApp,
                        with_macos_app,
                        init_macos_app_global
                    },
                    macos_window::MacosWindow
                },
                apple_classes::init_apple_classes_global,
                url_session::AppleHttpRequests,
            },
            metal_xpc::start_xpc_service,
            apple_media::CxAppleMedia,
            metal::{MetalCx, DrawPassMode},
        },
        pass::{CxPassParent, PassId},
        thread::SignalToUI,
        cx_stdin::PollTimers,
        window::WindowId,
        area::Area,
        event::{
            WindowGeom,
            MouseButton,
            MouseUpEvent,
            Event,
            TextImeEvent,
            NetworkResponseChannel
        },
        cx_api::{CxOsApi, CxOsOp, OpenUrlInPlace},
        cx::{Cx, OsType},
    }
};


pub struct MetalWindow {
    pub window_id: WindowId,
    pub window_geom: WindowGeom,
    cal_size: DVec2,
    ca_layer: RcObjcId,
    surface: Option<Box<SurfacePresenter>>,
    surface_attached: bool,
    backing_generation: u64,
    pub cocoa_window: Box<MacosWindow>,
    pub is_resizing: bool
}

impl MetalWindow {
    pub (crate) fn new(
        window_id: WindowId,
        metal_cx: &MetalCx,
        inner_size: DVec2,
        position: Option<DVec2>,
        title: &str
    ) -> MetalWindow {
        
        let ca_layer: ObjcId = unsafe {msg_send![class!(CAMetalLayer), new]};
        let ca_layer_owner = RcObjcId::from_owned(NonNull::new(ca_layer).unwrap());
        
        let mut cocoa_window = Box::new(MacosWindow::new(window_id));
        
        cocoa_window.init(title, inner_size, position);
        unsafe {
            let () = msg_send![ca_layer, setDevice: metal_cx.device];
            let () = msg_send![ca_layer, setPixelFormat: MTLPixelFormat::BGRA8Unorm];
            let () = msg_send![ca_layer, setPresentsWithTransaction: NO];
            let () = msg_send![ca_layer, setMaximumDrawableCount: 3];
            let () = msg_send![ca_layer, setDisplaySyncEnabled: YES];
            let () = msg_send![ca_layer, setNeedsDisplayOnBoundsChange: YES];
            let () = msg_send![ca_layer, setAutoresizingMask: (1 << 4) | (1 << 1)];
            let () = msg_send![ca_layer, setAllowsNextDrawableTimeout: NO];
            let () = msg_send![ca_layer, setDelegate: cocoa_window.view];
            let () = msg_send![ca_layer, setBackgroundColor: CGColorCreateGenericRGB(0.0, 0.0, 0.0, 1.0)];
            
            let view = cocoa_window.view;
            let () = msg_send![view, setWantsBestResolutionOpenGLSurface: YES];
            let () = msg_send![view, setWantsLayer: YES];
            let () = msg_send![view, setLayerContentsPlacement: 11];
            let () = msg_send![view, setLayer: ca_layer];
        }
        
        MetalWindow {
            is_resizing: false,
            window_id,
            cal_size: DVec2::default(),
            ca_layer: ca_layer_owner,
            surface: SurfacePresenter::new(cocoa_window.view).map(Box::new),
            surface_attached: false,
            backing_generation: cocoa_window.backing_generation,
            window_geom: cocoa_window.get_window_geom(),
            cocoa_window
        }
    }
    
    pub (crate) fn start_resize(&mut self) {
        self.is_resizing = true;
        if let Some(surface) = &mut self.surface { surface.invalidate(); }
        let () = unsafe {msg_send![self.ca_layer.as_id(), setPresentsWithTransaction: YES]};
    }
    
    pub (crate) fn stop_resize(&mut self) {
        self.is_resizing = false;
        let () = unsafe {msg_send![self.ca_layer.as_id(), setPresentsWithTransaction: NO]};
    }
    
    pub (crate) fn resize_core_animation_layer(&mut self, _metal_cx: &MetalCx) -> bool {
        let cal_size = DVec2 {
            x: self.window_geom.inner_size.x * self.window_geom.dpi_factor,
            y: self.window_geom.inner_size.y * self.window_geom.dpi_factor
        };
        if self.cal_size != cal_size {
            self.cal_size = cal_size;
            unsafe {
                let () = msg_send![self.ca_layer.as_id(), setDrawableSize: CGSize {width: cal_size.x, height: cal_size.y}];
                let () = msg_send![self.ca_layer.as_id(), setContentsScale: self.window_geom.dpi_factor];
            }
            true
        }
        else {
            false
        }
    }

    fn use_drawable_layer(&mut self) {
        if !self.surface_attached { return; }
        let view = self.cocoa_window.view;
        metal_surface::transaction(|| unsafe {
            let layer: ObjcId = msg_send![view, layer];
            let () = msg_send![layer, setContents: nil];
            let bounds: NSRect = msg_send![view, bounds];
            let () = msg_send![self.ca_layer.as_id(), setFrame: bounds];
            let () = msg_send![view, setLayer: self.ca_layer.as_id()];
        });
        self.surface_attached = false;
    }

    fn visible(&self) -> bool {
        let state: usize = unsafe { msg_send![self.cocoa_window.window, occlusionState] };
        state & (1 << 1) != 0 // NSWindowOcclusionStateVisible
    }

    fn disable_surface(&mut self, reason: &str) {
        eprintln!("paseo_metal_surface fallback: {reason}");
        self.surface = None;
    }

    fn surface_key(&self, device: ObjcId) -> SurfaceKey {
        let physical = self.window_geom.inner_size * self.window_geom.dpi_factor;
        SurfaceKey {
            device,
            size: [physical.x.round() as i32, physical.y.round() as i32],
            scale: self.cocoa_window.get_dpi_factor(),
            screen: unsafe { msg_send![self.cocoa_window.window, screen] },
        }
    }
    
}
 

const KEEP_ALIVE_COUNT: usize = 5;

impl Cx {
    pub(crate) fn macos_update_text_input(&mut self, area: Area, editor: u64, text: &str, start: usize, end: usize) {
        if !area.is_valid(self) || !self.has_key_focus(area) { return; }
        let Some(window_id) = self.area_window_id(area) else { return; };
        let Some((_, ime)) = self.os.text_inputs.iter().find(|(id, _)| *id == window_id) else { return; };
        let interruption = ime.borrow_mut().publish(area, editor, text, start, end);
        if interruption.cancel && !self.os.ime_cancel.contains(&window_id) { self.os.ime_cancel.push(window_id); }
        if let Some(event) = interruption.notify { self.os.ime_notify.push((window_id, area, event)); }
    }

    pub(crate) fn macos_clear_text_input(&mut self, area: Area) {
        for (window_id, ime) in &self.os.text_inputs {
            if ime.borrow_mut().clear(area) && !self.os.ime_cancel.contains(window_id) { self.os.ime_cancel.push(*window_id); }
        }
    }
    
    pub fn event_loop(cx: Rc<RefCell<Cx >>) {
        
        cx.borrow_mut().self_ref = Some(cx.clone());
        cx.borrow_mut().os_type = OsType::Macos;
        let metal_cx = Rc::new(RefCell::new(MetalCx::new(&mut cx.borrow_mut().gpu_info)));
        
        // store device object ID for double buffering
        cx.borrow_mut().os.metal_device = Some(metal_cx.borrow().device);
        
        //let cx = Rc::new(RefCell::new(self));
        if std::env::args().find( | v | v == "--stdin-loop").is_some() {
            let mut cx = cx.borrow_mut();
            cx.in_makepad_studio = true;
            let mut metal_cx = metal_cx.borrow_mut();
            return cx.stdin_event_loop(&mut metal_cx);
        }
        
        let metal_windows = Rc::new(RefCell::new(Vec::new()));
        init_macos_app_global(Box::new({
            let cx = cx.clone();
            move | event | {
                let mut cx_ref = cx.borrow_mut();
                let mut metal_cx = metal_cx.borrow_mut();
                let mut metal_windows = metal_windows.borrow_mut();
                let event_flow = cx_ref.cocoa_event_callback(event, &mut metal_cx, &mut metal_windows);
                let executor = cx_ref.executor.take().unwrap();
                drop(cx_ref);
                executor.run_until_stalled();
                let mut cx_ref = cx.borrow_mut();
                cx_ref.executor = Some(executor);
                event_flow
            }
        }));
        // lets set our signal poll timer
        // final bit of initflow
        with_macos_app(|app| app.start_timer(0, 0.008, true));
        
        cx.borrow_mut().call_event_handler(&Event::Startup);
        cx.borrow_mut().redraw_all();
        MacosApp::event_loop();
    }
    
    pub (crate) fn handle_repaint(&mut self, metal_windows: &mut [MetalWindow], metal_cx: &mut MetalCx, display_tick: bool) {
        self.present_surface_windows(metal_windows);
        let started = self.frame_trace.as_ref().map(|_| Instant::now());
        let mut rendered = 0;
        let mut passes_todo = std::mem::take(&mut self.os.passes_todo);
        self.compute_pass_repaint_order(&mut passes_todo);
        self.repaint_id += 1;
        let time_now = with_macos_app(|app| app.time_now() as f32);
        for pass_id in &passes_todo {
            match self.passes[*pass_id].parent.clone() {
                CxPassParent::Xr => {}
                CxPassParent::Window(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        self.passes[*pass_id].set_time(time_now);
                        if metal_window.surface.is_some() && !metal_window.is_resizing {
                            let key = metal_window.surface_key(metal_cx.device);
                            if key.size.iter().any(|&axis| axis <= 0) { continue; }
                            let visible = metal_window.visible();
                            let surface = metal_window.surface.as_mut().unwrap();
                            if let Some(viewport) = self.surface_viewport(*pass_id, key.size) {
                                match surface.acquire(
                                    key, metal_window.cocoa_window.window, display_tick, visible, self.frame_trace.as_deref(),
                                ) {
                                    Acquire::Deferred => continue,
                                    Acquire::Fallback(reason) => metal_window.disable_surface(reason),
                                    Acquire::Buffer(slot) => match self.draw_surface_pass(*pass_id, metal_cx, surface, slot, key.size, viewport) {
                                        Ok(()) => {
                                            rendered += 1;
                                            continue;
                                        }
                                        Err(reason) => metal_window.disable_surface(reason),
                                    },
                                }
                            } else {
                                // Fractional view geometry can round differently from the
                                // drawable viewport. Keep the existing path until they agree.
                                surface.invalidate();
                            }
                        }
                        metal_window.use_drawable_layer();
                        metal_window.resize_core_animation_layer(metal_cx);
                        let drawable: ObjcId = unsafe {msg_send![metal_window.ca_layer.as_id(), nextDrawable]};
                        if drawable == nil { continue; }
                        let mode = if metal_window.is_resizing {
                            DrawPassMode::Resizing(drawable)
                        } else {
                            DrawPassMode::Drawable(drawable)
                        };
                        self.draw_pass(*pass_id, metal_cx, mode);
                        rendered += 1;
                    }
                }
                CxPassParent::Pass(_) => {
                    //let dpi_factor = self.get_delegated_dpi_factor(parent_pass_id);
                    self.passes[*pass_id].set_time(with_macos_app(|app| app.time_now() as f32));
                    self.draw_pass(*pass_id, metal_cx, DrawPassMode::Texture);
                    rendered += 1;
                },
                CxPassParent::None => {
                    self.passes[*pass_id].set_time(with_macos_app(|app| app.time_now() as f32));
                    self.draw_pass(*pass_id, metal_cx, DrawPassMode::Texture);
                    rendered += 1;
                }
            }
        }
        self.os.passes_todo = passes_todo;
        if let (Some(trace), Some(started)) = (self.frame_trace.as_ref(), started) {
            trace.render(rendered, started.elapsed());
        }
    }

    fn present_surface_windows(&mut self, windows: &mut [MetalWindow]) -> bool {
        let mut repaint = false;
        for window in windows {
            let Some(surface) = &mut window.surface else { continue; };
            let pass = self.windows[window.window_id].main_pass_id;
            match surface.present(window.cocoa_window.view, self.frame_trace.as_deref()) {
                Ok(false) => (),
                Ok(true) => {
                    window.surface_attached = true;
                    repaint |= pass.is_some_and(|pass| self.passes[pass].paint_dirty);
                }
                Err(reason) => {
                    window.disable_surface(reason);
                    if let Some(pass) = pass {
                        self.repaint_pass(pass);
                        repaint = true;
                    }
                }
            }
        }
        repaint
    }
    
    pub (crate) fn handle_networking_events(&mut self) {
        let mut out = Vec::new();
        while let Ok(item) = self.os.network_response.receiver.try_recv() {
            // remove the request object on error or end
            self.os.http_requests.handle_response_item(&item);
            out.push(item);
        }
        if out.len()>0 {
            self.call_event_handler(&Event::NetworkResponses(out))
        }
    }
    
    fn cocoa_event_callback(
        &mut self,
        event: MacosEvent,
        metal_cx: &mut MetalCx,
        metal_windows: &mut Vec<MetalWindow>
    ) -> EventFlow {
        if let  EventFlow::Exit = self.handle_platform_ops(metal_windows, metal_cx){
            self.call_event_handler(&Event::Shutdown);
            return EventFlow::Exit
        }
        // send a mouse up when dragging starts
        match &event {
            MacosEvent::MouseDown(_) |
            MacosEvent::MouseMove(_) |
            MacosEvent::MouseUp(_) |
            MacosEvent::Scroll(_) |
            MacosEvent::KeyDown(_, _) |
            MacosEvent::KeyUp(_, _) |
            MacosEvent::TextInput(_, _) |
            MacosEvent::TextIme(_, _, _) => {
                self.os.keep_alive_counter = KEEP_ALIVE_COUNT;
            }
            MacosEvent::Timer(te) => {
                if te.timer_id == 0 {
                    if self.screenshot_requests.len()>0{
                        self.repaint_windows();
                    }
                    if self.os.keep_alive_counter>0 {
                        self.os.keep_alive_counter -= 1;
                        // The legacy drawable needs extra flips after input. A retained
                        // CALayer already keeps its last contents, and manufacturing paint
                        // requests here would force damage to full after every keystroke.
                        for window in metal_windows.iter() {
                            if window.surface.is_none() || window.is_resizing {
                                if let Some(pass) = self.windows[window.window_id].main_pass_id {
                                    self.repaint_pass(pass);
                                }
                            }
                        }
                    }
                    
                    // check signals
                    if SignalToUI::check_and_clear_ui_signal() {
                        self.handle_media_signals();
                        self.call_event_handler(&Event::Signal);
                    }
                    if SignalToUI::check_and_clear_action_signal() {
                        self.handle_action_receiver();
                    }
                    if self.handle_live_edit() {
                        self.call_event_handler(&Event::LiveEdit);
                        self.redraw_all();
                    }
                    self.handle_networking_events();
                    return self.cocoa_event_callback(MacosEvent::Paint, metal_cx, metal_windows);
                }
            }
            _ => ()
        }
        //self.process_desktop_pre_event(&mut event);
        match event {
            MacosEvent::AppGotFocus(window_id) => { // repaint all window passes. Metal sometimes doesnt flip buffers when hidden/no focus
                self.set_window_keyboard_focus(window_id, true);
                for window in metal_windows.iter_mut() {
                    if let Some(main_pass_id) = self.windows[window.window_id].main_pass_id {
                        self.repaint_pass(main_pass_id);
                    }
                }
                self.call_event_handler(&Event::AppGotFocus);
            }
            MacosEvent::AppLostFocus(window_id) => {
                self.set_window_keyboard_focus(window_id, false);
                self.call_event_handler(&Event::AppLostFocus);
            }
            MacosEvent::WindowResizeLoopStart(window_id) => {
                if let Some(window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                    window.start_resize();
                }
            }
            MacosEvent::WindowResizeLoopStop(window_id) => {
                if let Some(window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                    window.stop_resize();
                    if window.surface.is_some() {
                        if let Some(pass) = self.windows[window_id].main_pass_id { self.repaint_pass(pass); }
                    }
                }
            }
            MacosEvent::WindowGeomChange(mut re) => { // do this here because mac
                if let Some(window) = metal_windows.iter_mut().find( | w | w.window_id == re.window_id) {
                    self.windows[re.window_id].os_dpi_factor = Some(re.new_geom.dpi_factor);
                    if let Some(dpi_override) = self.windows[re.window_id].dpi_override {
                        re.new_geom.inner_size *= re.new_geom.dpi_factor / dpi_override;
                        re.new_geom.dpi_factor = dpi_override;
                    }
                    let size_changed = window.window_geom.dpi_factor != re.new_geom.dpi_factor
                        || window.window_geom.inner_size != re.new_geom.inner_size;
                    let mode_changed = window.window_geom.is_fullscreen != re.new_geom.is_fullscreen
                        || window.window_geom.is_fullscreen_transitioning != re.new_geom.is_fullscreen_transitioning;
                    let backing_changed = window.backing_generation != window.cocoa_window.backing_generation;
                    window.backing_generation = window.cocoa_window.backing_generation;
                    if size_changed || mode_changed || backing_changed {
                        if let Some(surface) = &mut window.surface {
                            surface.invalidate();
                            if let Some(pass) = self.windows[re.window_id].main_pass_id { self.repaint_pass(pass); }
                        }
                    }
                    window.window_geom = re.new_geom.clone();
                    self.windows[re.window_id].window_geom = re.new_geom.clone();
                    
                    // redraw just this windows root draw list
                    if size_changed {
                        if let Some(main_pass_id) = self.windows[re.window_id].main_pass_id {
                            self.redraw_pass_and_child_passes(main_pass_id);
                        }
                    }
                }
                // ok lets not redraw all, just this window
                self.call_event_handler(&Event::WindowGeomChange(re));
            }
            MacosEvent::WindowClosed(wc) => {
                // lets remove the window from the set
                let window_id = wc.window_id;
                self.call_event_handler(&Event::WindowClosed(wc));
                
                self.windows[window_id].is_created = false;
                self.os.text_inputs.retain(|(id, _)| *id != window_id);
                self.os.ime_cancel.retain(|id| *id != window_id);
                self.os.ime_notify.retain(|(window, _, _)| *window != window_id);
                if let Some(index) = metal_windows.iter().position( | w | w.window_id == window_id) {
                    metal_windows[index].use_drawable_layer();
                    metal_windows.remove(index);
                    if metal_windows.len() == 0 {
                        self.call_event_handler(&Event::Shutdown);
                        return EventFlow::Exit
                    }
                }
            }
            MacosEvent::SurfaceReady => {
                if self.present_surface_windows(metal_windows) {
                    // A render was coalesced behind this GPU frame. Queue the latest
                    // work now rather than waiting for the polling timer; don't
                    // advance animation time except on CVDisplayLink ticks.
                    return self.cocoa_event_callback(MacosEvent::Paint, metal_cx, metal_windows);
                }
                return EventFlow::Wait;
            }
            MacosEvent::SurfaceWake => {
                with_macos_app(|app| app.set_animation_screen(None));
                for window in metal_windows.iter_mut() {
                    if let Some(surface) = &mut window.surface {
                        surface.invalidate();
                        if let Some(pass) = self.windows[window.window_id].main_pass_id { self.repaint_pass(pass); }
                    }
                }
                return self.cocoa_event_callback(MacosEvent::Paint, metal_cx, metal_windows);
            }
            MacosEvent::Paint | MacosEvent::DisplayFrame => {
                if matches!(event, MacosEvent::DisplayFrame) && !self.new_next_frames.is_empty() {
                    self.call_next_frame_event(with_macos_app(|app| app.time_now()));
                }
                if self.need_redrawing() {
                    self.call_draw_event();
                    self.mtl_compile_shaders(&metal_cx);
                }
                self.handle_repaint(metal_windows, metal_cx, matches!(event, MacosEvent::DisplayFrame));
                let screen = (!self.new_next_frames.is_empty())
                    .then(|| Self::animation_screen(metal_windows))
                    .or_else(|| metal_windows.iter().find_map(|window| {
                        let surface = window.surface.as_ref()?;
                        (surface.needs_display_tick() && window.visible())
                            .then(|| unsafe { msg_send![window.cocoa_window.window, screen] })
                    }));
                with_macos_app(|app| app.set_animation_screen(screen));
                return EventFlow::Wait;
            }
            MacosEvent::MouseDown(mut e) => {
                self.dpi_override_scale(&mut e.abs, e.window_id);
                self.fingers.process_tap_count(
                    e.abs,
                    e.time
                );
                self.fingers.mouse_down(e.button, e.window_id);
                self.call_event_handler(&Event::MouseDown(e.into()));
            }
            MacosEvent::MouseMove(mut e) => {
                self.dpi_override_scale(&mut e.abs, e.window_id);
                self.call_event_handler(&Event::MouseMove(e.into()));
                self.fingers.cycle_hover_area(live_id!(mouse).into());
                self.fingers.switch_captures();
            }
            MacosEvent::MouseUp(mut e) => {
                self.dpi_override_scale(&mut e.abs, e.window_id);
                let button = e.button;
                self.call_event_handler(&Event::MouseUp(e.into()));
                self.fingers.mouse_up(button);
                self.fingers.cycle_hover_area(live_id!(mouse).into());
            }
            MacosEvent::Scroll(mut e) => {
                self.dpi_override_scale(&mut e.abs, e.window_id);
                self.call_event_handler(&Event::Scroll(e.into()));
            }
            MacosEvent::WindowDragQuery(mut e) => {
                self.dpi_override_scale(&mut e.abs, e.window_id);
                self.call_event_handler(&Event::WindowDragQuery(e))
            }
            MacosEvent::WindowCloseRequested(e) => {
                self.call_event_handler(&Event::WindowCloseRequested(e))
            }
            MacosEvent::TextPasteShortcut(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::TextPasteShortcut(e))
            }
            MacosEvent::TextInput(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::TextInput(e))
            }
            MacosEvent::TextIme(window_id, area, e) => {
                if self.has_key_focus(area) && self.area_window_id(area) == Some(window_id) {
                    self.call_window_event_handler(window_id, &Event::TextIme(e));
                }
            }
            MacosEvent::Drag(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::Drag(e));
                self.drag_drop.cycle_drag();
            }
            MacosEvent::Drop(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::Drop(e));
                self.drag_drop.cycle_drag();
            }
            MacosEvent::DragEnd => {
                self.call_event_handler(&Event::DragEnd);
                self.drag_drop.cycle_drag();
            }
            MacosEvent::DragSourceEnd(window_id) => {
                // AppKit owns the source gesture's release. Destination exit/drop
                // only clears feedback and must not release an unrelated mouse capture.
                self.call_window_event_handler(window_id, &Event::MouseUp(MouseUpEvent {
                    abs: dvec2(-100000.0, -100000.0),
                    button: MouseButton::PRIMARY,
                    window_id,
                    modifiers: Default::default(),
                    time: 0.0
                }));
                self.fingers.mouse_up(MouseButton::PRIMARY);
                self.fingers.cycle_hover_area(live_id!(mouse).into());

                self.call_event_handler(&Event::DragEnd);
                self.drag_drop.cycle_drag();
            }
            MacosEvent::KeyDown(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::KeyDown(e))
            }
            MacosEvent::KeyUp(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::KeyUp(e))
            }
            MacosEvent::TextCopy(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::TextCopy(e))
            }
            MacosEvent::TextCut(window_id, e) => {
                self.call_window_event_handler(window_id, &Event::TextCut(e))
            }
            MacosEvent::Timer(e) => {
                self.call_event_handler(&Event::Timer(e));
                return EventFlow::Wait;
            }
            MacosEvent::MacosMenuCommand(e) => {
                self.call_event_handler(&Event::MacosMenuCommand(e))
            }
        }
        
        //if self.any_passes_dirty() || self.need_redrawing()/* || self.new_next_frames.len() != 0*/ || paint_dirty {
        // the timer is the primary wait flow   
        EventFlow::Poll
        //} else {
         //   EventFlow::Wait
       // }
    }

    fn animation_screen(windows: &[MetalWindow]) -> ObjcId {
        unsafe {
            let mut first = nil;
            for window in windows {
                let native = window.cocoa_window.window;
                let screen: ObjcId = msg_send![native, screen];
                if screen == nil {
                    continue;
                }
                let is_key: BOOL = msg_send![native, isKeyWindow];
                if is_key == YES {
                    return screen;
                }
                if first == nil {
                    first = screen;
                }
            }
            first
        }
    }
    
    fn dpi_override_scale(&self, pos:&mut DVec2, window_id:WindowId){
        *pos = self.windows[window_id].remap_dpi_override(*pos)
    }
    
    fn handle_platform_ops(&mut self, metal_windows: &mut Vec<MetalWindow>, metal_cx: &MetalCx)->EventFlow {
        for window_id in std::mem::take(&mut self.os.ime_cancel) {
            if let Some(window) = metal_windows.iter_mut().find(|w| w.window_id == window_id) {
                window.cocoa_window.discard_marked_text();
            }
        }
        for (window_id, area, event) in std::mem::take(&mut self.os.ime_notify) {
            if self.has_key_focus(area) && self.os.text_inputs.iter().any(|(id, ime)| *id == window_id && ime.borrow().area() == area) {
                self.call_window_event_handler(window_id, &Event::TextIme(event));
            }
        }
        while let Some(op) = self.platform_ops.pop() {
            match op {
                CxOsOp::CreateWindow(window_id) => {
                    let window = &mut self.windows[window_id];
                    let metal_window = MetalWindow::new(
                        window_id,
                        &metal_cx,
                        window.create_inner_size.unwrap_or(dvec2(800., 600.)),
                        window.create_position,
                        &window.create_title
                    );
                    window.window_geom = metal_window.window_geom.clone();
                    self.os.text_inputs.push((window_id, metal_window.cocoa_window.ime.clone()));
                    metal_windows.push(metal_window);
                    window.is_created = true;
                },
                CxOsOp::ResizeWindow(window_id, size) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.set_outer_size(size);
                    }
                }
                CxOsOp::RepositionWindow(window_id, pos ) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.set_position(pos);
                    }
                }
                CxOsOp::CloseWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        self.windows[window_id].is_created = false;
                        metal_window.cocoa_window.close_window();
                        break;
                    }
                },
                CxOsOp::Quit => {
                    return EventFlow::Exit;
                },
                CxOsOp::MinimizeWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.minimize();
                    }
                },
                CxOsOp::Deminiaturize(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.deminiaturize();
                    }
                },
                CxOsOp::MaximizeWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.maximize();
                    }
                },
                CxOsOp::RestoreWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.restore();
                    }
                },
                CxOsOp::FullscreenWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find(|w| w.window_id == window_id) {
                        metal_window.cocoa_window.set_fullscreen(true);
                    }
                },
                CxOsOp::NormalizeWindow(window_id) => {
                    if let Some(metal_window) = metal_windows.iter_mut().find(|w| w.window_id == window_id) {
                        metal_window.cocoa_window.set_fullscreen(false);
                    }
                },
                CxOsOp::HideWindow(window_id) =>{
                    if let Some(metal_window) = metal_windows.iter_mut().find( | w | w.window_id == window_id) {
                        metal_window.cocoa_window.hide();
                    }
                }
                CxOsOp::ShowTextIME(area, pos) => {
                    let pos = area.clipped_rect(self).pos + pos;
                    metal_windows.iter_mut().for_each( | w | {
                        w.cocoa_window.set_ime_spot(pos);
                    });
                },
                CxOsOp::HideTextIME => {
                    metal_windows.iter_mut().for_each( | w | {
                        w.cocoa_window.set_ime_spot(dvec2(0.0,0.0));
                    });
                },
                CxOsOp::SetCursor(cursor) => {
                    with_macos_app(|app| app.set_mouse_cursor(cursor));
                },
                CxOsOp::StartTimer {timer_id, interval, repeats} => {
                    with_macos_app(|app| app.start_timer(timer_id, interval, repeats));
                },
                CxOsOp::StopTimer(timer_id) => {
                    with_macos_app(|app| app.stop_timer(timer_id));
                },
                CxOsOp::StartDragging(items) => {
                    //  lets start dragging on the right window
                    if let Some(metal_window) = metal_windows.iter_mut().next() {
                        metal_window.cocoa_window.start_dragging(items);
                        break;
                    }
                }
                CxOsOp::UpdateMacosMenu(menu) => {
                    with_macos_app(|app| app.update_macos_menu(&menu))
                },
                CxOsOp::HttpRequest {request_id, request} => {
                    self.os.http_requests.make_http_request(request_id, request, self.os.network_response.sender.clone());
                },
                CxOsOp::CancelHttpRequest {request_id} => {
                    self.os.http_requests.cancel_http_request(request_id);
                },
                CxOsOp::CopyToClipboard(content) => {
                    with_macos_app(|app| app.copy_to_clipboard(&content));
                },
                CxOsOp::SaveFileDialog(settings) => 
                {
                    with_macos_app(|app| app.open_save_file_dialog(settings));
                }
                
                CxOsOp::SelectFileDialog(settings) => 
                {
                    with_macos_app(|app| app.open_select_file_dialog(settings));
                }
                
                CxOsOp::SaveFolderDialog(settings) => 
                {
                    with_macos_app(|app| app.open_save_folder_dialog(settings));
                }
                
                CxOsOp::SelectFolderDialog(settings) => 
                {
                    with_macos_app(|app| app.open_select_folder_dialog(settings));
                }
                CxOsOp::ShowInDock(show) => {
                    with_macos_app(|app| app.show_in_dock(show));
                }
                e=>{
                    crate::error!("Not implemented on this platform: CxOsOp::{:?}", e);
                }
            }
        }
        EventFlow::Poll
    }
}

impl CxOsApi for Cx {
    fn pre_start() -> bool {
        init_apple_classes_global();
        for arg in std::env::args() {
            if arg == "--metal-xpc" {
                start_xpc_service();
                return true
            }
        }
        false
    }
    
    fn init_cx_os(&mut self) {
        self.os.start_time = Some(Instant::now());
        if let Some(item) = std::option_env!("MAKEPAD_PACKAGE_DIR"){
            self.live_registry.borrow_mut().package_root = Some(item.to_string());
        }
        self.live_expand();
        if !Self::has_studio_web_socket() {
            self.start_disk_live_file_watcher(100);
        }
        self.live_scan_dependencies();

        #[cfg(apple_bundle)]
        self.apple_bundle_load_dependencies();
        #[cfg(not(apple_bundle))]
        self.native_load_dependencies();
    }
    
    fn spawn_thread<F>(&mut self, f: F) where F: FnOnce() + Send + 'static {
        std::thread::spawn(f);
    }
    
    fn start_stdin_service(&mut self) {
        self.start_xpc_service()
    }
    
    fn seconds_since_app_start(&self)->f64{
        Instant::now().duration_since(self.os.start_time.unwrap()).as_secs_f64()
    }
    
    fn open_url(&mut self, _url:&str, _in_place:OpenUrlInPlace){
        crate::error!("open_url not implemented on this platform");
    }
    
    fn max_texture_width()->usize{16384}
    /*
    fn web_socket_open(&mut self, _url: String, _rec: WebSocketAutoReconnect) -> WebSocket {
        todo!()
    }
    
    fn web_socket_send(&mut self, _websocket: WebSocket, _data: Vec<u8>) {
        todo!()
    }*/
}


#[derive(Default)]
pub struct CxOs {
    pub(crate) text_inputs: Vec<(WindowId, Rc<RefCell<crate::ime_composer::ImeComposer>>)>,
    pub(crate) ime_cancel: Vec<WindowId>,
    pub(crate) ime_notify: Vec<(WindowId, Area, TextImeEvent)>,
    pub (crate) keep_alive_counter: usize,
    passes_todo: Vec<PassId>,
    pub (crate) media: CxAppleMedia,
    pub (crate) bytes_written: usize,
    pub (crate) draw_calls_done: usize,
    pub (crate) network_response: NetworkResponseChannel,
    pub (crate) stdin_timers: PollTimers,
    pub (crate) start_time: Option<Instant>,
    pub (crate) http_requests: AppleHttpRequests,
    pub metal_device: Option<ObjcId>,
}
