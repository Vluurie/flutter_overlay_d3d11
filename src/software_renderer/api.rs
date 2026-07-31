use crate::bindings::embedder::{
    self as e, FlutterEngine, FlutterEngineDartObject__bindgen_ty_1 as DartObjectUnion,
};
use crate::software_renderer::d3d11_compositor::primitive_3d_renderer::{
    BlendMode, PrimitiveOptions, PrimitiveType, Vertex3D,
};
use crate::software_renderer::d3d11_compositor::text_3d_renderer::{
    FontAtlas, GlyphInfo, TexturedVertex3D,
};
use crate::software_renderer::overlay::d3d::{
    create_compositing_texture, create_srv, create_texture,
};
use crate::software_renderer::overlay::engine::update_flutter_window_metrics;
use crate::software_renderer::overlay::init::{self as internal_embedder_init};

use crate::software_renderer::overlay::input::{handle_pointer_event, handle_set_cursor};
use crate::software_renderer::overlay::keyevents::handle_keyboard_event;
// Re-export so `FlutterOverlay` is reachable as a public type under this module
// (its inherent `impl` and all public methods live in this file). Without this,
// the type is only visible through the private `overlay` module and cannot be
// named or linked from public docs.
pub use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::overlay::platform_message_callback::send_platform_message;
use crate::software_renderer::ticker::spawn::start_task_runner;
use crate::software_renderer::ticker::ticker::tick;
use log::{error, info, warn};
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, ID3D11Device, ID3D11DeviceContext, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::core::Interface;

/// Value parameters for [`FlutterOverlay::create`]. Bundled into a struct so the
/// constructor takes a small, named argument set instead of a long positional
/// list. The D3D device + swap chain are passed separately as borrows.
pub struct OverlayCreateParams {
    /// Unique name for this overlay instance.
    pub name: String,
    /// Screen-space top-left position.
    pub x: i32,
    pub y: i32,
    /// Initial size in pixels.
    pub width: u32,
    pub height: u32,
    /// Directory containing the Flutter application's assets.
    pub flutter_data_dir: PathBuf,
    /// Optional Dart VM entrypoint arguments.
    pub dart_entrypoint_args: Option<Vec<String>>,
    /// Optional engine command-line arguments.
    pub engine_args: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RendererType {
    Software,
    OpenGL,
}
#[derive(Debug)]
pub enum FlutterEmbedderError {
    InitializationFailed(String),
    OperationFailed(String),
    EngineNotRunning,
    InvalidHandle,
}

impl std::fmt::Display for FlutterEmbedderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlutterEmbedderError::InitializationFailed(s) => {
                write!(f, "Flutter Initialization Failed: {s}")
            }
            FlutterEmbedderError::OperationFailed(s) => {
                write!(f, "Flutter Operation Failed: {s}")
            }
            FlutterEmbedderError::EngineNotRunning => {
                write!(f, "Flutter engine is not running or handle is null.")
            }
            FlutterEmbedderError::InvalidHandle => {
                write!(f, "Invalid Flutter overlay handle provided.")
            }
        }
    }
}
impl std::error::Error for FlutterEmbedderError {}

/// True when a resize request is a no-op: not forced and the new geometry
/// `(x, y, w, h)` equals the current geometry.
pub(crate) fn should_skip_resize(
    current: (i32, i32, u32, u32),
    new: (i32, i32, u32, u32),
    force: bool,
) -> bool {
    !force && current == new
}

impl FlutterOverlay {
    /// Creates and initializes a new `FlutterOverlay` instance.
    ///
    /// This is the primary constructor for creating and setting up a Flutter overlay.
    /// It handles loading necessary DLLs, Flutter assets, initializing the Flutter engine,
    /// and preparing rendering resources.
    ///
    /// # Arguments
    /// * `name`: A unique name for this overlay instance.
    /// * `d3d11_device`: A reference to the host application's D3D11 device.
    /// * `swap_chain`: A reference to the DXGI swap chain.
    /// * `initial_width`: The initial width for the Flutter view.
    /// * `initial_height`: The initial height for the Flutter view.
    /// * `flutter_data_dir`: Path to the directory containing Flutter application's assets.
    /// * `dart_entrypoint_args`: Optional vector of strings for Dart VM entrypoint arguments.
    ///
    /// # Returns
    /// A `Result` containing a `Box<FlutterOverlay>` or a `FlutterEmbedderError`.
    pub fn create(
        params: OverlayCreateParams,
        d3d11_device: &ID3D11Device,
        swap_chain: &IDXGISwapChain,
    ) -> Result<Box<Self>, FlutterEmbedderError> {
        info!(
            "[FlutterOverlay::create] Initializing Flutter Overlay '{}'. Data dir: {:?}",
            params.name, params.flutter_data_dir
        );

        let overlay_box = internal_embedder_init::init_overlay(params, d3d11_device, swap_chain);

        match overlay_box {
            Some(ob) if !ob.engine.0.is_null() => Ok(ob),
            _ => {
                error!(
                    "[FlutterOverlay::create] Initialization failed: Engine handle is null after init."
                );
                Err(FlutterEmbedderError::InitializationFailed(
                    "Engine handle was null after internal init.".to_string(),
                ))
            }
        }
    }

    /// Returns the raw `FlutterEngine` pointer. **USE WITH CAUTION.**
    /// Prefer using methods on `FlutterOverlay` for interaction.
    pub fn get_engine_ptr(&self) -> FlutterEngine {
        self.engine.0
    }

