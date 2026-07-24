use crate::bindings::embedder;

use crate::software_renderer::gl_renderer::nvidia_aftermath;
use crate::software_renderer::multiview::view_surface::ViewGlResources;
use crate::software_renderer::overlay::d3d::create_shared_texture_and_get_handle;
use crate::software_renderer::overlay::overlay_impl::{FlutterOverlay, SendableHandle};

use libloading::{Library, Symbol};
use log::{error, info, warn};
use once_cell::sync::OnceCell;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::thread::current;
use std::{mem, ptr};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

// EGL and OpenGL constants used for ANGLE configuration and operation.

/// Represents the platform's default display connection. Pass this to `eglGetDisplay`
/// to get a handle to the primary display device available to the system.
pub const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut::<c_void>();
/// A null handle for an EGL rendering context. It is used with `eglMakeCurrent`
/// to detach the current rendering context from a thread without attaching a new one.
pub const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut::<c_void>();
/// A null handle for an EGL display connection. Functions that return an `EGLDisplay`
/// will return this value on failure, for instance, if the requested display is not available.
pub const EGL_NO_DISPLAY: *mut c_void = std::ptr::null_mut::<c_void>();
/// A null handle for an EGL drawing surface. Functions that create a window, pbuffer,
/// or pixmap surface will return this value if the surface cannot be created.
pub const EGL_NO_SURFACE: *mut c_void = std::ptr::null_mut::<c_void>();
/// The boolean `true` value for EGL operations. EGL functions returning a boolean
/// success status will return this value.
pub const EGL_TRUE: i32 = 1;
/// A special token used to terminate attribute lists that are passed to functions like
/// `eglChooseConfig` and `eglCreateContext`. It signals the end of the list of key-value pairs.
pub const EGL_NONE: i32 = 0x3038;
/// The value returned by `eglGetError` when the most recently called EGL function
/// completed without any errors.
pub const EGL_SUCCESS: i32 = 0x3000;
/// The value returned by `eglGetError` when the EGL context has been lost due to
/// device reset or removal. Recovery requires recreating all EGL resources.
pub const EGL_CONTEXT_LOST: i32 = 0x300E;

/// An attribute key used to specify or query the width of a drawing surface in pixels.
/// Used when creating pbuffer surfaces or querying any surface's dimensions.
pub const EGL_WIDTH: i32 = 0x3057;
/// An attribute key used to specify or query the height of a drawing surface in pixels.
/// Used when creating pbuffer surfaces or querying any surface's dimensions.
pub const EGL_HEIGHT: i32 = 0x3056;
/// Pbuffer texture format attribute. `EGL_TEXTURE_RGBA` makes the pbuffer
/// bindable to a GL texture (render-to-texture) via `eglBindTexImage`.
pub const EGL_TEXTURE_FORMAT: i32 = 0x3080;
pub const EGL_TEXTURE_RGBA: i32 = 0x305E;
/// Pbuffer texture target attribute; `EGL_TEXTURE_2D` binds to a 2D texture.
pub const EGL_TEXTURE_TARGET: i32 = 0x3081;
pub const EGL_TEXTURE_2D: i32 = 0x305F;
/// An ANGLE-specific extension attribute used for operations involving Direct3D 11 textures.
pub const EGL_D3D11_TEXTURE_ANGLE: i32 = 0x3484;
/// An OpenGL extension token for a pixel format where color components are ordered
/// Blue, Green, Red, and Alpha. This is a common texture format on Windows.
pub const GL_BGRA_EXT: i32 = 0x87;

/// An attribute for `eglCreateContext` that specifies the desired version of the client API.
/// For example, setting this to `2` requests an OpenGL ES 2.x context.
pub const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;
/// An attribute of an `EGLConfig` that specifies which types of drawing surfaces (window,
/// pbuffer, or pixmap) can be created with it. The value is a bitmask.
pub const EGL_SURFACE_TYPE: i32 = 0x3033;
/// A bit for the `EGL_SURFACE_TYPE` attribute, indicating that an `EGLConfig`
/// supports creating offscreen pixel buffer (pbuffer) surfaces.
pub const EGL_PBUFFER_BIT: i32 = 0x0001;
/// An attribute of an `EGLConfig` that specifies which client APIs (like OpenGL ES or OpenVG)
/// can render to surfaces created with it. The value is a bitmask.
pub const EGL_RENDERABLE_TYPE: i32 = 0x3040;
/// A bit for the `EGL_RENDERABLE_TYPE` attribute, indicating that an `EGLConfig`
/// supports rendering with the OpenGL ES 2.x API.
pub const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
/// An attribute specifying the number of bits for the red color channel.
pub const EGL_RED_SIZE: i32 = 0x3024;
/// An attribute specifying the number of bits for the green color channel.
pub const EGL_GREEN_SIZE: i32 = 0x3023;
/// An attribute specifying the number of bits for the blue color channel.
pub const EGL_BLUE_SIZE: i32 = 0x3022;
/// An attribute specifying the number of bits for the alpha (transparency) channel.
pub const EGL_ALPHA_SIZE: i32 = 0x3021;
/// An attribute specifying the number of bits for the depth (Z-buffer).
pub const EGL_DEPTH_SIZE: i32 = 0x3025;
/// An attribute specifying the number of bits for the stencil buffer.
pub const EGL_STENCIL_SIZE: i32 = 0x3026;

/// A token identifying the ANGLE platform for use with `eglGetPlatformDisplay`.
pub const EGL_PLATFORM_ANGLE_ANGLE: i32 = 0x3202;
/// An attribute key used to select the underlying rendering backend for ANGLE
/// (e.g., D3D11, D3D9, OpenGL).
pub const EGL_PLATFORM_ANGLE_TYPE_ANGLE: i32 = 0x3203;
/// A value for `EGL_PLATFORM_ANGLE_TYPE_ANGLE` that explicitly selects the
/// Direct3D 11 rendering backend.
pub const EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE: i32 = 0x3208;
/// A boolean attribute that, when enabled, allows ANGLE's D3D11 backend to
/// automatically release and reallocate its internal texture cache to save memory.
pub const EGL_PLATFORM_ANGLE_ENABLE_AUTOMATIC_TRIM_ANGLE: i32 = 0x320F;
/// An experimental ANGLE attribute to control the presentation path for swap chains,
/// allowing for optimizations like bypassing the DWM compositor.
pub const EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE: i32 = 0x33A4;
/// A value for `EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE` that requests a fast,
/// low-latency presentation path, often used for applications like games.
pub const EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE: i32 = 0x33A9;

// --- ANGLE Device and Texture Extensions ---

/// An attribute for `eglQueryDisplayAttribEXT` that retrieves the EGL device
/// associated with an EGL display.
pub const EGL_DEVICE_EXT: i32 = 0x322C;
/// An attribute for `eglQueryDeviceAttribEXT` that retrieves the underlying
/// `ID3D11Device` pointer from an EGL device when using the D3D11 backend.
pub const EGL_D3D11_DEVICE_ANGLE: i32 = 0x33A1;
/// An ANGLE-specific attribute that enables the D3D11 debug layer for the device
/// created by ANGLE. Requires the D3D11 SDK debug layer to be installed.
pub const EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE: i32 = 0x3209;
/// Value for EGL_PLATFORM_ANGLE_DEVICE_TYPE_ANGLE to request a hardware device (default).
pub const EGL_PLATFORM_ANGLE_DEVICE_TYPE_HARDWARE_ANGLE: i32 = 0x320A;
/// An ANGLE-specific attribute to enable D3D11 debug validation on the device.
pub const EGL_PLATFORM_ANGLE_DEBUG_LAYERS_ENABLED_ANGLE: i32 = 0x3451;
/// A buffer type for `eglCreatePbufferFromClientBuffer` that indicates the client
/// buffer is a Direct3D texture.
pub const EGL_D3D_TEXTURE_ANGLE: i32 = 0x33A3;
/// An attribute to query the internal format of an EGL surface created from a
/// client buffer, used for format verification.
pub const EGL_TEXTURE_INTERNAL_FORMAT_ANGLE: i32 = 0x345D;

