use std::cell::{Cell, RefCell};

#[allow(unused)]
use makepad_jni_sys as jni_sys;
use crate::event::LongPressEvent;

use {
    std::rc::Rc,
    std::ffi::CString,
    //std::os::raw::{c_void},
    std::time::{Instant},
    std::sync::{mpsc},
    std::collections::HashMap,
    jni_sys::jobject,
    self::super::{
        android_media::CxAndroidMedia,
        android_jni::{self, *},
        android_keycodes::android_to_makepad_key_code,
        super::egl_sys::{self, LibEgl},
        super::libc_sys,
        ndk_sys,
        
    },
    self::super::super::{
        openxr::{CxOpenXr, CxOpenXrOptions},
        gl_sys,
        gl_sys::LibGl,
        //libc_sys,
    },
    crate::{
        cx_api::{CxOsOp, CxOsApi, OpenUrlInPlace},
        cx_stdin::{PollTimers,PollTimer},
        makepad_math::*,
        makepad_live_id::*,
        //makepad_live_compiler::LiveFileChange,
        thread::SignalToUI,
        studio::{AppToStudio,GPUSample},
        event::{
            VirtualKeyboardEvent,
            NetworkResponseItem,
            NetworkResponse,
            HttpResponse,
            HttpError,
            //TouchPoint,
            TouchUpdateEvent,
            WindowGeomChangeEvent,
            //TimerEvent,
            TextInputEvent,
            TextClipboardEvent,
            TextActionCapabilities,
            KeyEvent,
            KeyModifiers,
            KeyCode,
            Event,
            WindowGeom,
            VideoPlaybackPreparedEvent,
            VideoTextureUpdatedEvent,
            VideoDecodingErrorEvent,
            VideoPlaybackCompletedEvent,
            VideoPlaybackResourcesReleasedEvent,
            //HttpRequest,
            //HttpMethod,
        },
        //web_socket::WebSocket,
        window::CxWindowPool,
        pass::CxPassParent,
        cx::{Cx, OsType},
        gpu_info::GpuPerformance,
        os::cx_native::EventFlow,
        pass::{PassClearColor, PassClearDepth, PassId},
        web_socket::WebSocketMessage,
    },
    makepad_http::websocket::ServerWebSocket as WebSocketImpl,
    makepad_http::websocket::ServerWebSocketMessage as WebSocketMessageImpl
};

/*
fn android_debug_log(msg:&str){
    use std::ffi::c_int;
    extern "C" { 
        pub fn __android_log_write(prio: c_int, tag: *const u8, text: *const u8) -> c_int;
    }
    let msg = format!("{}\0", msg);
    unsafe{__android_log_write(3, "Makepad\0".as_ptr(), msg.as_ptr())};
}*/

impl Cx {

    /// Main event loop for the Android platform.
    /// This method waits for messages from the Java side, particularly the RenderLoop message,
    /// which is sent on Android Choreographer callbacks to sync with vsync.
    /// It handles all incoming messages, processes other events, and manages drawing operations.
    pub fn main_loop(&mut self, from_java_rx: mpsc::Receiver<FromJavaMessage>) {
        self.gpu_info.performance = GpuPerformance::Tier1;

        self.call_event_handler(&Event::Startup);
        self.redraw_all();

        self.start_network_live_file_watcher();
        
        while !self.os.quit {
            if self.os.in_xr_mode{
                if self.openxr_render_loop(&from_java_rx){continue};
            }
            
            // Wait for the next message, blocking until one is received.
            // This ensures we're in sync with the Android Choreographer when we receive a RenderLoop message.
            match from_java_rx.recv() {
                Ok(FromJavaMessage::RenderLoop) => {
                    while let Ok(msg) = from_java_rx.try_recv() {
                        self.handle_message(msg);
                    }
                    self.handle_other_events();
                    self.handle_drawing();
                },
                Ok(message) => {
                    self.handle_message(message);
                },
                Err(e) => {
                    crate::error!("Error receiving message: {:?}", e);
                    break;
                }
            }
        }
        self.os.openxr.destroy_instance(
            &self.os.display.as_ref().unwrap().libgl
        ).ok();
        from_java_messages_clear()
    }
    
