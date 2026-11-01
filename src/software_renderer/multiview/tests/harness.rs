//! Reusable real-engine test harness for multi-view integration and stress
//! tests. It mirrors the game's setup one-to-one: a real D3D11 device, a real
//! `FlutterOverlay` running the engine through ANGLE, real satellite windows
//! spawned on their own render threads via `spawn_window`, and a per-frame
//! `tick()` drive that matches the game's `render_ui()` path.
//!
//! Tests build an [`EngineHarness`] once, then spawn/resize/close satellite
//! windows through it. The harness must be driven (`tick`/`pump`) for the engine
//! frame loop to advance, exactly as the game's present hook drives it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIDevice, IDXGIFactory, IDXGISwapChain,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CreateWindowExW, DefWindowProcW, GetClientRect, RegisterClassW, SWP_NOMOVE,
    SWP_NOZORDER, SetWindowPos, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{Interface, PCWSTR};

use crate::software_renderer::api::{OverlayCreateParams, RendererPreference, RendererType};
use crate::software_renderer::multiview::tests::crash_report::install as install_crash_report;
use crate::software_renderer::multiview::window::{SatelliteWindow, WindowSpec, WindowStyle};
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;

/// Prints a flushed progress marker. Goes to stderr; the test task tees stdout
/// so these stream live.
pub fn step(msg: &str) {
    eprintln!("STEP: {msg}");
}

/// Initializes `env_logger` at Debug level. Idempotent across tests in one
/// process (`try_init` only succeeds once).
pub fn init_test_logging() {
    use env_logger::builder as env_logger_builder;
    use log::LevelFilter;
    install_crash_report();
    let _ = env_logger_builder()
        .is_test(false)
        .filter_level(LevelFilter::Debug)
        .try_init();
}

/// Path to the `flutter assemble` bundle built by `build.rs` under the
/// `engine-tests` feature.
pub fn test_bundle_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("flutter_artifacts")
        .join("test_libs")
        .join("test_app")
        .join("build")
}

/// True when the engine bundle + staged DLLs exist. Tests should early-return
/// (skip) when this is false rather than hard-fail, so a checkout without the
/// built bundle does not break unrelated runs.
pub fn bundle_is_present() -> bool {
    let bundle = test_bundle_root();
    if !bundle.join("flutter_engine.dll").is_file() {
        return false;
    }
    if release_build_requested() {
        bundle.join("windows").join("app.so").is_file()
    } else {
        bundle.join("flutter_assets").join("kernel_blob.bin").is_file()
    }
}

pub fn release_build_requested() -> bool {
    std::env::var("FLUTTER_TEST_RELEASE")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Creates a tiny hidden top-level window for use as the host overlay's render
/// target window.
pub fn create_hidden_window() -> HWND {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let class = windows::core::w!("EmbedderTestWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            PCWSTR::null(),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            64,
            64,
            None,
            None,
            Some(hinst),
            None,
        )
        .unwrap()
    }
}

/// Creates a D3D11 device + swapchain for `hwnd`. Tries the hardware driver
/// first (ANGLE needs a hardware-capable adapter for cross-device sharing) and
/// falls back to WARP.
pub fn create_device_and_swapchain(
    hwnd: HWND,
    width: u32,
    height: u32,
) -> (ID3D11Device, IDXGISwapChain) {
    unsafe {
        let mut device: Option<ID3D11Device> = None;
        let hw = D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        );
        if hw.is_err() {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("device creation failed");
        }
        let device = device.unwrap();

        let dxgi_device: IDXGIDevice = device.cast().unwrap();
        let adapter = dxgi_device.GetAdapter().unwrap();
        let factory: IDXGIFactory = adapter.GetParent().unwrap();

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            OutputWindow: hwnd,
            Windowed: true.into(),
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let mut swap_chain: Option<IDXGISwapChain> = None;
        factory
            .CreateSwapChain(&device, &desc, &mut swap_chain)
            .ok()
            .expect("swapchain creation failed");
        (device, swap_chain.unwrap())
    }
}

/// A running real-engine overlay plus everything needed to spawn satellite
/// windows and drive the engine frame loop. Mirrors the game's overlay-in-a-box
/// living inside the manager: the overlay is boxed so its address is stable for
/// the satellite window threads that hold a raw `*mut FlutterOverlay`.
pub struct EngineHarness {
    overlay: Box<FlutterOverlay>,
    pub device: ID3D11Device,
    pub host_context: ID3D11DeviceContext,
    _hwnd: HWND,
    _swap_chain: IDXGISwapChain,
}

// The harness owns COM/raw-pointer state the auto traits reject, but every test
// touches it only while holding `SHARED_ENGINE`'s lock and the test runner is
// single-threaded, so access is serialized.
unsafe impl Send for EngineHarness {}

