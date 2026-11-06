//! Pairing QR scanner (original platform/mobile/mobile_qr_scanner.dart:
//! mobile_scanner + permission_handler). `QrScanView` shows the camera preview
//! and posts `PlatformEvent::QrScanned { link }` once per scan session for the
//! first pairing link (`#offer=`, `#connect=`, `relay://`), then pauses until
//! `resume_scanning`. The scan screen chrome (back header, scan frame,
//! "Pairing...", permission card) belongs to the shell, driven by `state()`.
//! Camera frames come from Makepad's video input (Android NDK camera, iOS
//! AVCapture); desktop clients pair by link and report `Unavailable`.

use crate::action::{PlatformAction, PlatformRequestId};
use makepad_widgets::*;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

live_design! {
    use link::shaders::*;

    pub QrScanViewBase = {{QrScanView}} {}
    pub QrScanView = <QrScanViewBase> {
        width: Fill,
        height: Fill,
        draw_bg: {
            color: #000
        }
        draw_frame: {
            texture image: texture2d
            // BoxFit.cover (mobile_scanner's default fit): the share of the
            // texture shown on each axis.
            instance crop: vec2(1.0, 1.0)
            fn pixel(self) -> vec4 {
                return sample2d(self.image, (self.pos - 0.5) * self.crop + 0.5).xyzw;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum QrScanState {
    Idle,
    /// Waiting for camera permission or the first frame.
    Starting,
    Scanning,
    /// A link was posted; scanning resumes on `resume_scanning`.
    Paused,
    /// The permission card: camera access was denied.
    PermissionDenied,
    Failed(String),
    /// This platform has no pairing camera (desktop).
    Unavailable,
}

/// One luminance frame copied off the camera thread.
struct LumaFrame {
    width: usize,
    height: usize,
    luma: Vec<u8>,
    serial: u64,
}

#[derive(Default)]
struct Shared {
    frame: Mutex<Option<LumaFrame>>,
    decoding: AtomicBool,
    found: Mutex<Option<String>>,
}

#[derive(Live, LiveHook, Widget)]
pub struct QrScanView {
    #[walk]
    walk: Walk,
    #[redraw]
    #[live]
    draw_bg: DrawColor,
    #[live]
    draw_frame: DrawColor,
    #[rust]
    state: QrScanStateCell,
    #[rust]
    shared: Arc<Shared>,
    #[rust]
    texture: Option<Texture>,
    #[rust]
    texture_size: (usize, usize),
    #[rust]
    shown_serial: u64,
    #[rust]
    permission_request: Option<PlatformRequestId>,
    #[rust]
    camera_open: bool,
    /// Mounting of the camera in use; `None` before one is chosen.
    #[rust]
    mount: Option<CameraMount>,
}

/// Lens facing and sensor orientation (degrees clockwise) of a camera.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CameraMount {
    front: bool,
    sensor: u16,
}

/// How camera frames map onto the screen: rotate clockwise, then mirror
/// horizontally (front cameras preview as a mirror, like CameraX's and
/// AVCapture's preview layers that mobile_scanner used).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PreviewTransform {
    rotation: u16,
    mirror: bool,
}

/// `Default` wrapper so the derive can initialize the state.
#[derive(Debug, Clone, PartialEq)]
struct QrScanStateCell(QrScanState);

impl Default for QrScanStateCell {
    fn default() -> Self {
        Self(QrScanState::Idle)
    }
}

impl Widget for QrScanView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        match event {
            Event::Actions(actions) => {
                for action in actions {
                    let Some(action) = action.downcast_ref::<PlatformAction>() else { continue };
                    if let Some(request) = self.permission_request
                        && let Some(result) = action.completed(request) {
                            self.permission_request = None;
                            self.permission_result(cx, result.clone());
                        }
                }
            }
            Event::VideoInputs(inputs) => self.choose_input(cx, inputs),
            Event::Signal => self.poll(cx),
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        let rect = cx.peek_walk_turtle(walk);
        self.draw_bg.draw_walk(cx, walk);
        if let Some(texture) = &self.texture {
            self.draw_frame.draw_vars.set_texture(0, texture);
            self.draw_frame.draw_abs(cx, rect);
        }
        DrawStep::done()
    }
}

impl QrScanView {
    pub fn state(&self) -> QrScanState {
        self.state.0.clone()
    }

    fn set_state(&mut self, cx: &mut Cx, state: QrScanState) {
        if self.state.0 != state {
            self.state.0 = state;
            self.redraw(cx);
        }
    }

    /// Requests camera permission and opens the camera.
    pub fn start(&mut self, cx: &mut Cx) {
        if !cfg!(any(target_os = "android", target_os = "ios")) {
            self.set_state(cx, QrScanState::Unavailable);
            return;
        }
        let Some(platform) = crate::platform() else {
            self.set_state(cx, QrScanState::Failed("The platform layer has not started.".into()));
            return;
        };
        *self.shared.found.lock().unwrap() = None;
        self.set_state(cx, QrScanState::Starting);
        self.permission_request = Some(platform.camera_permission(true));
    }

    /// Closes the camera (leaving the scan screen or app pause).
    pub fn stop(&mut self, cx: &mut Cx) {
        if self.camera_open {
            cx.use_video_input(&[]);
            self.camera_open = false;
        }
        *self.shared.frame.lock().unwrap() = None;
        self.texture = None;
        self.set_state(cx, QrScanState::Idle);
    }

    /// After a failed or cancelled pairing ("Scan QR code" again).
    pub fn resume_scanning(&mut self, cx: &mut Cx) {
        *self.shared.found.lock().unwrap() = None;
        if self.state.0 == QrScanState::Paused {
            self.set_state(cx, QrScanState::Scanning);
        } else if matches!(self.state.0, QrScanState::Idle | QrScanState::PermissionDenied | QrScanState::Failed(_)) {
            self.start(cx);
        }
    }

    fn permission_result(&mut self, cx: &mut Cx, result: Result<crate::PlatformValue, crate::PlatformError>) {
        match result {
            Ok(crate::PlatformValue::Permission(state)) if state.allowed() => self.open_camera(cx),
            Ok(_) => self.set_state(cx, QrScanState::PermissionDenied),
            Err(error) => self.set_state(cx, QrScanState::Failed(error.message)),
        }
    }

    fn open_camera(&mut self, cx: &mut Cx) {
        let shared = self.shared.clone();
        let mut serial = 0u64;
        cx.video_input(0, move |buffer| {
            if let Some((width, height, luma)) = luminance(&buffer) {
                serial += 1;
                *shared.frame.lock().unwrap() = Some(LumaFrame { width, height, luma, serial });
                SignalToUI::set_ui_signal();
            }
        });
        self.camera_open = true;
    }

    /// Picks the back camera (mobile_scanner's default facing; the first
    /// camera when there is none) and a format whose first plane is luminance.
    fn choose_input(&mut self, cx: &mut Cx, inputs: &VideoInputsEvent) {
        if !self.camera_open || inputs.descs.is_empty() {
            if self.camera_open {
                self.set_state(cx, QrScanState::Failed("No camera is available.".into()));
            }
            return;
        }
        let mounts = camera::mounts(&inputs.descs);
        let index = mounts.iter().position(|mount| matches!(mount, Some(mount) if !mount.front)).unwrap_or(0);
        self.mount = mounts.get(index).copied().flatten();
        let preferred = if cfg!(target_os = "android") { VideoPixelFormat::YUV420 } else { VideoPixelFormat::YUY2 };
        let mut choice = inputs.find_format(index, 1280, 720, preferred);
        if choice.is_empty() {
            choice = inputs.find_format(index, 640, 480, preferred);
        }
        if choice.is_empty() {
            // Any non-JPEG format whose luminance can be read.
            let desc = &inputs.descs[index];
            if let Some(format) = desc.formats.iter().find(|f| {
                matches!(f.pixel_format, VideoPixelFormat::YUV420 | VideoPixelFormat::YUY2 | VideoPixelFormat::GRAY | VideoPixelFormat::RGB24)
            }) {
                choice = vec![(desc.input_id, format.format_id)];
            }
        }
        if choice.is_empty() {
            self.set_state(cx, QrScanState::Failed("The camera offers no readable preview format.".into()));
            return;
        }
        cx.use_video_input(&choice);
        self.set_state(cx, QrScanState::Scanning);
    }

    fn poll(&mut self, cx: &mut Cx) {
        let found = self.shared.found.lock().unwrap().take();
        if let Some(link) = found
            && self.state.0 == QrScanState::Scanning {
                self.set_state(cx, QrScanState::Paused);
                crate::action::post_event(crate::PlatformEvent::QrScanned { link });
            }
        let frame = {
            let guard = self.shared.frame.lock().unwrap();
            match guard.as_ref() {
                Some(frame) if frame.serial != self.shown_serial => Some((frame.width, frame.height, frame.luma.clone(), frame.serial)),
                _ => None,
            }
        };
        let Some((width, height, luma, serial)) = frame else { return };
        self.shown_serial = serial;
        self.update_texture(cx, width, height, &luma);
        crate::action::post_event(crate::PlatformEvent::CameraFrame);
        if self.state.0 == QrScanState::Scanning && !self.shared.decoding.swap(true, Ordering::SeqCst) {
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                let link = decode(width, height, &luma);
                if let Some(link) = link {
                    *shared.found.lock().unwrap() = Some(link);
                    SignalToUI::set_ui_signal();
                }
                shared.decoding.store(false, Ordering::SeqCst);
            });
        }
    }

    /// Grey preview, rotated and mirrored for the current display orientation
    /// and cropped to fill the view.
    fn update_texture(&mut self, cx: &mut Cx, width: usize, height: usize, luma: &[u8]) {
        let transform = self.mount.map(camera::transform).unwrap_or_default();
        let pixels = transform_luma(width, height, luma, transform);
        let (out_w, out_h) = if transform.rotation % 180 == 90 { (height, width) } else { (width, height) };
        if self.texture.is_none() || self.texture_size != (out_w, out_h) {
            self.texture = Some(Texture::new_with_format(
                cx,
                TextureFormat::VecBGRAu8_32 { width: out_w, height: out_h, data: Some(pixels), updated: TextureUpdated::Full },
            ));
            self.texture_size = (out_w, out_h);
        } else if let Some(texture) = &self.texture {
            texture.put_back_vec_u32(cx, pixels, None);
        }
        let rect = self.draw_bg.area().rect(cx);
        let crop = cover_crop(out_w as f64, out_h as f64, rect.size.x, rect.size.y);
        self.draw_frame.draw_vars.set_var_instance(cx, &[live_id!(crop)], &[crop.0 as f32, crop.1 as f32]);
        self.redraw(cx);
    }
}