    pub(crate) fn handle_message(&mut self, msg: FromJavaMessage) {
        match msg {
            FromJavaMessage::SwitchedActivity(activity_handle, activity_thread_id)=>{
                self.os.activity_thread_id = Some(activity_thread_id);
                if self.os.in_xr_mode{
                    if let Err(e) = self.os.openxr.create_instance(activity_handle){
                        crate::error!("OpenXR init failed: {}", e);
                    }
                }
            },
            FromJavaMessage::RenderLoop => {
                // This should not happen here, as it's handled in the main loop
            },
            FromJavaMessage::BackPressed => {
                self.call_event_handler(&Event::BackPressed { handled: Cell::new(false)});
            }
            FromJavaMessage::SurfaceCreated {window} => unsafe {
                self.os.display.as_mut().unwrap().update_surface(window);
            },
            FromJavaMessage::SurfaceDestroyed => unsafe {
                let owner = self.keyboard.text_selection;
                self.clear_text_selection(owner);
                self.clear_key_focus_if(owner);
                self.os.display.as_mut().unwrap().destroy_surface();
            },
            FromJavaMessage::SurfaceChanged {
                window,
                width,
                height,
            } => {
                
                if self.os.in_xr_mode && self.os.openxr.session.is_none(){
                    if self.os.openxr.libxr.is_none(){
                        let activity_handle = makepad_android_state::get_activity();
                        self.os.openxr.create_instance(activity_handle).ok();
                    }                    
                    if let Err(e) = self.os.openxr.create_session(self.os.display.as_ref().unwrap(),CxOpenXrOptions{
                        buffer_scale: 1.5,
                        multisamples: 4,
                        remove_hands_from_depth: false
                    },&self.os_type){
                        crate::error!("OpenXR create_xr_session failed: {}", e);
                    }
                }
                
                unsafe {
                    self.os.display.as_mut().unwrap().update_surface(window);
                }
                
                self.os.display_size = dvec2(width as f64, height as f64);
                let window_id = CxWindowPool::id_zero();
                let window = &mut self.windows[window_id];
                let old_geom = window.window_geom.clone();
                
                let dpi_factor = window.dpi_override.unwrap_or(self.os.dpi_factor);
                let size = self.os.display_size / dpi_factor;
                window.window_geom = WindowGeom {
                    dpi_factor,
                    can_fullscreen: false,
                    xr_is_presenting: false,
                    is_fullscreen: true,
                    is_topmost: true,
                    position: dvec2(0.0, 0.0),
                    inner_size: size,
                    outer_size: size,
                };
                let new_geom = window.window_geom.clone();
                self.call_event_handler(&Event::WindowGeomChange(WindowGeomChangeEvent {
                    window_id,
                    new_geom,
                    old_geom
                }));
                if let Some(main_pass_id) = self.windows[window_id].main_pass_id {
                    self.redraw_pass_and_child_passes(main_pass_id);
                }
                self.redraw_all();
                self.os.first_after_resize = true;
                self.call_event_handler(&Event::ClearAtlasses);
            }
            FromJavaMessage::LongClick { abs, pointer_id, time } => {
                let window = &mut self.windows[CxWindowPool::id_zero()];
                let dpi_factor = window.dpi_override.unwrap_or(self.os.dpi_factor);
                let e = Event::LongPress(LongPressEvent {
                    abs: abs / dpi_factor,
                    uid: pointer_id,
                    window_id: CxWindowPool::id_zero(),
                    time,
                });
                self.call_event_handler(&e);
            }
            FromJavaMessage::Touch { mut touches, cancelled } => {
                let time = touches[0].time;
                let window = &mut self.windows[CxWindowPool::id_zero()];
                let dpi_factor = window.dpi_override.unwrap_or(self.os.dpi_factor);
                for touch in &mut touches {
                    // When the software keyboard shifted the UI in the vertical axis,
                    //we need to make the math here to keep touch events positions synchronized.
                    //if self.os.keyboard_visible {touch.abs.y += self.os.keyboard_panning_offset as f64};
                    //crate::log!("{} {:?} {} {}", time, touch.state, touch.uid, touch.abs);
                    touch.abs /= dpi_factor;
                }
                if cancelled {
                    self.fingers.cancel_touch_update(&touches);
                }
                self.fingers.process_touch_update_start(time, &touches);
                let e = Event::TouchUpdate(
                    TouchUpdateEvent {
                        time,
                        cancelled,
                        window_id: CxWindowPool::id_zero(),
                        touches,
                        modifiers: Default::default()
                    }
                );
                self.call_event_handler(&e);
                let e = if let Event::TouchUpdate(e) = e {e}else {panic!()};
                self.fingers.process_touch_update_end(&e.touches);
            }
            FromJavaMessage::TextEdit(edit) => {
                if edit.session == self.os.text_input.session
                    && edit.revision > self.os.text_input.revision
                    && self.has_key_focus(self.os.text_input.area)
                {
                    self.os.text_input.revision = edit.revision;
                    self.os.text_input.processing = true;
                    self.call_event_handler(&Event::TextIme(edit));
                    self.os.text_input.processing = false;
                }
            }
            FromJavaMessage::TextAction { token, action, text } => {
                self.android_text_action(token, action, text);
            }
            FromJavaMessage::Character {character} => {
                if let Some(character) = char::from_u32(character) {
                    let e = Event::TextInput(
                        TextInputEvent {
                            input: character.to_string(),
                            replace_last: false,
                            was_paste: false,
                        }
                    );
                    self.call_event_handler(&e);
                }
            }
            FromJavaMessage::KeyDown {keycode, meta_state} => {
                let e: Event;
                let makepad_keycode = android_to_makepad_key_code(keycode);
                if !makepad_keycode.is_unknown() {
                    let control = meta_state & ANDROID_META_CTRL_MASK != 0;
                    let alt = meta_state & ANDROID_META_ALT_MASK != 0;
                    let shift = meta_state & ANDROID_META_SHIFT_MASK != 0;
                    if makepad_keycode == KeyCode::Back {
                        self.call_event_handler(&Event::BackPressed { handled: Cell::new(false)});
                    }

                    e = Event::KeyDown(
                        KeyEvent {
                            key_code: makepad_keycode,
                            is_repeat: false,
                            modifiers: KeyModifiers {shift, control, alt, ..Default::default()},
                            time: self.os.timers.time_now()
                        }
                    );
                    self.call_event_handler(&e);
                }
            }
            FromJavaMessage::KeyUp {keycode, meta_state} => {
                let makepad_keycode = android_to_makepad_key_code(keycode);
                let control = meta_state & ANDROID_META_CTRL_MASK != 0;
                let alt = meta_state & ANDROID_META_ALT_MASK != 0;
                let shift = meta_state & ANDROID_META_SHIFT_MASK != 0;

                let e = Event::KeyUp(
                    KeyEvent {
                        key_code: makepad_keycode,
                        is_repeat: false,
                        modifiers: KeyModifiers {shift, control, alt, ..Default::default()},
                        time: self.os.timers.time_now()
                    }
                );
                self.call_event_handler(&e);
            }
            FromJavaMessage::ResizeTextIME {keyboard_height, is_open} => {
                let keyboard_height = (keyboard_height as f64) / self.os.dpi_factor;
                let was_open = self.os.keyboard_visible;
                self.os.keyboard_visible = is_open;
                if !is_open {
                    self.os.keyboard_closed = keyboard_height;
                }
                if is_open {
                    self.call_event_handler(&Event::VirtualKeyboard(VirtualKeyboardEvent::DidShow {
                        height: keyboard_height - self.os.keyboard_closed,
                        time: self.os.timers.time_now()
                    }))
                }
                else if was_open {
                    // This is an observed dismissal, not another hide request.
                    self.os.ime_show_pending = false;
                    self.os.ime_show_requested = false;
                    self.keyboard.set_text_ime_dismissed();
                    self.platform_ops.retain(|op| !matches!(op, CxOsOp::ShowTextIME(..)));
                    self.call_event_handler(&Event::VirtualKeyboard(VirtualKeyboardEvent::DidHide {
                        time: self.os.timers.time_now()
                    }))
                }
            }
            FromJavaMessage::HttpResponse {request_id, metadata_id, status_code, headers, body} => {
                let e = Event::NetworkResponses(vec![
                    NetworkResponseItem {
                        request_id: LiveId(request_id),
                        response: NetworkResponse::HttpResponse(HttpResponse::new(
                            LiveId(metadata_id),
                            status_code,
                            headers,
                            Some(body)
                        ))
                    }
                ]);
                self.call_event_handler(&e);
            }
            FromJavaMessage::HttpRequestError {request_id, metadata_id, error, ..} => {
                let e = Event::NetworkResponses(vec![
                    NetworkResponseItem {
                        request_id: LiveId(request_id),
                        response: NetworkResponse::HttpRequestError(HttpError{
                            message: error,
                            metadata_id: LiveId(metadata_id)
                        })
                    }
                ]);
                self.call_event_handler(&e);
            }
            FromJavaMessage::WebSocketMessage {message, sender} => {
                let ws_message_parser = self.os.websocket_parsers.entry(sender.0).or_insert_with(||  WebSocketImpl::new());
                ws_message_parser.parse(&message, | result | {
                    match result {
                        Ok(WebSocketMessageImpl::Text(text_msg)) => {
                            let message = WebSocketMessage::String(text_msg.to_string());
                            sender.1.send(message).unwrap();
                        },
                        Ok(WebSocketMessageImpl::Binary(data)) => {
                            let message = WebSocketMessage::Binary(data.to_vec());
                            sender.1.send(message).unwrap();
                        },
                        Err(e) => {
                            println!("Websocket message parse error {:?}", e);
                        },
                        _ => ()
                    }
                });
            }
            FromJavaMessage::WebSocketClosed {sender} => {
                self.os.websocket_parsers.remove(&sender.0);
                let message = WebSocketMessage::Closed;
                sender.1.send(message).ok();
            }
            FromJavaMessage::WebSocketError {error, sender} => {
                self.os.websocket_parsers.remove(&sender.0);
                let message = WebSocketMessage::Error(error);
                sender.1.send(message).ok();
            }
            FromJavaMessage::MidiDeviceOpened {name, midi_device} => {
                self.os.media.android_midi().lock().unwrap().midi_device_opened(name, midi_device);
            }
            FromJavaMessage::VideoPlaybackPrepared {video_id, video_width, video_height, duration, surface_texture} => {
                let e = Event::VideoPlaybackPrepared(
                    VideoPlaybackPreparedEvent {
                        video_id: LiveId(video_id),
                        video_width,
                        video_height,
                        duration,
                    }
                );

                self.os.video_surfaces.insert(LiveId(video_id), surface_texture);
                self.call_event_handler(&e);
            },
            FromJavaMessage::VideoPlaybackCompleted {video_id} => {
                let e = Event::VideoPlaybackCompleted(
                    VideoPlaybackCompletedEvent {
                        video_id: LiveId(video_id)
                    }
                );
                self.call_event_handler(&e);
            },
            FromJavaMessage::VideoPlayerReleased {video_id} => {
                if let Some(decoder_ref) = self.os.video_surfaces.remove(&LiveId(video_id)) {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_cleanup_video_decoder_ref(env, decoder_ref);
                    }
                }

                let e = Event::VideoPlaybackResourcesReleased(
                    VideoPlaybackResourcesReleasedEvent {
                        video_id: LiveId(video_id)
                    }
                );
                self.call_event_handler(&e);
            },
            FromJavaMessage::VideoDecodingError {video_id, error} => {
                let e = Event::VideoDecodingError(
                    VideoDecodingErrorEvent {
                        video_id: LiveId(video_id),
                        error,
                    }
                );
                self.call_event_handler(&e);
            },
            FromJavaMessage::Pause => {
                self.call_event_handler(&Event::Pause);
            }
            FromJavaMessage::Resume => {
                if self.os.fullscreen {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_set_full_screen(env, true);
                    }
                }
                self.redraw_all();
                self.reinitialise_media();
                self.call_event_handler(&Event::Resume);
            }

