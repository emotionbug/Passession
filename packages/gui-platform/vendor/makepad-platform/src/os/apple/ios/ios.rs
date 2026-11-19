use {
    std::{
        time::Instant,
        rc::Rc,
        cell::{Cell, RefCell},
    },

    crate::{
        area::Area,
        makepad_math::{dvec2, Rect},
        AccessibilityAction,
        makepad_live_id::*,
        os::{
            cx_native::EventFlow,
            apple::{
                ios::{
                    ios_event::IosEvent,
                    ios_app::{IosApp, init_ios_app_global,with_ios_app}
                },
                url_session::{AppleHttpRequests},
                surface_present::{SurfacePresenter, transaction},
            },
            apple_classes::init_apple_classes_global,
            apple_media::CxAppleMedia,
            metal::{MetalCx, DrawPassMode},
        },
        pass::{CxPassParent, PassId},
        thread::SignalToUI,
        window::CxWindowPool,
        event::{
            Event,
            KeyCode, KeyEvent, NetworkResponseChannel, TextImeEvent,
        },
        cx_api::{CxOsApi, CxOsOp, OpenUrlInPlace},
        cx::{Cx, OsType},
    }
};

use super::{ios_accessibility::{self, Accessibility}, ios_text_input::TextInput, ios_wake};

impl Cx {

    pub fn event_loop(cx:Rc<RefCell<Cx>>) {
        cx.borrow_mut().self_ref = Some(cx.clone());
        cx.borrow_mut().os_type = OsType::Ios;
        let metal_cx = Rc::new(RefCell::new(MetalCx::new(&mut cx.borrow_mut().gpu_info)));
        //let cx = Rc::new(RefCell::new(self));
        //crate::log!("Makepad iOS application started.");
        //let metal_windows = Rc::new(RefCell::new(Vec::new()));
        init_apple_classes_global();
        init_ios_app_global(Box::new({
            let cx = cx.clone();
            move | event | {
                let mut cx_ref = cx.borrow_mut();
                let mut metal_cx = metal_cx.borrow_mut();
                cx_ref.ios_event_callback(event, &mut metal_cx);
                let executor = cx_ref.executor.take().unwrap();
                drop(cx_ref);
                executor.run_until_stalled();
                let mut cx_ref = cx.borrow_mut();
                cx_ref.executor = Some(executor);
                cx_ref.ios_flush_platform_ops();
                cx_ref.ios_event_flow()
            }
        }));
        IosApp::event_loop();
    }

    fn repaint_ios_surface(&mut self, metal_cx: &mut MetalCx, display_tick: bool, layout: bool) {
        let Some((view, key)) = with_ios_app(|app| {
            if !app.surface_active { return None; }
            app.surface_metrics.as_ref().map(|metrics| (app.view, metrics.key(metal_cx.device)))
        }) else { return; };

        let mut surface = self.os.surface.take().unwrap_or_else(|| super::ios_surface::presenter(view));
        if surface.key_changed(key) {
            surface.invalidate();
        } else if let Err(reason) = surface.present(view, self.frame_trace.as_deref()) {
            surface.retry_present(reason);
            self.repaint_windows();
        }
        let started = self.frame_trace.as_ref().map(|_| Instant::now());
        let mut rendered = 0;
        let mut passes_todo = std::mem::take(&mut self.os.passes_todo);
        self.compute_pass_repaint_order(&mut passes_todo);
        self.repaint_id += 1;
        let now = with_ios_app(|app| app.time_now() as f32);
        for &pass in &passes_todo {
            self.passes[pass].set_time(now);
            match self.passes[pass].parent {
                CxPassParent::Xr => (),
                CxPassParent::Window(_) => {
                    let result = surface.acquire(key, display_tick, true, self.frame_trace.as_deref());
                    let result = result.and_then(|slot| {
                        let Some(slot) = slot else { return Ok(()); };
                        self.draw_surface_pass(pass, metal_cx, &mut surface, slot, key.size, layout)?;
                        rendered += 1;
                        if layout {
                            surface.present(view, self.frame_trace.as_deref())?;
                        }
                        Ok(())
                    });
                    if let Err(reason) = result {
                        surface.retry_frame(key, reason);
                        self.passes[pass].paint_dirty = true;
                    }
                }
                CxPassParent::Pass(_) | CxPassParent::None => {
                    self.draw_pass(pass, metal_cx, DrawPassMode::Texture);
                    rendered += 1;
                }
            }
        }
        self.os.passes_todo = passes_todo;
        self.os.surface = Some(surface);
        if let (Some(trace), Some(started)) = (&self.frame_trace, started) {
            trace.render(rendered, started.elapsed());
        }
    }

