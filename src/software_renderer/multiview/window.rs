use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicIsize, AtomicU64, Ordering};
use std::thread::JoinHandle;

use ::windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use ::windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP;
use ::windows::Win32::Graphics::Direct3D11::{
    D3D11_COMPARISON_NEVER, D3D11_FILTER_MIN_MAG_MIP_LINEAR, D3D11_FLOAT32_MAX, D3D11_SAMPLER_DESC,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_VIEWPORT, ID3D11Device, ID3D11DeviceContext,
    ID3D11PixelShader, ID3D11RenderTargetView, ID3D11SamplerState, ID3D11ShaderResourceView,
    ID3D11Texture2D, ID3D11VertexShader,
};
use ::windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use ::windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_NONE, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory2,
    IDXGISwapChain1,
};
use ::windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::UI::HiDpi::GetDpiForWindow;
use ::windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWL_STYLE, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, GetWindowRect, HTBOTTOM,
    HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT,
    IDC_ARROW, IsIconic, KillTimer, LoadCursorW, MSG, MsgWaitForMultipleObjects, PM_REMOVE,
    PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT, RegisterClassW, SW_MINIMIZE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetTimer,
    SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCALCSIZE, WM_NCCREATE, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_QUIT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSW,
    WS_CAPTION, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_THICKFRAME,
    WS_VISIBLE,
};
use ::windows::core::{Interface, PCWSTR};
use log::{error, info};
use serde_json::{Value, from_slice};
use winapi::um::winuser::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};

use crate::bindings::embedder::{
    FlutterPointerDeviceKind_kFlutterPointerDeviceKindMouse, FlutterPointerEvent,
    FlutterPointerPhase, FlutterPointerPhase_kAdd, FlutterPointerPhase_kDown,
    FlutterPointerPhase_kHover, FlutterPointerPhase_kMove, FlutterPointerPhase_kRemove,
    FlutterPointerPhase_kUp, FlutterPointerSignalKind_kFlutterPointerSignalKindNone,
    FlutterPointerSignalKind_kFlutterPointerSignalKindScroll, FlutterViewId,
};
use crate::software_renderer::api::FlutterEmbedderError;
use crate::software_renderer::multiview::resize_decision::can_open_shared_texture;
use crate::software_renderer::overlay::d3d::create_d3d_device_on_same_adapter;
use crate::software_renderer::overlay::keyevents::handle_keyboard_event_for_view;
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::overlay::textinput::{
    SharedViewKeyboardState, ViewKeyboardState, register_view_keyboard_state,
    unregister_view_keyboard_state,
};

static WINDOW_CONTROLS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<FlutterViewId, WindowControls>>,
> = std::sync::OnceLock::new();

static LAST_POINTER_DOWN_MS: AtomicI64 = AtomicI64::new(0);

fn register_window_controls(view_id: FlutterViewId, controls: WindowControls) {
    let map = WINDOW_CONTROLS.get_or_init(Default::default);
    match map.lock() {
        Ok(mut g) => {
            g.insert(view_id, controls);
        }
        Err(poisoned) => {
            panic!(
                "register_window_controls: WINDOW_CONTROLS mutex poisoned while registering view {view_id}: {poisoned}"
            );
        }
    }
}

fn unregister_window_controls(view_id: FlutterViewId) {
    let map = WINDOW_CONTROLS.get_or_init(Default::default);
    match map.lock() {
        Ok(mut g) => {
            g.remove(&view_id);
        }
        Err(poisoned) => {
            panic!(
                "unregister_window_controls: WINDOW_CONTROLS mutex poisoned while unregistering view {view_id}: {poisoned}"
            );
        }
    }
}

fn monitor_work_area(hwnd: HWND) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut info).as_bool() {
            let wa = info.rcWork;
            Some(wa)
        } else {
            None
        }
    }
}

fn lookup_window_controls(view_id: FlutterViewId) -> Option<WindowControls> {
    let map = WINDOW_CONTROLS.get_or_init(Default::default);
    match map.lock() {
        Ok(g) => g.get(&view_id).cloned(),
        Err(poisoned) => panic!(
            "lookup_window_controls: WINDOW_CONTROLS mutex poisoned while looking up view {view_id}: {poisoned}"
        ),
    }
}

pub const WINDOW_CONTROL_CHANNEL: &str = "flutter_embedder/satellite_window";

pub fn handle_window_control_message(payload: &[u8]) -> Vec<u8> {
    let parsed: Value = match from_slice(payload) {
        Ok(v) => v,
        Err(_) => return b"{}".to_vec(),
    };
    let view_id = parsed.get("viewId").and_then(|v| v.as_i64());
    let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");

    let Some(view_id) = view_id else {
        return b"{}".to_vec();
    };
    let Some(controls) = lookup_window_controls(view_id) else {
        return b"{}".to_vec();
    };

    match method {
        "minimize" => controls.minimize(),
        "maximize" => controls.maximize(),
        "restore" => controls.restore(),
        "startDrag" => controls.start_drag(),
        "close" => controls.close(),
        "setTitle" => {
            if let Some(t) = parsed.get("title").and_then(|v| v.as_str()) {
                controls.set_title(t);
            }
        }
        "isMaximized" => {
            let maxed = controls.is_maximized();
            return format!("{{\"isMaximized\":{maxed}}}").into_bytes();
        }
        _ => {}
    }
    b"{}".to_vec()
}

