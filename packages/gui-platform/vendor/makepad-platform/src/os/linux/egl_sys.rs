#![allow(non_camel_case_types, non_snake_case, dead_code)]

pub type EGLNativeDisplayType = *mut ();
pub type EGLNativePixmapType = ::std::os::raw::c_ulong;
pub type EGLNativeWindowType = ::std::os::raw::c_ulong;

pub use core::ptr::null_mut;

pub const EGL_NO_CONTEXT: EGLContext = 0 as EGLContext;
pub const EGL_NO_SURFACE: EGLSurface = 0 as EGLSurface;

pub const EGL_WINDOW_BIT: u32 = 4;

pub const EGL_OPENGL_ES2_BIT: u32 = 4;

pub const EGL_SUCCESS: u32 = 12288;
pub const EGL_BAD_SURFACE: EGLint = 0x300D;
pub const EGL_CONTEXT_LOST: EGLint = 0x300E;
pub const EGL_VENDOR: EGLint = 0x3053;
pub const EGL_SAMPLE_BUFFERS: u32 = 0x3032;
pub const EGL_ALPHA_SIZE: u32 = 12321;
pub const EGL_BLUE_SIZE: u32 = 12322;
pub const EGL_GREEN_SIZE: u32 = 12323;
pub const EGL_RED_SIZE: u32 = 12324;
pub const EGL_DEPTH_SIZE: u32 = 12325;
pub const EGL_STENCIL_SIZE: u32 = 12326;
pub const EGL_NATIVE_VISUAL_ID: u32 = 12334;
pub const EGL_SURFACE_TYPE: u32 = 12339;
pub const EGL_NONE: u32 = 0x3038;
pub const EGL_RENDERABLE_TYPE: u32 = 12352;
pub const EGL_HEIGHT: u32 = 12374;
pub const EGL_WIDTH: u32 = 12375;
pub const EGL_CONTEXT_MAJOR_VERSION: u32 = 0x3098;
pub const EGL_CONTEXT_MINOR_VERSION_KHR: u32 = 0x30FB;
pub const EGL_OPENGL_ES_API: u32 = 12448;
pub const EGL_CONTEXT_OPENGL_PROFILE_MASK: u32 = 0x30FD;
pub const EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT: u32 = 0x0002;
pub const EGL_SAMPLES: u32 = 0x3031;
pub const EGL_OPENGL_ES3_BIT_KHR: u32 = 0x0040;
pub const EGL_COLOR_BUFFER_TYPE: u32 = 0x303F;
pub const EGL_RGB_BUFFER: u32 = 0x308E;
pub const EGL_PBUFFER_BIT: u32 = 0x0001;

pub const EGL_GL_TEXTURE_2D_KHR: u32 = 12465;

pub const EGL_PLATFORM_X11_EXT: u32 = 12757;
pub const EGL_PLATFORM_GBM_KHR: u32 = 12759;

