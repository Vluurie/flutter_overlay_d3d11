use crate::bindings::embedder::{
    self, FlutterEngine, FlutterLocale, FlutterProjectArgs, FlutterRendererConfig,
    FlutterWindowMetricsEvent,
};

use crate::software_renderer::dynamic_flutter_engine_dll_loader::FlutterEngineDll;
use crate::software_renderer::overlay::overlay_impl::{
    PendingPlatformMessage, SendableFlutterEngine,
};

use log::{error, info};
use std::ffi::{c_void, CString};
use std::ptr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::overlay_impl::FlutterOverlay;

// LOCALE_NAME_MAX_LENGTH from winnls.h;
const LOCALE_NAME_MAX_LENGTH: usize = 85;
unsafe extern "system" {
    fn GetUserDefaultLocaleName(lpLocaleName: *mut u16, cchLocaleName: i32) -> i32;
}

pub(crate) fn run_engine(
    version: usize,
    config: &FlutterRendererConfig,
    args: &FlutterProjectArgs,
    user_data: *mut c_void,
    overlay_raw_ptr: *mut FlutterOverlay,
    engine_dll_arc: Arc<FlutterEngineDll>,
) -> Result<FlutterEngine, String> {
    unsafe {
        let mut engine_handle: FlutterEngine = ptr::null_mut();

        if overlay_raw_ptr.is_null() {
            let err_msg =
                "[Engine] overlay_raw_ptr is null. Cannot proceed with engine initialization."
                    .to_string();
            error!("{err_msg}");
            return Err(err_msg);
        }

        if user_data as *mut FlutterOverlay != overlay_raw_ptr {
            let err_msg =
                "[Engine] user_data and overlay_raw_ptr mismatch — cannot safely proceed."
                    .to_string();
            error!("{err_msg}");
            return Err(err_msg);
        }

        let init_result = (engine_dll_arc.FlutterEngineInitialize)(
            version,
            config,
            args,
            user_data,
            &mut engine_handle,
        );

        if init_result != embedder::FlutterEngineResult_kSuccess || engine_handle.is_null() {
            let err_msg = format!(
                "[Engine] FlutterEngineInitialize failed with result: {init_result:?} or engine handle is null."
            );
            error!("{err_msg}");
            return Err(err_msg);
        }

        (*overlay_raw_ptr).engine = SendableFlutterEngine(engine_handle);
        (*overlay_raw_ptr)
            .engine_atomic_ptr
            .store(engine_handle, Ordering::SeqCst);

        let run_result = (engine_dll_arc.FlutterEngineRunInitialized)(engine_handle);

        if run_result != embedder::FlutterEngineResult_kSuccess {
            let err_msg =
                format!("[Engine] FlutterEngineRunInitialized failed with result: {run_result:?}");
            error!("{err_msg}");

            (engine_dll_arc.FlutterEngineDeinitialize)(engine_handle);
            (*overlay_raw_ptr).engine = SendableFlutterEngine(ptr::null_mut());
            (*overlay_raw_ptr)
                .engine_atomic_ptr
                .store(ptr::null_mut(), Ordering::SeqCst);

            return Err(err_msg);
        }
        Ok(engine_handle)
    }
}

pub(crate) fn update_flutter_window_metrics(
    engine: FlutterEngine,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    engine_dll: Arc<FlutterEngineDll>,
) {
    if engine.is_null() {
        error!("[Metrics] Attempted to send metrics with a null engine handle.");
        return;
    }

    let mut wm: FlutterWindowMetricsEvent = unsafe { std::mem::zeroed() };
    wm.struct_size = std::mem::size_of::<FlutterWindowMetricsEvent>();

    wm.width = width as usize;
    wm.height = height as usize;
    wm.pixel_ratio = 1.0;
    wm.left = x as usize;
    wm.top = y as usize;
    let r = unsafe { (engine_dll.FlutterEngineSendWindowMetricsEvent)(engine, &wm) };
    if r != embedder::FlutterEngineResult_kSuccess {
        error!("[Metrics] FlutterEngineSendWindowMetricsEvent failed with result: {r:?}");
    }
}

/// Reports the OS user-default locale to the engine via `UpdateLocales`.
pub(crate) fn send_system_locale_to_engine(engine: FlutterEngine, engine_dll: &FlutterEngineDll) {
    if engine.is_null() {
        error!("[Locale] null engine handle.");
        return;
    }

    let Some((language, country)) = read_user_default_locale() else {
        error!("[Locale] could not read the OS default locale.");
        return;
    };

    let language_c = match CString::new(language.as_str()) {
        Ok(c) => c,
        Err(_) => return,
    };
    let country_c = CString::new(country.as_str()).ok();

    let locale = FlutterLocale {
        struct_size: std::mem::size_of::<FlutterLocale>(),
        language_code: language_c.as_ptr(),
        country_code: country_c.as_ref().map_or(ptr::null(), |c| c.as_ptr()),
        script_code: ptr::null(),
        variant_code: ptr::null(),
    };

    let mut locales: [*const FlutterLocale; 1] = [&locale];
    let r = unsafe {
        (engine_dll.FlutterEngineUpdateLocales)(engine, locales.as_mut_ptr(), locales.len())
    };

    if r != embedder::FlutterEngineResult_kSuccess {
        error!("[Locale] FlutterEngineUpdateLocales failed: {r:?}");
    } else {
        info!("[Locale] reported OS locale to engine: {language}-{country}");
    }
}

fn read_user_default_locale() -> Option<(String, String)> {
    let mut buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    let len = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if len <= 0 {
        return None;
    }

    // `len` counts the trailing NUL.
    let end = (len as usize).saturating_sub(1).min(buf.len());
    let name = String::from_utf16_lossy(&buf[..end]);
    if name.is_empty() {
        return None;
    }

    let mut parts = name.split('-');
    let language = parts.next()?.to_ascii_lowercase();
    let country = parts.next().unwrap_or("").to_ascii_uppercase();
    Some((language, country))
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn on_root_isolate_created(user_data: *mut ::std::os::raw::c_void) {
    if user_data.is_null() {
        error!("[Engine] Root isolate created with null user_data.");
        return;
    }

    let overlay: &mut FlutterOverlay = unsafe { &mut *(user_data as *mut FlutterOverlay) };

    let channel = "flutter/lifecycle".to_string();
    let payload_bytes = "AppLifecycleState.resumed".to_string().into_bytes();

    let msg_lifecycle = PendingPlatformMessage {
        channel,
        payload_bytes,
    };

    let metrics_channel = "flutter/window".to_string();

    let metrics_payload = format!(
        r#"{{"method":"setWindowMetrics","args":{{"viewId":0,"width":{},"height":{},"devicePixelRatio":1.0,"left":{},"top":{}}}}}"#,
        overlay.width, overlay.height, overlay.x, overlay.y
    );
    let metrics_bytes = metrics_payload.into_bytes();

    let msg_metrics = PendingPlatformMessage {
        channel: metrics_channel,
        payload_bytes: metrics_bytes,
    };

    if let Ok(mut queue) = overlay.pending_platform_messages.lock() {
        queue.push_back(msg_metrics);
        queue.push_back(msg_lifecycle);
    } else {
        error!("[Engine] Failed to lock queue in isolate callback.");
    }
    overlay.task_queue_state.waker.wake_up();
}
