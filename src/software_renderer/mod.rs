//! # D3D11 Flutter embedder
//!
//! Renders Flutter into a host-owned Direct3D 11 texture so an existing D3D11
//! application or game can composite a Flutter UI on top of its own scene. This
//! is the "embed into a D3D11 app" path from the crate root.
//!
//! ## How it works
//!
//! The Flutter engine DLL is loaded dynamically at runtime (see
//! [`dynamic_flutter_engine_dll_loader`]). Each [`api::FlutterOverlay`] renders the
//! engine output into a D3D11 texture using one of three paths. The order they are
//! tried in is set by [`api::RendererPreference`]; each falls back to the next:
//!
//! * Vulkan (preferred): the engine renders with its Vulkan backend on the host's
//!   adapter. Frames land in a D3D11 texture imported into Vulkan and are handed
//!   over with a keyed mutex. Needs nothing shipped alongside the engine and is
//!   the stable home for Impeller, but renders the implicit view only.
//!   See [`vk_renderer`].
//! * OpenGL / ANGLE: the engine renders through ANGLE
//!   (OpenGL ES on top of D3D11). Frames land in a shared D3D11 texture and are
//!   handed to the host with a keyed-mutex handshake, so there is no CPU copy.
//!   See [`gl_renderer`].
//! * Software (fallback): the engine rasterizes on the CPU and the pixels are
//!   uploaded to a D3D11 texture each frame.
//!
//! Either way the host gets an `ID3D11ShaderResourceView` (via
//! [`api::FlutterOverlay::get_texture_srv`]) and draws it however it likes.
//!
//! ## Where to start
//!
//! * [`overlays_manager_api`]: the high-level [`FlutterOverlayManagerHandle`].
//!   A global handle that owns multiple overlays (a main UI plus per-plugin UIs),
//!   routes input, and drives per-frame rendering. This is what a real integration
//!   uses; its module docs contain the getting-started examples.
//! * [`api`]: the lower-level [`api::FlutterOverlay`] for driving a single overlay
//!   directly.
//!
//! ## Submodules
//!
//! * [`overlays_manager_api`]: global manager handle, input routing, keybinds.
//! * [`api`]: single-overlay create / tick / render / shutdown surface.
//! * [`d3d11_compositor`]: 3D primitive, 3D text, and post-processing renderers.
//! * [`gl_renderer`]: ANGLE (OpenGL ES on D3D11) interop and device-loss recovery.
//! * [`vk_renderer`]: Vulkan device, D3D11 texture import and keyed-mutex hand-off.
//! * [`multiview`]: extra Flutter views rendered into their own OS windows
//!   (OpenGL path only).
//! * [`dynamic_flutter_engine_dll_loader`]: dynamic load and cache of the engine DLL.
//!
//! [`FlutterOverlayManagerHandle`]: overlays_manager_api::FlutterOverlayManagerHandle

pub mod api;
pub mod d3d11_compositor;
pub mod dynamic_flutter_engine_dll_loader;
pub mod gl_renderer;
pub mod multiview;
mod overlay;
pub mod overlays_manager_api;
mod ticker;
pub mod vk_renderer;
#[cfg(test)]
mod tests;
