//! Serialises every access to the `VkQueue` shared with the Flutter engine.
//!
//! Vulkan requires external synchronisation on a `VkQueue`. The engine submits
//! from several threads (raster, IO/resource, Impeller workers) and the embedder
//! submits its own frame copy on the same queue, so all of them must agree on a
//! single lock. The engine resolves every Vulkan entry point through
//! `FlutterVulkanRendererConfig.get_instance_proc_address_callback`; this module
//! answers that callback and swaps the queue-touching entry points for wrappers
//! that take [`lock_queue`] before forwarding to the real function.
//!
//! The real functions come from `ash`'s typed tables, loaded through the loader's
//! own `vkGetInstanceProcAddr`. That yields the loader trampolines, which dispatch
//! on the queue/device handle and are therefore valid for every `VkDevice` in the
//! process, so one process-wide table serves all overlays.

use std::ffi::{CStr, c_char, c_void};
use std::mem;
use std::ptr;

use ash::khr::synchronization2;
use ash::vk::{self, Handle};
use ash::{DeviceFnV1_0, DeviceFnV1_3, Entry, Instance, InstanceFnV1_0};
use once_cell::sync::OnceCell;
use parking_lot::{Mutex, MutexGuard, const_mutex};

use crate::bindings::embedder::FlutterVulkanInstanceHandle;

type VoidFunction = unsafe extern "system" fn();

/// The real entry points the wrappers forward to.
struct RealFns {
    get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
    instance_v1_0: InstanceFnV1_0,
    device_v1_0: DeviceFnV1_0,
    device_v1_3: Option<DeviceFnV1_3>,
    synchronization2: Option<synchronization2::DeviceFn>,
}

static QUEUE_LOCK: Mutex<()> = const_mutex(());
static REAL_FNS: OnceCell<RealFns> = OnceCell::new();

/// Takes the process-wide queue lock. Hold it around every embedder-side
/// `vkQueueSubmit` / `vkQueueWaitIdle` / `vkDeviceWaitIdle`.
pub(crate) fn lock_queue() -> MutexGuard<'static, ()> {
    QUEUE_LOCK.lock()
}

/// Loads the real entry points through `instance`. Must succeed before the
/// engine is started with [`get_instance_proc_address_callback`]. Idempotent.
pub(crate) fn init_real_fns(entry: &Entry, instance: &Instance) -> Result<(), String> {
    REAL_FNS
        .get_or_try_init(|| {
            let get_instance_proc_addr = entry.static_fn().get_instance_proc_addr;
            let handle = instance.handle();
            let resolve = |name: &CStr| -> *const c_void {
                unsafe { get_instance_proc_addr(handle, name.as_ptr()) }
                    .map_or(ptr::null(), |function| function as *const c_void)
            };

            for required in [c"vkGetDeviceProcAddr", c"vkQueueSubmit", c"vkQueueWaitIdle"] {
                if resolve(required).is_null() {
                    return Err(format!(
                        "Vulkan loader does not expose {}",
                        required.to_string_lossy()
                    ));
                }
            }

            Ok(RealFns {
                get_instance_proc_addr,
                instance_v1_0: InstanceFnV1_0::load(resolve),
                device_v1_0: DeviceFnV1_0::load(resolve),
                device_v1_3: (!resolve(c"vkQueueSubmit2").is_null())
                    .then(|| DeviceFnV1_3::load(resolve)),
                synchronization2: (!resolve(c"vkQueueSubmit2KHR").is_null())
                    .then(|| synchronization2::DeviceFn::load(resolve)),
            })
        })
        .map(|_| ())
}

/// Returns the locking wrapper for `name`. `Some(None)` means the name is a
/// guarded entry point the loader does not provide; `None` means the name is not
/// guarded and must be resolved normally.
fn wrapper_for(real: &RealFns, name: &CStr) -> Option<vk::PFN_vkVoidFunction> {
    let wrapper = unsafe {
        match name.to_bytes() {
            b"vkGetInstanceProcAddr" => Some(mem::transmute::<
                vk::PFN_vkGetInstanceProcAddr,
                VoidFunction,
            >(guarded_get_instance_proc_addr)),
            b"vkGetDeviceProcAddr" => Some(mem::transmute::<
                vk::PFN_vkGetDeviceProcAddr,
                VoidFunction,
            >(guarded_get_device_proc_addr)),
            b"vkQueueSubmit" => Some(mem::transmute::<vk::PFN_vkQueueSubmit, VoidFunction>(
                guarded_queue_submit,
            )),
            b"vkQueueSubmit2" => real.device_v1_3.as_ref().map(|_| {
                mem::transmute::<vk::PFN_vkQueueSubmit2, VoidFunction>(guarded_queue_submit2)
            }),
            b"vkQueueSubmit2KHR" => real.synchronization2.as_ref().map(|_| {
                mem::transmute::<vk::PFN_vkQueueSubmit2, VoidFunction>(guarded_queue_submit2_khr)
            }),
            b"vkQueueWaitIdle" => Some(mem::transmute::<vk::PFN_vkQueueWaitIdle, VoidFunction>(
                guarded_queue_wait_idle,
            )),
            b"vkDeviceWaitIdle" => Some(mem::transmute::<vk::PFN_vkDeviceWaitIdle, VoidFunction>(
                guarded_device_wait_idle,
            )),
            _ => return None,
        }
    };
    Some(wrapper)
}