/// Defines the signature for the `eglGetProcAddress` function, which is the core
/// mechanism for dynamically resolving pointers to all other EGL and GL extension functions.
type EglGetProcAddress = unsafe extern "C" fn(*const i8) -> *mut c_void;
/// A type alias for the integer type used by EGL to represent boolean values,
/// where `EGL_TRUE` (1) and `EGL_FALSE` (0) are the standard values.
type EGLBoolean = i32;
/// Defines the signature for `eglGetPlatformDisplay`, used to obtain an `EGLDisplay`
/// handle for a specific platform (like ANGLE) with custom initialization attributes.
type EglGetPlatformDisplayEXT = unsafe extern "C" fn(i32, *mut c_void, *const i32) -> *mut c_void;
/// Defines the signature for `eglInitialize`, which must be called to initialize the
/// EGL implementation for a given `EGLDisplay` before other operations can be performed.
type EglInitialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> bool;
/// Defines the signature for `eglChooseConfig`, which queries the EGL implementation
/// for an `EGLConfig` that matches a set of specified requirements (e.g., color depth, API support).
type EglChooseConfig =
    unsafe extern "C" fn(*mut c_void, *const i32, *mut *mut c_void, i32, *mut i32) -> bool;
/// Defines the signature for `eglCreateContext`, which creates a rendering context
/// for a specific client API (e.g., OpenGL ES 2) that can be used for drawing operations.
type EglCreateContext =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
/// Defines the signature for `eglMakeCurrent`, which binds a rendering context to the
/// current thread and associates it with drawing and reading surfaces. This is a prerequisite
/// for issuing any rendering commands.
type EglMakeCurrent =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> i32;
/// Defines the signature for `eglDestroyContext`, used to release all resources
/// associated with a rendering context once it is no longer needed.
type EglDestroyContext = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;
/// Defines the signature for `eglTerminate`, which releases all resources associated
/// with a specific EGL display connection. This is the counterpart to `eglInitialize`.
type EglTerminate = unsafe extern "C" fn(*mut c_void) -> bool;
/// Defines the signature for `eglGetError`, which returns the error code for the
/// last EGL operation that failed on the current thread, allowing for detailed error handling.
type EglGetError = unsafe extern "C" fn() -> i32;
/// Defines the signature for the `eglQueryDisplayAttribEXT` extension function, which
/// retrieves specific attributes about an EGL display, such as the underlying native device.
type EglQueryDisplayAttribEXT = unsafe extern "C" fn(*mut c_void, i32, *mut isize) -> bool;
/// Defines the signature for the `eglQueryDeviceAttribEXT` extension function, which
/// retrieves attributes about an EGL device, such as the `ID3D11Device` pointer.
type EglQueryDeviceAttribEXT = unsafe extern "C" fn(*mut c_void, i32, *mut isize) -> bool;
/// Defines the signature for `glFinish`, an OpenGL command that blocks the calling
/// thread until all previously submitted rendering commands have been fully completed by the GPU.
type GlFinish = unsafe extern "C" fn();
/// Defines the signature for `glFlush`, an OpenGL command that ensures all previously
/// submitted commands are dispatched to the GPU without waiting for completion.
type GlFlush = unsafe extern "C" fn();
/// Defines the signature for `eglCreatePbufferFromClientBuffer`, used to create an
/// EGL pbuffer surface that wraps an existing native graphics resource, such as a Direct3D texture.
/// This is a key function for GPU-level interoperability.
type EglCreatePbufferFromClientBuffer =
    unsafe extern "C" fn(*mut c_void, u32, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
/// Defines the signature for `eglDestroySurface`, which releases all resources
/// associated with an EGL surface (window, pbuffer, or pixmap).
type EglDestroySurface = unsafe extern "C" fn(*mut c_void, *mut c_void) -> bool;

// --- GL entry points needed for per-view framebuffer backing stores ---
//
// The multi-view compositor hands Flutter a GL framebuffer per view. We create
// that framebuffer here: a color texture is bound from the view's pbuffer
// surface (via eglBindTexImage), then attached to an FBO. These are resolved
// lazily on the render thread the first time a view's resources are built.

/// `glGenFramebuffers(GLsizei n, GLuint* framebuffers)`.
type GlGenFramebuffers = unsafe extern "C" fn(i32, *mut u32);
/// `glBindFramebuffer(GLenum target, GLuint framebuffer)`.
type GlBindFramebuffer = unsafe extern "C" fn(u32, u32);
/// `glGenTextures(GLsizei n, GLuint* textures)`.
type GlGenTextures = unsafe extern "C" fn(i32, *mut u32);
/// `glBindTexture(GLenum target, GLuint texture)`.
type GlBindTexture = unsafe extern "C" fn(u32, u32);
/// `glFramebufferTexture2D(GLenum target, GLenum attachment, GLenum textarget, GLuint texture, GLint level)`.
type GlFramebufferTexture2D = unsafe extern "C" fn(u32, u32, u32, u32, i32);
/// `glDeleteFramebuffers(GLsizei n, const GLuint* framebuffers)`.
type GlDeleteFramebuffers = unsafe extern "C" fn(i32, *const u32);
/// `glDeleteTextures(GLsizei n, const GLuint* textures)`.
type GlDeleteTextures = unsafe extern "C" fn(i32, *const u32);
/// `glCheckFramebufferStatus(GLenum target) -> GLenum`.
type GlCheckFramebufferStatus = unsafe extern "C" fn(u32) -> u32;
/// `eglBindTexImage(EGLDisplay, EGLSurface, EGLint buffer) -> EGLBoolean`.
type EglBindTexImage = unsafe extern "C" fn(*mut c_void, *mut c_void, i32) -> i32;

/// GL/EGL constants for framebuffer creation.
pub const GL_FRAMEBUFFER: u32 = 0x8D40;
pub const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
pub const GL_TEXTURE_2D: u32 = 0x0DE1;
pub const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
pub const EGL_BACK_BUFFER: i32 = 0x3084;

/// GL/EGL function pointers resolved once and shared by all secondary views.
/// These are resolved through the same `eglGetProcAddress` used by the engine.
#[derive(Clone, Copy)]
pub struct ViewGlProcs {
    pub gen_framebuffers: GlGenFramebuffers,
    pub bind_framebuffer: GlBindFramebuffer,
    pub gen_textures: GlGenTextures,
    pub bind_texture: GlBindTexture,
    pub framebuffer_texture_2d: GlFramebufferTexture2D,
    pub delete_framebuffers: GlDeleteFramebuffers,
    pub delete_textures: GlDeleteTextures,
    pub check_framebuffer_status: GlCheckFramebufferStatus,
    pub bind_tex_image: EglBindTexImage,
    pub flush: GlFlush,
    pub finish: GlFinish,
}

unsafe impl Send for ViewGlProcs {}
unsafe impl Sync for ViewGlProcs {}

impl ViewGlProcs {
    /// Resolves all GL/EGL entry points via the shared `eglGetProcAddress`.
    /// Must be called after [`get_or_init_shared_egl`] has succeeded (i.e. after
    /// at least one [`AngleInteropState::new`]).
    pub fn resolve() -> Result<Self, String> {
        let shared = SHARED_EGL
            .get()
            .ok_or_else(|| "SHARED_EGL not initialized before ViewGlProcs::resolve".to_string())?;
        let get = |name: &str| -> Result<*mut c_void, String> {
            let c = CString::new(name).map_err(|e| e.to_string())?;
            let p = unsafe { (shared.egl_get_proc_address)(c.as_ptr()) };
            if p.is_null() {
                Err(format!("eglGetProcAddress returned null for {name}"))
            } else {
                Ok(p)
            }
        };
        unsafe {
            Ok(Self {
                gen_framebuffers: mem::transmute::<*mut c_void, GlGenFramebuffers>(get(
                    "glGenFramebuffers",
                )?),
                bind_framebuffer: mem::transmute::<*mut c_void, GlBindFramebuffer>(get(
                    "glBindFramebuffer",
                )?),
                gen_textures: mem::transmute::<*mut c_void, GlGenTextures>(get("glGenTextures")?),
                bind_texture: mem::transmute::<*mut c_void, GlBindTexture>(get("glBindTexture")?),
                framebuffer_texture_2d: mem::transmute::<*mut c_void, GlFramebufferTexture2D>(get(
                    "glFramebufferTexture2D",
                )?),
                delete_framebuffers: mem::transmute::<*mut c_void, GlDeleteFramebuffers>(get(
                    "glDeleteFramebuffers",
                )?),
                delete_textures: mem::transmute::<*mut c_void, GlDeleteTextures>(get(
                    "glDeleteTextures",
                )?),
                check_framebuffer_status: mem::transmute::<*mut c_void, GlCheckFramebufferStatus>(
                    get("glCheckFramebufferStatus")?,
                ),
                bind_tex_image: mem::transmute::<*mut c_void, EglBindTexImage>(get(
                    "eglBindTexImage",
                )?),
                flush: mem::transmute::<*mut c_void, GlFlush>(get("glFlush")?),
                finish: mem::transmute::<*mut c_void, GlFinish>(get("glFinish")?),
            })
        }
    }
}

///
/// Converts a raw EGL error code into a human-readable string literal.
///
pub(crate) fn egl_error_to_string(error_code: i32) -> &'static str {
    match error_code {
        0x3000 => "EGL_SUCCESS",
        0x3001 => "EGL_NOT_INITIALIZED",
        0x3002 => "EGL_BAD_ACCESS",
        0x3003 => "EGL_BAD_ALLOC",
        0x3004 => "EGL_BAD_ATTRIBUTE",
        0x3005 => "EGL_BAD_CONFIG",
        0x3006 => "EGL_BAD_CONTEXT",
        0x3007 => "EGL_BAD_CURRENT_SURFACE",
        0x3008 => "EGL_BAD_DISPLAY",
        0x3009 => "EGL_BAD_MATCH",
        0x300A => "EGL_BAD_NATIVE_PIXMAP",
        0x300B => "EGL_BAD_NATIVE_WINDOW",
        0x300C => "EGL_BAD_PARAMETER",
        0x300D => "EGL_BAD_SURFACE",
        0x300E => "EGL_CONTEXT_LOST",
        _ => "Unknown EGL error",
    }
}

