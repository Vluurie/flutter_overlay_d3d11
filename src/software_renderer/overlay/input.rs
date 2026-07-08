use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    HCURSOR, HTCLIENT, IDC_ARROW, IDC_HAND, IDC_IBEAM, IDC_NO, LoadCursorW, SetCursor,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCMOUSELEAVE, WM_RBUTTONDOWN, WM_RBUTTONUP,
};

use winapi::um::winuser::{
    MK_LBUTTON as WINAPI_MK_LBUTTON, MK_MBUTTON as WINAPI_MK_MBUTTON,
    MK_RBUTTON as WINAPI_MK_RBUTTON, WHEEL_DELTA,
};

use crate::bindings::embedder::{
    FlutterEngine, FlutterEngineResult, FlutterPointerDeviceKind_kFlutterPointerDeviceKindMouse,
    FlutterPointerEvent, FlutterPointerPhase, FlutterPointerPhase_kAdd, FlutterPointerPhase_kDown,
    FlutterPointerPhase_kHover, FlutterPointerPhase_kMove, FlutterPointerPhase_kRemove,
    FlutterPointerPhase_kUp, FlutterPointerSignalKind_kFlutterPointerSignalKindNone,
    FlutterPointerSignalKind_kFlutterPointerSignalKindScroll,
};

use crate::software_renderer::dynamic_flutter_engine_dll_loader::FlutterEngineDll;
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::overlays_manager_api::{POINTER_BUTTONS, POINTER_POS};

pub fn handle_pointer_event(
    overlay: &FlutterOverlay,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> bool {
    let engine = &overlay.engine;
    let engine_dll = &overlay.engine_dll;

    if engine.0.is_null() {
        return false;
    }

    match msg {
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i16 as f64;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;
            let key_states_from_wparam = wparam.0;
            let mut calculated_mk_buttons_i32: i32 = 0;

            if (key_states_from_wparam & WINAPI_MK_LBUTTON) != 0 {
                calculated_mk_buttons_i32 |= WINAPI_MK_LBUTTON as i32;
            }
            if (key_states_from_wparam & WINAPI_MK_RBUTTON) != 0 {
                calculated_mk_buttons_i32 |= WINAPI_MK_RBUTTON as i32;
            }
            if (key_states_from_wparam & WINAPI_MK_MBUTTON) != 0 {
                calculated_mk_buttons_i32 |= WINAPI_MK_MBUTTON as i32;
            }

            let current_buttons_state = calculated_mk_buttons_i32;

            let phase = if current_buttons_state != 0 {
                FlutterPointerPhase_kMove
            } else if !overlay.is_mouse_added.load(Ordering::SeqCst) {
                overlay.is_mouse_added.store(true, Ordering::SeqCst);
                FlutterPointerPhase_kAdd
            } else {
                FlutterPointerPhase_kHover
            };

            overlay
                .mouse_buttons_state
                .store(current_buttons_state, Ordering::Relaxed);
            send_pointer_event_to_flutter(
                overlay,
                engine.0,
                engine_dll,
                PointerSample {
                    phase,
                    x,
                    y,
                    scroll_delta_x: 0.0,
                    scroll_delta_y: 0.0,
                    buttons: current_buttons_state as i64,
                },
            );
            true
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i16 as f64;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;
            let button_flag_to_set: i32 = match msg {
                WM_LBUTTONDOWN => WINAPI_MK_LBUTTON as i32,
                WM_RBUTTONDOWN => WINAPI_MK_RBUTTON as i32,
                WM_MBUTTONDOWN => WINAPI_MK_MBUTTON as i32,
                _ => 0,
            };

            let mut new_button_state = overlay.mouse_buttons_state.load(Ordering::Relaxed);
            new_button_state |= button_flag_to_set;
            overlay
                .mouse_buttons_state
                .store(new_button_state, Ordering::Relaxed);

            if !overlay.is_mouse_added.load(Ordering::SeqCst) {
                overlay.is_mouse_added.store(true, Ordering::SeqCst);
                send_pointer_event_to_flutter(
                    overlay,
                    engine.0,
                    engine_dll,
                    PointerSample {
                        phase: FlutterPointerPhase_kAdd,
                        x,
                        y,
                        scroll_delta_x: 0.0,
                        scroll_delta_y: 0.0,
                        buttons: 0,
                    },
                );
            }

            send_pointer_event_to_flutter(
                overlay,
                engine.0,
                engine_dll,
                PointerSample {
                    phase: FlutterPointerPhase_kDown,
                    x,
                    y,
                    scroll_delta_x: 0.0,
                    scroll_delta_y: 0.0,
                    buttons: new_button_state as i64,
                },
            );
            true
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let x = (lparam.0 & 0xFFFF) as i16 as f64;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f64;
            let button_flag_to_clear: i32 = match msg {
                WM_LBUTTONUP => WINAPI_MK_LBUTTON as i32,
                WM_RBUTTONUP => WINAPI_MK_RBUTTON as i32,
                WM_MBUTTONUP => WINAPI_MK_MBUTTON as i32,
                _ => 0,
            };

            let mut current_buttons_state = overlay.mouse_buttons_state.load(Ordering::Relaxed);
            let buttons_for_kup_event = current_buttons_state;
            current_buttons_state &= !button_flag_to_clear;
            overlay
                .mouse_buttons_state
                .store(current_buttons_state, Ordering::Relaxed);

            send_pointer_event_to_flutter(
                overlay,
                engine.0,
                engine_dll,
                PointerSample {
                    phase: FlutterPointerPhase_kUp,
                    x,
                    y,
                    scroll_delta_x: 0.0,
                    scroll_delta_y: 0.0,
                    buttons: buttons_for_kup_event as i64,
                },
            );
            true
        }
        WM_NCMOUSELEAVE => {
            if overlay.is_mouse_added.load(Ordering::SeqCst) {
                overlay.is_mouse_added.store(false, Ordering::SeqCst);
                send_pointer_event_to_flutter(
                    overlay,
                    engine.0,
                    engine_dll,
                    PointerSample {
                        phase: FlutterPointerPhase_kRemove,
                        x: 0.0,
                        y: 0.0,
                        scroll_delta_x: 0.0,
                        scroll_delta_y: 0.0,
                        buttons: 0,
                    },
                );
            }
            overlay.mouse_buttons_state.store(0, Ordering::Relaxed);
            false
        }
        WM_MOUSEWHEEL => {
            let wheel_delta = (wparam.0 >> 16) as i16;
            let x_screen = (lparam.0 & 0xFFFF) as i16;
            let y_screen = ((lparam.0 >> 16) & 0xFFFF) as i16;
            let mut point = POINT {
                x: x_screen as i32,
                y: y_screen as i32,
            };

            unsafe {
                if ScreenToClient(hwnd, &mut point) == false {
                    return true;
                }
            }

            let x_client = point.x as f64;
            let y_client = point.y as f64;
            let scroll_delta_y_flutter = -(wheel_delta as f64 / WHEEL_DELTA as f64) * 20.0;

            send_pointer_event_to_flutter(
                overlay,
                engine.0,
                engine_dll,
                PointerSample {
                    phase: FlutterPointerPhase_kHover,
                    x: x_client,
                    y: y_client,
                    scroll_delta_x: 0.0,
                    scroll_delta_y: scroll_delta_y_flutter,
                    buttons: overlay.mouse_buttons_state.load(Ordering::Relaxed) as i64,
                },
            );
            true
        }
        _ => false,
    }
}