pub const EGL_LINUX_DMA_BUF_EXT: u32 = 12912;
pub const EGL_LINUX_DRM_FOURCC_EXT: u32 = 12913;
pub const EGL_DMA_BUF_PLANE0_FD_EXT: u32 = 12914;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: u32 = 12915;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: u32 = 12916;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: u32 = 13379;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: u32 = 13380;
pub const EGL_SWAP_BEHAVIOR: i32 = 0x3093;
pub const EGL_BUFFER_PRESERVED: i32 = 0x3094;
pub const EGL_BUFFER_DESTROYED: i32 = 0x3095;
pub type NativeDisplayType = EGLNativeDisplayType;
pub type NativePixmapType = EGLNativePixmapType;
pub type NativeWindowType = EGLNativeWindowType;
pub type EGLint = i32;
pub type EGLuint64KHR = u64;
pub type EGLenum = ::std::os::raw::c_uint;
pub type EGLBoolean = ::std::os::raw::c_uint;
pub type EGLDisplay = *mut ::std::os::raw::c_void;
pub type EGLConfig = *mut ::std::os::raw::c_void;
pub type EGLSurface = *mut ::std::os::raw::c_void;
pub type EGLContext = *mut ::std::os::raw::c_void;
pub type EGLClientBuffer = *mut ::std::os::raw::c_void;
pub type EGLImageKHR = *mut ::std::os::raw::c_void;
pub type __eglMustCastToProperFunctionPointerType = ::std::option::Option<unsafe extern "C" fn()>;
pub type PFNEGLBINDAPIPROC = ::std::option::Option<unsafe extern "C" fn(api: EGLenum) -> EGLBoolean>;
pub type PFNEGLCHOOSECONFIGPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    attrib_list: *const EGLint,
    configs: *mut EGLConfig,
    config_size: EGLint,
    num_config: *mut EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLCOPYBUFFERSPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    surface: EGLSurface,
    target: EGLNativePixmapType,
) -> EGLBoolean,
>;
pub type PFNEGLCREATECONTEXTPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    config: EGLConfig,
    share_context: EGLContext,
    attrib_list: *const EGLint,
) -> EGLContext,
>;
pub type PFNEGLCREATEIMAGEKHRPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    ctx: EGLContext,
    target: EGLenum,
    buffer: EGLClientBuffer,
    attrib_list: *const EGLint,
) -> EGLImageKHR,
>;
pub type PFNEGLDESTROYIMAGEKHRPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    image: EGLImageKHR,
) -> EGLBoolean,
>;
pub type PFNEGLCREATEPBUFFERSURFACEPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    config: EGLConfig,
    attrib_list: *const EGLint,
) -> EGLSurface,
>;
pub type PFNEGLCREATEPIXMAPSURFACEPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    config: EGLConfig,
    pixmap: EGLNativePixmapType,
    attrib_list: *const EGLint,
) -> EGLSurface,
>;
pub type PFNEGLCREATEWINDOWSURFACEPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    config: EGLConfig,
    win: EGLNativeWindowType,
    attrib_list: *const EGLint,
) -> EGLSurface,
>;
pub type PFNEGLDESTROYCONTEXTPROC =
::std::option::Option<unsafe extern "C" fn(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean>;
pub type PFNEGLDESTROYSURFACEPROC =
::std::option::Option<unsafe extern "C" fn(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean>;
pub type PFNEGLGETCONFIGATTRIBPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    config: EGLConfig,
    attribute: EGLint,
    value: *mut EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLGETCONFIGSPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    configs: *mut EGLConfig,
    config_size: EGLint,
    num_config: *mut EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLGETCURRENTDISPLAYPROC = ::std::option::Option<unsafe extern "C" fn() -> EGLDisplay>;
pub type PFNEGLGETCURRENTSURFACEPROC =
::std::option::Option<unsafe extern "C" fn(readdraw: EGLint) -> EGLSurface>;
pub type PFNEGLGETDISPLAYPROC =
::std::option::Option<unsafe extern "C" fn(display_id: EGLNativeDisplayType) -> EGLDisplay>;
pub type PFNEGLGETERRORPROC = ::std::option::Option<unsafe extern "C" fn() -> EGLint>;
pub type PFNEGLGETPROCADDRESSPROC = ::std::option::Option<
unsafe extern "C" fn(
    procname: *const ::std::os::raw::c_char,
) -> *mut ::std::os::raw::c_void,
>;
pub type PFNEGLINITIALIZEPROC = ::std::option::Option<
unsafe extern "C" fn(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean,
>;
pub type PFNEGLMAKECURRENTPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    draw: EGLSurface,
    read: EGLSurface,
    ctx: EGLContext,
) -> EGLBoolean,
>;
pub type PFNEGLQUERYCONTEXTPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    ctx: EGLContext,
    attribute: EGLint,
    value: *mut EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLQUERYSTRINGPROC = ::std::option::Option<
unsafe extern "C" fn(dpy: EGLDisplay, name: EGLint) -> *const ::std::os::raw::c_char,
>;
pub type PFNEGLQUERYSURFACEPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: EGLint,
    value: *mut EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLSWAPBUFFERSPROC =
::std::option::Option<unsafe extern "C" fn(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean>;
pub type PFNEGLTERMINATEPROC =
::std::option::Option<unsafe extern "C" fn(dpy: EGLDisplay) -> EGLBoolean>;
pub type PFNEGLWAITGLPROC = ::std::option::Option<unsafe extern "C" fn() -> EGLBoolean>;
pub type PFNEGLWAITNATIVEPROC =
::std::option::Option<unsafe extern "C" fn(engine: EGLint) -> EGLBoolean>;
pub type PFNEGLBINDTEXIMAGEPROC = ::std::option::Option<
unsafe extern "C" fn(dpy: EGLDisplay, surface: EGLSurface, buffer: EGLint) -> EGLBoolean,
>;
pub type PFNEGLRELEASETEXIMAGEPROC = ::std::option::Option<
unsafe extern "C" fn(dpy: EGLDisplay, surface: EGLSurface, buffer: EGLint) -> EGLBoolean,
>;
pub type PFNEGLSURFACEATTRIBPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    surface: EGLSurface,
    attribute: EGLint,
    value: EGLint,
) -> EGLBoolean,
>;
pub type PFNEGLSWAPINTERVALPROC =
::std::option::Option<unsafe extern "C" fn(dpy: EGLDisplay, interval: EGLint) -> EGLBoolean>;

pub type PFNEGLGETPLATFORMDISPLAYEXTPROC = ::std::option::Option<
unsafe extern "C" fn(
    platform: EGLenum,
    native_display: *mut ::std::os::raw::c_void,
    attrib_list: *const EGLint,
) -> EGLDisplay,
>;

pub type PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    image: EGLImageKHR,
    fourcc: *mut i32,
    num_planes: *mut i32,
    modifiers: *mut EGLuint64KHR,
) -> EGLBoolean,
>;
pub type PFNEGLEXPORTDMABUFIMAGEMESAPROC = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    image: EGLImageKHR,
    fds: *mut i32,
    strides: *mut EGLint,
    offsets: *mut EGLint,
) -> EGLBoolean,
>;

