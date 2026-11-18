use crate::file_dialogs::FileDialog;
use crate::os::window_state::{PendingWindowState, WindowStateChange};
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

// CVDisplayLink supports the macOS 13 deployment target. Its callback runs on
// a CoreVideo thread and only posts an event; all UI work stays on AppKit.
type DisplayLinkRef = *mut c_void;
type DisplayLinkCallback = extern "C" fn(
    DisplayLinkRef, *const c_void, *const c_void, u64, *mut u64, *mut c_void,
) -> CVReturn;

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVDisplayLinkCreateWithCGDisplay(display_id: u32, link: *mut DisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkSetOutputCallback(link: DisplayLinkRef, callback: DisplayLinkCallback, data: *mut c_void) -> CVReturn;
    fn CVDisplayLinkStart(link: DisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkStop(link: DisplayLinkRef) -> CVReturn;
    fn CVDisplayLinkRelease(link: DisplayLinkRef);
}

const DISPLAY_FRAME_SUBTYPE: i16 = 0x5046;
static DISPLAY_FRAME_QUEUED: AtomicBool = AtomicBool::new(false);
const SURFACE_READY_SUBTYPE: i16 = 0x5053;
static SURFACE_READY_QUEUED: AtomicBool = AtomicBool::new(false);
static SURFACE_READY_EVENT: AtomicPtr<Object> = AtomicPtr::new(std::ptr::null_mut());
const SURFACE_WAKE_SUBTYPE: i16 = 0x5057;
static SURFACE_WAKE_QUEUED: AtomicBool = AtomicBool::new(false);

extern "C" fn display_link_tick(
    _: DisplayLinkRef, _: *const c_void, _: *const c_void, _: u64, _: *mut u64, _: *mut c_void,
) -> CVReturn {
    post_frame_event(DISPLAY_FRAME_SUBTYPE, &DISPLAY_FRAME_QUEUED);
    0
}

pub(in crate::os::apple) fn post_surface_ready() {
    // An immutable, application-defined event needs no new payload for each frame.
    let event = SURFACE_READY_EVENT.load(Ordering::Acquire);
    if event.is_null() || SURFACE_READY_QUEUED.swap(true, Ordering::AcqRel) { return; }
    unsafe { post_frame_wake(event); }
}

pub(super) fn post_surface_wake() {
    post_frame_event(SURFACE_WAKE_SUBTYPE, &SURFACE_WAKE_QUEUED);
}

fn post_frame_event(subtype: i16, queued: &AtomicBool) {
    if queued.swap(true, Ordering::AcqRel) { return; }
    unsafe {
        let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
        post_frame_wake(new_frame_event(subtype));
        let () = msg_send![pool, release];
    }
}

unsafe fn new_frame_event(subtype: i16) -> ObjcId {
    msg_send![
        class!(NSEvent),
        otherEventWithType: NSEventType::NSApplicationDefined
        location: NSPoint { x: 0., y: 0. }
        modifierFlags: 0u64
        timestamp: 0f64
        windowNumber: 0u64
        context: nil
        subtype: subtype
        data1: 0u64
        data2: 0u64
    ]
}

unsafe fn post_frame_wake(event: ObjcId) {
    let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
    let () = msg_send![ns_app, postEvent: event atStart: NO];
}

use {
    std::{
        rc::Rc,
        cell::{Cell, RefCell},
        time::Instant,
        collections::HashMap,
        os::raw::c_void,
    },
    crate::{
        makepad_live_id::*,
        makepad_math::DVec2,
        os::{
            apple::apple_sys::*,
            macos::{
                macos_delegates::*,
                macos_event::*,
                macos_window::MacosWindow,
            },
            apple_util::{
                nsstring_to_string,
                str_to_nsstring,
                keycode_to_menu_key,
                get_event_keycode,
                get_event_key_modifier
            },
            cx_native::EventFlow,
        },
        //macos_menu::{
        //    CxCommandSetting
        //},
        //turtle::{
        //    Rect
        //},
        event::{
            KeyCode,
            KeyEvent,
            TextInputEvent,
            TextClipboardEvent,
            TimerEvent,
            KeyModifiers,
        },
        cursor::MouseCursor,
        macos_menu::MacosMenu,
        window::WindowId,
    }
};

// this is unsafe, however we don't have much choice since the system calls into
// the objective C entrypoints we need to enter our eventloop
// So wherever we put this boundary, it will be unsafe

// this value will be fetched from multiple threads (post signal uses it)
pub static mut MACOS_CLASSES: *const MacosClasses = 0 as *const _;
// this value should not. Todo: guard this somehow proper

thread_local! {
    pub static MACOS_APP: RefCell<Option<MacosApp>> = RefCell::new(None);
}

pub fn with_macos_app<R>(f: impl FnOnce(&mut MacosApp) -> R) -> R {
    MACOS_APP.with_borrow_mut(|app| {
        f(app.as_mut().unwrap())
    })
}

pub fn init_macos_app_global(event_callback: Box<dyn FnMut(MacosEvent) -> EventFlow>) {
    unsafe {
        MACOS_CLASSES = Box::into_raw(Box::new(MacosClasses::new()));
    }
    MACOS_APP.with(|app| {
        *app.borrow_mut() = Some(MacosApp::new(event_callback));
    })
}

pub fn get_macos_class_global() -> &'static MacosClasses {
    unsafe {
        &*(MACOS_CLASSES)
    }
}