            FromJavaMessage::Start => {
                self.call_event_handler(&Event::Foreground);
                
            }
            FromJavaMessage::Stop => {
                self.call_event_handler(&Event::Background);
            }
            FromJavaMessage::Destroy => {
                if !self.os.ignore_destroy{
                    self.call_event_handler(&Event::Shutdown);
                    self.os.quit = true;
                }
                
                self.os.ignore_destroy = false;
            }
            FromJavaMessage::WindowFocusChanged { has_focus } => {
                if has_focus {
                    self.call_event_handler(&Event::AppGotFocus);
                } else {
                    self.call_event_handler(&Event::AppLostFocus);
                }
            }
            FromJavaMessage::Init(_) => {
            }
        }
    }

    pub(crate) fn handle_drawing(&mut self) {
        if self.any_passes_dirty() || self.need_redrawing() || !self.new_next_frames.is_empty() || self.demo_time_repaint {
            if !self.new_next_frames.is_empty() {
                self.call_next_frame_event(self.os.timers.time_now());
            }
            if self.need_redrawing() {
                self.call_draw_event();
                self.opengl_compile_shaders();
            }

            if self.os.first_after_resize {
                self.os.first_after_resize = false;
                self.redraw_all();
            }

            self.handle_repaint();
        }
        // A blur may be followed by another editor during this draw. Only hide
        // after that editor has had a chance to publish its native input state.
        let editor_area = self.os.text_input.area;
        if editor_area != crate::area::Area::Empty
            && (!editor_area.is_valid(self) || !self.has_key_focus(editor_area)) {
            self.android_clear_text_input(editor_area);
        }
        if self.os.ime_blur_pending {
            self.hide_text_ime();
        }
        self.android_sync_text_actions();
    }

    /// Processes events that need to be checked regularly, regardless of incoming messages.
    /// This includes timers, signals, video updates, live edits, and platform operations.
    pub(crate) fn handle_other_events(&mut self) {
        // Timers
        for event in self.os.timers.get_dispatch() {
            self.call_event_handler(&event);
        }
    
        // Signals
        if SignalToUI::check_and_clear_ui_signal() {
            self.handle_media_signals();
            self.call_event_handler(&Event::Signal);
        }
        if SignalToUI::check_and_clear_action_signal() {
            self.handle_action_receiver();
        }

        // Video updates
        let to_dispatch = self.get_video_updates();
        for video_id in to_dispatch {
            let e = Event::VideoTextureUpdated(
                VideoTextureUpdatedEvent {
                    video_id,
                }
            );
            self.call_event_handler(&e);
        }

        // Live edits
        if self.handle_live_edit() {
            self.call_event_handler(&Event::LiveEdit);
            self.redraw_all();
        }

        // Platform operations
        self.handle_platform_ops();
    }

    fn get_video_updates(&mut self) -> Vec<LiveId> {
        let mut videos_to_update = Vec::new();
        for (live_id, surface_texture) in self.os.video_surfaces.iter_mut() {
                unsafe {
                    let env = attach_jni_env();
                    let updated = android_jni::to_java_update_tex_image(env, *surface_texture);
                    if updated {
                        videos_to_update.push(*live_id);
                    }
                }
        }
        videos_to_update
    }

    pub fn android_entry<F>(activity: *const std::ffi::c_void, startup: F) where F: FnOnce() -> Box<Cx> + Send + 'static {
        let activity_thread_id = unsafe { libc_sys::gettid() as u64 };
        let activity_handle = unsafe {android_jni::fetch_activity_handle(activity)};
        
        let already_running = android_jni::from_java_messages_already_set();
        
        if already_running{
            android_jni::jni_update_activity(activity_handle);
            // maybe send activity update?
            android_jni::send_from_java_message(FromJavaMessage::SwitchedActivity(
                activity_handle, activity_thread_id
            ));
            
            return
        }
        
        let (from_java_tx, from_java_rx) = mpsc::channel();
        
        std::panic::set_hook(Box::new(|info| {
            crate::log!("Custom panic hook: {}", info);
        }));
        
        android_jni::jni_set_activity(activity_handle);
        android_jni::jni_set_from_java_tx(from_java_tx);
                        
        // lets start a thread
        std::thread::spawn(move || {
                        
            // SAFETY: This attaches the current thread to the JVM. It's safe as long as we're in the correct thread.
            unsafe {attach_jni_env()};
            let mut cx = startup();
            cx.android_load_dependencies();
            let mut libegl = LibEgl::try_load().expect("Cant load LibEGL");
            
            cx.os.activity_thread_id = Some(activity_thread_id);
            cx.os.render_thread_id = Some(unsafe { libc_sys::gettid() as u64 });
            
            let window = loop {
                // Here use blocking method `recv` to reduce CPU usage during cold start.
                match from_java_rx.recv() {
                    Ok(FromJavaMessage::Init(params)) => {
                        cx.os.dpi_factor = params.density;
                        cx.os_type = OsType::Android(params);
                    }
                    Ok(FromJavaMessage::SurfaceChanged {
                        window,
                        width,
                        height,
                    }) => {
                        cx.os.display_size = dvec2(width as f64, height as f64);
                        break window;
                    }
                    _=>()
                }
            };
            
            // SAFETY:
            // The LibEgl instance (libegl) has been properly loaded and initialized earlier.
            // We're not requesting a robust context (false), which is usually fine for most applications.
            #[cfg(not(quest))]
            let (egl_context, egl_config, egl_display) = unsafe {egl_sys::create_egl_context(
                &mut libegl,
                std::ptr::null_mut(),/* EGL_DEFAULT_DISPLAY */
            ).expect("Cant create EGL context")};
            
            #[cfg(quest)]
            let (egl_context, egl_config, egl_display) = unsafe {egl_sys::create_egl_context_openxr(
                &mut libegl,
                std::ptr::null_mut(),/* EGL_DEFAULT_DISPLAY */
            ).expect("Cant create EGL context")};
            
            // SAFETY: This is loading OpenGL function pointers. It's safe as long as we have a valid EGL context.
            let libgl = LibGl::try_load(| s | {
                for s in s{
                    let s = CString::new(*s).unwrap();
                    let p = unsafe{libegl.eglGetProcAddress.unwrap()(s.as_ptr())};
                    if !p.is_null(){
                        return p
                    }
                }
                0 as _
            }).expect("Cant load openGL functions");
            
            // SAFETY: This creates an EGL surface. It's safe as long as we have valid EGL display, config, and window.
            //libgl.enable_debugging();
            
            let surface = unsafe {(libegl.eglCreateWindowSurface.unwrap())(
                egl_display,
                egl_config,
                window as _,
                std::ptr::null_mut(),
            )};

            if unsafe {(libegl.eglMakeCurrent.unwrap())(egl_display, surface, surface, egl_context)} == 0 {
                panic!();
            }
            
            //libgl.enable_debugging();

            //cx.maybe_warn_hardware_support();

            cx.os.display = Some(CxAndroidDisplay {
                libegl,
                libgl,
                egl_display,
                egl_config,
                egl_context,
                surface,
                window
            });
            
            cx.main_loop(from_java_rx);
            cx.stop_studio_websocket();
            
            let display = cx.os.display.take().unwrap();

            // SAFETY: These calls clean up EGL resources. They're safe as long as we have valid EGL objects.
            unsafe {
                (display.libegl.eglMakeCurrent.unwrap())(
                    display.egl_display,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                );
                (display.libegl.eglDestroySurface.unwrap())(display.egl_display, display.surface);
                (display.libegl.eglDestroyContext.unwrap())(display.egl_display, display.egl_context);
                (display.libegl.eglTerminate.unwrap())(display.egl_display);
            }
        });
    }


    pub fn start_network_live_file_watcher(&mut self) {

        /*
        log!("WATCHING NETWORK FOR FILE WATCHER");
        let studio_uid: Option<&'static str> = std::option_env!("MAKEPAD_STUDIO_UID");
        if studio_uid.is_none(){
            return
        }
        let studio_uid:u64 = studio_uid.unwrap().parse().unwrap_or(0);
        std::thread::spawn(move || {
            let discovery = UdpSocket::bind("0.0.0.0:41533").unwrap();
            discovery.set_read_timeout(Some(Duration::new(0, 1))).unwrap();
            discovery.set_broadcast(true).unwrap();

            let mut other_uid = [0u8; 8];
            let mut host_addr = None;
            // nonblockingly (timeout=1ns) check our discovery socket for peers
            'outer: loop{
                while let Ok((_, mut addr)) = discovery.recv_from(&mut other_uid) {
                    let recv_uid = u64::from_be_bytes(other_uid);
                    log!("GOT ADDR {} {}",studio_uid, recv_uid);
                    if studio_uid == recv_uid {
                        // we found our host. lets connect to it
                        host_addr = Some(addr);
                        break 'outer;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let host_addr = host_addr.unwrap();
            // ok we can connect
            log!("WE CAN CONNECT {:?}", host_addr);
        });*/
    }

    /*
    pub fn from_java_on_paste_from_clipboard(&mut self, content: Option<String>, to_java: AndroidToJava) {
        if let Some(text) = content {
            let e = Event::TextInput(
                TextInputEvent {
                    input: text,
                    replace_last: false,
                    was_paste: true,
                }
            );
            self.call_event_handler(&e);
            self.after_every_event(&to_java);
        }
    }

    pub fn from_java_on_cut_to_clipboard(&mut self, to_java: AndroidToJava) {
        let e = Event::TextCut(
            TextClipboardEvent {
                response: Rc::new(RefCell::new(None))
            }
        );
        self.call_event_handler(&e);
        self.after_every_event(&to_java);
    }
   */


    pub fn android_load_dependencies(&mut self) {
        for (path, dep) in &mut self.dependencies {
            if let Some(data) = unsafe {to_java_load_asset(path)} {
                dep.data = Some(Ok(Rc::new(data)))
            }
            else {
                let message = format!("cannot load dependency {}", path);
                crate::error!("Android asset failed: {}", message);
                dep.data = Some(Err(message));
            }
        }
    }

    pub fn draw_pass_to_fullscreen(
        &mut self,
        pass_id: PassId,
    ) {
        let draw_list_id = self.passes[pass_id].main_draw_list_id.unwrap();

        self.setup_render_pass(pass_id);

        // keep repainting in a loop
        self.passes[pass_id].paint_dirty = false;
        //let panning_offset = if self.os.keyboard_visible {self.os.keyboard_panning_offset} else {0};

        let gl = self.os.gl();
        unsafe {
            (gl.glViewport)(0, 0, self.os.display_size.x as i32, self.os.display_size.y as i32);
        }

        let clear_color = if self.passes[pass_id].color_textures.len() == 0 {
            self.passes[pass_id].clear_color
        }
        else {
            match self.passes[pass_id].color_textures[0].clear_color {
                PassClearColor::InitWith(color) => color,
                PassClearColor::ClearWith(color) => color
            }
        };
        let clear_depth = match self.passes[pass_id].clear_depth {
            PassClearDepth::InitWith(depth) => depth,
            PassClearDepth::ClearWith(depth) => depth
        };

        if !self.passes[pass_id].dont_clear {
            unsafe {
                //(gl.glBindFramebuffer)(gl_sys::FRAMEBUFFER, 0);
                (gl.glClearDepthf)(clear_depth as f32);
                (gl.glClearColor)(clear_color.x, clear_color.y, clear_color.z, clear_color.w);
                (gl.glClear)(gl_sys::COLOR_BUFFER_BIT | gl_sys::DEPTH_BUFFER_BIT);
            }
        }
        Self::set_default_depth_and_blend_mode(gl);

        let mut zbias = 0.0;
        let zbias_step = self.passes[pass_id].zbias_step;

        self.render_view(
            pass_id,
            draw_list_id,
            &mut zbias,
            zbias_step,
        );

        //to_java.swap_buffers();
        //unsafe {
        //direct_app.drm.swap_buffers_and_wait(&direct_app.egl);
        //}
    }

    pub (crate) fn handle_repaint(&mut self) {
        //opengl_cx.make_current();
        let mut passes_todo = Vec::new();
        self.compute_pass_repaint_order(&mut passes_todo);
        self.repaint_id += 1;
        for pass_id in &passes_todo {
            self.passes[*pass_id].set_time(self.os.timers.time_now() as f32);
            match self.passes[*pass_id].parent.clone() {
                CxPassParent::Xr=>{
                    // cant happen
                }
                CxPassParent::Window(_) => {
                    //let window = &self.windows[window_id];
                    let start = self.seconds_since_app_start();
                    self.draw_pass_to_fullscreen(*pass_id);
                    let end = self.seconds_since_app_start(); 
                    Cx::send_studio_message(AppToStudio::GPUSample(GPUSample{
                        start, end 
                    }));
                    unsafe {
                        if let Some(display) = &mut self.os.display { 
                            (display.libegl.eglSwapBuffers.unwrap())(display.egl_display, display.surface);
                        }
                    }
                }
                CxPassParent::Pass(_) => {
                    //let dpi_factor = self.get_delegated_dpi_factor(parent_pass_id);
                    self.draw_pass_to_texture(*pass_id, None);
                },
                CxPassParent::None => {
                    self.draw_pass_to_texture(*pass_id, None);
                }
            }
        }


    }

    fn handle_platform_ops(&mut self) -> EventFlow {
        while let Some(op) = self.platform_ops.pop() {
            match op {
                CxOsOp::CreateWindow(window_id) => {
                    let window = &mut self.windows[window_id];
                    let dpi_factor = window.dpi_override.unwrap_or(self.os.dpi_factor);
                    let size = self.os.display_size / dpi_factor;
                    window.window_geom = WindowGeom {
                        dpi_factor,
                        can_fullscreen: false,
                        xr_is_presenting: false,
                        is_fullscreen: true,
                        is_topmost: true,
                        position: dvec2(0.0, 0.0),
                        inner_size: size,
                        outer_size: size,
                    };
                    window.is_created = true;
                    //let ret = unsafe{ndk_sys::ANativeWindow_setFrameRate(self.os.display.as_ref().unwrap().window, 120.0, 0)};
                    //crate::log!("{}",ret);
                    let new_geom = window.window_geom.clone();
                    let old_geom = window.window_geom.clone();
                    self.call_event_handler(&Event::WindowGeomChange(WindowGeomChangeEvent {
                        window_id,
                        new_geom,
                        old_geom
                    }));
                },
                CxOsOp::StartTimer {timer_id, interval, repeats} => {
                    self.os.timers.timers.insert(timer_id, PollTimer::new(interval, repeats));
                },
                CxOsOp::StopTimer(timer_id) => {
                    self.os.timers.timers.remove(&timer_id);
                },
                CxOsOp::ShowTextIME(_area, _pos) => {
                    // ShowTextIME is queued only for a new focus or explicit reopening.
                    unsafe {android_jni::to_java_show_keyboard(true);}
                },
                CxOsOp::HideTextIME => {
                    unsafe {android_jni::to_java_show_keyboard(false);}
                },
                CxOsOp::CopyToClipboard(content) => {
                    unsafe {android_jni::to_java_copy_to_clipboard(content);}
                },
                CxOsOp::HttpRequest {request_id, request} => {
                    unsafe {android_jni::to_java_http_request(request_id, request);}
                },
                CxOsOp::PrepareVideoPlayback(video_id, source, external_texture_id, autoplay, should_loop) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_prepare_video_playback(env, video_id, source, external_texture_id, autoplay, should_loop);
                    }
                },
                CxOsOp::BeginVideoPlayback(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_begin_video_playback(env, video_id);
                    }
                }
                CxOsOp::PauseVideoPlayback(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_pause_video_playback(env, video_id);
                    }
                },
                CxOsOp::ResumeVideoPlayback(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_resume_video_playback(env, video_id);
                    }
                },
                CxOsOp::MuteVideoPlayback(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_mute_video_playback(env, video_id);
                    }
                },
                CxOsOp::UnmuteVideoPlayback(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_unmute_video_playback(env, video_id);
                    }
                },
                CxOsOp::CleanupVideoPlaybackResources(video_id) => {
                    unsafe {
                        let env = attach_jni_env();
                        android_jni::to_java_cleanup_video_playback_resources(env, video_id);
                    }
                },
                CxOsOp::XrStartPresenting=>{
                    self.os.ignore_destroy = true;
                    if !self.os.in_xr_mode{
                        self.os.in_xr_mode = true;
                        unsafe {
                            let env = attach_jni_env();
                            android_jni::to_java_switch_activity(env);
                        }
                    }                        
                }
                CxOsOp::XrStopPresenting=>{
                    self.os.ignore_destroy = true;
                    if self.os.in_xr_mode{
                        self.os.in_xr_mode = false;
                        unsafe {
                            let env = attach_jni_env();
                            android_jni::to_java_switch_activity(env);
                        }
                    }       
                }
                CxOsOp::XrAdvertiseAnchor(anchor)=>{
                    self.os.openxr.advertise_anchor(anchor);
                }
                CxOsOp::XrSetLocalAnchor(anchor)=>{
                    self.os.openxr.set_local_anchor(anchor);
                }
                CxOsOp::XrDiscoverAnchor(id)=>{
                    self.os.openxr.discover_anchor(id);
                }
                CxOsOp::SetCursor(_)=>{
                    // no need
                },
                e=>{
                    crate::error!("Not implemented on this platform: CxOsOp::{:?}", e);
                }
            }
        }
        EventFlow::Poll
    }
}

