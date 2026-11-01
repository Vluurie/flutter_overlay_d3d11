use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{DXGI_PRESENT, DXGI_SWAP_CHAIN_FLAG, IDXGISwapChain};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
    TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{HSTRING, w};

use crate::software_renderer::api::{OverlayCreateParams, RendererPreference, RendererType};
use crate::software_renderer::multiview::tests::harness::{
    bundle_is_present, create_device_and_swapchain, init_test_logging, release_build_requested,
    test_bundle_root,
};
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::vk_renderer::tests::support::{
    APP_BACKGROUND_BGRA, assert_bgra, bgra_matches, read_pixel, vulkan_path_is_available,
};

const INITIAL_CLIENT_SIZE: (u32, u32) = (900, 600);
const DEFAULT_WINDOW_SECONDS: u64 = 5;

static CLOSE_REQUESTED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_CLOSE {
        CLOSE_REQUESTED.store(true, Ordering::Release);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn create_visible_window(title: &str) -> HWND {
    unsafe {
        let instance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
        let class_name = w!("VulkanRendererVisibleTestWindow");
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&class);

        let mut frame = RECT {
            left: 0,
            top: 0,
            right: INITIAL_CLIENT_SIZE.0 as i32,
            bottom: INITIAL_CLIENT_SIZE.1 as i32,
        };
        let _ = AdjustWindowRect(&mut frame, WS_OVERLAPPEDWINDOW, false);

        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            &HSTRING::from(title),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            frame.right - frame.left,
            frame.bottom - frame.top,
            None,
            None,
            Some(instance),
            None,
        )
        .unwrap()
    }
}

fn pump_window_messages() {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rect);
    }
    (
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    )
}

fn window_seconds() -> u64 {
    std::env::var("FLUTTER_VK_WINDOW_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_WINDOW_SECONDS)
}

/// Copies the overlay's frame into the backbuffer and presents it.
fn present_overlay(
    overlay: &FlutterOverlay,
    context: &ID3D11DeviceContext,
    swap_chain: &IDXGISwapChain,
    backbuffer_size: (u32, u32),
) {
    if overlay.get_dimensions() != backbuffer_size {
        return;
    }
    unsafe {
        if let Ok(backbuffer) = swap_chain.GetBuffer::<ID3D11Texture2D>(0) {
            context.CopyResource(&backbuffer, &overlay.texture);
        }
        let _ = swap_chain.Present(1, DXGI_PRESENT(0));
    }
}

fn show_flutter_app_in_window(impeller: bool) {
    init_test_logging();
    if !bundle_is_present() {
        eprintln!("SKIP: engine bundle missing");
        return;
    }
    if !vulkan_path_is_available() {
        return;
    }

    let backend = if impeller { "Impeller" } else { "Skia" };
    let aot_requested = release_build_requested();
    let mode = if aot_requested { "AOT" } else { "JIT" };
    CLOSE_REQUESTED.store(false, Ordering::Release);
    let hwnd = create_visible_window(&format!(
        "Flutter on Vulkan ({backend}, {mode}) - integration test"
    ));
    let (device, swap_chain) =
        create_device_and_swapchain(hwnd, INITIAL_CLIENT_SIZE.0, INITIAL_CLIENT_SIZE.1);
    let context = unsafe { device.GetImmediateContext() }.unwrap();

    let params = OverlayCreateParams {
        name: format!("vulkan-visible-{backend}"),
        x: 0,
        y: 0,
        width: INITIAL_CLIENT_SIZE.0,
        height: INITIAL_CLIENT_SIZE.1,
        flutter_data_dir: test_bundle_root(),
        dart_entrypoint_args: None,
        engine_args: Some(vec![
            "vk_renderer_visible_window".to_string(),
            format!("--enable-impeller={impeller}"),
        ]),
        renderer: RendererPreference::Vulkan,
    };
    let mut overlay = FlutterOverlay::create(params, &device, &swap_chain)
        .expect("Vulkan works on this machine, so the engine must start on it");
    assert_eq!(overlay.renderer_type, RendererType::Vulkan);
    assert_eq!(
        !overlay.is_debug_build, aot_requested,
        "requested {mode} but the engine runs in the other build mode"
    );
    overlay.set_visibility(true);

    let mut size = INITIAL_CLIENT_SIZE;
    let mut presented_to_window = 0u64;
    let deadline = Instant::now() + Duration::from_secs(window_seconds());

    while Instant::now() < deadline && !CLOSE_REQUESTED.load(Ordering::Acquire) {
        pump_window_messages();

        let client = client_size(hwnd);
        if client != size && client.0 > 0 && client.1 > 0 {
            let resized = unsafe {
                swap_chain.ResizeBuffers(
                    0,
                    client.0,
                    client.1,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
            };
            if resized.is_ok() {
                size = client;
                overlay.handle_window_resize(0, 0, size.0, size.1, &swap_chain);
            }
        }

        let _ = overlay.request_frame();
        overlay.tick(&context);
        present_overlay(&overlay, &context, &swap_chain, size);
        presented_to_window += 1;
    }

    let window_was_visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let frames = overlay.presented_frame_count().unwrap_or(0);
    eprintln!(
        "STEP: {backend}: engine presented {frames} frames, window presented {presented_to_window}, final size {}x{}",
        size.0, size.1
    );

    assert!(window_was_visible, "the test window was never visible");
    assert!(frames > 0, "the engine never presented a frame");
    assert!(!overlay.is_device_lost());
    let settle_deadline = Instant::now() + Duration::from_secs(10);
    let mut corner = read_pixel(&device, &context, &overlay.texture, 1, 1);
    while !bgra_matches(corner, APP_BACKGROUND_BGRA) && Instant::now() < settle_deadline {
        let _ = overlay.request_frame();
        overlay.tick(&context);
        std::thread::sleep(Duration::from_millis(16));
        corner = read_pixel(&device, &context, &overlay.texture, 1, 1);
    }
    assert_bgra(
        corner,
        APP_BACKGROUND_BGRA,
        "app background in the presented texture",
    );

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    Box::leak(overlay);
}

#[test]
fn visible_window_shows_flutter_app_on_impeller() {
    show_flutter_app_in_window(true);
}