///
/// Retrieves the last EGL error using the provided function pointer and logs it
/// to the error channel if an error has occurred.
///
fn log_egl_error(func: &str, line: u32, egl_get_error_fn: EglGetError) {
    let code = unsafe { egl_get_error_fn() };
    if code != EGL_SUCCESS {
        error!(
            "[ANGLE DEBUG] EGL Error in {}:{} -> {} ({:#X})",
            func,
            line,
            egl_error_to_string(code),
            code
        );
    }
}

///
/// Builds the EGL display attributes array for ANGLE initialization.
/// Debug layers are enabled only if the `ANGLE_DEBUG_LAYERS_ENABLED` environment variable is set.
///
pub(crate) fn build_display_attributes() -> Vec<i32> {
    let debug_layers_enabled = std::env::var("ANGLE_DEBUG_LAYERS_ENABLED").is_ok();

    let mut attrs = vec![
        EGL_PLATFORM_ANGLE_TYPE_ANGLE,
        EGL_PLATFORM_ANGLE_TYPE_D3D11_ANGLE,
        EGL_PLATFORM_ANGLE_ENABLE_AUTOMATIC_TRIM_ANGLE,
        EGL_TRUE,
        EGL_EXPERIMENTAL_PRESENT_PATH_ANGLE,
        EGL_EXPERIMENTAL_PRESENT_PATH_FAST_ANGLE,
    ];

    if debug_layers_enabled {
        attrs.push(EGL_PLATFORM_ANGLE_DEBUG_LAYERS_ENABLED_ANGLE);
        attrs.push(EGL_TRUE);
    }

    attrs.push(EGL_NONE);
    attrs
}

///
/// Global, thread-safe, lazily-initialized container for the shared EGL state.
/// This ensures that ANGLE libraries are loaded exactly once per process.
///
static SHARED_EGL: OnceCell<SharedEglState> = OnceCell::new();

///
/// Holds the process-wide, shared handles to the loaded ANGLE libraries (`libEGL.dll`,
/// `libGLESv2.dll`) and the core `eglGetProcAddress` function pointer.
/// This struct is initialized once by `get_or_init_shared_egl` and then shared
/// across all `AngleInteropState` instances to ensure consistency.
///
struct SharedEglState {
    libegl: Library,
    _libgles: Library,
    egl_get_proc_address: EglGetProcAddress,
}

/// Manages the state of an ANGLE EGL environment for Direct3D 11 interoperability.
///
/// This struct encapsulates all resources required to render an OpenGL ES client (like Flutter)
/// into an offscreen Direct3D 11 texture. It orchestrates the initialization of ANGLE with a
/// D3D11 backend, creates and manages the EGL contexts and surfaces, and provides the
/// fundamental synchronization and interoperability needed for the host application to consume
/// the rendered frames.
#[derive(Debug)]
pub struct AngleInteropState {
    /// Function pointer to `eglMakeCurrent`, used by the engine's callbacks to activate the
    /// appropriate context (`context` or `resource_context`) on the correct thread before
    /// rendering or resource operations can begin.
    pub egl_make_current: EglMakeCurrent,

    /// Function pointer to `eglGetError`, serving as the internal mechanism for turning
    /// numerical EGL error codes into human-readable logs, which is crucial for debugging
    /// the complex interop setup.
    egl_get_error: EglGetError,

    /// Function pointer to `eglDestroyContext`, utilized during the `drop` process to clean up
    /// both the main and resource contexts, ensuring no GPU resources are leaked.
    egl_destroy_context: EglDestroyContext,

    /// Function pointer to `eglTerminate`, which performs the final cleanup step in the `drop`
    /// implementation by severing the connection to the ANGLE EGL driver and releasing all
    /// associated memory.
    egl_terminate: EglTerminate,

    /// Function pointer to `eglCreateContext`, used during initialization to create the two
    /// EGL rendering contexts managed by this state: the main `context` for rendering and
    /// the shared `resource_context` for background asset loading.
    egl_create_context: EglCreateContext,

    /// Function pointer to `glFinish`, used during device recovery and resize to ensure
    /// all GL commands complete before recreating resources.
    gl_finish: GlFinish,
    /// Function pointer to `glFlush`, non-blocking variant used in the present callback.
    gl_flush: GlFlush,

    /// Function pointer to `eglCreatePbufferFromClientBuffer`, the most critical function
    /// for this interoperability. It is used to create the `pbuffer_surface` by wrapping a
    /// native D3D11 texture handle, which directs the EGL client's rendering output into a D3D object.
    egl_create_pbuffer_from_client_buffer: EglCreatePbufferFromClientBuffer,

    /// Function pointer to `eglDestroySurface`, used to destroy the `pbuffer_surface` when
    /// resources are recreated (e.g., on resize) and during final cleanup in `drop`.
    egl_destroy_surface: EglDestroySurface,

    /// The handle to the ANGLE EGL implementation (`EGLDisplay`), configured specifically to
    /// use the D3D11 backend. It is the root object for all other state managed by this struct.
    pub display: *mut c_void,

    /// The main rendering context (`EGLContext`) that Flutter's raster thread will use.
    /// All drawing commands from the Flutter engine are executed within this context.
    pub context: *mut c_void,

    /// A secondary, resource-loading context (`EGLContext`) that shares its resource
    /// namespace (textures, shaders) with the main `context`. It is intended for use on a
    /// background thread to allow asynchronous asset compilation without stalling the renderer.
    pub resource_context: *mut c_void,

    /// A handle to the underlying `ID3D11Device` that ANGLE created. This is a key
    /// "output" of the initialization, as this device is used by the host application to create
    /// the shared texture that this struct will render into.
    pub angle_d3d11_device: ID3D11Device,

