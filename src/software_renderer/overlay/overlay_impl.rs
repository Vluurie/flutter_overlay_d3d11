use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU64},
    },
    thread,
};

use windows::Win32::{
    Foundation::{HANDLE, HWND},
    Graphics::Direct3D11::{
        ID3D11Device, ID3D11Query, ID3D11ShaderResourceView, ID3D11Texture2D,
    },
    Graphics::Dxgi::IDXGIKeyedMutex,
};

use crate::{
    bindings::embedder::{
        self, FlutterCompositor, FlutterEngine, FlutterKeyEventType, FlutterRect, FlutterViewId,
    },
    software_renderer::{
        api::RendererType,
        d3d11_compositor::{
            effects::EffectConfig, post_processing_renderer::PostProcessRenderer,
            primitive_3d_renderer::Primitive3DRenderer,
            text_3d_renderer::Text3DRenderer,
        },
        dynamic_flutter_engine_dll_loader::FlutterEngineDll,
        gl_renderer::angle_interop::SendableAngleState,
        multiview::{ViewRegistry, view_surface::ViewGlResources},
        overlay::{
            semantics_handler::ProcessedSemanticsNode,
            textinput::{ActiveTextInputState, SharedViewKeyboardState},
        },
        ticker::{
            on_present as ticker_on_present,
            task_scheduler::{
                SendableFlutterCustomTaskRunners, SendableFlutterTaskRunnerDescription,
                TaskQueueState, TaskRunnerContext,
            },
        },
    },
};

pub static FLUTTER_LOG_TAG: &CStr = c"rust_embedder";

/// Represents a platform message that needs to be sent on the platform thread
#[derive(Debug, Clone)]
pub struct PendingPlatformMessage {
    pub channel: String,
    pub payload_bytes: Vec<u8>,
}

/// Represents a Key-Event that is waiting to be sent
/// to the engine from the platform thread.
#[derive(Debug, Clone)]
pub struct PendingKeyEvent {
    pub event_type: FlutterKeyEventType,
    pub physical: u64,
    pub logical: u64,
    pub characters: String,
    pub synthesized: bool,
}

/// Queue for pending key events (new API)
pub type PendingKeyEventQueue = Arc<Mutex<VecDeque<PendingKeyEvent>>>;

/// Queue for pending platform messages that need to be sent from the platform thread
pub type PendingPlatformMessageQueue = Arc<Mutex<VecDeque<PendingPlatformMessage>>>;

/// A platform-channel message handler: takes the request bytes and returns a response.
pub type ChannelHandler = Box<dyn Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static>;
/// Map of channel name → handler, shared across threads.
pub type ChannelHandlers = Arc<Mutex<HashMap<String, ChannelHandler>>>;

// A wrapper around the raw FlutterEngine pointer to make it Send + Sync.
// WARNING: This is only safe because we PROMISE to only use the pointer
// on the thread that the Flutter Engine is designed to run on.
#[derive(Debug, Copy, Clone)]
pub struct SendableFlutterEngine(pub FlutterEngine);

// By implementing these traits, we are making a manual guarantee to the compiler.
unsafe impl Send for SendableFlutterEngine {}
unsafe impl Sync for SendableFlutterEngine {}

