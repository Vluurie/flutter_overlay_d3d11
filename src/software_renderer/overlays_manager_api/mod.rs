//! # Overlay manager API (getting started)
//!
//! The high-level entry point for embedding Flutter into a host-owned D3D11
//! application or game. [`FlutterOverlayManagerHandle`] is a small, copyable handle
//! to a global manager that owns one or more overlays (for example a main UI plus
//! one UI per plugin), routes input to the right one, and drives rendering each
//! frame.
//!
//! Get the handle with [`get_flutter_overlay_manager_handle`]. It is cheap to clone
//! and pass between threads.
//!
//! ## Path A: embed into an existing D3D11 app or game
//!
//! This is the realistic integration: you already have a host that owns a
//! `ID3D11Device`, an `ID3D11DeviceContext`, and an `IDXGISwapChain` (a game you
//! hook, or your own renderer). You pass those host objects straight in. The
//! embedder does not create a proxy or dummy device; it renders onto the host's
//! own device.
//!
//! Setup (once, early; before installing your present/render hook is a good time):
//!
//! ```no_run
//! use std::path::Path;
//! use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::{
//!     get_flutter_overlay_manager_handle, preload_flutter_runtime_dlls,
//! };
//! use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
//!
//! # fn demo(swap_chain: &IDXGISwapChain) {
//! // Load the Flutter engine + ANGLE DLLs from your release bundle.
//! let engine_dir = Path::new(r"C:\game\mods\flutter_main_overlay\Release");
//! preload_flutter_runtime_dlls(engine_dir);
//!
//! let manager = get_flutter_overlay_manager_handle().unwrap();
//!
//! // Create the main overlay on the host's own swapchain. `identifier` names this
//! // instance; `dart_args` / `engine_args` are optional Dart-VM and engine flags.
//! let ok = manager.init_instance(
//!     swap_chain,
//!     engine_dir,        // folder containing data/flutter_assets, icudtl.dat, app.so
//!     "main_ui",
//!     None,              // dart_entrypoint_args
//!     None,              // engine_args
//! );
//! assert!(ok);
//! # }
//! ```
//!
//! Per frame, from inside your present/render hook, in this order:
//!
//! ```no_run
//! # use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::FlutterOverlayManagerHandle;
//! # use directx_math::XMMatrix;
//! # use windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView;
//! # use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
//! # fn frame(
//! #     manager: FlutterOverlayManagerHandle,
//! #     swap_chain: &IDXGISwapChain,
//! #     view_proj: XMMatrix,
//! #     game_dsv: Option<ID3D11DepthStencilView>,
//! # ) {
//! // Recover first if the D3D device was lost (for example after a mode switch).
//! if manager.should_attempt_recovery() {
//!     manager.attempt_device_recovery(swap_chain);
//! }
//!
//! // Snapshot any 3D primitives/text submitted since last frame.
//! manager.latch_all_queued_primitives();
//! manager.latch_all_queued_text();
//!
//! // Draw world-space 3D primitives against the game's depth buffer. Do this
//! // BEFORE any post-processing that flattens or rescales the game's render target.
//! manager.render_primitives(&view_proj, &game_dsv);
//!
//! // ... game post-processing here, if any ...
//!
//! // Tick the engine(s) and composite the 2D Flutter UI on top.
//! manager.render_ui();
//! # }
//! ```
//!
//! Wire up the host's window messages and resize:
//!
//! ```no_run
//! # use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
//! # use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
//! # use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
//! // In your WndProc: forward input first; if Flutter consumed it, do not pass it
//! // on to the game/host.
//! # fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
//! if let Some(manager) = get_flutter_overlay_manager_handle() {
//!     if manager.forward_input_to_flutter(hwnd, msg, wparam, lparam) {
//!         return true; // consumed by Flutter
//!     }
//! }
//! false
//! # }
//!
//! // In your ResizeBuffers hook:
//! # fn on_resize(swap_chain: &IDXGISwapChain, x: i32, y: i32, w: u32, h: u32) {
//! if let Some(manager) = get_flutter_overlay_manager_handle() {
//!     manager.resize_flutter_overlays(swap_chain, x, y, w, h);
//! }
//! # }
//! ```
//!
//! ## Path B: standalone host window (no game swapchain)
//!
//! If you do not have a host swapchain to hook, create your own D3D11 device and a
//! swapchain for a window you own, then drive a single overlay directly through
//! [`crate::software_renderer::api::FlutterOverlay`]: build it with
//! [`FlutterOverlay::create`], call [`FlutterOverlay::tick`] each frame, and read
//! the result with [`FlutterOverlay::get_texture_srv`] to composite it yourself.
//!
//! For the very simplest case, where you just want a Flutter window and do not need
//! to composite it into your own scene, skip this module entirely and use the
//! crate-root [`crate::init_flutter_window_from_dir`].
//!
//! ## Beyond setup
//!
//! Once running, the same handle submits 3D geometry, text, and custom shaders, and
//! exchanges messages with Dart. See the methods on [`FlutterOverlayManagerHandle`]
//! and the renderers in [`crate::software_renderer::d3d11_compositor`]. Multiple
//! Flutter views in their own OS windows are available on the OpenGL path via
//! [`crate::software_renderer::multiview`].
//!
//! [`FlutterOverlay::create`]: crate::software_renderer::api::FlutterOverlay::create
//! [`FlutterOverlay::tick`]: crate::software_renderer::api::FlutterOverlay::tick
//! [`FlutterOverlay::get_texture_srv`]: crate::software_renderer::api::FlutterOverlay::get_texture_srv

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Instant;

/// Global flag indicating that the overlay system is fully initialized and ready.
static OVERLAY_SYSTEM_READY: AtomicBool = AtomicBool::new(false);
/// Backbuffer size as (w << 32 | h), updated at init/resize, readable lock-free.
static SCREEN_SIZE: AtomicU64 = AtomicU64::new(0);
/// Last pointer position in screen px as (x_bits << 32 | y_bits) (f32 bit patterns),
/// updated on every pointer sample, readable lock-free across DLL copies.
pub static POINTER_POS: AtomicU64 = AtomicU64::new(0);
/// Last pointer button bitmask (FlutterPointerMouseButtons; bit 0 = primary),
/// updated on every pointer sample, readable lock-free across DLL copies.
pub static POINTER_BUTTONS: AtomicI64 = AtomicI64::new(0);

use directx_math::{XMMatrix, XMMatrixIdentity};
use log::{error, info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext, ID3D11SamplerState,
    ID3D11ShaderResourceView,
};
use windows::Win32::Graphics::Dxgi::{DXGI_SWAP_CHAIN_DESC, IDXGISwapChain};
use windows::Win32::UI::WindowsAndMessaging::{
    WM_CHAR, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCMOUSELEAVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP,
};
use windows::core::Result as WindowsResult;

use crate::bindings::embedder::FlutterViewId;
use crate::init_logging;
use crate::software_renderer::api::{FlutterEmbedderError, OverlayCreateParams};
use crate::software_renderer::d3d11_compositor::effects::{
    EffectConfig, EffectParams, EffectTarget, HologramParams, PostEffect, WarpFieldParams,
};
use crate::software_renderer::overlay::overlay_impl::PendingPlatformMessage;

use crate::software_renderer::d3d11_compositor::primitive_3d_renderer::{
    BlendMode, PrimitiveOptions, PrimitiveType, Vertex3D,
};
use crate::software_renderer::d3d11_compositor::text_3d_renderer::{FontAtlas, TexturedVertex3D};
use crate::software_renderer::d3d11_compositor::traits::{FrameParams, Renderer};
use crate::software_renderer::dynamic_flutter_engine_dll_loader::FlutterEngineDll;
use crate::software_renderer::gl_renderer::angle_interop::preload_angle_dlls;
use crate::software_renderer::multiview::window::{SatelliteWindow, WindowSpec};
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::overlay::semantics_handler::update_interactive_widget_hover_state;

/// A thread-safe, clonable handle for interacting with the global OverlayManager.
#[derive(Clone, Copy)]
pub struct FlutterOverlayManagerHandle {
    pub manager: &'static Mutex<OverlayManager>,
    /// Pointer into this module's SCREEN_SIZE so reads work across DLL copies.
    pub screen: &'static AtomicU64,
    /// Pointer into this module's POINTER_POS so reads work across DLL copies.
    pub pointer: &'static AtomicU64,
    /// Pointer into this module's POINTER_BUTTONS so reads work across DLL copies.
    pub pointer_buttons: &'static AtomicI64,
}

/// Gets a thread-safe handle to the global OverlayManager.
///
/// This handle is lightweight and can be cloned and passed between threads.
pub fn get_flutter_overlay_manager_handle() -> Option<FlutterOverlayManagerHandle> {
    get_overlay_manager().map(|manager_mutex| FlutterOverlayManagerHandle {
        manager: manager_mutex,
        screen: &SCREEN_SIZE,
        pointer: &POINTER_POS,
        pointer_buttons: &POINTER_BUTTONS,
    })
}

/// Load the Flutter runtime DLLs from `engine_dir` ahead of overlay creation.
pub fn preload_flutter_runtime_dlls(engine_dir: &std::path::Path) {
    init_logging();

    match FlutterEngineDll::get_for(Some(engine_dir)) {
        Ok(_) => info!("[Preload] flutter_engine.dll loaded from {engine_dir:?}"),
        Err(e) => warn!("[Preload] failed to load flutter_engine.dll: {e}"),
    }

    match preload_angle_dlls(Some(engine_dir)) {
        Ok(()) => info!("[Preload] ANGLE DLLs loaded from {engine_dir:?}"),
        Err(e) => warn!("[Preload] failed to load ANGLE DLLs: {e}"),
    }
}

static OVERLAY_MANAGER: Once = Once::new();
static mut GLOBAL_OVERLAY_MANAGER: Option<Mutex<OverlayManager>> = None;
/// Provides access to the global `OverlayManager` singleton.
fn get_overlay_manager() -> Option<&'static Mutex<OverlayManager>> {
    unsafe {
        OVERLAY_MANAGER.call_once(|| {
            GLOBAL_OVERLAY_MANAGER = Some(Mutex::new(OverlayManager::new()));
        });
        GLOBAL_OVERLAY_MANAGER.as_ref()
    }
}

mod keybind;
#[cfg(test)]
mod tests;
mod types;
use keybind::{Keybind, parse_keybind};
pub use keybind::{KeybindCallback, VisibilityToggleCallback};
pub use types::{FlutterRenderPass, FontAtlasSpec};

/// Manages all active Flutter overlay instances.
///
/// This struct is the central point for creating, tracking, rendering, and managing the lifecycle
/// of all Flutter overlays. It is not intended to be used directly, but rather through the
/// `FlutterOverlayManagerHandle`.
pub struct OverlayManager {
    /// Stores the actual FlutterOverlay instances, keyed by a unique identifier.
    pub active_instances: HashMap<String, Box<FlutterOverlay>>,
    /// Defines the rendering and input priority. The last element is considered topmost.
    pub overlay_order: Vec<String>,
    /// Identifier of the overlay that currently has keyboard focus.
    pub focused_overlay_id: Option<String>,
    /// Shared Direct3D device context for ticking overlays.
    shared_d3d_context: Option<ID3D11DeviceContext>,
    swap_chain: Option<IDXGISwapChain>,
    /// The width of the screen in pixels.
    screen_width: u32,
    /// The height of the screen in pixels.
    screen_height: u32,
    /// The time when the `OverlayManager` was created or resumed.
    start_time: Instant,
    /// Indicates whether the `OverlayManager` is currently paused.
    is_paused: bool,
    /// The time in seconds when the `OverlayManager` was paused.
    time_at_pause: f32,
    /// Cooldown counter for device recovery attempts. When > 0, recovery won't be attempted.
    recovery_cooldown: u32,
    /// Keybind-to-overlay visibility toggles. Key: original keybind string, Value: (parsed keybind, overlay_id, optional callback).
    /// Processed *before* the visibility gate so hidden overlays can be toggled back on.
    visibility_toggles: Vec<(String, Keybind, String, Option<VisibilityToggleCallback>)>,
    /// Generic keybind actions. (key_string, keybind, action_id, overlay_id, callback, allow_repeat).
    /// Fired at the manager level so they work even when overlays are invisible.
    /// The callback fires directly on the input thread — no platform message involved.
    keybind_actions: Vec<(
        String,
        Keybind,
        String,
        String,
        Option<KeybindCallback>,
        bool,
    )>,
}