    fn present_ios_surface(&mut self) {
        let view = with_ios_app(|app| app.surface_active.then_some(app.view));
        let Some(view) = view else { return; };
        let Some(surface) = &mut self.os.surface else { return; };
        if let Err(reason) = surface.present(view, self.frame_trace.as_deref()) {
            surface.retry_present(reason);
            self.repaint_windows();
        }
    }

    fn ios_surface_frame(&mut self, metal_cx: &mut MetalCx, display_tick: bool, layout: bool) {
        if !with_ios_app(|app| app.surface_active) { return; }
        if display_tick && !self.new_next_frames.is_empty() {
            let time_now = with_ios_app(|app| app.time_now());
            self.call_next_frame_event(time_now);
        }
        if self.need_redrawing() {
            self.call_draw_event();
            // A synchronous UIKit callback can suspend the app during Draw.
            if !with_ios_app(|app| app.surface_active) { return; }
            self.ios_sync_accessibility();
            self.mtl_compile_shaders(metal_cx);
        }
        self.repaint_ios_surface(metal_cx, display_tick, layout);
    }

    pub(crate) fn handle_networking_events(&mut self) {
        let mut out = Vec::new();
        while let Ok(item) = self.os.network_response.receiver.try_recv(){
            self.os.http_requests.handle_response_item(&item);
            out.push(item);
        }
        if out.len()>0{
            self.call_event_handler(& Event::NetworkResponses(out))
        }
    }