    /// The framebuffer configuration (`EGLConfig`) chosen during setup. It serves as a
    /// blueprint that guarantees the contexts and the pbuffer surface are all compatible and
    /// meet the necessary rendering requirements (e.g., 8-bit RGBA channels).
    config: *mut c_void,

    /// The handle to the EGL pbuffer surface which acts as the "bridge" between the
    /// GL and D3D worlds. While it is a valid `EGLSurface` for the GL client to target, its
    /// backing store is a D3D11 texture, making the rendering results immediately available to the host.
    pub pbuffer_surface: *mut c_void,

    /// A runtime safety check that stores the ID of the thread where the main `context`
    /// was first made current. This is used to assert that the non-thread-safe context is
    /// only ever accessed from its designated raster thread.
    main_thread_id: Option<std::thread::ThreadId>,

    /// A runtime safety check for the `resource_context`. It ensures that all operations
    /// on the resource context are confined to its designated background thread.
    resource_thread_id: Option<std::thread::ThreadId>,

    /// Flag indicating that a device lost condition has been detected and recovery is needed.
    /// When true, the next call to make_current should attempt to reinitialize.
    pub device_lost: bool,

    /// Pending resize dimensions. When set, make_current_callback will call
    /// recreate_resources on the render thread (where it's safe) before proceeding.
    pub pending_resize: Option<(u32, u32)>,

    /// Old pbuffer surfaces pending deferred destruction, drained on the render thread.
    /// A queue, not a single slot: back-to-back resizes must not clobber an undestroyed surface.
    pub old_pbuffer_surfaces: Vec<*mut c_void>,
}

