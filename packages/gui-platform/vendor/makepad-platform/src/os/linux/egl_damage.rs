use super::{egl_sys::{EGLBoolean, EGLDisplay, EGLSurface, LibEgl}, gl_sys::{self, LibGl}};
use crate::{cx::Cx, damage::{force_full_redraw, DamageFrame, DamageRect}, makepad_math::Vec4, pass::PassId};
use std::{ffi::CStr, sync::LazyLock};

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_BUFFER_AGE: i32 = 0x313D; // EXT and KHR assign the same enum.
const HISTORY: usize = 4;

type SwapDamage = unsafe extern "C" fn(EGLDisplay, EGLSurface, *const i32, i32) -> EGLBoolean;
type SetDamage = unsafe extern "C" fn(EGLDisplay, EGLSurface, *mut i32, i32) -> EGLBoolean;

#[derive(Clone, Copy, Default)]
struct Capabilities {
    age: bool,
    swap: Option<SwapDamage>,
    set_region: Option<SetDamage>,
}

pub(crate) struct Surface<'a> {
    pub egl: &'a LibEgl,
    pub display: EGLDisplay,
    pub surface: EGLSurface,
}

impl Surface<'_> {
    fn capabilities(&self) -> Capabilities {
        unsafe {
            let (Some(query), Some(get_proc)) = (self.egl.eglQueryString, self.egl.eglGetProcAddress) else {
                return Capabilities::default();
            };
            let ptr = query(self.display, EGL_EXTENSIONS);
            if ptr.is_null() || self.egl.eglQuerySurface.is_none() { return Capabilities::default(); }
            let extensions = CStr::from_ptr(ptr).to_bytes();
            let has = |name: &[u8]| extensions.split(|c| *c == b' ').any(|extension| extension == name);
            let mut swap = None;
            for (extension, symbol) in [
                (b"EGL_KHR_swap_buffers_with_damage".as_slice(), c"eglSwapBuffersWithDamageKHR"),
                (b"EGL_EXT_swap_buffers_with_damage".as_slice(), c"eglSwapBuffersWithDamageEXT"),
            ] {
                if has(extension) {
                    let ptr = get_proc(symbol.as_ptr());
                    if !ptr.is_null() {
                        swap = Some(std::mem::transmute::<_, SwapDamage>(ptr));
                        break;
                    }
                }
            }
            let partial_update = has(b"EGL_KHR_partial_update");
            let set_region = if partial_update {
                let ptr = get_proc(c"eglSetDamageRegionKHR".as_ptr());
                (!ptr.is_null()).then(|| std::mem::transmute::<_, SetDamage>(ptr))
            } else {
                None
            };
            Capabilities { age: has(b"EGL_EXT_buffer_age") || set_region.is_some(), swap, set_region }
        }
    }

    fn query(&self, attribute: i32) -> Option<i32> {
        let query = self.egl.eglQuerySurface?;
        let mut value = 0;
        let success = unsafe { query(self.display, self.surface, attribute, &mut value) } != 0;
        success.then_some(value)
    }
}

#[derive(Clone, Default)]
struct DamageHistory {
    frames: [DamageRect; HISTORY],
    next: usize,
    len: usize,
}

impl DamageHistory {
    fn repair(&self, age: i32, current: DamageRect) -> Option<DamageRect> {
        let count = usize::try_from(age.checked_sub(1)?).ok()?;
        if count > self.len { return None; }
        Some((1..=count).fold(current, |rect, index| rect.union(self.frames[(self.next + HISTORY - index) % HISTORY])))
    }

    fn push(&mut self, rect: DamageRect) {
        self.frames[self.next] = rect;
        self.next = (self.next + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
    }
}

/// Histories are owned by EGL *surfaces*, not GL contexts or passes.
#[derive(Clone, Default)]
pub(crate) struct EglWindowDamage {
    caps: Option<Capabilities>,
    scene: DamageFrame,
    history: DamageHistory,
    size: Option<[i32; 2]>,
    disabled: bool,
    oracle: PixelOracle,
    frame: u64,
}

#[derive(Clone, Copy)]
pub(crate) struct Frame {
    logical: DamageRect,
    repair: DamageRect,
    size: [i32; 2],
    age: i32,
    tracked: bool,
    partial: bool,
    checked: bool,
}

static VERIFY_DAMAGE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("PASEO_VERIFY_DAMAGE").is_some_and(|value| value == "1"));

impl EglWindowDamage {
    pub fn invalidate(&mut self) {
        self.scene.invalidate();
        self.history.len = 0;
        self.size = None;
    }

