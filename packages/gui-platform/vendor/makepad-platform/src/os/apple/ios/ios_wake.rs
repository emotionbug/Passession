use super::{ios_app::IosApp, ios_event::IosEvent, ios_state::WakeGate};
use std::{ffi::c_void, ptr};

// dispatch_get_main_queue() is an inline accessor in the public dispatch header.
#[link(name = "System")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, callback: extern "C" fn(*mut c_void));
}

pub(super) fn dispatch_main(callback: extern "C" fn(*mut c_void)) {
    unsafe { dispatch_async_f(ptr::addr_of!(_dispatch_main_q).cast(), ptr::null_mut(), callback) };
}

static WAKE: WakeGate = WakeGate::new();

pub(super) fn set_active(active: bool) {
    WAKE.set_active(active);
    if active { post(); }
}

pub(crate) fn post() {
    extern "C" fn drain(_: *mut c_void) {
        if WAKE.take() { IosApp::do_callback(IosEvent::Wake); }
    }
    if WAKE.request() { dispatch_main(drain); }
}