/// BGRA grey pixels of `luma` (`width * height`) after `transform`.
fn transform_luma(width: usize, height: usize, luma: &[u8], transform: PreviewTransform) -> Vec<u32> {
    let (out_w, out_h) = if transform.rotation % 180 == 90 { (height, width) } else { (width, height) };
    let mut pixels = Vec::with_capacity(out_w * out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let x = if transform.mirror { out_w - 1 - x } else { x };
            let (sx, sy) = match transform.rotation {
                90 => (y, height - 1 - x),
                180 => (width - 1 - x, height - 1 - y),
                270 => (width - 1 - y, x),
                _ => (x, y),
            };
            let g = luma.get(sy * width + sx).copied().unwrap_or(0) as u32;
            pixels.push(0xff00_0000 | (g << 16) | (g << 8) | g);
        }
    }
    pixels
}

/// Share of a `tex_w`×`tex_h` image visible on each axis when it covers a
/// `view_w`×`view_h` view (BoxFit.cover).
fn cover_crop(tex_w: f64, tex_h: f64, view_w: f64, view_h: f64) -> (f64, f64) {
    if tex_w <= 0.0 || tex_h <= 0.0 || view_w <= 0.0 || view_h <= 0.0 {
        return (1.0, 1.0);
    }
    let (tex, view) = (tex_w / tex_h, view_w / view_h);
    if tex > view { (view / tex, 1.0) } else { (1.0, tex / view) }
}