    pub fn prepare(&mut self, surface: &Surface<'_>, cx: &Cx, pass: PassId, size: [i32; 2], (color, depth): (Vec4, f32)) -> Frame {
        let full = DamageRect::full(size);
        let mut frame = Frame { logical: full, repair: full, size, age: 0, tracked: false, partial: false, checked: false };
        if force_full_redraw() || self.disabled || full.is_empty() { return frame; }
        let caps = *self.caps.get_or_insert_with(|| surface.capabilities());
        if !caps.age { return frame; }
        if surface.query(EGL_WIDTH) != Some(size[0]) || surface.query(EGL_HEIGHT) != Some(size[1]) {
            self.invalidate();
            return frame;
        }
        if self.size != Some(size) {
            self.invalidate();
            self.size = Some(size);
        }
        let root = cx.passes[pass].main_draw_list_id.unwrap();
        frame.logical = self.scene.collect(cx, pass, root, size, [color.x.to_bits(), color.y.to_bits(), color.z.to_bits(), color.w.to_bits(), depth.to_bits()]);
        frame.tracked = true;
        frame.age = surface.query(EGL_BUFFER_AGE).unwrap_or(0);
        frame.repair = self.history.repair(frame.age, frame.logical).unwrap_or(full);
        frame.partial = frame.repair != full;
        if frame.partial && !*VERIFY_DAMAGE {
            if let Some(set_region) = caps.set_region {
                let mut rect = frame.repair.egl();
                let success = unsafe { set_region(surface.display, surface.surface, rect.as_mut_ptr(), 1) } != 0;
                if !success {
                    self.disabled = true;
                    self.invalidate();
                    frame.logical = full;
                    frame.repair = full;
                    frame.tracked = false;
                    frame.partial = false;
                }
            }
        }
        frame
    }

    pub fn render(&mut self, cx: &mut Cx, pass: PassId, clear: (Vec4, f32), frame: &mut Frame) {
        cx.render_gl_window(pass, frame.size, clear, frame.partial.then_some((&self.scene, frame.repair)));
        if frame.partial && *VERIFY_DAMAGE {
            // Same time, inputs, surface, retained instances, and offscreen textures.
            // No second event loop: its animation clock would make A/B incomparable.
            let candidate_ok = read_pixels(cx.os.gl(), frame.size, &mut self.oracle.candidate);
            cx.render_gl_window(pass, frame.size, clear, None);
            let reference_ok = read_pixels(cx.os.gl(), frame.size, &mut self.oracle.reference);
            frame.checked = true;
            if !candidate_ok || !reference_ok || self.oracle.candidate != self.oracle.reference {
                let first = self.oracle.candidate.chunks_exact(4).zip(self.oracle.reference.chunks_exact(4))
                    .position(|(a, b)| a != b)
                    .map(|i| (i % frame.size[0] as usize, i / frame.size[0] as usize));
                eprintln!("paseo_damage frame={} result=mismatch size={:?} repair={:?} first_pixel={first:?} readback=({candidate_ok},{reference_ok})", self.frame + 1, frame.size, frame.repair.egl());
                // The backbuffer now contains the reference. Never present the mismatch.
                self.disabled = true;
                self.invalidate();
                frame.tracked = false;
                frame.partial = false;
                frame.logical = DamageRect::full(frame.size);
            }
        }
    }

    pub fn present(&mut self, surface: &Surface<'_>, frame: Frame) -> bool {
        let damage_swap = self.caps.and_then(|caps| if frame.partial { caps.swap } else { None });
        let result = unsafe {
            if let Some(swap) = damage_swap {
                // The compositor is interested in the delta from the last *presentation*,
                // not the larger repair of a backbuffer last used several swaps ago.
                swap(surface.display, surface.surface, frame.logical.egl().as_ptr(), 1)
            } else {
                (surface.egl.eglSwapBuffers.unwrap())(surface.display, surface.surface)
            }
        } != 0;
        if !result {
            self.invalidate();
            if damage_swap.is_some() { self.disabled = true; }
            return false;
        }
        if frame.tracked {
            self.scene.commit();
            self.history.push(frame.logical);
        }
        self.frame += 1;
        if *VERIFY_DAMAGE {
            eprintln!("paseo_damage frame={} result={} age={} swap_damage={} size={}x{} repair={:?}", self.frame,
                if frame.checked && frame.partial { "equal" } else { "full" }, frame.age, damage_swap.is_some(),
                frame.size[0], frame.size[1], frame.repair.egl());
        }
        true
    }
}

#[derive(Clone, Default)]
struct PixelOracle {
    candidate: Vec<u8>,
    reference: Vec<u8>,
}

fn read_pixels(gl: &LibGl, [width, height]: [i32; 2], bytes: &mut Vec<u8>) -> bool {
    let Some(len) = (width as usize).checked_mul(height as usize).and_then(|area| area.checked_mul(4)) else { return false; };
    bytes.resize(len, 0);
    unsafe {
        // PACK defaults to four bytes; RGBA8 rows are tightly packed at every width.
        for _ in 0..16 {
            if (gl.glGetError)() == 0 { break; }
        }
        (gl.glReadPixels)(0, 0, width, height, gl_sys::RGBA, gl_sys::UNSIGNED_BYTE, bytes.as_mut_ptr().cast());
        (gl.glGetError)() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32) -> DamageRect { DamageRect { x0: x, y0: 0, x1: x + 1, y1: 1 } }

    #[test]
    fn repair_uses_only_intervening_successful_presentations() {
        let mut history = DamageHistory::default();
        assert_eq!(history.repair(0, rect(9)), None);
        assert_eq!(history.repair(1, rect(9)), Some(rect(9)));
        assert_eq!(history.repair(2, rect(9)), None);
        for i in 0..8 { history.push(rect(i)); }
        assert_eq!(history.repair(3, rect(9)), Some(rect(6).union(rect(9))));
        assert_eq!(history.repair(5, rect(9)), Some(rect(4).union(rect(9))));
        assert_eq!(history.repair(6, rect(9)), None);
    }
}
