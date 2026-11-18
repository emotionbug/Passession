//! Main-thread ownership of retained Metal window buffers.
use super::macos_app::post_surface_ready;
use crate::{
    damage::{DamageFrame, DamageHistory, DamageRect},
    frame_trace::FrameTrace,
    makepad_math::Vec4,
    os::apple::{
        apple_sys::*, apple_util::str_to_nsstring,
        metal::MetalRwLockGpuReadGuard,
    },
    texture::TextureId,
};
use std::{sync::{Arc, LazyLock, Mutex}, time::{Duration, Instant}};

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    static kIOSurfaceWidth: ObjcId;
    static kIOSurfaceHeight: ObjcId;
    static kIOSurfacePixelFormat: ObjcId;
    static kIOSurfaceBytesPerElement: ObjcId;
    static kIOSurfaceBytesPerRow: ObjcId;
    static kIOSurfaceAllocSize: ObjcId;
    static kIOSurfaceColorSpace: ObjcId;
    fn IOSurfaceAlignProperty(property: ObjcId, value: usize) -> usize;
    fn IOSurfaceCreate(properties: ObjcId) -> ObjcId;
    fn IOSurfaceSetValue(surface: ObjcId, key: ObjcId, value: ObjcId);
    fn IOSurfaceIsInUse(surface: ObjcId) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: ObjcId);
}

struct OwnedCf(NonNull<Object>);

impl OwnedCf {
    fn as_id(&self) -> ObjcId { self.0.as_ptr() }
}

impl Drop for OwnedCf {
    fn drop(&mut self) { unsafe { CFRelease(self.as_id()); } }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> ObjcId;
    fn CGColorSpaceCopyPropertyList(space: ObjcId) -> ObjcId;
}

#[link(name = "QuartzCore", kind = "framework")]
extern "C" {
    static kCAGravityTopLeft: ObjcId;
}

static ORACLE: LazyLock<bool> = LazyLock::new(|| {
    std::env::var_os("PASEO_DAMAGE_ORACLE").is_some_and(|value| value == "1")
});

struct Surface {
    io: OwnedCf,
    texture: RcObjcId,
    presented: u64,
}

unsafe fn texture_descriptor(size: [i32; 2], format: MTLPixelFormat, storage: MTLStorageMode) -> ObjcId {
    let descriptor: ObjcId = msg_send![
        class!(MTLTextureDescriptor), texture2DDescriptorWithPixelFormat: format
        width: size[0] as usize height: size[1] as usize mipmapped: NO
    ];
    let () = msg_send![descriptor, setUsage: MTLTextureUsage::RenderTarget];
    let () = msg_send![descriptor, setStorageMode: storage];
    descriptor
}

impl Surface {
    fn new(device: ObjcId, size: [i32; 2], profile: ObjcId, descriptor: ObjcId) -> Option<Self> {
        unsafe {
            let [width, height] = size.map(|axis| axis as usize);
            let stride = IOSurfaceAlignProperty(kIOSurfaceBytesPerRow, width.checked_mul(4)?);
            let allocation = IOSurfaceAlignProperty(kIOSurfaceAllocSize, height.checked_mul(stride)?);
            let properties: ObjcId = msg_send![class!(NSMutableDictionary), dictionaryWithCapacity: 6usize];
            for (key, value) in [
                (kIOSurfaceWidth, width), (kIOSurfaceHeight, height),
                (kIOSurfacePixelFormat, u32::from_be_bytes(*b"BGRA") as usize),
                (kIOSurfaceBytesPerElement, 4), (kIOSurfaceBytesPerRow, stride),
                (kIOSurfaceAllocSize, allocation),
            ] {
                let number: ObjcId = msg_send![class!(NSNumber), numberWithUnsignedInteger: value];
                let () = msg_send![properties, setObject: number forKey: key];
            }
            let io = OwnedCf(NonNull::new(IOSurfaceCreate(properties))?);
            IOSurfaceSetValue(io.as_id(), kIOSurfaceColorSpace, profile);
            let texture = RcObjcId::from_owned(NonNull::new(msg_send![
                device, newTextureWithDescriptor: descriptor iosurface: io.as_id() plane: 0usize
            ])?);
            Some(Self { io, texture, presented: 0 })
        }
    }