impl CxOsApi for Cx {
    fn init_cx_os(&mut self) {
        self.live_registry.borrow_mut().package_root = Some("makepad".to_string());
        self.live_expand();
        self.live_scan_dependencies();
    }

    fn spawn_thread<F>(&mut self, f: F) where F: FnOnce() + Send + 'static {
        std::thread::spawn(f);
    }
    
    fn seconds_since_app_start(&self)->f64{
        Instant::now().duration_since(self.os.start_time).as_secs_f64()
    }
    
    fn open_url(&mut self, _url:&str, _in_place:OpenUrlInPlace){
        crate::error!("open_url not implemented on this platform");
    }
    
    fn in_xr_mode(&self)->bool{
        self.os.in_xr_mode
    }
    
    fn micro_zbias_step(&self)->f32{
        if self.os.in_xr_mode{
            -0.0001
        }
        else{
            0.00001
        }
    }
}

#[derive(Default)]
pub(crate) struct AndroidTextInput {
    pub area: crate::area::Area,
    editor: u64,
    session: u64,
    revision: u64,
    processing: bool,
    text: String,
    selection: (usize, usize),
    password: bool,
    numeric: bool,
}

impl Cx {
    pub(crate) fn android_sync_text_actions(&mut self) {
        if let Some(request) = self.keyboard.text_actions {
            if !request.area.is_valid(self) || !self.has_key_focus(request.area)
                || self.keyboard.text_selection != request.area {
                self.keyboard.hide_text_actions(request.area);
            }
        }
        let dpi = self.windows[CxWindowPool::id_zero()].dpi_override.unwrap_or(self.os.dpi_factor);
        let request = self.keyboard.text_actions.and_then(|request| {
            let clip = request.area.clipped_rect(self);
            let rect = match request.anchor {
                crate::event::TextActionsAnchor::Rect(relative) => {
                    let mut origin = request.area.rect(self).pos;
                    if let Some(id) = request.area.draw_list_id() {
                        let list = &self.draw_lists[id];
                        if list.draw_list_has_clip {
                            origin += dvec2(list.draw_list_uniforms.view_shift.x as f64, list.draw_list_uniforms.view_shift.y as f64);
                        }
                    }
                    Some(relative.translate(origin))
                }
                crate::event::TextActionsAnchor::Area(area) => self.android_visible_text_action_rect(area),
            }?.clip((clip.pos, clip.pos + clip.size));
            (rect.size.x > 0.0 && rect.size.y > 0.0)
                .then_some((request.token, rect, request.capabilities, dpi))
        });
        if request.is_none() {
            if let Some(stale) = self.keyboard.text_actions {
                self.keyboard.hide_text_actions(stale.area);
            }
        }
        if self.os.text_actions_sent == request { return; }
        self.os.text_actions_sent = request;
        if let Some((token, rect, caps, dpi)) = request {
            let flags = caps.copy as i32 | ((caps.cut as i32) << 1)
                | ((caps.paste as i32) << 2) | ((caps.select_all as i32) << 3);
            unsafe { android_jni::to_java_text_actions(token, flags, rect, dpi); }
        } else {
            unsafe { android_jni::to_java_hide_text_actions(); }
        }
    }