impl AngleInteropState {
    ///
    /// Creates and initializes a new ANGLE interop context for a Flutter overlay.
    ///
    /// This function orchestrates the entire ANGLE setup, including obtaining the shared
    /// EGL state, creating an EGL display, initializing EGL, querying for a D3D11
    /// device created by ANGLE, and preparing EGL contexts and configurations.
    ///
    /// # Arguments
    ///
    /// * `engine_dir`: An optional path to the directory containing `libEGL.dll` and
    ///   `libGLESv2.dll`. This path is only used during the very first initialization
    ///   of the shared EGL state within the process. Subsequent calls will ignore this
    ///   parameter and reuse the existing shared state.
    ///
    /// # Returns
    ///
    /// A `Result` containing the fully initialized `AngleInteropState` on success,
    /// or an error string on failure.
    ///
    pub fn new(engine_dir: Option<&Path>) -> Result<Box<Self>, String> {
        unsafe {
            info!("[AngleInterop] Initializing ANGLE and letting it create a D3D11 device...");

            let shared_egl = get_or_init_shared_egl(engine_dir)?;

            let get_proc = |name: &str| -> *mut c_void {
                let c_name = CString::new(name).unwrap();
                (shared_egl.egl_get_proc_address)(c_name.as_ptr())
            };

            let get_proc_assert = |name: &str| {
                get_proc(name)
            };

            let proc_ptr = get_proc("eglGetPlatformDisplayEXT");

            if proc_ptr.is_null() {
                return Err("eglGetPlatformDisplayEXT not available".to_string());
            }

            let egl_get_platform_display_ext: EglGetPlatformDisplayEXT = mem::transmute(proc_ptr);
            let egl_initialize: EglInitialize = mem::transmute(get_proc("eglInitialize"));
            let egl_get_error: EglGetError = mem::transmute(get_proc("eglGetError"));

            let display_attributes = build_display_attributes();

            let display = egl_get_platform_display_ext(
                EGL_PLATFORM_ANGLE_ANGLE,
                EGL_DEFAULT_DISPLAY,
                display_attributes.as_ptr(),
            );

            if display == EGL_NO_DISPLAY {
                log_egl_error("eglGetPlatformDisplayEXT", line!(), egl_get_error);
                return Err("Failed to get EGL display.".to_string());
            }

            if let Err(e) = nvidia_aftermath::enable_gpu_crash_dumps(engine_dir) {
                warn!("[AngleInterop] Failed to enable Aftermath: {e}");
            }

            if !egl_initialize(display, ptr::null_mut(), ptr::null_mut()) {
                log_egl_error("eglInitialize", line!(), egl_get_error);
                return Err("Failed to initialize EGL.".to_string());
            }

            let egl_query_display_attrib_ext: EglQueryDisplayAttribEXT =
                mem::transmute(get_proc_assert("eglQueryDisplayAttribEXT"));
            let egl_query_device_attrib_ext: EglQueryDeviceAttribEXT =
                mem::transmute(get_proc_assert("eglQueryDeviceAttribEXT"));

            let mut egl_device: isize = 0;
            if !egl_query_display_attrib_ext(display, EGL_DEVICE_EXT, &mut egl_device) {
                log_egl_error("eglQueryDisplayAttribEXT", line!(), egl_get_error);
                return Err("Failed to query EGL display attribute for device.".to_string());
            }

            let mut d3d11_device_ptr: isize = 0;
            if !egl_query_device_attrib_ext(
                egl_device as *mut c_void,
                EGL_D3D11_DEVICE_ANGLE,
                &mut d3d11_device_ptr,
            ) {
                log_egl_error("eglQueryDeviceAttribEXT", line!(), egl_get_error);
                return Err("Failed to query EGL device attribute for D3D11 device.".to_string());
            }

            if d3d11_device_ptr == 0 {
                return Err("ANGLE created a null D3D11 device.".to_string());
            }

            let angle_d3d11_device: ID3D11Device = Interface::from_raw(d3d11_device_ptr as *mut _);

            // Enable D3D11 multithread protection - CRITICAL for thread safety!
            // Flutter uses multiple threads (raster, resource) that can call D3D11 simultaneously.
            // Without this, concurrent access causes memory corruption and crashes.
            if let Ok(multithread) = angle_d3d11_device.cast::<ID3D10Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            } else {
                warn!(
                    "[AngleInterop] Failed to enable D3D11 multithread protection - thread safety not guaranteed!"
                );
            }

            if let Err(e) = nvidia_aftermath::initialize_d3d11_device(&angle_d3d11_device) {
                warn!(
                    "[AngleInterop] Failed to initialize Aftermath for D3D11 device: {e}"
                );
            }

            let egl_choose_config: EglChooseConfig =
                mem::transmute(get_proc_assert("eglChooseConfig"));

            let egl_create_context: EglCreateContext =
                mem::transmute(get_proc_assert("eglCreateContext"));
            let egl_make_current: EglMakeCurrent =
                mem::transmute(get_proc_assert("eglMakeCurrent"));
            let egl_destroy_context: EglDestroyContext =
                mem::transmute(get_proc_assert("eglDestroyContext"));
            let egl_terminate: EglTerminate = mem::transmute(get_proc_assert("eglTerminate"));
            let gl_finish: GlFinish = mem::transmute(get_proc_assert("glFinish"));
            let gl_flush: GlFlush = mem::transmute(get_proc_assert("glFlush"));
            let egl_create_pbuffer_from_client_buffer: EglCreatePbufferFromClientBuffer =
                mem::transmute(get_proc_assert("eglCreatePbufferFromClientBuffer"));
            let egl_destroy_surface: EglDestroySurface =
                mem::transmute(get_proc_assert("eglDestroySurface"));

            let config_attribs = [
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_DEPTH_SIZE,
                8,
                EGL_STENCIL_SIZE,
                8,
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES2_BIT,
                EGL_NONE,
            ];
            let mut config: *mut c_void = ptr::null_mut();
            let mut num_config = 0;

            if !egl_choose_config(
                display,
                config_attribs.as_ptr(),
                &mut config,
                1,
                &mut num_config,
            ) || num_config == 0
            {
                return Err("eglChooseConfig failed.".to_string());
            }

            info!("[AngleInterop] ANGLE initialized successfully with provided device.");
            Ok(Box::new(Self {
                egl_make_current,
                egl_get_error,
                egl_destroy_context,
                egl_terminate,
                egl_create_context,
                gl_finish,
                gl_flush,
                egl_create_pbuffer_from_client_buffer,
                egl_destroy_surface,
                display,
                context: EGL_NO_CONTEXT,
                resource_context: EGL_NO_CONTEXT,
                angle_d3d11_device,
                config,
                pbuffer_surface: EGL_NO_SURFACE,
                main_thread_id: None,
                resource_thread_id: None,
                device_lost: false,
                pending_resize: None,
                old_pbuffer_surfaces: Vec::new(),
            }))
        }
    }

    ///
    /// Returns a cloned handle to the D3D11 device that was created and is managed by ANGLE.
    ///
    pub fn get_d3d_device(&self) -> Result<ID3D11Device, String> {
        Ok(self.angle_d3d11_device.clone())
    }

    /// The EGL display this state is bound to. Shared by secondary views so they
    /// live on the same ANGLE device/context as the implicit view.
    pub fn display(&self) -> *mut c_void {
        self.display
    }

    /// The chosen EGL config. Secondary-view pbuffer surfaces must use the same
    /// config to be compatible with the shared context.
    pub fn config(&self) -> *mut c_void {
        self.config
    }

    /// The main rendering context. Secondary views render on the same raster
    /// thread under this context; their FBOs are just additional render targets.
    pub fn context(&self) -> *mut c_void {
        self.context
    }

    /// Creates a pbuffer surface wrapping `d3d_texture` (a shared D3D11 texture
    /// on ANGLE's device) so GL can render into it. Returns the EGL surface
    /// handle. The caller owns the surface and must destroy it via
    /// [`destroy_pbuffer`].
    ///
    /// # Safety
    /// Must be called on the render thread with this state's context current.
    pub unsafe fn create_pbuffer_for_texture(
        &self,
        d3d_texture: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<*mut c_void, String> {
        unsafe {
            // Texture-target pbuffer: EGL_TEXTURE_FORMAT/EGL_TEXTURE_TARGET make
            // the surface bindable to a GL texture via eglBindTexImage, which is
            // how satellite views attach the shared D3D11 texture to their own
            // FBO. (The implicit view uses FBO 0 directly and does not need this.)
            let attribs = [
                EGL_WIDTH,
                width as i32,
                EGL_HEIGHT,
                height as i32,
                EGL_TEXTURE_FORMAT,
                EGL_TEXTURE_RGBA,
                EGL_TEXTURE_TARGET,
                EGL_TEXTURE_2D,
                EGL_NONE,
            ];
            let surface = (self.egl_create_pbuffer_from_client_buffer)(
                self.display,
                EGL_D3D_TEXTURE_ANGLE as u32,
                d3d_texture.as_raw(),
                self.config,
                attribs.as_ptr(),
            );
            if surface == EGL_NO_SURFACE {
                let code = (self.egl_get_error)();
                return Err(format!(
                    "create_pbuffer_for_texture failed: {} ({:#X})",
                    egl_error_to_string(code),
                    code
                ));
            }
            Ok(surface)
        }
    }

    /// Destroys a pbuffer surface previously created by
    /// [`create_pbuffer_for_texture`].
    ///
    /// # Safety
    /// `surface` must be a pbuffer created against this state's display.
    pub unsafe fn destroy_pbuffer(&self, surface: *mut c_void) {
        if surface != EGL_NO_SURFACE {
            unsafe {
                (self.egl_destroy_surface)(self.display, surface);
            }
        }
    }

    /// Binds `surface` and `context` current on the calling (render) thread.
    /// Used by secondary views before issuing GL into their FBO.
    ///
    /// `eglBindTexImage`-binds the pbuffer `surface` to the currently-bound GL
    /// texture as `GL_BACK_BUFFER`, using the provided proc table.
    ///
    /// # Safety
    /// `surface` must be a valid pbuffer for this state's display and a GL
    /// texture must be bound on the current thread.
    pub unsafe fn bind_tex_image(&self, procs: &ViewGlProcs, surface: *mut c_void) -> bool {
        unsafe { (procs.bind_tex_image)(self.display, surface, EGL_BACK_BUFFER) != 0 }
    }

    ///
    /// Checks if the ANGLE device has been lost and needs recovery.
    ///
    pub fn is_device_lost(&self) -> bool {
        self.device_lost
    }

    ///
    /// Attempts to reset the device lost state and prepare for recovery.
    /// This cleans up the existing contexts and surface so they can be recreated.
    /// Returns true if cleanup was successful and recovery can be attempted.
    ///
    pub fn prepare_for_recovery(&mut self) -> bool {
        if !self.device_lost {
            return true;
        }
        unsafe {
            self.cleanup_surface_resources();

            if self.context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.context);
                self.context = EGL_NO_CONTEXT;
            }
            if self.resource_context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.resource_context);
                self.resource_context = EGL_NO_CONTEXT;
            }

            self.main_thread_id = None;
            self.resource_thread_id = None;

            self.device_lost = false;
        }

        true
    }

    /// Performs a full reinitialization of the ANGLE/EGL state after a device lost condition.
    pub fn full_reinitialize(&mut self) -> Result<(), String> {
        if nvidia_aftermath::is_enabled() {
            info!("[AngleInterop] Waiting for Aftermath crash dump collection...");
            nvidia_aftermath::wait_for_crash_dump(3000);
        }

        unsafe {
            self.cleanup_surface_resources();

            if self.context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.context);
                self.context = EGL_NO_CONTEXT;
            }
            if self.resource_context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.resource_context);
                self.resource_context = EGL_NO_CONTEXT;
            }

            if self.display != EGL_NO_DISPLAY {
                info!("[AngleInterop] Terminating dead EGL display...");
                (self.egl_terminate)(self.display);
                self.display = EGL_NO_DISPLAY;
            }

            self.main_thread_id = None;
            self.resource_thread_id = None;

            let shared_egl = get_or_init_shared_egl(None)?;

            let get_proc = |name: &str| -> *mut c_void {
                let c_name = CString::new(name).unwrap();
                (shared_egl.egl_get_proc_address)(c_name.as_ptr())
            };

            let egl_get_error: EglGetError = mem::transmute(get_proc("eglGetError"));
            let egl_get_platform_display_ext: EglGetPlatformDisplayEXT =
                mem::transmute(get_proc("eglGetPlatformDisplayEXT"));
            let egl_initialize: EglInitialize = mem::transmute(get_proc("eglInitialize"));

            let _ = egl_get_error();

            let display_attributes = build_display_attributes();

            let new_display = egl_get_platform_display_ext(
                EGL_PLATFORM_ANGLE_ANGLE,
                EGL_DEFAULT_DISPLAY,
                display_attributes.as_ptr(),
            );

            if new_display == EGL_NO_DISPLAY {
                let error_code = egl_get_error();
                return Err(format!(
                    "Failed to get new EGL display during recovery: {} ({:#X})",
                    egl_error_to_string(error_code),
                    error_code
                ));
            }

            if !egl_initialize(new_display, ptr::null_mut(), ptr::null_mut()) {
                let error_code = egl_get_error();
                return Err(format!(
                    "Failed to initialize new EGL display during recovery: {} ({:#X})",
                    egl_error_to_string(error_code),
                    error_code
                ));
            }

            info!("[AngleInterop] New EGL display created and initialized successfully.");

            let _ = egl_get_error();

            let egl_query_display_attrib_ext: EglQueryDisplayAttribEXT =
                mem::transmute(get_proc("eglQueryDisplayAttribEXT"));
            let egl_query_device_attrib_ext: EglQueryDeviceAttribEXT =
                mem::transmute(get_proc("eglQueryDeviceAttribEXT"));

            let mut egl_device: isize = 0;
            if !egl_query_display_attrib_ext(new_display, EGL_DEVICE_EXT, &mut egl_device) {
                let error_code = egl_get_error();
                return Err(format!(
                    "Failed to query EGL device during recovery: {} ({:#X})",
                    egl_error_to_string(error_code),
                    error_code
                ));
            }

            let mut d3d11_device_ptr: isize = 0;
            if !egl_query_device_attrib_ext(
                egl_device as *mut c_void,
                EGL_D3D11_DEVICE_ANGLE,
                &mut d3d11_device_ptr,
            ) {
                let error_code = egl_get_error();
                return Err(format!(
                    "Failed to query D3D11 device during recovery: {} ({:#X})",
                    egl_error_to_string(error_code),
                    error_code
                ));
            }

            if d3d11_device_ptr == 0 {
                return Err("ANGLE created a null D3D11 device during recovery.".to_string());
            }

            let new_d3d11_device: ID3D11Device = Interface::from_raw(d3d11_device_ptr as *mut _);

            if let Ok(multithread) = new_d3d11_device.cast::<ID3D10Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            } else {
                warn!(
                    "[AngleInterop] Failed to enable D3D11 multithread protection on recovered device"
                );
            }

            if let Err(e) = nvidia_aftermath::initialize_d3d11_device(&new_d3d11_device) {
                warn!(
                    "[AngleInterop] Failed to initialize Aftermath for recovered D3D11 device: {e}"
                );
            }

            let egl_choose_config: EglChooseConfig = mem::transmute(get_proc("eglChooseConfig"));

            let config_attribs = [
                EGL_RED_SIZE,
                8,
                EGL_GREEN_SIZE,
                8,
                EGL_BLUE_SIZE,
                8,
                EGL_ALPHA_SIZE,
                8,
                EGL_DEPTH_SIZE,
                8,
                EGL_STENCIL_SIZE,
                8,
                EGL_SURFACE_TYPE,
                EGL_PBUFFER_BIT,
                EGL_RENDERABLE_TYPE,
                EGL_OPENGL_ES2_BIT,
                EGL_NONE,
            ];
            let mut new_config: *mut c_void = ptr::null_mut();
            let mut num_config = 0;

            if !egl_choose_config(
                new_display,
                config_attribs.as_ptr(),
                &mut new_config,
                1,
                &mut num_config,
            ) || num_config == 0
            {
                return Err("eglChooseConfig failed during recovery.".to_string());
            }

            self.display = new_display;
            self.config = new_config;
            self.angle_d3d11_device = new_d3d11_device;
            self.egl_get_error = egl_get_error;
            self.device_lost = false;

            info!("[AngleInterop] Full reinitialization successful! New D3D11 device created.");
            Ok(())
        }
    }

    ///
    /// Destroys the current EGL pbuffer surface and detaches the EGL context from the current thread.
    /// This is typically called before recreating resources for a new size.
    ///
    pub fn cleanup_surface_resources(&mut self) {
        if self.pbuffer_surface != EGL_NO_SURFACE {
            self.old_pbuffer_surfaces.push(self.pbuffer_surface);
            self.pbuffer_surface = EGL_NO_SURFACE;
        }
    }

    ///
    /// Recreates the underlying shared D3D11 texture and the associated EGL pbuffer surface.
    /// This is necessary when the overlay is resized.
    ///
    /// # Arguments
    ///
    /// * `width`: The new width of the texture and surface.
    /// * `height`: The new height of the texture and surface.
    ///
    /// # Returns
    ///
    /// A `Result` containing a tuple of the new `ID3D11Texture2D` and its `HANDLE` for
    /// cross-device sharing, or an error string on failure.
    ///
    pub fn recreate_resources(
        &mut self,
        width: u32,
        height: u32,
    ) -> Result<(ID3D11Texture2D, HANDLE), String> {
        self.cleanup_surface_resources();
        let angle_device = self.get_d3d_device()?;

        let (d3d_texture, handle) =
            create_shared_texture_and_get_handle(&angle_device, width, height)
                .map_err(|e| e.to_string())?;

        unsafe {
            let pbuffer_attribs = [EGL_WIDTH, width as i32, EGL_HEIGHT, height as i32, EGL_NONE];

            let d3d_texture_ptr = d3d_texture.as_raw();
            self.pbuffer_surface = (self.egl_create_pbuffer_from_client_buffer)(
                self.display,
                EGL_D3D_TEXTURE_ANGLE as u32,
                d3d_texture_ptr,
                self.config,
                pbuffer_attribs.as_ptr(),
            );

            if self.pbuffer_surface == EGL_NO_SURFACE {
                log_egl_error(
                    "eglCreatePbufferFromClientBuffer",
                    line!(),
                    self.egl_get_error,
                );
                return Err("Failed to create pbuffer surface.".to_string());
            }
        }
        Ok((d3d_texture, handle))
    }
}

