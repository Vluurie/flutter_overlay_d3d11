use std::time::Duration;

use super::harness::{
    client_size, init_test_logging, resize_window_client_area, step, window_hwnd,
    with_shared_engine,
};

#[test]
fn satellite_texture_converges_to_resized_dimensions() {
    init_test_logging();
    let ran = with_shared_engine(|h| {
        let window = h.spawn("resize-it", 800, 600);
        let view_id = h.wait_for_view_id(&window, Duration::from_secs(5));
        step(&format!("view id published={view_id}"));
        assert!(view_id > 0, "satellite window never published a view id");

        let initial = h.wait_for_texture_size(view_id, (800, 600), Duration::from_secs(5));
        assert!(initial, "satellite never reached its spawn size 800x600");

        // Resize the real OS window. The window thread observes the new client
        // rect via GetClientRect and sends resize_view itself — the genuine game
        // path. The engine renders at the window's actual client size, which after
        // frame adjustment / DPI is not exactly the requested figure.
        let sat_hwnd = window_hwnd(&window);
        resize_window_client_area(sat_hwnd, 1920, 1080);
        let target = client_size(sat_hwnd);
        step(&format!(
            "resized; actual client size={}x{}",
            target.0, target.1
        ));
        assert!(
            target.0 >= 1900 && target.1 >= 1000,
            "window did not actually grow: client size {target:?}"
        );

        let converged = h.wait_for_texture_size(view_id, target, Duration::from_secs(5));
        step(&format!(
            "converged to {}x{}={converged}",
            target.0, target.1
        ));

        h.close_window(window);
        assert!(
            converged,
            "satellite texture did not converge to {target:?} after window resize"
        );
    });

    if ran.is_none() {
        step("engine unavailable — skipped");
    }
}