    pub fn handle_window_resize(
        &mut self,
        new_x: i32,
        new_y: i32,
        new_width: u32,
        new_height: u32,
        swap_chain: &IDXGISwapChain,
    ) {
        self.handle_window_resize_inner(new_x, new_y, new_width, new_height, swap_chain, false);
    }

    pub fn handle_window_resize_force(
        &mut self,
        new_x: i32,
        new_y: i32,
        new_width: u32,
        new_height: u32,
        swap_chain: &IDXGISwapChain,
    ) {
        self.handle_window_resize_inner(new_x, new_y, new_width, new_height, swap_chain, true);
    }

    fn handle_window_resize_inner(
        &mut self,
        new_x: i32,
        new_y: i32,
        new_width: u32,
        new_height: u32,
        swap_chain: &IDXGISwapChain,
        force: bool,
    ) {
        if should_skip_resize(
            (self.x, self.y, self.width, self.height),
            (new_x, new_y, new_width, new_height),
            force,
        ) {
            return;
        }

        self.width = new_width;
        self.height = new_height;
        self.x = new_x;
        self.y = new_y;

        let game_device = match unsafe { swap_chain.GetDevice::<ID3D11Device>() } {
            Ok(d) => d,
            Err(e) => {
                error!(
                    "[handle_window_resize] Failed to get device from swap chain: {e}"
                );
                return;
            }
        };

        match self.renderer_type {
            RendererType::Software => {
                if let Some(pixel_buffer) = self.pixel_buffer.as_mut() {
                    self.texture = create_texture(&game_device, self.width, self.height);
                    self.srv = create_srv(&game_device, &self.texture);
                    let new_buffer_size = (self.width as usize) * (self.height as usize) * 4;
                    pixel_buffer.resize(new_buffer_size, 0);
                }
            }
            RendererType::OpenGL => {
                if let Some(angle_state) = self.angle_state.as_mut() {
                    angle_state.0.pending_resize = Some((self.width, self.height));

                    self.texture =
                        create_compositing_texture(&game_device, self.width, self.height);
                    self.srv = create_srv(&game_device, &self.texture);

                    // Force full repaint after resize since the FBO dimensions changed.
                    self.full_repaint_needed
                        .store(true, std::sync::atomic::Ordering::Release);
                } else {
                    warn!(
                        "[handle_window_resize] ANGLE state not found for OpenGL renderer during resize."
                    );
                }
            }
        }

        if !self.engine.0.is_null() {
            update_flutter_window_metrics(
                self.engine.0,
                self.x,
                self.y,
                self.width,
                self.height,
                self.engine_dll.clone(),
            );
        }
    }

    /// crate(INTERNAL) Starts the dedicated task runner thread for this overlay instance.
    /// Does nothing if the task runner is already running.
    pub(crate) fn start_task_runner(&mut self) {
        start_task_runner(self);
    }

    /// After a deferred resize, the game-side shared texture needs to be re-opened.
    /// Call this before tick() when the overlay has mutable access.
    pub fn reopen_shared_texture_if_needed(&mut self, context: &ID3D11DeviceContext) {
        if self.angle_shared_texture.is_none()
            && let Some(handle) = &self.d3d11_shared_handle {
                unsafe {
                    let game_device: ID3D11Device = context.GetDevice().unwrap();
                    let mut opt: Option<ID3D11Texture2D> = None;
                    if game_device.OpenSharedResource(handle.0, &mut opt).is_ok()
                        && let Some(tex) = opt {
                            self.game_keyed_mutex = tex.cast().ok();
                            self.angle_shared_texture = Some(tex);
                            // Now safe to drop the old shared texture
                            self.angle_shared_texture_back = None;
                        }
                }
            }
    }

