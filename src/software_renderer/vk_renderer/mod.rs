//! # Vulkan rendering path
//!
//! The engine renders with its Vulkan backend (Impeller or Skia, whichever the
//! engine switches select) on a `VkDevice` the embedder creates on the host's
//! adapter. Finished frames reach the host's D3D11 device through a D3D11
//! texture imported into Vulkan, ordered by a keyed mutex — no ANGLE, no second
//! D3D11 device, no CPU copy.
//!
//! Nothing has to be shipped for this path: `vulkan-1.dll` is installed by the
//! GPU driver and loaded at runtime. When Vulkan itself is unusable,
//! initialisation fails cleanly and the overlay falls back to the next renderer.
//!
//! ## Two hand-off modes
//!
//! * [`HandoffMode::SharedTexture`](context::HandoffMode): the zero-copy path
//!   described above. Needs `VK_KHR_external_memory_win32` +
//!   `VK_KHR_win32_keyed_mutex` and a device on the host's adapter, i.e. a native
//!   Windows driver stack.
//! * [`HandoffMode::Readback`](context::HandoffMode): for Wine/Proton and
//!   MoltenVK, where Vulkan works but cannot share a texture with D3D11. The
//!   frame is read back into host memory and uploaded into the host's texture.
//!   Rasterisation stays on the GPU, so Impeller and fragment shaders work where
//!   the only alternative would be the software renderer. Chosen automatically;
//!   [`FORCE_READBACK_ENV`](interop::FORCE_READBACK_ENV) forces it.
//!
//! ## Limitation: implicit view only
//!
//! The engine implements Vulkan *backing stores* (the `FlutterCompositor` path
//! that multi-view needs) for Skia only; under Impeller it is unimplemented in
//! the engine. This path therefore drives the engine through
//! `get_next_image` / `present_image`, which renders the implicit view only.
//! [Satellite views](crate::software_renderer::multiview) still require the
//! OpenGL renderer; select it with
//! [`RendererPreference::OpenGL`](crate::software_renderer::api::RendererPreference).
//!
//! ## Submodules
//!
//! * [`context`]: instance + device creation on the host adapter.
//! * [`queue_guard`]: the queue lock shared with the engine.
//! * [`shared_image`]: the D3D11 texture imported into Vulkan and its keyed mutex.
//! * [`readback`]: the host-memory frame buffers of the readback hand-off.
//! * [`interop`]: per-frame flow, engine callbacks and the renderer config.

pub mod context;
pub mod interop;
pub(crate) mod queue_guard;
pub(crate) mod readback;
pub(crate) mod shared_image;
#[cfg(test)]
mod tests;
