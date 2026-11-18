use {
    std::{
        mem,
        os::raw::{c_long, c_void},
        ffi::CString,
        os::{self, fd::{AsRawFd as _, FromRawFd as _, OwnedFd}},
    },
    self::super::{
        x11_sys,
        xlib_window::XlibWindow,
    },
    self::super::super::{
        dma_buf,
        egl_damage::{EglWindowDamage, Surface},
        egl_sys::{self, LibEgl},
        gl_sys,
        gl_sys::LibGl,
    },
    crate::{
        cx::Cx,
        window::WindowId,
        makepad_math::DVec2,
        pass::PassId,
        event::*,
        texture::{CxTexture, Texture},
    },
};

impl Cx {
    
    pub fn draw_pass_to_window(
        &mut self,
        pass_id: PassId,
        opengl_window: &mut OpenglWindow,
    ) {
        let opengl_cx = self.os.opengl_cx.as_ref().unwrap();
        let surface = opengl_window.egl_surface;
        let current = unsafe {
            (opengl_cx.libegl.eglMakeCurrent.unwrap())(opengl_cx.egl_display, surface, surface, opengl_cx.egl_context)
        } != 0;
        if !current {
            opengl_window.damage.invalidate();
            self.passes[pass_id].paint_dirty = false;
            return;
        }
        if self.setup_render_pass(pass_id).is_none() {
            opengl_window.damage.invalidate();
            return;
        }
        let geom = &opengl_window.window_geom;
        let size = [
            (geom.inner_size.x * geom.dpi_factor).floor() as i32,
            (geom.inner_size.y * geom.dpi_factor).floor() as i32,
        ];
        let clear = self.gl_window_clear(pass_id);
        let mut frame = opengl_window.damage.prepare(&self.os.opengl_cx.as_ref().unwrap().surface(surface), self, pass_id, size, clear);
        opengl_window.damage.render(self, pass_id, clear, &mut frame);
        opengl_window.damage.present(&self.os.opengl_cx.as_ref().unwrap().surface(surface), frame);
    }

    pub fn share_texture_for_presentable_image(
        &mut self,
        texture: &Texture,
    ) -> dma_buf::Image<OwnedFd> {
        let cxtexture = &mut self.textures[texture.texture_id()];
        cxtexture.update_shared_texture(self.os.gl());

        let opengl_cx = self.os.opengl_cx.as_ref().unwrap();
        unsafe {
            let egl_image = (opengl_cx.libegl.eglCreateImageKHR.unwrap())(
                opengl_cx.egl_display,
                opengl_cx.egl_context,
                egl_sys::EGL_GL_TEXTURE_2D_KHR,
                cxtexture.os.gl_texture.unwrap() as egl_sys::EGLClientBuffer,
                std::ptr::null(),
            );
            assert!(!egl_image.is_null(), "eglCreateImageKHR failed");

            let (mut fourcc, mut num_planes) = (0, 0);
            assert!(
                (
                    opengl_cx.libegl.eglExportDMABUFImageQueryMESA
                        .expect("eglExportDMABUFImageQueryMESA unsupported")
                )(
                    opengl_cx.egl_display,
                    egl_image,
                    &mut fourcc as *mut u32 as *mut i32,
                    &mut num_planes,
                    std::ptr::null_mut(),
                ) != 0,
                "eglExportDMABUFImageQueryMESA failed",
            );
            assert!(
                num_planes == 1,
                "planar DRM format {:?} ({fourcc:#x}) unsupported (num_planes={num_planes})",
                std::str::from_utf8(&u32::to_le_bytes(fourcc))
            );

            // HACK(eddyb) `modifiers` are reported per-plane, so to avoid UB,
            // a second query call is used *after* the `num_planes == 1` check.
            let mut modifiers = 0;
            assert!(
                (opengl_cx.libegl.eglExportDMABUFImageQueryMESA.unwrap())(
                    opengl_cx.egl_display,
                    egl_image,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut modifiers,
                ) != 0,
                "eglExportDMABUFImageQueryMESA failed",
            );

            let (mut dma_buf_fd, mut offset, mut stride) = (0, 0, 0);
            assert!(
                (opengl_cx.libegl.eglExportDMABUFImageMESA.unwrap())(
                    opengl_cx.egl_display,
                    egl_image,
                    &mut dma_buf_fd,
                    &mut stride as *mut u32 as *mut i32,
                    &mut offset as *mut u32 as *mut i32,
                ) != 0,
                "eglExportDMABUFImageMESA failed",
            );

            assert!(
                (opengl_cx.libegl.eglDestroyImageKHR.unwrap())(
                    opengl_cx.egl_display,
                    egl_image,
                ) != 0,
                "eglDestroyImageKHR failed",
            );

            dma_buf::Image {
                drm_format: dma_buf::DrmFormat {
                    fourcc,
                    modifiers,
                },
                planes: dma_buf::ImagePlane {
                    dma_buf_fd: os::fd::OwnedFd::from_raw_fd(dma_buf_fd),
                    offset,
                    stride,
                },
            }
        }
    }
}


