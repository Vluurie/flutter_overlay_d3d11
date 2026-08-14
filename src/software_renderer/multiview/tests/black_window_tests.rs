use std::time::{Duration, Instant};

use super::harness::{
    client_size, init_test_logging, resize_window_client_area, step, window_hwnd,
    window_present_count, with_shared_engine,
};

fn wait_for_presents(
    h: &super::harness::EngineHarness,
    window: &crate::software_renderer::multiview::window::SatelliteWindow,
    baseline: u64,
    needed: u64,
    timeout: Duration,
) -> u64 {
    let start = Instant::now();
    while start.elapsed() < timeout {
        h.tick();
        h.request_frame();
        let now = window_present_count(window);
        if now.saturating_sub(baseline) >= needed {
            return now;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    window_present_count(window)
}

#[test]
fn satellite_keeps_presenting_across_resize() {
    init_test_logging();
    let ran = with_shared_engine(|h| {
        let window = h.spawn("black-window", 800, 600);
        let view_id = h.wait_for_view_id(&window, Duration::from_secs(8));
        assert!(view_id > 0, "no view id");
        assert!(
            h.wait_for_texture_size(view_id, (800, 600), Duration::from_secs(8)),
            "never reached spawn size"
        );

        let engine_before = h.view_frame_counter(view_id);
        let before_spawn = window_present_count(&window);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(3) {
            h.tick();
            h.request_frame();
            std::thread::sleep(Duration::from_millis(8));
        }
        let engine_delta = h.view_frame_counter(view_id).saturating_sub(engine_before);
        let present_delta = window_present_count(&window).saturating_sub(before_spawn);
        step(&format!(
            "3s live (no resize): present delta {present_delta}, engine view-frame delta {engine_delta}"
        ));
        assert!(
            engine_delta > 30,
            "engine stopped rastering the satellite view at spawn ({engine_delta} new view frames in 3s) — the view is frozen at frame 1, the window thread is not driving it"
        );
        assert!(
            present_delta > 30,
            "window thread stopped presenting at spawn ({present_delta} in 3s) — black/frozen window"
        );

        let hwnd = window_hwnd(&window);
        resize_window_client_area(hwnd, 1600, 1000);
        let target = client_size(hwnd);
        let converged = h.wait_for_texture_size(view_id, target, Duration::from_secs(10));
        step(&format!("resized to {target:?}, converged={converged}"));

        let baseline = window_present_count(&window);
        let after_resize = wait_for_presents(h, &window, baseline, 10, Duration::from_secs(8));
        step(&format!(
            "presents after resize: {} -> {} (delta {})",
            baseline,
            after_resize,
            after_resize.saturating_sub(baseline)
        ));

        h.close_window(window);

        assert!(
            converged,
            "texture did not converge to {target:?} after resize"
        );
        assert!(
            after_resize.saturating_sub(baseline) >= 10,
            "satellite STOPPED presenting after resize ({} new frames in 8s) — the window froze/went black. The window thread did not adopt the engine's new shared-texture size and reopen, so it never blits the resized frame.",
            after_resize.saturating_sub(baseline)
        );
    });
    if ran.is_none() {
        step("engine unavailable — skipped");
    }
}