/// Paseo: the native title bars, menus and system panels follow the app's light/dark theme
/// rather than only the system setting (`Cx::set_window_dark_appearance`).
pub fn set_dark_appearance(dark: bool) {
    unsafe {
        let name = str_to_nsstring(if dark {"NSAppearanceNameDarkAqua"} else {"NSAppearanceNameAqua"});
        let appearance: ObjcId = msg_send![class!(NSAppearance), appearanceNamed: name];
        let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
        let () = msg_send![ns_app, setAppearance: appearance];
    }
}

#[derive(Clone)]
pub struct CocoaTimer {
    timer_id: u64,
    nstimer: ObjcId,
    repeats: bool
}

pub struct MacosClasses {
    pub window: *const Class,
    pub window_delegate: *const Class,
    pub menu_delegate: *const Class,
    pub app_delegate: *const Class,
    pub menu_target: *const Class,
    pub view: *const Class,
    pub timer_delegate: *const Class,
}

impl MacosClasses {
    pub fn new() -> Self {
        /*let const_attributes = vec![
            RcObjcId::from_unowned(NonNull::new(str_to_nsstring("NSMarkedClauseSegment")).unwrap()).forget(),
            RcObjcId::from_unowned(NonNull::new(str_to_nsstring("NSGlyphInfo")).unwrap()).forget(),
        ];*/
        Self {
            timer_delegate: define_macos_timer_delegate(),
            window: define_macos_window_class(),
            window_delegate: define_macos_window_delegate(),
            //post_delegate: define_cocoa_post_delegate(),
            menu_delegate: define_menu_delegate(),
            app_delegate: define_app_delegate(),
            menu_target: define_menu_target_class(),
            view: define_cocoa_view_class(),
        }
    }
}

pub struct MacosApp {
    menu_delegate_instance: ObjcId,
    //app_delegate_instance: ObjcId,
    pub time_start: Instant,
    pub timer_delegate_instance: ObjcId,
    timers: Vec<CocoaTimer>,
    //pub signals: Mutex<RefCell<HashSet<Signal>>>,
    pub cocoa_windows: Vec<(ObjcId, ObjcId)>,
    last_key_mod: KeyModifiers,
    #[allow(unused)]
    pasteboard: ObjcId,
    startup_focus_hack_ran: bool,
    event_callback: Option<Box<dyn FnMut(MacosEvent) -> EventFlow >>,
    pending_window_state: PendingWindowState,
    event_flow: EventFlow,
    animation_screen: Option<ObjcId>,
    display_link: DisplayLinkRef,
    fallback_frame_at: f64,
    
    pub cursors: HashMap<MouseCursor, ObjcId>,
    pub current_cursor: MouseCursor,
    //current_ns_event: Option<ObjcId>,
}

impl MacosApp {
    pub fn new(event_callback: Box<dyn FnMut(MacosEvent) -> EventFlow>) -> MacosApp {
        unsafe {
            let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
            let app_delegate_instance: ObjcId = msg_send![get_macos_class_global().app_delegate, new];
            
            let () = msg_send![ns_app, setDelegate: app_delegate_instance];
            let () = msg_send![ns_app, setActivationPolicy: NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular as i64];
            // Completion handlers can still post during window teardown.
            let ready: ObjcId = msg_send![new_frame_event(SURFACE_READY_SUBTYPE), retain];
            SURFACE_READY_EVENT.store(ready, Ordering::Release);
            let workspace: ObjcId = msg_send![class!(NSWorkspace), sharedWorkspace];
            let center: ObjcId = msg_send![workspace, notificationCenter];
            let () = msg_send![center, addObserver: app_delegate_instance
                selector: sel!(surfaceWorkspaceDidWake:)
                name: str_to_nsstring("NSWorkspaceDidWakeNotification") object: nil];
            
            
            // Construct the bits that are shared between windows
            MacosApp {
                startup_focus_hack_ran: false,
                pasteboard: msg_send![class!(NSPasteboard), generalPasteboard],
                time_start: Instant::now(),
                timer_delegate_instance: msg_send![get_macos_class_global().timer_delegate, new],
                menu_delegate_instance: msg_send![get_macos_class_global().menu_delegate, new],
                //app_delegate_instance,
                //signals: Mutex::new(RefCell::new(HashSet::new())),
                timers: Vec::new(),
                cocoa_windows: Vec::new(),
                event_flow: EventFlow::Poll,
                animation_screen: None,
                display_link: std::ptr::null_mut(),
                fallback_frame_at: 0.0,
                last_key_mod: KeyModifiers {..Default::default()},
                event_callback: Some(event_callback),
                pending_window_state: PendingWindowState::default(),
                cursors: HashMap::new(),
                current_cursor: MouseCursor::Default,
                //current_ns_event: None,
            }
        }
    }