impl CxTexture {
    fn update_shared_texture(&mut self, gl:&LibGl) {
        if !self.alloc_shared(){
            return
        }
        let alloc = self.alloc.as_ref().unwrap();
        
        // HACK(eddyb) drain error queue, so that we can check erors below.
        while unsafe { (gl.glGetError)() } != 0 {}

        unsafe {
            if self.os.gl_texture.is_none() {
                let mut gl_texture = std::mem::MaybeUninit::uninit();
                (gl.glGenTextures)(1, gl_texture.as_mut_ptr());
                self.os.gl_texture = Some(gl_texture.assume_init());
            }

            (gl.glBindTexture)(gl_sys::TEXTURE_2D, self.os.gl_texture.unwrap());

            (gl.glTexParameteri)(gl_sys::TEXTURE_2D, gl_sys::TEXTURE_MIN_FILTER, gl_sys::NEAREST as i32);
            (gl.glTexParameteri)(gl_sys::TEXTURE_2D, gl_sys::TEXTURE_MAG_FILTER, gl_sys::NEAREST as i32);
            (gl.glTexImage2D)(
                gl_sys::TEXTURE_2D,
                0,
                gl_sys::RGBA as i32,
                alloc.width as i32,
                alloc.height as i32,
                0,
                gl_sys::RGBA,
                gl_sys::UNSIGNED_BYTE,
                std::ptr::null()
            );
            assert_eq!((gl.glGetError)(), 0, "glTexImage2D({}, {}) failed", alloc.width, alloc.height);
            (gl.glBindTexture)(gl_sys::TEXTURE_2D, 0);
        }
    }

    pub fn update_from_shared_dma_buf_image(
        &mut self,
        gl:&LibGl,
        opengl_cx: &OpenglCx,
        dma_buf_image: &dma_buf::Image<os::fd::OwnedFd>,
    ) {
        if !self.alloc_shared(){
            return
        }
        let alloc = self.alloc.as_ref().unwrap();

        // HACK(eddyb) drain error queue, so that we can check erors below.
        while unsafe { (gl.glGetError)() } != 0 {}
        opengl_cx.make_current();
        while unsafe { (gl.glGetError)() } != 0 {}

        let dma_buf::Image { drm_format, planes: ref plane0 } = *dma_buf_image;

        let image_attribs = [
            egl_sys::EGL_LINUX_DRM_FOURCC_EXT,
            drm_format.fourcc,
            egl_sys::EGL_WIDTH,
            alloc.width as u32,
            egl_sys::EGL_HEIGHT,
            alloc.height as u32,
            egl_sys::EGL_DMA_BUF_PLANE0_FD_EXT,
            plane0.dma_buf_fd.as_raw_fd() as u32,
            egl_sys::EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane0.offset,
            egl_sys::EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane0.stride,
            egl_sys::EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            drm_format.modifiers as u32,
            egl_sys::EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            (drm_format.modifiers >> 32) as u32,
            egl_sys::EGL_NONE,
        ];
        let egl_image = unsafe { (opengl_cx.libegl.eglCreateImageKHR.unwrap())(
            opengl_cx.egl_display,
            std::ptr::null_mut(),
            egl_sys::EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null_mut(),
            image_attribs.as_ptr() as _,
        ) };
        assert!(!egl_image.is_null(), "eglCreateImageKHR failed");

        unsafe {
            let gl_texture = *self.os.gl_texture.get_or_insert_with(|| {
                let mut gl_texture = std::mem::MaybeUninit::uninit();
                (gl.glGenTextures)(1, gl_texture.as_mut_ptr());
                assert_eq!((gl.glGetError)(), 0, "glGenTextures failed");
                gl_texture.assume_init()
            });

            (gl.glBindTexture)(gl_sys::TEXTURE_2D, gl_texture);
            assert_eq!((gl.glGetError)(), 0, "glBindTexture({gl_texture}) failed");

            (opengl_cx.libegl.glEGLImageTargetTexture2DOES.unwrap())(gl_sys::TEXTURE_2D, egl_image);
            assert_eq!((gl.glGetError)(), 0, "glEGLImageTargetTexture2DOES failed");

            (gl.glBindTexture)(gl_sys::TEXTURE_2D, 0);
        }
    }
}