    /// Performs per-frame updates, preparing the GPU texture with the latest Flutter content.
    /// - For `Software` mode, it uploads pixel data from the CPU.
    /// - For `OpenGL` mode, it waits for ANGLE to finish rendering, then copies from the shared texture.
    pub fn tick(&self, context: &ID3D11DeviceContext) {
        if !self.visible || self.width == 0 || self.height == 0 {
            if !self.secondary_view_ids().is_empty() {
                // View 0 is hidden but satellite views still render. The engine's
                // raster thread blocks until view 0's keyed mutex is handed back to
                // key 0 (present_implicit_view releases it to key 1 each frame). If
                // we skip that handshake the whole engine — including every
                // satellite view — stalls. So still acknowledge view 0's frame:
                // acquire key 1, release key 0, without copying to the (hidden)
                // view-0 texture. Then schedule the next frame.
                let presented = self
                    .angle_frame_presented
                    .load(std::sync::atomic::Ordering::Acquire);
                let copied = self
                    .angle_frame_copied
                    .load(std::sync::atomic::Ordering::Relaxed);
                if presented > copied {
                    if let Some(mutex) = &self.game_keyed_mutex {
                        unsafe {
                            let _ = mutex.AcquireSync(1, u32::MAX);
                            let _ = mutex.ReleaseSync(0);
                        }
                    }
                    self.angle_frame_copied
                        .store(presented, std::sync::atomic::Ordering::Relaxed);
                }
                let _ = self.request_frame();
            }
            return;
        }

        match self.renderer_type {
            RendererType::Software => {
                tick(self, context);
            }
            RendererType::OpenGL => {
                if let Some(angle_state) = &self.angle_state
                    && angle_state.0.is_device_lost() {
                        return;
                    }

                if let Some(angle_texture) = &self.angle_shared_texture {
                    let presented = self
                        .angle_frame_presented
                        .load(std::sync::atomic::Ordering::Acquire);
                    let copied = self
                        .angle_frame_copied
                        .load(std::sync::atomic::Ordering::Relaxed);

                    if presented > copied {
                        let damage: Vec<_> = self
                            .frame_damage_rects
                            .lock()
                            .map(|mut r| r.drain(..).collect())
                            .unwrap_or_default();

                        // No damage → nothing changed, reuse previous texture as-is.
                        if damage.is_empty() {
                            if let Some(mutex) = &self.game_keyed_mutex {
                                unsafe {
                                    let _ = mutex.AcquireSync(1, u32::MAX);
                                    let _ = mutex.ReleaseSync(0);
                                }
                            }
                            self.angle_frame_copied.store(presented, Ordering::Relaxed);
                            return;
                        }

                        unsafe {
                            if let Some(mutex) = &self.game_keyed_mutex {
                                let _ = mutex.AcquireSync(1, u32::MAX);
                            }

                            let w = self.width;
                            let h = self.height;

                            for rect in &damage {
                                let left = (rect.left as u32).min(w);
                                let top = (rect.top as u32).min(h);
                                let right = (rect.right as u32).min(w);
                                let bottom = (rect.bottom as u32).min(h);

                                if left >= right || top >= bottom {
                                    continue;
                                }

                                let src_box = D3D11_BOX {
                                    left,
                                    top,
                                    front: 0,
                                    right,
                                    bottom,
                                    back: 1,
                                };
                                context.CopySubresourceRegion(
                                    &self.texture,
                                    0,
                                    left,
                                    top,
                                    0,
                                    angle_texture,
                                    0,
                                    Some(&src_box),
                                );
                            }

                            if let Some(mutex) = &self.game_keyed_mutex {
                                let _ = mutex.ReleaseSync(0);
                            }
                        }
                        self.angle_frame_copied.store(presented, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Checks if the ANGLE device has been lost due to D3D11 device removal.
    /// When this returns true, rendering is disabled and recovery may be attempted.
    pub fn is_device_lost(&self) -> bool {
        if let Some(angle_state) = &self.angle_state {
            angle_state.0.is_device_lost()
        } else {
            false
        }
    }

    /// Attempts to recover from a device lost condition by reinitializing ANGLE resources.
    /// This should be called when is_device_lost() returns true and the application
    /// wants to attempt to restore rendering capability.
    pub fn attempt_device_recovery(&mut self, swap_chain: &IDXGISwapChain) -> bool {
        if let Some(angle_state) = &mut self.angle_state {
            if !angle_state.0.is_device_lost() {
                return true;
            }

            info!(
                "[FlutterOverlay:'{}'] Attempting device recovery...",
                self.name
            );

            if let Err(e) = angle_state.0.full_reinitialize() {
                error!(
                    "[FlutterOverlay:'{}'] Failed to reinitialize ANGLE: {}",
                    self.name, e
                );
                return false;
            }

            self.game_keyed_mutex.take();
            self.angle_keyed_mutex.take();

            let counter = self
                .angle_frame_presented
                .load(std::sync::atomic::Ordering::Relaxed);
            self.angle_frame_copied
                .store(counter, std::sync::atomic::Ordering::Relaxed);

            match angle_state.0.recreate_resources(self.width, self.height) {
                Ok((new_angle_texture, new_shared_handle)) => {
                    let game_device = match unsafe { swap_chain.GetDevice::<ID3D11Device>() } {
                        Ok(d) => d,
                        Err(e) => {
                            error!(
                                "[FlutterOverlay:'{}'] Failed to get device from swap chain during recovery: {}",
                                self.name, e
                            );
                            return false;
                        }
                    };

                    let angle_texture_on_game_device: ID3D11Texture2D = unsafe {
                        let mut opened_resource_option: Option<ID3D11Texture2D> = None;
                        if let Err(e) = game_device
                            .OpenSharedResource(new_shared_handle, &mut opened_resource_option)
                        {
                            error!(
                                "[FlutterOverlay:'{}'] Failed to open shared resource during recovery: {}",
                                self.name, e
                            );
                            return false;
                        }
                        match opened_resource_option {
                            Some(tex) => tex,
                            None => {
                                error!(
                                    "[FlutterOverlay:'{}'] Opened shared resource was null during recovery",
                                    self.name
                                );
                                return false;
                            }
                        }
                    };

                    let local_compositing_texture =
                        create_compositing_texture(&game_device, self.width, self.height);

                    use crate::software_renderer::overlay::overlay_impl::SendableHandle;

                    self.texture = local_compositing_texture;
                    self.srv = create_srv(&game_device, &self.texture);
                    // Recreate keyed mutexes before moving textures.
                    // New mutex defaults to "released with key 0".
                    self.angle_keyed_mutex = new_angle_texture.cast().ok();
                    self.game_keyed_mutex = angle_texture_on_game_device.cast().ok();

                    self.gl_internal_linear_texture = Some(new_angle_texture);
                    self.d3d11_shared_handle = Some(SendableHandle(new_shared_handle));
                    self.angle_shared_texture = Some(angle_texture_on_game_device);

                    // Force full repaint after device recovery since all FBO content is lost.
                    self.full_repaint_needed.store(true, Ordering::Release);

                    info!(
                        "[FlutterOverlay:'{}'] Device recovery successful!",
                        self.name
                    );
                    return true;
                }
                Err(e) => {
                    error!(
                        "[FlutterOverlay:'{}'] Failed to recreate ANGLE resources during recovery: {}",
                        self.name, e
                    );
                    return false;
                }
            }
        }

        true
    }

    pub fn clear_all_queued_primitives(&mut self) {
        self.primitive_renderer.clear_all_primitives();
    }

    pub fn set_primitives(
        &mut self,
        group_id: &str,
        vertices: &[Vertex3D],
        topology: PrimitiveType,
    ) {
        match topology {
            PrimitiveType::Triangles => {
                self.primitive_renderer
                    .set_primitives(group_id, vertices, &[]);
            }
            PrimitiveType::Lines => {
                self.primitive_renderer
                    .set_primitives(group_id, &[], vertices);
            }
        }
    }

    pub fn set_primitives_ex(
        &mut self,
        group_id: &str,
        vertices: &[Vertex3D],
        topology: PrimitiveType,
        options: PrimitiveOptions,
    ) {
        match topology {
            PrimitiveType::Triangles => {
                self.primitive_renderer
                    .set_primitives_ex(group_id, vertices, &[], options);
            }
            PrimitiveType::Lines => {
                self.primitive_renderer
                    .set_primitives_ex(group_id, &[], vertices, options);
            }
        }
    }

    pub fn clear_primitives(&mut self, group_id: &str) {
        self.primitive_renderer.clear_primitives(group_id);
    }

    pub fn latch_queued_primitives(&mut self) {
        self.primitive_renderer.latch_buffers();
    }

    /// Registers a new custom shader effect from compiled byte code.
    /// This shader can then be used to render primitives by referencing its `effect_id`.
    ///
    /// # Arguments
    /// * `device`: The D3D11 device to use for creating shader resources.
    /// * `effect_id`: A unique string identifier for this effect.
    /// * `vs_bytes`: Optional compiled vertex shader byte code (CSO). If `None`, uses the default vertex shader.
    ///   Custom vertex shaders can pass additional data to the pixel shader (e.g., world position, normals).
    /// * `ps_bytes`: The compiled pixel shader byte code (CSO).
    /// * `constant_buffer_size`: If the shader uses a constant buffer (at register `b2`),
    ///   specify its size in bytes. This buffer can be updated per-frame using
    ///   `update_custom_effect_constants`.
    /// * `blend_mode`: The blending mode to use when rendering primitives with this effect.
    ///   Use `BlendMode::Transparent` for standard alpha blending or `BlendMode::Opaque` for no blending.
    pub fn register_custom_pixel_shader(
        &mut self,
        device: &ID3D11Device,
        effect_id: &str,
        vs_bytes: Option<&[u8]>,
        ps_bytes: &[u8],
        constant_buffer_size: Option<u32>,
        blend_mode: BlendMode,
    ) {
        self.primitive_renderer.register_custom_pixel_shader(
            device,
            effect_id,
            vs_bytes,
            ps_bytes,
            constant_buffer_size,
            blend_mode,
        );
    }

    /// Sets a texture at a specific shader resource slot for a custom effect.
    /// This allows binding textures to non-sequential slots, enabling optional textures
    /// like normal maps, specular maps, etc.
    ///
    /// # Arguments
    /// * `effect_id`: The identifier of the custom effect to modify.
    /// * `slot`: The shader resource slot index (corresponds to `tN` in HLSL where N = slot).
    /// * `texture`: The `ID3D11ShaderResourceView` handle for the texture.
    /// * `sampler`: The `ID3D11SamplerState` handle for the sampler.
    ///
    /// # Example
    /// ```rust
    /// // Base texture at slot 0
    /// overlay.set_custom_effect_texture_at_slot("my_effect", 0, base_texture, sampler);
    /// // Optional normal map at slot 1
    /// overlay.set_custom_effect_texture_at_slot("my_effect", 1, normal_map, sampler);
    /// // Optional roughness map at slot 2
    /// overlay.set_custom_effect_texture_at_slot("my_effect", 2, roughness_map, sampler);
    /// ```
    pub fn set_custom_effect_texture_at_slot(
        &mut self,
        effect_id: &str,
        slot: u32,
        texture: ID3D11ShaderResourceView,
        sampler: ID3D11SamplerState,
    ) {
        self.primitive_renderer
            .set_custom_effect_texture_at_slot(effect_id, slot, texture, sampler);
    }

    /// Clears a texture from a specific slot for a custom effect.
    /// Use this to remove optional textures that are no longer needed.
    ///
    /// # Arguments
    /// * `effect_id`: The identifier of the custom effect.
    /// * `slot`: The shader resource slot index to clear.
    pub fn clear_custom_effect_texture_at_slot(&mut self, effect_id: &str, slot: u32) {
        self.primitive_renderer
            .clear_custom_effect_texture_at_slot(effect_id, slot);
    }

    /// Convenience method to set multiple textures at once with explicit slot assignments.
    ///
    /// # Arguments
    /// * `effect_id`: The identifier of the custom effect.
    /// * `textures`: A `Vec` of `(slot, texture, sampler)` tuples.
    ///
    /// # Example
    /// ```rust
    /// overlay.set_custom_effect_textures_bulk("my_effect", vec![
    ///     (0, base_texture, sampler),     // t0: base color
    ///     (1, normal_map, sampler),       // t1: normal map
    ///     (2, roughness_map, sampler),    // t2: roughness
    /// ]);
    /// ```
    pub fn set_custom_effect_textures_bulk(
        &mut self,
        effect_id: &str,
        textures: Vec<(u32, ID3D11ShaderResourceView, ID3D11SamplerState)>,
    ) {
        self.primitive_renderer
            .set_custom_effect_textures_bulk(effect_id, textures);
    }

    /// Updates the constant buffer data for a custom effect.
    ///
    /// The data is uploaded to the GPU just before rendering the primitives that use this effect.
    ///
    /// # Arguments
    /// * `effect_id`: The identifier of the custom effect whose constant buffer is to be updated.
    /// * `data`: A byte slice `&[u8]` containing the new data for the constant buffer. The slice's
    ///   length should match the size specified during `register_custom_pixel_shader`.
    pub fn update_custom_effect_constants(&mut self, effect_id: &str, data: &[u8]) {
        self.primitive_renderer
            .update_custom_effect_constants(effect_id, data);
    }

    /// Queues a set of custom primitives (triangles and lines) to be rendered with a specific
    /// user-defined effect. Primitives are grouped by `group_id` and will replace any
    /// existing primitives in that group.
    ///
    /// # Arguments
    /// * `group_id`: A string identifier for this group of primitives.
    /// * `triangles`: A slice of `Vertex3D` defining the triangles for this group.
    /// * `lines`: A slice of `Vertex3D` defining the lines for this group.
    /// * `effect_id`: The identifier of the custom effect (registered via
    ///   `register_custom_pixel_shader`) to use for rendering these primitives.
    pub fn set_custom_primitives(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
    ) {
        self.primitive_renderer
            .set_custom_primitives(group_id, triangles, lines, effect_id);
    }

    pub fn set_custom_primitives_ex(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
        options: PrimitiveOptions,
    ) {
        self.primitive_renderer
            .set_custom_primitives_ex(group_id, triangles, lines, effect_id, options);
    }

    /// Registers a font atlas for 3D text rendering.
    ///
    /// A font atlas is a texture containing all the glyphs for a font, along with
    /// metadata about each glyph's position, size, and spacing.
    ///
    /// # Arguments
    /// * `font_id` - Unique identifier for this font (used in subsequent text calls)
    /// * `texture` - The font atlas texture as a shader resource view
    /// * `sampler` - The sampler state for the texture
    /// * `glyphs` - A HashMap mapping characters to their glyph information
    /// * `line_height` - The height of a line in font units (normalized)
    /// * `base_font_size` - The base font size in pixels (used for scaling)
    ///
    /// # Example
    /// ```rust
    /// use std::collections::HashMap;
    /// use flutter_embedder::software_renderer::d3d11_compositor::text_3d_renderer::GlyphInfo;
    ///
    /// let mut glyphs = HashMap::new();
    /// glyphs.insert('A', GlyphInfo {
    ///     uv_rect: [0.0, 0.0, 0.0625, 0.0625],  // UV coords in atlas
    ///     bearing_x: 0.0,
    ///     bearing_y: 14.0,
    ///     width: 12.0,
    ///     height: 14.0,
    ///     advance: 14.0,
    /// });
    /// // ... add more glyphs
    ///
    /// overlay.register_font_atlas("default", texture_srv, sampler, glyphs, 20.0, 16.0);
    /// ```
    pub fn register_font_atlas(
        &mut self,
        font_id: &str,
        texture: ID3D11ShaderResourceView,
        sampler: ID3D11SamplerState,
        glyphs: std::collections::HashMap<
            char,
            GlyphInfo,
        >,
        line_height: f32,
        base_font_size: f32,
    ) {
        self.text_renderer.register_font_atlas(
            font_id,
            texture,
            sampler,
            glyphs,
            line_height,
            base_font_size,
        );
    }

    /// Unregisters a font atlas and clears all text using it.
    pub fn unregister_font_atlas(&mut self, font_id: &str) {
        self.text_renderer.unregister_font_atlas(font_id);
    }

    /// Returns a reference to a registered font atlas, if it exists.
    /// Useful for generating text vertices using the text_presets helpers.
    pub fn get_font_atlas(
        &self,
        font_id: &str,
    ) -> Option<&FontAtlas> {
        self.text_renderer.get_font_atlas(font_id)
    }

    /// Sets pre-built text vertices for rendering.
    ///
    /// Use the `text_presets::generate_text_vertices` helper to create the vertices
    /// from a text string and font atlas.
    ///
    /// # Arguments
    /// * `font_id` - The font atlas to use (must be registered first)
    /// * `group_id` - Unique identifier for this text group (for updates/removal)
    /// * `vertices` - Pre-built text vertices (from `generate_text_vertices`)
    /// * `options` - Rendering options (depth, blend mode, etc.)
    ///
    /// # Example
    /// ```rust
    /// use flutter_embedder::software_renderer::d3d11_compositor::text_presets;
    /// use flutter_embedder::software_renderer::d3d11_compositor::primitive_3d_renderer::PrimitiveOptions;
    ///
    /// if let Some(font) = overlay.get_font_atlas("default") {
    ///     let vertices = text_presets::generate_text_vertices(
    ///         "Hello World",
    ///         [0.0, 5.0, 10.0],  // 3D position
    ///         font,
    ///         1.0,               // scale
    ///         [1.0, 1.0, 1.0, 1.0], // white color
    ///         [1.0, 0.0, 0.0],   // right direction
    ///         [0.0, 1.0, 0.0],   // up direction
    ///     );
    ///     overlay.set_text("default", "greeting", &vertices, PrimitiveOptions::default());
    /// }
    /// ```
    pub fn set_text(
        &mut self,
        font_id: &str,
        group_id: &str,
        vertices: &[TexturedVertex3D],
        options: PrimitiveOptions,
    ) {
        self.text_renderer
            .set_text(font_id, group_id, vertices, options);
    }

    /// Clears text for a specific group.
    pub fn clear_text(&mut self, font_id: &str, group_id: &str) {
        self.text_renderer.clear_text(font_id, group_id);
    }

    /// Clears all text for a specific font.
    pub fn clear_font_text(&mut self, font_id: &str) {
        self.text_renderer.clear_font_text(font_id);
    }

    /// Clears all text from all fonts.
    pub fn clear_all_text(&mut self) {
        self.text_renderer.clear_all_text();
    }

    /// Latches the current text buffers for rendering.
    /// Must be called before rendering to prepare the text geometry.
    pub fn latch_queued_text(&mut self) {
        self.text_renderer.latch_buffers();
    }

    /// Processes a Windows keyboard message for this overlay.
    /// # Returns
    /// `true` if Flutter handled the event, `false` otherwise.
    pub fn handle_keyboard_event(&self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
        handle_keyboard_event(self, msg, wparam, lparam)
    }

    /// Processes a Windows mouse pointer message for this overlay.
    /// # Returns
    /// `true` if Flutter handled the event, `false` otherwise.
    pub fn handle_pointer_event(
        &self,
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> bool {
        handle_pointer_event(self, hwnd, msg, wparam, lparam)
    }

    /// Handles a `WM_SETCURSOR` Windows message to set the cursor based on Flutter's request.
    /// # Returns
    /// `Some(LRESULT(1))` if Flutter handled the message and set the cursor, `None` otherwise.
    pub fn handle_set_cursor(
        &self,
        hwnd_from_wparam: HWND,
        lparam_from_message: LPARAM,
        main_app_hwnd: HWND,
    ) -> Option<LRESULT> {
        handle_set_cursor(self, hwnd_from_wparam, lparam_from_message, main_app_hwnd)
    }

    /// Notifies the Flutter engine that this overlay instance requests a new frame.
    /// Call this in your main loop to drive Flutter animations and UI updates.
    pub fn request_frame(&self) -> Result<(), FlutterEmbedderError> {
        if self.engine.0.is_null() {
            return Err(FlutterEmbedderError::EngineNotRunning);
        }
        unsafe {
            let result_code = (self.engine_dll.FlutterEngineScheduleFrame)(self.engine.0);
            if result_code == e::FlutterEngineResult_kSuccess {
                Ok(())
            } else {
                let err_msg = format!(
                    "FlutterEngineScheduleFrame FAILED for '{}': {:?}",
                    self.name, result_code
                );
                error!("[FlutterOverlay] {err_msg}");
                Err(FlutterEmbedderError::OperationFailed(err_msg))
            }
        }
    }

    /// Retrieves the D3D11 Shader Resource View (SRV) for this overlay's texture.
    /// Used by the host application to render the Flutter UI.
    /// This clones the SRV (calls AddRef). The caller must Release it.
    pub fn get_texture_srv(&self) -> Result<ID3D11ShaderResourceView, FlutterEmbedderError> {
        unsafe {
            if self.srv.GetResource().is_err() {
                error!("[FlutterOverlay] SRV for '{}' is invalid.", self.name);
                return Err(FlutterEmbedderError::OperationFailed(format!(
                    "Texture SRV for overlay '{}' is not valid.",
                    self.name
                )));
            }
        }
        Ok(self.srv.clone())
    }

    /// Shuts down the Flutter engine associated with this overlay and cleans up all related resources.
    ///
    /// This method takes ownership of the `Box<FlutterOverlay>` instance to ensure that all
    /// resources, including the Flutter engine and any loaded libraries or textures,
    /// are properly deallocated. After calling this method, the overlay instance is consumed
    /// and can no longer be used.
    ///
    /// # Returns
    /// `Ok(())` on successful shutdown or if the overlay was already effectively shut down.
    /// Logs an error if `FlutterEngineShutdown` reports a failure but still attempts to complete resource cleanup.
    pub fn shutdown(self: Box<Self>) -> Result<(), FlutterEmbedderError> {
        if self.engine.0.is_null() {
            warn!(
                "[FlutterOverlay::shutdown] Shutdown attempted on an overlay with a null engine handle."
            );
            return Ok(());
        }

        if let Some(handle_arc) = self.task_runner_thread {
            if let Ok(handle) = Arc::try_unwrap(handle_arc) {
                if let Err(e) = handle.join() {
                    error!(
                        "[FlutterOverlay::shutdown] Failed to join task runner thread for '{}': {:?}",
                        self.name, e
                    );
                }
            } else {
                warn!(
                    "[FlutterOverlay::shutdown] Task runner thread handle for '{}' still has multiple owners, cannot join directly here. Ensure graceful thread termination if necessary.",
                    self.name
                );
            }
        }

        unsafe {
            let result = (self.engine_dll.FlutterEngineShutdown)(self.engine.0);
            if result != e::FlutterEngineResult_kSuccess {
                let err_msg = format!(
                    "FlutterEngineShutdown failed for '{}': {:?}",
                    self.name, result
                );
                error!("[FlutterOverlay::shutdown] {err_msg}");
                return Err(FlutterEmbedderError::OperationFailed(err_msg));
            } else {
                info!(
                    "[FlutterOverlay::shutdown] FlutterEngineShutdown successful for '{}'.",
                    self.name
                );
            }
        }
        Ok(())
    }

    /// Sets the screen-space position of the overlay's top-left corner.
    pub fn set_position(&mut self, new_x: i32, new_y: i32) {
        self.x = new_x;
        self.y = new_y;

        if !self.engine.0.is_null() {
            update_flutter_window_metrics(
                self.engine.0,
                self.x,
                self.y,
                self.width,
                self.height,
                self.engine_dll.clone(),
            );
        }
    }

    /// Get th widht and height of the overlay.
    pub fn get_dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Returns the current (x, y) position of the overlay.
    /// The counterpart to the `set_position` method you implemented.
    pub fn get_position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    pub fn send_platform_message(
        &self,
        channel: &str,
        message: &[u8],
    ) -> Result<(), FlutterEmbedderError> {
        send_platform_message(self, channel, message)
    }

    /// Sets the visibility of the overlay.
    /// An invisible overlay will not be rendered and will not receive input.
    pub fn set_visibility(&mut self, is_visible: bool) {
        self.visible = is_visible;
    }

    /// Checks if the overlay is currently marked as visible.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Monotonic count of frames the renderer has presented. `None` when the
    /// renderer has no frame counter (software mode).
    pub fn presented_frame_count(&self) -> Option<u64> {
        match self.renderer_type {
            RendererType::OpenGL => Some(
                self.angle_frame_presented
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            RendererType::Software => None,
        }
    }

    /// Returns true once the renderer has produced at least one frame.
    pub fn has_first_frame(&self) -> bool {
        match self.renderer_type {
            RendererType::OpenGL => {
                self.angle_frame_presented
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0
            }
            RendererType::Software => self
                .software_first_frame_rendered
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Registers a custom handler for a platform channel.
    ///
    /// The handler takes the request bytes as an owned `Vec<u8>` and returns its
    /// response as a new `Vec<u8>`. This is a simpler, higher-level abstraction
    /// that handles memory allocation for the response automatically.
    ///
    /// # Arguments
    /// * `channel` - The name of the channel to listen on.
    /// * `handler` - A closure that takes `payload: Vec<u8>` and returns `Vec<u8>`.
    ///
    /// # Example
    /// ```rust, no_run
    /// my_overlay.register_channel_handler("my_game/get_player_state", |payload| {
    ///     let request_str = String::from_utf8_lossy(&payload);
    ///     println!("Request from Dart: {}", request_str);
    ///     
    ///     let state_json = r#"{"health": 100, "mana": 80}"#;
    ///     state_json.as_bytes().to_vec() // Return the response bytes
    /// });
    /// ```
    pub fn register_channel_handler<F>(&mut self, channel: &str, handler: F)
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        match self.message_handlers.lock() {
            Ok(mut handlers) => {
                handlers.insert(channel.to_string(), Box::new(handler));
            }
            Err(poisoned) => {
                log::error!(
                    "Failed to acquire lock on message_handlers because it was poisoned: {poisoned}"
                );
            }
        }
    }
    /// Triggers a "Hot Restart" for the running Flutter application.
    ///
    /// This works by sending a specific message on the "app/lifecycle" platform
    /// channel, which the Dart application must listen for.
    /// ```dart
    ///   const channel = BasicMessageChannel<String?>('app/lifecycle', StringCodec());
    ///   channel.setMessageHandler((String? message) async {
    ///     if (message == 'hot.restart') {
    ///       debugPrint("Hot restart command received from native code. Restarting...");
    ///       await ServicesBinding.instance.reassembleApplication();
    ///     }
    ///     return null;
    ///   });
    ///   ```
    pub fn hot_restart(&self) -> Result<(), FlutterEmbedderError> {
        info!(
            "[FlutterOverlay:'{}'] Sending 'hot.restart' command...",
            self.name
        );

        self.send_platform_message("app/lifecycle", "hot.restart".as_bytes())
    }

    /// Stores a Dart `SendPort` to enable native-to-Dart communication for this overlay.
    pub fn register_dart_port(&self, port: e::FlutterEngineDartPort) {
        info!(
            "[FlutterOverlay:'{}'] Registering Dart port: {}",
            self.name, port
        );
        self.dart_send_port.store(port, Ordering::SeqCst);
    }

    /// The internal dispatcher for sending a pre-constructed `FlutterEngineDartObject`
    /// to the Dart isolate.
    ///
    /// # Arguments
    ///
    /// * `object`: A reference to the FFI-compatible object to be sent.
    ///
    /// # Returns
    ///
    /// * `Ok(())` if the object was successfully posted.
    /// * `Err(FlutterEmbedderError)` if the engine is not running or if a Dart port
    ///   has not been registered via `register_dart_port`.
    ///
    /// # Safety
    ///
    /// The call to `FlutterEnginePostDartObject` is `unsafe` because it's a raw FFI
    /// call. This is considered safe in this context because:
    /// 1. The `FlutterEngineDll` loader ensures the function pointer is valid upon initialization.
    /// 2. We explicitly check that the `engine` handle is not null.
    /// 3. The `object` structure is built according to the C API's expectations.
    fn post_dart_object(
        &self,
        object: &e::FlutterEngineDartObject,
    ) -> Result<(), FlutterEmbedderError> {
        let port = self.dart_send_port.load(Ordering::SeqCst);
        if self.engine.0.is_null() {
            return Err(FlutterEmbedderError::EngineNotRunning);
        }
        if port == 0 {
            return Err(FlutterEmbedderError::OperationFailed(
                "Dart port not registered. Call `register_dart_port` first.".to_string(),
            ));
        }

        let result =
            unsafe { (self.engine_dll.FlutterEnginePostDartObject)(self.engine.0, port, object) };

        if result == e::FlutterEngineResult_kSuccess {
            Ok(())
        } else {
            let err_msg = format!("Failed to post Dart object with code: {result:?}");
            error!("[FlutterOverlay:'{}'] {}", self.name, err_msg);
            Err(FlutterEmbedderError::OperationFailed(err_msg))
        }
    }

    /// Posts a boolean value to the Dart isolate.
    ///
    /// In Dart, this will be received as a `bool`.
    ///
    /// # Arguments
    ///
    /// * `value`: The `bool` value to send.
    ///
    /// # Returns
    ///
    /// * A `Result` indicating the success or failure of the operation. See `post_dart_object`.
    pub fn post_bool(&self, value: bool) -> Result<(), FlutterEmbedderError> {
        let obj = e::FlutterEngineDartObject {
            type_: e::FlutterEngineDartObjectType_kFlutterEngineDartObjectTypeBool,
            __bindgen_anon_1: DartObjectUnion { bool_value: value },
        };
        self.post_dart_object(&obj)
    }

    /// Posts a 64-bit integer to the Dart isolate.
    ///
    /// In Dart, this will be received as an `int`.
    ///
    /// # Arguments
    ///
    /// * `value`: The `i64` value to send.
    ///
    /// # Returns
    ///
    /// * A `Result` indicating the success or failure of the operation. See `post_dart_object`.
    pub fn post_i64(&self, value: i64) -> Result<(), FlutterEmbedderError> {
        let obj = e::FlutterEngineDartObject {
            type_: e::FlutterEngineDartObjectType_kFlutterEngineDartObjectTypeInt64,
            __bindgen_anon_1: DartObjectUnion { int64_value: value },
        };
        self.post_dart_object(&obj)
    }

    /// Posts a 64-bit floating-point number to the Dart isolate.
    ///
    /// In Dart, this will be received as a `double`.
    ///
    /// # Arguments
    ///
    /// * `value`: The `f64` value to send.
    ///
    /// # Returns
    ///
    /// * A `Result` indicating the success or failure of the operation. See `post_dart_object`.
    pub fn post_f64(&self, value: f64) -> Result<(), FlutterEmbedderError> {
        let obj = e::FlutterEngineDartObject {
            type_: e::FlutterEngineDartObjectType_kFlutterEngineDartObjectTypeDouble,
            __bindgen_anon_1: DartObjectUnion {
                double_value: value,
            },
        };
        self.post_dart_object(&obj)
    }

    /// Posts a UTF-8 string to the Dart isolate.
    ///
    /// This function handles the conversion from a Rust `&str` to a C-compatible,
    /// null-terminated string. The Flutter engine makes a copy of the string data,
    /// so the memory allocated for the C-string is safely freed when this function returns.
    ///
    /// In Dart, this will be received as a `String`.
    ///
    /// # Arguments
    ///
    /// * `value`: The string slice to send.
    ///
    /// # Errors
    ///
    /// Returns an error if the input string contains internal null `\0` bytes,
    /// as this is not permitted in C-style strings.
    pub fn post_string(&self, value: &str) -> Result<(), FlutterEmbedderError> {
        let c_string = match CString::new(value) {
            Ok(s) => s,
            Err(_) => {
                return Err(FlutterEmbedderError::OperationFailed(
                    "String contains null bytes.".to_string(),
                ));
            }
        };
        let obj = e::FlutterEngineDartObject {
            type_: e::FlutterEngineDartObjectType_kFlutterEngineDartObjectTypeString,
            __bindgen_anon_1: DartObjectUnion {
                string_value: c_string.as_ptr(),
            },
        };
        self.post_dart_object(&obj)
    }

    /// Posts a raw byte slice to the Dart isolate.
    ///
    /// This method is highly efficient for sending arbitrary binary data, such as
    /// serialized objects, file contents, or image data. The engine makes an internal
    /// copy of the buffer, so the caller retains ownership of the original slice.
    ///
    /// In Dart, this will be received as a `Uint8List`.
    ///
    /// # Arguments
    ///
    /// * `buffer`: The byte slice to send.
    ///
    /// # Returns
    ///
    /// * A `Result` indicating the success or failure of the operation. See `post_dart_object`.
    pub fn post_buffer(&self, buffer: &[u8]) -> Result<(), FlutterEmbedderError> {
        let dart_buffer = e::FlutterEngineDartBuffer {
            struct_size: std::mem::size_of::<e::FlutterEngineDartBuffer>(),
            user_data: std::ptr::null_mut(),
            buffer_collect_callback: None, // Lets the engine perform a copy.
            buffer: buffer.as_ptr() as *mut u8,
            buffer_size: buffer.len(),
        };

        let obj = e::FlutterEngineDartObject {
            type_: e::FlutterEngineDartObjectType_kFlutterEngineDartObjectTypeBuffer,
            __bindgen_anon_1: DartObjectUnion {
                buffer_value: &dart_buffer,
            },
        };
        self.post_dart_object(&obj)
    }
}