/// Android: Camera2 characteristics through the Java bridge. Makepad names
/// each input by the Debug form of its NDK camera id.
#[cfg(target_os = "android")]
mod camera {
    use super::{CameraMount, PreviewTransform};
    use makepad_widgets::{LiveId, VideoInputDesc};
    use serde_json::{json, Value};

    pub(super) fn mounts(descs: &[VideoInputDesc]) -> Vec<Option<CameraMount>> {
        let cameras = match crate::android::call("cameraGeometry", json!({})) {
            Ok(Value::Array(cameras)) => cameras,
            _ => Vec::new(),
        };
        descs
            .iter()
            .map(|desc| {
                cameras.iter().find_map(|camera| {
                    let id = std::ffi::CString::new(camera.get("id")?.as_str()?).ok()?;
                    (desc.input_id.0 == LiveId::from_str(&format!("{:?}", id.as_c_str()))).then(|| CameraMount {
                        front: camera.get("front").and_then(Value::as_bool).unwrap_or(false),
                        sensor: (camera.get("sensor").and_then(Value::as_u64).unwrap_or(0) % 360) as u16,
                    })
                })
            })
            .collect()
    }

    /// Camera2 display orientation: back `(sensor - display)`, front
    /// `(sensor + display)` then mirrored.
    pub(super) fn transform(mount: CameraMount) -> PreviewTransform {
        let display = match crate::android::call("displayRotation", json!({})) {
            Ok(value) => (value.as_u64().unwrap_or(0) % 360) as u16,
            Err(_) => 0,
        };
        if mount.front {
            PreviewTransform { rotation: (mount.sensor + display) % 360, mirror: true }
        } else {
            PreviewTransform { rotation: (mount.sensor + 360 - display) % 360, mirror: false }
        }
    }
}