    fn in_use(&self) -> bool {
        unsafe { IOSurfaceIsInUse(self.io.as_id()) != 0 }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) struct SurfaceKey {
    pub(crate) device: ObjcId,
    pub(crate) size: [i32; 2],
    pub(crate) scale: f64,
    pub(crate) screen: ObjcId,
}

pub(crate) struct SurfacePresenter {
    layer: RcObjcId,
    clear: Option<RepairClear>,
    ring: Option<Ring>,
    history: DamageHistory,
    scene: DamageFrame,
    /// Retry surface ownership only at the display cadence, not on every paint/timer.
    waiting_since: Option<Instant>,
    deferred: bool,
    retry: Option<(SurfaceKey, Instant)>,
    partial_failure: bool,
}

struct Ring {
    key: SurfaceKey,
    storage: MTLStorageMode,
    surfaces: [Option<Surface>; 3],
    descriptor: RcObjcId,
    profile: OwnedCf,
    next: usize,
    front: Option<usize>,
    presented: u64,
    observed: u64,
    pending: Option<Pending>,
    depth_baseline: Option<(TextureId, u64)>,
    completion: Arc<SurfaceCompletion>,
    oracle: Option<Oracle>,
    fallback_depth: Option<RcObjcId>,
}

#[derive(Clone, Copy)]
struct Pending {
    slot: usize,
    age: i32,
    logical: DamageRect,
    repair: DamageRect,
    oracle: bool,
    depth_baseline: Option<(TextureId, u64)>,
    started: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct SurfaceCompletion(Mutex<CompletionState>);

#[derive(Default)]
struct CompletionState {
    success: Option<bool>,
    guards: Vec<MetalRwLockGpuReadGuard>,
}

impl SurfaceCompletion {
    pub(crate) fn complete(&self, success: bool, mut guards: Vec<MetalRwLockGpuReadGuard>, wake: bool) {
        guards.clear(); // Release GPU reads before allowing further work on the UI thread.
        *self.0.lock().unwrap() = CompletionState { success: Some(success), guards };
        if wake { post_surface_ready(); }
    }
}

impl SurfacePresenter {
    pub(crate) fn new(view: ObjcId) -> Self {
        let layer = unsafe {
            let layer = RcObjcId::from_owned(NonNull::new(msg_send![class!(CALayer), new]).unwrap());
            let () = msg_send![layer.as_id(), setDelegate: view];
            // Keep stale contents at native size if rendering must be retried.
            let () = msg_send![layer.as_id(), setContentsGravity: kCAGravityTopLeft];
            let () = msg_send![layer.as_id(), setNeedsDisplayOnBoundsChange: YES];
            let () = msg_send![layer.as_id(), setAutoresizingMask: (1usize << 4) | (1 << 1)];
            if let Some(background) = NonNull::new(CGColorCreateGenericRGB(0.0, 0.0, 0.0, 1.0)) {
                let background = OwnedCf(background);
                let () = msg_send![layer.as_id(), setBackgroundColor: background.as_id()];
            }
            let () = msg_send![view, setLayer: layer.as_id()];
            let () = msg_send![view, setWantsLayer: YES];
            let () = msg_send![view, setLayerContentsPlacement: 11usize]; // TopLeft
            layer
        };
        Self {
            layer, clear: None, ring: None, history: DamageHistory::default(),
            scene: DamageFrame::default(), waiting_since: None, deferred: false,
            retry: None, partial_failure: false,
        }
    }

    pub(crate) fn invalidate(&mut self) {
        // CA retains its current contents; Metal retains encoded textures until command
        // completion. Neither the detached ring nor its completion can be published again.
        self.ring = None;
        self.history.invalidate();
        self.scene.invalidate();
        self.waiting_since = None;
        self.deferred = false;
    }