impl OverlayManager {
    /// Creates a new, empty `OverlayManager`.
    fn new() -> Self {
        OverlayManager {
            active_instances: HashMap::new(),
            overlay_order: Vec::new(),
            focused_overlay_id: None,
            shared_d3d_context: None,
            swap_chain: None,
            screen_width: 0,
            screen_height: 0,
            start_time: Instant::now(),
            is_paused: false,
            time_at_pause: 0.0,
            recovery_cooldown: 0,
            visibility_toggles: Vec::new(),
            keybind_actions: Vec::new(),
        }
    }

    /// Gets an immutable reference to a target overlay.
    ///
    /// If `identifier` is `None`, it attempts to get the single active instance.
    fn get_instance(&self, identifier: Option<&str>) -> Result<&FlutterOverlay, String> {
        match identifier {
            Some(id) => self
                .active_instances
                .get(id)
                .map(Box::as_ref)
                .ok_or_else(|| format!("No overlay with identifier '{id}' found.")),
            None => {
                if self.active_instances.len() == 1 {
                    Ok(self.active_instances.values().next().unwrap().as_ref())
                } else if self.active_instances.is_empty() {
                    Err("No active overlay instance found.".to_string())
                } else {
                    Err("Multiple overlays exist; an identifier is required.".to_string())
                }
            }
        }
    }

    /// Gets a mutable reference to a target overlay.
    ///
    /// If `identifier` is `None`, it attempts to get the single active instance.
    fn get_instance_mut(
        &mut self,
        identifier: Option<&str>,
    ) -> Result<&mut Box<FlutterOverlay>, String> {
        match identifier {
            Some(id) => self
                .active_instances
                .get_mut(id)
                .ok_or_else(|| format!("No overlay with identifier '{id}' found.")),
            None => {
                if self.active_instances.len() == 1 {
                    Ok(self.active_instances.values_mut().next().unwrap())
                } else if self.active_instances.is_empty() {
                    Err("No active overlay instance found.".to_string())
                } else {
                    Err("Multiple overlays exist; an identifier is required.".to_string())
                }
            }
        }
    }

    /// Spawns a real, separate top-level OS window hosting a new Flutter view
    /// driven by the named overlay's engine (shared Dart state with the in-game
    /// overlay). The crate creates the window, its swapchain, and its render
    /// loop; the caller only supplies a title + size.
    ///
    /// Call this from your in-game editor when the user clicks "open in new
    /// window". Returns a [`SatelliteWindow`] handle — keep it to close the
    /// window programmatically, or let the user close it.
    ///
    /// Requires the overlay to be using the OpenGL (hardware) renderer.
    ///
    /// [`SatelliteWindow`]: crate::software_renderer::multiview::window::SatelliteWindow
    pub fn spawn_window_for_overlay(
        &mut self,
        identifier: Option<&str>,
        spec: WindowSpec,
    ) -> Result<SatelliteWindow, FlutterEmbedderError> {
        // Resolve the game device from the manager's swapchain.
        let swap_chain = self.swap_chain.as_ref().ok_or_else(|| {
            FlutterEmbedderError::OperationFailed(
                "OverlayManager has no swap chain; cannot derive game device".to_string(),
            )
        })?;
        let game_device: ID3D11Device = unsafe { swap_chain.GetDevice() }.map_err(|e| {
            FlutterEmbedderError::OperationFailed(format!("swap_chain.GetDevice failed: {e}"))
        })?;

        let overlay = self
            .get_instance_mut(identifier)
            .map_err(FlutterEmbedderError::OperationFailed)?;

        // SAFETY: the overlay lives in a `Box` inside `active_instances`, so its
        // address is stable for as long as the entry exists. The caller must not
        // remove/shutdown this overlay while the satellite window is open.
        unsafe { overlay.spawn_window(&game_device, spec) }
    }