// HACK(eddyb) this is actually an OpenGL extension function.
type PFNGLEGLIMAGETARGETTEXTURE2DOESPROC = ::std::option::Option<
unsafe extern "C" fn(
    super::gl_sys::GLenum,
    EGLImageKHR,
),
>;

type PFNEGLPRESENTATIONTIMEANDROID = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    surface: EGLSurface,
    time: i64
),
>;

type PFNEGLSURFACEATTRIB = ::std::option::Option<
unsafe extern "C" fn(
    dpy: EGLDisplay,
    surface: EGLSurface,
    attrib: EGLint,
    value: EGLint
)->EGLBoolean,
>;

pub struct LibEgl {
    pub eglPresentationTimeANDROID: PFNEGLPRESENTATIONTIMEANDROID,
    pub eglBindAPI: PFNEGLBINDAPIPROC,
    pub eglChooseConfig: PFNEGLCHOOSECONFIGPROC,
    pub eglCopyBuffers: PFNEGLCOPYBUFFERSPROC,
    pub eglCreateContext: PFNEGLCREATECONTEXTPROC,
    pub eglCreatePbufferSurface: PFNEGLCREATEPBUFFERSURFACEPROC,
    pub eglCreatePixmapSurface: PFNEGLCREATEPIXMAPSURFACEPROC,
    pub eglCreateWindowSurface: PFNEGLCREATEWINDOWSURFACEPROC,
    pub eglDestroyContext: PFNEGLDESTROYCONTEXTPROC,
    pub eglDestroySurface: PFNEGLDESTROYSURFACEPROC,
    pub eglGetConfigAttrib: PFNEGLGETCONFIGATTRIBPROC,
    pub eglGetConfigs: PFNEGLGETCONFIGSPROC,
    pub eglGetCurrentDisplay: PFNEGLGETCURRENTDISPLAYPROC,
    pub eglGetCurrentSurface: PFNEGLGETCURRENTSURFACEPROC,
    pub eglGetDisplay: PFNEGLGETDISPLAYPROC,
    pub eglGetError: PFNEGLGETERRORPROC,
    pub eglGetProcAddress: PFNEGLGETPROCADDRESSPROC,
    pub eglInitialize: PFNEGLINITIALIZEPROC,
    pub eglMakeCurrent: PFNEGLMAKECURRENTPROC,
    pub eglQueryContext: PFNEGLQUERYCONTEXTPROC,
    pub eglQueryString: PFNEGLQUERYSTRINGPROC,
    pub eglQuerySurface: PFNEGLQUERYSURFACEPROC,
    pub eglSwapBuffers: PFNEGLSWAPBUFFERSPROC,
    pub eglTerminate: PFNEGLTERMINATEPROC,
    pub eglWaitGL: PFNEGLWAITGLPROC,
    pub eglWaitNative: PFNEGLWAITNATIVEPROC,
    pub eglBindTexImage: PFNEGLBINDTEXIMAGEPROC,
    pub eglReleaseTexImage: PFNEGLRELEASETEXIMAGEPROC,
    pub eglSurfaceAttrib: PFNEGLSURFACEATTRIBPROC,
    pub eglSwapInterval: PFNEGLSWAPINTERVALPROC,

