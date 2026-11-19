use super::{ios_accessibility, ios_surface::{DisplayLink, SurfaceMetrics}, ios_text_input, ios_wake};
use std::collections::VecDeque;

use {
    crate::{
        area::Area, event::{
            KeyModifiers, LongPressEvent, TimerEvent, TouchPoint, TouchState, TouchUpdateEvent, WindowGeom, WindowGeomChangeEvent
        }, makepad_math::*, os::{
            apple::{apple_sys::*, apple_util::str_to_nsstring}, cx_native::EventFlow, ios::{
                ios_delegates::*,
                ios_event::*,
            }
        }, window::CxWindowPool
    }, std::{
        cell::{Cell,RefCell},
        time::Instant,
    }
};

pub static mut IOS_CLASSES: *const IosClasses = 0 as *const _;

thread_local! {
    pub static IOS_APP: RefCell<Option<IosApp>> = RefCell::new(None);
}

pub fn with_ios_app<R>(f: impl FnOnce(&mut IosApp) -> R) -> R {
    IOS_APP.with_borrow_mut(|app| {
        f(app.as_mut().unwrap())
    })
}

pub fn init_ios_app_global(event_callback: Box<dyn FnMut(IosEvent) -> EventFlow>) {
    unsafe {
        IOS_CLASSES = Box::into_raw(Box::new(IosClasses::new()));
        IOS_APP.with(|app| {
            *app.borrow_mut() = Some(IosApp::new(event_callback));
        })
    }
}


pub fn get_ios_class_global() -> &'static IosClasses {
    unsafe {
        &*(IOS_CLASSES)
    }
}

#[derive(Clone)]
pub struct IosTimer {
    timer_id: u64,
    nstimer: ObjcId,
    repeats: bool
}

pub struct IosClasses {
    pub app_delegate: *const Class,
    pub view: *const Class,
    pub gesture_recognizer_handler: *const Class,
    pub text_view: *const Class,
    pub(super) accessibility_element: *const Class,
    pub input_delegate: *const Class,
    pub timer_delegate: *const Class,
}
impl IosClasses {
    pub fn new() -> Self {
        Self {
            app_delegate: define_ios_app_delegate(),
            view: define_ios_view(),
            gesture_recognizer_handler: define_gesture_recognizer_handler(),
            text_view: ios_text_input::define_text_view(),
            accessibility_element: ios_accessibility::define_element(),
            input_delegate: define_input_delegate(),
            timer_delegate: define_ios_timer_delegate()
        }
    }
}

pub struct IosApp {
    pub time_start: Instant,
    pub timer_delegate_instance: ObjcId,
    timers: Vec<IosTimer>,
    executor_timer: ObjcId,
    touches: Vec<TouchPoint>,
    pub last_window_geom: WindowGeom,
    pub(super) view: ObjcId,
    pub(super) surface_active: bool,
    pub(super) surface_metrics: Option<SurfaceMetrics>,
    display_link: Option<DisplayLink>,
    pending_events: VecDeque<IosEvent>,
    layout_pending: bool,
    input_delegate: ObjcId,
    event_callback: Option<Box<dyn FnMut(IosEvent) -> EventFlow >>,
    pasteboard: ObjcId
}

impl IosApp {
    pub fn new(event_callback: Box<dyn FnMut(IosEvent) -> EventFlow>) -> IosApp {
        unsafe {
            let pasteboard: ObjcId = msg_send![class!(UIPasteboard), generalPasteboard];
            IosApp {
                touches: Vec::new(),
                last_window_geom: WindowGeom::default(),
                view: nil,
                surface_active: false,
                surface_metrics: None,
                display_link: None,
                pending_events: VecDeque::new(),
                layout_pending: false,
                input_delegate: nil,
                time_start: Instant::now(),
                timer_delegate_instance: msg_send![get_ios_class_global().timer_delegate, new],
                timers: Vec::new(),
                executor_timer: nil,
                event_callback: Some(event_callback),
                pasteboard,
            }
        }
    }
    