///
/// Handles the complete teardown of the EGL context, display, and associated resources
/// when the `AngleInteropState` instance is dropped.
///
impl Drop for AngleInteropState {
    fn drop(&mut self) {
        unsafe {
            info!(
                "[AngleInterop] Dropping AngleInteropState on thread {:?}.",
                std::thread::current().id()
            );
            // Destroy any pending old surfaces
            for old_surface in self.old_pbuffer_surfaces.drain(..) {
                (self.egl_destroy_surface)(self.display, old_surface);
            }
            self.cleanup_surface_resources();
            (self.egl_make_current)(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
            if self.context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.context);
            }
            if self.resource_context != EGL_NO_CONTEXT {
                (self.egl_destroy_context)(self.display, self.resource_context);
            }
            if self.display != EGL_NO_DISPLAY {
                (self.egl_terminate)(self.display);
            }
        }
    }
}

///
/// A newtype wrapper around `Box<AngleInteropState>` to mark it as `Send` and `Sync`.
///
/// # Safety
///
/// This implementation is marked `unsafe` because the underlying EGL/OpenGL contexts
/// are not inherently thread-safe. The caller must guarantee that methods on `AngleInteropState`
/// are only called on the correct thread (e.g., the main render thread or resource loading thread
/// as established during context creation).
///
#[derive(Debug)]
pub struct SendableAngleState(pub Box<AngleInteropState>);
unsafe impl Send for SendableAngleState {}
unsafe impl Sync for SendableAngleState {}