pub fn handle_set_cursor(
    overlay: &FlutterOverlay,
    hwnd_from_wparam: HWND,
    lparam_from_message: LPARAM,
    main_app_hwnd: HWND,
) -> Option<LRESULT> {
    unsafe {
        let hit_test_code = (lparam_from_message.0 & 0xFFFF) as i16;

        if hwnd_from_wparam == main_app_hwnd
            && hit_test_code == HTCLIENT as i16
            && let Ok(desired_kind_guard) = overlay.desired_cursor.try_lock()
            && let Some(kind) = desired_kind_guard.as_ref()
        {
            let mut h_cursor_to_set: HCURSOR = HCURSOR(std::ptr::null_mut());
            let mut flutter_did_request_cursor_change = true;

            let h_instance_null: HINSTANCE = HINSTANCE(std::ptr::null_mut());

            match kind.as_str() {
                "basic" | "basic.default" => {
                    h_cursor_to_set = LoadCursorW(Some(h_instance_null), IDC_ARROW)
                        .unwrap_or(HCURSOR(std::ptr::null_mut()));
                }
                "click" | "pointer" => {
                    h_cursor_to_set = LoadCursorW(Some(h_instance_null), IDC_HAND)
                        .unwrap_or(HCURSOR(std::ptr::null_mut()));
                }
                "text" | "text.TextEditable" => {
                    h_cursor_to_set = LoadCursorW(Some(h_instance_null), IDC_IBEAM)
                        .unwrap_or(HCURSOR(std::ptr::null_mut()));
                }
                "forbidden" | "basic.forbidden" => {
                    h_cursor_to_set = LoadCursorW(Some(h_instance_null), IDC_NO)
                        .unwrap_or(HCURSOR(std::ptr::null_mut()));
                }
                _ => {
                    flutter_did_request_cursor_change = false;
                }
            }

            if flutter_did_request_cursor_change && !h_cursor_to_set.0.is_null() {
                SetCursor(Some(h_cursor_to_set));
                return Some(LRESULT(1));
            }
        }
        None
    }
}
/// One mouse pointer sample to forward to the Flutter engine.
struct PointerSample {
    phase: FlutterPointerPhase,
    x: f64,
    y: f64,
    scroll_delta_x: f64,
    scroll_delta_y: f64,
    buttons: i64,
}

