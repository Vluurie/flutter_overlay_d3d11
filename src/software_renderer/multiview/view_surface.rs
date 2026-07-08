//! Per-view GPU resources for a secondary (satellite) Flutter view.
//!
//! A [`ViewSurface`] owns everything needed to render one Flutter view into a
//! host-owned HWND/swapchain:
//!
//! * an ANGLE pbuffer surface bound to a shared D3D11 texture that Flutter's
//!   raster thread renders into,
//! * the game-device view of that shared texture (opened from the shared handle),
//! * a compositing texture + SRV the host samples when drawing the window,
//! * frame counters and damage rects mirroring the implicit-view bookkeeping.
//!
//! A keyed mutex is **NOT** used for multi-window/satellite views: it does not
//! work across the three different device round-trips a satellite frame takes
//! (ANGLE device renders → shared texture → window-thread device samples), so
//! there is no keyed-mutex handshake here. Cross-device safety is instead
//! provided by the per-frame GL flush before present plus the present-hold
//! logic on the window thread, which never samples the shared texture until the
//! engine has confirmed a freshly-rendered frame at the current size.
//!
//! The implicit view (`view_id == 0`, the in-game overlay) does **not** use this
//! type — its resources still live directly on [`FlutterOverlay`], which DOES use
//! a keyed mutex (single device round-trip). This keeps the existing,
//! battle-tested overlay path untouched while satellite views get an isolated
//! copy of the same machinery, minus the keyed mutex.
//!
//! [`FlutterOverlay`]: crate::software_renderer::api::FlutterOverlay

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Query, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::System::Threading::SetEvent;

use std::ffi::c_void;

use crate::bindings::embedder::{FlutterRect, FlutterViewId};
use crate::software_renderer::gl_renderer::angle_interop::{
    GL_COLOR_ATTACHMENT0, GL_FRAMEBUFFER, GL_FRAMEBUFFER_COMPLETE, GL_TEXTURE_2D, ViewGlProcs,
};
use crate::software_renderer::overlay::overlay_impl::{SendHwnd, SendableHandle};

/// GL/EGL render resources for one secondary view: a pbuffer surface wrapping
/// the view's shared D3D11 texture, a GL color texture bound from it, and an FBO
/// the engine renders into. All created on the render thread.
pub struct ViewGlResources {
    /// Resolved GL/EGL entry points (shared across views).
    pub procs: ViewGlProcs,
    /// EGL pbuffer surface backed by the shared D3D11 texture.
    pub pbuffer_surface: *mut c_void,
    /// GL color texture name (bound from `pbuffer_surface`).
    pub color_texture: u32,
    /// GL framebuffer object name handed to the engine as the backing store.
    pub fbo: u32,
}

unsafe impl Send for ViewGlResources {}
unsafe impl Sync for ViewGlResources {}

/// GPU + windowing resources backing a single secondary Flutter view.
///
/// All raw-pointer / COM fields are only ever touched on the engine's platform
/// (render) thread or under the registry lock, mirroring the safety contract of
/// the implicit-view fields on [`FlutterOverlay`](crate::software_renderer::api::FlutterOverlay).
pub struct ViewSurface {
    /// The view id assigned by [`ViewRegistry::allocate_id`] and passed to
    /// `FlutterEngineAddView`.
    ///
    /// [`ViewRegistry::allocate_id`]: super::ViewRegistry::allocate_id
    pub view_id: FlutterViewId,

    /// The host-owned top-level window this view is composited into.
    pub hwnd: SendHwnd,

    /// Current backing size in pixels. Updated immediately on a resize request,
    /// so this is the *target* size the engine is being told to render at.
    pub width: u32,
    pub height: u32,

    /// Actual pixel size of the currently-allocated shared texture. Unlike
    /// `width`/`height` (which jump to the target size as soon as a resize is
    /// requested), this only changes once the shared texture is really recreated
    /// at the new size on the engine render thread. A window thread compares this
    /// against its swapchain size to know when it's safe to sample the texture.
    pub texture_size: (u32, u32),

    /// Pixel size of the host compositing texture (`texture`/`srv`). Tracks
    /// `texture_size`; when they differ the host texture is recreated.
    pub host_texture_size: (u32, u32),

    /// Device-pixel-ratio reported to Flutter for this view's metrics.
    pub pixel_ratio: f64,

    /// The host-device compositing texture the window samples from, and its SRV.
    pub texture: ID3D11Texture2D,
    pub srv: ID3D11ShaderResourceView,

    /// The texture ANGLE renders into (on ANGLE's device), kept alive for the
    /// lifetime of the pbuffer surface.
    pub angle_internal_texture: Option<ID3D11Texture2D>,
    /// The game-device view of the shared texture (opened from the shared handle).
    pub angle_shared_texture: Option<ID3D11Texture2D>,
    /// Shared handle used to reopen the texture on the game device after resize.
    pub shared_handle: Option<SendableHandle>,

    /// GPU event query signalled after ANGLE finishes the frame, used to avoid
    /// copying a half-written texture.
    pub frame_complete_query: Option<ID3D11Query>,

    /// Incremented after each present by the compositor present callback.
    pub frame_presented: AtomicU64,
    /// Last frame the host copied. Compared against `frame_presented` to detect
    /// new frames.
    pub frame_copied: AtomicU64,