/// `FlutterVulkanRendererConfig.get_instance_proc_address_callback`.
pub(crate) extern "C" fn get_instance_proc_address_callback(
    _user_data: *mut c_void,
    instance: FlutterVulkanInstanceHandle,
    name: *const c_char,
) -> *mut c_void {
    let instance = vk::Instance::from_raw(instance as u64);
    unsafe { guarded_get_instance_proc_addr(instance, name) }
        .map_or(ptr::null_mut(), |function| function as *mut c_void)
}

unsafe extern "system" fn guarded_get_instance_proc_addr(
    instance: vk::Instance,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let real = REAL_FNS.get()?;
    if name.is_null() {
        return None;
    }
    match wrapper_for(real, unsafe { CStr::from_ptr(name) }) {
        Some(wrapper) => wrapper,
        None => unsafe { (real.get_instance_proc_addr)(instance, name) },
    }
}

unsafe extern "system" fn guarded_get_device_proc_addr(
    device: vk::Device,
    name: *const c_char,
) -> vk::PFN_vkVoidFunction {
    let real = REAL_FNS.get()?;
    if name.is_null() {
        return None;
    }
    match wrapper_for(real, unsafe { CStr::from_ptr(name) }) {
        Some(wrapper) => wrapper,
        None => unsafe { (real.instance_v1_0.get_device_proc_addr)(device, name) },
    }
}

unsafe extern "system" fn guarded_queue_submit(
    queue: vk::Queue,
    submit_count: u32,
    submits: *const vk::SubmitInfo<'_>,
    fence: vk::Fence,
) -> vk::Result {
    let Some(real) = REAL_FNS.get() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _guard = lock_queue();
    unsafe { (real.device_v1_0.queue_submit)(queue, submit_count, submits, fence) }
}

unsafe extern "system" fn guarded_queue_submit2(
    queue: vk::Queue,
    submit_count: u32,
    submits: *const vk::SubmitInfo2<'_>,
    fence: vk::Fence,
) -> vk::Result {
    let Some(table) = REAL_FNS.get().and_then(|real| real.device_v1_3.as_ref()) else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _guard = lock_queue();
    unsafe { (table.queue_submit2)(queue, submit_count, submits, fence) }
}

unsafe extern "system" fn guarded_queue_submit2_khr(
    queue: vk::Queue,
    submit_count: u32,
    submits: *const vk::SubmitInfo2<'_>,
    fence: vk::Fence,
) -> vk::Result {
    let Some(table) = REAL_FNS
        .get()
        .and_then(|real| real.synchronization2.as_ref())
    else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _guard = lock_queue();
    unsafe { (table.queue_submit2_khr)(queue, submit_count, submits, fence) }
}

unsafe extern "system" fn guarded_queue_wait_idle(queue: vk::Queue) -> vk::Result {
    let Some(real) = REAL_FNS.get() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _guard = lock_queue();
    unsafe { (real.device_v1_0.queue_wait_idle)(queue) }
}

unsafe extern "system" fn guarded_device_wait_idle(device: vk::Device) -> vk::Result {
    let Some(real) = REAL_FNS.get() else {
        return vk::Result::ERROR_INITIALIZATION_FAILED;
    };
    let _guard = lock_queue();
    unsafe { (real.device_v1_0.device_wait_idle)(device) }
}

/// Resolves `name` exactly as the engine would. Test-only window into the
/// wrapper table.
#[cfg(test)]
pub(crate) fn resolve_for_tests(instance: vk::Instance, name: &CStr) -> vk::PFN_vkVoidFunction {
    unsafe { guarded_get_instance_proc_addr(instance, name.as_ptr()) }
}
