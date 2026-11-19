//! Network.framework's default path monitor (the C API behind `NWPathMonitor`).
use block2::{Block, RcBlock};
use paseo_gui_core::CoreHandle;
use std::{
    cell::RefCell,
    ffi::{c_char, c_void},
    ptr::{NonNull, null_mut},
    sync::atomic::{AtomicBool, Ordering},
};

type Object = *mut c_void;

#[link(name = "Network", kind = "framework")]
unsafe extern "C" {
    fn nw_path_monitor_create() -> Object;
    fn nw_path_monitor_set_queue(monitor: Object, queue: Object);
    fn nw_path_monitor_set_update_handler(monitor: Object, handler: *const Block<dyn Fn(Object)>);
    fn nw_path_monitor_start(monitor: Object);
    fn nw_path_monitor_cancel(monitor: Object);
    fn nw_release(object: Object);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attributes: Object) -> Object;
    fn dispatch_release(queue: Object);
}

struct Monitor {
    path: NonNull<c_void>,
    queue: NonNull<c_void>,
}

impl Monitor {
    fn start(core: CoreHandle) -> Option<Self> {
        // Network.framework copies the update block onto a serial queue. Its
        // only captures are thread-safe and do not retain the monitor.
        unsafe {
            let path = NonNull::new(nw_path_monitor_create())?;
            let Some(queue) = NonNull::new(dispatch_queue_create(
                c"sh.paseo.sessions.network".as_ptr(),
                null_mut(),
            )) else {
                nw_release(path.as_ptr());
                return None;
            };
            let seen_initial = AtomicBool::new(false);
            let update = RcBlock::new(move |_: Object| {
                if seen_initial.swap(true, Ordering::Relaxed) {
                    core.network_changed();
                }
            });
            nw_path_monitor_set_queue(path.as_ptr(), queue.as_ptr());
            nw_path_monitor_set_update_handler(path.as_ptr(), &*update);
            nw_path_monitor_start(path.as_ptr());
            Some(Self { path, queue })
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            nw_path_monitor_cancel(self.path.as_ptr());
            nw_release(self.path.as_ptr());
            dispatch_release(self.queue.as_ptr());
        }
    }
}

thread_local! {
    static MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
}

pub(super) fn start(core: CoreHandle) {
    MONITOR.with(|monitor| *monitor.borrow_mut() = Monitor::start(core));
}

pub(super) fn stop() {
    let _ = MONITOR.try_with(|monitor| monitor.borrow_mut().take());
}