    pub(crate) fn set_animation_screen(&mut self, screen: Option<ObjcId>) {
        if self.animation_screen == screen {
            return;
        }
        unsafe {
            if !self.display_link.is_null() {
                CVDisplayLinkStop(self.display_link);
                CVDisplayLinkRelease(self.display_link);
                self.display_link = std::ptr::null_mut();
            }
            // Retain cached identity across display changes; never dereference a
            // stale NSScreen or allocate its deviceDescription on every frame.
            if let Some(previous) = self.animation_screen.filter(|screen| *screen != nil) {
                let () = msg_send![previous, release];
            }
            self.animation_screen = screen;
            let Some(screen) = screen else { return };
            let mut display = CGMainDisplayID();
            if screen != nil {
                let _: ObjcId = msg_send![screen, retain];
                let description: ObjcId = msg_send![screen, deviceDescription];
                let key = str_to_nsstring("NSScreenNumber");
                let number: ObjcId = msg_send![description, objectForKey: key];
                if number != nil {
                    display = msg_send![number, unsignedIntValue];
                }
            }
            let mut link = std::ptr::null_mut();
            if CVDisplayLinkCreateWithCGDisplay(display, &mut link) != 0
                || CVDisplayLinkSetOutputCallback(link, display_link_tick, std::ptr::null_mut()) != 0
                || CVDisplayLinkStart(link) != 0
            {
                if !link.is_null() {
                    CVDisplayLinkRelease(link);
                }
                self.fallback_frame_at = self.time_now();
                crate::warning!("Could not start CVDisplayLink for display {display}; using paced timer");
                return;
            }
            self.display_link = link;
        }
    }

    fn needs_fallback_frame(&mut self, now: f64) -> bool {
        if self.animation_screen.is_none() || !self.display_link.is_null() || now < self.fallback_frame_at {
            return false;
        }
        self.fallback_frame_at = now + 1.0 / 60.0;
        true
    }
    pub fn init_quit_menu(&mut self){
        self.update_macos_menu(
            &MacosMenu::Main{items:vec![MacosMenu::Sub{
                name:"Makepad".to_string(),
                items:vec![MacosMenu::Item{
                    command:live_id!(quit),
                    key:KeyCode::KeyQ,
                    shift: false,
                    enabled: true,
                    name:"Quit Example".to_string()
                }]
            }]}
        );
    }
    