/// iOS: the same AVCaptureDeviceDiscoverySession query Makepad runs, in the
/// same order, for each device's position. Buffers arrive in the sensor's
/// landscape orientation (home button on the right).
#[cfg(target_os = "ios")]
mod camera {
    use super::{CameraMount, PreviewTransform};
    use crate::apple::{ns_string, rust_string, Id};
    use makepad_widgets::{LiveId, VideoInputDesc};
    use objc2::{class, msg_send};

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeVideo: Id;
    }

    /// Makepad 1.0.0 `av_capture.rs` device types, in its order.
    const DEVICE_TYPES: [&str; 9] = [
        "AVCaptureDeviceTypeBuiltInDualCamera",
        "AVCaptureDeviceTypeBuiltInDualWideCamera",
        "AVCaptureDeviceTypeBuiltInTripleCamera",
        "AVCaptureDeviceTypeBuiltInWideAngleCamera",
        "AVCaptureDeviceTypeBuiltInUltraWideCamera",
        "AVCaptureDeviceTypeBuiltInTelephotoCamera",
        "AVCaptureDeviceTypeBuiltInTrueDepthCamera",
        "AVCaptureDeviceTypeExternal",
        "AVCaptureDeviceTypeContinuityCamera",
    ];
    /// `AVCaptureDevicePositionFront`.
    const POSITION_FRONT: isize = 2;

    pub(super) fn mounts(descs: &[VideoInputDesc]) -> Vec<Option<CameraMount>> {
        unsafe {
            let types: Id = msg_send![class!(NSMutableArray), array];
            for kind in DEVICE_TYPES {
                let _: () = msg_send![types, addObject: ns_string(kind)];
            }
            let session: Id = msg_send![
                class!(AVCaptureDeviceDiscoverySession),
                discoverySessionWithDeviceTypes: types,
                mediaType: AVMediaTypeVideo,
                position: 0isize
            ];
            let devices: Id = msg_send![session, devices];
            let count: usize = msg_send![devices, count];
            descs
                .iter()
                .enumerate()
                .map(|(index, desc)| {
                    if index >= count {
                        return None;
                    }
                    let device: Id = msg_send![devices, objectAtIndex: index];
                    let model = rust_string(msg_send![device, modelID])?;
                    let position: isize = msg_send![device, position];
                    (desc.input_id.0 == LiveId::from_str(&model)).then_some(CameraMount { front: position == POSITION_FRONT, sensor: 90 })
                })
                .collect()
        }
    }

    /// Apple's EXIF orientation for live capture by interface orientation:
    /// back camera portrait `.right`, upside down `.left`, home button right
    /// `.up`, left `.down`; the front camera is the mirrored counterpart
    /// (`.leftMirrored`, `.rightMirrored`, `.downMirrored`, `.upMirrored`).
    pub(super) fn transform(mount: CameraMount) -> PreviewTransform {
        let orientation: isize = unsafe {
            let app: Id = msg_send![class!(UIApplication), sharedApplication];
            msg_send![app, statusBarOrientation]
        };
        let back = match orientation {
            2 => (mount.sensor + 180) % 360, // portrait upside down
            3 => (mount.sensor + 270) % 360, // landscape right (home button right)
            4 => (mount.sensor + 90) % 360,  // landscape left (home button left)
            _ => mount.sensor,               // portrait
        };
        if mount.front {
            PreviewTransform { rotation: (540 - back) % 360, mirror: true }
        } else {
            PreviewTransform { rotation: back, mirror: false }
        }
    }
}