// FIXME(eddyb) move this out of `linux::x11`, since it's mostly generic EGL.
pub struct OpenglCx {
    libegl: LibEgl,
    pub libgl: LibGl,
    egl_display: egl_sys::EGLDisplay,
    egl_config: egl_sys::EGLConfig,
    egl_context: egl_sys::EGLContext,

    egl_platform: egl_sys::EGLenum,
    egl_platform_display: *mut c_void,
}

impl OpenglCx {
    fn surface(&self, surface: egl_sys::EGLSurface) -> Surface<'_> {
        Surface { egl: &self.libegl, display: self.egl_display, surface }
    }

    pub unsafe fn from_egl_platform_display<T>(
        egl_platform: egl_sys::EGLenum,
        egl_platform_display: *mut T,
    ) -> OpenglCx {
        let egl_platform_display = egl_platform_display as *mut c_void;

        // Load EGL function pointers.
        let libegl = LibEgl::try_load().expect("can't load LibEGL");

        let mut major = 0;
        let mut minor = 0;

        let egl_display = (libegl.eglGetPlatformDisplayEXT.unwrap())(
            egl_platform,
            egl_platform_display,
            std::ptr::null(),
        );
        assert!(!egl_display.is_null(), "can't get EGL platform display");

        assert!(
            (libegl.eglInitialize.unwrap())(egl_display, &mut major, &mut minor) != 0,
            "can't initialize EGL",
        );

        assert!(
            (libegl.eglBindAPI.unwrap())(egl_sys::EGL_OPENGL_ES_API) != 0,
            "can't bind EGL_OPENGL_ES_API",
        );

        // Choose framebuffer configuration.
        let cfg_attribs = [
            egl_sys::EGL_RED_SIZE,
            8,
            egl_sys::EGL_GREEN_SIZE,
            8,
            egl_sys::EGL_BLUE_SIZE,
            8,
            egl_sys::EGL_ALPHA_SIZE,
            8,
            // egl_sys::EGL_DEPTH_SIZE,
            // 24,
            // egl_sys::EGL_STENCIL_SIZE,
            // 8,
            egl_sys::EGL_RENDERABLE_TYPE,
            egl_sys::EGL_OPENGL_ES2_BIT,
            egl_sys::EGL_NONE
        ];

        let mut egl_config = 0 as egl_sys::EGLConfig;
        let mut matched_egl_configs = 0;
        assert!(
            (libegl.eglChooseConfig.unwrap())(
                egl_display,
                cfg_attribs.as_ptr() as _,
                &mut egl_config,
                1,
                &mut matched_egl_configs
            ) != 0 && matched_egl_configs == 1,
            "eglChooseConfig failed",
        );

        // Create EGL context.
        let ctx_attribs = [
            egl_sys::EGL_CONTEXT_MAJOR_VERSION,
            #[cfg(use_gles_3)]
            3,
            #[cfg(not(use_gles_3))]
            2,
            egl_sys::EGL_NONE
        ];

        let egl_context = (libegl.eglCreateContext.unwrap())(
            egl_display,
            egl_config,
            egl_sys::EGL_NO_CONTEXT,
            ctx_attribs.as_ptr() as _,
        );
        assert!(!egl_context.is_null(), "eglCreateContext failed");
        
        let libgl = LibGl::try_load(| s | {
            for s in s{
                let s = CString::new(*s).unwrap();
                let p = unsafe{libegl.eglGetProcAddress.unwrap()(s.as_ptr())};
                if !p.is_null(){
                    return p
                }
            }
            0 as * const _
        }).expect("Cant load openGL functions");
        
        OpenglCx {
            libegl,
            libgl,
            egl_display,
            egl_config,
            egl_context,

            egl_platform,
            egl_platform_display,
        }
    }

    pub(crate) fn record_renderer(&self, info: &mut crate::gpu_info::GpuInfo) {
        super::super::opengl::record_gl_renderer(info, &self.libgl, &self.libegl, self.egl_display);
    }

    pub fn make_current(&self) {
        unsafe {
            (self.libegl.eglMakeCurrent.unwrap())(
                self.egl_display,
                egl_sys::EGL_NO_SURFACE,
                egl_sys::EGL_NO_SURFACE,
                self.egl_context,
            );
        }
    }
}

