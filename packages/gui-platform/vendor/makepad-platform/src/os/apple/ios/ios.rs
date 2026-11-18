use {
    std::{
        time::Instant,
        rc::Rc,
        cell::{RefCell},
    },

    crate::{
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
            NetworkResponseChannel
        },
        cx_api::{CxOsApi, CxOsOp, OpenUrlInPlace},
        cx::{Cx, OsType},
    }
};

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
                let event_flow = cx_ref.ios_event_callback(event, &mut metal_cx);
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
    ) -> EventFlow {

        self.handle_platform_ops();

        match &event {
            IosEvent::Timer(te) => {
                if te.timer_id == 0 {
                    let vk = with_ios_app(|app| app.virtual_keyboard_event.take());
                    if let Some(vk) = vk{
                        self.call_event_handler(&Event::VirtualKeyboard(vk));
                    }
                    // check signals
                    if SignalToUI::check_and_clear_ui_signal(){
                        self.handle_media_signals();
                        self.call_event_handler(&Event::Signal);
                    }
                    if SignalToUI::check_and_clear_action_signal() {
                        self.handle_action_receiver();
                    }

                    if self.handle_live_edit(){
                        // self.draw_shaders.ptr_to_item.clear();
                        // self.draw_shaders.fingerprints.clear();
                        self.call_event_handler(&Event::LiveEdit);
                        self.redraw_all();
                    }
                    self.handle_networking_events();
                }
            }
            _ => ()
        }

        match event {
            IosEvent::VirtualKeyboard(vk)=>{
                self.call_event_handler(&Event::VirtualKeyboard(vk));
            }
            IosEvent::Init=>{
                with_ios_app(|app| app.start_timer(0, 0.008, true));
                self.start_studio_websocket_delayed();
                self.call_event_handler(&Event::Startup);
                self.redraw_all();
            }
            IosEvent::SurfaceResumed => {
                if with_ios_app(|app| app.surface_metrics.is_some()) {
                    self.call_event_handler(&Event::AppGotFocus);
                    self.redraw_all();
                    self.repaint_windows();
                }
            }
            IosEvent::SurfaceSuspended => {
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
            IosEvent::TextInput(e) => {
                self.call_event_handler(&Event::TextInput(e))
            }

            IosEvent::KeyDown(e) => {
                self.keyboard.process_key_down(e.clone());
                self.call_event_handler(&Event::KeyDown(e))
            }
            IosEvent::KeyUp(e) => {
                self.keyboard.process_key_up(e.clone());
                self.call_event_handler(&Event::KeyUp(e))
            }
            IosEvent::TextCopy(e) => {
                self.call_event_handler(&Event::TextCopy(e))
            }
            IosEvent::TextCut(e) => {
                self.call_event_handler(&Event::TextCut(e))
            }
            IosEvent::Timer(e) => if e.timer_id != 0 {
                self.call_event_handler(&Event::Timer(e))
            }
        }

        let waiting_for_gpu = self.os.surface.as_ref().is_some_and(SurfacePresenter::frame_in_flight);
        let repaint = !waiting_for_gpu && (self.any_passes_dirty() || self.demo_time_repaint)
            || self.os.surface.as_ref().is_some_and(SurfacePresenter::needs_display_tick);
        if repaint || self.need_redrawing() || !self.new_next_frames.is_empty() {
            EventFlow::Poll
        } else {
            EventFlow::Wait
        }
    }

    fn handle_platform_ops(&mut self) {
        while let Some(op) = self.platform_ops.pop() {
            match op {
                CxOsOp::CreateWindow(window_id) => {
                    let window = &mut self.windows[window_id];
                    window.window_geom = with_ios_app(|app| app.last_window_geom.clone());
                    window.os_dpi_factor = Some(window.window_geom.dpi_factor);
                    window.is_created = true;
                },
                CxOsOp::ShowTextIME(_area, _pos) => {
                    IosApp::show_keyboard();
                },
                CxOsOp::HideTextIME => {
                    IosApp::hide_keyboard();
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
    surface: Option<SurfacePresenter>,
    passes_todo: Vec<PassId>,
}

