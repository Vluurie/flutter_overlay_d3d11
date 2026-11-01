//! Multi-view support for the hardware-accelerated (ANGLE/OpenGL) embedder.
//!
//! # Architecture
//!
//! A single Flutter engine (one isolate, shared Dart state) can drive multiple
//! *views*. View `0` is the **implicit view**; in this crate that is the
//! in-game overlay represented by [`FlutterOverlay`]. Additional views
//! (`view_id > 0`) are created at runtime via [`FlutterEngineAddView`] and each
//! renders into its own GPU texture, which the host composites into a separate
//! top-level HWND/swapchain.
//!
//! The engine is told about views through a `FlutterCompositor` supplied in
//! `FlutterProjectArgs`. When a compositor is present, the engine no longer uses
//! the renderer-config `present`/`present_with_info` path; instead it asks the
//! embedder for a *backing store* per layer (`create_backing_store_callback`)
//! and then presents composed layers per view (`present_view_callback`). The
//! `FlutterBackingStoreConfig` and `FlutterPresentViewInfo` both carry a
//! `view_id`, which is how we route frames to the right surface.
//!
//! # Why a registry
//!
//! Every compositor callback receives the `FlutterCompositor.user_data` baton.
//! We set that baton to a pointer to the owning [`FlutterOverlay`] (the engine
//! host). The overlay holds a [`ViewRegistry`] mapping `view_id -> ViewSurface`,
//! so the callbacks can look up the correct GPU resources by `view_id`.
//!
//! [`FlutterEngineAddView`]: crate::bindings::embedder::FlutterEngineAddView
//! [`FlutterOverlay`]: crate::software_renderer::api::FlutterOverlay

pub mod api;
pub mod backing_store;
pub mod compositor;
pub(crate) mod resize_decision;
pub mod view_surface;
pub mod window;
#[cfg(test)]
pub(crate) mod tests;

use std::collections::HashMap;
use std::sync::Mutex;

use crate::bindings::embedder::FlutterViewId;
use view_surface::ViewSurface;

/// The `view_id` of the implicit view (the in-game overlay). The engine reserves
/// this id; it cannot be added or removed via the AddView/RemoveView APIs.
pub const IMPLICIT_VIEW_ID: FlutterViewId = 0;

/// Thread-safe registry of secondary views keyed by `view_id`.
///
/// The implicit view (`view_id == 0`) is **not** stored here; it is the owning
/// [`FlutterOverlay`](crate::software_renderer::api::FlutterOverlay) itself. Only
/// `add_window_view`-created satellite views live in the registry.
#[derive(Default)]
pub struct ViewRegistry {
    views: Mutex<HashMap<FlutterViewId, ViewSurface>>,
    /// Monotonic allocator for new view ids. Starts at 1 because 0 is implicit.
    next_id: Mutex<FlutterViewId>,
}

impl ViewRegistry {
    pub fn new() -> Self {
        Self {
            views: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
        }
    }

    /// Reserves the next free `view_id`. The caller is responsible for actually
    /// inserting a [`ViewSurface`] under that id via [`insert`].
    ///
    /// [`insert`]: ViewRegistry::insert
    pub fn allocate_id(&self) -> FlutterViewId {
        let mut guard = self.next_id.lock().expect("view id allocator poisoned");
        let id = *guard;
        *guard += 1;
        id
    }

    /// Registers a fully-initialized surface under `view_id`.
    pub fn insert(&self, view_id: FlutterViewId, surface: ViewSurface) {
        self.views
            .lock()
            .expect("view registry poisoned")
            .insert(view_id, surface);
    }

    /// Removes and returns the surface for `view_id`, if present.
    pub fn remove(&self, view_id: FlutterViewId) -> Option<ViewSurface> {
        self.views
            .lock()
            .expect("view registry poisoned")
            .remove(&view_id)
    }

    /// Runs `f` against the surface for `view_id` while holding the registry
    /// lock. Returns `None` if no such view exists.
    pub fn with_view<R>(
        &self,
        view_id: FlutterViewId,
        f: impl FnOnce(&mut ViewSurface) -> R,
    ) -> Option<R> {
        let mut guard = self.views.lock().expect("view registry poisoned");
        guard.get_mut(&view_id).map(f)
    }

    /// Number of secondary views currently registered.
    pub fn len(&self) -> usize {
        self.views.lock().expect("view registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the list of currently registered secondary view ids.
    pub fn view_ids(&self) -> Vec<FlutterViewId> {
        self.views
            .lock()
            .expect("view registry poisoned")
            .keys()
            .copied()
            .collect()
    }
}
