//! Vulkan instance + logical device owned by the embedder and lent to the engine.
//!
//! The device is created on the **same adapter as the host's D3D11 device**
//! (matched by LUID): the frame is handed over through a D3D11 texture imported
//! into Vulkan, which only works within one adapter.
//!
//! The extension and feature set is the union of what the engine assumes from an
//! embedder-supplied device and what the D3D11 interop needs:
//!
//! * Impeller refuses to start unless `VK_KHR_surface` (instance) and
//!   `VK_KHR_swapchain` (device) are in the lists passed through
//!   `FlutterVulkanRendererConfig`, even though an embedder never presents to a
//!   swapchain. It also assumes `fillModeNonSolid`, `samplerAnisotropy` and
//!   16-bit storage are enabled whenever the adapter supports them.
//! * The Skia backend tells Ganesh that every feature the adapter *supports* is
//!   *enabled*, so every supported core feature is enabled here.
//! * `VK_KHR_external_memory_win32` imports the shared D3D11 texture and
//!   `VK_KHR_win32_keyed_mutex` orders GPU access to it across the two APIs.

use std::ffi::{CStr, c_char};

use ash::khr::{external_memory_win32, surface, swapchain, win32_keyed_mutex, win32_surface};
use ash::vk;
use ash::{Device, Entry, Instance};
use log::info;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;

use crate::software_renderer::vk_renderer::queue_guard::{init_real_fns, lock_queue};

/// Vulkan API version requested for the instance and reported to the engine.
pub(crate) const VULKAN_API_VERSION: u32 = vk::API_VERSION_1_1;

/// Pixel format of every image in the frame hand-off. Matches the host's
/// `DXGI_FORMAT_B8G8R8A8_UNORM` textures so the copy is format-identical.
pub(crate) const FRAME_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Handle type of the shared texture: an NT handle to an `ID3D11Texture2D`.
pub(crate) const SHARED_HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags =
    vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;

/// Usage of the imported shared image. It is only ever a copy destination.
pub(crate) const SHARED_IMAGE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::TRANSFER_DST;

const INSTANCE_EXTENSIONS: [&CStr; 2] = [surface::NAME, win32_surface::NAME];
const REQUIRED_DEVICE_EXTENSIONS: [&CStr; 1] = [swapchain::NAME];
const SHARED_TEXTURE_DEVICE_EXTENSIONS: [&CStr; 2] =
    [external_memory_win32::NAME, win32_keyed_mutex::NAME];
/// Must be enabled whenever the device offers it (MoltenVK and other layered
/// implementations).
const PORTABILITY_SUBSET_EXTENSION: &CStr = c"VK_KHR_portability_subset";

/// How finished frames get from Vulkan to the host's D3D11 device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffMode {
    /// A D3D11 texture imported into Vulkan, ordered by a keyed mutex. Zero CPU
    /// copies. Needs a native Windows driver stack.
    SharedTexture,
    /// The frame is read back into host memory and uploaded into the host's
    /// texture. Works wherever Vulkan works at all (Wine/Proton, MoltenVK).
    Readback,
}

/// Embedder-owned Vulkan objects. Must outlive the engine that borrows them.
pub struct VulkanContext {
    _entry: Entry,
    instance: Instance,
    physical_device: vk::PhysicalDevice,
    device: Device,
    queue: vk::Queue,
    queue_family_index: u32,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    handoff: HandoffMode,
    external_memory: Option<external_memory_win32::Device>,
    instance_extension_ptrs: Vec<*const c_char>,
    device_extension_ptrs: Vec<*const c_char>,
}

unsafe impl Send for VulkanContext {}
unsafe impl Sync for VulkanContext {}

