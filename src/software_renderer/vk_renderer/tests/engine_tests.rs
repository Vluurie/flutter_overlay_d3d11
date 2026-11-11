//! Integration tests: the real Flutter `test_app` running on the Vulkan renderer.
//!
//! The app paints every view solid `0xFF2244AA` with a centred label, so a frame
//! that made it through engine → Vulkan → keyed mutex → D3D11 shows up as that
//! colour in the corners of the host's compositing texture.
//!
//! Each engine backend gets one long-lived engine for the whole test process,
//! mirroring the host (one overlay, driven for its lifetime). A test skips only
//! when the engine bundle is missing or the machine cannot run the Vulkan path at
//! all; once Vulkan is known to work, a failure to start the engine on it fails
//! the test instead of skipping it.

use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::software_renderer::api::{RendererPreference, RendererType};
use crate::software_renderer::multiview::tests::harness::{
    EngineHarness, bundle_is_present, init_test_logging, release_build_requested, step,
};
use crate::software_renderer::vk_renderer::context::HandoffMode;
use crate::software_renderer::vk_renderer::interop::FORCE_READBACK_ENV;
use crate::software_renderer::vk_renderer::tests::support::{
    APP_BACKGROUND_BGRA, assert_bgra, bgra_matches, read_pixel, vulkan_path_is_available,
};

const FRAME_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Backend {
    Skia,
    Impeller,
    /// Impeller with the readback hand-off forced: what a Wine/MoltenVK stack
    /// without Vulkan↔D3D11 interop runs.
    ImpellerReadback,
}

impl Backend {
    fn engine_args(self) -> Vec<String> {
        let switch = match self {
            Backend::Skia => "--enable-impeller=false",
            Backend::Impeller | Backend::ImpellerReadback => "--enable-impeller=true",
        };
        vec!["vk_renderer_engine_tests".to_string(), switch.to_string()]
    }

    fn expected_handoff(self) -> Option<HandoffMode> {
        (self == Backend::ImpellerReadback).then_some(HandoffMode::Readback)
    }

    fn slot(self) -> &'static OnceLock<Option<Mutex<EngineHarness>>> {
        static SKIA: OnceLock<Option<Mutex<EngineHarness>>> = OnceLock::new();
        static IMPELLER: OnceLock<Option<Mutex<EngineHarness>>> = OnceLock::new();
        static IMPELLER_READBACK: OnceLock<Option<Mutex<EngineHarness>>> = OnceLock::new();
        match self {
            Backend::Skia => &SKIA,
            Backend::Impeller => &IMPELLER,
            Backend::ImpellerReadback => &IMPELLER_READBACK,
        }
    }
}

/// Runs `f` against the shared Vulkan engine for `backend`. Returns `None`
/// (a skip) when the environment cannot run it; panics when it should have run.
fn with_vulkan_engine<R>(backend: Backend, f: impl FnOnce(&mut EngineHarness) -> R) -> Option<R> {
    init_test_logging();
    let slot = backend.slot().get_or_init(|| {
        if !bundle_is_present() {
            step("engine bundle missing — Vulkan engine tests will skip");
            return None;
        }
        if !vulkan_path_is_available() {
            return None;
        }
        let force_readback = backend.expected_handoff() == Some(HandoffMode::Readback);
        if force_readback {
            unsafe { std::env::set_var(FORCE_READBACK_ENV, "1") };
        }
        let harness = EngineHarness::create_with_renderer(
            &format!("vulkan-{backend:?}-test-engine"),
            None,
            Some(backend.engine_args()),
            RendererPreference::Vulkan,
            RendererType::Vulkan,
        );
        if force_readback {
            unsafe { std::env::remove_var(FORCE_READBACK_ENV) };
        }
        let harness = harness.unwrap_or_else(|| {
            panic!("Vulkan works on this machine but the {backend:?} engine did not start on it")
        });

        let handoff = harness
            .overlay()
            .vulkan_state
            .as_ref()
            .map(|vulkan_state| vulkan_state.handoff());
        if let Some(expected) = backend.expected_handoff() {
            assert_eq!(handoff, Some(expected));
        }
        step(&format!("{backend:?} engine uses hand-off {handoff:?}"));
        Some(Mutex::new(harness))
    });
    let mutex = slot.as_ref()?;
    let mut guard = mutex.lock().unwrap_or_else(|p| p.into_inner());
    Some(f(&mut guard))
}

