//! WebKitGTK 4.1 (GTK 3), loaded at run time: the app starts without it, and
//! [`supported`] is `false` where it is missing.
//!
//! Makepad draws into its own X11 top-level, so the view lives in a GTK popup window
//! reparented into a child X window of that top-level. The child window is the visible part
//! of the view (the card inside the timeline's viewport) and clips it; the popup sits at the
//! card's offset inside it. GDK is restricted to X11: on a Wayland session GTK would
//! otherwise pick Wayland, and a Wayland surface cannot live inside Makepad's window.
//!
//! GTK runs on the UI thread, pumped by [`WebView::pump`]. X keyboard focus goes to the popup
//! when the view is pressed and back to the top-level on [`WebView::blur`]: a reparented
//! window gets no focus from the window manager.

use super::{WebViewEvent, WebViewOptions, bootstrap, parse_message, wrap_html};
use libloading::Library;
use makepad_widgets::makepad_platform::os::linux::x11::xlib_app::get_xlib_app_global;
use makepad_widgets::{Cx, Rect, WindowId};
use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_ulong, c_void};
use std::rc::Rc;
use std::sync::LazyLock;

type Ptr = *mut c_void;

const GTK_WINDOW_POPUP: c_int = 1;
const WEBKIT_USER_CONTENT_INJECT_TOP_FRAME: c_int = 1;
const WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_END: c_int = 1;
const WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION: c_int = 0;
const WEBKIT_POLICY_DECISION_TYPE_RESPONSE: c_int = 2;
const REVERT_TO_PARENT: c_int = 2;
const CURRENT_TIME: c_ulong = 0;

#[repr(C)]
struct GdkRgba {
    red: f64,
    green: f64,
    blue: f64,
    alpha: f64,
}

