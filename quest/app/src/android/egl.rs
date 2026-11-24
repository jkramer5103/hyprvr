//! EGL context for OpenXR (XR_KHR_opengl_es_enable) plus the extension entry points needed
//! to import decoder output (AHardwareBuffer) as GL_TEXTURE_EXTERNAL_OES.

use std::ffi::c_void;

use anyhow::{Context, Result, anyhow};
use khronos_egl as egl;
use log::info;

pub type EglImage = *mut c_void;

type GetNativeClientBuffer = unsafe extern "C" fn(*const c_void) -> *mut c_void;
type CreateImageKhr = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> EglImage;
type DestroyImageKhr = unsafe extern "C" fn(*mut c_void, EglImage) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

pub struct Egl {
    pub egl: egl::Instance<egl::Static>,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
    #[allow(dead_code)] // must stay alive while current
    pub surface: egl::Surface,
    get_native_client_buffer: GetNativeClientBuffer,
    create_image: CreateImageKhr,
    destroy_image: DestroyImageKhr,
    image_target_texture: ImageTargetTexture,
}

const EGL_NATIVE_BUFFER_ANDROID: u32 = 0x3140;
const EGL_IMAGE_PRESERVED_KHR: i32 = 0x30D2;
const EGL_OPENGL_ES3_BIT: i32 = 0x0040;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
pub const GL_TEXTURE_EXTERNAL_OES: u32 = 0x8D65;

impl Egl {
    pub fn new() -> Result<Self> {
        let e = egl::Instance::new(egl::Static);
        let display = unsafe { e.get_display(egl::DEFAULT_DISPLAY) }.context("eglGetDisplay")?;
        let (maj, min) = e.initialize(display).context("eglInitialize")?;
        info!("EGL {maj}.{min}");
        let attrs = [
            egl::RED_SIZE, 8,
            egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE, 8,
            egl::ALPHA_SIZE, 8,
            egl::DEPTH_SIZE, 0,
            egl::STENCIL_SIZE, 0,
            egl::SAMPLES, 0,
            egl::RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT,
            egl::SURFACE_TYPE, egl::PBUFFER_BIT,
            egl::NONE,
        ];
        let config = e.choose_first_config(display, &attrs).context("eglChooseConfig")?.ok_or_else(|| anyhow!("no EGL config"))?;
        e.bind_api(egl::OPENGL_ES_API).context("eglBindAPI")?;
        let context = e
            .create_context(display, config, None, &[EGL_CONTEXT_CLIENT_VERSION, 3, egl::NONE])
            .context("eglCreateContext")?;
        let surface = e
            .create_pbuffer_surface(display, config, &[egl::WIDTH, 16, egl::HEIGHT, 16, egl::NONE])
            .context("eglCreatePbufferSurface")?;
        e.make_current(display, Some(surface), Some(surface), Some(context)).context("eglMakeCurrent")?;

        let load = |name: &str| -> Result<extern "system" fn()> {
            e.get_proc_address(name).ok_or_else(|| anyhow!("missing EGL/GL function {name}"))
        };
        unsafe {
            Ok(Egl {
                get_native_client_buffer: std::mem::transmute::<extern "system" fn(), GetNativeClientBuffer>(load("eglGetNativeClientBufferANDROID")?),
                create_image: std::mem::transmute::<extern "system" fn(), CreateImageKhr>(load("eglCreateImageKHR")?),
                destroy_image: std::mem::transmute::<extern "system" fn(), DestroyImageKhr>(load("eglDestroyImageKHR")?),
                image_target_texture: std::mem::transmute::<extern "system" fn(), ImageTargetTexture>(load("glEGLImageTargetTexture2DOES")?),
                egl: e,
                display,
                config,
                context,
                surface,
            })
        }
    }

    pub fn proc_address(&self, name: &str) -> *const c_void {
        self.egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
    }

    /// Wrap an AHardwareBuffer in an EGLImage.
    pub fn image_from_hardware_buffer(&self, hb: *mut c_void) -> Option<EglImage> {
        unsafe {
            let cb = (self.get_native_client_buffer)(hb);
            if cb.is_null() {
                return None;
            }
            let attrs = [EGL_IMAGE_PRESERVED_KHR, 1, egl::NONE];
            let img = (self.create_image)(self.display.as_ptr(), std::ptr::null_mut(), EGL_NATIVE_BUFFER_ANDROID, cb, attrs.as_ptr());
            (!img.is_null()).then_some(img)
        }
    }

    pub fn destroy_image(&self, img: EglImage) {
        unsafe {
            (self.destroy_image)(self.display.as_ptr(), img);
        }
    }

    /// Bind `img` to the currently bound GL_TEXTURE_EXTERNAL_OES texture.
    pub fn bind_external(&self, img: EglImage) {
        unsafe { (self.image_target_texture)(GL_TEXTURE_EXTERNAL_OES, img) }
    }
}