    pub(crate) fn retry_frame(&mut self, key: SurfaceKey, reason: &'static str) {
        if self.retry.is_none() {
            eprintln!("paseo_metal_surface retry: {reason}");
        }
        self.retry = Some((key, Instant::now() + Duration::from_millis(16)));
        if let Some(ring) = &mut self.ring {
            ring.pending = None;
            ring.depth_baseline = None;
        }
        self.history.invalidate();
        self.scene.invalidate();
    }

    pub(crate) fn retry_present(&mut self, reason: &'static str) {
        self.retry_frame(self.ring.as_ref().unwrap().key, reason);
    }

    fn disable_partial(&mut self, reason: &'static str) {
        if !std::mem::replace(&mut self.partial_failure, true) {
            eprintln!("paseo_metal_surface full repair: {reason}");
        }
    }

    pub(crate) fn partial_available(&self) -> bool { !self.partial_failure }

    pub(crate) fn fallback_depth(&mut self) -> Option<ObjcId> {
        let ring = self.ring.as_mut().unwrap();
        if ring.fallback_depth.is_none() {
            unsafe {
                let descriptor = texture_descriptor(ring.key.size, MTLPixelFormat::Depth32Float, MTLStorageMode::Private);
                ring.fallback_depth = Some(RcObjcId::from_owned(NonNull::new(msg_send![
                    ring.key.device, newTextureWithDescriptor: descriptor
                ])?));
            }
        }
        ring.fallback_depth.as_ref().map(RcObjcId::as_id)
    }

    pub(crate) fn detach(&self) {
        transaction(|| unsafe {
            let () = msg_send![self.layer.as_id(), setDelegate: nil];
            let () = msg_send![self.layer.as_id(), setContents: nil];
        });
    }

    pub(crate) fn needs_display_tick(&self) -> bool { self.waiting_since.is_some() }

    pub(crate) fn acquire(
        &mut self, key: SurfaceKey, display_tick: bool,
        visible: bool, trace: Option<&FrameTrace>,
    ) -> Result<Option<usize>, &'static str> {
        if self.retry.is_some_and(|(failed_key, ready)| failed_key == key && Instant::now() < ready) {
            return Ok(None);
        }
        if self.ring.as_ref().is_none_or(|ring| ring.key != key) {
            self.invalidate();
            if key.screen == nil { return Ok(None); }
            self.ring = Some(Ring::new(key).ok_or("IOSurface ring allocation failed")?);
        }
        let ring = self.ring.as_mut().unwrap();
        if ring.pending.is_some() {
            if !self.deferred {
                self.deferred = true;
                if let Some(trace) = trace { trace.surface_deferred(false); }
            }
            return Ok(None);
        }
        if let Some(start) = self.waiting_since {
            if !visible {
                self.waiting_since = Some(Instant::now());
                return Ok(None);
            }
            if !display_tick { return Ok(None); }
            if start.elapsed() > Duration::from_millis(250) {
                // Replace the ring without touching any quarantined slot. CA
                // still owns the old contents until it releases them itself.
                self.ring = None;
                return Err("Core Animation did not release a reusable surface");
            }
        }
        if let Some(front) = ring.front {
            let front = ring.surfaces[front].as_ref().unwrap();
            if front.presented > ring.observed && front.in_use() {
                ring.observed = front.presented;
            }
        }
        for offset in 0..ring.surfaces.len() {
            let index = (ring.next + offset) % ring.surfaces.len();
            // A newer front must have registered use before a previous front can
            // be reclaimed. A false use count alone can mean CA hasn't latched it
            // yet. Once a successor is in use, CA no longer needs to acquire any
            // older content from this layer; ongoing use is still checked below.
            if Some(index) != ring.front
                && ring.surfaces[index].as_ref().is_none_or(|surface| {
                    (surface.presented == 0 || surface.presented < ring.observed) && !surface.in_use()
                })
            {
                ring.ensure_surface(index).ok_or("IOSurface slot allocation failed")?;
                ring.next = (index + 1) % ring.surfaces.len();
                self.waiting_since = None;
                self.deferred = false;
                return Ok(Some(index));
            }
        }
        self.waiting_since.get_or_insert_with(Instant::now);
        if let Some(trace) = trace { trace.surface_deferred(true); }
        Ok(None)
    }