    // Determines whether to show your application in the dock when it runs. The default value is true. 
    pub fn show_in_dock(&mut self, show: bool)  {
        unsafe{
            let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
            if show {
                let () = msg_send![ns_app, setActivationPolicy: NSApplicationActivationPolicy::NSApplicationActivationPolicyRegular as i64];
            } else {
                let () = msg_send![ns_app, setActivationPolicy: NSApplicationActivationPolicy::NSApplicationActivationPolicyAccessory as i64]; 
            }
        }
    }
    pub fn update_macos_menu(&mut self, menu: &MacosMenu) {
        unsafe fn make_menu(
            parent_menu: ObjcId,
            delegate: ObjcId,
            menu_target_class: *const Class,
            menu: &MacosMenu,
        ) {
            
            match menu {
                MacosMenu::Main {items} => {
                    let main_menu: ObjcId = msg_send![class!(NSMenu), new];
                    let () = msg_send![main_menu, setTitle: str_to_nsstring("MainMenu")];
                    let () = msg_send![main_menu, setAutoenablesItems: NO];
                    let () = msg_send![main_menu, setDelegate: delegate];
                    
                    for item in items {
                        make_menu(main_menu, delegate, menu_target_class, item);
                    }
                    let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
                    let () = msg_send![
                        ns_app,
                        setMainMenu: main_menu
                    ];
                },
                MacosMenu::Sub {name, items} => {
                    let sub_menu: ObjcId = msg_send![class!(NSMenu), new];
                    let () = msg_send![sub_menu, setTitle: str_to_nsstring(name)];
                    let () = msg_send![sub_menu, setAutoenablesItems: NO];
                    let () = msg_send![sub_menu, setDelegate: delegate];
                    // append item to parebt
                    let sub_item: ObjcId = msg_send![
                        parent_menu,
                        addItemWithTitle: str_to_nsstring(name)
                        action: nil
                        keyEquivalent: str_to_nsstring("")
                    ];
                    // connect submenu
                    let () = msg_send![parent_menu, setSubmenu: sub_menu forItem: sub_item];
                    for item in items {
                        make_menu(sub_menu, delegate, menu_target_class, item);
                    }
                },
                MacosMenu::Item {name, command, shift, key, enabled} => {
                    
                    let sub_item: ObjcId = msg_send![
                        parent_menu,
                        addItemWithTitle: str_to_nsstring(name)
                        action: sel!(menuAction:)
                        keyEquivalent: str_to_nsstring(keycode_to_menu_key(*key, *shift))
                    ];
                    let target: ObjcId = msg_send![menu_target_class, new];
                    let () = msg_send![sub_item, setTarget: target];
                    let () = msg_send![sub_item, setEnabled: if *enabled {YES}else {NO}];
                    /*
                    let command_usize = if let Ok(mut status_map) = status_map.lock() {
                        if let Some(id) = status_map.command_to_usize.get(&command) {
                            *id
                        }
                        else {
                            let id = status_map.status_to_usize.len();
                            status_map.command_to_usize.insert(*command, id);
                            status_map.usize_to_command.insert(id, *command);
                            id
                        }
                    }
                    else {
                        panic!("cannot lock cmd_map");
                    };*/
                    
                    //(*target).set_ivar("macos_app_ptr", GLOBAL_COCOA_APP as *mut _ as *mut c_void);
                    (*target).set_ivar("command_u64", command.0);
                },
                MacosMenu::Line => {
                    let sep_item: ObjcId = msg_send![class!(NSMenuItem), separatorItem];
                    let () = msg_send![
                        parent_menu,
                        addItem: sep_item
                    ];
                }
            }
        }
        unsafe {
            make_menu(nil, self.menu_delegate_instance, get_macos_class_global().menu_target, menu);
        }
    }
    /*
    pub fn startup_focus_hack(&mut self) {
        
        unsafe {
            self.startup_focus_hack_ran = true;
            if !self.startup_focus_hack_ran {
                self.startup_focus_hack_ran = true;
                
                //let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
                //let active: bool = msg_send![ns_app, isActive];
                //if !active {
                let dock_bundle_id = str_to_nsstring("com.apple.dock");
                let dock_array: ObjcId = msg_send![
                    class!(NSRunningApplication),
                    runningApplicationsWithBundleIdentifier: dock_bundle_id
                ];
                let my_app: ObjcId = msg_send![
                    class!(NSRunningApplication),
                    runningApplicationWithProcessIdentifier: std::process::id()
                ];
                let dock_array_len: u64 = msg_send![dock_array, count];
                if dock_array_len == 0 {
                    panic!("Dock not running"); 
                } else {
                    let dock: ObjcId = msg_send![dock_array, objectAtIndex: 0];
                        let _status: BOOL = msg_send![
                            dock, 
                            activateWithOptions: NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps
                        ];
                        //let ns_running_app: ObjcId = msg_send![class!(NSRunningApplication), currentApplication];
                    
                    let () = msg_send![
                        my_app,
                        activateWithOptions: NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps
                    ];
                    let () = msg_send![self.cocoa_windows[0].0, makeKeyAndOrderFront: nil];
                }
                //}
            }
        }
    }*/
    pub fn startup_focus_hack(&mut self) {
        return
        unsafe {
            if !self.startup_focus_hack_ran {
                self.startup_focus_hack_ran = true;
                let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
                let active: bool = msg_send![ns_app, isActive];
                if !active {
                    let dock_bundle_id: ObjcId = str_to_nsstring("com.apple.dock");
                    let dock_array: ObjcId = msg_send![
                        class!(NSRunningApplication),
                        runningApplicationsWithBundleIdentifier: dock_bundle_id
                    ];
                    let dock_array_len: u64 = msg_send![dock_array, count];
                    if dock_array_len == 0 {
                        panic!("Dock not running");
                    } else {
                        let dock: ObjcId = msg_send![dock_array, objectAtIndex: 0];
                        let _status: BOOL = msg_send![
                            dock,
                            activateWithOptions: NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps
                        ];
                        let ns_running_app: ObjcId = msg_send![class!(NSRunningApplication), currentApplication];
                        let () = msg_send![
                            ns_running_app,
                            activateWithOptions: NSApplicationActivationOptions::NSApplicationActivateIgnoringOtherApps
                        ];
                    }
                }
            }
        }
    }
    
    pub fn time_now(&self) -> f64 {
        let time_now = Instant::now(); //unsafe {mach_absolute_time()};
        (time_now.duration_since(self.time_start)).as_secs_f64() 
    }
    