impl VulkanContext {
    /// Creates the instance and a logical device, on the adapter `game_device`
    /// runs on when Vulkan can identify it. Picks [`HandoffMode::SharedTexture`]
    /// when the driver supports the D3D11 interop and `force_readback` is false,
    /// [`HandoffMode::Readback`] otherwise. Fails (so the caller can fall back to
    /// another renderer) only when Vulkan itself is unusable.
    pub fn new(game_device: &ID3D11Device, force_readback: bool) -> Result<Self, String> {
        unsafe {
            let entry =
                Entry::load().map_err(|e| format!("Vulkan loader (vulkan-1.dll) unavailable: {e}"))?;

            let loader_version = entry
                .try_enumerate_instance_version()
                .map_err(|e| format!("vkEnumerateInstanceVersion failed: {e}"))?
                .unwrap_or(vk::API_VERSION_1_0);
            if loader_version < VULKAN_API_VERSION {
                return Err(format!(
                    "Vulkan loader only supports {}.{}, 1.1 is required",
                    vk::api_version_major(loader_version),
                    vk::api_version_minor(loader_version)
                ));
            }

            let available_instance_extensions = entry
                .enumerate_instance_extension_properties(None)
                .map_err(|e| format!("vkEnumerateInstanceExtensionProperties failed: {e}"))?;
            require_extensions(&available_instance_extensions, &INSTANCE_EXTENSIONS, "instance")?;

            let instance_extension_ptrs: Vec<*const c_char> =
                INSTANCE_EXTENSIONS.iter().map(|name| name.as_ptr()).collect();

            let application_info = vk::ApplicationInfo::default()
                .application_name(c"flutter_rust_windows_embedder")
                .engine_name(c"flutter")
                .api_version(VULKAN_API_VERSION);
            let instance_info = vk::InstanceCreateInfo::default()
                .application_info(&application_info)
                .enabled_extension_names(&instance_extension_ptrs);
            let instance = entry
                .create_instance(&instance_info, None)
                .map_err(|e| format!("vkCreateInstance failed: {e}"))?;

            match Self::with_instance(
                entry,
                instance.clone(),
                instance_extension_ptrs,
                game_device,
                force_readback,
            ) {
                Ok(context) => Ok(context),
                Err(e) => {
                    instance.destroy_instance(None);
                    Err(e)
                }
            }
        }
    }