    pub fn did_finish_launching_with_options(&mut self, delegate: ObjcId) {
        unsafe {
            let main_screen: ObjcId = msg_send![class!(UIScreen), mainScreen];
            let screen_rect: NSRect = msg_send![main_screen, bounds];
            
            let window_obj: ObjcId = msg_send![class!(UIWindow), alloc];
            let window_obj: ObjcId = msg_send![window_obj, initWithFrame: screen_rect];
            
            let view: ObjcId = msg_send![get_ios_class_global().view, alloc];
            let view: ObjcId = msg_send![view, initWithFrame: screen_rect];

            // Instantiate a long-press gesture recognizer and our delegate,
            // set that delegate to be the target of the "gesture recognized" action,
            // and add the gesture recognizer to the content view.
            let gesture_recognizer_handler_obj: ObjcId = msg_send![get_ios_class_global().gesture_recognizer_handler, alloc];
            let gesture_recognizer_handler_obj: ObjcId = msg_send![gesture_recognizer_handler_obj, init];
            let gesture_recognizer_obj: ObjcId = msg_send![class!(UILongPressGestureRecognizer), alloc];
            let gesture_recognizer_obj: ObjcId = msg_send![
                gesture_recognizer_obj,
                initWithTarget: gesture_recognizer_handler_obj
                action: sel!(handleLongPressGesture: gestureRecognizer:)
            ];
            // Set `cancelsTouchesInView` to NO so that the gesture recognizer doesn't prevent
            // later touch events from being sent to the view after recognizing its gesture.
            let () = msg_send!(gesture_recognizer_obj, setCancelsTouchesInView: NO);
            let () = msg_send![view, addGestureRecognizer: gesture_recognizer_obj];
            
            let view_ctrl_obj: ObjcId = msg_send![class!(UIViewController), alloc];
            let view_ctrl_obj: ObjcId = msg_send![view_ctrl_obj, init];
            
            let () = msg_send![view_ctrl_obj, setView: view];
            let () = msg_send![view, setUserInteractionEnabled: YES];
            let () = msg_send![view, setMultipleTouchEnabled: YES];
            
            let input_delegate: ObjcId = msg_send![get_ios_class_global().input_delegate, new];
            let notification_center: ObjcId = msg_send![class!(NSNotificationCenter), defaultCenter];
            for (name, selector) in [
                (UIKeyboardDidChangeFrameNotification, sel!(keyboardDidChangeFrame:)),
                (UIKeyboardWillChangeFrameNotification, sel!(keyboardWillChangeFrame:)),
                (UIKeyboardDidShowNotification, sel!(keyboardDidShow:)),
                (UIKeyboardWillShowNotification, sel!(keyboardWillShow:)),
                (UIKeyboardDidHideNotification, sel!(keyboardDidHide:)),
                (UIKeyboardWillHideNotification, sel!(keyboardWillHide:)),
            ] {
                let () = msg_send![notification_center, addObserver: input_delegate selector: selector name: name object: nil];
            }
            let () = msg_send![view, setIsAccessibilityElement: NO];
            let () = msg_send![notification_center, addObserver: delegate selector: sel!(voiceOverStatusChanged:) name: ios_accessibility::status_notification() object: nil];
            
            let () = msg_send![window_obj, addSubview: view];
            
            let () = msg_send![window_obj, setRootViewController: view_ctrl_obj];
            
            let () = msg_send![window_obj, makeKeyAndVisible];
            
            self.input_delegate = input_delegate;
            self.view = view;
            self.display_link = Some(DisplayLink::new(self.timer_delegate_instance));

            // This bundle uses the UIApplicationDelegate lifecycle. Also honor
            // the hosting scene's notifications if UIKit supplies a windowScene.
            for (name, selector) in [
                ("UISceneDidActivateNotification", sel!(surfaceSceneActivated:)),
                ("UISceneWillDeactivateNotification", sel!(surfaceSceneSuspended:)),
                ("UISceneDidEnterBackgroundNotification", sel!(surfaceSceneSuspended:)),
            ] {
                let () = msg_send![notification_center, addObserver: delegate selector: selector name: str_to_nsstring(name) object: nil];
            }
        }
    }
    
    pub(super) fn set_surface_active(active: bool) {
        let changed = with_ios_app(|app| {
            if app.surface_active == active { return false; }
            app.surface_active = active;
            if !active {
                if let Some(link) = &mut app.display_link { link.set_running(false); }
                app.update_executor_timer(false);
            }
            true
        });
        if changed {
            ios_wake::set_active(active);
            Self::do_callback(if active { IosEvent::SurfaceResumed } else { IosEvent::SurfaceSuspended });
            if active { Self::surface_frame(false); }
        }
    }