/// Process-wide engine, created at most once. `FlutterOverlay::create` brings up
/// the ANGLE/EGL singletons on first success; a second overlay in the same
/// process does not get the OpenGL renderer, so every engine test must share one
/// long-lived overlay — which also matches the game (one overlay, many satellite
/// windows churned over its lifetime).
static SHARED_ENGINE: std::sync::OnceLock<Option<std::sync::Mutex<EngineHarness>>> =
    std::sync::OnceLock::new();

/// Runs `f` against the shared engine, or returns `None` (without calling `f`) if
/// the engine is unavailable in this environment — the caller should treat that
/// as a skip. Serializes all engine access through one mutex.
pub fn with_shared_engine<R>(f: impl FnOnce(&mut EngineHarness) -> R) -> Option<R> {
    let slot = SHARED_ENGINE.get_or_init(|| {
        if !bundle_is_present() {
            step("engine bundle missing — engine tests will skip");
            return None;
        }
        EngineHarness::create("shared-test-engine").map(std::sync::Mutex::new)
    });
    let mutex = slot.as_ref()?;
    let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
    Some(f(&mut guard))
}

impl EngineHarness {
    /// Builds the harness, or returns `None` if the engine could not initialize
    /// the OpenGL/ANGLE renderer in this environment (the caller should skip).
    /// Assumes [`bundle_is_present`] is true.
    pub fn create(name: &str) -> Option<Self> {
        Self::create_with_dart_args(name, None)
    }

    /// Like [`create`](Self::create) but passes Dart entrypoint args to the test
    /// app's `main(List<String> args)`. Because the ANGLE renderer is a
    /// per-process singleton, a harness built this way only gets the OpenGL
    /// renderer if no other engine has been created in the process yet, so tests
    /// that need custom Dart args must run isolated (their own process / filtered
    /// run), not alongside the shared engine.
    pub fn create_with_dart_args(name: &str, dart_args: Option<Vec<String>>) -> Option<Self> {
        Self::create_with_renderer(
            name,
            dart_args,
            None,
            RendererPreference::OpenGL,
            RendererType::OpenGL,
        )
    }

    /// Builds a harness asking for `preference`, or returns `None` (skip) when the
    /// overlay did not end up on `expected`, i.e. that renderer is unavailable in
    /// this environment.
    pub fn create_with_renderer(
        name: &str,
        dart_args: Option<Vec<String>>,
        engine_args: Option<Vec<String>>,
        preference: RendererPreference,
        expected: RendererType,
    ) -> Option<Self> {
        let bundle = test_bundle_root();
        let hwnd = create_hidden_window();
        let (device, swap_chain) = create_device_and_swapchain(hwnd, 256, 256);

        let params = OverlayCreateParams {
            name: name.to_string(),
            x: 0,
            y: 0,
            width: 256,
            height: 256,
            flutter_data_dir: bundle,
            dart_entrypoint_args: dart_args,
            engine_args,
            renderer: preference,
        };

        let mut overlay = match FlutterOverlay::create(params, &device, &swap_chain) {
            Ok(o) => o,
            Err(e) => {
                step(&format!("engine create failed ({expected:?} unavailable headless?): {e}"));
                return None;
            }
        };
        if overlay.renderer_type != expected {
            step(&format!(
                "{expected:?} renderer not active (got {:?}); skipping",
                overlay.renderer_type
            ));
            return None;
        }
        overlay.set_visibility(true);
        let host_context = unsafe {
            device
                .GetImmediateContext()
                .expect("host device has no immediate context")
        };

        Some(Self {
            overlay,
            device,
            host_context,
            _hwnd: hwnd,
            _swap_chain: swap_chain,
        })
    }

    /// The overlay under test.
    pub fn overlay(&self) -> &FlutterOverlay {
        &self.overlay
    }

    /// Mutable access to the overlay under test (resize, visibility, add_view).
    pub fn overlay_mut(&mut self) -> &mut FlutterOverlay {
        &mut self.overlay
    }

    /// Advances the engine one tick — the game's per-frame `render_ui()` drive.
    pub fn tick(&self) {
        self.overlay.tick(&self.host_context);
    }

    /// Sets the main overlay's visibility (view 0). The game toggles this when
    /// the in-game UI is hidden.
    pub fn set_main_visibility(&mut self, visible: bool) {
        self.overlay.set_visibility(visible);
    }

    /// Whether the main overlay is currently visible.
    pub fn main_is_visible(&self) -> bool {
        self.overlay.is_visible()
    }

    /// Schedules an engine frame independent of overlay visibility.
    pub fn request_frame(&self) {
        let _ = self.overlay.request_frame();
    }

