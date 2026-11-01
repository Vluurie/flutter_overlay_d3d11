use crate::bindings::embedder::FlutterTaskRunnerDescription;
use crate::software_renderer::overlay::overlay_impl::FLUTTER_LOG_TAG;
use crate::software_renderer::ticker::task_scheduler::{
    TaskQueueState, TaskRunnerContext, destroy_task_runner_context_callback, post_task_callback,
    runs_task_on_current_thread_callback,
};

use crate::software_renderer::api::RendererType;
use log::{info, warn};
use std::ffi::{CStr, CString, OsStr, c_void};
use std::sync::Arc;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn flutter_log_callback(
    tag: *const std::os::raw::c_char,
    message: *const std::os::raw::c_char,
    _user_data: *mut c_void,
) {
    let tag_str = if tag.is_null() {
        FLUTTER_LOG_TAG.to_string_lossy().into_owned()
    } else {
        unsafe { CStr::from_ptr(tag).to_string_lossy().into_owned() }
    };
    let msg_str = if message.is_null() {
        ""
    } else {
        unsafe { &CStr::from_ptr(message).to_string_lossy().into_owned() }
    };
    info!("[Flutter][{tag_str}] {msg_str}");
}

fn create_task_runner_description_with_context(
    identifier: usize,
    task_queue: Arc<TaskQueueState>,
) -> (FlutterTaskRunnerDescription, Box<TaskRunnerContext>) {
    let context = Box::new(TaskRunnerContext {
        task_runner_thread_id: None,
        task_queue,
    });
    let context_ptr = Box::into_raw(context);

    let description = FlutterTaskRunnerDescription {
        struct_size: std::mem::size_of::<FlutterTaskRunnerDescription>(),
        user_data: context_ptr as *mut c_void, // looks at TaskRunnerContext
        runs_task_on_current_thread_callback: Some(runs_task_on_current_thread_callback),
        post_task_callback: Some(post_task_callback),
        identifier,
        destruction_callback: Some(destroy_task_runner_context_callback),
    };
    (description, unsafe { Box::from_raw(context_ptr) })
}

pub(crate) fn build_project_args_and_strings(
    assets: &str,
    icu: &str,
    dart_args_opt: Option<&[String]>,
    engine_args_opt: Option<&[String]>,
) -> (
    CString,      // assets_c
    CString,      // icu_c
    Vec<CString>, // engine_argv_cs
    Vec<CString>, // dart_argv_cs
) {
    let assets_c = CString::new(assets).expect("Failed to convert assets path to CString");
    let icu_c = CString::new(icu).expect("Failed to convert icu data path to CString");

    let engine_argv_cs: Vec<CString> = engine_args_opt
        .unwrap_or(&[])
        .iter()
        .map(|s| CString::new(s.as_str()).expect("Failed to create CString from engine ARGS"))
        .collect();

    let dart_argv_cs: Vec<CString> = dart_args_opt
        .unwrap_or(&[])
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();

    (assets_c, icu_c, engine_argv_cs, dart_argv_cs)
}
const ENABLE_IMPELLER_SWITCH: &str = "--enable-impeller";

fn is_impeller_switch(arg: &CStr) -> bool {
    let bytes = arg.to_bytes();
    let switch = ENABLE_IMPELLER_SWITCH.as_bytes();
    bytes == switch || (bytes.starts_with(switch) && bytes.get(switch.len()) == Some(&b'='))
}

/// Forces Impeller off for the software renderer, which is Skia-only.
pub(crate) fn engine_args_for_renderer(
    engine_args: Vec<CString>,
    renderer: &RendererType,
) -> Vec<CString> {
    if *renderer != RendererType::Software {
        return engine_args;
    }

    let mut args = engine_args.into_iter();
    let program_name = args
        .next()
        .unwrap_or_else(|| CString::from(c"flutter_embedder"));

    let mut impeller_was_requested = false;
    let mut adjusted = vec![program_name];
    for arg in args {
        if is_impeller_switch(&arg) {
            impeller_was_requested |= arg.to_bytes() != b"--enable-impeller=false";
        } else {
            adjusted.push(arg);
        }
    }
    adjusted.push(CString::from(c"--enable-impeller=false"));

    if impeller_was_requested {
        warn!(
            "[InitOverlay] Impeller was requested, but the software renderer only supports Skia. Starting the engine with Impeller disabled."
        );
    }
    adjusted
}

pub(crate) fn maybe_load_aot_path_to_cstring(aot_opt: Option<&OsStr>) -> Option<CString> {
    if let Some(os) = aot_opt {
        let path = os.to_string_lossy();
        let aot_c = CString::new(path.as_ref()).expect("Failed to create CString for AOT path");
        Some(aot_c)
    } else {
        None
    }
}