    pub(super) fn surface_scene_changed(notification: ObjcId, active: bool) {
        let ours = with_ios_app(|app| unsafe {
            let window: ObjcId = msg_send![app.view, window];
            let scene: ObjcId = msg_send![window, windowScene];
            let subject: ObjcId = msg_send![notification, object];
            scene != nil && scene == subject
        });
        if ours { Self::set_surface_active(active); }
    }

    pub(super) fn surface_frame(display_tick: bool) {
        let update = IOS_APP.with(|cell| {
            // UIKit can run layout synchronously from makeKeyAndVisible (during
            // launch) or during a Rust callback that updates the native view.
            let mut app = cell.try_borrow_mut().ok()?;
            let app = app.as_mut()?;
            if !app.surface_active || app.view == nil { return None; }
            if app.event_callback.is_none() {
                app.layout_pending = true;
                return None;
            }
            // UIKit notifications drive geometry; do not re-read traits, safe
            // area and window bounds on every animation tick.
            if display_tick && app.surface_metrics.is_some() { return Some((false, None)); }
            let metrics = SurfaceMetrics::get(app.view)?;
            let first = app.surface_metrics.is_none();
            let change = (app.surface_metrics.as_ref() != Some(&metrics)).then(|| {
                if let Some(link) = &mut app.display_link { link.set_screen(metrics.screen); }
                let old_geom = std::mem::replace(&mut app.last_window_geom, metrics.geom.clone());
                let change = WindowGeomChangeEvent {
                    window_id: CxWindowPool::id_zero(), old_geom, new_geom: metrics.geom.clone(),
                };
                app.surface_metrics = Some(metrics);
                change
            });
            Some((first, change))
        });
        let Some((first, change)) = update else { return; };
        if first { Self::do_callback(IosEvent::Init); }
        let changed = change.is_some();
        if let Some(change) = change { Self::do_callback(IosEvent::WindowGeomChange(change)); }
        if changed {
            Self::do_callback(IosEvent::SurfaceLayout);
        } else if display_tick {
            Self::do_callback(IosEvent::DisplayFrame);
        }
    }
    
    pub fn update_touch(&mut self, uid: u64, abs: DVec2, state: TouchState) {
        if let Some(touch) = self.touches.iter_mut().find( | v | v.uid == uid) {
            touch.state = state;
            touch.abs = abs;
        }
        else {
            self.touches.push(TouchPoint {
                state,
                abs,
                uid,
                time: self.time_now(),
                rotation_angle: 0.0,
                force: 0.0,
                radius: dvec2(0.0, 0.0),
                handled: Cell::new(Area::Empty),
                sweep_lock: Cell::new(Area::Empty)
            })
        }
    }
    
    pub fn send_touch_update() {
        let time_now = with_ios_app(|app| app.time_now());
        let touches = with_ios_app(|app| app.touches.clone());
        IosApp::do_callback(IosEvent::TouchUpdate(TouchUpdateEvent {
            time: time_now,
            cancelled: false,
            window_id: CxWindowPool::id_zero(),
            modifiers: KeyModifiers::default(),
            touches
        }));
        // remove the stopped touches
        with_ios_app(|app| app.touches.retain( | v | if let TouchState::Stop = v.state {false}else {true}));
    }

    pub fn send_long_press(abs: NSPoint, uid: u64) {
        let time_now = with_ios_app(|app| app.time_now());
        IosApp::do_callback(IosEvent::LongPress(LongPressEvent {
            abs: dvec2(abs.x, abs.y),
            time: time_now,
            window_id: CxWindowPool::id_zero(),
            uid,
        }));
    }

    pub fn time_now(&self) -> f64 {
        let time_now = Instant::now(); //unsafe {mach_absolute_time()};
        (time_now.duration_since(self.time_start)).as_micros() as f64 / 1_000_000.0
    }
    
    pub fn event_loop() {
        unsafe {
            let app_delegate = get_ios_class_global().app_delegate;
            let class: ObjcId = msg_send!(app_delegate, class);
            let class_string = NSStringFromClass(class as _);
            let argc = 1;
            let mut argv = b"Makepad\0" as *const u8 as *mut i8;
            
            UIApplicationMain(argc, &mut argv, nil, class_string);
        }
    }
    