    fn android_visible_text_action_rect(&self, area: crate::area::Area) -> Option<crate::makepad_math::Rect> {
        if !area.is_valid(self) { return None; }
        if let crate::area::Area::Instance(instance) = area {
            let item = &self.draw_lists[instance.draw_list_id].draw_items[instance.draw_item_id];
            let shader = item.draw_call()?.draw_shader;
            let stride = self.draw_shaders[shader.draw_shader_id].mapping.instances.total_slots;
            for index in 0..instance.instance_count {
                let part = crate::area::Area::Instance(crate::area::InstanceArea {
                    instance_offset: instance.instance_offset + index * stride,
                    instance_count: 1,
                    ..instance
                });
                let rect = part.clipped_rect(self);
                if rect.size.x > 0.0 && rect.size.y > 0.0 { return Some(rect); }
            }
            None
        } else {
            let rect = area.clipped_rect(self);
            (rect.size.x > 0.0 && rect.size.y > 0.0).then_some(rect)
        }
    }

    fn android_text_action(&mut self, token: u64, action: u32, text: Option<String>) {
        let Some(request) = self.keyboard.text_actions.filter(|request| {
            request.token == token && self.keyboard.text_selection == request.area
                && request.area.is_valid(self) && self.has_key_focus(request.area)
        }) else { return; };
        let caps = request.capabilities;
        match action {
            0 => self.hide_text_actions(request.area),
            1 | 2 if if action == 1 { caps.copy } else { caps.cut } => {
                let response = Rc::new(RefCell::new(None));
                let clipboard = TextClipboardEvent { response: response.clone() };
                self.call_event_handler(&if action == 1 {
                    Event::TextCopy(clipboard)
                } else {
                    Event::TextCut(clipboard)
                });
                if let Some(text) = response.borrow_mut().take() {
                    unsafe { android_jni::to_java_copy_to_clipboard(text); }
                }
                self.hide_text_actions(request.area);
            }
            3 if caps.paste => {
                if let Some(input) = text {
                    self.call_event_handler(&Event::TextInput(TextInputEvent {
                        input, replace_last: false, was_paste: true,
                    }));
                }
                self.hide_text_actions(request.area);
            }
            4 if caps.select_all => self.call_event_handler(&Event::TextSelectAll),
            _ => {}
        }
    }

