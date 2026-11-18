use super::{egl_sys::{EGLBoolean, EGLDisplay, EGLSurface, LibEgl}, gl_sys::{self, LibGl}};
use crate::{cx::Cx, damage::{force_full_redraw, log_oracle, DamageFrame, DamageHistory, DamageRect, FullDamageReason}, makepad_math::Vec4, pass::PassId};
use std::{ffi::CStr, sync::LazyLock};

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_BUFFER_AGE: i32 = 0x313D; // EXT and KHR assign the same enum.

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

/// Histories are owned by EGL *surfaces*, not GL contexts or passes.
#[derive(Default)]
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
    egl_fallback: &'static str,
    logical_full: Option<FullDamageReason>,
    oracle_result: OracleResult,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OracleResult {
    Full,
    Equal,
    Mismatch,
    Error,
}

impl OracleResult {
    fn label(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Equal => "equal",
            Self::Mismatch => "mismatch",
            Self::Error => "error",
        }
    }
}

static VERIFY_DAMAGE: LazyLock<bool> = LazyLock::new(|| {
    #[cfg(target_os = "android")]
    {
        use std::ffi::{c_char, c_int};
        unsafe extern "C" {
            fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
        }
        let mut value = [0 as c_char; 92]; // PROP_VALUE_MAX
        let len = unsafe { __system_property_get(c"debug.paseo.verify_damage".as_ptr(), value.as_mut_ptr()) };
        if len == 1 && value[0] == b'1' as c_char { return true; }
    }
    std::env::var_os("PASEO_VERIFY_DAMAGE").is_some_and(|value| value == "1")
});

impl EglWindowDamage {
    pub fn invalidate(&mut self) {
        self.scene.invalidate();
        self.history.invalidate();
        self.size = None;
        self.oracle.valid = false;
    }

    pub fn release_oracle(&mut self, gl: &LibGl) {
        self.oracle.valid = false;
        if let Some(previous) = self.oracle.previous.take() {
            unsafe {
                (gl.glDeleteFramebuffers)(1, &previous.framebuffer);
                (gl.glDeleteRenderbuffers)(1, &previous.color);
            }
        }
    }

