//! UIKit host for the shared Metal IOSurface presenter.
use super::{ios_app::IosApp, ios_event::IosEvent};
use crate::{
    event::WindowGeom,
    makepad_math::dvec2,
    os::apple::{apple_sys::*, surface_present::{self, SurfaceKey, SurfacePresenter}},
};
use std::sync::atomic::{AtomicBool, Ordering};

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    static kCAGravityBottomLeft: ObjcId;
}

// dispatch_get_main_queue() in the public dispatch header is an inline accessor
// for this exported queue. Only UIKit/main-thread code may inspect IosApp.
#[link(name = "System")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(queue: *const c_void, context: *mut c_void, callback: extern "C" fn(*mut c_void));
}

static READY_QUEUED: AtomicBool = AtomicBool::new(false);

pub(in crate::os::apple) fn post_surface_ready() {
    extern "C" fn wake(_: *mut c_void) {
        READY_QUEUED.store(false, Ordering::Release);
        // UIKit may have queued layout and completion in the same run-loop turn.
        IosApp::surface_frame(false);
        IosApp::do_callback(IosEvent::SurfaceReady);
    }
    if !READY_QUEUED.swap(true, Ordering::AcqRel) {
        unsafe { dispatch_async_f(std::ptr::addr_of!(_dispatch_main_q).cast(), std::ptr::null_mut(), wake) };
    }
}

pub(super) fn presenter(view: ObjcId) -> SurfacePresenter {
    let layer = surface_present::new_layer();
    surface_present::transaction(|| unsafe {
        let backing: ObjcId = msg_send![view, layer];
        // UIKit's layer coordinates increase downward. Gravity "bottom" is
        // minimum Y, i.e. the visual top. The image itself is not transformed.
        let () = msg_send![layer.as_id(), setContentsGravity: kCAGravityBottomLeft];
        let () = msg_send![backing, setMasksToBounds: YES];
        let () = msg_send![backing, insertSublayer: layer.as_id() atIndex: 0u32];
    });
    SurfacePresenter::new(layer)
}

#[derive(Clone, PartialEq)]
pub(super) struct SurfaceMetrics {
    pub(super) geom: WindowGeom,
    pub(super) screen: ObjcId,
    safe_area: EdgeInsets,
    size_classes: [isize; 2],
}

impl SurfaceMetrics {
    pub(super) fn get(view: ObjcId) -> Option<Self> {
        unsafe {
            let window: ObjcId = msg_send![view, window];
            let screen: ObjcId = msg_send![window, screen];
            if screen == nil { return None; }
            let bounds: NSRect = msg_send![view, bounds];
            let traits: ObjcId = msg_send![view, traitCollection];
            let trait_scale: f64 = msg_send![traits, displayScale];
            let scale = if trait_scale > 0.0 { trait_scale } else { msg_send![screen, scale] };
            let size = dvec2(bounds.size.width, bounds.size.height);
            if !(scale > 0.0 && size.x > 0.0 && size.y > 0.0) { return None; }
            let window_bounds: NSRect = msg_send![window, bounds];
            let geom = WindowGeom {
                is_fullscreen: true,
                inner_size: size,
                outer_size: dvec2(window_bounds.size.width, window_bounds.size.height),
                dpi_factor: scale,
                ..WindowGeom::default()
            };
            Some(Self {
                geom, screen,
                safe_area: msg_send![view, safeAreaInsets],
                size_classes: [msg_send![traits, horizontalSizeClass], msg_send![traits, verticalSizeClass]],
            })
        }
    }

    pub(super) fn key(&self, device: ObjcId) -> SurfaceKey {
        let scale = self.geom.dpi_factor;
        let physical = self.geom.inner_size * scale;
        SurfaceKey { device, size: [physical.x.round() as i32, physical.y.round() as i32], scale, screen: self.screen }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq)]
struct EdgeInsets {
    top: f64,
    left: f64,
    bottom: f64,
    right: f64,
}

#[repr(C)]
struct FrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

pub(super) struct DisplayLink {
    link: RcObjcId,
    maximum: f32,
    running: bool,
}

impl DisplayLink {
    pub(super) fn new(target: ObjcId) -> Self {
        unsafe {
            let link: ObjcId = msg_send![class!(CADisplayLink), displayLinkWithTarget: target selector: sel!(displayFrame:)];
            let link = RcObjcId::from_unowned(NonNull::new(link).unwrap());
            let () = msg_send![link.as_id(), setPaused: YES];
            let run_loop: ObjcId = msg_send![class!(NSRunLoop), mainRunLoop];
            let () = msg_send![link.as_id(), addToRunLoop: run_loop forMode: NSRunLoopCommonModes];
            Self { link, maximum: 0.0, running: false }
        }
    }

    pub(super) fn set_screen(&mut self, screen: ObjcId) {
        let maximum: isize = unsafe { msg_send![screen, maximumFramesPerSecond] };
        let maximum = maximum.clamp(1, 120) as f32;
        if maximum != self.maximum {
            self.maximum = maximum;
            let range = FrameRateRange { minimum: maximum.min(30.0), maximum, preferred: maximum };
            let () = unsafe { msg_send![self.link.as_id(), setPreferredFrameRateRange: range] };
        }
    }

    pub(super) fn set_running(&mut self, running: bool) {
        if self.running == running { return; }
        self.running = running;
        let paused: BOOL = if running { NO } else { YES };
        let () = unsafe { msg_send![self.link.as_id(), setPaused: paused] };
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        let () = unsafe { msg_send![self.link.as_id(), invalidate] };
    }
}