    /// Add an offscreen Flutter view (no OS window) to the given overlay and
    /// return its view id. The game reads the view's texture via
    /// `offscreen_view_srvs` and draws it as a native UI layer.
    pub fn add_offscreen_view_for_overlay(
        &mut self,
        identifier: Option<&str>,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<FlutterViewId, FlutterEmbedderError> {
        let swap_chain = self.swap_chain.as_ref().ok_or_else(|| {
            FlutterEmbedderError::OperationFailed(
                "OverlayManager has no swap chain; cannot derive game device".to_string(),
            )
        })?;
        let game_device: ID3D11Device = unsafe { swap_chain.GetDevice() }.map_err(|e| {
            FlutterEmbedderError::OperationFailed(format!("swap_chain.GetDevice failed: {e}"))
        })?;

        let overlay = self
            .get_instance_mut(identifier)
            .map_err(FlutterEmbedderError::OperationFailed)?;

        overlay.add_offscreen_view(&game_device, width, height, pixel_ratio)
    }

    /// Returns `(view_id, srv)` for every secondary view on the given overlay,
    /// sorted by ascending view id. Ids are allocated monotonically, so the sort
    /// makes the surface->layer mapping deterministic across frames.
    pub fn offscreen_view_srvs(
        &self,
        identifier: Option<&str>,
    ) -> Vec<(FlutterViewId, ID3D11ShaderResourceView)> {
        let Ok(overlay) = self.get_instance(identifier) else {
            return Vec::new();
        };
        let mut out: Vec<(FlutterViewId, ID3D11ShaderResourceView)> = overlay
            .secondary_view_ids()
            .into_iter()
            .filter_map(|id| overlay.view_texture_srv(id).map(|srv| (id, srv)))
            .collect();
        out.sort_by_key(|(id, _)| *id);
        out
    }

    /// Set where an offscreen view is drawn on screen (client px) so pointer
    /// events can be routed to it. Pass `None` to disable input for the view.
    pub fn set_offscreen_view_rect(
        &self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
        rect: Option<(f32, f32, f32, f32)>,
    ) {
        if let Ok(overlay) = self.get_instance(identifier) {
            overlay.set_view_screen_rect(view_id, rect);
        }
    }

    /// Re-metrics an offscreen view to a new size (invalidates its SRV).
    pub fn resize_offscreen_view(
        &mut self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<(), FlutterEmbedderError> {
        let overlay = self
            .get_instance_mut(identifier)
            .map_err(FlutterEmbedderError::OperationFailed)?;
        overlay.resize_view(view_id, width, height, pixel_ratio)
    }

    /// Removes an offscreen view from the engine.
    pub fn remove_offscreen_view(
        &mut self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
    ) -> Result<(), FlutterEmbedderError> {
        let overlay = self
            .get_instance_mut(identifier)
            .map_err(FlutterEmbedderError::OperationFailed)?;
        overlay.remove_view(view_id)
    }

    /// Retrieves the dimensions (width, height) for all active overlays.
    ///
    /// # Returns
    ///
    /// A `HashMap` where keys are overlay identifiers and values are tuples
    /// containing the width and height of the overlay.
    pub fn get_all_overlay_dimensions(&self) -> HashMap<String, (u32, u32)> {
        self.active_instances
            .iter()
            .map(|(id, overlay)| (id.clone(), overlay.get_dimensions()))
            .collect()
    }

    /// Finds the topmost, visible overlay that contains the given screen coordinates.
    pub fn find_topmost_overlay_at_position(&self, x: i32, y: i32) -> Option<String> {
        for identifier in self.overlay_order.iter().rev() {
            if let Some(overlay) = self.active_instances.get(identifier) {
                if !overlay.is_visible() {
                    continue;
                }
                let (ox, oy) = overlay.get_position();
                let (ow, oh) = overlay.get_dimensions();
                if x >= ox && x < ox + (ow as i32) && y >= oy && y < oy + (oh as i32) {
                    return Some(identifier.clone());
                }
            }
        }
        None
    }

    /// Gets a clone of the shared Direct3D device context.
    pub fn get_d3d_context(&self) -> Option<ID3D11DeviceContext> {
        self.shared_d3d_context.clone()
    }

    /// Latches all queued primitives for all overlay instances.
    pub fn latch_all_queued_primitives(&mut self) {
        for overlay in self.active_instances.values_mut() {
            overlay.latch_queued_primitives();
        }
    }

    /// Latches all queued text for all overlay instances.
    pub fn latch_all_queued_text(&mut self) {
        for overlay in self.active_instances.values_mut() {
            overlay.latch_queued_text();
        }
    }

    /// Internal helper to add an overlay instance and manage its order and focus.
    fn add_overlay_instance(&mut self, identifier: String, overlay_box: Box<FlutterOverlay>) {
        if self.active_instances.contains_key(&identifier) {
            warn!(
                "[OverlayManager] Overlay with identifier '{identifier}' already exists. It will be replaced and brought to front."
            );
            if let Some(old_overlay) = self.active_instances.remove(&identifier)
                && let Err(e) = old_overlay.shutdown()
            {
                error!(
                    "[OverlayManager] Error shutting down old overlay instance '{identifier}' during replacement: {e}"
                );
            }
            self.overlay_order.retain(|id| id != &identifier);
        }

        self.active_instances
            .insert(identifier.clone(), overlay_box);
        self.overlay_order.push(identifier.clone());

        if self.focused_overlay_id.is_none() {
            self.focused_overlay_id = Some(identifier.clone());
        }
    }

    /// Initializes a new Flutter overlay instance.
    fn init(
        &mut self,
        swap_chain: &IDXGISwapChain,
        flutter_asset_dir: &Path,
        identifier: &str,
        dart_args_for_this_instance: Option<Vec<String>>,
        engine_args_opt: Option<Vec<String>>,
    ) -> bool {
        if self.active_instances.contains_key(identifier) {
            self.bring_to_front(Some(identifier));
            // self.set_keyboard_focus(identifier);
            return true;
        }

        let device = match unsafe { swap_chain.GetDevice::<ID3D11Device>() } {
            Ok(d) => d,
            Err(e) => {
                error!(
                    "[OverlayManager:{identifier}] Failed to get D3D11 Device from swap chain: {e:?}"
                );
                return false;
            }
        };

        if self.shared_d3d_context.is_none() {
            match unsafe { device.GetImmediateContext() } {
                Ok(ctx) => {
                    self.shared_d3d_context = Some(ctx);
                }
                Err(e) => {
                    error!(
                        "[OverlayManager:{identifier}] Failed to get D3D11 Immediate Context: {e:?}"
                    );
                    return false;
                }
            }
        }

        let get_desc_result: WindowsResult<DXGI_SWAP_CHAIN_DESC> = unsafe { swap_chain.GetDesc() };

        let desc: DXGI_SWAP_CHAIN_DESC = match get_desc_result {
            Ok(d) => d,
            Err(e) => {
                error!("[OverlayManager:{identifier}] Failed to get SwapChain description: {e:?}");
                return false;
            }
        };

        let width = desc.BufferDesc.Width;
        let height = desc.BufferDesc.Height;

        self.screen_width = width;
        self.screen_height = height;
        SCREEN_SIZE.store(((width as u64) << 32) | height as u64, Ordering::Release);
        self.swap_chain = Some(swap_chain.clone());

        init_logging();

        match FlutterOverlay::create(
            OverlayCreateParams {
                name: identifier.to_string(),
                x: 0,
                y: 0,
                width,
                height,
                flutter_data_dir: flutter_asset_dir.to_path_buf(),
                dart_entrypoint_args: dart_args_for_this_instance,
                engine_args: engine_args_opt,
            },
            &device,
            swap_chain,
        ) {
            Ok(overlay_box) => {
                self.add_overlay_instance(identifier.to_string(), overlay_box);
                info!(
                    "[OverlayManager:{identifier}] Flutter overlay initialized and added to manager."
                );
                true
            }
            Err(e) => {
                error!(
                    "[OverlayManager:{identifier}] Failed to create FlutterOverlay instance: {e:?}"
                );
                false
            }
        }
    }

    /// Handles input events, routing them based on Z-order and focus.
    fn handle_input_event(
        &mut self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> (bool, Option<(String, bool, VisibilityToggleCallback)>) {
        if self.active_instances.is_empty() {
            return (false, None);
        }

        // Visibility toggle keybinds — processed BEFORE the visibility gate
        // so that hidden overlays can be toggled back on. Only fires on key-down.
        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
            let vk = wparam.0 as u16;
            let is_repeat = (lparam.0 >> 30) & 1 != 0;
            for (_key_str, keybind, overlay_id, callback) in &self.visibility_toggles {
                if keybind.vk == vk && keybind.modifiers_match() {
                    if is_repeat {
                        return (true, None);
                    }
                    if let Some(overlay) = self.active_instances.get_mut(overlay_id) {
                        if overlay.keep_alive {
                            let ui_hidden = !overlay.ui_hidden;
                            overlay.ui_hidden = ui_hidden;
                            let msg_payload = if ui_hidden {
                                b"false" as &[u8]
                            } else {
                                b"true"
                            };
                            let _ =
                                overlay.send_platform_message("overlay/visibility", msg_payload);
                            let deferred = callback
                                .as_ref()
                                .map(|cb| (overlay_id.clone(), !ui_hidden, cb.clone()));
                            return (true, deferred);
                        }

                        let was_visible = overlay.is_visible();
                        let new_visible = !was_visible;
                        overlay.set_visibility(new_visible);

                        if new_visible
                            && !was_visible
                            && let Some(sc) = &self.swap_chain
                        {
                            overlay.handle_window_resize_force(
                                0,
                                0,
                                self.screen_width,
                                self.screen_height,
                                sc,
                            );
                        }

                        let msg_payload = if new_visible {
                            b"true" as &[u8]
                        } else {
                            b"false"
                        };
                        let _ = overlay.send_platform_message("overlay/visibility", msg_payload);
                        let deferred = callback
                            .as_ref()
                            .map(|cb| (overlay_id.clone(), new_visible, cb.clone()));
                        return (true, deferred);
                    }
                }
            }
        }

        // Generic keybind actions — also processed before the visibility gate.
        if matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN) {
            let vk = wparam.0 as u16;
            let is_repeat = (lparam.0 >> 30) & 1 != 0;
            for (_key_str, keybind, action_id, _overlay_id, callback, allow_repeat) in
                &self.keybind_actions
            {
                if keybind.vk == vk && keybind.modifiers_match() {
                    if is_repeat && !allow_repeat {
                        return (true, None);
                    }
                    if let Some(cb) = callback {
                        cb(action_id);
                    }
                    return (true, None);
                }
            }
        }

        let is_pointer_event = matches!(
            msg,
            WM_MOUSEMOVE
                | WM_LBUTTONDOWN
                | WM_RBUTTONDOWN
                | WM_MBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONUP
                | WM_MBUTTONUP
                | WM_NCMOUSELEAVE
                | WM_MOUSEWHEEL
        );

        let is_key_event = matches!(
            msg,
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP | WM_CHAR
        );

        if matches!(msg, WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN)
            && let Some(instance) = self
                .focused_overlay_id
                .as_ref()
                .and_then(|id| self.active_instances.get(id))
                .or_else(|| self.active_instances.values().next())
        {
            instance.send_view_focus(0, true);
        }

        if is_pointer_event {
            let overlay_order_copy: Vec<String> = self.overlay_order.clone();

            for identifier in overlay_order_copy.iter().rev() {
                if let Some(overlay_instance) = self.active_instances.get(identifier) {
                    if !overlay_instance.is_visible() {
                        continue;
                    }

                    overlay_instance.handle_pointer_event(hwnd, msg, wparam, lparam);

                    if overlay_instance
                        .is_interactive_widget_hovered
                        .load(Ordering::SeqCst)
                    {
                        self.bring_to_front(Some(identifier));
                        return (true, None);
                    }
                }
            }
        } else if is_key_event
            && let Some(focused_id) = &self.focused_overlay_id
            && let Some(overlay_instance) = self.active_instances.get(focused_id)
            && overlay_instance.handle_keyboard_event(msg, wparam, lparam)
        {
            return (true, None);
        }

        (false, None)
    }

    /// Handles WM_SETCURSOR, respecting Z-order and hover states.
    fn handle_set_cursor(
        &self,
        hwnd_for_setcursor_message: HWND,
        lparam_from_message: LPARAM,
        main_app_hwnd: HWND,
    ) -> Option<LRESULT> {
        for identifier in self.overlay_order.iter().rev() {
            // Topmost first
            if let Some(overlay_instance) = self.active_instances.get(identifier)
                && overlay_instance
                    .is_interactive_widget_hovered
                    .load(std::sync::atomic::Ordering::SeqCst)
                && let Some(lresult) = overlay_instance.handle_set_cursor(
                    hwnd_for_setcursor_message,
                    lparam_from_message,
                    main_app_hwnd,
                )
            {
                return Some(lresult);
            }
        }
        None
    }

    /// Handles resizing for all active overlays.
    fn handle_resize(
        &mut self,
        swap_chain: &IDXGISwapChain,
        x_pos: i32,
        y_pos: i32,
        width: u32,
        height: u32,
    ) {
        self.screen_width = width;
        self.screen_height = height;
        SCREEN_SIZE.store(((width as u64) << 32) | height as u64, Ordering::Release);
        self.swap_chain = Some(swap_chain.clone());

        if self.active_instances.is_empty() {
            return;
        }

        for (id, overlay_instance) in self.active_instances.iter_mut() {
            if !overlay_instance.engine.0.is_null() {
                overlay_instance.handle_window_resize(x_pos, y_pos, width, height, swap_chain);
            } else {
                warn!("[OverlayManager:{id}] Engine handle is null, cannot resize.");
            }
        }
    }

    /// Shuts down a specific Flutter overlay instance.
    fn shutdown_instance(&mut self, identifier: &str) -> Result<(), FlutterEmbedderError> {
        if let Some(overlay_box) = self.active_instances.remove(identifier) {
            info!("[OverlayManager:{identifier}] Shutting down overlay instance.");
            self.overlay_order.retain(|id| id != identifier);

            if self.focused_overlay_id.as_deref() == Some(identifier) {
                self.focused_overlay_id = self.overlay_order.last().cloned();
            }
            overlay_box.shutdown()
        } else {
            warn!(
                "[OverlayManager:{identifier}] Shutdown called for unknown or already removed instance."
            );
            Ok(())
        }
    }

    /// Shuts down all active Flutter overlay instances.
    pub fn shutdown_all_instances(&mut self) {
        let all_ids: Vec<String> = self.active_instances.keys().cloned().collect();

        for id in all_ids {
            if let Err(e) = self.shutdown_instance(&id) {
                error!("[OverlayManager] Error during shutdown of instance {id}: {e}");
            }
        }

        info!("[OverlayManager] All instances shut down.");
    }

    /// Sends the same platform message to all visible overlay instances.
    /// This is useful for broadcasting global events.
    ///
    /// # Arguments
    /// * `channel` - The channel to send the message on.
    /// * `message` - The message to send.
    pub fn broadcast_platform_message(&self, channel: &str, message: &[u8]) {
        for (id, overlay) in self.active_instances.iter() {
            if !overlay.is_visible() {
                continue;
            }

            let pending_message = PendingPlatformMessage {
                channel: channel.to_string(),
                payload_bytes: message.to_vec(),
            };

            if let Ok(mut queue) = overlay.pending_platform_messages.lock() {
                queue.push_back(pending_message);
            } else {
                error!("[OverlayManager] Failed to queue message for overlay '{id}': lock failed");
            }
            overlay.task_queue_state.waker.wake_up();
        }
    }

    /// Gets the rendered textures from all active and visible overlays.
    ///
    /// # Returns
    /// A vector of tuples, where each tuple contains the overlay identifier and its corresponding
    /// `ID3D11ShaderResourceView`.
    pub fn get_all_overlay_textures(&self) -> Vec<(String, ID3D11ShaderResourceView)> {
        let mut textures = Vec::new();

        for identifier in &self.overlay_order {
            if let Some(overlay) = self.active_instances.get(identifier)
                && overlay.is_visible()
                && let Ok(texture_srv) = overlay.get_texture_srv()
            {
                textures.push((identifier.clone(), texture_srv));
            }
        }

        textures
    }

    /// Checks if the specified overlay currently has keyboard focus.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    pub fn is_focused(&self, identifier: Option<&str>) -> bool {
        if let Ok(overlay) = self.get_instance(identifier) {
            self.focused_overlay_id.as_deref() == Some(overlay.name.as_str())
        } else {
            false
        }
    }

    /// Sets the visibility of a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `is_visible` - A boolean indicating whether the overlay should be visible (`true`) or hidden (`false`).
    pub fn set_overlay_visibility(&mut self, identifier: Option<&str>, is_visible: bool) {
        match self.get_instance_mut(identifier) {
            Ok(overlay) => overlay.set_visibility(is_visible),
            Err(e) => warn!("[OverlayManager] set_overlay_visibility failed: {e}"),
        }
    }

    /// Registers a keybind that toggles an overlay's visibility.
    ///
    /// The toggle is processed *before* the visibility gate in input handling,
    /// which solves the chicken-and-egg problem: a hidden overlay can't receive
    /// the key event that would make it visible again, so the manager handles it.
    ///
    /// # Arguments
    /// * `key_name` - A human-readable key name (e.g., `"F1"`, `"Escape"`, `"T"`).
    /// * `overlay_id` - The identifier of the overlay to toggle.
    /// * `callback` - Optional callback invoked after toggling with `(overlay_id, new_visibility)`.
    ///   Return `true` to consume the event.
    pub fn register_visibility_toggle(
        &mut self,
        key_name: &str,
        overlay_id: &str,
        callback: Option<VisibilityToggleCallback>,
    ) {
        if let Some(keybind) = parse_keybind(key_name) {
            // Remove any existing toggle for the same key string
            self.visibility_toggles.retain(|(k, _, _, _)| k != key_name);
            info!(
                "[OverlayManager] Registered visibility toggle: '{}' (VK 0x{:X}, ctrl={}, shift={}, alt={}) → overlay '{}'",
                key_name, keybind.vk, keybind.ctrl, keybind.shift, keybind.alt, overlay_id
            );
            self.visibility_toggles.push((
                key_name.to_string(),
                keybind,
                overlay_id.to_string(),
                callback,
            ));
        } else {
            warn!("[OverlayManager] Failed to parse keybind '{key_name}' for visibility toggle");
        }
    }