#[derive(Clone)]
pub struct WindowControls {
    should_close: Arc<AtomicBool>,
    hwnd: Arc<AtomicIsize>,
    present_count: Arc<AtomicU64>,
}

impl WindowControls {
    pub fn present_count(&self) -> u64 {
        self.present_count.load(Ordering::Acquire)
    }

    pub fn minimize(&self) {
        self.post(WM_APP_MINIMIZE);
    }

    pub fn maximize(&self) {
        self.post(WM_APP_MAXIMIZE);
    }

    pub fn restore(&self) {
        self.post(WM_APP_RESTORE);
    }

    pub fn is_maximized(&self) -> bool {
        let mut maxed = false;
        self.with_hwnd(|h| unsafe {
            let ptr = GetWindowLongPtrW(h, GWLP_USERDATA) as *const WindowProcState;
            if let Some(state) = ptr.as_ref() {
                maxed = state.maximized.get();
            }
        });
        maxed
    }

    pub fn hwnd_value(&self) -> isize {
        self.hwnd.load(Ordering::Acquire)
    }

    pub fn start_drag(&self) {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn ReleaseCapture() -> i32;
        }
        self.with_hwnd(|h| unsafe {
            ReleaseCapture();
            SendMessageW(
                h,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                Some(LPARAM(0)),
            );
        });
    }

    pub fn set_title(&self, title: &str) {
        let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
        self.with_hwnd(|h| unsafe {
            let _ = SetWindowTextW(h, PCWSTR(wide.as_ptr()));
        });
    }

    pub fn close(&self) {
        self.should_close.store(true, Ordering::Release);
        self.post(WM_APP_CLOSE);
    }

    fn post(&self, msg: u32) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let _ = unsafe { PostMessageW(Some(HWND(raw as *mut _)), msg, WPARAM(0), LPARAM(0)) };
        }
    }

    fn with_hwnd(&self, f: impl FnOnce(HWND)) {
        let raw = self.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            f(HWND(raw as *mut _));
        }
    }
}

/// A live secondary OS window backed by its own Flutter view.
///
/// Returned when you spawn an extra window for an overlay (OpenGL path only). The
/// window runs on its own thread; dropping this handle, or calling [`close`], tears
/// the window down. Use [`view_id`] to refer to the underlying Flutter view, and
/// the control methods to drive standard window operations from native code (for
/// example wiring a custom Flutter title bar to [`minimize`] / [`maximize`] /
/// [`start_drag`]).
///
/// [`close`]: SatelliteWindow::close
/// [`view_id`]: SatelliteWindow::view_id
/// [`minimize`]: SatelliteWindow::minimize
/// [`maximize`]: SatelliteWindow::maximize
/// [`start_drag`]: SatelliteWindow::start_drag
pub struct SatelliteWindow {
    view_id: Arc<AtomicI64>,
    controls: WindowControls,
    thread: Option<JoinHandle<()>>,
}

impl SatelliteWindow {
    /// The Flutter view id this window renders.
    pub fn view_id(&self) -> FlutterViewId {
        self.view_id.load(Ordering::Acquire)
    }

    /// A cloneable handle to this window's controls, usable from other threads.
    pub fn controls(&self) -> WindowControls {
        self.controls.clone()
    }

    /// Minimizes the window.
    pub fn minimize(&self) {
        self.controls.minimize();
    }
    /// Maximizes the window.
    pub fn maximize(&self) {
        self.controls.maximize();
    }
    /// Restores the window from a minimized or maximized state.
    pub fn restore(&self) {
        self.controls.restore();
    }
    /// Returns `true` if the window is currently maximized.
    pub fn is_maximized(&self) -> bool {
        self.controls.is_maximized()
    }
    /// Begins a drag-move of the window (for a custom, borderless title bar).
    pub fn start_drag(&self) {
        self.controls.start_drag();
    }
    /// Sets the window's title-bar text.
    pub fn set_title(&self, title: &str) {
        self.controls.set_title(title);
    }