    pub eglCreateImageKHR: PFNEGLCREATEIMAGEKHRPROC,
    pub eglDestroyImageKHR: PFNEGLDESTROYIMAGEKHRPROC,
    pub eglExportDMABUFImageQueryMESA: PFNEGLEXPORTDMABUFIMAGEQUERYMESAPROC,
    pub eglExportDMABUFImageMESA: PFNEGLEXPORTDMABUFIMAGEMESAPROC,
    pub eglGetPlatformDisplayEXT: PFNEGLGETPLATFORMDISPLAYEXTPROC,

    // HACK(eddyb) this is actually an OpenGL extension function.
    pub glEGLImageTargetTexture2DOES: PFNGLEGLIMAGETARGETTEXTURE2DOESPROC,

    _keep_module_alive: ModuleLoader,
}

use std::ffi::CStr;

use crate::module_loader::ModuleLoader;

impl LibEgl {
    pub fn try_load() -> Option<LibEgl> {
        
        let module = ModuleLoader::load("libEGL.so").or_else(|_| ModuleLoader::load("libEGL.so.1")).ok()?;

        let eglGetProcAddress: PFNEGLGETPROCADDRESSPROC = module.get_symbol("eglGetProcAddress").ok();
        macro_rules! get_ext_fn {
            ($name:literal) => {
                eglGetProcAddress.and_then(|gpa| unsafe {
                    std::mem::transmute(gpa(CStr::from_bytes_with_nul(concat!($name, "\0").as_bytes()).unwrap().as_ptr()))
                })
            }
        }

        Some(LibEgl {
            eglPresentationTimeANDROID: module.get_symbol("eglPresentationTimeANDROID").ok(),
            eglBindAPI: module.get_symbol("eglBindAPI").ok(),
            eglChooseConfig: module.get_symbol("eglChooseConfig").ok(),
            eglCopyBuffers: module.get_symbol("eglCopyBuffers").ok(),
            eglCreateContext: module.get_symbol("eglCreateContext").ok(),
            eglCreatePbufferSurface: module.get_symbol("eglCreatePbufferSurface").ok(),
            eglCreatePixmapSurface: module.get_symbol("eglCreatePixmapSurface").ok(),
            eglCreateWindowSurface: module.get_symbol("eglCreateWindowSurface").ok(),
            eglDestroyContext: module.get_symbol("eglDestroyContext").ok(),
            eglDestroySurface: module.get_symbol("eglDestroySurface").ok(),
            eglGetConfigAttrib: module.get_symbol("eglGetConfigAttrib").ok(),
            eglGetConfigs: module.get_symbol("eglGetConfigs").ok(),
            eglGetCurrentDisplay: module.get_symbol("eglGetCurrentDisplay").ok(),
            eglGetCurrentSurface: module.get_symbol("eglGetCurrentSurface").ok(),
            eglGetDisplay: module.get_symbol("eglGetDisplay").ok(),
            eglGetError: module.get_symbol("eglGetError").ok(),
            eglGetProcAddress,
            eglInitialize: module.get_symbol("eglInitialize").ok(),
            eglMakeCurrent: module.get_symbol("eglMakeCurrent").ok(),
            eglQueryContext: module.get_symbol("eglQueryContext").ok(),
            eglQueryString: module.get_symbol("eglQueryString").ok(),
            eglQuerySurface: module.get_symbol("eglQuerySurface").ok(),
            eglSwapBuffers: module.get_symbol("eglSwapBuffers").ok(),
            eglTerminate: module.get_symbol("eglTerminate").ok(),
            eglWaitGL: module.get_symbol("eglWaitGL").ok(),
            eglWaitNative: module.get_symbol("eglWaitNative").ok(),
            eglBindTexImage: module.get_symbol("eglBindTexImage").ok(),
            eglReleaseTexImage: module.get_symbol("eglReleaseTexImage").ok(),
            eglSurfaceAttrib: module.get_symbol("eglSurfaceAttrib").ok(),
            eglSwapInterval: module.get_symbol("eglSwapInterval").ok(),

            eglCreateImageKHR: get_ext_fn!("eglCreateImageKHR"),
            eglDestroyImageKHR: get_ext_fn!("eglDestroyImageKHR"),
            eglExportDMABUFImageQueryMESA: get_ext_fn!("eglExportDMABUFImageQueryMESA"),
            eglExportDMABUFImageMESA: get_ext_fn!("eglExportDMABUFImageMESA"),
            eglGetPlatformDisplayEXT: get_ext_fn!("eglGetPlatformDisplayEXT"),

            glEGLImageTargetTexture2DOES: get_ext_fn!("glEGLImageTargetTexture2DOES"),

            _keep_module_alive: module,
        })
    }
}