fn host_pixel(harness: &EngineHarness, x: u32, y: u32) -> [u8; 4] {
    read_pixel(
        &harness.device,
        &harness.host_context,
        &harness.overlay().texture,
        x,
        y,
    )
}

/// Drives the engine like the host does until the pixel at (`x`, `y`) of the
/// host texture shows the app background. Returns the last pixel read.
fn wait_for_app_pixel(harness: &EngineHarness, x: u32, y: u32) -> [u8; 4] {
    let start = Instant::now();
    let mut pixel = [0u8; 4];
    while start.elapsed() < FRAME_TIMEOUT {
        harness.request_frame();
        harness.tick();
        pixel = host_pixel(harness, x, y);
        if bgra_matches(pixel, APP_BACKGROUND_BGRA) {
            return pixel;
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    pixel
}

fn frame_count(harness: &EngineHarness) -> u64 {
    harness
        .overlay()
        .presented_frame_count()
        .expect("Vulkan renderer reports a frame count")
}

fn app_frame_reaches_host_texture(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        assert_eq!(harness.overlay().renderer_type, RendererType::Vulkan);
        let (width, height) = harness.overlay().get_dimensions();

        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "top-left",
        );
        assert_bgra(
            wait_for_app_pixel(harness, width - 2, height - 2),
            APP_BACKGROUND_BGRA,
            "bottom-right",
        );
        assert!(harness.overlay().has_first_frame());
        assert!(frame_count(harness) > 0);
        assert!(!harness.overlay().is_device_lost());
    });
}

fn resize_keeps_delivering(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "before resize",
        );

        let swap_chain = harness.host_swapchain();
        for (width, height) in [(420u32, 310u32), (200, 140), (256, 256)] {
            harness
                .overlay_mut()
                .handle_window_resize(0, 0, width, height, &swap_chain);
            assert_eq!(harness.overlay().get_dimensions(), (width, height));
            assert_bgra(
                wait_for_app_pixel(harness, width - 2, height - 2),
                APP_BACKGROUND_BGRA,
                &format!("bottom-right at {width}x{height}"),
            );
        }
        assert!(!harness.overlay().is_device_lost());
    });
}

fn hidden_overlay_never_stalls_the_engine(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "before hiding",
        );

        harness.set_main_visibility(false);
        let before = frame_count(harness);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) && frame_count(harness) < before + 5 {
            harness.request_frame();
            harness.tick();
            std::thread::sleep(Duration::from_millis(16));
        }
        let while_hidden = frame_count(harness);
        harness.set_main_visibility(true);

        assert!(
            while_hidden >= before + 5,
            "engine stalled while the host was not consuming frames: {before} -> {while_hidden}"
        );
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "after showing again",
        );
    });
}

/// A low memory warning makes the engine drop its GPU caches. It has to reach
/// the engine through the task runner, and the overlay has to keep delivering
/// frames from the emptied caches.
fn low_memory_warning_keeps_delivering(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "before the warning",
        );

        harness
            .overlay()
            .notify_low_memory()
            .expect("the engine is running");
        let pending = || {
            harness
                .overlay()
                .pending_low_memory_warning
                .load(Ordering::Acquire)
        };
        let start = Instant::now();
        while start.elapsed() < FRAME_TIMEOUT && pending() {
            std::thread::sleep(Duration::from_millis(16));
        }
        assert!(
            !pending(),
            "the task runner never delivered the low memory warning"
        );

        let before = frame_count(harness);
        let start = Instant::now();
        while start.elapsed() < FRAME_TIMEOUT && frame_count(harness) < before + 5 {
            harness.request_frame();
            harness.tick();
            std::thread::sleep(Duration::from_millis(16));
        }
        assert!(
            frame_count(harness) >= before + 5,
            "engine stopped presenting after the low memory warning"
        );
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "after the warning",
        );
        assert!(!harness.overlay().is_device_lost());
    });
}