    unsafe fn event_window_input(ns_event: ObjcId) -> Option<(WindowId, Rc<Cell<bool>>)> {
        let window: ObjcId = msg_send![ns_event, window];
        if window == nil {
            return None;
        }
        let delegate: ObjcId = msg_send![window, delegate];
        if delegate == nil {
            return None;
        }
        let is_macos_delegate: BOOL = msg_send![delegate, isKindOfClass: get_macos_class_global().window_delegate];
        if is_macos_delegate != YES {
            return None;
        }
        let ptr: *mut c_void = *(*delegate).get_ivar("macos_window_ptr");
        if ptr.is_null() {
            return None;
        }
        let window = &*(ptr as *const MacosWindow);
        Some((window.window_id, window.ime_key_consumed.clone()))
    }

    unsafe fn process_ns_event(ns_event: ObjcId) {
        let ev_type: NSEventType = msg_send![ns_event, type];
        if ev_type == NSEventType::NSApplicationDefined {
            let subtype: i16 = msg_send![ns_event, subtype];
            if subtype == DISPLAY_FRAME_SUBTYPE {
                DISPLAY_FRAME_QUEUED.store(false, Ordering::Release);
                if with_macos_app(|app| !app.display_link.is_null()) {
                    Self::do_callback(MacosEvent::DisplayFrame);
                }
                return;
            }
            if subtype == SURFACE_READY_SUBTYPE {
                SURFACE_READY_QUEUED.store(false, Ordering::Release);
                Self::do_callback(MacosEvent::SurfaceReady);
                return;
            }
            if subtype == SURFACE_WAKE_SUBTYPE {
                SURFACE_WAKE_QUEUED.store(false, Ordering::Release);
                Self::do_callback(MacosEvent::SurfaceWake);
                return;
            }
        }
        // sendEvent can invoke responder/IME callbacks or change/close the key window.
        // Preserve the event's own source, never a later key window or widget focus.
        let source = match ev_type {
            NSEventType::NSKeyDown | NSEventType::NSKeyUp | NSEventType::NSFlagsChanged => {
                Self::event_window_input(ns_event)
            }
            _ => None,
        };
        let window_id = source.as_ref().map(|(window, _)| *window);
        if let Some((_, consumed)) = &source { consumed.set(false); }
        // Let the native menu consume a shortcut once, before the text input
        // context or the app's raw-key handlers can interpret the same key.
        if ev_type == NSEventType::NSKeyDown && source.is_some() && super::key_equivalent::perform(ns_event) {
            return;
        }
        
        let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
        let () = msg_send![ns_app, sendEvent: ns_event];
        
        if ev_type as u64 == 21 { // some missing event from cocoa-rs crate
            return;
        }
        
        match ev_type {
            NSEventType::NSApplicationDefined => { // event loop unblocker
            },
            NSEventType::NSKeyUp => {
                let Some(window_id) = window_id else { return };
                if let Some(key_code) = get_event_keycode(ns_event) {
                    let modifiers = get_event_key_modifier(ns_event);
                    //let key_char = get_event_char(ns_event);
                    let is_repeat: bool = msg_send![ns_event, isARepeat];
                    let time = with_macos_app(|app| app.time_now());
                    MacosApp::do_callback(
                        MacosEvent::KeyUp(window_id, KeyEvent {
                            key_code: key_code,
                            //key_char: key_char,
                            is_repeat: is_repeat,
                            modifiers: modifiers,
                            time
                        })
                    );
                }
            },
            NSEventType::NSKeyDown => {
                if source.as_ref().is_some_and(|(_, consumed)| consumed.get()) { return; }
                let Some(window_id) = window_id else { return };
                if let Some(key_code) = get_event_keycode(ns_event) {
                    let modifiers = get_event_key_modifier(ns_event);
                    //let key_char = get_event_char(ns_event);
                    let is_repeat: bool = msg_send![ns_event, isARepeat];
                    //let is_return = if let KeyCode::Return = key_code{true} else{false};
                    
                    
                    #[cfg(target_os = "macos")]
                    match key_code {
                        KeyCode::KeyV => if modifiers.logo || modifiers.control {
                            let request = crate::event::TextPasteShortcutEvent::new(modifiers);
                            MacosApp::do_callback(MacosEvent::TextPasteShortcut(window_id, request.clone()));
                            if request.allowed.get() {
                                let pasteboard: ObjcId = with_macos_app(|app| app.pasteboard);
                                let nsstring: ObjcId = msg_send![pasteboard, stringForType: NSStringPboardType];
                                if nsstring != std::ptr::null_mut() {
                                    let string = nsstring_to_string(nsstring);
                                    MacosApp::do_callback(
                                        MacosEvent::TextInput(window_id, TextInputEvent {
                                            input: string,
                                            was_paste: true,
                                            replace_last: false
                                        })
                                    );
                                }
                            }
                        },
                        KeyCode::KeyC => if modifiers.logo || modifiers.control {
                            let pasteboard: ObjcId = with_macos_app(|app| app.pasteboard);
                            let response = Rc::new(RefCell::new(None));
                            MacosApp::do_callback(
                                MacosEvent::TextCopy(window_id, TextClipboardEvent {
                                    response: response.clone()
                                })
                            );
                            let response = response.borrow();
                            if let Some(response) = response.as_ref() {
                                let nsstring = str_to_nsstring(&response);
                                let array: ObjcId = msg_send![class!(NSArray), arrayWithObject: NSStringPboardType];
                                let () = msg_send![pasteboard, declareTypes: array owner: nil];
                                let () = msg_send![pasteboard, setString: nsstring forType: NSStringPboardType];
                            }
                        },
                        KeyCode::KeyX => if modifiers.logo || modifiers.control {
                            let pasteboard: ObjcId = with_macos_app(|app| app.pasteboard);
                            let response = Rc::new(RefCell::new(None));
                            MacosApp::do_callback(
                                MacosEvent::TextCut(window_id, TextClipboardEvent {
                                    response: response.clone()
                                })
                            );
                            let response = response.borrow();
                            if let Some(response) = response.as_ref() {
                                let nsstring = str_to_nsstring(&response);
                                let array: ObjcId = msg_send![class!(NSArray), arrayWithObject: NSStringPboardType];
                                let () = msg_send![pasteboard, declareTypes: array owner: nil];
                                let () = msg_send![pasteboard, setString: nsstring forType: NSStringPboardType];
                            }
                        },
                        _ => {}
                    }
                    let time = with_macos_app(|app: &mut MacosApp| app.time_now());
                    MacosApp::do_callback(
                        MacosEvent::KeyDown(window_id, KeyEvent {
                            key_code: key_code,
                            is_repeat: is_repeat,
                            modifiers: modifiers,
                            time
                        })
                    );
                    /*
                    if is_return{
                        self.do_callback(&mut vec![
                            Event::TextInput(TextInputEvent{
                                input:"\n".to_string(),
                                was_paste:false,
                                replace_last:false
                            })
                        ]);
                    }*/
                    
                    
                    
                }
            },
            NSEventType::NSFlagsChanged => {
                let modifiers = get_event_key_modifier(ns_event);
                let last_key_mod = with_macos_app(|app| app.last_key_mod.clone());
                with_macos_app(|app| app.last_key_mod = modifiers.clone());
                let Some(window_id) = window_id else { return };
                let mut events = Vec::new();
                fn add_event(window_id: WindowId, time: f64, old: bool, new: bool, modifiers: KeyModifiers, events: &mut Vec<MacosEvent>, key_code: KeyCode) {
                    if old != new {
                        let event = KeyEvent {
                            key_code: key_code,
                            //key_char: '\0',
                            is_repeat: false,
                            modifiers: modifiers,
                            time: time
                        };
                        if new {
                            events.push(MacosEvent::KeyDown(window_id, event));
                        }
                        else {
                            events.push(MacosEvent::KeyUp(window_id, event));
                        }
                    }
                }
                let time = with_macos_app(|app| app.time_now());
                add_event(window_id, time, last_key_mod.shift, modifiers.shift, modifiers.clone(), &mut events, KeyCode::Shift);
                add_event(window_id, time, last_key_mod.alt, modifiers.alt, modifiers.clone(), &mut events, KeyCode::Alt);
                add_event(window_id, time, last_key_mod.logo, modifiers.logo, modifiers.clone(), &mut events, KeyCode::Logo);
                add_event(window_id, time, last_key_mod.control, modifiers.control, modifiers.clone(), &mut events, KeyCode::Control);
                if events.len() >0 {
                    for event in events {
                        MacosApp::do_callback(event);
                    }
                }
            },
            NSEventType::NSMouseEntered => {},
            NSEventType::NSMouseExited => {},
            NSEventType::NSScrollWheel => {
                let window: ObjcId = msg_send![ns_event, window];
                if window == nil {
                    return
                }
                let window_delegate: ObjcId = msg_send![window, delegate];
                if window_delegate == nil {
                    return
                }
                let ptr: *mut c_void = *(*window_delegate).get_ivar("macos_window_ptr");
                let cocoa_window = &mut *(ptr as *mut MacosWindow);
                let dx: f64 = msg_send![ns_event, scrollingDeltaX];
                let dy: f64 = msg_send![ns_event, scrollingDeltaY];
                let has_prec: BOOL = msg_send![ns_event, hasPreciseScrollingDeltas];
                return if has_prec == YES {
                    cocoa_window.send_scroll(DVec2 {x: -dx, y: -dy}, get_event_key_modifier(ns_event), false);
                } else {
                    cocoa_window.send_scroll(DVec2 {x: -dx * 32., y: -dy * 32.}, get_event_key_modifier(ns_event), true);
                }
            },
            NSEventType::NSEventTypePressure => {
            },
            _ => (),
        }
    }
    