    pub(crate) fn scene_mut(&mut self) -> &mut DamageFrame { &mut self.scene }

    pub(crate) fn depth_baseline(&self) -> Option<(TextureId, u64)> {
        self.ring.as_ref()?.depth_baseline
    }

    pub(crate) fn prepare(
        &mut self, slot: usize, logical: DamageRect, depth_baseline: Option<(TextureId, u64)>,
        full_repair: bool, trace: bool,
    ) -> SurfacePaint<'_> {
        let ring = self.ring.as_ref().unwrap();
        let key = ring.key;
        let full = DamageRect::full(key.size);
        let presented = ring.surfaces[slot].as_ref().unwrap().presented;
        let age = if presented == 0 { 0 } else { i32::try_from(ring.presented - presented + 1).unwrap_or(0) };
        let mut repair = if full_repair || !self.partial_available() {
            full
        } else {
            self.history.repair(age, logical).unwrap_or(full)
        };
        if repair != full && self.clear.as_ref().is_none_or(|clear| clear.device != key.device) {
            self.clear = RepairClear::new(key.device);
            if self.clear.is_none() {
                self.disable_partial("Metal scissored clear unavailable");
                repair = full;
            }
        }
        if repair != full && *ORACLE && self.ring.as_ref().unwrap().oracle.is_none() {
            let oracle = Oracle::new(key.device, key.size);
            if oracle.is_none() {
                self.disable_partial("Metal damage oracle allocation failed");
                repair = full;
            }
            self.ring.as_mut().unwrap().oracle = oracle;
        }
        let ring = self.ring.as_mut().unwrap();
        let partial = repair != full;
        let oracle = if partial { ring.oracle.as_ref() } else { None };
        ring.pending = Some(Pending {
            slot, age, logical, repair, oracle: oracle.is_some(), depth_baseline, started: trace.then(Instant::now),
        });
        SurfacePaint {
            texture: ring.surfaces[slot].as_ref().unwrap().texture.as_id(),
            storage: ring.storage,
            repair: partial.then(|| (&self.scene, repair, self.clear.as_ref().unwrap())),
            oracle,
            completion: Arc::clone(&ring.completion),
            guards: std::mem::take(&mut ring.completion.0.lock().unwrap().guards),
        }
    }

    /// Call only on AppKit's thread. A command buffer's *scheduled* notification
    /// is not a texture fence. Completion guarantees shared texture visibility.
    pub(crate) fn present(&mut self, view: ObjcId, trace: Option<&FrameTrace>) -> Result<bool, &'static str> {
        let Some(ring) = self.ring.as_mut() else { return Ok(false); };
        let Some(success) = ring.completion.0.lock().unwrap().success.take() else { return Ok(false); };
        let frame = ring.pending.take().unwrap();
        self.deferred = false;
        if !success { return Err("Metal window command buffer failed"); }
        if frame.oracle {
            if let Err(reason) = ring.oracle.as_ref().unwrap().check(ring.presented + 1, ring.key.size, frame) {
                self.partial_failure = true;
                return Err(reason);
            }
        } else if *ORACLE {
            eprintln!("paseo_metal_damage frame={} result=full age={} size={:?}", ring.presented + 1, frame.age, ring.key.size);
        }
        let surface = ring.surfaces[frame.slot].as_mut().unwrap();
        transaction(|| unsafe {
            if ring.front.is_none() {
                let () = msg_send![self.layer.as_id(), setContentsScale: ring.key.scale];
                let bounds: NSRect = msg_send![view, bounds];
                let () = msg_send![self.layer.as_id(), setFrame: bounds];
            }
            let () = msg_send![self.layer.as_id(), setContents: surface.io.as_id()];
        });
        ring.presented += 1;
        surface.presented = ring.presented;
        ring.front = Some(frame.slot);
        self.scene.commit();
        ring.depth_baseline = frame.depth_baseline;
        self.history.push(frame.logical);
        self.retry = None;
        if let (Some(trace), Some(started)) = (trace, frame.started) {
            trace.surface_presented(ring.key.size, frame.repair, started.elapsed());
        }
        Ok(true)
    }
}