///
/// FFI callback for the Flutter engine to make the main EGL rendering context current.
///
/// This function is called by Flutter on its rendering thread. It also handles the
/// lazy initialization of the main EGL context on the first call.
///
/// # Arguments
///
/// * `user_data`: A raw pointer to the `FlutterOverlay` instance associated with this engine.
///
extern "C" fn make_current_callback(user_data: *mut c_void) -> bool {
    unsafe {
        let overlay = &mut *(user_data as *mut FlutterOverlay);

        if let Some(angle_state) = &mut overlay.angle_state
            && let Some((w, h)) = angle_state.0.pending_resize.take() {
                match angle_state.0.recreate_resources(w, h) {
                    Ok((new_angle_texture, new_shared_handle)) => {
                        overlay.angle_keyed_mutex = new_angle_texture.cast().ok();

                        overlay.gl_internal_linear_texture = Some(new_angle_texture);
                        overlay.d3d11_shared_handle = Some(SendableHandle(new_shared_handle));

                        // Stash old game-side shared texture so the NVIDIA driver's
                        // internal threads don't access freed memory. It will be dropped
                        // when reopen_shared_texture_if_needed() replaces it.
                        overlay.angle_shared_texture_back = overlay.angle_shared_texture.take();
                        overlay.game_keyed_mutex = None;
                        let counter = overlay
                            .angle_frame_presented
                            .load(std::sync::atomic::Ordering::Relaxed);
                        overlay
                            .angle_frame_copied
                            .store(counter, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => {
                        error!("[AngleInterop] Deferred resize failed: {e}");
                    }
                }
            }

        // Acquire the shared texture so ANGLE can write to it.
        // Key 0 = "ANGLE can write". Blocks until game side calls ReleaseSync(0).
        if let Some(mutex) = &overlay.angle_keyed_mutex {
            let _ = mutex.AcquireSync(0, u32::MAX);
        }

        if let Some(angle_state) = &mut overlay.angle_state {
            let state = &mut angle_state.0;

            if state.device_lost {
                return false;
            }

            if state.context == EGL_NO_CONTEXT {
                info!(
                    "[AngleInterop] First call on main render thread {:?}. Initializing main EGL context.",
                    std::thread::current().id()
                );
                let context_attribs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
                state.context = (state.egl_create_context)(
                    state.display,
                    state.config,
                    state.resource_context,
                    context_attribs.as_ptr(),
                );
                if state.context == EGL_NO_CONTEXT {
                    let error_code = (state.egl_get_error)();
                    let reason_hr = match state.angle_d3d11_device.GetDeviceRemovedReason() {
                        Ok(()) => 0i32,
                        Err(e) => e.code().0,
                    };
                    error!(
                        "[AngleInterop] Failed to create main context. EGL error: {} ({:#X}). GetDeviceRemovedReason: {:#X} ({}). Marking device as lost.",
                        egl_error_to_string(error_code),
                        error_code,
                        reason_hr,
                        device_removed_reason_to_string(reason_hr)
                    );
                    state.device_lost = true;
                    return false;
                }
                state.main_thread_id = Some(std::thread::current().id());
            }

            if state.main_thread_id != Some(current().id()) {
                error!("FATAL: make_current_callback on wrong thread!");
                return false;
            }

            // Deferred cleanup: destroy the old surfaces from the render thread
            // where they were current. This is safe because eglMakeCurrent below
            // will detach them before we destroy.
            if !state.old_pbuffer_surfaces.is_empty() {
                (state.egl_make_current)(
                    state.display,
                    EGL_NO_SURFACE,
                    EGL_NO_SURFACE,
                    EGL_NO_CONTEXT,
                );
                for old_surface in state.old_pbuffer_surfaces.drain(..) {
                    (state.egl_destroy_surface)(state.display, old_surface);
                }
            }

            let result: EGLBoolean = (state.egl_make_current)(
                state.display,
                state.pbuffer_surface,
                state.pbuffer_surface,
                state.context,
            );

            if result != EGL_TRUE {
                let error_code = (state.egl_get_error)();
                if error_code == EGL_CONTEXT_LOST {
                    let reason_hr = match state.angle_d3d11_device.GetDeviceRemovedReason() {
                        Ok(()) => 0i32,
                        Err(e) => e.code().0,
                    };
                    error!(
                        "[AngleInterop] EGL_CONTEXT_LOST detected - D3D11 device was removed. GetDeviceRemovedReason: {:#X} ({}). Marking device as lost.",
                        reason_hr,
                        device_removed_reason_to_string(reason_hr)
                    );
                    state.device_lost = true;
                } else {
                    error!(
                        "[ANGLE DEBUG] EGL Error in make_current_callback:{} -> {} ({:#X})",
                        line!(),
                        egl_error_to_string(error_code),
                        error_code
                    );
                }
                return false;
            }

            // For the implicit view under the compositor path, the backing store
            // is simply the pbuffer surface's DEFAULT framebuffer (FBO 0): the
            // pbuffer is already backed by our shared D3D11 texture, and it is
            // current on this thread. No separate FBO / eglBindTexImage is needed
            // (and would be invalid for a D3D-client-buffer pbuffer). We resolve
            // the GL proc table once for the flush in present.
            if overlay.compositor_active && overlay.view0_gl.is_none() {
                match ViewGlProcs::resolve() {
                    Ok(procs) => {
                        overlay.view0_gl = Some(
                            ViewGlResources {
                                procs,
                                // Default framebuffer of the current pbuffer surface.
                                pbuffer_surface: state.pbuffer_surface,
                                color_texture: 0,
                                fbo: 0,
                            },
                        );
                        info!(
                            "[AngleInterop] Implicit view uses default framebuffer (FBO 0) for compositor path."
                        );
                    }
                    Err(e) => {
                        error!("[AngleInterop] Failed to resolve GL procs for view 0: {e}");
                    }
                }
            }
            return true;
        }
        false
    }
}

fn device_removed_reason_to_string(hr: i32) -> &'static str {
    match hr as u32 {
        0x00000000 => "S_OK (no error)",
        0x887A0001 => "DXGI_ERROR_INVALID_CALL",
        0x887A0002 => "DXGI_ERROR_NOT_FOUND",
        0x887A0005 => "DXGI_ERROR_UNSUPPORTED",
        0x887A0006 => "DXGI_ERROR_DEVICE_REMOVED",
        0x887A0007 => "DXGI_ERROR_DEVICE_HUNG (GPU timeout/TDR)",
        0x887A0008 => "DXGI_ERROR_DEVICE_RESET",
        0x887A0020 => "DXGI_ERROR_DRIVER_INTERNAL_ERROR",
        _ => "Unknown error",
    }
}

///
/// FFI callback for the Flutter engine to make the resource-loading EGL context current.
///
/// This function is called by Flutter on its resource loading thread. It handles the
/// lazy initialization of the shared resource EGL context on the first call.
///
/// # Arguments
///
/// * `user_data`: A raw pointer to the `FlutterOverlay` instance associated with this engine.
///
extern "C" fn make_resource_current_callback(user_data: *mut c_void) -> bool {
    unsafe {
        let overlay = &mut *(user_data as *mut FlutterOverlay);
        if let Some(angle_state) = &mut overlay.angle_state {
            let state = &mut angle_state.0;

            if state.device_lost {
                return false;
            }

            if state.resource_context == EGL_NO_CONTEXT {
                info!(
                    "[AngleInterop] First call on resource thread {:?}. Initializing resource EGL context.",
                    std::thread::current().id()
                );
                let context_attribs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];

                state.resource_context = (state.egl_create_context)(
                    state.display,
                    state.config,
                    EGL_NO_CONTEXT,
                    context_attribs.as_ptr(),
                );

                if state.resource_context == EGL_NO_CONTEXT {
                    let error_code = (state.egl_get_error)();
                    let reason_hr = match state.angle_d3d11_device.GetDeviceRemovedReason() {
                        Ok(()) => 0i32,
                        Err(e) => e.code().0,
                    };
                    error!(
                        "[AngleInterop] Failed to create resource context. EGL error: {} ({:#X}). GetDeviceRemovedReason: {:#X} ({}). Marking device as lost.",
                        egl_error_to_string(error_code),
                        error_code,
                        reason_hr,
                        device_removed_reason_to_string(reason_hr)
                    );
                    state.device_lost = true;
                    return false;
                }
                state.resource_thread_id = Some(std::thread::current().id());
            }

            if state.resource_thread_id != Some(current().id()) {
                error!("FATAL: make_resource_current_callback on wrong thread!");
                return false;
            }

            let result: EGLBoolean = (state.egl_make_current)(
                state.display,
                EGL_NO_SURFACE,
                EGL_NO_SURFACE,
                state.resource_context,
            );
            if result != EGL_TRUE {
                let error_code = (state.egl_get_error)();
                if error_code == EGL_CONTEXT_LOST {
                    let reason_hr = match state.angle_d3d11_device.GetDeviceRemovedReason() {
                        Ok(()) => 0i32,
                        Err(e) => e.code().0,
                    };
                    error!(
                        "[AngleInterop] EGL_CONTEXT_LOST detected in resource context - D3D11 device was removed. GetDeviceRemovedReason: {:#X} ({}). Marking device as lost.",
                        reason_hr,
                        device_removed_reason_to_string(reason_hr)
                    );
                    state.device_lost = true;
                } else {
                    error!(
                        "[ANGLE DEBUG] EGL Error in make_resource_current_callback:{} -> {} ({:#X})",
                        line!(),
                        egl_error_to_string(error_code),
                        error_code
                    );
                }
            }
            return result == EGL_TRUE;
        }
        false
    }
}

///
/// FFI callback for the Flutter engine to clear the current EGL context.
///
/// # Arguments
///
/// * `user_data`: A raw pointer to the `FlutterOverlay` instance associated with this engine.
///
extern "C" fn clear_current_callback(user_data: *mut c_void) -> bool {
    unsafe {
        let overlay = &mut *(user_data as *mut FlutterOverlay);
        if let Some(angle_state) = &mut overlay.angle_state {
            let state = &mut angle_state.0;
            (state.egl_make_current)(
                state.display,
                EGL_NO_SURFACE,
                EGL_NO_SURFACE,
                EGL_NO_CONTEXT,
            ) == EGL_TRUE
        } else {
            false
        }
    }
}