/// `FLUTTER_TEST_RELEASE=1` must really run the AOT snapshot on the release
/// engine, and its absence must really run JIT: a silent fallback would make the
/// AOT run of this suite meaningless.
fn engine_runs_in_the_requested_build_mode(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        let aot_requested = release_build_requested();
        let runs_aot = !harness.overlay().is_debug_build;
        assert_eq!(
            runs_aot, aot_requested,
            "requested {} but the engine runs {}",
            if aot_requested { "AOT" } else { "JIT" },
            if runs_aot { "AOT" } else { "JIT" },
        );
        step(&format!(
            "{backend:?} on Vulkan runs {}",
            if runs_aot { "AOT (release)" } else { "JIT (debug)" }
        ));
        assert_bgra(
            wait_for_app_pixel(harness, 1, 1),
            APP_BACKGROUND_BGRA,
            "frame in the requested build mode",
        );
    });
}

fn satellite_views_are_rejected_with_guidance(backend: Backend) {
    with_vulkan_engine(backend, |harness| {
        let device = harness.device.clone();
        let error = harness
            .overlay_mut()
            .add_offscreen_view(&device, 64, 64, 1.0)
            .expect_err("the Vulkan renderer has no compositor");
        let message = error.to_string();
        assert!(
            message.contains("RendererPreference::OpenGL"),
            "error does not tell the caller what to do: {message}"
        );
    });
}

#[test]
fn skia_app_frame_reaches_host_texture() {
    app_frame_reaches_host_texture(Backend::Skia);
}

#[test]
fn impeller_app_frame_reaches_host_texture() {
    app_frame_reaches_host_texture(Backend::Impeller);
}

#[test]
fn skia_engine_runs_in_the_requested_build_mode() {
    engine_runs_in_the_requested_build_mode(Backend::Skia);
}

#[test]
fn impeller_engine_runs_in_the_requested_build_mode() {
    engine_runs_in_the_requested_build_mode(Backend::Impeller);
}

#[test]
fn impeller_readback_app_frame_reaches_host_texture() {
    app_frame_reaches_host_texture(Backend::ImpellerReadback);
}

#[test]
fn impeller_readback_resize_keeps_delivering() {
    resize_keeps_delivering(Backend::ImpellerReadback);
}

#[test]
fn impeller_readback_hidden_overlay_never_stalls_the_engine() {
    hidden_overlay_never_stalls_the_engine(Backend::ImpellerReadback);
}

#[test]
fn skia_resize_keeps_delivering() {
    resize_keeps_delivering(Backend::Skia);
}

#[test]
fn impeller_resize_keeps_delivering() {
    resize_keeps_delivering(Backend::Impeller);
}

#[test]
fn skia_hidden_overlay_never_stalls_the_engine() {
    hidden_overlay_never_stalls_the_engine(Backend::Skia);
}

#[test]
fn impeller_hidden_overlay_never_stalls_the_engine() {
    hidden_overlay_never_stalls_the_engine(Backend::Impeller);
}

#[test]
fn skia_low_memory_warning_keeps_delivering() {
    low_memory_warning_keeps_delivering(Backend::Skia);
}

#[test]
fn impeller_low_memory_warning_keeps_delivering() {
    low_memory_warning_keeps_delivering(Backend::Impeller);
}

#[test]
fn impeller_readback_low_memory_warning_keeps_delivering() {
    low_memory_warning_keeps_delivering(Backend::ImpellerReadback);
}

#[test]
fn skia_satellite_views_are_rejected_with_guidance() {
    satellite_views_are_rejected_with_guidance(Backend::Skia);
}

#[test]
fn impeller_satellite_views_are_rejected_with_guidance() {
    satellite_views_are_rejected_with_guidance(Backend::Impeller);
}