pub(crate) fn transaction(update: impl FnOnce()) {
    unsafe {
        let transaction = class!(CATransaction);
        let () = msg_send![transaction, begin];
        let () = msg_send![transaction, setDisableActions: YES];
        update();
        let () = msg_send![transaction, commit];
    }
}

impl Ring {
    fn new(key: SurfaceKey) -> Option<Self> {
        unsafe {
            if key.size.iter().any(|&axis| !(1..=16384).contains(&axis)) { return None; }
            let unified: BOOL = msg_send![key.device, hasUnifiedMemory];
            let storage = if unified == YES { MTLStorageMode::Shared } else { MTLStorageMode::Managed };
            // Match CAMetalLayer.colorspace == nil: the shader's SDR channel
            // values are unmanaged device RGB. A calibrated window/screen profile
            // reinterprets those values and visibly changes colors on a P3 screen.
            let space = OwnedCf(NonNull::new(CGColorSpaceCreateDeviceRGB())?);
            let profile = OwnedCf(NonNull::new(CGColorSpaceCopyPropertyList(space.as_id()))?);
            let descriptor = RcObjcId::from_unowned(NonNull::new(texture_descriptor(
                key.size, MTLPixelFormat::BGRA8Unorm, storage,
            ))?);
            Some(Self {
                key, storage, profile, descriptor, surfaces: [None, None, None], next: 0, front: None,
                presented: 0, observed: 0, pending: None, depth_baseline: None,
                completion: Arc::default(), oracle: None, fallback_depth: None,
            })
        }
    }

    fn ensure_surface(&mut self, index: usize) -> Option<&Surface> {
        let slot = &mut self.surfaces[index];
        if slot.is_none() {
            // Resize steps typically publish only once at a given size. Allocate
            // the next slot only if another frame actually needs that backing.
            *slot = Some(Surface::new(self.key.device, self.key.size, self.profile.as_id(), self.descriptor.as_id())?);
        }
        slot.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" {
        fn IOSurfaceCopyValue(surface: ObjcId, key: ObjcId) -> ObjcId;
        fn CGColorSpaceCreateWithPropertyList(profile: ObjcId) -> ObjcId;
        fn CFEqual(left: ObjcId, right: ObjcId) -> u8;
    }

    #[test]
    fn ring_surfaces_preserve_unmanaged_device_rgb() {
        unsafe {
            let _pool = RcObjcId::from_owned(
                NonNull::new(msg_send![class!(NSAutoreleasePool), new]).unwrap(),
            );
            let device = RcObjcId::from_owned(
                NonNull::new(MTLCreateSystemDefaultDevice()).expect("Native Metal device required"),
            );
            let mut ring = Ring::new(SurfaceKey {
                device: device.as_id(), size: [4, 4], scale: 1.0, screen: nil,
            }).expect("Create IOSurface ring");
            let expected_space = OwnedCf(NonNull::new(CGColorSpaceCreateDeviceRGB()).unwrap());
            let expected = OwnedCf(NonNull::new(CGColorSpaceCopyPropertyList(
                expected_space.as_id(),
            )).unwrap());
            // Inspect the serialized metadata on every real IOSurface. Shader
            // readback alone cannot detect a compositor color-space conversion.
            for index in 0..ring.surfaces.len() {
                let surface = ring.ensure_surface(index).expect("Allocate IOSurface slot");
                let profile = OwnedCf(NonNull::new(IOSurfaceCopyValue(
                    surface.io.as_id(), kIOSurfaceColorSpace,
                )).expect("IOSurface color-space metadata"));
                let _space = OwnedCf(NonNull::new(CGColorSpaceCreateWithPropertyList(
                    profile.as_id(),
                )).expect("Decode IOSurface color space"));
                // CoreGraphics may canonicalize Device RGB when serializing it;
                // the IOSurface contract is the serialized value, not CFEqual on
                // the original and reconstructed CGColorSpace objects.
                assert_ne!(CFEqual(profile.as_id(), expected.as_id()), 0,
                    "The compositor must receive unmanaged device RGB, not a display profile");
            }
        }
    }
}