    pub fn prepare(&mut self, surface: &Surface<'_>, cx: &Cx, pass: PassId, size: [i32; 2], (color, depth): (Vec4, f32)) -> Frame {
        let full = DamageRect::full(size);
        let mut frame = Frame {
            logical: full, repair: full, size, age: 0, tracked: false, partial: false,
            egl_fallback: "none", logical_full: None, oracle_result: OracleResult::Full,
        };
        let forced = force_full_redraw();
        if forced || self.disabled || full.is_empty() {
            frame.egl_fallback = if forced {
                "forced"
            } else if self.disabled {
                "disabled"
            } else {
                "empty_surface"
            };
            return frame;
        }
        let verify = *VERIFY_DAMAGE;
        let caps = *self.caps.get_or_insert_with(|| surface.capabilities());
        if !caps.age && !verify { return frame; }
        if surface.query(EGL_WIDTH) != Some(size[0]) || surface.query(EGL_HEIGHT) != Some(size[1]) {
            self.invalidate();
            frame.egl_fallback = "surface_size";
            return frame;
        }
        if self.size != Some(size) {
            self.invalidate();
            self.size = Some(size);
        }
        let root = cx.passes[pass].main_draw_list_id.unwrap();
        let clear = [color.x.to_bits(), color.y.to_bits(), color.z.to_bits(), color.w.to_bits(), depth.to_bits()];
        if verify {
            (frame.logical, frame.logical_full) = self.scene.collect_detailed(cx, pass, root, size, clear);
        } else {
            frame.logical = self.scene.collect(cx, pass, root, size, clear);
        }
        frame.tracked = true;
        let age = if caps.age { surface.query(EGL_BUFFER_AGE) } else { None };
        frame.age = age.unwrap_or(0);
        let repair = self.history.repair(frame.age, frame.logical);
        frame.repair = repair.unwrap_or(full);
        frame.partial = frame.repair != full;
        if verify && !frame.partial {
            frame.egl_fallback = if !caps.age {
                "unsupported_age"
            } else if age.is_none() {
                "age_query"
            } else if frame.age <= 0 {
                "invalid_age"
            } else if repair.is_none() {
                "missing_history"
            } else if frame.logical == full {
                "logical_full"
            } else {
                "history_coverage"
            };
        }
        if frame.partial && !verify {
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
        if !*VERIFY_DAMAGE {
            cx.render_gl_window(pass, frame.size, clear, frame.partial.then_some((&self.scene, frame.repair)));
            return;
        }
        if !frame.tracked {
            cx.render_gl_window(pass, frame.size, clear, None);
            return;
        }

        // Reconstruct age one from our own previous presentation, even if EGL
        // has no buffer-age extension or the backbuffer contains older pixels.
        // The partial pass clears depth inside the repair; only color survives.
        let restored = if frame.logical != DamageRect::full(frame.size) {
            self.oracle.restore(cx.os.gl())
        } else {
            Ok(false)
        };
        let candidate_ok = matches!(restored, Ok(true)) && {
            cx.render_gl_window(pass, frame.size, clear, Some((&self.scene, frame.logical)));
            read_pixels(cx.os.gl(), frame.size, &mut self.oracle.candidate)
        };
        // Same time, retained instances, and offscreen inputs as the candidate.
        cx.render_gl_window(pass, frame.size, clear, None);
        match restored {
            Ok(true) => {
                let reference_ok = read_pixels(cx.os.gl(), frame.size, &mut self.oracle.reference);
                if !candidate_ok || !reference_ok {
                    self.oracle_error(frame, "pixel readback");
                } else if self.oracle.candidate == self.oracle.reference {
                    frame.oracle_result = OracleResult::Equal;
                } else {
                    self.log_mismatch(cx, frame);
                    frame.oracle_result = OracleResult::Mismatch;
                    // Present the full reference and resume checking after a fresh baseline.
                    self.invalidate();
                    frame.tracked = false;
                }
            }
            Ok(false) => (),
            Err(error) => self.oracle_error(frame, error),
        }
        if let Err(error) = self.oracle.save(cx.os.gl(), frame.size) {
            self.oracle_error(frame, error);
        }
    }

    fn oracle_error(&self, frame: &mut Frame, error: &'static str) {
        log_oracle(format_args!("paseo_damage oracle_error frame={} reason={error}", self.frame + 1));
        if frame.oracle_result != OracleResult::Mismatch {
            frame.oracle_result = OracleResult::Error;
        }
    }

    fn log_mismatch(&mut self, cx: &Cx, frame: &Frame) {
        let mut first = None;
        let mut bounds = DamageRect::default();
        let mut pixels = 0;
        for (i, (candidate, reference)) in self.oracle.candidate.chunks_exact(4).zip(self.oracle.reference.chunks_exact(4)).enumerate() {
            if candidate == reference { continue; }
            let x = (i % frame.size[0] as usize) as i32;
            let y = (i / frame.size[0] as usize) as i32;
            first.get_or_insert((x, y, candidate, reference));
            bounds = bounds.union(DamageRect { x0: x, y0: y, x1: x + 1, y1: y + 1 });
            pixels += 1;
        }
        log_oracle(format_args!("paseo_damage mismatch_detail frame={} repair={:?} first_pixel={first:?} bbox={:?} pixels={pixels}",
            self.frame + 1, frame.logical.egl(), bounds.egl()));
        if let Some((x, y, ..)) = first {
            self.scene.log_pixel(cx, self.frame + 1, (x, y), frame.logical);
        }
        #[cfg(target_os = "linux")]
        if self.oracle.mismatch_count < 4 {
            if let Some(dir) = std::env::var_os("PASEO_DAMAGE_DUMP_DIR") {
                for (name, bytes) in [("partial", &self.oracle.candidate), ("reference", &self.oracle.reference)] {
                    let file = std::path::Path::new(&dir).join(format!("{}-{}x{}-{name}.rgba", self.frame + 1, frame.size[0], frame.size[1]));
                    if let Err(error) = std::fs::write(&file, bytes) {
                        log_oracle(format_args!("paseo_damage could not dump {file:?}: {error}"));
                    }
                }
            }
            self.oracle.mismatch_count += 1;
        }
    }

    pub fn present(&mut self, surface: &Surface<'_>, frame: Frame) -> bool {
        let damage_swap = self.caps.and_then(|caps| if frame.partial && !*VERIFY_DAMAGE { caps.swap } else { None });
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
        if *VERIFY_DAMAGE {
            self.report_frame(&frame);
        }
        true
    }

    fn report_frame(&mut self, frame: &Frame) {
        self.frame += 1;
        #[cfg(target_os = "android")]
        {
            self.oracle.results[frame.oracle_result as usize] += 1;
            if self.frame == 1 || self.frame.is_multiple_of(120) {
                let [full, equal, mismatch, error] = std::mem::take(&mut self.oracle.results);
                log_oracle(format_args!("paseo_damage summary frame={} full={full} equal={equal} mismatch={mismatch} error={error} age={} egl_fallback={} logical_full={:?} size={}x{}",
                    self.frame, frame.age, frame.egl_fallback, frame.logical_full, frame.size[0], frame.size[1]));
            }
            if !matches!(frame.oracle_result, OracleResult::Mismatch | OracleResult::Error) {
                return;
            }
        }
        log_oracle(format_args!("paseo_damage frame={} result={} age={} egl_fallback={} logical_full={:?} size={}x{} repair={:?} logical={:?}",
            self.frame, frame.oracle_result.label(), frame.age, frame.egl_fallback, frame.logical_full,
            frame.size[0], frame.size[1], frame.repair.egl(), frame.logical.egl()));
    }
}

#[derive(Default)]
struct PixelOracle {
    candidate: Vec<u8>,
    reference: Vec<u8>,
    #[cfg(target_os = "linux")]
    mismatch_count: u32,
    previous: Option<PreviousFrame>,
    valid: bool,
    #[cfg(target_os = "android")]
    results: [u32; 4],
}

#[derive(Default)]
struct PreviousFrame {
    framebuffer: u32,
    color: u32,
    size: [i32; 2],
}

impl PixelOracle {
    fn restore(&self, gl: &LibGl) -> Result<bool, &'static str> {
        if !self.valid { return Ok(false); }
        let previous = self.previous.as_ref().unwrap();
        blit(gl, previous.framebuffer, 0, previous.size)?;
        Ok(true)
    }