    fn ios_event_callback(
        &mut self,
        event: IosEvent,
        metal_cx: &mut MetalCx,
    ) {

        self.handle_platform_ops();

        match event {
            IosEvent::Wake => {
                if with_ios_app(|app| app.surface_active) {
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
                }
            }
            IosEvent::ExecutorTick => {}
            IosEvent::VirtualKeyboard(vk)=>{
                self.call_event_handler(&Event::VirtualKeyboard(vk));
            }
            IosEvent::Init=>{
                self.ios_accessibility_status();
                self.start_studio_websocket_delayed();
                self.call_event_handler(&Event::Startup);
                self.redraw_all();
            }
            IosEvent::SurfaceResumed => {
                self.ios_accessibility_status();
                if with_ios_app(|app| app.surface_metrics.is_some()) {
                    self.call_event_handler(&Event::AppGotFocus);
                    self.redraw_all();
                    self.repaint_windows();
                }
            }
            IosEvent::SurfaceSuspended => {
                if let Some(edit) = self.os.text_input.end_composition() {
                    self.ios_deliver_text_ime(edit);
                }
                metal_cx.drain_surface_queue();
                if let Some(surface) = &mut self.os.surface { surface.invalidate(); }
                self.call_event_handler(&Event::AppLostFocus);
            }
            IosEvent::SurfaceMemoryWarning => {
                metal_cx.drain_surface_queue();
                if let Some(surface) = &mut self.os.surface { surface.purge(); }
                let window = CxWindowPool::id_zero();
                if self.windows.is_valid(window) {
                    if let Some(pass) = self.windows[window].main_pass_id {
                        self.evict_metal_window_depth(pass);
                    }
                }
                self.repaint_windows();
            }
            IosEvent::WindowGeomChange(re) => {
                let window = &mut self.windows[re.window_id];
                window.os_dpi_factor = Some(re.new_geom.dpi_factor);
                window.window_geom = re.new_geom.clone();
                self.call_event_handler(&Event::WindowGeomChange(re));
                self.redraw_all();
                self.repaint_windows();
            }
            IosEvent::SurfaceLayout => transaction(|| self.ios_surface_frame(metal_cx, false, true)),
            IosEvent::DisplayFrame => self.ios_surface_frame(metal_cx, true, false),
            IosEvent::SurfaceReady => self.present_ios_surface(),
            IosEvent::AccessibilityStatus => self.ios_accessibility_status(),
            IosEvent::AccessibilityActivate { id, handled } => {
                if ios_accessibility::is_running() && with_ios_app(|app| app.surface_active) {
                    handled.set(self.perform_accessibility_action(id, AccessibilityAction::Activate));
                }
            }
            IosEvent::TouchUpdate(e)=>{
                self.fingers.process_touch_update_start(e.time, &e.touches);
                let e = Event::TouchUpdate(e);
                self.call_event_handler(&e);
                let e = if let Event::TouchUpdate(e) = e{e}else{panic!()};
                self.fingers.process_touch_update_end(&e.touches);
            }
            IosEvent::LongPress(e) => {
                self.call_event_handler(&Event::LongPress(e.into()));
            }
            IosEvent::MouseDown(e) => {
                self.fingers.process_tap_count(
                    e.abs,
                    e.time
                );
                self.fingers.mouse_down(e.button, e.window_id);
                self.call_event_handler(&Event::MouseDown(e.into()))
            }
            IosEvent::MouseMove(e) => {
                self.call_event_handler(&Event::MouseMove(e.into()));
                self.fingers.cycle_hover_area(live_id!(mouse).into());
                self.fingers.switch_captures();
            }
            IosEvent::MouseUp(e) => {
                let button = e.button;
                self.call_event_handler(&Event::MouseUp(e.into()));
                self.fingers.mouse_up(button);
                self.fingers.cycle_hover_area(live_id!(mouse).into());
            }
            IosEvent::Scroll(e) => {
                self.call_event_handler(&Event::Scroll(e.into()))
            }
            IosEvent::TextEdit { session, text, selection, composing, ended } => {
                if self.has_key_focus(self.os.text_input.area()) && with_ios_app(|app| app.surface_active) {
                    if ended {
                        if !self.os.text_input.did_end_editing(session) { return; }
                        self.keyboard.set_text_ime_dismissed();
                    }
                    if let Some(edit) = self.os.text_input.observe(session, text, selection, composing) {
                        self.ios_deliver_text_ime(edit);
                    }
                }
            }
            IosEvent::TextCompositionEnded(edit) => self.ios_deliver_text_ime(edit),
            IosEvent::TextSubmit(session) => {
                if self.has_key_focus(self.os.text_input.area()) && self.os.text_input.can_submit(session)
                    && with_ios_app(|app| app.surface_active) {
                    let key = KeyEvent {
                        key_code: KeyCode::ReturnKey,
                        is_repeat: false,
                        modifiers: Default::default(),
                        time: with_ios_app(|app| app.time_now()),
                    };
                    self.call_event_handler(&Event::KeyDown(key));
                }
            }

            IosEvent::Timer(e) => self.call_event_handler(&Event::Timer(e)),
        }
    }

    fn ios_event_flow(&self) -> EventFlow {

        let waiting_for_gpu = self.os.surface.as_ref().is_some_and(SurfacePresenter::frame_in_flight);
        let repaint = !waiting_for_gpu && (self.any_passes_dirty() || self.demo_time_repaint)
            || self.os.surface.as_ref().is_some_and(SurfacePresenter::needs_display_tick);
        if repaint || self.need_redrawing() || !self.new_next_frames.is_empty() {
            EventFlow::Poll
        } else {
            EventFlow::Wait
        }
    }

    fn ios_accessibility_status(&mut self) {
        self.set_accessibility_enabled(ios_accessibility::is_running());
        if !self.accessibility_enabled() { self.ios_sync_accessibility(); }
    }

    fn ios_sync_accessibility(&mut self) {
        let view = with_ios_app(|app| app.view);
        let enabled = self.accessibility_enabled();
        self.os.accessibility.sync(view, &self.accessibility, enabled);
    }

    pub(crate) fn ios_update_text_input(&mut self, area: Area, editor: u64, text: &str,
        selection: (usize, usize), traits: (bool, bool, bool, bool)) {
        if !area.is_valid(self) || !self.has_key_focus(area) { return; }
        if let Some(edit) = self.os.text_input.publish(area, editor, text, selection, traits) {
            IosApp::defer_callback(IosEvent::TextCompositionEnded(edit));
        }
    }

    pub(crate) fn ios_clear_text_input(&mut self, area: Area) {
        self.os.text_input.clear(area);
    }