pub(crate) struct SurfacePaint<'a> {
    pub(crate) texture: ObjcId,
    pub(crate) storage: MTLStorageMode,
    pub(crate) repair: Option<(&'a DamageFrame, DamageRect, &'a RepairClear)>,
    pub(crate) oracle: Option<&'a Oracle>,
    pub(crate) completion: Arc<SurfaceCompletion>,
    pub(crate) guards: Vec<MetalRwLockGpuReadGuard>,
}

pub(crate) struct RepairClear {
    device: ObjcId,
    pipeline: RcObjcId,
    depth: RcObjcId,
}

#[repr(C)]
struct Scissor {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

impl RepairClear {
    fn new(device: ObjcId) -> Option<Self> {
        const SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;
vertex float4 repair_vertex(uint id [[vertex_id]], constant float &depth [[buffer(0)]]) {
    const float2 vertices[] = {float2(-1., -1.), float2(3., -1.), float2(-1., 3.)};
    return float4(vertices[id], depth, 1.);
}
fragment float4 repair_fragment(constant float4 &color [[buffer(0)]]) { return color; }
"#;
        unsafe {
            let library = RcObjcId::from_owned(NonNull::new(msg_send![
                device, newLibraryWithSource: str_to_nsstring(SOURCE) options: nil error: std::ptr::null_mut::<ObjcId>()
            ])?);
            let vertex = RcObjcId::from_owned(NonNull::new(msg_send![library.as_id(), newFunctionWithName: str_to_nsstring("repair_vertex")])?);
            let fragment = RcObjcId::from_owned(NonNull::new(msg_send![library.as_id(), newFunctionWithName: str_to_nsstring("repair_fragment")])?);
            let desc = RcObjcId::from_owned(NonNull::new(msg_send![class!(MTLRenderPipelineDescriptor), new])?);
            let () = msg_send![desc.as_id(), setVertexFunction: vertex.as_id()];
            let () = msg_send![desc.as_id(), setFragmentFunction: fragment.as_id()];
            let () = msg_send![desc.as_id(), setDepthAttachmentPixelFormat: MTLPixelFormat::Depth32Float];
            let colors: ObjcId = msg_send![desc.as_id(), colorAttachments];
            let color: ObjcId = msg_send![colors, objectAtIndexedSubscript: 0usize];
            let () = msg_send![color, setPixelFormat: MTLPixelFormat::BGRA8Unorm];
            let pipeline = RcObjcId::from_owned(NonNull::new(msg_send![
                device, newRenderPipelineStateWithDescriptor: desc.as_id() error: std::ptr::null_mut::<ObjcId>()
            ])?);
            let desc = RcObjcId::from_owned(NonNull::new(msg_send![class!(MTLDepthStencilDescriptor), new])?);
            let () = msg_send![desc.as_id(), setDepthCompareFunction: MTLCompareFunction::Always];
            let () = msg_send![desc.as_id(), setDepthWriteEnabled: YES];
            let depth = RcObjcId::from_owned(NonNull::new(msg_send![device, newDepthStencilStateWithDescriptor: desc.as_id()])?);
            Some(Self { device, pipeline, depth })
        }
    }