    fn save(&mut self, gl: &LibGl, size: [i32; 2]) -> Result<(), &'static str> {
        self.valid = false;
        if gl.glBlitFramebuffer.is_none() { return Err("glBlitFramebuffer unavailable"); }
        let previous = self.previous.get_or_insert_with(|| {
            let mut previous = PreviousFrame::default();
            unsafe {
                (gl.glGenFramebuffers)(1, &mut previous.framebuffer);
                (gl.glGenRenderbuffers)(1, &mut previous.color);
            }
            previous
        });
        if previous.size != size {
            clear_gl_errors(gl);
            unsafe {
                (gl.glBindRenderbuffer)(gl_sys::RENDERBUFFER, previous.color);
                (gl.glRenderbufferStorage)(gl_sys::RENDERBUFFER, gl_sys::RGBA8, size[0], size[1]);
                (gl.glBindFramebuffer)(gl_sys::FRAMEBUFFER, previous.framebuffer);
                (gl.glFramebufferRenderbuffer)(gl_sys::FRAMEBUFFER, gl_sys::COLOR_ATTACHMENT0, gl_sys::RENDERBUFFER, previous.color);
                let status = (gl.glCheckFramebufferStatus)(gl_sys::FRAMEBUFFER);
                (gl.glBindRenderbuffer)(gl_sys::RENDERBUFFER, 0);
                (gl.glBindFramebuffer)(gl_sys::FRAMEBUFFER, 0);
                if status != gl_sys::FRAMEBUFFER_COMPLETE || (gl.glGetError)() != 0 {
                    return Err("previous-frame framebuffer allocation");
                }
            }
            previous.size = size;
        }
        blit(gl, 0, previous.framebuffer, size)?;
        self.valid = true;
        Ok(())
    }
}

fn blit(gl: &LibGl, source: u32, destination: u32, [width, height]: [i32; 2]) -> Result<(), &'static str> {
    let copy = gl.glBlitFramebuffer.ok_or("glBlitFramebuffer unavailable")?;
    clear_gl_errors(gl);
    unsafe {
        (gl.glDisable)(gl_sys::SCISSOR_TEST);
        (gl.glBindFramebuffer)(gl_sys::READ_FRAMEBUFFER, source);
        (gl.glBindFramebuffer)(gl_sys::DRAW_FRAMEBUFFER, destination);
        copy(0, 0, width, height, 0, 0, width, height, gl_sys::COLOR_BUFFER_BIT, gl_sys::NEAREST);
        (gl.glBindFramebuffer)(gl_sys::FRAMEBUFFER, 0);
        if (gl.glGetError)() != 0 {
            return Err("previous-frame blit");
        }
    }
    Ok(())
}

fn clear_gl_errors(gl: &LibGl) {
    for _ in 0..16 {
        if unsafe { (gl.glGetError)() } == 0 { break; }
    }
}

fn read_pixels(gl: &LibGl, [width, height]: [i32; 2], bytes: &mut Vec<u8>) -> bool {
    let Some(len) = (width as usize).checked_mul(height as usize).and_then(|area| area.checked_mul(4)) else { return false; };
    bytes.resize(len, 0);
    clear_gl_errors(gl);
    unsafe {
        // PACK defaults to four bytes; RGBA8 rows are tightly packed at every width.
        (gl.glReadPixels)(0, 0, width, height, gl_sys::RGBA, gl_sys::UNSIGNED_BYTE, bytes.as_mut_ptr().cast());
        (gl.glGetError)() == 0
    }
}