/// Must be called with this window surface current on the render thread.
pub(crate) unsafe fn set_window_swap_interval(egl: &LibEgl, display: EGLDisplay) -> bool {
    if (egl.eglSwapInterval.unwrap())(display, 1) == 0 {
        crate::warning!("eglSwapInterval(1) failed: {:#x}", (egl.eglGetError.unwrap())());
        return false;
    }
    true
}

#[derive(Debug)]
pub enum EglError {
    NoDisplay,
    InitializeFailed,
    CreateContextFailed,
    ChooseConfigFailed,
}

pub struct Egl {}

#[cfg(any(test, target_os = "android"))]
fn android_config_rank(
    [r, g, b, alpha, depth, stencil, samples, sample_buffers]: [i32; 8],
    alpha_needed: i32,
    depth_needed: i32,
) -> Option<(i32, i32, i32, i32)> {
    if r < 8 || g < 8 || b < 8 || alpha < alpha_needed || depth < depth_needed
        || samples != 0 || sample_buffers != 0
    {
        return None;
    }
    // EGL's chooser uses minima; zero does not exclude unused attributes.
    Some((r + g + b - 24, depth - depth_needed, stencil, alpha - alpha_needed))
}

#[cfg(target_os = "android")]
pub unsafe fn create_egl_context(
    egl: &LibEgl,
    native_display: *mut std::ffi::c_void,
) -> Result<(EGLContext, EGLConfig, EGLDisplay), EglError> {
    let display = (egl.eglGetDisplay.unwrap())(native_display as _);
    if display.is_null() {
        return Err(EglError::NoDisplay);
    }
    if (egl.eglInitialize.unwrap())(display, null_mut(), null_mut()) == 0 {
        return Err(EglError::InitializeFailed);
    }
    let result = (|| {
        if (egl.eglBindAPI.unwrap())(EGL_OPENGL_ES_API) == 0 {
            return Err(EglError::InitializeFailed);
        }
        let xr = cfg!(quest);
        let renderable = if cfg!(any(use_gles_3, quest)) { EGL_OPENGL_ES3_BIT_KHR } else { EGL_OPENGL_ES2_BIT };
        // OpenXR uses alpha, pbuffers and its own depth targets. Ordinary views
        // are opaque and keep 24-bit window depth for UI Z ordering.
        let alpha = if xr { 8 } else { 0 };
        let depth = if xr { 0 } else { 24 };
        let surface_type = EGL_WINDOW_BIT | if xr { EGL_PBUFFER_BIT } else { 0 };
        let attributes = [
            EGL_SURFACE_TYPE as i32, surface_type as i32,
            EGL_RENDERABLE_TYPE as i32, renderable as i32,
            EGL_RED_SIZE as i32, 8, EGL_GREEN_SIZE as i32, 8, EGL_BLUE_SIZE as i32, 8,
            EGL_ALPHA_SIZE as i32, alpha, EGL_DEPTH_SIZE as i32, depth,
            EGL_SAMPLE_BUFFERS as i32, 0, EGL_SAMPLES as i32, 0, EGL_NONE as i32,
        ];
        let choose = egl.eglChooseConfig.unwrap();
        let get = egl.eglGetConfigAttrib.unwrap();
        let mut count = 0;
        if choose(display, attributes.as_ptr(), null_mut(), 0, &mut count) == 0 || count <= 0 {
            return Err(EglError::ChooseConfigFailed);
        }
        let mut configs = vec![null_mut(); count as usize];
        if choose(display, attributes.as_ptr(), configs.as_mut_ptr(), count, &mut count) == 0 {
            return Err(EglError::ChooseConfigFailed);
        }
        let keys = [EGL_RED_SIZE, EGL_GREEN_SIZE, EGL_BLUE_SIZE, EGL_ALPHA_SIZE,
                    EGL_DEPTH_SIZE, EGL_STENCIL_SIZE, EGL_SAMPLES, EGL_SAMPLE_BUFFERS];
        let mut best = None;
        for &config in configs.iter().take(count as usize) {
            let mut values = [0; 8];
            if keys.iter().zip(&mut values).all(|(&key, value)| get(display, config, key as i32, value) != 0) {
                if let Some(rank) = android_config_rank(values, alpha, depth) {
                    if best.is_none_or(|(previous, _)| rank < previous) {
                        best = Some((rank, config));
                    }
                    if rank == (0, 0, 0, 0) {
                        break;
                    }
                }
            }
        }
        let config = best.ok_or(EglError::ChooseConfigFailed)?.1;
        let context = create_android_gl_context(egl, display, config)?;
        Ok((context, config, display))
    })();
    if result.is_err() {
        (egl.eglTerminate.unwrap())(display);
    }
    result
}