    /// Closes the window and waits for its thread to finish. Dropping the handle
    /// does the same close without joining.
    pub fn close(mut self) {
        self.controls.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SatelliteWindow {
    fn drop(&mut self) {
        self.controls.close();
    }
}

/// Describes a secondary window to spawn for an overlay. Implements [`Default`]
/// (an 800x600 window titled "Flutter Window"), so override only what you need.
pub struct WindowSpec {
    /// Initial title-bar text.
    pub title: String,
    /// Initial client width in logical pixels.
    pub width: u32,
    /// Initial client height in logical pixels.
    pub height: u32,
    /// Device pixel ratio for the view. `None` lets the embedder pick a default.
    pub pixel_ratio: Option<f64>,
    /// Window chrome / style (borders, resizability).
    pub style: WindowStyle,
}

impl Default for WindowSpec {
    fn default() -> Self {
        Self {
            title: "Flutter Window".to_string(),
            width: 800,
            height: 600,
            pixel_ratio: None,
            style: WindowStyle::default(),
        }
    }
}

#[derive(Clone, Copy)]
pub struct WindowStyle {
    pub decorated: bool,

    pub resizable: bool,
}

impl Default for WindowStyle {
    fn default() -> Self {
        Self {
            decorated: true,
            resizable: true,
        }
    }
}

impl FlutterOverlay {
    pub unsafe fn spawn_window(
        &mut self,
        game_device: &ID3D11Device,
        spec: WindowSpec,
    ) -> Result<SatelliteWindow, FlutterEmbedderError> {
        let host = self as *mut FlutterOverlay;
        unsafe { self.spawn_window_view(host, game_device, spec) }
    }

    pub unsafe fn spawn_window_view(
        &self,
        host: *mut FlutterOverlay,
        game_device: &ID3D11Device,
        spec: WindowSpec,
    ) -> Result<SatelliteWindow, FlutterEmbedderError> {
        if host.is_null() {
            return Err(FlutterEmbedderError::InvalidHandle);
        }

        let device = game_device.clone();
        let view_id = Arc::new(AtomicI64::new(-1));
        let should_close = Arc::new(AtomicBool::new(false));
        let hwnd_slot = Arc::new(AtomicIsize::new(0));
        let present_count = Arc::new(AtomicU64::new(0));

        let host_send = HostPtr(host);
        let device_send = SendDevice(device);
        let view_id_t = view_id.clone();
        let should_close_t = should_close.clone();
        let hwnd_t = hwnd_slot.clone();
        let present_count_t = present_count.clone();

        let thread = std::thread::Builder::new()
            .name(format!("flutter-window-{}", spec.title))
            .spawn(move || {
                if let Err(e) = run_window_thread(
                    host_send,
                    device_send,
                    spec,
                    view_id_t,
                    should_close_t,
                    hwnd_t,
                    present_count_t,
                ) {
                    error!("[multiview::window] window thread failed: {e}");
                }
            })
            .map_err(|e| {
                FlutterEmbedderError::OperationFailed(format!("spawn window thread: {e}"))
            })?;

        let controls = WindowControls {
            should_close,
            hwnd: hwnd_slot,
            present_count,
        };

        Ok(SatelliteWindow {
            view_id,
            controls,
            thread: Some(thread),
        })
    }
}

struct HostPtr(*mut FlutterOverlay);
unsafe impl Send for HostPtr {}

struct SendDevice(ID3D11Device);
unsafe impl Send for SendDevice {}

const MODAL_PUMP_TIMER_ID: usize = 0xF1;

struct RenderState {
    hwnd: HWND,
    host_ptr: *mut FlutterOverlay,
    view_id: FlutterViewId,
    win_device: ID3D11Device,
    win_ctx: ID3D11DeviceContext,
    swap_chain: Option<IDXGISwapChain1>,
    blit: BlitPipeline,
    shared_tex: Option<ID3D11Texture2D>,
    shared_srv: Option<ID3D11ShaderResourceView>,
    cur_w: u32,
    cur_h: u32,
    target_w: u32,
    target_h: u32,
    opened_w: u32,
    opened_h: u32,
    last_frame: u64,
    cur_dpr: f64,
    auto_dpr: bool,
    pending_resize: Arc<AtomicU64>,
    last_present: std::time::Instant,
    present_count: Arc<AtomicU64>,
}

thread_local! {
    static RENDER_STATE: std::cell::Cell<*mut RenderState> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

fn run_pump(_hwnd: HWND) {
    let ptr = RENDER_STATE.with(|c| c.get());
    if ptr.is_null() {
        return;
    }
    let rs = unsafe { &mut *ptr };
    pump_frame(rs, false);
}

fn pump_frame(rs: &mut RenderState, block_for_frame: bool) {
    let pending = rs.pending_resize.swap(0, Ordering::Acquire);

    if unsafe { IsIconic(rs.hwnd) }.as_bool() {
        if block_for_frame {
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        return;
    }

    let (new_w, new_h) = {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(rs.hwnd, &mut rc);
        }
        (
            (rc.right - rc.left).max(1) as u32,
            (rc.bottom - rc.top).max(1) as u32,
        )
    };

    let _ = pending;

    if new_w != rs.target_w || new_h != rs.target_h {
        rs.target_w = new_w;
        rs.target_h = new_h;
        if rs.auto_dpr {
            rs.cur_dpr = dpi_scale_for_window(rs.hwnd);
        }
        let host_ref = unsafe { &mut *rs.host_ptr };
        if let Err(e) = host_ref.resize_view(rs.view_id, new_w, new_h, rs.cur_dpr) {
            error!("[multiview::window] resize_view failed: {e}");
        }
    }

    let engine_size = unsafe { &*rs.host_ptr }
        .view_shared_handle(rs.view_id)
        .map(|(_, w, h)| (w, h));

    let engine_grew = matches!(engine_size, Some((ew, eh)) if (ew, eh) != (rs.cur_w, rs.cur_h));

    if engine_grew && let Some((ew, eh)) = engine_size {
        rs.cur_w = ew;
        rs.cur_h = eh;
        rs.shared_tex = None;
        rs.shared_srv = None;
        rs.blit.release_rtv();
        if let Some(sc) = &rs.swap_chain {
            let resize = unsafe {
                sc.ResizeBuffers(
                    0,
                    rs.cur_w,
                    rs.cur_h,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            };
            match resize {
                Ok(()) => {
                    if let Err(e) = rs.blit.rebuild_rtv(&rs.win_device, sc, rs.cur_w, rs.cur_h) {
                        error!("[multiview::window] blit RTV rebuild on resize failed: {e}");
                    }
                }
                Err(e) => error!("[multiview::window] swapchain ResizeBuffers failed: {e}"),
            }
        }
        rs.last_frame = u64::MAX;
        rs.opened_w = 0;
        rs.opened_h = 0;
    }

    let need_open = rs.shared_srv.is_none() || rs.opened_w != rs.cur_w || rs.opened_h != rs.cur_h;
    if need_open && can_open_shared_texture(engine_size, rs.cur_w, rs.cur_h) {
        let (t, s) = open_shared_view_texture(rs.host_ptr, &rs.win_device, rs.view_id);
        if s.is_some() {
            rs.shared_tex = t;
            rs.shared_srv = s;
            rs.opened_w = rs.cur_w;
            rs.opened_h = rs.cur_h;
        }
    }

    let engine_mismatch = engine_size.is_some() && engine_size != Some((rs.cur_w, rs.cur_h));
    if engine_mismatch || rs.shared_srv.is_none() {
        let _ = unsafe { &*rs.host_ptr }.request_frame();
    }

    let frame = unsafe { &*rs.host_ptr }.view_frame_counter(rs.view_id);
    let new_frame = frame != rs.last_frame || need_open;

    let presented = if let Some(srv) = rs.shared_srv.clone() {
        if new_frame {
            rs.last_frame = frame;
            rs.blit.draw(&rs.win_ctx, &srv);
            if let Some(sc) = &rs.swap_chain {
                unsafe {
                    let _ = sc.Present(1, DXGI_PRESENT(0));
                }
            }
            rs.present_count.fetch_add(1, Ordering::Release);
            true
        } else {
            false
        }
    } else {
        rs.blit.clear(&rs.win_ctx);
        if let Some(sc) = &rs.swap_chain {
            unsafe {
                let _ = sc.Present(1, DXGI_PRESENT(0));
            }
        }
        true
    };

    if block_for_frame {
        if !presented {
            let frame_event = unsafe { &*rs.host_ptr }.view_frame_event(rs.view_id);
            unsafe {
                MsgWaitForMultipleObjects(Some(&[frame_event]), false, 16, QS_ALLINPUT);
            }
        }
        let since = rs.last_present.elapsed();
        if since < std::time::Duration::from_millis(8) {
            std::thread::sleep(std::time::Duration::from_millis(8) - since);
        }
        rs.last_present = std::time::Instant::now();
    }
}

fn run_window_thread(
    host: HostPtr,
    device: SendDevice,
    spec: WindowSpec,
    view_id_out: Arc<AtomicI64>,
    should_close: Arc<AtomicBool>,
    hwnd_out: Arc<AtomicIsize>,
    present_count: Arc<AtomicU64>,
) -> Result<(), FlutterEmbedderError> {
    let host_ptr = host.0;
    let game_device = device.0;

    let pending_resize = Arc::new(AtomicU64::new(0));
    register_satellite_class();
    let keyboard: SharedViewKeyboardState = Arc::new(ViewKeyboardState::new());
    let hwnd = create_satellite_window(
        &spec,
        &should_close,
        host_ptr,
        view_id_out.clone(),
        pending_resize.clone(),
        keyboard.clone(),
    )?;
    hwnd_out.store(hwnd.0 as isize, Ordering::Release);

    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }

    let win_device = create_d3d_device_on_same_adapter(&game_device, false)
        .map_err(|e| FlutterEmbedderError::OperationFailed(format!("window device: {e}")))?;
    let win_ctx = unsafe {
        win_device.GetImmediateContext().map_err(|e| {
            FlutterEmbedderError::OperationFailed(format!("window GetImmediateContext: {e}"))
        })?
    };

    let (client_w, client_h) = {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(hwnd, &mut rc);
        }
        let w = (rc.right - rc.left).max(1) as u32;
        let h = (rc.bottom - rc.top).max(1) as u32;
        (w, h)
    };

    let swap_chain: Option<IDXGISwapChain1> = Some(create_swapchain_for_hwnd(
        &win_device,
        hwnd,
        client_w,
        client_h,
    )?);
    let blit = BlitPipeline::new(
        &win_device,
        swap_chain.as_ref().unwrap(),
        client_w,
        client_h,
    )?;

    let cur_dpr = spec
        .pixel_ratio
        .unwrap_or_else(|| dpi_scale_for_window(hwnd));
    let auto_dpr = spec.pixel_ratio.is_none();

    let host_ref = unsafe { &mut *host_ptr };
    let view_id = host_ref.add_window_view(&game_device, hwnd, client_w, client_h, cur_dpr)?;
    view_id_out.store(view_id, Ordering::Release);
    register_view_keyboard_state(view_id, keyboard.clone());
    unsafe { (*host_ptr).send_view_focus(view_id, true) };
    register_window_controls(
        view_id,
        WindowControls {
            should_close: should_close.clone(),
            hwnd: hwnd_out.clone(),
            present_count: present_count.clone(),
        },
    );
    info!("[multiview::window] spawned window for view {view_id}");

    let (shared_tex, shared_srv) = open_shared_view_texture(host_ptr, &win_device, view_id);

    let mut render_state = Box::new(RenderState {
        hwnd,
        host_ptr,
        view_id,
        win_device: win_device.clone(),
        win_ctx: win_ctx.clone(),
        swap_chain,
        blit,
        shared_tex,
        shared_srv,
        cur_w: client_w,
        cur_h: client_h,
        target_w: client_w,
        target_h: client_h,
        opened_w: client_w,
        opened_h: client_h,
        last_frame: u64::MAX,
        cur_dpr,
        auto_dpr,
        pending_resize: pending_resize.clone(),
        last_present: std::time::Instant::now(),
        present_count,
    });
    let render_state_ptr: *mut RenderState = &mut *render_state;
    RENDER_STATE.with(|c| c.set(render_state_ptr));

    let mut msg = MSG::default();
    loop {
        WINDOW_LOOP_ITERS.fetch_add(1, Ordering::Relaxed);
        if should_close.load(Ordering::Acquire) {
            break;
        }
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    should_close.store(true, Ordering::Release);
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        if should_close.load(Ordering::Acquire) {
            break;
        }

        pump_frame(&mut render_state, true);
    }

    RENDER_STATE.with(|c| c.set(std::ptr::null_mut()));
    drop(render_state);

    let host_ref = unsafe { &mut *host_ptr };
    if let Err(e) = host_ref.remove_view(view_id) {
        error!("[multiview::window] remove_view on close failed: {e}");
    }
    unregister_view_keyboard_state(view_id);
    unregister_window_controls(view_id);
    view_id_out.store(-1, Ordering::Release);
    hwnd_out.store(0, Ordering::Release);
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    info!("[multiview::window] window thread for view {view_id} exited");
    Ok(())
}

fn open_shared_view_texture(
    host_ptr: *mut FlutterOverlay,
    win_device: &ID3D11Device,
    view_id: FlutterViewId,
) -> (Option<ID3D11Texture2D>, Option<ID3D11ShaderResourceView>) {
    let host = unsafe { &*host_ptr };
    let Some((handle, _w, _h)) = host.view_shared_handle(view_id) else {
        return (None, None);
    };
    if handle.0.is_null() {
        return (None, None);
    }

    unsafe {
        let mut opened: Option<ID3D11Texture2D> = None;
        if win_device.OpenSharedResource(handle, &mut opened).is_err() {
            return (None, None);
        }
        let Some(tex) = opened else {
            return (None, None);
        };

        let mut srv = None;
        if win_device
            .CreateShaderResourceView(&tex, None, Some(&mut srv))
            .is_err()
        {
            return (None, None);
        }

        (Some(tex), srv)
    }
}

const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_ENTERSIZEMOVE: u32 = 0x0231;
const WM_EXITSIZEMOVE: u32 = 0x0232;
const WM_APP_MINIMIZE: u32 = 0x8000 + 1;
const WM_APP_MAXIMIZE: u32 = 0x8000 + 2;
const WM_APP_RESTORE: u32 = 0x8000 + 3;
const WM_APP_CLOSE: u32 = 0x8000 + 4;
const SATELLITE_CLASS: PCWSTR = windows::core::w!("FlutterSatelliteWindow");
static CLASS_REGISTERED: std::sync::Once = std::sync::Once::new();

fn register_satellite_class() {
    CLASS_REGISTERED.call_once(|| unsafe {
        let hinst = GetModuleHandleW(None).expect("GetModuleHandleW failed");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(satellite_wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: SATELLITE_CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            error!("[multiview::window] RegisterClassW failed");
        }
    });
}

struct WindowProcState {
    should_close: Arc<AtomicBool>,

    host_ptr: *mut FlutterOverlay,

    view_id: Arc<AtomicI64>,

    pending_resize: Arc<AtomicU64>,

    keyboard: SharedViewKeyboardState,

    mouse_added: std::cell::Cell<bool>,

    restore_rect: std::cell::Cell<Option<RECT>>,
    maximized: std::cell::Cell<bool>,

    loop_iters_at_enter_size: std::cell::Cell<u64>,
}

static WINDOW_LOOP_ITERS: AtomicU64 = AtomicU64::new(0);

struct PointerInput {
    phase: FlutterPointerPhase,
    x: f64,
    y: f64,
    buttons: i64,
    scroll_dx: f64,
    scroll_dy: f64,
}

unsafe fn send_window_pointer(
    host_ptr: *mut FlutterOverlay,
    view_id: FlutterViewId,
    input: PointerInput,
) {
    let PointerInput {
        phase,
        x,
        y,
        buttons,
        scroll_dx,
        scroll_dy,
    } = input;
    let host = unsafe { &*host_ptr };
    let engine = host.engine.0;
    if engine.is_null() {
        return;
    }
    let dll = &host.engine_dll;
    let event = FlutterPointerEvent {
        struct_size: std::mem::size_of::<FlutterPointerEvent>(),
        phase,
        timestamp: unsafe { (dll.FlutterEngineGetCurrentTime)() } as usize / 1000,
        x,
        y,
        device: 0,
        signal_kind: if scroll_dx != 0.0 || scroll_dy != 0.0 {
            FlutterPointerSignalKind_kFlutterPointerSignalKindScroll
        } else {
            FlutterPointerSignalKind_kFlutterPointerSignalKindNone
        },
        scroll_delta_x: scroll_dx,
        scroll_delta_y: scroll_dy,
        device_kind: FlutterPointerDeviceKind_kFlutterPointerDeviceKindMouse,
        buttons,
        pan_x: 0.0,
        pan_y: 0.0,
        scale: 1.0,
        rotation: 0.0,
        view_id,
        pressure: 0.0,
        pressure_min: 0.0,
        pressure_max: 0.0,
    };
    if !x.is_finite() || !y.is_finite() || view_id < 0 {
        return;
    }
    let _ = unsafe { (dll.FlutterEngineSendPointerEvent)(engine, &event as *const _, 1) };
}

fn create_satellite_window(
    spec: &WindowSpec,
    should_close: &Arc<AtomicBool>,
    host_ptr: *mut FlutterOverlay,
    view_id: Arc<AtomicI64>,
    pending_resize: Arc<AtomicU64>,
    keyboard: SharedViewKeyboardState,
) -> Result<HWND, FlutterEmbedderError> {
    let title: Vec<u16> = spec
        .title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let state = Box::new(WindowProcState {
        should_close: should_close.clone(),
        host_ptr,
        view_id,
        pending_resize,
        keyboard,
        mouse_added: std::cell::Cell::new(false),
        restore_rect: std::cell::Cell::new(None),
        maximized: std::cell::Cell::new(false),
        loop_iters_at_enter_size: std::cell::Cell::new(0),
    });
    let state_ptr = Box::into_raw(state);

    let style = window_style_bits(spec.style);

    let hwnd = unsafe {
        let hinst = GetModuleHandleW(None)
            .map_err(|e| FlutterEmbedderError::OperationFailed(format!("GetModuleHandleW: {e}")))?;
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            SATELLITE_CLASS,
            PCWSTR(title.as_ptr()),
            style | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            spec.width as i32,
            spec.height as i32,
            None,
            None,
            Some(hinst.into()),
            Some(state_ptr as *const _),
        )
    };

    match hwnd {
        Ok(h) if !h.0.is_null() => Ok(h),
        _ => {
            let _ = unsafe { Box::from_raw(state_ptr) };
            Err(FlutterEmbedderError::OperationFailed(
                "CreateWindowExW failed for satellite window".to_string(),
            ))
        }
    }
}

fn window_style_bits(style: WindowStyle) -> WINDOW_STYLE {
    if style.decorated {
        let mut s = WS_OVERLAPPEDWINDOW;
        if !style.resizable {
            s &= !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        }
        s
    } else {
        let mut s = WS_POPUP | WS_CAPTION | WS_MINIMIZEBOX | WS_MAXIMIZEBOX | WS_THICKFRAME;
        if !style.resizable {
            s &= !(WS_THICKFRAME | WS_MAXIMIZEBOX);
        }
        s
    }
}

fn dpi_scale_for_window(hwnd: HWND) -> f64 {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 { 1.0 } else { dpi as f64 / 96.0 }
}

extern "system" fn satellite_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_NCCREATE => {
                if let Some(cs) = (lparam.0 as *const CREATESTRUCTW).as_ref() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_CLOSE | WM_APP_CLOSE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    state.should_close.store(true, Ordering::Release);
                }