    pub fn event_loop() {
        unsafe {
            let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
            //let () = msg_send![ns_app, activateIgnoringOtherApps:YES];
            let () = msg_send![ns_app, finishLaunching];
           // get_macos_app_global().init_quit_menu();
           // get_macos_app_global().startup_focus_hack();
           
            loop {
                let event_flow = with_macos_app(|app| app.event_flow);
                match event_flow {
                    EventFlow::Exit => {
                        break;
                    }
                    EventFlow::Poll | EventFlow::Wait => {
                        let event_wait = if let EventFlow::Wait = with_macos_app(|app| app.event_flow) {true}else {false};
                        let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
                        let ns_until: ObjcId = if event_wait {
                            msg_send![class!(NSDate), distantFuture]
                        }else {
                            msg_send![class!(NSDate), distantPast]
                        };
                        
                        let ns_event: ObjcId = msg_send![
                            ns_app,
                            nextEventMatchingMask: NSEventMask::NSAnyEventMask as u64 | NSEventMask::NSEventMaskPressure as u64
                            untilDate: ns_until
                            inMode: NSDefaultRunLoopMode
                            dequeue: YES
                        ];
                        //self.current_ns_event = Some(ns_event);
                        if ns_event != nil {
                            MacosApp::process_ns_event(ns_event);
                        }

                        //let event_wait = if let EventFlow::Wait = with_macos_app(|app| app.event_flow) {true}else {false};
                        
                        //if ns_event == nil || event_wait {
                        //if event_wait{
                        //    with_macos_app(|app| app.event_flow = EventFlow::Wait);
                        //}
                        //self.current_ns_event = None;
                        
                        let () = msg_send![pool, release];
                    }
                }
            }
        }
    }
    