#[cfg(target_os = "android")]
pub(crate) unsafe fn create_android_gl_context(
    egl: &LibEgl, display: EGLDisplay, config: EGLConfig,
) -> Result<EGLContext, EglError> {
    let version = if cfg!(any(use_gles_3, quest)) { 3 } else { 2 };
    let attributes = [EGL_CONTEXT_MAJOR_VERSION as i32, version, EGL_NONE as i32];
    let context = (egl.eglCreateContext.unwrap())(display, config, EGL_NO_CONTEXT, attributes.as_ptr());
    if context.is_null() {
        return Err(EglError::CreateContextFailed);
    }
    Ok(context)
}

#[cfg(test)]
mod tests {
    use super::android_config_rank;

    #[test]
    fn ranks_opaque_configs_and_rejects_msaa_or_insufficient_depth() {
        let rank = |format| android_config_rank(format, 0, 24);
        let exact = rank([8, 8, 8, 0, 24, 0, 0, 0]).unwrap();
        assert!(exact < rank([8, 8, 8, 8, 24, 0, 0, 0]).unwrap());
        assert!(exact < rank([8, 8, 8, 0, 24, 8, 0, 0]).unwrap());
        assert!(exact < rank([8, 8, 8, 0, 32, 0, 0, 0]).unwrap());
        assert_eq!(rank([8, 8, 8, 0, 24, 0, 4, 1]), None);
        assert_eq!(rank([8, 8, 8, 0, 16, 0, 0, 0]), None);
        assert_eq!(rank([5, 6, 5, 0, 24, 0, 0, 0]), None);
    }