    pub(super) fn text_input_host() -> Option<(ObjcId, ObjcId)> {
        with_ios_app(|app| (app.view != nil).then_some((app.view, app.input_delegate)))
    }

    pub(super) fn defer_callback(event: IosEvent) {
        let needs_wake = with_ios_app(|app| {
            app.pending_events.push_back(event);
            app.event_callback.is_some()
        });
        if needs_wake { ios_wake::post(); }
    }
    
    pub fn do_callback(mut event: IosEvent) {
        let callback = with_ios_app(|app| app.event_callback.take());
        let Some(mut callback) = callback else {
            if matches!(event, IosEvent::Wake | IosEvent::SurfaceResumed | IosEvent::SurfaceSuspended
                | IosEvent::SurfaceMemoryWarning | IosEvent::AccessibilityStatus | IosEvent::TextEdit { .. }) {
                with_ios_app(|app| app.pending_events.push_back(event));
            }
            return;
        };
        loop {
            let flow = callback(event);
            let pending = with_ios_app(|app| {
                if let Some(link) = &mut app.display_link {
                    link.set_running(app.surface_active && matches!(flow, EventFlow::Poll));
                }
                app.pending_events.pop_front()
            });
            if let Some(pending) = pending { event = pending; } else { break; }
        }
        let layout_pending = with_ios_app(|app| {
            app.event_callback = Some(callback);
            std::mem::take(&mut app.layout_pending)
        });
        if layout_pending { Self::surface_frame(false); }
    }
    
    // makepad-futures does not expose a task-ready hook on its mpsc executor.
    // Do not poll it at all unless a client asks for Cx::spawner().
    pub(super) fn update_executor_timer(&mut self, spawner_requested: bool) {
        unsafe {
            if self.surface_active && spawner_requested {
                if self.executor_timer != nil { return; }
                self.executor_timer = self.add_timer(0.250, sel!(executorTick:), YES);
            } else if self.executor_timer != nil {
                let () = msg_send![self.executor_timer, invalidate];
                self.executor_timer = nil;
            }
        }
    }

    unsafe fn add_timer(&self, interval: f64, selector: Sel, repeats: BOOL) -> ObjcId {
        let timer: ObjcId = msg_send![
            class!(NSTimer),
            timerWithTimeInterval: interval
            target: self.timer_delegate_instance
            selector: selector
            userInfo: nil
            repeats: repeats
        ];
        let run_loop: ObjcId = msg_send![class!(NSRunLoop), mainRunLoop];
        let () = msg_send![run_loop, addTimer: timer forMode: NSRunLoopCommonModes];
        timer
    }
    
    pub fn start_timer(&mut self, timer_id: u64, interval: f64, repeats: bool) {
        unsafe {
            let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
            let nstimer = self.add_timer(interval, sel!(receivedTimer:), if repeats { YES } else { NO });
            
            self.timers.push(IosTimer {
                timer_id: timer_id,
                nstimer: nstimer,
                repeats: repeats
            });
            let () = msg_send![pool, release];
        }
    }
    

    pub fn stop_timer(&mut self, timer_id: u64) {
        for i in 0..self.timers.len() {
            if self.timers[i].timer_id == timer_id {
                unsafe {
                    let () = msg_send![self.timers[i].nstimer, invalidate];
                }
                self.timers.remove(i);
                return;
            }
        }
    }
    
    
    pub fn send_timer_received(nstimer: ObjcId) {
        let len = with_ios_app(|app| app.timers.len());
        let time = with_ios_app(|app| app.time_now());
        for i in 0..len {
            if with_ios_app(|app| app.timers[i].nstimer == nstimer) {
                let timer_id = with_ios_app(|app| app.timers[i].timer_id);
                if !with_ios_app(|app| app.timers[i].repeats) {
                    with_ios_app(|app| app.timers.remove(i));
                }
                IosApp::do_callback(IosEvent::Timer(TimerEvent {timer_id: timer_id, time:Some(time)}));
                return
            }
        }
    }
    
    pub fn copy_to_clipboard(&self, content: &str) {
        unsafe {
            let nsstring = str_to_nsstring(content);
            let pasteboard: ObjcId = self.pasteboard;
            let _: () = msg_send![pasteboard, setString: nsstring];
        }
    }
}