    /// Auto-reset event signalled by the present callback when a new frame is
    /// ready. The window thread blocks on it (via `MsgWaitForMultipleObjects`)
    /// so it renders exactly once per Flutter frame instead of spinning.
    pub frame_event: SendableHandle,

    /// Buffer damage fed back to Flutter so it can skip re-rasterizing.
    pub damage_rects: Mutex<Vec<FlutterRect>>,
    /// Frame damage consumed by the host copy step for partial updates.
    pub frame_damage_rects: Mutex<Vec<FlutterRect>>,

    /// GL/EGL render resources (FBO + color texture + pbuffer). `None` until the
    /// first render-thread setup completes.
    pub gl: Option<ViewGlResources>,

    /// Where this view is drawn on screen (x, y, w, h) in window client pixels,
    /// set by the host so pointer events can be hit-tested + offset into the
    /// view's local space. `None` = not positioned (skip input routing).
    pub screen_rect: Option<(f32, f32, f32, f32)>,
}

// SAFETY: identical contract to the implicit-view COM fields on FlutterOverlay —
// these handles are only used on the engine's designated threads / under lock.
unsafe impl Send for ViewSurface {}
unsafe impl Sync for ViewSurface {}

impl Drop for ViewSurface {
    fn drop(&mut self) {
        if !self.frame_event.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.frame_event.0);
            }
        }
    }
}

impl ViewSurface {
    /// Wakes the window thread after the engine presented a new frame for this
    /// view.
    pub fn signal_frame(&self) {
        if !self.frame_event.0.is_invalid() {
            unsafe {
                let _ = SetEvent(self.frame_event.0);
            }
        }
    }

    /// GL framebuffer name handed to the engine as this view's backing store.
    /// Returns 0 if GL resources are not yet set up (the engine will get an
    /// invalid FBO and the create callback will fail, which is the correct
    /// fail-closed behaviour).
    pub fn gl_fbo(&self) -> u32 {
        self.gl.as_ref().map(|g| g.fbo).unwrap_or(0)
    }

    /// GL color texture name for this view's FBO.
    pub fn gl_color_texture(&self) -> u32 {
        self.gl.as_ref().map(|g| g.color_texture).unwrap_or(0)
    }

    /// Non-blocking GL flush, ensuring submitted commands reach the GPU before
    /// the host reads the shared texture. There is no keyed mutex on satellite
    /// views (it does not work across the three device round-trips), so GPU
    /// ordering is provided by this flush plus the window thread's present-hold,
    /// which never samples the texture until a fresh frame at the current size
    /// has been confirmed.
    pub fn gl_flush(&self) {
        if let Some(gl) = &self.gl {
            unsafe { (gl.procs.finish)() };
        }
    }

    /// Tears down this view's GL resources (FBO + color texture + pbuffer).
    /// Must run on the render thread with the ANGLE context current. The pbuffer
    /// is destroyed by the caller (it owns the EGL display).
    pub fn delete_gl(&mut self, destroy_pbuffer: impl FnOnce(*mut c_void)) {
        if let Some(gl) = self.gl.take() {
            unsafe {
                if gl.fbo != 0 {
                    (gl.procs.delete_framebuffers)(1, &gl.fbo);
                }
                if gl.color_texture != 0 {
                    (gl.procs.delete_textures)(1, &gl.color_texture);
                }
            }
            destroy_pbuffer(gl.pbuffer_surface);
        }
    }

    /// Builds the GL FBO + color texture for `pbuffer_surface` using `procs`,
    /// binding the pbuffer image to a fresh GL texture and attaching it to a new
    /// FBO. Returns the completed [`ViewGlResources`] or an error string.
    ///
    /// # Safety
    /// Must be called on the render thread with the ANGLE context current and
    /// `pbuffer_surface` already made current (so GL state targets it).
    pub fn build_gl_resources(
        procs: ViewGlProcs,
        pbuffer_surface: *mut c_void,
        bind_tex_image: impl FnOnce(&ViewGlProcs, *mut c_void) -> bool,
    ) -> Result<ViewGlResources, String> {
        unsafe {
            let mut color_texture: u32 = 0;
            (procs.gen_textures)(1, &mut color_texture);
            if color_texture == 0 {
                return Err("glGenTextures returned 0".to_string());
            }
            (procs.bind_texture)(GL_TEXTURE_2D, color_texture);

            // Alias the pbuffer's D3D11-backed image as this GL texture.
            if !bind_tex_image(&procs, pbuffer_surface) {
                (procs.delete_textures)(1, &color_texture);
                return Err("eglBindTexImage failed for view pbuffer".to_string());
            }

            let mut fbo: u32 = 0;
            (procs.gen_framebuffers)(1, &mut fbo);
            if fbo == 0 {
                (procs.delete_textures)(1, &color_texture);
                return Err("glGenFramebuffers returned 0".to_string());
            }
            (procs.bind_framebuffer)(GL_FRAMEBUFFER, fbo);
            (procs.framebuffer_texture_2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                color_texture,
                0,
            );

            let status = (procs.check_framebuffer_status)(GL_FRAMEBUFFER);
            if status != GL_FRAMEBUFFER_COMPLETE {
                (procs.delete_framebuffers)(1, &fbo);
                (procs.delete_textures)(1, &color_texture);
                return Err(format!(
                    "framebuffer incomplete for view pbuffer: status {status:#X}"
                ));
            }

            Ok(ViewGlResources {
                procs,
                pbuffer_surface,
                color_texture,
                fbo,
            })
        }
    }
}