impl PartialEq for SendableFlutterEngine {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl PartialEq<FlutterEngine> for SendableFlutterEngine {
    fn eq(&self, other: &FlutterEngine) -> bool {
        self.0 == *other
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SendHwnd(pub HWND);

unsafe impl Send for SendHwnd {}
unsafe impl Sync for SendHwnd {}

#[derive(Clone, Copy, Debug)]
pub struct SendableHandle(pub HANDLE);
unsafe impl Send for SendableHandle {}
unsafe impl Sync for SendableHandle {}

/// Represents a single Flutter overlay instance, managing its engine, rendering,
/// and various UI-related states.
/// Initialization is handled by `init_overlay`, which is responsible for correctly
/// populating all necessary fields.
#[repr(C)]
pub struct FlutterOverlay {
    /// The DirectX 11 texture resource on the GPU where the Flutter content is rendered.
    /// Other parts of the crate (e.g., the main game renderer) will use this to draw the overlay.
    /// **IMPORTANT: Must be a valid texture resource, initialized by `init_overlay`.**
    pub texture: ID3D11Texture2D,

    /// The DirectX 11 shader resource view for the `texture`.
    /// Used by shaders to sample from the overlay texture during rendering.
    /// **IMPORTANT: Must be a valid SRV, initialized by `init_overlay`.**
    pub srv: ID3D11ShaderResourceView,

    /// Current width of the overlay in pixels.
    /// Can be read by other parts of the crate for layout purposes.
    /// Updated by `handle_window_resize`. Must be > 0 for rendering.
    pub width: u32,

    /// Current height of the overlay in pixels.
    /// Can be read by other parts of the crate for layout purposes.
    /// Updated by `handle_window_resize`. Must be > 0 for rendering.
    pub height: u32,

    /// Overlay position X
    pub x: i32,
    /// Overlay position X
    pub y: i32,

    /// By default true and does nothing directly even when set to false
    /// Caller requires to call it when appropiated. Example like this:
    /// ```rust
    /// if !overlay_instance.is_visible() {
    ///     continue; // <-- ...we skip EVERYTHING below for this overlay.
    /// }
    ///
    /// overlay_instance.request_frame();
    /// update_interactive_widget_hover_state(overlay_instance);
    /// Self::paint_flutter_overlay(overlay_instance, painter, id);
    /// ```
    pub visible: bool,
    pub keep_alive: bool,
    pub ui_hidden: bool,

    pub effect_config: EffectConfig,
    pub effect_frames_remaining: u32,
    pub effect_total_frames: u32,

    /// A user-defined name for this overlay instance. Useful for identification,
    /// logging, or debugging purposes by any part of the crate.
    pub name: String,
    pub renderer_type: RendererType,

    /// Atomic boolean indicating if the mouse cursor is currently hovering over an
    /// interactive widget (e.g., button, text field) within this overlay's semantics tree.
    /// Can be read by other parts of the crate (e.g., game input logic) to alter behavior.
    pub is_interactive_widget_hovered: AtomicBool,

    /// A boolean flag indicating if this specific overlay instance is running with
    /// debug assets (e.g., in JIT mode due to the absence of an AOT snapshot).
    /// Determined during `init_overlay` and can be read for diagnostic or conditional logic.
    pub is_debug_build: bool,

    /// Pointer to the native Flutter engine instance. Managed internally.
    /// Can be used to check for operations if the engine !is_null()
    /// **CRITICAL: Must be valid post-`init_overlay` for all operations.**
    pub engine: SendableFlutterEngine,

    /// Device `texture`/`srv` were created on. Reused for every later resource so resizes and
    /// device recovery cannot land on a different device than creation did.
    pub(crate) d3d11_device: ID3D11Device,

    /// The Direct3D 11 compositor responsible for rendering Flutter content to the texture.
    pub post_processor: PostProcessRenderer,
    pub primitive_renderer: Primitive3DRenderer,
    /// The 3D text renderer for rendering text as textured quads in 3D space.
    pub text_renderer: Text3DRenderer,

    // Crate-Internal API - Fields used within the embedder logic.
    // Not intended for modification by the end-user.
    //
    /// A map of channel names to callback functions for handling incoming platform messages from Dart.
    /// This is populated via the public `register_channel_handler` method.
    pub(crate) message_handlers: ChannelHandlers,

    /// A reusable buffer for crafting platform channel responses, avoiding allocations on the hot path.
    pub(crate) response_buffer: Arc<Mutex<Vec<u8>>>,
    /// An atomically accessible pointer to the Flutter Engine. Used for safe access from multiple threads.
    pub(crate) engine_atomic_ptr: Arc<AtomicPtr<embedder::_FlutterEngine>>,

    /// CPU-side buffer storing RGBA pixel data. Managed by Flutter rendering callbacks and `tick` method.
    pub(crate) pixel_buffer: Option<Vec<u8>>,
    /// Set by `on_present` when new pixel data is available, cleared by `tick` after uploading.
    pub(crate) software_frame_dirty: AtomicBool,
    /// Set once by `on_present` after the first frame is rendered. Never cleared.
    pub(crate) software_first_frame_rendered: AtomicBool,

    /// The current cursor style requested by Flutter. Managed internally by `handle_set_cursor`
    /// and platform message callbacks.
    pub(crate) desired_cursor: Arc<Mutex<Option<String>>>,

    /// The Windows HWND this overlay is associated with. Set by `init_overlay`, used internally.
    pub(crate) windows_handler: SendHwnd,

    /// Shared reference to the loaded Flutter engine DLL. Managed internally.
    /// **CRITICAL INTERNAL: Must be valid post-`init_overlay`.**
    pub(crate) engine_dll: Arc<FlutterEngineDll>,

    /// Keyboard and text-input state for the implicit view (view 0).
    pub(crate) view0_keyboard: SharedViewKeyboardState,

    pub(crate) active_text_input: Arc<Mutex<Option<ActiveTextInputState>>>,

    /// Queue for pending platform messages that need to be sent from the platform thread.
    /// Messages sent from non-platform threads (like Windows UI thread) are queued here
    /// and processed by the platform task runner thread.
    pub(crate) pending_platform_messages: PendingPlatformMessageQueue,

    /// Used for pending keys for the new key event api
    pub(crate) pending_key_events: PendingKeyEventQueue,

    /// Pending `(view_id, focused)` focus events. `FlutterEngineSendViewFocusEvent`
    /// asserts it runs on the platform task-runner thread, so callers on the
    /// window thread enqueue here and the task runner drains it.
    pub(crate) pending_view_focus: Arc<Mutex<VecDeque<(i64, bool)>>>,

    /// Instance-specific task queue. Managed by task runner and `post_task_callback`.
    pub(crate) task_queue_state: Arc<TaskQueueState>,

    /// Join handle for the task runner thread. Managed by `start_task_runner` and potentially `drop`.
    pub(crate) task_runner_thread: Option<Arc<thread::JoinHandle<()>>>,

    /// Tracks pressed mouse buttons for this overlay. Managed by `handle_pointer_event`.
    pub(crate) mouse_buttons_state: AtomicI32,

    /// Tracks if the `kAdd` pointer event was sent. Managed by `handle_pointer_event`.
    pub(crate) is_mouse_added: AtomicBool,

    /// Semantics tree data for this overlay. Managed by semantics callbacks and hover state updates.
    pub(crate) semantics_tree_data: Arc<Mutex<HashMap<i32, ProcessedSemanticsNode>>>,

    /// The Dart port used for sending messages directly to the Dart isolate.
    pub(crate) dart_send_port: Arc<AtomicI64>,

    // --- ANGLE (OpenGL) specific fields ---
    /// Manages the state for ANGLE's EGL context and surfaces for OpenGL rendering.
    pub(crate) angle_state: Option<SendableAngleState>,
    /// The shared handle for the D3D11 texture created by ANGLE.
    pub(crate) d3d11_shared_handle: Option<SendableHandle>,
    /// The internal texture that Flutter (via ANGLE) renders into.
    pub(crate) gl_internal_linear_texture: Option<ID3D11Texture2D>,
    /// A D3D11 texture on the host device that shares the resource created by ANGLE.
    /// With double buffering, this is the front buffer (index 0).
    pub(crate) angle_shared_texture: Option<ID3D11Texture2D>,
    /// Second shared texture for double buffering (back buffer, index 1).
    pub(crate) angle_shared_texture_back: Option<ID3D11Texture2D>,
    /// Keyed mutex on the ANGLE-side shared texture for cross-device GPU sync.
    /// Key 0 = ANGLE owns (can write), Key 1 = game owns (can read).
    pub(crate) angle_keyed_mutex: Option<IDXGIKeyedMutex>,
    /// Keyed mutex on the game-side view of the shared texture.
    pub(crate) game_keyed_mutex: Option<IDXGIKeyedMutex>,
    /// D3D11 event query used for GPU synchronization between ANGLE and host device.
    /// This query is signaled after ANGLE finishes rendering to ensure the host
    /// doesn't copy from the shared texture while it's still being written to.
    pub(crate) angle_frame_complete_query: Option<ID3D11Query>,
    /// Frame counter incremented by Flutter's render thread after each present.
    /// The host checks this to know when a new frame is available and GPU-ready.
    pub(crate) angle_frame_presented: AtomicU64,
    /// Frame counter for the last frame copied by the host. Used with angle_frame_presented
    /// to detect when a new frame is available.
    pub(crate) angle_frame_copied: AtomicU64,

    /// Buffer damage rects from `present_with_info`. Fed back to Flutter via
    /// `populate_existing_damage` so it can skip re-rasterizing unchanged areas.
    pub(crate) damage_rects: Mutex<Vec<FlutterRect>>,
    /// Frame damage rects from `present_with_info`. Consumed by `tick()` to
    /// copy only the changed regions from the shared texture.
    pub(crate) frame_damage_rects: Mutex<Vec<FlutterRect>>,
    /// When true, forces a full repaint on the next frame. Set on resize and device recovery.
    pub(crate) full_repaint_needed: AtomicBool,

    /// Registry of secondary (satellite) views driven by this engine. The
    /// implicit view (`view_id == 0`) is this overlay itself and is NOT stored
    /// here. Populated by `add_window_view`. Shared via `Arc` because the
    /// compositor callbacks reach it through the engine `user_data` pointer.
    pub(crate) view_registry: Arc<ViewRegistry>,

    /// GL framebuffer + color texture for the implicit view (view 0) when the
    /// compositor present path is active. Built lazily on the render thread from
    /// the overlay's existing pbuffer surface. `None` for the software renderer
    /// or before the first compositor backing-store request.
    pub(crate) view0_gl: Option<ViewGlResources>,

    /// True when a `FlutterCompositor` is installed and therefore the multi-view
    /// present path is active (OpenGL renderer only). When true, the implicit
    /// view renders into `view0_gl` and presents via `present_view_callback`
    /// instead of the legacy `present_with_info` path.
    pub(crate) compositor_active: bool,

    // --- FFI argument storage ---
    // These fields hold C-compatible strings and argument structures for the lifetime
    // of the engine, as the engine may read from this memory at any time.
    pub(crate) _assets_c: CString,
    pub(crate) _icu_c: CString,
    pub(crate) _engine_argv_cs: Vec<CString>,
    pub(crate) _dart_argv_cs: Vec<CString>,
    pub(crate) _aot_c: Option<CString>,
    pub(crate) _platform_runner_context: Option<Box<TaskRunnerContext>>,
    pub(crate) _platform_runner_description: Option<Box<SendableFlutterTaskRunnerDescription>>,
    pub(crate) _custom_task_runners_struct: Option<Box<SendableFlutterCustomTaskRunners>>,
    /// The compositor descriptor passed to the engine for multi-view rendering.
    /// Held for the engine's lifetime because `FlutterProjectArgs.compositor`
    /// stores a borrowed pointer to it. Only set when the OpenGL renderer is
    /// active (the software path keeps the legacy present callback).
    pub(crate) _compositor: Option<Box<SendableFlutterCompositor>>,
}

/// `Send`/`Sync` wrapper for the engine compositor descriptor. Safe because the
/// embedded raw `user_data` pointer is only dereferenced on engine threads, and
/// the struct itself is immutable after construction.
pub struct SendableFlutterCompositor(pub FlutterCompositor);
unsafe impl Send for SendableFlutterCompositor {}
unsafe impl Sync for SendableFlutterCompositor {}

impl FlutterOverlay {
    /// Queues a view focus change. The actual engine call is made on the platform
    /// task-runner thread (the engine asserts thread affinity), so this is safe to
    /// call from any thread (e.g. a satellite window's WndProc thread).
    pub(crate) fn send_view_focus(&self, view_id: FlutterViewId, focused: bool) {
        if let Ok(mut q) = self.pending_view_focus.lock() {
            q.push_back((view_id, focused));
        }
        self.task_queue_state.waker.wake_up();
    }

}

impl Clone for FlutterOverlay {
    /// Clones the `FlutterOverlay`.
    ///
    /// # Warning
    ///
    /// This is a **shallow clone**. It creates a new `FlutterOverlay` struct but shares
    /// ownership of the underlying engine, task queues, and other thread-safe resources (`Arc<T>`).
    /// It does **not** create a new, independent Flutter instance.
    ///
    /// This is primarily useful for passing overlay state information around without transferring
    /// ownership. Critical resources like the task runner thread handle are **not** cloned
    /// and are set to `None`.
    fn clone(&self) -> Self {
        Self {
            // --- Shallow copy of shared resources ---
            engine: self.engine,
            engine_atomic_ptr: self.engine_atomic_ptr.clone(),
            texture: self.texture.clone(),
            srv: self.srv.clone(),
            d3d11_device: self.d3d11_device.clone(),
            post_processor: self.post_processor.clone(),
            primitive_renderer: self.primitive_renderer.clone(),
            text_renderer: self.text_renderer.clone(),
            desired_cursor: self.desired_cursor.clone(),
            name: self.name.clone(),
            dart_send_port: self.dart_send_port.clone(),
            engine_dll: self.engine_dll.clone(),
            task_queue_state: self.task_queue_state.clone(),
            view0_keyboard: self.view0_keyboard.clone(),
            active_text_input: self.active_text_input.clone(),
            pending_platform_messages: self.pending_platform_messages.clone(),
            pending_key_events: self.pending_key_events.clone(),
            pending_view_focus: self.pending_view_focus.clone(),
            semantics_tree_data: self.semantics_tree_data.clone(),
            message_handlers: self.message_handlers.clone(),
            response_buffer: self.response_buffer.clone(),

            width: self.width,
            height: self.height,
            renderer_type: self.renderer_type.clone(),
            visible: self.visible,
            keep_alive: self.keep_alive,
            ui_hidden: self.ui_hidden,
            effect_config: self.effect_config,
            effect_frames_remaining: self.effect_frames_remaining,
            effect_total_frames: self.effect_total_frames,
            x: self.x,
            y: self.y,
            windows_handler: self.windows_handler,
            is_debug_build: self.is_debug_build,
            pixel_buffer: self.pixel_buffer.clone(),
            software_frame_dirty: AtomicBool::new(false),
            software_first_frame_rendered: AtomicBool::new(false),

            mouse_buttons_state: AtomicI32::new(
                self.mouse_buttons_state
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            is_mouse_added: AtomicBool::new(
                self.is_mouse_added
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            is_interactive_widget_hovered: AtomicBool::new(
                self.is_interactive_widget_hovered
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),

            task_runner_thread: None,
            _platform_runner_context: None,
            _platform_runner_description: None,
            _custom_task_runners_struct: None,
            _compositor: None,
            angle_state: None,
            d3d11_shared_handle: None,
            angle_frame_complete_query: None,
            angle_frame_presented: AtomicU64::new(0),
            angle_frame_copied: AtomicU64::new(0),
            damage_rects: Mutex::new(Vec::new()),
            frame_damage_rects: Mutex::new(Vec::new()),
            full_repaint_needed: AtomicBool::new(true),
            angle_shared_texture_back: None,
            angle_keyed_mutex: None,
            game_keyed_mutex: None,

            _assets_c: self._assets_c.clone(),
            _icu_c: self._icu_c.clone(),
            _engine_argv_cs: self._engine_argv_cs.clone(),
            _dart_argv_cs: self._dart_argv_cs.clone(),
            _aot_c: self._aot_c.clone(),

            gl_internal_linear_texture: self.gl_internal_linear_texture.clone(),
            angle_shared_texture: self.angle_shared_texture.clone(),

            view_registry: self.view_registry.clone(),
            view0_gl: None,
            compositor_active: self.compositor_active,
        }
    }
}

/// A C-compatible callback function that the Flutter engine invokes when a new frame
/// is ready for presentation in **software rendering mode**.
pub(crate) extern "C" fn on_present(
    user_data: *mut std::ffi::c_void,
    allocation: *const std::ffi::c_void,
    row_bytes_flutter: usize,
    height_flutter: usize,
) -> bool {
    ticker_on_present(
        user_data,
        allocation,
        row_bytes_flutter,
        height_flutter,
    )
}