macro_rules! api {
    ($($library:ident { $($name:ident: fn($($arg:ty),*) $(-> $ret:ty)?;)* })*) => {
        // Fields keep the C symbol names.
        #[allow(non_snake_case)]
        struct Gtk {
            _libraries: Vec<Library>,
            $($($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*)*
        }

        impl Gtk {
            fn load() -> Result<Self, String> {
                let open = |name: &str| unsafe { Library::new(name) }.map_err(|e| format!("{name}: {e}"));
                $(let $library = open(api!(@soname $library))?;)*
                Ok(Self {
                    $($($name: unsafe {
                        *$library
                            .get::<unsafe extern "C" fn($($arg),*) $(-> $ret)?>(concat!(stringify!($name), "\0").as_bytes())
                            .map_err(|e| format!("{}: {e}", stringify!($name)))?
                    },)*)*
                    _libraries: vec![$($library),*],
                })
            }
        }
    };
    (@soname glib) => { "libglib-2.0.so.0" };
    (@soname gobject) => { "libgobject-2.0.so.0" };
    (@soname gdk) => { "libgdk-3.so.0" };
    (@soname gtk) => { "libgtk-3.so.0" };
    (@soname x11) => { "libX11.so.6" };
    (@soname jsc) => { "libjavascriptcoregtk-4.1.so.0" };
    (@soname webkit) => { "libwebkit2gtk-4.1.so.0" };
}

api! {
    glib {
        g_main_context_iteration: fn(Ptr, c_int) -> c_int;
        g_free: fn(Ptr);
    }
    gobject {
        g_signal_connect_data: fn(Ptr, *const c_char, *const c_void, Ptr, *const c_void, c_uint) -> c_ulong;
        g_signal_handler_disconnect: fn(Ptr, c_ulong);
        g_object_unref: fn(Ptr);
    }
    gdk {
        gdk_set_allowed_backends: fn(*const c_char);
        gdk_display_get_default: fn() -> Ptr;
        gdk_x11_display_get_xdisplay: fn(Ptr) -> Ptr;
        gdk_x11_window_get_xid: fn(Ptr) -> c_ulong;
        gdk_x11_display_error_trap_push: fn(Ptr);
        gdk_x11_display_error_trap_pop_ignored: fn(Ptr);
    }
    gtk {
        gtk_init_check: fn(*mut c_int, *mut *mut *mut c_char) -> c_int;
        gtk_window_new: fn(c_int) -> Ptr;
        gtk_window_move: fn(Ptr, c_int, c_int);
        gtk_window_resize: fn(Ptr, c_int, c_int);
        gtk_container_add: fn(Ptr, Ptr);
        gtk_widget_realize: fn(Ptr);
        gtk_widget_show_all: fn(Ptr);
        gtk_widget_destroy: fn(Ptr);
        gtk_widget_get_window: fn(Ptr) -> Ptr;
        gtk_widget_queue_draw: fn(Ptr);
    }
    x11 {
        XCreateSimpleWindow: fn(Ptr, c_ulong, c_int, c_int, c_uint, c_uint, c_uint, c_ulong, c_ulong) -> c_ulong;
        XDestroyWindow: fn(Ptr, c_ulong) -> c_int;
        XReparentWindow: fn(Ptr, c_ulong, c_ulong, c_int, c_int) -> c_int;
        XMoveResizeWindow: fn(Ptr, c_ulong, c_int, c_int, c_uint, c_uint) -> c_int;
        XMapWindow: fn(Ptr, c_ulong) -> c_int;
        XUnmapWindow: fn(Ptr, c_ulong) -> c_int;
        XSetInputFocus: fn(Ptr, c_ulong, c_int, c_ulong) -> c_int;
        XFlush: fn(Ptr) -> c_int;
    }
    jsc {
        jsc_value_to_string: fn(Ptr) -> *mut c_char;
    }
    webkit {
        webkit_user_content_manager_new: fn() -> Ptr;
        webkit_user_content_manager_register_script_message_handler: fn(Ptr, *const c_char) -> c_int;
        webkit_user_content_manager_add_script: fn(Ptr, Ptr);
        webkit_user_script_new: fn(*const c_char, c_int, c_int, *const *const c_char, *const *const c_char) -> Ptr;
        webkit_user_script_unref: fn(Ptr);
        webkit_web_view_new_with_user_content_manager: fn(Ptr) -> Ptr;
        webkit_web_view_get_settings: fn(Ptr) -> Ptr;
        webkit_settings_set_enable_javascript_markup: fn(Ptr, c_int);
        webkit_web_view_set_background_color: fn(Ptr, *const GdkRgba);
        webkit_web_view_set_zoom_level: fn(Ptr, f64);
        webkit_web_view_load_html: fn(Ptr, *const c_char, *const c_char);
        webkit_web_view_run_javascript: fn(Ptr, *const c_char, Ptr, Ptr, Ptr);
        webkit_javascript_result_get_js_value: fn(Ptr) -> Ptr;
        webkit_policy_decision_ignore: fn(Ptr);
    }
}

/// GTK, loaded and initialized on the UI thread on first use.
struct Loaded(Result<Gtk, String>);
// The function pointers are callable from any thread; GTK itself is only used on the UI thread.
unsafe impl Send for Loaded {}
unsafe impl Sync for Loaded {}

static GTK: LazyLock<Loaded> = LazyLock::new(|| {
    Loaded(Gtk::load().and_then(|gtk| {
        unsafe { (gtk.gdk_set_allowed_backends)(c"x11".as_ptr()) };
        if unsafe { (gtk.gtk_init_check)(std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
            return Err("GTK could not open the X11 display".into());
        }
        Ok(gtk)
    }))
});

fn gtk() -> Result<&'static Gtk, String> {
    GTK.0.as_ref().map_err(Clone::clone)
}

pub(super) fn supported() -> bool {
    // One message per process: the result never changes.
    static REPORTED: std::sync::Once = std::sync::Once::new();
    let result = gtk();
    if let Err(error) = &result {
        REPORTED.call_once(|| makepad_widgets::log!("Web views unavailable: {error}"));
    }
    result.is_ok()
}

/// What the GTK callbacks share with the view.
#[derive(Default)]
struct Shared {
    events: Vec<WebViewEvent>,
    /// The initial page load has been allowed; every later navigation is refused.
    navigated: bool,
    /// The popup holds X keyboard focus.
    focused: bool,
    display: Ptr,
    xdisplay: Ptr,
    popup: c_ulong,
}

pub(super) struct WebView {
    gtk: &'static Gtk,
    popup: Ptr,
    view: Ptr,
    content: Ptr,
    clip: c_ulong,
    top: c_ulong,
    shared: Rc<RefCell<Shared>>,
    /// `Rc::into_raw(shared)` handed to the signal handlers, released on drop.
    user_data: *const RefCell<Shared>,
    handlers: Vec<(Ptr, c_ulong)>,
    /// The device-pixel clip and view rects last applied, `None` while hidden.
    placed: Option<([i32; 4], [i32; 4])>,
    shown: bool,
    zoom: f64,
}

impl WebView {
    pub(super) fn new(
        _cx: &mut Cx,
        window: WindowId,
        options: WebViewOptions,
    ) -> Result<Self, String> {
        let gtk = gtk()?;
        let top = top_level(window).ok_or("The window has no X11 window")?;
        let script = CString::new(bootstrap(
            &options,
            "(message) => window.webkit.messageHandlers.paseo.postMessage(message)",
        ))
        .map_err(|_| "The form contains a NUL character")?;
        let page = CString::new(wrap_html(&options.html, &options.theme))
            .map_err(|_| "The form contains a NUL character")?;
        let shared = Rc::new(RefCell::new(Shared::default()));
        let user_data = Rc::into_raw(shared.clone());
        unsafe {
            let content = (gtk.webkit_user_content_manager_new)();
            (gtk.webkit_user_content_manager_register_script_message_handler)(
                content,
                c"paseo".as_ptr(),
            );
            let user_script = (gtk.webkit_user_script_new)(
                script.as_ptr(),
                WEBKIT_USER_CONTENT_INJECT_TOP_FRAME,
                WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_END,
                std::ptr::null(),
                std::ptr::null(),
            );
            (gtk.webkit_user_content_manager_add_script)(content, user_script);
            (gtk.webkit_user_script_unref)(user_script);
            let view = (gtk.webkit_web_view_new_with_user_content_manager)(content);
            // Page scripts and inline handlers stay off; the user script above still runs.
            (gtk.webkit_settings_set_enable_javascript_markup)(
                (gtk.webkit_web_view_get_settings)(view),
                0,
            );
            let background = options.theme.background;
            (gtk.webkit_web_view_set_background_color)(
                view,
                &GdkRgba {
                    red: background.x as f64,
                    green: background.y as f64,
                    blue: background.z as f64,
                    alpha: 1.0,
                },
            );
            let mut handlers = Vec::new();
            let mut connect = |instance: Ptr, signal: &CStr, callback: *const c_void| {
                let id = (gtk.g_signal_connect_data)(
                    instance,
                    signal.as_ptr(),
                    callback,
                    user_data as Ptr,
                    std::ptr::null(),
                    0,
                );
                handlers.push((instance, id));
            };
            connect(
                content,
                c"script-message-received::paseo",
                on_message as *const c_void,
            );
            connect(view, c"decide-policy", on_decide_policy as *const c_void);
            connect(view, c"context-menu", on_context_menu as *const c_void);
            connect(
                view,
                c"button-press-event",
                on_button_press as *const c_void,
            );
            connect(
                view,
                c"web-process-terminated",
                on_terminated as *const c_void,
            );

            let popup = (gtk.gtk_window_new)(GTK_WINDOW_POPUP);
            (gtk.gtk_container_add)(popup, view);
            (gtk.gtk_window_resize)(popup, 1, 1);
            (gtk.gtk_widget_realize)(popup);
            let popup_xid = (gtk.gdk_x11_window_get_xid)((gtk.gtk_widget_get_window)(popup));
            let display = (gtk.gdk_display_get_default)();
            let xdisplay = (gtk.gdk_x11_display_get_xdisplay)(display);
            let pixel = |value: f32| ((value.clamp(0.0, 1.0) * 255.0).round() as c_ulong) & 0xff;
            let clip = (gtk.XCreateSimpleWindow)(
                xdisplay,
                top,
                0,
                0,
                1,
                1,
                0,
                0,
                pixel(background.x) << 16 | pixel(background.y) << 8 | pixel(background.z),
            );
            (gtk.gdk_x11_display_error_trap_push)(display);
            (gtk.XReparentWindow)(xdisplay, popup_xid, clip, 0, 0);
            (gtk.XFlush)(xdisplay);
            (gtk.gdk_x11_display_error_trap_pop_ignored)(display);
            {
                let mut shared = shared.borrow_mut();
                shared.display = display;
                shared.xdisplay = xdisplay;
                shared.popup = popup_xid;
            }
            (gtk.webkit_web_view_load_html)(view, page.as_ptr(), c"about:blank".as_ptr());
            Ok(Self {
                gtk,
                popup,
                view,
                content,
                clip,
                top,
                shared,
                user_data,
                handlers,
                placed: None,
                shown: false,
                zoom: 0.0,
            })
        }
    }

    pub(super) fn place(&mut self, _cx: &mut Cx, frame: Rect, visible: Option<Rect>, dpi: f64) {
        let gtk = self.gtk;
        let xdisplay = self.shared.borrow().xdisplay;
        let device = |value: f64| (value * dpi).round() as i32;
        let Some(visible) = visible else {
            if self.placed.take().is_some() {
                unsafe {
                    (gtk.XUnmapWindow)(xdisplay, self.clip);
                    (gtk.XFlush)(xdisplay);
                }
            }
            return;
        };
        let clip = [
            device(visible.pos.x),
            device(visible.pos.y),
            device(visible.size.x).max(1),
            device(visible.size.y).max(1),
        ];
        let view = [
            device(frame.pos.x) - clip[0],
            device(frame.pos.y) - clip[1],
            device(frame.size.x).max(1),
            device(frame.size.y).max(1),
        ];
        if self.placed == Some((clip, view)) && self.zoom == dpi {
            return;
        }
        unsafe {
            if self.zoom != dpi {
                // CSS px follow the app's logical px.
                (gtk.webkit_web_view_set_zoom_level)(self.view, dpi);
                self.zoom = dpi;
            }
            (gtk.XMoveResizeWindow)(
                xdisplay,
                self.clip,
                clip[0],
                clip[1],
                clip[2] as c_uint,
                clip[3] as c_uint,
            );
            (gtk.gtk_window_move)(self.popup, view[0], view[1]);
            (gtk.gtk_window_resize)(self.popup, view[2], view[3]);
            if !self.shown {
                (gtk.gtk_widget_show_all)(self.popup);
                self.shown = true;
            }
            if self.placed.is_none() {
                (gtk.XMapWindow)(xdisplay, self.clip);
            }
            // Parts the clip uncovered were never painted while hidden: GTK does not repaint
            // them on its own.
            (gtk.gtk_widget_queue_draw)(self.view);
            (gtk.XFlush)(xdisplay);
        }
        self.placed = Some((clip, view));
    }

    pub(super) fn run_script(&mut self, script: &str) {
        let Ok(script) = CString::new(script) else {
            return;
        };
        unsafe {
            (self.gtk.webkit_web_view_run_javascript)(
                self.view,
                script.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
    }

    pub(super) fn blur(&mut self) {
        let mut shared = self.shared.borrow_mut();
        if !std::mem::take(&mut shared.focused) {
            return;
        }
        let gtk = self.gtk;
        unsafe {
            (gtk.gdk_x11_display_error_trap_push)(shared.display);
            (gtk.XSetInputFocus)(shared.xdisplay, self.top, REVERT_TO_PARENT, CURRENT_TIME);
            (gtk.XFlush)(shared.xdisplay);
            (gtk.gdk_x11_display_error_trap_pop_ignored)(shared.display);
        }
    }

    pub(super) fn pump(&mut self, _cx: &mut Cx) -> Vec<WebViewEvent> {
        // Bounded: a page that keeps GLib busy cannot stall the app's event loop.
        for _ in 0..64 {
            if unsafe { (self.gtk.g_main_context_iteration)(std::ptr::null_mut(), 0) } == 0 {
                break;
            }
        }
        std::mem::take(&mut self.shared.borrow_mut().events)
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.blur();
        let gtk = self.gtk;
        unsafe {
            for (instance, id) in self.handlers.drain(..) {
                (gtk.g_signal_handler_disconnect)(instance, id);
            }
            (gtk.gtk_widget_destroy)(self.popup);
            (gtk.g_object_unref)(self.content);
            let shared = self.shared.borrow();
            (gtk.gdk_x11_display_error_trap_push)(shared.display);
            (gtk.XDestroyWindow)(shared.xdisplay, self.clip);
            (gtk.XFlush)(shared.xdisplay);
            (gtk.gdk_x11_display_error_trap_pop_ignored)(shared.display);
            drop(shared);
            drop(Rc::from_raw(self.user_data));
        }
    }
}

/// The X11 top-level Makepad created for `window`.
fn top_level(window: WindowId) -> Option<c_ulong> {
    get_xlib_app_global()
        .window_map
        .iter()
        .find(|(_, xlib)| unsafe { (***xlib).window_id } == window)
        .map(|(xid, _)| *xid)
}

/// # Safety
/// `data` is the view's `user_data`, valid until its handlers are disconnected.
unsafe fn shared<'a>(data: Ptr) -> &'a RefCell<Shared> {
    unsafe { &*(data as *const RefCell<Shared>) }
}

unsafe extern "C" fn on_message(_content: Ptr, result: Ptr, data: Ptr) {
    let Ok(gtk) = gtk() else { return };
    unsafe {
        let text = (gtk.jsc_value_to_string)((gtk.webkit_javascript_result_get_js_value)(result));
        if text.is_null() {
            return;
        }
        let message = CStr::from_ptr(text).to_string_lossy().into_owned();
        (gtk.g_free)(text.cast());
        if let Some(event) = parse_message(&message) {
            shared(data).borrow_mut().events.push(event);
        }
    }
}

/// Allows the page's initial load and its resources; refuses links, form posts, refreshes
/// and new windows.
unsafe extern "C" fn on_decide_policy(_view: Ptr, decision: Ptr, kind: c_int, data: Ptr) -> c_int {
    if kind == WEBKIT_POLICY_DECISION_TYPE_RESPONSE {
        return 0;
    }
    let mut shared = unsafe { shared(data) }.borrow_mut();
    if kind == WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION && !shared.navigated {
        shared.navigated = true;
        return 0;
    }
    if let Ok(gtk) = gtk() {
        unsafe { (gtk.webkit_policy_decision_ignore)(decision) };
    }
    1
}

unsafe extern "C" fn on_context_menu(
    _view: Ptr,
    _menu: Ptr,
    _event: Ptr,
    _hit: Ptr,
    _data: Ptr,
) -> c_int {
    1
}

/// A press gives the view X keyboard focus, which the window manager never does for it.
unsafe extern "C" fn on_button_press(_view: Ptr, _event: Ptr, data: Ptr) -> c_int {
    let Ok(gtk) = gtk() else { return 0 };
    let mut shared = unsafe { shared(data) }.borrow_mut();
    unsafe {
        (gtk.gdk_x11_display_error_trap_push)(shared.display);
        (gtk.XSetInputFocus)(
            shared.xdisplay,
            shared.popup,
            REVERT_TO_PARENT,
            CURRENT_TIME,
        );
        (gtk.XFlush)(shared.xdisplay);
        (gtk.gdk_x11_display_error_trap_pop_ignored)(shared.display);
    }
    shared.focused = true;
    0
}

unsafe extern "C" fn on_terminated(_view: Ptr, reason: c_int, data: Ptr) {
    unsafe { shared(data) }
        .borrow_mut()
        .events
        .push(WebViewEvent::Failed(format!(
            "The web view's process ended ({reason})"
        )));
}