    /// Removes a previously registered visibility toggle keybind.
    pub fn unregister_visibility_toggle(&mut self, key_name: &str) {
        let before = self.visibility_toggles.len();
        self.visibility_toggles.retain(|(k, _, _, _)| k != key_name);
        if self.visibility_toggles.len() == before {
            warn!("[OverlayManager] No visibility toggle found for '{key_name}' to unregister");
        }
    }

    /// Registers a generic keybind action that fires a callback.
    /// Works regardless of overlay visibility.
    pub fn register_keybind_action(
        &mut self,
        key_name: &str,
        action_id: &str,
        overlay_id: &str,
        callback: Option<KeybindCallback>,
        allow_repeat: bool,
    ) {
        if let Some(keybind) = parse_keybind(key_name) {
            self.keybind_actions
                .retain(|(k, _, a, _, _, _)| !(k == key_name && a == action_id));
            self.keybind_actions.push((
                key_name.to_string(),
                keybind,
                action_id.to_string(),
                overlay_id.to_string(),
                callback,
                allow_repeat,
            ));
        } else {
            warn!("[OverlayManager] Failed to parse keybind '{key_name}' for action '{action_id}'");
        }
    }

    /// Removes a keybind action by action_id.
    pub fn unregister_keybind_action(&mut self, action_id: &str) {
        self.keybind_actions
            .retain(|(_, _, a, _, _, _)| a != action_id);
    }

    /// Updates the key for an existing keybind action (rebind).
    pub fn rebind_keybind_action(&mut self, action_id: &str, new_key_name: &str) {
        if let Some(new_keybind) = parse_keybind(new_key_name) {
            for (key_str, keybind, a, _, _, _) in &mut self.keybind_actions {
                if a == action_id {
                    *key_str = new_key_name.to_string();
                    *keybind = new_keybind.clone();
                }
            }
        } else {
            warn!(
                "[OverlayManager] Failed to parse keybind '{new_key_name}' for rebind of '{action_id}'"
            );
        }
    }

    /// Registers a Dart port for a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    pub fn register_dart_port(&self, identifier: Option<&str>, port: i64) {
        match self.get_instance(identifier) {
            Ok(overlay) => overlay.register_dart_port(port),
            Err(e) => warn!("[OverlayManager] register_dart_port failed: {e}"),
        }
    }

    /// Posts a boolean message to a specific overlay instance.
    /// Posts a boolean message to a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `value` - The boolean value to post.
    pub fn post_bool_to_overlay(
        &self,
        identifier: Option<&str>,
        value: bool,
    ) -> Result<(), FlutterEmbedderError> {
        self.get_instance(identifier)
            .and_then(|overlay| overlay.post_bool(value).map_err(|e| e.to_string()))
            .map_err(|e| {
                warn!("[OverlayManager] post_bool_to_overlay failed: {e}");
                FlutterEmbedderError::InvalidHandle
            })
    }

    /// Posts an i64 message to a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `value` - The i64 value to post.
    pub fn post_i64_to_overlay(
        &self,
        identifier: Option<&str>,
        value: i64,
    ) -> Result<(), FlutterEmbedderError> {
        self.get_instance(identifier)
            .and_then(|overlay| overlay.post_i64(value).map_err(|e| e.to_string()))
            .map_err(|e| {
                warn!("[OverlayManager] post_i64_to_overlay failed: {e}");
                FlutterEmbedderError::InvalidHandle
            })
    }

    /// Posts an f64 message to a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `value` - The f64 value to post.
    pub fn post_f64_to_overlay(
        &self,
        identifier: Option<&str>,
        value: f64,
    ) -> Result<(), FlutterEmbedderError> {
        self.get_instance(identifier)
            .and_then(|overlay| overlay.post_f64(value).map_err(|e| e.to_string()))
            .map_err(|e| {
                warn!("[OverlayManager] post_f64_to_overlay failed: {e}");
                FlutterEmbedderError::InvalidHandle
            })
    }

    /// Posts a string message to a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `value` - The string value to post.
    pub fn post_string_to_overlay(
        &self,
        identifier: Option<&str>,
        value: &str,
    ) -> Result<(), FlutterEmbedderError> {
        self.get_instance(identifier)
            .and_then(|overlay| overlay.post_string(value).map_err(|e| e.to_string()))
            .map_err(|e| {
                warn!("[OverlayManager] post_string_to_overlay failed: {e}");
                FlutterEmbedderError::InvalidHandle
            })
    }

    /// Posts a byte buffer to a specific overlay instance.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `buffer` - The byte buffer to post.
    pub fn post_buffer_to_overlay(
        &self,
        identifier: Option<&str>,
        buffer: &[u8],
    ) -> Result<(), FlutterEmbedderError> {
        self.get_instance(identifier)
            .and_then(|overlay| overlay.post_buffer(buffer).map_err(|e| e.to_string()))
            .map_err(|e| {
                warn!("[OverlayManager] post_buffer_to_overlay failed: {e}");
                FlutterEmbedderError::InvalidHandle
            })
    }

    /// Sets the screen-space position for a specific overlay.
    pub fn set_overlay_position(&mut self, identifier: Option<&str>, x: i32, y: i32) {
        match self.get_instance_mut(identifier) {
            Ok(overlay) => overlay.set_position(x, y),
            Err(e) => warn!("[OverlayManager] set_overlay_position failed: {e}"),
        }
    }

    /// Registers a custom channel handler for a specific overlay instance.
    pub fn register_channel_handler_for_instance<F>(
        &mut self,
        identifier: Option<&str>,
        channel: &str,
        handler: F,
    ) where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        match self.get_instance_mut(identifier) {
            Ok(overlay) => overlay.register_channel_handler(channel, handler),
            Err(e) => warn!("[OverlayManager] register_channel_handler failed: {e}"),
        }
    }

    /// Brings the specified overlay to the top of the Z-order.
    pub fn bring_to_front(&mut self, identifier: Option<&str>) {
        if let Ok(id_str) = self.get_instance(identifier).map(|ov| ov.name.clone()) {
            self.overlay_order.retain(|id| id != &id_str);
            self.overlay_order.push(id_str);
        }
    }

    /// Sets keyboard focus to the specified overlay and brings it to the front.
    pub fn set_keyboard_focus(&mut self, identifier: Option<&str>) {
        if let Ok(id_str) = self.get_instance(identifier).map(|ov| ov.name.clone()) {
            self.focused_overlay_id = Some(id_str.clone());
            self.bring_to_front(Some(&id_str));
        }
    }
}