///
/// FFI callback for the Flutter engine to signal that a frame should be presented,
/// with damage information for dirty region management.
///
/// Replaces the simpler `present` callback. Flutter passes `FlutterPresentInfo` containing
/// `buffer_damage` — the regions that were modified in this frame. We store these rects
/// so `populate_existing_damage_callback` can feed them back next frame, telling Flutter
/// which parts of the FBO are already dirty.
///
extern "C" fn present_with_info_callback(
    user_data: *mut c_void,
    present_info: *const embedder::FlutterPresentInfo,
) -> bool {
    unsafe {
        let overlay = &*(user_data as *mut FlutterOverlay);

        if let Some(angle_state) = &overlay.angle_state {
            let state = &angle_state.0;

            // Non-blocking flush. GPU sync is handled by the keyed mutex.
            (state.gl_flush)();

            if !present_info.is_null() {
                let info = &*present_info;

                // Buffer damage → fed back via populate_existing_damage next frame.
                if let Ok(mut rects) = overlay.damage_rects.lock() {
                    rects.clear();
                    let num = info.buffer_damage.num_rects;
                    if num > 0 && !info.buffer_damage.damage.is_null() {
                        let slice = std::slice::from_raw_parts(info.buffer_damage.damage, num);
                        rects.extend_from_slice(slice);
                    }
                }

                // Frame damage → accumulated until tick() drains it for partial copy.
                // Multiple presents can fire between tick() calls during fast interaction.
                if let Ok(mut rects) = overlay.frame_damage_rects.lock() {
                    let num = info.frame_damage.num_rects;
                    if num > 0 && !info.frame_damage.damage.is_null() {
                        let slice = std::slice::from_raw_parts(info.frame_damage.damage, num);
                        rects.extend_from_slice(slice);
                    }
                }
            }

            // Release the shared texture so the game device can read it.
            if let Some(mutex) = &overlay.angle_keyed_mutex {
                let _ = mutex.ReleaseSync(1);
            }

            overlay
                .angle_frame_presented
                .fetch_add(1, std::sync::atomic::Ordering::Release);

            return true;
        }
        false
    }
}

///
/// FFI callback that tells Flutter which regions of the FBO are already dirty
/// (i.e. were modified since the FBO was last used). Flutter uses this to avoid
/// re-rasterizing unchanged areas.
///
/// With a single FBO (always ID 0), we feed back the buffer damage rects that
/// Flutter gave us in the previous `present_with_info` call.
///
extern "C" fn populate_existing_damage_callback(
    user_data: *mut c_void,
    _fbo_id: isize,
    existing_damage: *mut embedder::FlutterDamage,
) {
    unsafe {
        let overlay = &*(user_data as *mut FlutterOverlay);

        if existing_damage.is_null() {
            return;
        }

        // If a full repaint is needed (resize, device recovery), report zero existing
        // damage — Flutter interprets this as "the entire FBO is clean, repaint everything".
        if overlay
            .full_repaint_needed
            .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            (*existing_damage).num_rects = 0;
            (*existing_damage).damage = ptr::null_mut();
            return;
        }

        if let Ok(rects) = overlay.damage_rects.lock() {
            if rects.is_empty() {
                (*existing_damage).num_rects = 0;
                (*existing_damage).damage = ptr::null_mut();
            } else {
                (*existing_damage).num_rects = rects.len();
                (*existing_damage).damage = rects.as_ptr() as *mut embedder::FlutterRect;
            }
        } else {
            // Mutex poisoned — force full repaint.
            (*existing_damage).num_rects = 0;
            (*existing_damage).damage = ptr::null_mut();
        }
    }
}

///
/// FFI callback for the Flutter engine to get the framebuffer object ID.
/// Returns 0 to indicate that Flutter should render to the default framebuffer of the current surface.
///
extern "C" fn fbo_callback(_user_data: *mut c_void) -> u32 {
    0
}

///
/// FFI callback for the Flutter engine to resolve GL/EGL function pointers.
///
/// This function is the central resolver for the engine. It queries the globally shared
/// `eglGetProcAddress` function, which was loaded when the first `AngleInteropState`
/// was initialized. The `user_data` parameter is not used by the Flutter engine for this callback.
///
extern "C" fn gl_proc_resolver_callback(_user_data: *mut c_void, proc: *const i8) -> *mut c_void {
    if let Some(shared_egl) = SHARED_EGL.get() {
        unsafe { (shared_egl.egl_get_proc_address)(proc) }
    } else {
        error!("[gl_proc_resolver] SHARED_EGL was not initialized before use!");
        ptr::null_mut()
    }
}

///
/// Retrieves the singleton `SharedEglState`, initializing it if necessary.
///
/// On the first call within the process, this function uses the provided `engine_dir`
/// to load `libEGL.dll` and `libGLESv2.dll` and caches the library handles and the
/// `eglGetProcAddress` function pointer. On all subsequent calls, it returns the
/// already-initialized state and ignores the `engine_dir` parameter.
///
/// # Arguments
///
/// * `engine_dir`: An optional path to the directory containing ANGLE libraries. Only used on the first call.
///
/// # Returns
///
/// A `Result` containing a static reference to the `SharedEglState` on success,
/// or an error string on failure.
///
/// Load the ANGLE DLLs (`libEGL.dll`, `libGLESv2.dll`) ahead of time.
pub fn preload_angle_dlls(engine_dir: Option<&Path>) -> Result<(), String> {
    get_or_init_shared_egl(engine_dir).map(|_| ())
}

fn get_or_init_shared_egl(engine_dir: Option<&Path>) -> Result<&'static SharedEglState, String> {
    SHARED_EGL.get_or_try_init(|| {
        let libegl_path = engine_dir
            .map(|d| d.join("libEGL.dll"))
            .unwrap_or_else(|| PathBuf::from("libEGL.dll"));
        let libgles_path = engine_dir
            .map(|d| d.join("libGLESv2.dll"))
            .unwrap_or_else(|| PathBuf::from("libGLESv2.dll"));

        info!(
            "[SharedEGL] Initializing for the first time with paths: {libegl_path:?}, {libgles_path:?}"
        );

        let libegl = unsafe {
            Library::new(&libegl_path)
                .map_err(|e| format!("Failed to load libEGL.dll from {libegl_path:?}: {e}"))
        }?;
        let libgles = unsafe {
            Library::new(&libgles_path).map_err(|e| {
                format!(
                    "Failed to load libGLESv2.dll from {libgles_path:?}: {e}"
                )
            })
        }?;

        let egl_get_proc_address_symbol: Symbol<EglGetProcAddress> =
            unsafe { libegl.get(b"eglGetProcAddress") }.map_err(|e| e.to_string())?;

        let egl_get_proc_address = *egl_get_proc_address_symbol;

        Ok(SharedEglState {
            libegl,
            _libgles: libgles,
            egl_get_proc_address,
        })
    })
}

///
/// Constructs the `FlutterRendererConfig` struct required by the Flutter Engine
/// for an OpenGL-based renderer.
///
/// This function populates the configuration struct with the necessary C-ABI compatible
/// callback functions that bridge the engine's rendering lifecycle events with the
/// custom ANGLE implementation.
///
pub fn build_opengl_renderer_config() -> embedder::FlutterRendererConfig {
    embedder::FlutterRendererConfig {
        type_: embedder::FlutterRendererType_kOpenGL,
        __bindgen_anon_1: embedder::FlutterRendererConfig__bindgen_ty_1 {
            open_gl: embedder::FlutterOpenGLRendererConfig {
                struct_size: std::mem::size_of::<embedder::FlutterOpenGLRendererConfig>(),
                make_current: Some(make_current_callback),
                clear_current: Some(clear_current_callback),
                present: None,
                fbo_callback: Some(fbo_callback),
                make_resource_current: Some(make_resource_current_callback),
                fbo_reset_after_present: false,
                gl_proc_resolver: Some(gl_proc_resolver_callback),
                surface_transformation: None,
                gl_external_texture_frame_callback: None,
                fbo_with_frame_info_callback: None,
                present_with_info: Some(present_with_info_callback),
                populate_existing_damage: Some(populate_existing_damage_callback),
            },
        },
    }
}