    /// Drives the engine until the view's shared texture reaches `target`,
    /// advancing the engine with both `tick()` (no-op while the main overlay is
    /// hidden) and `request_frame()` (visibility-independent) so the satellite
    /// keeps progressing even when the main overlay is invisible.
    pub fn wait_for_texture_size_independent(
        &self,
        view_id: i64,
        target: (u32, u32),
        timeout: Duration,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            self.tick();
            self.request_frame();
            if let Some((_, w, h)) = self.overlay.view_shared_handle(view_id)
                && (w, h) == target
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        false
    }

    /// The host overlay's swapchain. Shares its DXGI vtable with every other
    /// swapchain in the process (including satellite windows), so it is a valid
    /// target for installing a process-global vtable hook in tests.
    pub fn host_swapchain(&self) -> IDXGISwapChain {
        self._swap_chain.clone()
    }

    /// Drives the engine for `duration`, ~60 ticks/second.
    pub fn pump(&self, duration: Duration) {
        let start = Instant::now();
        while start.elapsed() < duration {
            self.tick();
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    /// Reproduces the game's `ResizeBuffers` hook
    /// (`modloader::hk_resize_buffers_hook`): it resizes the host backbuffer
    /// swapchain in place and then resizes the main overlay (view 0) to match,
    /// the same path `om.resize_flutter_overlays` → `handle_resize` →
    /// `handle_window_resize` takes. Stress tests call this concurrently with
    /// satellite-window resizes to exercise the real in-game resize race.
    pub fn resize_host(&mut self, width: u32, height: u32) {
        unsafe {
            let r = self._swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
            );
            if let Err(e) = r {
                step(&format!("host ResizeBuffers failed: {e}"));
                return;
            }
        }
        let swap_chain = self._swap_chain.clone();
        self.overlay
            .handle_window_resize(0, 0, width, height, &swap_chain);
    }

    /// Spawns a real satellite window (OS window + render thread), exactly as the
    /// game does via `spawn_window_for_overlay`.
    pub fn spawn(&mut self, title: &str, width: u32, height: u32) -> SatelliteWindow {
        let spec = WindowSpec {
            title: title.to_string(),
            width,
            height,
            pixel_ratio: Some(1.0),
            style: WindowStyle {
                decorated: true,
                resizable: true,
            },
        };
        let device = self.device.clone();
        unsafe { self.overlay.spawn_window(&device, spec) }.expect("spawn_window failed")
    }

    /// Drives the engine until the window publishes its view id (set on the
    /// window thread once its HWND + swapchain + AddView complete).
    pub fn wait_for_view_id(&self, window: &SatelliteWindow, timeout: Duration) -> i64 {
        let start = Instant::now();
        while start.elapsed() < timeout {
            self.tick();
            let id = window.view_id();
            if id > 0 {
                return id;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        window.view_id()
    }

    /// The view's reported backing texture size (the "renderable" signal a window
    /// thread polls), or None if the view is gone.
    pub fn view_texture_size(&self, view_id: i64) -> Option<(u32, u32)> {
        self.overlay.view_shared_handle(view_id).map(|(_, w, h)| (w, h))
    }

    /// The engine's presented-frame counter for the view. Increments only after
    /// the engine has rendered AND presented a frame for that view.
    pub fn view_frame_counter(&self, view_id: i64) -> u64 {
        self.overlay.view_frame_counter(view_id)
    }

    /// Drives the engine until the view's shared texture reaches `target`.
    pub fn wait_for_texture_size(
        &self,
        view_id: i64,
        target: (u32, u32),
        timeout: Duration,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            self.tick();
            if let Some((_, w, h)) = self.overlay.view_shared_handle(view_id)
                && (w, h) == target
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        false
    }

    /// Closes a satellite window cleanly: requests close, then keeps ticking so
    /// the window thread's `remove_view` engine task runs and the join does not
    /// time out.
    pub fn close_window(&self, window: SatelliteWindow) {
        let controls = window.controls();
        controls.close();
        let start = Instant::now();
        while controls.hwnd_value() != 0 && start.elapsed() < Duration::from_secs(3) {
            self.tick();
            std::thread::sleep(Duration::from_millis(16));
        }
        window.close();
    }
}

/// Resizes a top-level window so its *client* area is approximately `w`x`h`,
/// matching how a user drag-resize lands a target client size.
pub fn resize_window_client_area(hwnd: HWND, w: u32, h: u32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: w as i32,
        bottom: h as i32,
    };
    unsafe {
        let _ = AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE(0));
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
}

/// Returns the current client-area size of a window.
pub fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    (
        (rc.right - rc.left).max(1) as u32,
        (rc.bottom - rc.top).max(1) as u32,
    )
}

/// Returns the satellite window's HWND from its controls (0 if not yet created).
pub fn window_hwnd(window: &SatelliteWindow) -> HWND {
    HWND(window.controls().hwnd_value() as *mut _)
}

/// Frames the window thread has actually blitted+presented. A satellite that goes
/// black after resize stops advancing this.
pub fn window_present_count(window: &SatelliteWindow) -> u64 {
    window.controls().present_count()
}