impl FlutterOverlayManagerHandle {
    /// Creates and initializes a new Flutter overlay instance and adds it to the manager.
    ///
    /// This function is the primary entry point for creating a new Flutter UI surface. It
    /// handles loading the Flutter engine, preparing rendering resources, and running the
    /// Dart isolate. If an overlay with the same `identifier` already exists, it is
    /// shut down and replaced by the new instance.
    ///
    /// # What it solves
    /// This is the foundational step to get any Flutter UI running. It abstracts away the
    /// complexities of engine startup, renderer configuration, and asset loading.
    ///
    /// # Renderer Selection
    ///
    /// This function automatically determines the best available renderer. It will first
    /// attempt to initialize a hardware-accelerated **OpenGL** renderer via ANGLE.
    ///
    /// If OpenGL initialization fails for any reason (e.g., `libEGL.dll` or `libGLESv2.dll`
    /// are not found, or a graphics driver issue occurs), it will log an error and
    /// automatically fall back to a **Software** renderer. This ensures that the overlay
    /// can be displayed even on systems without proper OpenGL support.
    ///
    /// # Arguments
    ///
    /// * `swap_chain`: A reference to the host application's `IDXGISwapChain`.
    /// * `flutter_asset_build_dir`: Path to the Flutter application's build output
    ///   directory. This can be the output of a standard `flutter build windows` command
    ///   (e.g., `build/windows/runner/Release`) or the output of a `flutter assemble`
    ///   command. An example `assemble` command is:
    ///   ```bash
    ///   flutter assemble --output=build -dTargetPlatform=windows-x64 -dBuildMode={build_mode} {build_mode}_bundle_windows-x64_assets
    ///   ```
    ///   Regardless of the method used, this directory must contain the necessary Flutter
    ///   assets (`flutter_assets`), `icudtl.dat`, the compiled Dart code, and the
    ///   `flutter_engine.dll`.
    ///   - For **OpenGL** support, this directory must also contain `libEGL.dll` and `libGLESv2.dll`.
    /// * `identifier`: A unique string that identifies this overlay instance for all
    ///   subsequent API calls (e.g., "main_menu_ui").
    /// * `dart_args`: Optional. A vector of string arguments for the Dart `main()` function.
    /// * `engine_args`: Optional. A vector of command-line switches for the Flutter Engine,
    ///   typically used in debug builds.
    ///
    /// # Returns
    ///
    /// Returns `true` if the overlay was initialized successfully using either the OpenGL
    /// or Software renderer. Returns `false` if a critical error occurred and initialization
    /// failed completely. Errors are logged internally.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let assets_path = PathBuf::from("./flutter_build");
    /// manager.init_instance(
    ///     &my_swap_chain,
    ///     &assets_path,
    ///     "main_hud",
    ///     None, // No special Dart arguments
    ///     None, // No special engine arguments
    /// );
    /// ```
    pub fn init_instance(
        &self,
        swap_chain: &IDXGISwapChain,
        flutter_asset_build_dir: &Path,
        identifier: &str,
        dart_args: Option<Vec<String>>,
        engine_args: Option<Vec<String>>,
    ) -> bool {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.init(
                swap_chain,
                flutter_asset_build_dir,
                identifier,
                dart_args,
                engine_args,
            )
        } else {
            false
        }
    }

    /// Add an offscreen Flutter view (no OS window) to an overlay; returns its
    /// view id. See `OverlayManager::add_offscreen_view_for_overlay`.
    pub fn add_offscreen_view(
        &self,
        identifier: Option<&str>,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<FlutterViewId, FlutterEmbedderError> {
        self.manager
            .lock()
            .add_offscreen_view_for_overlay(identifier, width, height, pixel_ratio)
    }

    /// `(view_id, srv)` for every secondary view on an overlay, sorted by id.
    pub fn offscreen_view_srvs(
        &self,
        identifier: Option<&str>,
    ) -> Vec<(FlutterViewId, ID3D11ShaderResourceView)> {
        self.manager.lock().offscreen_view_srvs(identifier)
    }

    /// Set an offscreen view's on-screen rect (client px) for pointer routing.
    pub fn set_offscreen_view_rect(
        &self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
        rect: Option<(f32, f32, f32, f32)>,
    ) {
        self.manager
            .lock()
            .set_offscreen_view_rect(identifier, view_id, rect);
    }

    /// Re-metrics an offscreen view to a new size.
    pub fn resize_offscreen_view(
        &self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<(), FlutterEmbedderError> {
        self.manager
            .lock()
            .resize_offscreen_view(identifier, view_id, width, height, pixel_ratio)
    }

    /// Removes an offscreen view from the engine.
    pub fn remove_offscreen_view(
        &self,
        identifier: Option<&str>,
        view_id: FlutterViewId,
    ) -> Result<(), FlutterEmbedderError> {
        self.manager
            .lock()
            .remove_offscreen_view(identifier, view_id)
    }

    /// Renders all latched 3D primitives for all visible overlays.
    ///
    /// This function is the primary method for drawing 3D geometry (e.g., entity highlights,
    /// debug lines, world-space widgets) that needs to correctly interact with the host
    /// application's depth buffer. It handles the transformation of vertices using the
    /// game's camera, rendering to the entire available surface.
    ///
    /// ## Usage
    /// This should be called once per frame from your main render hook (e.g., `new_present`).
    /// For correct depth testing and integration, it must be called *before* any
    /// post-processing effects that flatten or scale the game's render target.
    ///
    /// ## Arguments
    ///
    /// * `view_projection_matrix`: The combined view and projection matrix from the host camera.
    /// * `depth_stencil_view`: The host application's active `ID3D11DepthStencilView` to use
    ///   for depth testing.
    ///
    /// ## Example
    /// ```rust, no_run
    /// // In your main render hook (e.g., dxgi_present_hook.rs)
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let game_dsv = get_current_game_dsv();
    /// let game_matrix = get_current_game_matrix();
    ///
    /// manager.render_primitives(
    ///     &game_matrix,
    ///     &Some(game_dsv)
    /// );
    ///
    /// // ... now run post-processing on the combined scene ...
    /// ```
    pub fn render_primitives(
        &self,
        view_projection_matrix: &XMMatrix,
        depth_stencil_view: &Option<ID3D11DepthStencilView>,
    ) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };

        let context = match manager.shared_d3d_context.clone() {
            Some(ctx) => ctx,
            None => {
                return;
            }
        };

        let time = manager.start_time.elapsed().as_secs_f32();

        let frame_params = FrameParams {
            context: &context,
            view_projection_matrix,
            depth_stencil_view,
            screen_width: 0.0,
            screen_height: 0.0,
            time,
        };

        for overlay in manager.active_instances.values_mut() {
            {
                overlay.primitive_renderer.draw(&frame_params);
                overlay.text_renderer.draw(&frame_params);
            }
        }
    }

    /// Ticks the Flutter engine and composites the final 2D UI for all visible overlays.
    ///
    /// This function handles two critical tasks: it drives the Flutter engine's internal
    /// state and animations (`tick`), and then it draws the resulting UI texture onto the
    /// screen. It should be called at the end of your render loop to ensure the UI
    /// appears on top of all other game and primitive rendering.
    ///
    /// ## Usage
    /// Call this once per frame in your main render hook after all 3D scene rendering
    /// and post-processing is complete.
    ///
    /// ## Example
    /// ```rust, no_run
    /// // In your main render hook (e.g., dxgi_present_hook.rs)
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    ///
    /// // ... render game world and 3D primitives ...
    /// // ... run post-processing ...
    ///
    /// // Finally, draw the UI on top of everything.
    /// manager.render_ui();
    /// ```
    pub fn render_ui(&self) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };

        let context = match manager.shared_d3d_context.clone() {
            Some(ctx) => ctx,
            None => return,
        };

        let time = manager.start_time.elapsed().as_secs_f32();
        let identity_matrix = XMMatrix(XMMatrixIdentity());

        let frame_params = FrameParams {
            context: &context,
            view_projection_matrix: &identity_matrix,
            depth_stencil_view: &None,
            screen_width: manager.screen_width as f32,
            screen_height: manager.screen_height as f32,
            time,
        };

        let mut rendered_any = false;

        for (_id, overlay) in manager.active_instances.iter_mut() {
            if overlay.is_visible() && overlay.has_first_frame() {
                overlay.reopen_shared_texture_if_needed(&context);
                overlay.tick(&context);
                for view_id in overlay.secondary_view_ids() {
                    overlay.tick_view(view_id, &context);
                }
                update_interactive_widget_hover_state(overlay);

                if overlay.effect_frames_remaining > 0 {
                    overlay.effect_frames_remaining -= 1;
                    let t = 1.0
                        - (overlay.effect_frames_remaining as f32
                            / overlay.effect_total_frames.max(1) as f32);
                    let fade = 1.0 - t;
                    overlay.effect_config.params = EffectParams::Glitch(HologramParams {
                        aberration_amount: 0.005 * fade,
                        glitch_speed: 10.0 * fade,
                        scanline_intensity: 0.1 * fade,
                    });
                    if overlay.effect_frames_remaining == 0 {
                        overlay.effect_config = EffectConfig::default();
                    }
                }

                overlay.post_processor.queue_texture_render(
                    &overlay.srv,
                    &overlay.effect_config,
                    overlay.x,
                    overlay.y,
                    overlay.width,
                    overlay.height,
                );
                overlay.post_processor.draw(&frame_params);
                rendered_any = true;
            } else if !overlay.secondary_view_ids().is_empty() {
                overlay.tick(&context);
            }
        }

        if rendered_any && !OVERLAY_SYSTEM_READY.load(Ordering::Acquire) {
            OVERLAY_SYSTEM_READY.store(true, Ordering::Release);
            for (id, overlay) in manager.active_instances.iter() {
                info!(
                    "[OverlayManager] Overlay '{}' using renderer: {:?}",
                    id, overlay.renderer_type
                );
            }
            manager.broadcast_platform_message("overlay/system_ready", b"true");
        }
    }

    /// Ticks all overlays to update their texture content for the current frame.
    ///
    /// # What it solves
    /// This function drives all Flutter animations and state updates. It processes
    /// scheduled tasks and renders a new frame into each overlay's texture if needed.
    /// This should be called once per frame *before* any compositing. For advanced
    /// render pipelines, this gives you a chance to work with the updated texture
    /// before it's drawn to the screen.
    ///
    /// # Example
    /// ```rust, no_run
    /// // In your main game loop
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.tick_overlays();
    /// // ... do other game logic or rendering ...
    /// manager.composite_overlays();
    /// ```
    pub fn tick_overlays(&self) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Some(context) = manager.shared_d3d_context.clone() {
            for overlay in manager.active_instances.values_mut() {
                if overlay.is_visible() && overlay.has_first_frame() {
                    overlay.reopen_shared_texture_if_needed(&context);
                    overlay.tick(&context);
                }
                for view_id in overlay.secondary_view_ids() {
                    overlay.tick_view(view_id, &context);
                }
                let _ = overlay.request_frame();
            }
        }
    }

    /// Composites (draws) all visible overlays onto the screen in their specified Z-order.
    ///
    /// # What it solves
    /// This function handles the final drawing of the user interfaces. It should be
    /// called once per frame after `tick_overlays` and after your main 3D scene has
    /// been rendered, to ensure the UI appears on top.
    ///
    /// # Example
    /// ```rust, no_run
    /// // In your main game loop
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.tick_overlays();
    /// render_my_3d_world();
    /// manager.composite_overlays(); // Draws the UI on top of the world
    /// ```
    pub fn composite_overlays(&self, view_projection_matrix: &XMMatrix) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Some(context) = manager.shared_d3d_context.clone() {
            let time = if manager.is_paused {
                manager.time_at_pause
            } else {
                manager.start_time.elapsed().as_secs_f32()
            };

            let frame_params = FrameParams {
                context: &context,
                view_projection_matrix,
                depth_stencil_view: &None,
                screen_width: manager.screen_width as f32,
                screen_height: manager.screen_height as f32,
                time,
            };

            for id in manager.overlay_order.clone() {
                if let Some(overlay) = manager.active_instances.get_mut(&id)
                    && overlay.is_visible()
                {
                    update_interactive_widget_hover_state(overlay);

                    // Draw 3D primitives and text
                    overlay.primitive_renderer.draw(&frame_params);
                    overlay.text_renderer.draw(&frame_params);

                    // Queue and draw the 2D Flutter UI
                    overlay.post_processor.queue_texture_render(
                        &overlay.srv,
                        &overlay.effect_config,
                        overlay.x,
                        overlay.y,
                        overlay.width,
                        overlay.height,
                    );
                    overlay.post_processor.draw(&frame_params);
                }
            }
        }
    }

    /// Updates the screen dimensions used by the overlays.
    /// # Example
    /// ```rust, no_run
    /// // In your WndProc, when you receive a WM_SIZE message
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.update_screen_size(new_width, new_height);
    /// ```
    pub fn update_screen_size(&self, width: u32, height: u32) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.screen_width = width;
            manager.screen_height = height;
        }
    }

    /// Pauses all shader animations for all overlays.
    ///
    /// Freezes the `time` uniform sent to any custom shaders, effectively pausing
    /// time-based visual effects. This does not pause the Flutter UI's internal animations.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // When the game is paused:
    /// manager.pause_animations();
    /// ```
    pub fn pause_animations(&self) {
        if let Some(mut manager) = self.manager.try_lock()
            && !manager.is_paused
        {
            manager.time_at_pause = manager.start_time.elapsed().as_secs_f32();
            manager.is_paused = true;
        }
    }

    /// Sets 3D primitives for a specific group in an overlay.
    ///
    /// # Arguments
    /// * `identifier`: The unique name of the target overlay. `None` targets the single active overlay.
    /// * `group_id`: A string slice that identifies this group of primitives.
    /// * `vertices`: A slice of `Vertex3D` points that define the geometry.
    /// * `topology`: A `PrimitiveType` enum that specifies how the vertices should be connected.
    pub fn set_primitives(
        &self,
        identifier: Option<&str>,
        group_id: &str,
        vertices: &[Vertex3D],
        topology: PrimitiveType,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_primitives(group_id, vertices, topology);
        }
    }

    /// Sets 3D primitives with rendering options.
    ///
    /// # Arguments
    /// * `identifier`: The unique name of the target overlay. `None` targets the single active overlay.
    /// * `group_id`: A string slice that identifies this group of primitives.
    /// * `vertices`: A slice of `Vertex3D` points that define the geometry.
    /// * `topology`: A `PrimitiveType` enum that specifies how the vertices should be connected.
    /// * `options`: Rendering options like depth stencil, blend mode, etc.
    pub fn set_primitives_ex(
        &self,
        identifier: Option<&str>,
        group_id: &str,
        vertices: &[Vertex3D],
        topology: PrimitiveType,
        options: PrimitiveOptions,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_primitives_ex(group_id, vertices, topology, options);
        }
    }

    /// Clears all submitted 3D primitives from all groups and all active overlays.
    ///
    /// # Example
    /// ```rust,no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.clear_all_primitives();
    /// ```
    pub fn clear_all_primitives(&self) {
        if let Some(mut manager) = self.manager.try_lock() {
            for overlay in manager.active_instances.values_mut() {
                overlay.clear_all_queued_primitives();
            }
        }
    }

    /// Clears primitives from a specific group for a specific overlay.
    ///
    /// # Arguments
    /// * `identifier`: The unique name of the target overlay. `None` targets the single active overlay.
    /// * `group_id`: The ID of the group to clear.
    pub fn clear_primitives(&self, identifier: Option<&str>, group_id: &str) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.clear_primitives(group_id);
        }
    }

    /// Takes a snapshot of all submitted 3D primitive data, making it ready for rendering.
    ///
    /// This function is the core of the "Submit & Latch" system, which resolves rendering flickers and
    /// disappearing primitives. The problem occurs because the game's update logic (which submits new data)
    /// and the render hook (`new_present`, which draws the data) can run at different rates.
    ///
    /// By calling this function once at the beginning of a render frame, we "latch" a stable, complete
    /// copy of the data into a dedicated render buffer. This guarantees that all draw calls within the
    /// same frame use the exact same data, eliminating the race condition that causes  visual artifacts.
    ///
    /// ## Usage
    /// Call this function at the very beginning of your `new_present` render hook.
    ///
    /// ```rust
    /// // in dxgi_present_hook.rs
    /// pub(crate) extern "system" fn new_present(...) -> HRESULT {
    ///     // Latch the data at the start of the frame.
    ///     if let Some(om) = get_flutter_overlay_manager_handle() {
    ///         om.latch_all_queued_primitives();
    ///     }
    ///
    ///     // ... rest of the render hook ...
    /// }
    /// ```
    pub fn latch_all_queued_primitives(&self) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.latch_all_queued_primitives();
        }
    }

    /// Latches all queued 3D text for all visible overlays.
    ///
    /// Call this once per frame, typically alongside `latch_all_queued_primitives()`,
    /// to prepare text geometry for rendering.
    ///
    /// # Example
    /// ```rust, no_run
    /// // in dxgi_present_hook.rs
    /// pub(crate) extern "system" fn new_present(...) -> HRESULT {
    ///     if let Some(om) = get_flutter_overlay_manager_handle() {
    ///         om.latch_all_queued_primitives();
    ///         om.latch_all_queued_text();
    ///     }
    ///     // ... rest of the render hook ...
    /// }
    /// ```
    pub fn latch_all_queued_text(&self) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.latch_all_queued_text();
        }
    }

    /// Registers a font atlas for 3D text rendering on a specific overlay.
    ///
    /// A font atlas is a texture containing all the glyphs for a font, along with
    /// metadata about each glyph's position, size, and spacing. Must be called before
    /// any `set_text` calls that use this font.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `spec` - The font atlas to register ([`FontAtlasSpec`]).
    pub fn register_font_atlas(&self, identifier: Option<&str>, spec: FontAtlasSpec) {
        let FontAtlasSpec {
            font_id,
            texture,
            sampler,
            glyphs,
            line_height,
            base_font_size,
        } = spec;
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.register_font_atlas(
                &font_id,
                texture,
                sampler,
                glyphs,
                line_height,
                base_font_size,
            );
        }
    }

    /// Unregisters a font atlas and clears all text using it.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `font_id` - The font identifier to unregister.
    pub fn unregister_font_atlas(&self, identifier: Option<&str>, font_id: &str) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.unregister_font_atlas(font_id);
        }
    }

    /// Sets pre-built 3D text vertices for rendering.
    ///
    /// Use the `text_presets::generate_text_vertices` helper to create the vertices
    /// from a text string and font atlas.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `font_id` - The font atlas to use (must be registered first)
    /// * `group_id` - Unique identifier for this text group (for updates/removal)
    /// * `vertices` - Pre-built text vertices (from `generate_text_vertices`)
    /// * `options` - Rendering options (depth, blend mode, etc.)
    ///
    /// # Example
    /// ```rust, no_run
    /// use flutter_embedder::software_renderer::d3d11_compositor::text_presets;
    /// use flutter_embedder::software_renderer::d3d11_compositor::primitive_3d_renderer::PrimitiveOptions;
    ///
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Generate vertices using the text_presets module (requires font atlas reference)
    /// manager.set_text(None, "my_font", "label_1", &vertices, PrimitiveOptions::default());
    /// ```
    pub fn set_text(
        &self,
        identifier: Option<&str>,
        font_id: &str,
        group_id: &str,
        vertices: &[TexturedVertex3D],
        options: PrimitiveOptions,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_text(font_id, group_id, vertices, options);
        }
    }

    /// Clears text for a specific group.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `font_id` - The font the text was registered with.
    /// * `group_id` - The group identifier to clear.
    pub fn clear_text(&self, identifier: Option<&str>, font_id: &str, group_id: &str) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.clear_text(font_id, group_id);
        }
    }

    /// Clears all text for a specific font on an overlay.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `font_id` - The font identifier whose text should be cleared.
    pub fn clear_font_text(&self, identifier: Option<&str>, font_id: &str) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.clear_font_text(font_id);
        }
    }

    /// Clears all text from all fonts on all overlays.
    pub fn clear_all_text(&self) {
        if let Some(mut manager) = self.manager.try_lock() {
            for overlay in manager.active_instances.values_mut() {
                overlay.clear_all_text();
            }
        }
    }

    /// Returns a reference to a registered font atlas, if it exists.
    ///
    /// This is useful for generating text vertices using the `text_presets` helpers.
    /// Note: This acquires the manager lock briefly to clone the font atlas.
    ///
    /// # Arguments
    /// * `identifier` - The target overlay. `None` targets the single active overlay.
    /// * `font_id` - The font identifier to retrieve.
    ///
    /// # Returns
    /// A cloned `FontAtlas` if found, otherwise `None`.
    pub fn get_font_atlas(&self, identifier: Option<&str>, font_id: &str) -> Option<FontAtlas> {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.get_font_atlas(font_id).cloned();
        }
        None
    }

    /// Resumes all shader animations for all overlays.
    ///
    /// Unfreezes the `time` uniform sent to custom shaders, allowing visual effects
    /// to resume from where they left off.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // When the game is unpaused:
    /// manager.resume_animations();
    /// ```
    pub fn resume_animations(&self) {
        if let Some(mut manager) = self.manager.try_lock()
            && manager.is_paused
        {
            manager.start_time =
                Instant::now() - std::time::Duration::from_secs_f32(manager.time_at_pause);
            manager.is_paused = false;
        }
    }

    /// Sets a post-processing effect for the **entire** overlay.
    ///
    /// Applies a full-screen shader effect to an overlay's texture, allowing for
    /// dynamic visual styles like holograms, warp fields, or color grading,
    /// controlled directly from your Rust code.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect` - The `PostEffect` enum variant to apply.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Make the main menu look like a hologram
    /// manager.set_fullscreen_effect(Some("main_menu"), PostEffect::Hologram);
    /// ```
    pub fn set_fullscreen_effect(&self, identifier: Option<&str>, effect: PostEffect) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Ok(overlay) = manager.get_instance_mut(identifier) {
            overlay.effect_config.params = match effect {
                PostEffect::Passthrough => EffectParams::None,
                PostEffect::Hologram => EffectParams::Hologram(HologramParams::default()),
                PostEffect::WarpField => EffectParams::WarpField(WarpFieldParams::default()),
                PostEffect::Glitch => EffectParams::Glitch(HologramParams::default()),
            };
            overlay.effect_config.target = EffectTarget::Fullscreen;
        }
    }

    /// Applies a post-processing effect to a **specific area** of an overlay.
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect` - The `PostEffect` enum variant to apply.
    /// * `bounds` - An array `[x, y, width, height]` defining the target rectangle in logical pixels.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Make a specific button on the HUD glow with a warp effect
    /// let button_bounds = [100.0, 200.0, 150.0, 50.0];
    /// manager.set_widget_effect(Some("main_hud"), PostEffect::WarpField, button_bounds);
    /// ```
    pub fn set_widget_effect(
        &self,
        identifier: Option<&str>,
        effect: PostEffect,
        bounds: [f32; 4],
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.effect_config.params = match effect {
                PostEffect::Passthrough => EffectParams::None,
                PostEffect::Hologram => EffectParams::Hologram(HologramParams::default()),
                PostEffect::WarpField => EffectParams::WarpField(WarpFieldParams::default()),
                PostEffect::Glitch => EffectParams::Glitch(HologramParams::default()),
            };
            overlay.effect_config.target = EffectTarget::Widget(bounds);
        }
    }

    /// Removes any active effect from an overlay, reverting it to the default passthrough shader.
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.clear_effect(Some("main_menu"));
    /// ```
    pub fn set_keep_alive(&self, identifier: Option<&str>, keep_alive: bool) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Ok(overlay) = manager.get_instance_mut(identifier) {
            overlay.keep_alive = keep_alive;
        }
    }

    /// Triggers a frame-based glitch effect that auto-fades and auto-clears.
    /// NOTE: Currently hardcoded to the Glitch shader. Should be refactored
    /// to accept a dynamic EffectParams for any effect type.
    pub fn trigger_glitch_effect(&self, identifier: Option<&str>, frames: u32) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Ok(overlay) = manager.get_instance_mut(identifier) {
            overlay.effect_frames_remaining = frames;
            overlay.effect_total_frames = frames;
            overlay.effect_config.params = EffectParams::Glitch(HologramParams::default());
            overlay.effect_config.target = EffectTarget::Fullscreen;
        }
    }

    pub fn clear_effect(&self, identifier: Option<&str>) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Ok(overlay) = manager.get_instance_mut(identifier) {
            overlay.effect_config = EffectConfig::default();
        }
    }

    /// Updates the entire effect configuration for a specific overlay.
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `config`: The complete `EffectConfig` struct.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let config = EffectConfig {
    ///     target: EffectTarget::Fullscreen,
    ///     params: EffectParams::Hologram(HologramParams { intensity: 0.8, ..Default::default() }),
    /// };
    /// manager.update_effect_config(Some("main_menu"), config);
    /// ```
    pub fn update_effect_config(&self, identifier: Option<&str>, config: EffectConfig) {
        let Some(mut manager) = self.manager.try_lock() else {
            return;
        };
        if let Ok(overlay) = manager.get_instance_mut(identifier) {
            overlay.effect_config = config;
        }
    }

    /// Forwards a raw Windows message to the manager for input processing.
    ///
    /// This is the critical function for making UIs interactive. It translates Windows
    /// input messages (mouse moves, clicks, key presses) into events that Flutter
    /// can understand and deliver to the appropriate widgets. Without this, your
    /// UI will be visible but completely non-interactive.
    ///
    /// # Returns
    /// `true` if a Flutter overlay consumed the event. The host application can
    /// use this to suppress further processing of the input (e.g., stop the game
    /// camera from moving when the mouse is over a UI button).
    ///
    /// # Example
    /// ```rust, no_run
    /// // Inside your application's WndProc
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// if manager.forward_input_to_flutter(hwnd, msg, wparam, lparam) {
    ///     return LRESULT(0); // Flutter handled it, so we stop processing.
    /// }
    /// // ... continue with normal message processing for the game ...
    /// ```
    pub fn forward_input_to_flutter(
        &self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> bool {
        type DeferredCb = (String, bool, VisibilityToggleCallback);
        let (result, deferred_cb): (bool, Option<DeferredCb>) =
            if let Some(mut manager) = self.manager.try_lock() {
                manager.handle_input_event(hwnd, msg, wparam, lparam)
            } else {
                (false, None)
            };
        if let Some((overlay_id, visible, cb)) = deferred_cb {
            cb(&overlay_id, visible);
        }
        result
    }

    /// Requests that the topmost active overlay under the cursor set the mouse cursor style.
    /// Call this from your `WndProc` when handling `WM_SETCURSOR`.
    ///
    /// Allows the Flutter UI to control the appearance of the mouse cursor, for example,
    /// changing it from an arrow to a text-input I-beam when hovering over a text field,
    /// or to a hand pointer over a button. This provides essential visual feedback to the user.
    ///
    /// # Returns
    /// * `Some(LRESULT(1))` if a Flutter overlay handled the cursor request.
    /// * `None` if no overlay handled the request.
    ///
    /// # Example
    /// ```rust, no_run
    /// // In your WndProc
    /// // case WM_SETCURSOR:
    /// if let Some(manager) = get_flutter_overlay_manager_handle() {
    ///     if let Some(result) = manager.set_flutter_cursor(hwnd, lparam, original_hwnd) {
    ///         return result; // Flutter handled the cursor
    ///     }
    /// }
    /// // Default handling...
    /// ```
    pub fn set_flutter_cursor(
        &self,
        hwnd_for_setcursor_message: HWND,
        lparam: LPARAM,
        original_hwnd: HWND,
    ) -> Option<LRESULT> {
        if let Some(manager) = self.manager.try_lock() {
            manager.handle_set_cursor(hwnd_for_setcursor_message, lparam, original_hwnd)
        } else {
            None
        }
    }

    /// Notifies all active overlays of a window or render area resize.
    ///
    /// Informs all Flutter instances about the new size of the window, allowing them
    /// to recalculate layouts and adapt to the new resolution. It also ensures the
    /// underlying GPU textures are resized correctly to prevent stretching or clipping.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.resize_flutter_overlays(&my_swap_chain, 0, 0, new_width, new_height);
    /// ```
    pub fn resize_flutter_overlays(
        &self,
        swap_chain: &IDXGISwapChain,
        x_pos: i32,
        y_pos: i32,
        width: u32,
        height: u32,
    ) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.handle_resize(swap_chain, x_pos, y_pos, width, height);
        }
    }

    /// Shuts down a specific Flutter overlay instance, releasing all its resources.
    /// # Arguments
    /// * `identifier`: The unique identifier of the overlay to shut down.
    pub fn shutdown_instance(&self, identifier: &str) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Err(e) = manager.shutdown_instance(identifier)
        {
            error!("[OverlayManagerHandle] Error during shutdown of instance {identifier}: {e}");
        }
    }

    /// Shuts down all currently active Flutter overlay instances.
    /// # Example
    /// ```rust, no_run
    /// // In your application's exit/cleanup logic:
    /// if let Some(manager) = get_flutter_overlay_manager_handle() {
    ///     manager.shutdown_all_instances();
    /// }
    /// ```
    pub fn shutdown_all_instances(&self) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.shutdown_all_instances();
        }
    }

    /// Brings the specified overlay to the top of the rendering order (Z-order).
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // When a popup is shown
    /// manager.bring_to_front(Some("popup_dialog"));
    /// ```
    pub fn bring_to_front(&self, identifier: Option<&str>) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.bring_to_front(identifier);
        }
    }

    /// Sets keyboard focus to the specified overlay, which also brings it to the front.
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // When the user clicks on the chat input field
    /// manager.set_focus(Some("chat_ui"));
    /// ```
    pub fn set_focus(&self, identifier: Option<&str>) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.set_keyboard_focus(identifier);
        }
    }

    /// Checks if the specified overlay currently has keyboard focus.
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// if manager.is_focused(Some("chat_ui")) {
    ///     // Don't process game movement keys
    /// }
    /// ```
    pub fn is_focused(&self, identifier: Option<&str>) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return manager.focused_overlay_id.as_deref() == Some(overlay.name.as_str());
        }
        false
    }

    pub fn has_rendered_frame(&self) -> bool {
        if let Some(manager) = self.manager.try_lock() {
            return manager
                .active_instances
                .values()
                .any(|o| o.has_first_frame());
        }
        false
    }

    /// Sets the visibility of a Flutter overlay. An invisible overlay is not rendered and does not receive input.
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `is_visible` - Whether the overlay should be visible.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // When the player presses the escape key:
    /// manager.set_visibility(Some("pause_menu"), true);
    /// ```
    pub fn set_visibility(&self, identifier: Option<&str>, is_visible: bool) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_visibility(is_visible);
        }
    }

    /// Registers a keybind that toggles an overlay's visibility.
    ///
    /// Solves the chicken-and-egg problem: when an overlay is hidden, it can't receive
    /// input events — so it can never process the key that would show it again. This
    /// registration moves the toggle logic to the manager level, *before* the visibility
    /// gate, so the key always works regardless of overlay state.
    ///
    /// # Arguments
    /// * `key_name` - A human-readable key name (e.g., `"F1"`, `"Escape"`, `"T"`).
    /// * `overlay_id` - The identifier of the overlay to toggle.
    /// * `callback` - Optional callback `(overlay_id, new_visibility) -> consumed`.
    ///
    /// # Example
    /// ```rust, no_run
    /// use std::sync::Arc;
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // F1 toggles the HUD overlay
    /// manager.register_visibility_toggle("F1", "hud", None);
    /// // F2 toggles debug panel with a callback
    /// manager.register_visibility_toggle("F2", "debug_panel", Some(Arc::new(|id, visible| {
    ///     println!("Debug panel '{}' is now {}", id, if visible { "shown" } else { "hidden" });
    ///     true
    /// })));
    /// ```
    pub fn register_visibility_toggle(
        &self,
        key_name: &str,
        overlay_id: &str,
        callback: Option<VisibilityToggleCallback>,
    ) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.register_visibility_toggle(key_name, overlay_id, callback);
        }
    }

    /// Removes a previously registered visibility toggle keybind.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.unregister_visibility_toggle("F1");
    /// ```
    pub fn unregister_visibility_toggle(&self, key_name: &str) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.unregister_visibility_toggle(key_name);
        }
    }

    /// Registers a generic keybind action. When the key is pressed, the optional Rust
    /// callback fires and a platform message is sent to the overlay on channel
    /// Works even when the overlay is invisible — the callback fires directly on the input thread.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.register_keybind_action("F5", "freeze_time", "main_hud", None, false);
    /// manager.register_keybind_action("F6", "speed_up", "main_hud", None, true);
    /// ```
    pub fn register_keybind_action(
        &self,
        key_name: &str,
        action_id: &str,
        overlay_id: &str,
        callback: Option<KeybindCallback>,
        allow_repeat: bool,
    ) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.register_keybind_action(
                key_name,
                action_id,
                overlay_id,
                callback,
                allow_repeat,
            );
        }
    }

    /// Removes a keybind action by action_id.
    pub fn unregister_keybind_action(&self, action_id: &str) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.unregister_keybind_action(action_id);
        }
    }

    /// Changes the key for an existing keybind action without re-registering.
    pub fn rebind_keybind_action(&self, action_id: &str, new_key_name: &str) {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.rebind_keybind_action(action_id, new_key_name);
        }
    }

    /// Sets the screen-space position of an overlay's top-left corner.
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `x`, `y` - The new screen-space coordinates.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Move the health bar to follow the player
    /// manager.set_position(Some("player_health_bar"), player.x + 10, player.y - 50);
    /// ```
    pub fn set_position(&self, identifier: Option<&str>, x: i32, y: i32) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_position(x, y);
        }
    }

    /// Sends a platform message to all visible overlays.
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Notify all UIs that the game is saving
    /// manager.broadcast_message("game/events", "saving".as_bytes());
    /// ```
    pub fn broadcast_message(&self, channel: &str, message: &[u8]) {
        if let Some(manager) = self.manager.try_lock() {
            manager.broadcast_platform_message(channel, message);
        }
    }

    /// Registers a custom message handler for a specific channel on an overlay.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `channel` - The name of the channel the handler will listen to (e.g., "game/settings").
    /// * `handler` - A closure that processes an incoming `Vec<u8>` and returns a `Vec<u8>` response.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.register_channel_handler(Some("settings_menu"), "settings/setVolume", |payload| {
    ///     if let Some(volume_byte) = payload.get(0) {
    ///         let volume = *volume_byte as f32 / 255.0;
    ///         println!("Game volume set to {}", volume);
    ///     }
    ///     vec![1] // Return a success code as a Vec<u8>
    /// });
    /// ```
    pub fn register_channel_handler<F>(&self, identifier: Option<&str>, channel: &str, handler: F)
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        if let Some(mut manager) = self.manager.try_lock() {
            manager.register_channel_handler_for_instance(identifier, channel, handler);
        }
    }

    /// Gets the dimensions (width, height) of all active overlays.
    ///
    /// Allows the host application to get the size of all UIs, which can be useful
    /// for layout calculations, screen captures, or debugging.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let all_sizes = manager.get_all_dimensions();
    /// for (id, (width, height)) in all_sizes {
    ///     println!("Overlay '{}' is {}x{}", id, width, height);
    /// }
    /// ```
    pub fn get_all_dimensions(&self) -> HashMap<String, (u32, u32)> {
        if let Some(manager) = self.manager.try_lock() {
            manager.get_all_overlay_dimensions()
        } else {
            HashMap::new()
        }
    }

    /// Backbuffer size, lock-free (mirrored atomically at init/resize).
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        let v = self.screen.load(Ordering::Acquire);
        if v == 0 {
            None
        } else {
            Some(((v >> 32) as u32, v as u32))
        }
    }

    /// Last pointer position in screen pixels, or `None` if no sample yet.
    pub fn pointer_pos(&self) -> Option<(f32, f32)> {
        let v = self.pointer.load(Ordering::Acquire);
        if v == 0 {
            None
        } else {
            let x = f32::from_bits((v >> 32) as u32);
            let y = f32::from_bits(v as u32);
            Some((x, y))
        }
    }

    /// Last pointer button bitmask (bit 0 = primary/left button).
    pub fn pointer_buttons(&self) -> i64 {
        self.pointer_buttons.load(Ordering::Acquire)
    }

    /// Gets a clone of the shared Direct3D device context used by the manager.
    ///
    /// Provides direct access to the D3D11 context for advanced, custom rendering
    /// needs that might need to interoperate with the overlay's rendering.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// if let Some(context) = manager.get_d3d_context() {
    ///     // Perform custom D3D11 operations
    /// }
    /// ```
    pub fn get_d3d_context(&self) -> Option<ID3D11DeviceContext> {
        self.manager
            .try_lock()
            .and_then(|m| m.shared_d3d_context.clone())
    }

    /// Finds the identifier of the topmost, visible overlay at a given screen coordinate.
    pub fn find_at_position(&self, x: i32, y: i32) -> Option<String> {
        self.manager
            .try_lock()
            .and_then(|m| m.find_topmost_overlay_at_position(x, y))
    }

    /// Registers a Dart `SendPort` with an overlay for Rust-to-Dart communication.
    ///
    /// Establishes a direct, low-level communication channel for pushing data from Rust
    /// to Dart. This is faster than platform channels and is ideal for
    /// frequent, fire-and-forget data updates that need to be reflected in the UI every frame.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `port` - The native port handle from Dart's `ReceivePort.sendPort.nativePort`.
    ///
    /// # Example
    /// ```rust, no_run
    /// // This function would be exposed via FFI and called from Dart at startup.
    /// #[no_mangle]
    /// pub extern "C" fn register_dart_port(port: i64) {
    ///     if let Some(manager) = get_flutter_overlay_manager_handle() {
    ///         manager.register_dart_port(None, port);
    ///     }
    /// }
    /// ```
    pub fn register_dart_port(&self, identifier: Option<&str>, port: i64) {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            overlay.register_dart_port(port);
        }
    }

    /// Sends a boolean message to a single overlay via its registered `SendPort`.
    ///
    /// A fast path for pushing boolean state to Dart. See `register_dart_port` for the use case.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.post_bool(Some("main_hud"), true); // e.g., show "In Combat" indicator
    /// ```
    pub fn post_bool(&self, identifier: Option<&str>, value: bool) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.post_bool(value).is_ok();
        }
        false
    }

    /// Sends an i64 message to a single overlay via its registered `SendPort`.
    ///
    /// A fast path for pushing integer data to Dart. See `register_dart_port` for the use case.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let current_score = 1500;
    /// manager.post_i64(Some("main_hud"), current_score);
    /// ```
    pub fn post_i64(&self, identifier: Option<&str>, value: i64) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.post_i64(value).is_ok();
        }
        false
    }

    /// Sends an f64 message to a single overlay via its registered `SendPort`.
    ///
    /// A fast path for pushing floating-point data to Dart. See `register_dart_port` for the use case.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let time_remaining = 29.5;
    /// manager.post_f64(Some("main_hud"), time_remaining);
    /// ```
    pub fn post_f64(&self, identifier: Option<&str>, value: f64) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.post_f64(value).is_ok();
        }
        false
    }

    /// Sends a string message to a single overlay via its registered `SendPort`.
    ///
    /// A fast path for pushing string data to Dart. See `register_dart_port` for the use case.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Send a quest update to the HUD
    /// manager.post_string(Some("main_hud"), "New quest: Defeat the dragon!");
    /// ```
    pub fn post_string(&self, identifier: Option<&str>, value: &str) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.post_string(value).is_ok();
        }
        false
    }

    /// A fast path for pushing raw binary data to Dart. See `register_dart_port` for the use case.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let minimap_data: Vec<u8> = vec![0, 1, 2, 3];
    /// manager.post_buffer(Some("main_hud"), &minimap_data);
    /// ```
    pub fn post_buffer(&self, identifier: Option<&str>, buffer: &[u8]) -> bool {
        if let Some(manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance(identifier)
        {
            return overlay.post_buffer(buffer).is_ok();
        }
        false
    }

    /// Registers a custom shader effect from compiled byte code.
    ///
    /// This allows for extending the rendering capabilities with custom visual effects for 3D primitives.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect_id` - A unique string to identify this shader effect.
    /// * `vs_bytes` - Optional compiled vertex shader byte code (`.cso` file content). If `None`, uses the default vertex shader.
    ///   Custom vertex shaders can pass additional data to the pixel shader (e.g., world position, normals).
    /// * `ps_bytes` - The compiled pixel shader byte code (`.cso` file content).
    /// * `constant_buffer_size` - Optional size of the constant buffer for this shader.
    /// * `blend_mode` - The blending mode to use when rendering primitives with this effect.
    ///   Use `BlendMode::Transparent` for standard alpha blending or `BlendMode::Opaque` for no blending.
    ///
    /// # Example
    /// ```rust, no_run
    /// use crate::software_renderer::d3d11_compositor::primitive_3d_renderer::BlendMode;
    ///
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // The shaders can be compiled offline using `fxc.exe`
    /// let vs_bytes = include_bytes!("my_vertex_shader.cso");
    /// let ps_bytes = include_bytes!("my_pixel_shader.cso");
    /// manager.register_custom_pixel_shader(None, "my_custom_effect", Some(vs_bytes), ps_bytes, Some(16), BlendMode::Transparent);
    /// ```
    pub fn register_custom_pixel_shader(
        &self,
        identifier: Option<&str>,
        effect_id: &str,
        vs_bytes: Option<&[u8]>,
        ps_bytes: &[u8],
        constant_buffer_size: Option<u32>,
        blend_mode: BlendMode,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            let device = unsafe { overlay.srv.GetDevice().unwrap() };
            overlay.register_custom_pixel_shader(
                &device,
                effect_id,
                vs_bytes,
                ps_bytes,
                constant_buffer_size,
                blend_mode,
            );
        }
    }

    /// Sets a texture at a specific shader resource slot for a custom effect.
    /// This allows binding textures to non-sequential slots, enabling optional textures
    /// like normal maps, specular maps, etc.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect_id` - The ID of the custom effect to which these resources will be bound.
    /// * `slot` - The shader resource slot index (corresponds to `tN` in HLSL where N = slot).
    /// * `texture` - The `ID3D11ShaderResourceView` handle for the texture.
    /// * `sampler` - The `ID3D11SamplerState` handle for the sampler.
    ///
    /// # Example
    /// ```rust, no_run
    /// use windows::Win32::Graphics::Direct3D11::{ID3D11ShaderResourceView, ID3D11SamplerState};
    ///
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// // Base texture at slot 0
    /// manager.set_custom_effect_texture_at_slot(None, "my_effect", 0, base_texture, sampler);
    /// // Optional normal map at slot 1
    /// manager.set_custom_effect_texture_at_slot(None, "my_effect", 1, normal_map, sampler);
    /// ```
    pub fn set_custom_effect_texture_at_slot(
        &self,
        identifier: Option<&str>,
        effect_id: &str,
        slot: u32,
        texture: ID3D11ShaderResourceView,
        sampler: ID3D11SamplerState,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_custom_effect_texture_at_slot(effect_id, slot, texture, sampler);
        }
    }

    /// Clears a texture from a specific slot for a custom effect.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect_id` - The ID of the custom effect.
    /// * `slot` - The shader resource slot index to clear.
    pub fn clear_custom_effect_texture_at_slot(
        &self,
        identifier: Option<&str>,
        effect_id: &str,
        slot: u32,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.clear_custom_effect_texture_at_slot(effect_id, slot);
        }
    }

    /// Convenience method to set multiple textures at once with explicit slot assignments.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect_id` - The ID of the custom effect.
    /// * `textures` - A `Vec` of `(slot, texture, sampler)` tuples.
    ///
    /// # Example
    /// ```rust, no_run
    /// use windows::Win32::Graphics::Direct3D11::{ID3D11ShaderResourceView, ID3D11SamplerState};
    ///
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// manager.set_custom_effect_textures_bulk(None, "my_effect", vec![
    ///     (0, base_texture, sampler),     // t0: base color
    ///     (1, normal_map, sampler),       // t1: normal map
    ///     (2, roughness_map, sampler),    // t2: roughness
    /// ]);
    /// ```
    pub fn set_custom_effect_textures_bulk(
        &self,
        identifier: Option<&str>,
        effect_id: &str,
        textures: Vec<(u32, ID3D11ShaderResourceView, ID3D11SamplerState)>,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_custom_effect_textures_bulk(effect_id, textures);
        }
    }

    /// Updates the constant buffer data for a custom effect.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `effect_id` - The ID of the custom effect whose constants are being updated.
    /// * `data` - A byte slice containing the new constant buffer data.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    ///
    /// #[repr(C)]
    /// struct MyConstants {
    ///     time: f32,
    ///     intensity: f32,
    /// }
    ///
    /// let my_constants = MyConstants { time: 1.0, intensity: 0.8 };
    ///
    /// let data = unsafe {
    ///     std::slice::from_raw_parts(
    ///         &my_constants as *const _ as *const u8,
    ///         std::mem::size_of::<MyConstants>(),
    ///     )
    /// };
    ///
    /// manager.update_custom_effect_constants(None, "my_custom_effect", data);
    /// ```
    pub fn update_custom_effect_constants(
        &self,
        identifier: Option<&str>,
        effect_id: &str,
        data: &[u8],
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.update_custom_effect_constants(effect_id, data);
        }
    }

    /// Replaces the primitives in a group and applies a custom shader effect to them.
    ///
    /// # Arguments
    /// * `identifier` - The unique identifier of the overlay. If `None`, targets the single active overlay.
    /// * `group_id` - The ID of the primitive group to replace.
    /// * `triangles` - A slice of `Vertex3D` for the triangle list.
    /// * `lines` - A slice of `Vertex3D` for the line list.
    /// * `effect_id` - The ID of the custom effect to apply to these primitives.
    ///
    /// # Example
    /// ```rust, no_run
    /// use crate::software_renderer::d3d11_compositor::primitive_3d_renderer::Vertex3D;
    ///
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    ///
    /// let triangle = vec![
    ///     Vertex3D { pos: [0.0, 0.5, 0.0], color: [1.0, 0.0, 0.0, 1.0] },
    ///     Vertex3D { pos: [0.5, -0.5, 0.0], color: [0.0, 1.0, 0.0, 1.0] },
    ///     Vertex3D { pos: [-0.5, -0.5, 0.0], color: [0.0, 0.0, 1.0, 1.0] },
    /// ];
    ///
    /// manager.set_custom_primitives(
    ///     None,
    ///     "my_group",
    ///     &triangle,
    ///     &[],
    ///     "my_custom_effect",
    /// );
    /// ```
    pub fn set_custom_primitives(
        &self,
        identifier: Option<&str>,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_custom_primitives(group_id, triangles, lines, effect_id);
        }
    }

    /// Sets custom primitives with rendering options.
    pub fn set_custom_primitives_ex(
        &self,
        identifier: Option<&str>,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
        options: PrimitiveOptions,
    ) {
        if let Some(mut manager) = self.manager.try_lock()
            && let Ok(overlay) = manager.get_instance_mut(identifier)
        {
            overlay.set_custom_primitives_ex(group_id, triangles, lines, effect_id, options);
        }
    }

    /// Retrieves the rendered textures from all active and visible overlays.
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// let textures = manager.get_all_overlay_textures();
    /// for (id, texture_srv) in textures {
    ///     // Use texture_srv to draw the UI in a custom way
    /// }
    /// ```
    pub fn get_all_overlay_textures(&self) -> Vec<(String, ID3D11ShaderResourceView)> {
        if let Some(manager) = self.manager.try_lock() {
            manager.get_all_overlay_textures()
        } else {
            Vec::new()
        }
    }

    /// Checks if any overlay has experienced a device lost condition.
    ///
    /// A device lost condition occurs when the D3D11 device is removed (e.g., driver crash,
    /// driver update, GPU reset). When this returns true, rendering will be disabled until
    /// recovery is attempted.
    ///
    /// # Returns
    /// `true` if any overlay has a lost device.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// if manager.is_any_device_lost() {
    ///     // Attempt recovery or notify user
    ///     manager.attempt_device_recovery(&swap_chain);
    /// }
    /// ```
    pub fn is_any_device_lost(&self) -> bool {
        if let Some(manager) = self.manager.try_lock() {
            for overlay in manager.active_instances.values() {
                if overlay.is_device_lost() {
                    return true;
                }
            }
        }
        false
    }

    /// Checks if device recovery should be attempted now, considering the cooldown.
    /// Returns true if any device is lost AND we're not in a cooldown period.
    /// Also decrements the cooldown counter each call.
    pub fn should_attempt_recovery(&self) -> bool {
        let Some(mut manager) = self.manager.try_lock() else {
            return false;
        };

        if manager.recovery_cooldown > 0 {
            manager.recovery_cooldown -= 1;
            return false;
        }

        for overlay in manager.active_instances.values() {
            if overlay.is_device_lost() {
                return true;
            }
        }
        false
    }

    /// Attempts to recover all overlays from a device lost condition.
    ///
    /// This should be called when `is_any_device_lost()` returns true. It will attempt to
    /// reinitialize the ANGLE contexts and textures for all affected overlays.
    ///
    /// If recovery fails, a cooldown is set to prevent spamming recovery attempts.
    ///
    /// # Arguments
    /// * `swap_chain` - The swap chain to use for recovering device references.
    ///
    /// # Returns
    /// `true` if all overlays recovered successfully.
    ///
    /// # Example
    /// ```rust, no_run
    /// let manager = get_flutter_overlay_manager_handle().unwrap();
    /// if manager.should_attempt_recovery() {
    ///     if manager.attempt_device_recovery(&swap_chain) {
    ///         info!("Flutter overlay device recovery successful");
    ///     } else {
    ///         error!("Flutter overlay device recovery failed");
    ///     }
    /// }
    /// ```
    pub fn attempt_device_recovery(&self, swap_chain: &IDXGISwapChain) -> bool {
        let Some(mut manager) = self.manager.try_lock() else {
            return false;
        };
        let mut all_recovered = true;

        for (id, overlay) in manager.active_instances.iter_mut() {
            if overlay.is_device_lost() {
                info!("[OverlayManager] Attempting device recovery for overlay '{id}'");
                if !overlay.attempt_device_recovery(swap_chain) {
                    error!("[OverlayManager] Device recovery failed for overlay '{id}'");
                    all_recovered = false;
                }
            }
        }

        if !all_recovered {
            manager.recovery_cooldown = 300;
            warn!(
                "[OverlayManager] Device recovery failed, will retry in {} frames",
                manager.recovery_cooldown
            );
        }

        all_recovered
    }
}