    pub(crate) fn android_update_text_input(&mut self, area: crate::area::Area,
        editor: u64, text: &str, start: usize, end: usize, password: bool, numeric: bool) {
        if !area.is_valid(self) || !self.has_key_focus(area) { return; }
        self.os.ime_blur_pending = false;
        let editor_changed = self.os.text_input.area != area || self.os.text_input.editor != editor;
        if editor_changed {
            self.os.ime_show_pending = true;
            self.keyboard.reset_text_ime_dismissed();
        }
        let state = &mut self.os.text_input;
        if state.area == area && state.editor == editor && state.text == text && state.selection == (start, end)
            && state.password == password && state.numeric == numeric && !state.processing { return; }
        let restart = editor_changed || state.password != password || state.numeric != numeric;
        if restart || !state.processing {
            state.session += 1;
            state.revision = 0;
        }
        state.area = area;
        state.editor = editor;
        state.text.clear();
        state.text.push_str(text);
        state.selection = (start, end);
        state.password = password;
        state.numeric = numeric;
        unsafe { android_jni::to_java_text_input(state.session, state.revision, Some(text),
            text[..start].encode_utf16().count(), text[..end].encode_utf16().count(),
            password, numeric, restart); }
    }

    pub(crate) fn android_clear_text_input(&mut self, area: crate::area::Area) {
        let state = &mut self.os.text_input;
        if area == crate::area::Area::Empty || state.area != area { return; }
        self.os.ime_blur_pending = true;
        state.area = crate::area::Area::Empty;
        state.session += 1;
        state.revision = 0;
        state.text.clear();
        unsafe { android_jni::to_java_text_input(state.session, 0, None, 0, 0, false, false, true); }
    }
}