    #[test]
    fn xr_requires_rgba_and_prefers_depthless_surface() {
        let rank = |format| android_config_rank(format, 8, 0);
        assert_eq!(rank([8, 8, 8, 8, 0, 0, 0, 0]), Some((0, 0, 0, 0)));
        assert_eq!(rank([8, 8, 8, 0, 0, 0, 0, 0]), None);
    }
}


#[cfg(target_env="ohos")]
pub unsafe  fn create_egl_context(
    egl: &mut LibEgl
) -> Result<(EGLContext, EGLConfig, EGLDisplay), EglError> {
    let display = (egl.eglGetDisplay.unwrap())(null_mut());
    if display == null_mut() {
        return Err(EglError::NoDisplay);
    }

    if (egl.eglInitialize.unwrap())(display,null_mut(),null_mut()) == 0 {
        return Err(EglError::InitializeFailed);
    }

    #[rustfmt::skip]
    let cfg_attributes = vec![
        EGL_SURFACE_TYPE,
        EGL_WINDOW_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 8,
        EGL_RENDERABLE_TYPE,
        EGL_OPENGL_ES2_BIT,
        EGL_DEPTH_SIZE, 0,
        EGL_STENCIL_SIZE, 0,
        EGL_NONE
    ];
    let available_cfgs: Vec<EGLConfig> = vec![null_mut(); 1];
    let mut cfg_count = 0;

    if (egl.eglChooseConfig.unwrap())(
        display,
        cfg_attributes.as_ptr() as _,
        available_cfgs.as_ptr() as _,
        1,&mut cfg_count as *mut _ as *mut _,
    ) == 0 {
        return Err(EglError::ChooseConfigFailed);
    }

    assert!(cfg_count > 0);

    let config = available_cfgs[0];

    #[cfg(ohos_sim)]
    let ctx_attributes = vec![
        EGL_CONTEXT_MAJOR_VERSION, 2, // version 1 and 3 also work
        EGL_CONTEXT_MINOR_VERSION_KHR , 0,
        // The rest of this was taken from servo's OpenGL config for OpenHarmony.
        EGL_CONTEXT_OPENGL_PROFILE_MASK,
        EGL_CONTEXT_OPENGL_COMPATIBILITY_PROFILE_BIT, // allows use of deprecated features
        EGL_NONE, 0, 0, 0,
    ];

    #[cfg(not(ohos_sim))]
    let ctx_attributes = vec![
        EGL_CONTEXT_MAJOR_VERSION, 
        #[cfg(use_gles_3)]
        3, // version 1 and 3 also work
        #[cfg(not(use_gles_3))]
        2, // version 1 and 3 also work
        EGL_CONTEXT_MINOR_VERSION_KHR , 0,
        EGL_NONE,
    ];

    let context = (egl.eglCreateContext.unwrap())(
        display,
        config,
        /* EGL_NO_CONTEXT */ null_mut(),
        ctx_attributes.as_ptr() as _
    );
    if context.is_null(){
        return Err(EglError::CreateContextFailed);
    }
    crate::log!("create elg context success");
    return Ok((context, config, display));
}