    pub fn do_callback(mut event: MacosEvent) {
        let cb = with_macos_app(|app| app.event_callback.take());
        if let Some(mut callback) = cb {
            loop {
                let event_flow = callback(event);
                with_macos_app(|app| app.event_flow = event_flow);
                if let EventFlow::Exit = event_flow {
                    unsafe {
                        let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
                        let () = msg_send![ns_app, terminate: nil];
                    }
                    break;
                }
                let Some(pending) = with_macos_app(|app| app.pending_window_state.pop_front()) else {
                    break;
                };
                event = match pending {
                    WindowStateChange::Focus(window_id, true) => MacosEvent::AppGotFocus(window_id),
                    WindowStateChange::Focus(window_id, false) => MacosEvent::AppLostFocus(window_id),
                    WindowStateChange::Geometry(change) => MacosEvent::WindowGeomChange(change),
                };
            }
            with_macos_app(|app| app.event_callback = Some(callback));
        } else {
            // AppKit setFrame/activation can synchronously notify during a Cx callback.
            // Drag, clipboard and window queries still require synchronous answers.
            with_macos_app(|app| match event {
                MacosEvent::AppGotFocus(window_id) => app.pending_window_state.focus(window_id, true),
                MacosEvent::AppLostFocus(window_id) => app.pending_window_state.focus(window_id, false),
                MacosEvent::WindowGeomChange(change) => app.pending_window_state.geometry(change),
                _ => (),
            });
        }
    }
    /*
    pub fn post_signal(signal: Signal) {
        unsafe {
            let cocoa_app = get_macos_app_global();
            if let Ok(signals) = cocoa_app.signals.lock(){
                let mut signals = signals.borrow_mut();
                // if empty, we do shit. otherwise we add
                if signals.is_empty(){
                    signals.insert(signal);
                    let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
                    //let cocoa_app = get_macos_app_global();
                    let post_delegate_instance: ObjcId = msg_send![get_macos_class_global().post_delegate, new];
                    //(*post_delegate_instance).set_ivar("macos_app_ptr", GLOBAL_COCOA_APP as *mut _ as *mut c_void);
                    let nstimer: ObjcId = msg_send![
                        class!(NSTimer),
                        timerWithTimeInterval: 0.
                        target: post_delegate_instance
                        selector: sel!(receivedPost:)
                        userInfo: nil
                        repeats: false
                    ];
                    let nsrunloop: ObjcId = msg_send![class!(NSRunLoop), mainRunLoop];
                    let () = msg_send![nsrunloop, addTimer: nstimer forMode: NSRunLoopCommonModes];
                    
                    let () = msg_send![pool, release];
                }
                else{
                    signals.insert(signal);
                }
            }
            
        }
    }*/
    