    pub(crate) fn encode(&self, encoder: ObjcId, rect: DamageRect, height: i32, color: Vec4, depth: f32) {
        let [x, _, width, height_px] = rect.egl();
        let scissor = Scissor { x: x as usize, y: (height - rect.y1) as usize,
            width: width as usize, height: height_px as usize };
        let color = [color.x, color.y, color.z, color.w];
        unsafe {
            let () = msg_send![encoder, setScissorRect: scissor];
            let () = msg_send![encoder, setRenderPipelineState: self.pipeline.as_id()];
            let () = msg_send![encoder, setDepthStencilState: self.depth.as_id()];
            let () = msg_send![encoder, setVertexBytes: &depth as *const f32 length: 4usize atIndex: 0usize];
            let () = msg_send![encoder, setFragmentBytes: color.as_ptr() length: 16usize atIndex: 0usize];
            let () = msg_send![encoder, drawPrimitives: MTLPrimitiveType::Triangle vertexStart: 0usize vertexCount: 3usize];
        }
    }
}

pub(crate) struct Oracle {
    pub(crate) color: RcObjcId,
    pub(crate) depth: RcObjcId,
    readback: RcObjcId,
    stride: usize,
    image_bytes: usize,
}

impl Oracle {
    fn new(device: ObjcId, size: [i32; 2]) -> Option<Self> {
        let [width, height] = size.map(|axis| axis as usize);
        let stride = width.checked_mul(4)?.next_multiple_of(256);
        let image_bytes = stride.checked_mul(height)?;
        unsafe {
            let texture = |format| {
                let descriptor = texture_descriptor(size, format, MTLStorageMode::Private);
                NonNull::new(msg_send![device, newTextureWithDescriptor: descriptor]).map(RcObjcId::from_owned)
            };
            let readback = RcObjcId::from_owned(NonNull::new(msg_send![
                device, newBufferWithLength: image_bytes.checked_mul(2)? options: MTLResourceOptions::StorageModeShared
            ])?);
            Some(Self { color: texture(MTLPixelFormat::BGRA8Unorm)?, depth: texture(MTLPixelFormat::Depth32Float)?,
                readback, stride, image_bytes })
        }
    }

    pub(crate) fn encode_readback(&self, command: ObjcId, candidate: ObjcId, size: [i32; 2]) -> bool {
        unsafe {
            let blit: ObjcId = msg_send![command, blitCommandEncoder];
            if blit == nil { return false; }
            for (texture, offset) in [(candidate, 0), (self.color.as_id(), self.image_bytes)] {
                let () = msg_send![blit, copyFromTexture: texture sourceSlice: 0usize sourceLevel: 0usize
                    sourceOrigin: MTLOrigin {x: 0, y: 0, z: 0}
                    sourceSize: MTLSize {width: size[0] as u64, height: size[1] as u64, depth: 1}
                    toBuffer: self.readback.as_id() destinationOffset: offset
                    destinationBytesPerRow: self.stride destinationBytesPerImage: self.image_bytes];
            }
            let () = msg_send![blit, endEncoding];
            true
        }
    }

    fn check(&self, frame: u64, size: [i32; 2], pending: Pending) -> Result<(), &'static str> {
        let contents: *const u8 = unsafe { msg_send![self.readback.as_id(), contents] };
        if contents.is_null() { return Err("Metal damage oracle readback unavailable"); }
        // GPU completion precedes this access; a later frame cannot be encoded until
        // it returns. Ignore row padding, which Metal doesn't promise to initialize.
        let (candidate, reference) = unsafe {
            std::slice::from_raw_parts(contents, self.image_bytes * 2).split_at(self.image_bytes)
        };
        let mut mismatches = 0usize;
        let mut first = None;
        let width = size[0] as usize * 4;
        for (y, (actual, expected)) in candidate.chunks_exact(self.stride).zip(reference.chunks_exact(self.stride)).enumerate() {
            if actual[..width] == expected[..width] { continue; }
            for (x, (a, b)) in actual[..width].chunks_exact(4).zip(expected[..width].chunks_exact(4)).enumerate() {
                if a != b {
                    first.get_or_insert((x, y));
                    mismatches += 1;
                }
            }
        }
        let result = if mismatches == 0 { "equal" } else { "mismatch" };
        eprintln!("paseo_metal_damage frame={frame} result={result} age={} size={size:?} logical={:?} repair={:?} mismatched_pixels={mismatches} first_top_left={first:?}",
            pending.age, pending.logical.egl(), pending.repair.egl());
        if mismatches == 0 { Ok(()) } else { Err("Metal damage oracle mismatch") }
    }
}
