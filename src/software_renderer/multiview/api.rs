use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use log::{error, info};
use windows::Win32::Foundation::{HANDLE, HWND};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::System::Threading::CreateEventW;

use crate::bindings::embedder::{
    self as e, FlutterAddViewInfo, FlutterAddViewResult, FlutterRemoveViewInfo,
    FlutterRemoveViewResult, FlutterViewId, FlutterWindowMetricsEvent,
};
use crate::software_renderer::api::{FlutterEmbedderError, RendererType};
use crate::software_renderer::multiview::resize_decision::{
    needs_host_texture_recreate, should_copy_frame,
};
use crate::software_renderer::multiview::view_surface::ViewSurface;
use crate::software_renderer::multiview::window::{
    WINDOW_CONTROL_CHANNEL, handle_window_control_message,
};
use crate::software_renderer::overlay::d3d::{
    create_compositing_texture, create_shared_texture_no_mutex, create_srv,
};
use crate::software_renderer::overlay::overlay_impl::{FlutterOverlay, SendHwnd, SendableHandle};

struct AddViewSync {
    done: std::sync::atomic::AtomicBool,
    added: std::sync::atomic::AtomicBool,
}

impl FlutterOverlay {
    pub fn add_window_view(
        &mut self,
        game_device: &ID3D11Device,
        hwnd: HWND,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<FlutterViewId, FlutterEmbedderError> {
        self.add_view_inner(game_device, Some(hwnd), width, height, pixel_ratio)
    }

    /// Add an offscreen Flutter view: renders into a game-readable D3D11 texture
    /// with no OS window (no window-control channel, null HWND). The game reads
    /// its `view_texture_srv` and draws it as a native UI layer. Requires the
    /// OpenGL renderer + compositor, so it must run on the main overlay.
    pub fn add_offscreen_view(
        &mut self,
        game_device: &ID3D11Device,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<FlutterViewId, FlutterEmbedderError> {
        self.add_view_inner(game_device, None, width, height, pixel_ratio)
    }

    fn add_view_inner(
        &mut self,
        game_device: &ID3D11Device,
        hwnd: Option<HWND>,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<FlutterViewId, FlutterEmbedderError> {
        if self.engine.0.is_null() {
            return Err(FlutterEmbedderError::EngineNotRunning);
        }
        if self.renderer_type != RendererType::OpenGL || !self.compositor_active {
            return Err(FlutterEmbedderError::OperationFailed(
                "add_view requires the OpenGL renderer with the compositor active".to_string(),
            ));
        }
        if width == 0 || height == 0 {
            return Err(FlutterEmbedderError::OperationFailed(
                "add_view: width/height must be non-zero".to_string(),
            ));
        }

        if hwnd.is_some() {
            self.register_channel_handler(WINDOW_CONTROL_CHANNEL, |payload| {
                handle_window_control_message(&payload)
            });
        }

        let angle_device = match &self.angle_state {
            Some(s) => s.0.get_d3d_device().map_err(|e| {
                FlutterEmbedderError::OperationFailed(format!("angle device unavailable: {e}"))
            })?,
            None => {
                return Err(FlutterEmbedderError::OperationFailed(
                    "no ANGLE state on host overlay".to_string(),
                ));
            }
        };

        let (angle_internal_texture, shared_handle) =
            create_shared_texture_no_mutex(&angle_device, width, height).map_err(|e| {
                FlutterEmbedderError::OperationFailed(format!("shared texture failed: {e}"))
            })?;

        let mut opened: Option<ID3D11Texture2D> = None;
        let game_view_texture = unsafe {
            game_device
                .OpenSharedResource(shared_handle, &mut opened)
                .ok()
                .and(opened)
        }
        .ok_or_else(|| {
            FlutterEmbedderError::OperationFailed(
                "OpenSharedResource failed for new view".to_string(),
            )
        })?;

        let texture = create_compositing_texture(game_device, width, height);
        let srv = create_srv(game_device, &texture);

        let view_id = self.view_registry.allocate_id();
        let surface = ViewSurface {
            view_id,
            hwnd: SendHwnd(hwnd.unwrap_or_default()),
            width,
            height,
            texture_size: (width, height),
            host_texture_size: (width, height),
            pixel_ratio,
            texture,
            srv,
            angle_internal_texture: Some(angle_internal_texture),
            angle_shared_texture: Some(game_view_texture),
            shared_handle: Some(SendableHandle(shared_handle)),
            frame_complete_query: None,
            frame_presented: AtomicU64::new(0),
            frame_copied: AtomicU64::new(0),
            frame_event: SendableHandle(
                unsafe { CreateEventW(None, false, false, None) }.unwrap_or_default(),
            ),
            damage_rects: Mutex::new(Vec::new()),
            frame_damage_rects: Mutex::new(Vec::new()),
            gl: None,
        };
        self.view_registry.insert(view_id, surface);

        let metrics = FlutterWindowMetricsEvent {
            struct_size: std::mem::size_of::<FlutterWindowMetricsEvent>(),
            width: width as usize,
            height: height as usize,
            pixel_ratio,
            left: 0,
            top: 0,
            physical_view_inset_top: 0.0,
            physical_view_inset_right: 0.0,
            physical_view_inset_bottom: 0.0,
            physical_view_inset_left: 0.0,
            display_id: 0,
            view_id,
        };

        let sync = Box::new(AddViewSync {
            done: std::sync::atomic::AtomicBool::new(false),
            added: std::sync::atomic::AtomicBool::new(false),
        });
        let sync_ptr = Box::into_raw(sync);

        let info = FlutterAddViewInfo {
            struct_size: std::mem::size_of::<FlutterAddViewInfo>(),
            view_id,
            view_metrics: &metrics,
            user_data: sync_ptr as *mut c_void,
            add_view_callback: Some(add_view_callback),
        };

        let result = unsafe { (self.engine_dll.FlutterEngineAddView)(self.engine.0, &info) };
        if result != e::FlutterEngineResult_kSuccess {
            let _ = unsafe { Box::from_raw(sync_ptr) };
            self.view_registry.remove(view_id);
            return Err(FlutterEmbedderError::OperationFailed(format!(
                "FlutterEngineAddView failed to start: {result:?}"
            )));
        }

        let added = wait_for_add_view(sync_ptr);
        let _ = unsafe { Box::from_raw(sync_ptr) };

        if !added {
            self.view_registry.remove(view_id);
            return Err(FlutterEmbedderError::OperationFailed(
                "engine reported add_view failed".to_string(),
            ));
        }

        info!("[multiview] added view {view_id} ({width}x{height})");
        Ok(view_id)
    }

    pub fn remove_view(&mut self, view_id: FlutterViewId) -> Result<(), FlutterEmbedderError> {
        if self.engine.0.is_null() {
            return Err(FlutterEmbedderError::EngineNotRunning);
        }
        if view_id == super::IMPLICIT_VIEW_ID {
            return Err(FlutterEmbedderError::OperationFailed(
                "cannot remove the implicit view (view 0)".to_string(),
            ));
        }

        let sync = Box::new(AddViewSync {
            done: std::sync::atomic::AtomicBool::new(false),
            added: std::sync::atomic::AtomicBool::new(false),
        });
        let sync_ptr = Box::into_raw(sync);

        let info = FlutterRemoveViewInfo {
            struct_size: std::mem::size_of::<FlutterRemoveViewInfo>(),
            view_id,
            user_data: sync_ptr as *mut c_void,
            remove_view_callback: Some(remove_view_callback),
        };

        let result = unsafe { (self.engine_dll.FlutterEngineRemoveView)(self.engine.0, &info) };
        if result != e::FlutterEngineResult_kSuccess {
            let _ = unsafe { Box::from_raw(sync_ptr) };
            return Err(FlutterEmbedderError::OperationFailed(format!(
                "FlutterEngineRemoveView failed to start: {result:?}"
            )));
        }

        let removed = wait_for_add_view(sync_ptr);
        let _ = unsafe { Box::from_raw(sync_ptr) };

        self.view_registry.remove(view_id);

        if !removed {
            error!("[multiview] engine reported remove_view {view_id} failed");
        } else {
            info!("[multiview] removed view {view_id}");
        }
        Ok(())
    }

    pub fn view_texture_srv(&self, view_id: FlutterViewId) -> Option<ID3D11ShaderResourceView> {
        self.view_registry.with_view(view_id, |s| s.srv.clone())
    }

    pub fn secondary_view_ids(&self) -> Vec<FlutterViewId> {
        self.view_registry.view_ids()
    }

    pub fn view_shared_handle(&self, view_id: FlutterViewId) -> Option<(HANDLE, u32, u32)> {
        self.view_registry
            .with_view(view_id, |s| {
                s.shared_handle
                    .map(|h| (h.0, s.texture_size.0, s.texture_size.1))
            })
            .flatten()
    }

    pub fn tick_view(&self, view_id: FlutterViewId, context: &ID3D11DeviceContext) -> bool {
        self.view_registry
            .with_view(view_id, |surface| tick_view_surface(surface, context))
            .unwrap_or(false)
    }

    pub fn resize_view(
        &mut self,
        view_id: FlutterViewId,
        width: u32,
        height: u32,
        pixel_ratio: f64,
    ) -> Result<(), FlutterEmbedderError> {
        if width == 0 || height == 0 {
            return Err(FlutterEmbedderError::OperationFailed(
                "resize_view: zero dimension".to_string(),
            ));
        }

        let updated = self.view_registry.with_view(view_id, |s| {
            s.width = width;
            s.height = height;
            s.pixel_ratio = pixel_ratio;
        });
        updated.ok_or(FlutterEmbedderError::InvalidHandle)?;

        let metrics = FlutterWindowMetricsEvent {
            struct_size: std::mem::size_of::<FlutterWindowMetricsEvent>(),
            width: width as usize,
            height: height as usize,
            pixel_ratio,
            left: 0,
            top: 0,
            physical_view_inset_top: 0.0,
            physical_view_inset_right: 0.0,
            physical_view_inset_bottom: 0.0,
            physical_view_inset_left: 0.0,
            display_id: 0,
            view_id,
        };
        let r = unsafe {
            (self.engine_dll.FlutterEngineSendWindowMetricsEvent)(self.engine.0, &metrics)
        };
        if r != e::FlutterEngineResult_kSuccess {
            return Err(FlutterEmbedderError::OperationFailed(format!(
                "resize metrics failed: {r:?}"
            )));
        }
        Ok(())
    }

    /// Monotonic counter of frames the engine has presented for a secondary view.
    /// A window thread compares it against its last-drawn value to skip redundant
    /// blits/presents when Flutter produced nothing new.
    pub fn view_frame_counter(&self, view_id: FlutterViewId) -> u64 {
        self.view_registry
            .with_view(view_id, |s| s.frame_presented.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// Auto-reset event a window thread blocks on until the engine presents a new
    /// frame for `view_id`. Returns an invalid handle if the view is gone.
    pub fn view_frame_event(&self, view_id: FlutterViewId) -> HANDLE {
        self.view_registry
            .with_view(view_id, |s| s.frame_event.0)
            .unwrap_or_default()
    }
}

fn tick_view_surface(surface: &mut ViewSurface, context: &ID3D11DeviceContext) -> bool {
    use windows::Win32::Graphics::Direct3D11::D3D11_BOX;

    let presented = surface.frame_presented.load(Ordering::Acquire);
    let copied = surface.frame_copied.load(Ordering::Relaxed);
    if !should_copy_frame(presented, copied) {
        return false;
    }

    let (tw, th) = surface.texture_size;
    if needs_host_texture_recreate((tw, th), surface.host_texture_size)
        && let Ok(game_device) = unsafe { context.GetDevice() }
    {
        let game_device: ID3D11Device = game_device;
        surface.texture = create_compositing_texture(&game_device, tw, th);
        surface.srv = create_srv(&game_device, &surface.texture);
        surface.host_texture_size = (tw, th);
    }

    if surface.angle_shared_texture.is_none()
        && let Some(handle) = &surface.shared_handle
        && let Ok(game_device) = unsafe { context.GetDevice() }
    {
        let game_device: ID3D11Device = game_device;
        let mut opened: Option<ID3D11Texture2D> = None;
        if unsafe { game_device.OpenSharedResource(handle.0, &mut opened) }.is_ok()
            && let Some(tex) = opened
        {
            surface.angle_shared_texture = Some(tex);
        }
    }

    let shared = match &surface.angle_shared_texture {
        Some(t) => t,
        None => return false,
    };

    let damage: Vec<_> = surface
        .frame_damage_rects
        .lock()
        .map(|mut r| r.drain(..).collect())
        .unwrap_or_default();

    // No keyed mutex on satellite views — it does not work across the three
    // device round-trips (ANGLE device → shared texture → window device). The
    // engine flushed its GL before bumping `frame_presented`, and this copy only
    // runs once `should_copy_frame` sees a newly-presented frame, so we copy the
    // last completed frame into our private compositing texture without a mutex
    // handshake.
    unsafe {
        let w = tw;
        let h = th;

        if damage.is_empty() {
            context.CopyResource(&surface.texture, shared);
        } else {
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
                    &surface.texture,
                    0,
                    left,
                    top,
                    0,
                    shared,
                    0,
                    Some(&src_box),
                );
            }
        }
    }

    surface.frame_copied.store(presented, Ordering::Relaxed);
    true
}

fn wait_for_add_view(sync_ptr: *mut AddViewSync) -> bool {
    let sync = unsafe { &*sync_ptr };

    for _ in 0..2000 {
        if sync.done.load(Ordering::Acquire) {
            return sync.added.load(Ordering::Acquire);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    error!("[multiview] timed out waiting for add/remove view callback");
    false
}

extern "C" fn add_view_callback(result: *const FlutterAddViewResult) {
    if result.is_null() {
        return;
    }
    let result = unsafe { &*result };
    if result.user_data.is_null() {
        return;
    }
    let sync = unsafe { &*(result.user_data as *const AddViewSync) };
    sync.added.store(result.added, Ordering::Release);
    sync.done.store(true, Ordering::Release);
}

extern "C" fn remove_view_callback(result: *const FlutterRemoveViewResult) {
    if result.is_null() {
        return;
    }
    let result = unsafe { &*result };
    if result.user_data.is_null() {
        return;
    }
    let sync = unsafe { &*(result.user_data as *const AddViewSync) };
    sync.added.store(result.removed, Ordering::Release);
    sync.done.store(true, Ordering::Release);
}