#[derive(Clone)]
pub struct OpenglWindow {
    pub first_draw: bool,
    pub window_id: WindowId,
    pub window_geom: WindowGeom,
    pub opening_repaint_count: u32,
    pub cal_size: DVec2,
    pub xlib_window: Box<XlibWindow>,
    pub egl_surface: egl_sys::EGLSurface,
    pub(crate) swap_interval_set: bool,
    pub(crate) damage: EglWindowDamage,
}

impl OpenglWindow {
    pub fn new(
        window_id: WindowId,
        opengl_cx: &OpenglCx,
        inner_size: DVec2,
        position: Option<DVec2>,
        title: &str
    ) -> OpenglWindow {
        // Checked "downcast" of the EGL platform display to a X11 display.
        assert_eq!(opengl_cx.egl_platform, egl_sys::EGL_PLATFORM_X11_EXT);
        let display = opengl_cx. egl_platform_display as *mut x11_sys::Display;

        let mut xlib_window = Box::new(XlibWindow::new(window_id));

        // Get X11 visual from EGL configuration.
        let visual_info = unsafe {
            let mut native_visual_id = 0;
            assert!(
                (opengl_cx.libegl.eglGetConfigAttrib.unwrap())(
                    opengl_cx.egl_display,
                    opengl_cx.egl_config,
                    egl_sys::EGL_NATIVE_VISUAL_ID as _,
                    &mut native_visual_id,
                ) != 0,
                "eglGetConfigAttrib(EGL_NATIVE_VISUAL_ID) failed",
            );

            let mut visual_template = mem::zeroed::<x11_sys::XVisualInfo>();
            visual_template.visualid = native_visual_id as _;

            let mut count = 0;
            let visual_info_ptr = x11_sys::XGetVisualInfo(
                display,
                x11_sys::VisualIDMask as c_long,
                &mut visual_template,
                &mut count,
            );
            assert!(
                !visual_info_ptr.is_null() && count == 1,
                "can't get visual from EGL configuration with XGetVisualInfo",
            );

            let visual_info = *visual_info_ptr;
            x11_sys::XFree(visual_info_ptr as *mut c_void);
            visual_info
        };

        let custom_window_chrome = false;
        xlib_window.init(title, inner_size, position, visual_info, custom_window_chrome);

        let egl_surface = unsafe {
            (opengl_cx.libegl.eglCreateWindowSurface.unwrap())(
                opengl_cx.egl_display,
                opengl_cx.egl_config,
                xlib_window.window.unwrap(),
                std::ptr::null(),
            )
        };
        assert!(!egl_surface.is_null(), "eglCreateWindowSurface failed");

        let swap_interval_set = unsafe {
            assert_ne!(
                (opengl_cx.libegl.eglMakeCurrent.unwrap())(
                    opengl_cx.egl_display, egl_surface, egl_surface, opengl_cx.egl_context,
                ),
                0,
                "eglMakeCurrent failed for the new X11 window surface",
            );
            egl_sys::set_window_swap_interval(&opengl_cx.libegl, opengl_cx.egl_display)
        };

        OpenglWindow {
            first_draw: true,
            window_id,
            opening_repaint_count: 0,
            cal_size: DVec2::default(),
            window_geom: xlib_window.get_window_geom(),
            xlib_window,
            egl_surface,
            swap_interval_set,
            damage: EglWindowDamage::default(),
        }
    }
    
    pub fn resize_buffers(&mut self) -> bool {
        let cal_size = DVec2 {
            x: self.window_geom.inner_size.x * self.window_geom.dpi_factor,
            y: self.window_geom.inner_size.y * self.window_geom.dpi_factor
        };
        if self.cal_size != cal_size {
            self.cal_size = cal_size;
            // resize the framebuffer
            true
        }
        else {
            false
        }
    }
    
}