    unsafe fn with_instance(
        entry: Entry,
        instance: Instance,
        instance_extension_ptrs: Vec<*const c_char>,
        game_device: &ID3D11Device,
        force_readback: bool,
    ) -> Result<Self, String> {
        unsafe {
            let adapter_luid = d3d11_adapter_luid(game_device)?;
            let (physical_device, on_host_adapter) =
                find_physical_device(&instance, adapter_luid)?;

            let available_device_extensions = instance
                .enumerate_device_extension_properties(physical_device)
                .map_err(|e| format!("vkEnumerateDeviceExtensionProperties failed: {e}"))?;
            require_extensions(
                &available_device_extensions,
                &REQUIRED_DEVICE_EXTENSIONS,
                "device",
            )?;

            let shared_texture_blocker = if force_readback {
                Some("readback was requested".to_string())
            } else if !on_host_adapter {
                Some("Vulkan cannot identify the host's D3D11 adapter".to_string())
            } else {
                require_extensions(
                    &available_device_extensions,
                    &SHARED_TEXTURE_DEVICE_EXTENSIONS,
                    "device",
                )
                .and_then(|()| require_shared_image_support(&instance, physical_device))
                .err()
            };
            let handoff = match &shared_texture_blocker {
                None => HandoffMode::SharedTexture,
                Some(reason) => {
                    info!("[VulkanContext] Using the readback hand-off: {reason}.");
                    HandoffMode::Readback
                }
            };

            let mut device_extensions: Vec<&CStr> = REQUIRED_DEVICE_EXTENSIONS.to_vec();
            if handoff == HandoffMode::SharedTexture {
                device_extensions.extend(SHARED_TEXTURE_DEVICE_EXTENSIONS);
            }
            if require_extensions(
                &available_device_extensions,
                &[PORTABILITY_SUBSET_EXTENSION],
                "device",
            )
            .is_ok()
            {
                device_extensions.push(PORTABILITY_SUBSET_EXTENSION);
            }

            let queue_family_index = find_queue_family(&instance, physical_device)?;

            let mut supported_storage16 = vk::PhysicalDevice16BitStorageFeatures::default();
            let supported_core = {
                let mut supported = vk::PhysicalDeviceFeatures2::default()
                    .push_next(&mut supported_storage16);
                instance.get_physical_device_features2(physical_device, &mut supported);
                supported.features
            };

            let mut enabled_storage16 = vk::PhysicalDevice16BitStorageFeatures::default()
                .uniform_and_storage_buffer16_bit_access(
                    supported_storage16.uniform_and_storage_buffer16_bit_access == vk::TRUE,
                );
            let mut enabled_features = vk::PhysicalDeviceFeatures2::default()
                .features(supported_core.robust_buffer_access(false))
                .push_next(&mut enabled_storage16);

            let device_extension_ptrs: Vec<*const c_char> =
                device_extensions.iter().map(|name| name.as_ptr()).collect();
            let queue_priorities = [1.0f32];
            let queue_infos = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family_index)
                .queue_priorities(&queue_priorities)];
            let device_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_infos)
                .enabled_extension_names(&device_extension_ptrs)
                .push_next(&mut enabled_features);

            let device = instance
                .create_device(physical_device, &device_info, None)
                .map_err(|e| format!("vkCreateDevice failed: {e}"))?;

            if let Err(e) = init_real_fns(&entry, &instance) {
                device.destroy_device(None);
                return Err(e);
            }

            let queue = device.get_device_queue(queue_family_index, 0);
            let memory_properties = instance.get_physical_device_memory_properties(physical_device);
            let external_memory = (handoff == HandoffMode::SharedTexture)
                .then(|| external_memory_win32::Device::new(&instance, &device));

            let properties = instance.get_physical_device_properties(physical_device);
            info!(
                "[VulkanContext] Device '{}' ready (Vulkan {}.{}.{}, queue family {queue_family_index}, hand-off {handoff:?}).",
                properties
                    .device_name_as_c_str()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                vk::api_version_major(properties.api_version),
                vk::api_version_minor(properties.api_version),
                vk::api_version_patch(properties.api_version),
            );

            Ok(Self {
                _entry: entry,
                instance,
                physical_device,
                device,
                queue,
                queue_family_index,
                memory_properties,
                handoff,
                external_memory,
                instance_extension_ptrs,
                device_extension_ptrs,
            })
        }
    }

    pub(crate) fn instance(&self) -> &Instance {
        &self.instance
    }

    pub(crate) fn physical_device(&self) -> vk::PhysicalDevice {
        self.physical_device
    }

    pub(crate) fn device(&self) -> &Device {
        &self.device
    }

    pub(crate) fn queue(&self) -> vk::Queue {
        self.queue
    }

    pub(crate) fn queue_family_index(&self) -> u32 {
        self.queue_family_index
    }

    /// How frames are handed to the host on this device.
    pub fn handoff(&self) -> HandoffMode {
        self.handoff
    }

    /// `VK_KHR_external_memory_win32` entry points. `Some` only for
    /// [`HandoffMode::SharedTexture`].
    pub(crate) fn external_memory(&self) -> Option<&external_memory_win32::Device> {
        self.external_memory.as_ref()
    }

    /// Enabled instance extension names, in the layout the engine config wants.
    pub(crate) fn instance_extension_ptrs(&self) -> &[*const c_char] {
        &self.instance_extension_ptrs
    }

    /// Enabled device extension names, in the layout the engine config wants.
    pub(crate) fn device_extension_ptrs(&self) -> &[*const c_char] {
        &self.device_extension_ptrs
    }

    /// Picks a memory type allowed by `type_bits` that has all of `required`,
    /// preferring one that also has all of `preferred`.
    pub(crate) fn find_memory_type(
        &self,
        type_bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        select_memory_type(&self.memory_properties, type_bits, required, preferred)
    }

    /// Blocks until the device is idle, holding the queue lock for the duration.
    pub(crate) fn wait_idle(&self) -> Result<(), vk::Result> {
        let _guard = lock_queue();
        unsafe { self.device.device_wait_idle() }
    }
}