                LRESULT(0)
            }
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            WM_NCHITTEST => {
                let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
                let resizable = (style & WS_THICKFRAME) != WINDOW_STYLE(0);
                let maximized = {
                    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                    ptr.as_ref().map(|s| s.maximized.get()).unwrap_or(false)
                };
                if !resizable || maximized {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let mut rc = RECT::default();
                let _ = GetWindowRect(hwnd, &mut rc);
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let dpi = GetDpiForWindow(hwnd);
                let border = (8.0 * dpi as f32 / 96.0) as i32;
                let left = x < rc.left + border;
                let right = x >= rc.right - border;
                let top = y < rc.top + border;
                let bottom = y >= rc.bottom - border;
                let ht = if top && left {
                    HTTOPLEFT
                } else if top && right {
                    HTTOPRIGHT
                } else if bottom && left {
                    HTBOTTOMLEFT
                } else if bottom && right {
                    HTBOTTOMRIGHT
                } else if left {
                    HTLEFT
                } else if right {
                    HTRIGHT
                } else if top {
                    HTTOP
                } else if bottom {
                    HTBOTTOM
                } else {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                };
                LRESULT(ht as isize)
            }
            WM_APP_MINIMIZE => {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                LRESULT(0)
            }
            WM_APP_MAXIMIZE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref()
                    && !state.maximized.get()
                {
                    let mut wr = RECT::default();
                    if GetWindowRect(hwnd, &mut wr).is_ok() {
                        state.restore_rect.set(Some(wr));
                    }
                    if let Some(work) = monitor_work_area(hwnd) {
                        state.maximized.set(true);
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            work.left,
                            work.top,
                            work.right - work.left,
                            work.bottom - work.top,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                        );
                        let w = (work.right - work.left).max(1) as u64;
                        let h = (work.bottom - work.top).max(1) as u64;
                        state.pending_resize.store((w << 32) | h, Ordering::Release);
                    }
                }
                LRESULT(0)
            }
            WM_APP_RESTORE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref()
                    && state.maximized.get()
                    && let Some(r) = state.restore_rect.get()
                {
                    state.maximized.set(false);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                    );
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let w = (rc.right - rc.left).max(1) as u64;
                    let h = (rc.bottom - rc.top).max(1) as u64;
                    state.pending_resize.store((w << 32) | h, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_ENTERSIZEMOVE => {
                let _ = SetTimer(Some(hwnd), MODAL_PUMP_TIMER_ID, 8, None);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_EXITSIZEMOVE => {
                let _ = KillTimer(Some(hwnd), MODAL_PUMP_TIMER_ID);
                run_pump(hwnd);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_TIMER if wparam.0 == MODAL_PUMP_TIMER_ID => {
                run_pump(hwnd);
                LRESULT(0)
            }
            WM_SIZE => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    let w = (lparam.0 & 0xFFFF) as u64;
                    let h = ((lparam.0 >> 16) & 0xFFFF) as u64;
                    if w > 0 && h > 0 {
                        state.pending_resize.store((w << 32) | h, Ordering::Release);
                    }
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let suggested = lparam.0 as *const RECT;
                if let Some(r) = suggested.as_ref() {
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    let mut rc = RECT::default();
                    let _ = GetClientRect(hwnd, &mut rc);
                    let w = (rc.right - rc.left).max(1) as u64;
                    let h = (rc.bottom - rc.top).max(1) as u64;
                    state.pending_resize.store((w << 32) | h, Ordering::Release);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowProcState;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_SETFOCUS | WM_KILLFOCUS => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    let view_id = state.view_id.load(Ordering::Acquire);
                    if view_id >= 0 {
                        (*state.host_ptr).send_view_focus(view_id, msg == WM_SETFOCUS);
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP | WM_CHAR => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                let view_id = ptr
                    .as_ref()
                    .map(|s| s.view_id.load(Ordering::Acquire))
                    .unwrap_or(-2);
                if let Some(state) = ptr.as_ref()
                    && view_id >= 0
                {
                    let overlay = &*state.host_ptr;
                    handle_keyboard_event_for_view(overlay, &state.keyboard, msg, wparam, lparam);
                    if msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP {
                        return DefWindowProcW(hwnd, msg, wparam, lparam);
                    }
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NCLBUTTONDOWN => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    let view_id = state.view_id.load(Ordering::Acquire);
                    if view_id >= 0 && state.mouse_added.get() {
                        state.mouse_added.set(false);
                        send_window_pointer(
                            state.host_ptr,
                            view_id,
                            PointerInput {
                                phase: FlutterPointerPhase_kRemove,
                                x: 0.0,
                                y: 0.0,
                                buttons: 0,
                                scroll_dx: 0.0,
                                scroll_dy: 0.0,
                            },
                        );
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_LBUTTONDOWN => {
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowProcState;
                if let Some(state) = ptr.as_ref() {
                    let view_id = state.view_id.load(Ordering::Acquire);
                    if view_id >= 0 {
                        (*state.host_ptr).send_view_focus(view_id, true);
                    }
                }
                if handle_window_mouse(hwnd, msg, wparam, lparam) {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            _ => {
                if handle_window_mouse(hwnd, msg, wparam, lparam) {
                    LRESULT(0)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
        }
    }
}

unsafe fn handle_window_mouse(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> bool {
    const BTN_LEFT: i64 = 1 << 0;
    const BTN_RIGHT: i64 = 1 << 1;
    const BTN_MIDDLE: i64 = 1 << 2;

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowProcState;
    let Some(state) = (unsafe { state_ptr.as_ref() }) else {
        return false;
    };
    let view_id = state.view_id.load(Ordering::Acquire);
    if view_id < 0 {
        return false;
    }

    let x = (lparam.0 & 0xFFFF) as i16 as f64;
    let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;

    let mk = wparam.0 as u32;
    let mut buttons: i64 = 0;
    if mk & 0x0001 != 0 {
        buttons |= BTN_LEFT;
    }
    if mk & 0x0002 != 0 {
        buttons |= BTN_RIGHT;
    }
    if mk & 0x0010 != 0 {
        buttons |= BTN_MIDDLE;
    }

    match msg {
        WM_MOUSEMOVE => {
            if !state.mouse_added.get() {
                state.mouse_added.set(true);
                unsafe {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd.0.cast(),
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    send_window_pointer(
                        state.host_ptr,
                        view_id,
                        PointerInput {
                            phase: FlutterPointerPhase_kAdd,
                            x,
                            y,
                            buttons: 0,
                            scroll_dx: 0.0,
                            scroll_dy: 0.0,
                        },
                    );
                }
            }
            let phase = if buttons != 0 {
                FlutterPointerPhase_kMove
            } else {
                FlutterPointerPhase_kHover
            };
            unsafe {
                send_window_pointer(
                    state.host_ptr,
                    view_id,
                    PointerInput {
                        phase,
                        x,
                        y,
                        buttons,
                        scroll_dx: 0.0,
                        scroll_dy: 0.0,
                    },
                );
            }
            true
        }
        WM_MOUSELEAVE => {
            if state.mouse_added.get() {
                state.mouse_added.set(false);
                unsafe {
                    send_window_pointer(
                        state.host_ptr,
                        view_id,
                        PointerInput {
                            phase: FlutterPointerPhase_kRemove,
                            x,
                            y,
                            buttons: 0,
                            scroll_dx: 0.0,
                            scroll_dy: 0.0,
                        },
                    );
                }
            }
            true
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let b = match msg {
                WM_LBUTTONDOWN => BTN_LEFT,
                WM_RBUTTONDOWN => BTN_RIGHT,
                _ => BTN_MIDDLE,
            };
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            LAST_POINTER_DOWN_MS.store(now_ms, Ordering::Release);
            unsafe {
                send_window_pointer(
                    state.host_ptr,
                    view_id,
                    PointerInput {
                        phase: FlutterPointerPhase_kDown,
                        x,
                        y,
                        buttons: b,
                        scroll_dx: 0.0,
                        scroll_dy: 0.0,
                    },
                );
            }
            true
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            unsafe {
                send_window_pointer(
                    state.host_ptr,
                    view_id,
                    PointerInput {
                        phase: FlutterPointerPhase_kUp,
                        x,
                        y,
                        buttons: 0,
                        scroll_dx: 0.0,
                        scroll_dy: 0.0,
                    },
                );
            }
            true
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as f64 / 120.0;
            unsafe {
                send_window_pointer(
                    state.host_ptr,
                    view_id,
                    PointerInput {
                        phase: FlutterPointerPhase_kHover,
                        x,
                        y,
                        buttons: 0,
                        scroll_dx: 0.0,
                        scroll_dy: -delta * 50.0,
                    },
                );
            }
            true
        }
        _ => false,
    }
}

fn create_swapchain_for_hwnd(
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1, FlutterEmbedderError> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|e| FlutterEmbedderError::OperationFailed(format!("IDXGIDevice cast: {e}")))?;
    let adapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|e| FlutterEmbedderError::OperationFailed(format!("GetAdapter: {e}")))?;
    let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
        .map_err(|e| FlutterEmbedderError::OperationFailed(format!("GetParent factory: {e}")))?;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };

    unsafe {
        factory
            .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
            .map_err(|e| {
                FlutterEmbedderError::OperationFailed(format!("CreateSwapChainForHwnd: {e}"))
            })
    }
}

struct BlitPipeline {
    rtv: Option<ID3D11RenderTargetView>,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    viewport: D3D11_VIEWPORT,
}

impl BlitPipeline {
    fn new(
        device: &ID3D11Device,
        swap_chain: &IDXGISwapChain1,
        width: u32,
        height: u32,
    ) -> Result<Self, FlutterEmbedderError> {
        let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }
            .map_err(|e| FlutterEmbedderError::OperationFailed(format!("GetBuffer: {e}")))?;
        let mut rtv = None;
        unsafe {
            device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))
                .map_err(|e| {
                    FlutterEmbedderError::OperationFailed(format!("CreateRenderTargetView: {e}"))
                })?;
        }
        let rtv = rtv.unwrap();

        let vs = {
            let bytes = include_bytes!("../d3d11_compositor/shaders/fullscreen_quad_vs.cso");
            let mut s = None;
            unsafe {
                device
                    .CreateVertexShader(bytes, None, Some(&mut s))
                    .map_err(|e| {
                        FlutterEmbedderError::OperationFailed(format!("CreateVertexShader: {e}"))
                    })?;
            }
            s.unwrap()
        };
        let ps = {
            let bytes = include_bytes!("../d3d11_compositor/shaders/blit_flip_y_ps.cso");
            let mut s = None;
            unsafe {
                device
                    .CreatePixelShader(bytes, None, Some(&mut s))
                    .map_err(|e| {
                        FlutterEmbedderError::OperationFailed(format!("CreatePixelShader: {e}"))
                    })?;
            }
            s.unwrap()
        };

        let sampler = {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
                ..Default::default()
            };
            let mut s = None;
            unsafe {
                device
                    .CreateSamplerState(&desc, Some(&mut s))
                    .map_err(|e| {
                        FlutterEmbedderError::OperationFailed(format!("CreateSamplerState: {e}"))
                    })?;
            }
            s.unwrap()
        };

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };

        Ok(Self {
            rtv: Some(rtv),
            vs,
            ps,
            sampler,
            viewport,
        })
    }

    fn release_rtv(&mut self) {
        self.rtv = None;
    }

    fn rebuild_rtv(
        &mut self,
        device: &ID3D11Device,
        swap_chain: &IDXGISwapChain1,
        width: u32,
        height: u32,
    ) -> Result<(), FlutterEmbedderError> {
        let back_buffer: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }
            .map_err(|e| FlutterEmbedderError::OperationFailed(format!("GetBuffer: {e}")))?;
        let mut rtv = None;
        unsafe {
            device
                .CreateRenderTargetView(&back_buffer, None, Some(&mut rtv))
                .map_err(|e| {
                    FlutterEmbedderError::OperationFailed(format!("CreateRenderTargetView: {e}"))
                })?;
        }
        self.rtv = rtv;
        self.viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        Ok(())
    }

    fn draw(&mut self, context: &ID3D11DeviceContext, srv: &ID3D11ShaderResourceView) {
        let Some(rtv) = self.rtv.clone() else {
            return;
        };
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            context.RSSetViewports(Some(&[self.viewport]));
            context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 0.0]);

            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            context.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            context.Draw(4, 0);
            context.PSSetShaderResources(0, Some(&[None]));
        }
    }

    fn clear(&self, context: &ID3D11DeviceContext) {
        let Some(rtv) = self.rtv.clone() else {
            return;
        };
        unsafe {
            context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            context.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
        }
    }
}