    fn ios_deliver_text_ime(&mut self, edit: TextImeEvent) {
        if self.has_key_focus(self.os.text_input.area()) && self.os.text_input.admit(&edit) {
            self.call_event_handler(&Event::TextIme(edit));
        }
    }

    pub(crate) fn ios_spawner_requested(&self) {
        if !self.os.spawner_requested.replace(true) { ios_wake::post(); }
    }

    fn ios_flush_platform_ops(&mut self) {
        self.handle_platform_ops();
        let area = self.os.text_input.area();
        if !self.has_key_focus(area) { self.os.text_input.clear(area); }
        with_ios_app(|app| app.update_executor_timer(self.os.spawner_requested.get()));
    }

    fn handle_platform_ops(&mut self) {
        let mut handled_ime = false;
        while let Some(op) = self.platform_ops.pop() {
            match op {
                CxOsOp::CreateWindow(window_id) => {
                    let window = &mut self.windows[window_id];
                    window.window_geom = with_ios_app(|app| app.last_window_geom.clone());
                    window.os_dpi_factor = Some(window.window_geom.dpi_factor);
                    window.is_created = true;
                },
                CxOsOp::ShowTextIME(area, pos) => {
                    if !std::mem::replace(&mut handled_ime, true) && area.is_valid(self)
                        && self.has_key_focus(self.os.text_input.area()) && !self.keyboard.text_ime_dismissed
                        && with_ios_app(|app| app.surface_active) {
                        let caret = Rect { pos: area.rect(self).pos + pos, size: dvec2(1.0, self.ime_caret_height) };
                        self.os.text_input.show(caret);
                    }
                },
                CxOsOp::HideTextIME => {
                    if !std::mem::replace(&mut handled_ime, true) {
                        if let Some(edit) = self.os.text_input.hide() { self.ios_deliver_text_ime(edit); }
                    }
                },
                CxOsOp::StartTimer {timer_id, interval, repeats} => {
                    with_ios_app(|app| app.start_timer(timer_id, interval, repeats));
                },
                CxOsOp::StopTimer(timer_id) => {
                    with_ios_app(|app| app.stop_timer(timer_id));
                },
                CxOsOp::HttpRequest {request_id, request} => {
                    self.os.http_requests.make_http_request(request_id, request, self.os.network_response.sender.clone());
                },
                CxOsOp::CancelHttpRequest {request_id} => {
                    self.os.http_requests.cancel_http_request(request_id);
                },
                CxOsOp::CopyToClipboard(content) => {
                    with_ios_app(|app| app.copy_to_clipboard(&content));
                }
                CxOsOp::SetCursor(_)=>{
                    // no need
                }
                e=>{
                    crate::error!("Not implemented on this platform: CxOsOp::{:?}", e);
                }
            }
        }
    }


    /*
    let _ = self.live_file_change_sender.send(vec![LiveFileChange{
        file_name:file_name.to_string(),
        content
    }]);*/

}

impl CxOsApi for Cx {
    fn init_cx_os(&mut self) {
        self.os.start_time = Some(Instant::now());
        #[cfg(not(apple_sim))]{
            self.live_registry.borrow_mut().package_root = Some("makepad".to_string());
        }

        self.live_expand();

        if !Self::has_studio_web_socket() {
            #[cfg(apple_sim)]
            self.start_disk_live_file_watcher(50);
        }

        self.live_scan_dependencies();

        #[cfg(apple_sim)]
        self.native_load_dependencies();
        #[cfg(not(apple_sim))]
        self.apple_bundle_load_dependencies();
    }

    fn spawn_thread<F>(&mut self, f: F) where F: FnOnce() + Send + 'static {
        std::thread::spawn(f);
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
    pub (crate) start_time: Option<Instant>,
    pub (crate) media: CxAppleMedia,
    pub (crate) bytes_written: usize,
    pub (crate) draw_calls_done: usize,
    pub (crate) network_response: NetworkResponseChannel,
    pub (crate) http_requests: AppleHttpRequests,
    text_input: TextInput,
    accessibility: Accessibility,
    spawner_requested: Cell<bool>,
    surface: Option<SurfacePresenter>,
    passes_todo: Vec<PassId>,
}