impl Default for CxOs {
    fn default() -> Self {
        Self {
            text_input: Default::default(),
            text_actions_sent: None,
            start_time: Instant::now(),
            first_after_resize: true,
            frame_time: 0,
            display_size: dvec2(100., 100.),
            dpi_factor: 1.5,
            keyboard_closed: 0.0,
            keyboard_visible: false,
            ime_show_pending: false,
            ime_show_requested: false,
            ime_blur_pending: false,
            media: CxAndroidMedia::default(),
            display: None,
            quit: false,
            fullscreen: false,
            timers: Default::default(),
            video_surfaces: HashMap::new(),
            websocket_parsers: HashMap::new(),
            openxr: CxOpenXr::default(),
            activity_thread_id: None,
            render_thread_id: None,
            ignore_destroy: false,
            in_xr_mode: false
        }
    }
}

pub struct CxAndroidDisplay {
    pub libegl: LibEgl,
    pub libgl: LibGl,
    pub egl_display: egl_sys::EGLDisplay,
    pub egl_config: egl_sys::EGLConfig,
    pub egl_context: egl_sys::EGLContext,
    surface: egl_sys::EGLSurface,
    window: *mut ndk_sys::ANativeWindow,
    //event_handler: Box<dyn EventHandler>,
}


pub struct CxOs {
    pub(crate) text_input: AndroidTextInput,
    text_actions_sent: Option<(u64, Rect, TextActionCapabilities, f64)>,
    pub first_after_resize: bool,
    pub display_size: DVec2,
    pub dpi_factor: f64,
    pub keyboard_closed: f64,
    pub(crate) keyboard_visible: bool,
    pub(crate) ime_show_pending: bool,
    pub(crate) ime_show_requested: bool,
    pub(crate) ime_blur_pending: bool,
    pub frame_time: i64,
    pub quit: bool,
    pub fullscreen: bool,
    pub (crate) start_time: Instant,
    pub (crate) timers: PollTimers,
    pub (crate) display: Option<CxAndroidDisplay>,
    pub (crate) media: CxAndroidMedia,
    pub (crate) video_surfaces: HashMap<LiveId, jobject>,
    websocket_parsers: HashMap<u64, WebSocketImpl>,
    pub (crate) openxr: CxOpenXr,
    pub (crate) activity_thread_id: Option<u64>,
    pub (crate) render_thread_id: Option<u64>,
    pub (crate) ignore_destroy: bool,
    pub (crate) in_xr_mode: bool
}

impl CxOs{
    pub (crate) fn gl(&self)->&LibGl{
        &self.display.as_ref().unwrap().libgl
    }
}

impl CxAndroidDisplay {
    unsafe fn destroy_surface(&mut self) {
        (self.libegl.eglMakeCurrent.unwrap())(
            self.egl_display,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        (self.libegl.eglDestroySurface.unwrap())(self.egl_display, self.surface);
        self.surface = std::ptr::null_mut();
    }

    unsafe fn update_surface(&mut self, window: *mut ndk_sys::ANativeWindow) {
        if !self.window.is_null() {
            ndk_sys::ANativeWindow_release(self.window);
        }
        self.window = window;
        if self.surface.is_null() == false {
            self.destroy_surface();
        }

        self.surface = (self.libegl.eglCreateWindowSurface.unwrap())(
            self.egl_display,
            self.egl_config,
            window as _,
            std::ptr::null_mut(),
        );
        if self.surface.is_null(){
            return
        }

        let res = (self.libegl.eglMakeCurrent.unwrap())(
            self.egl_display,
            self.surface,
            self.surface,
            self.egl_context,
        );

        assert!(res != 0);
    }
}