impl Drop for VulkanContext {
    fn drop(&mut self) {
        let _ = self.wait_idle();
        unsafe {
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// Pure memory-type selection, split out so it is testable without a device.
pub(crate) fn select_memory_type(
    memory_properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
    preferred: vk::MemoryPropertyFlags,
) -> Option<u32> {
    let flags = |index: u32| memory_properties.memory_types[index as usize].property_flags;
    let usable = |index: &u32| type_bits & (1u32 << index) != 0 && flags(*index).contains(required);
    let count = memory_properties.memory_type_count;
    (0..count)
        .filter(usable)
        .find(|&index| flags(index).contains(preferred))
        .or_else(|| (0..count).find(usable))
}

/// Serialises a D3D/DXGI `LUID` the way `VkPhysicalDeviceIDProperties::deviceLUID`
/// stores it: low part then high part, little-endian.
pub(crate) fn luid_to_bytes(low_part: u32, high_part: i32) -> [u8; vk::LUID_SIZE] {
    let mut bytes = [0u8; vk::LUID_SIZE];
    bytes[..4].copy_from_slice(&low_part.to_le_bytes());
    bytes[4..].copy_from_slice(&high_part.to_le_bytes());
    bytes
}

fn d3d11_adapter_luid(game_device: &ID3D11Device) -> Result<[u8; vk::LUID_SIZE], String> {
    let dxgi_device: IDXGIDevice = game_device
        .cast()
        .map_err(|e| format!("ID3D11Device is not an IDXGIDevice: {e}"))?;
    let adapter =
        unsafe { dxgi_device.GetAdapter() }.map_err(|e| format!("GetAdapter failed: {e}"))?;
    let desc = unsafe { adapter.GetDesc() }.map_err(|e| format!("GetDesc failed: {e}"))?;
    Ok(luid_to_bytes(desc.AdapterLuid.LowPart, desc.AdapterLuid.HighPart))
}

/// Returns the physical device on the host's D3D11 adapter and `true`, or, when
/// no device reports that LUID (layered stacks report none), the first device
/// and `false`.
unsafe fn find_physical_device(
    instance: &Instance,
    adapter_luid: [u8; vk::LUID_SIZE],
) -> Result<(vk::PhysicalDevice, bool), String> {
    unsafe {
        let physical_devices = instance
            .enumerate_physical_devices()
            .map_err(|e| format!("vkEnumeratePhysicalDevices failed: {e}"))?;

        for &physical_device in &physical_devices {
            let mut id_properties = vk::PhysicalDeviceIDProperties::default();
            {
                let mut properties =
                    vk::PhysicalDeviceProperties2::default().push_next(&mut id_properties);
                instance.get_physical_device_properties2(physical_device, &mut properties);
            }
            if id_properties.device_luid_valid == vk::TRUE
                && id_properties.device_luid == adapter_luid
            {
                return Ok((physical_device, true));
            }
        }

        physical_devices
            .first()
            .map(|&physical_device| (physical_device, false))
            .ok_or_else(|| "no Vulkan physical device available".to_string())
    }
}

unsafe fn find_queue_family(
    instance: &Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<u32, String> {
    let required = vk::QueueFlags::GRAPHICS | vk::QueueFlags::COMPUTE;
    unsafe { instance.get_physical_device_queue_family_properties(physical_device) }
        .iter()
        .position(|family| family.queue_count > 0 && family.queue_flags.contains(required))
        .map(|index| index as u32)
        .ok_or_else(|| "no graphics+compute queue family".to_string())
}

unsafe fn require_shared_image_support(
    instance: &Instance,
    physical_device: vk::PhysicalDevice,
) -> Result<(), String> {
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    {
        let mut external_info =
            vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(SHARED_HANDLE_TYPE);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(FRAME_FORMAT)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(SHARED_IMAGE_USAGE)
            .push_next(&mut external_info);
        let mut format_properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        unsafe {
            instance.get_physical_device_image_format_properties2(
                physical_device,
                &format_info,
                &mut format_properties,
            )
        }
        .map_err(|e| format!("D3D11 texture import is not supported for BGRA8: {e}"))?;
    }

    if !external_properties
        .external_memory_properties
        .external_memory_features
        .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
    {
        return Err("driver cannot import D3D11 textures into Vulkan".to_string());
    }
    Ok(())
}

fn require_extensions(
    available: &[vk::ExtensionProperties],
    required: &[&CStr],
    kind: &str,
) -> Result<(), String> {
    for name in required {
        let present = available
            .iter()
            .any(|extension| extension.extension_name_as_c_str().is_ok_and(|n| n == *name));
        if !present {
            return Err(format!(
                "required Vulkan {kind} extension {} is not available",
                name.to_string_lossy()
            ));
        }
    }
    Ok(())
}
