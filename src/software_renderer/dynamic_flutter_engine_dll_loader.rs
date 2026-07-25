//! # Dynamic Flutter engine DLL loader
//!
//! Loads `flutter_engine.dll` at runtime and resolves the engine's C API entry
//! points into the [`FlutterEngineDll`] table, so the crate does not link against
//! the engine at build time and can find it next to the host's release bundle.
//!
//! Use [`FlutterEngineDll::get_for`] to obtain a process-wide cached, refcounted
//! handle for a given directory; it loads on first request and returns the cached
//! `Arc` afterwards. [`FlutterEngineDll::load`] performs an uncached load.

use anyhow::{Context, Error, Result, anyhow};
use libloading::{Library, Symbol};
use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    ffi::c_void,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::bindings::embedder as e;

/// Resolved function-pointer table for the Flutter engine C API.
///
/// Each field is a symbol looked up from `flutter_engine.dll`. Obtain one through
/// [`FlutterEngineDll::get_for`] (cached) or [`FlutterEngineDll::load`] (uncached)
/// rather than constructing it directly.
#[derive(Debug)]
pub struct FlutterEngineDll {
    _lib: &'static Library,

    pub FlutterEngineRun: Symbol<
        'static,
        unsafe extern "C" fn(
            version: usize,
            config: *const e::FlutterRendererConfig,
            project_args: *const e::FlutterProjectArgs,
            user_data: *mut c_void,
            engine_out: *mut e::FlutterEngine,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineShutdown:
        Symbol<'static, unsafe extern "C" fn(engine: e::FlutterEngine) -> e::FlutterEngineResult>,
    pub FlutterEngineInitialize: Symbol<
        'static,
        unsafe extern "C" fn(
            version: usize,
            config: *const e::FlutterRendererConfig,
            project_args: *const e::FlutterProjectArgs,
            user_data: *mut c_void,
            engine_out: *mut e::FlutterEngine,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineRunInitialized:
        Symbol<'static, unsafe extern "C" fn(engine: e::FlutterEngine) -> e::FlutterEngineResult>,
    pub FlutterEngineDeinitialize:
        Symbol<'static, unsafe extern "C" fn(engine: e::FlutterEngine) -> e::FlutterEngineResult>,

    pub FlutterEngineSendWindowMetricsEvent: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            event: *const e::FlutterWindowMetricsEvent,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineAddView: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            info: *const e::FlutterAddViewInfo,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineRemoveView: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            info: *const e::FlutterRemoveViewInfo,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineSendViewFocusEvent: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            event: *const e::FlutterViewFocusEvent,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineSendPointerEvent: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            events: *const e::FlutterPointerEvent,
            events_count: usize,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineSendKeyEvent: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            event: *const e::FlutterKeyEvent,
            key_handler: e::FlutterKeyEventCallback,
            user_data: *mut c_void,
        ) -> e::FlutterEngineResult,
    >,

    pub FlutterEngineSendPlatformMessage: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            message: *const e::FlutterPlatformMessage,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineSendPlatformMessageResponse: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            handle: *const e::FlutterPlatformMessageResponseHandle,
            bytes: *const u8,
            bytes_length: usize,
        ) -> e::FlutterEngineResult,
    >,

    pub FlutterEngineRunTask: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            task: *const e::FlutterTask,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineScheduleFrame:
        Symbol<'static, unsafe extern "C" fn(engine: e::FlutterEngine) -> e::FlutterEngineResult>,
    pub FlutterEngineGetCurrentTime: Symbol<'static, unsafe extern "C" fn() -> u64>,

    pub FlutterEngineUpdateSemanticsEnabled: Symbol<
        'static,
        unsafe extern "C" fn(engine: e::FlutterEngine, enabled: bool) -> e::FlutterEngineResult,
    >,

    pub FlutterEngineCreateAOTData: Symbol<
        'static,
        unsafe extern "C" fn(
            source: *const e::FlutterEngineAOTDataSource,
            aot_data_out: *mut e::FlutterEngineAOTData,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineOnVsync: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            baton: isize,
            frame_start_time_nanos: u64,
            frame_target_time_nanos: u64,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEnginePostDartObject: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            port: e::FlutterEngineDartPort,
            object: *const e::FlutterEngineDartObject,
        ) -> e::FlutterEngineResult,
    >,
    pub FlutterEngineUpdateLocales: Symbol<
        'static,
        unsafe extern "C" fn(
            engine: e::FlutterEngine,
            locales: *mut *const e::FlutterLocale,
            locales_count: usize,
        ) -> e::FlutterEngineResult,
    >,
}

static ENGINE_DLL_CACHE: Lazy<Mutex<HashMap<PathBuf, Arc<FlutterEngineDll>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