/// Desktop clients pair by link; the scanner reports `Unavailable` before a
/// camera opens.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
mod camera {
    use super::{CameraMount, PreviewTransform};
    use makepad_widgets::VideoInputDesc;

    pub(super) fn mounts(descs: &[VideoInputDesc]) -> Vec<Option<CameraMount>> {
        vec![None; descs.len()]
    }

    pub(super) fn transform(_mount: CameraMount) -> PreviewTransform {
        PreviewTransform::default()
    }
}

impl QrScanViewRef {
    pub fn start(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.start(cx);
        }
    }
    pub fn stop(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.stop(cx);
        }
    }
    pub fn resume_scanning(&self, cx: &mut Cx) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.resume_scanning(cx);
        }
    }
    pub fn state(&self) -> QrScanState {
        self.borrow().map(|inner| inner.state()).unwrap_or(QrScanState::Idle)
    }
}

/// Luminance plane of a camera frame (row-packed, `width * height` bytes).
fn luminance(buffer: &VideoBufferRef) -> Option<(usize, usize, Vec<u8>)> {
    let format = buffer.format;
    let (width, height) = (format.width, format.height);
    if width == 0 || height == 0 {
        return None;
    }
    let bytes: &[u8] = match &buffer.data {
        VideoBufferRefData::U8(data) => data,
        VideoBufferRefData::U32(data) => unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 4) },
    };
    let mut luma = vec![0u8; width * height];
    match format.pixel_format {
        // Android YUV_420_888: plane 0 is Y with a row stride ≥ width.
        VideoPixelFormat::YUV420 | VideoPixelFormat::GRAY | VideoPixelFormat::NV12 => {
            let stride = bytes.len().div_ceil(height);
            let stride = stride.max(width);
            for y in 0..height {
                let start = y * stride;
                let end = (start + width).min(bytes.len());
                if start >= end {
                    break;
                }
                luma[y * width..y * width + (end - start)].copy_from_slice(&bytes[start..end]);
            }
        }
        // Packed 4:2:2 (UYVY or YUYV; Makepad labels both YUY2): the luminance
        // byte of each pair varies far more than chroma.
        VideoPixelFormat::YUY2 => {
            let pairs = (bytes.len() / 2).min(4096);
            let spread = |offset: usize| -> u64 { (0..pairs).map(|i| (bytes[i * 2 + offset] as i64 - 128).unsigned_abs()).sum() };
            let offset = if spread(1) > spread(0) { 1 } else { 0 };
            let stride = (bytes.len() / height).max(width * 2);
            for y in 0..height {
                for x in 0..width {
                    luma[y * width + x] = bytes.get(y * stride + x * 2 + offset).copied().unwrap_or(0);
                }
            }
        }
        VideoPixelFormat::RGB24 => {
            for (i, pixel) in luma.iter_mut().enumerate() {
                let (r, g, b) = (bytes.get(i * 3).copied()?, bytes.get(i * 3 + 1).copied()?, bytes.get(i * 3 + 2).copied()?);
                *pixel = ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as u8;
            }
        }
        VideoPixelFormat::MJPEG | VideoPixelFormat::Unsupported(_) => return None,
    }
    Some((width, height, luma))
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn decode(width: usize, height: usize, luma: &[u8]) -> Option<String> {
    let mut image = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| luma[y * width + x]);
    image
        .detect_grids()
        .into_iter()
        .filter_map(|grid| grid.decode().ok().map(|(_, content)| content))
        .find_map(|content| crate::deep_link::pairing_link_from_scan(&content))
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn decode(_width: usize, _height: usize, _luma: &[u8]) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Grey levels of a transformed frame, row by row.
    fn levels(width: usize, height: usize, rotation: u16, mirror: bool) -> Vec<u8> {
        let luma: Vec<u8> = (0..(width * height) as u8).collect();
        transform_luma(width, height, &luma, PreviewTransform { rotation, mirror }).into_iter().map(|p| p as u8).collect()
    }

    // Source 3×2:  0 1 2
    //              3 4 5
    #[test]
    fn rotations_turn_the_frame_clockwise() {
        assert_eq!(levels(3, 2, 0, false), [0, 1, 2, 3, 4, 5]);
        // Top row becomes the right column.
        assert_eq!(levels(3, 2, 90, false), [3, 0, 4, 1, 5, 2]);
        assert_eq!(levels(3, 2, 180, false), [5, 4, 3, 2, 1, 0]);
        // Top row becomes the left column, read bottom-up.
        assert_eq!(levels(3, 2, 270, false), [2, 5, 1, 4, 0, 3]);
    }

    #[test]
    fn mirroring_follows_rotation() {
        assert_eq!(levels(3, 2, 0, true), [2, 1, 0, 5, 4, 3]);
        // 90° then mirror is a transpose (EXIF `.leftMirrored`, iOS front camera in portrait).
        assert_eq!(levels(3, 2, 90, true), [0, 3, 1, 4, 2, 5]);
        // 180° then mirror is a vertical flip (`.downMirrored`).
        assert_eq!(levels(3, 2, 180, true), [3, 4, 5, 0, 1, 2]);
        // 270° then mirror is a transverse (`.rightMirrored`).
        assert_eq!(levels(3, 2, 270, true), [5, 2, 4, 1, 3, 0]);
    }

    #[test]
    fn cover_crops_the_longer_axis() {
        // 720×1280 portrait frame in a wider 1080×1440 view: top and bottom are cut.
        let (x, y) = cover_crop(720.0, 1280.0, 1080.0, 1440.0);
        assert_eq!(x, 1.0);
        assert!((y - (720.0 / 1280.0) / (1080.0 / 1440.0)).abs() < 1e-9);
        // A wider frame in a narrower view: the sides are cut.
        let (x, y) = cover_crop(1280.0, 720.0, 720.0, 1280.0);
        assert!((x - (720.0 / 1280.0) / (1280.0 / 720.0)).abs() < 1e-9);
        assert_eq!(y, 1.0);
        assert_eq!(cover_crop(0.0, 720.0, 100.0, 100.0), (1.0, 1.0));
    }
}