    pub fn set_mouse_cursor(&mut self, cursor: MouseCursor) {
        if self.current_cursor != cursor {
            self.current_cursor = cursor;
            // todo set it on all windows
            unsafe {
                for (window, view) in &self.cocoa_windows {
                    let _: () = msg_send![
                        *window,
                        invalidateCursorRectsForView: *view
                    ];
                }
            }
        }
    }
    
    pub fn start_timer(&mut self, timer_id: u64, interval: f64, repeats: bool) {
        unsafe {
            let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
            
            let nstimer: ObjcId = msg_send![
                class!(NSTimer),
                timerWithTimeInterval: interval
                target: self.timer_delegate_instance
                selector: sel!(receivedTimer:)
                userInfo: nil
                repeats: repeats
            ];
            let nsrunloop: ObjcId = msg_send![class!(NSRunLoop), mainRunLoop];
            let () = msg_send![nsrunloop, addTimer: nstimer forMode: NSRunLoopCommonModes];
            
            self.timers.push(CocoaTimer {
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
        
        let len = with_macos_app(|app| app.timers.len());
        for i in 0..len {
            let time =with_macos_app(|app| app.time_now());
            if with_macos_app(|app| app.timers[i].nstimer == nstimer) {
                let timer_id = with_macos_app(|app| app.timers[i].timer_id);
                if !with_macos_app(|app| app.timers[i].repeats) {
                    with_macos_app(|app| app.timers.remove(i));
                }
                
                MacosApp::do_callback(MacosEvent::Timer(TimerEvent {time:Some(time), timer_id: timer_id}));
                if timer_id == 0 && with_macos_app(|app| app.needs_fallback_frame(time)) {
                    MacosApp::do_callback(MacosEvent::DisplayFrame);
                }
                // break the eventloop if its in blocked mode
                unsafe {
                    let pool: ObjcId = msg_send![class!(NSAutoreleasePool), new];
                    let nsevent: ObjcId = msg_send![
                        class!(NSEvent),
                        otherEventWithType: NSEventType::NSApplicationDefined
                        location: NSPoint {x: 0., y: 0.}
                        modifierFlags: 0u64
                        timestamp: 0f64
                        windowNumber: 1u64
                        context: nil
                        subtype: 0i16
                        data1: 0u64
                        data2: 0u64
                    ];
                    let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
                    let () = msg_send![ns_app, postEvent: nsevent atStart: 0];
                    let () = msg_send![pool, release];
                }
                return;
            }
        }
    }
    /*
    pub fn send_signal_event(&mut self) {
        let signals = if let Ok(signals) = self.signals.lock(){
            let mut new_signals = HashSet::new();
            std::mem::swap(&mut *signals.borrow_mut(), &mut new_signals);
            new_signals
        }else{panic!()};
        
        self.do_callback(vec![
            MacosEvent::Signal(SignalEvent {
                signals,
            })
        ]);
        self.do_callback(vec![MacosEvent::Paint]);
    }*/
    
    pub fn send_command_event(command: LiveId) {
        MacosApp::do_callback(
            MacosEvent::MacosMenuCommand(command)
        );
        MacosApp::do_callback(MacosEvent::Paint);
    }
    
    pub fn send_paint_event() {
        MacosApp::do_callback(MacosEvent::Paint);
    }
    /*
    #[cfg(target_os = "macos")]
    pub fn start_dragging(&mut self, items: Vec<DragItem>) {
       unsafe {
            let ns_app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
            let ns_event: ObjcId = msg_send![ns_app, currentEvent];
            
            let window: ObjcId = msg_send![ns_event, window];
            let window_delegate: ObjcId = msg_send![window, delegate];
            if window == nil {
                crate::error!("start_dragging: Cocoa window nil on event");
                return
            }
            let cocoa_window: *mut c_void = *(*window_delegate).get_ivar("macos_window_ptr");
            let cocoa_window = &mut *(cocoa_window as *mut MacosWindow);
            cocoa_window.start_dragging(ns_event, items);
        };
    }*/

    pub fn copy_to_clipboard(&mut self, content: &str) {
        unsafe {
            let pasteboard: ObjcId = self.pasteboard;
            let nsstring = str_to_nsstring(content);
            let array: ObjcId = msg_send![class!(NSArray), arrayWithObject: NSStringPboardType];
            let () = msg_send![pasteboard, declareTypes: array owner: nil];
            let () = msg_send![pasteboard, setString: nsstring forType: NSStringPboardType];
        }
    }

    pub fn open_save_file_dialog(&mut self, _settings: FileDialog)
    {
        println!("open save file dialog!");
    }

    pub fn open_select_file_dialog(&mut self, _settings: FileDialog )
    {
        println!("open select file dialog!");
    }

    pub fn open_save_folder_dialog(&mut self,  _settings: FileDialog)
    {
        println!("open save folder dialog!");
    }

    pub fn open_select_folder_dialog(&mut self, _settings: FileDialog)
    {
        println!("open select folder dialog!");
    }

}