fn send_pointer_event_to_flutter(
    overlay: &FlutterOverlay,
    engine: FlutterEngine,
    engine_dll: &FlutterEngineDll,
    sample: PointerSample,
) {
    POINTER_POS.store(
        (((sample.x as f32).to_bits() as u64) << 32) | (sample.y as f32).to_bits() as u64,
        std::sync::atomic::Ordering::Release,
    );
    POINTER_BUTTONS.store(sample.buttons, std::sync::atomic::Ordering::Release);
    send_pointer_to_secondary_views(overlay, &sample);
    let PointerSample {
        phase,
        x,
        y,
        scroll_delta_x,
        scroll_delta_y,
        buttons,
    } = sample;
    unsafe {
        if engine.is_null() {
            return;
        }

        let event = FlutterPointerEvent {
            struct_size: std::mem::size_of::<FlutterPointerEvent>(),
            phase,
            timestamp: (engine_dll.FlutterEngineGetCurrentTime)() as usize / 1000,
            x,
            y,
            device: 0,
            signal_kind: if scroll_delta_x != 0.0 || scroll_delta_y != 0.0 {
                FlutterPointerSignalKind_kFlutterPointerSignalKindScroll
            } else {
                FlutterPointerSignalKind_kFlutterPointerSignalKindNone
            },
            scroll_delta_x,
            scroll_delta_y,
            device_kind: FlutterPointerDeviceKind_kFlutterPointerDeviceKindMouse,
            buttons,
            pan_x: 0.0,
            pan_y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            view_id: 0,
        };

        let _res: FlutterEngineResult =
            (engine_dll.FlutterEngineSendPointerEvent)(engine, &event as *const _, 1);
    }
}

/// Send the same pointer sample to every secondary (offscreen) view, offsetting
/// screen coords into each view's local space via its registered screen rect.
fn send_pointer_to_secondary_views(overlay: &FlutterOverlay, sample: &PointerSample) {
    let engine = overlay.engine.0;
    if engine.is_null() {
        return;
    }
    let dll = &overlay.engine_dll;
    let sids = overlay.secondary_view_ids();
    for view_id in sids {
        let Some((rx, ry, rw, rh)) = overlay.view_screen_rect(view_id) else {
            continue;
        };
        if sample.x < rx as f64
            || sample.y < ry as f64
            || sample.x >= (rx + rw) as f64
            || sample.y >= (ry + rh) as f64
        {
            continue;
        }
        let (vw, vh) = overlay
            .view_pixel_size(view_id)
            .unwrap_or((rw as u32, rh as u32));
        let lx = (sample.x - rx as f64) / rw as f64 * vw as f64;
        let ly = (sample.y - ry as f64) / rh as f64 * vh as f64;
        let event = FlutterPointerEvent {
            struct_size: std::mem::size_of::<FlutterPointerEvent>(),
            phase: sample.phase,
            timestamp: unsafe { (dll.FlutterEngineGetCurrentTime)() } as usize / 1000,
            x: lx,
            y: ly,
            device: view_id as i32 + 1,
            signal_kind: if sample.scroll_delta_x != 0.0 || sample.scroll_delta_y != 0.0 {
                FlutterPointerSignalKind_kFlutterPointerSignalKindScroll
            } else {
                FlutterPointerSignalKind_kFlutterPointerSignalKindNone
            },
            scroll_delta_x: sample.scroll_delta_x,
            scroll_delta_y: sample.scroll_delta_y,
            device_kind: FlutterPointerDeviceKind_kFlutterPointerDeviceKindMouse,
            buttons: sample.buttons,
            pan_x: 0.0,
            pan_y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            view_id,
        };
        let _ = unsafe { (dll.FlutterEngineSendPointerEvent)(engine, &event as *const _, 1) };
    }
}