impl FlutterEngineDll {
    /// Loads `flutter_engine.dll` from `dir` (or the current exe's folder when
    /// `None`) and resolves every engine symbol. Uncached; prefer
    /// [`get_for`](Self::get_for) for normal use.
    pub fn load(dir: Option<&Path>) -> Result<Self> {
        let dll_dir = if let Some(d) = dir {
            d.to_path_buf()
        } else {
            std::env::current_exe()
                .context("Failed to get current exe path")?
                .parent()
                .map(PathBuf::from)
                .context("Exe has no parent directory")?
        };

        let dll_path = dll_dir.join("flutter_engine.dll");

        let lib = {
            let mut attempt = 0;
            loop {
                match unsafe { Library::new(&dll_path) } {
                    Ok(lib) => break lib,
                    Err(e) => {
                        attempt += 1;
                        if attempt >= 50 {
                            return Err(Error::new(e).context(format!(
                                "Failed to load {} after {} attempts",
                                dll_path.display(),
                                attempt
                            )));
                        }
                        log::warn!(
                            "[FlutterEngineDll] LoadLibrary attempt {attempt}/50 failed: {e}, retrying..."
                        );
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        };
        let lib_static: &'static Library = Box::leak(Box::new(lib));

        macro_rules! load_symbol {
            ($lib:expr, $name:expr) => {
                unsafe { $lib.get($name) }.with_context(|| {
                    format!(
                        "Missing symbol: {} in {}",
                        String::from_utf8_lossy($name),
                        dll_path.display()
                    )
                })
            };
        }

        Ok(FlutterEngineDll {
            _lib: lib_static,
            FlutterEngineRun: load_symbol!(lib_static, b"FlutterEngineRun\0")?,
            FlutterEngineShutdown: load_symbol!(lib_static, b"FlutterEngineShutdown\0")?,
            FlutterEngineInitialize: load_symbol!(lib_static, b"FlutterEngineInitialize\0")?,
            FlutterEngineRunInitialized: load_symbol!(
                lib_static,
                b"FlutterEngineRunInitialized\0"
            )?,
            FlutterEngineDeinitialize: load_symbol!(lib_static, b"FlutterEngineDeinitialize\0")?,
            FlutterEngineSendWindowMetricsEvent: load_symbol!(
                lib_static,
                b"FlutterEngineSendWindowMetricsEvent\0"
            )?,
            FlutterEngineAddView: load_symbol!(lib_static, b"FlutterEngineAddView\0")?,
            FlutterEngineRemoveView: load_symbol!(lib_static, b"FlutterEngineRemoveView\0")?,
            FlutterEngineSendViewFocusEvent: load_symbol!(
                lib_static,
                b"FlutterEngineSendViewFocusEvent\0"
            )?,
            FlutterEngineSendPointerEvent: load_symbol!(
                lib_static,
                b"FlutterEngineSendPointerEvent\0"
            )?,
            FlutterEngineSendKeyEvent: load_symbol!(lib_static, b"FlutterEngineSendKeyEvent\0")?,
            FlutterEngineSendPlatformMessage: load_symbol!(
                lib_static,
                b"FlutterEngineSendPlatformMessage\0"
            )?,
            FlutterEngineSendPlatformMessageResponse: load_symbol!(
                lib_static,
                b"FlutterEngineSendPlatformMessageResponse\0"
            )?,
            FlutterEngineRunTask: load_symbol!(lib_static, b"FlutterEngineRunTask\0")?,
            FlutterEngineScheduleFrame: load_symbol!(lib_static, b"FlutterEngineScheduleFrame\0")?,
            FlutterEngineGetCurrentTime: load_symbol!(
                lib_static,
                b"FlutterEngineGetCurrentTime\0"
            )?,
            FlutterEngineUpdateSemanticsEnabled: load_symbol!(
                lib_static,
                b"FlutterEngineUpdateSemanticsEnabled\0"
            )?,
            FlutterEngineCreateAOTData: load_symbol!(lib_static, b"FlutterEngineCreateAOTData\0")?,
            FlutterEngineOnVsync: load_symbol!(lib_static, b"FlutterEngineOnVsync\0")?,
            FlutterEnginePostDartObject: load_symbol!(
                lib_static,
                b"FlutterEnginePostDartObject\0"
            )?,
            FlutterEngineUpdateLocales: load_symbol!(
                lib_static,
                b"FlutterEngineUpdateLocales\0"
            )?,
        })
    }

    /// Returns a process-wide cached, refcounted handle for the resolved engine
    /// directory, loading the DLL on first request. Subsequent calls that resolve
    /// to the same path return the cached `Arc`.
    pub fn get_for(dir: Option<&Path>) -> Result<Arc<Self>> {
        let key = compute_dll_search_path(dir)?;

        let mut cache = ENGINE_DLL_CACHE
            .lock()
            .map_err(|_| anyhow!("Failed to acquire DLL cache lock"))?;

        if let Some(existing) = cache.get(&key) {
            return Ok(existing.clone());
        }

        let dll = Self::load(Some(&key)).with_context(|| {
            format!(
                "Failed to load FlutterEngineDll from directory: {}",
                key.display()
            )
        })?;
        let arc_dll = Arc::new(dll);
        cache.insert(key.clone(), arc_dll.clone());
        Ok(arc_dll)
    }
}

/// Resolves the directory used as the engine-DLL cache key: the given directory
/// if present, otherwise the directory of the current executable.
pub(crate) fn compute_dll_search_path(dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(d) = dir {
        Ok(d.to_path_buf())
    } else {
        std::env::current_exe()
            .context("Failed to get current exe path for DLL key")?
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("Exe has no parent directory for DLL key"))
    }
}
