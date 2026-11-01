//! Test-only crash diagnostics. A native access violation kills the test process
//! with nothing but an exit code; this reports where it happened (module +
//! offset for the faulting instruction and the call chain) before the process
//! dies, so an intermittent crash can be attributed to the engine, the GPU
//! driver, the Vulkan loader or this crate from a single occurrence.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Once;

use windows::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, HMODULE};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_POINTERS, RtlCaptureStackBackTrace,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::core::PCWSTR;

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const MAX_FRAMES: usize = 32;

/// Installs the reporter once per process.
pub fn install() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| unsafe {
        AddVectoredExceptionHandler(1, Some(report_access_violation));
    });
}

/// `module.dll+0xoffset` for a code address, or the raw address when it lies
/// outside every loaded module.
fn describe_address(address: *const c_void) -> String {
    let mut module = HMODULE::default();
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(address as *const u16),
            &mut module,
        )
    };
    if found.is_err() || module.is_invalid() {
        return format!("{address:p} (no module)");
    }

    let mut path = [0u16; 512];
    let length = unsafe { GetModuleFileNameW(Some(module), &mut path) } as usize;
    let full_path = String::from_utf16_lossy(&path[..length.min(path.len())]);
    let name = Path::new(&full_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(full_path);
    let offset = (address as usize).wrapping_sub(module.0 as usize);
    format!("{name}+{offset:#x}")
}

unsafe extern "system" fn report_access_violation(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let record = unsafe { (*info).ExceptionRecord };
    if record.is_null() || unsafe { (*record).ExceptionCode } != EXCEPTION_ACCESS_VIOLATION {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let record = unsafe { &*record };

    let operation = match record.ExceptionInformation[0] {
        0 => "read",
        1 => "write",
        8 => "execute",
        _ => "access",
    };
    let thread = std::thread::current();
    eprintln!(
        "CRASH: access violation ({operation} of {:#x}) at {} on thread '{}'",
        record.ExceptionInformation[1],
        describe_address(record.ExceptionAddress),
        thread.name().unwrap_or("<unnamed>"),
    );

    let mut frames = [std::ptr::null_mut::<c_void>(); MAX_FRAMES];
    let captured = unsafe { RtlCaptureStackBackTrace(0, &mut frames, None) } as usize;
    for (index, frame) in frames.iter().take(captured).enumerate() {
        eprintln!("CRASH:   #{index:02} {}", describe_address(*frame));
    }

    EXCEPTION_CONTINUE_SEARCH
}
