//! The cross-API frame hand-off surface: one D3D11 texture, visible to both APIs.
//!
//! The texture is created **on the host's D3D11 device** (no second D3D11 device,
//! no ANGLE) with an NT shared handle and a keyed mutex, then imported into
//! Vulkan as a `VkImage` through `VK_KHR_external_memory_win32`. Vulkan only ever
//! copies into it; the host only ever copies out of it.
//!
//! GPU ordering between the two APIs is the keyed mutex, driven with two keys:
//!
//! * [`KEY_WRITER`] — the image is free for Vulkan to write (initial state, and
//!   the state after the host consumed a frame).
//! * [`KEY_READER`] — a finished frame is waiting for the host.
//!
//! Vulkan takes part through `VkWin32KeyedMutexAcquireReleaseInfoKHR` on its copy
//! submit, the host through `IDXGIKeyedMutex`. Because a keyed mutex may be
//! re-acquired with the key it was last released with, the writer can overwrite a
//! frame the host never picked up ([`KEY_READER`] → [`KEY_READER`]); it never has
//! to wait for the host's frame cadence.

use ash::vk;
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::core::Interface;

use crate::software_renderer::vk_renderer::context::{
    FRAME_FORMAT, SHARED_HANDLE_TYPE, SHARED_IMAGE_USAGE, VulkanContext,
};

/// Keyed-mutex key meaning "free for the Vulkan writer".
pub(crate) const KEY_WRITER: u64 = 0;
/// Keyed-mutex key meaning "a finished frame is waiting for the host reader".
pub(crate) const KEY_READER: u64 = 1;

/// Closes the wrapped NT handle on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// A host D3D11 texture plus its Vulkan import. Vulkan objects are released with
/// [`SharedFrameTarget::destroy`], which needs the owning [`VulkanContext`].
pub(crate) struct SharedFrameTarget {
    pub(crate) d3d_texture: ID3D11Texture2D,
    pub(crate) keyed_mutex: IDXGIKeyedMutex,
    pub(crate) vk_image: vk::Image,
    pub(crate) vk_memory: vk::DeviceMemory,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

unsafe impl Send for SharedFrameTarget {}

impl SharedFrameTarget {
    pub(crate) fn new(
        context: &VulkanContext,
        game_device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let (d3d_texture, keyed_mutex, handle) = create_d3d_texture(game_device, width, height)?;
        let (vk_image, vk_memory) = import_into_vulkan(context, handle.0, width, height)?;
        Ok(Self {
            d3d_texture,
            keyed_mutex,
            vk_image,
            vk_memory,
            width,
            height,
        })
    }

    /// Releases the Vulkan side. The caller guarantees the GPU no longer uses it.
    pub(crate) fn destroy(self, context: &VulkanContext) {
        unsafe {
            context.device().destroy_image(self.vk_image, None);
            context.device().free_memory(self.vk_memory, None);
        }
    }

    /// Host-side acquire. Returns `true` only on `S_OK`: `AcquireSync` reports
    /// `WAIT_TIMEOUT` / `WAIT_ABANDONED` as *success* HRESULTs, so the wrapped
    /// `Result` cannot be used to tell whether the mutex is actually held.
    pub(crate) fn acquire_for_host(&self, timeout_ms: u32) -> bool {
        let mutex = &self.keyed_mutex;
        let hresult = unsafe {
            (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), KEY_READER, timeout_ms)
        };
        hresult == S_OK
    }

    /// Host-side release, handing the image back to the Vulkan writer.
    pub(crate) fn release_from_host(&self) {
        unsafe {
            let _ = self.keyed_mutex.ReleaseSync(KEY_WRITER);
        }
    }
}

fn create_d3d_texture(
    game_device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, IDXGIKeyedMutex, OwnedHandle), String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
            | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
    };

    unsafe {
        let mut texture: Option<ID3D11Texture2D> = None;
        game_device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .map_err(|e| format!("CreateTexture2D (shared, {width}x{height}) failed: {e}"))?;
        let texture = texture.ok_or_else(|| "CreateTexture2D returned no texture".to_string())?;

        let keyed_mutex: IDXGIKeyedMutex = texture
            .cast()
            .map_err(|e| format!("shared texture has no IDXGIKeyedMutex: {e}"))?;
        let resource: IDXGIResource1 = texture
            .cast()
            .map_err(|e| format!("shared texture has no IDXGIResource1: {e}"))?;
        let handle = resource
            .CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )
            .map_err(|e| format!("CreateSharedHandle failed: {e}"))?;

        Ok((texture, keyed_mutex, OwnedHandle(handle)))
    }
}

fn import_into_vulkan(
    context: &VulkanContext,
    handle: HANDLE,
    width: u32,
    height: u32,
) -> Result<(vk::Image, vk::DeviceMemory), String> {
    let device = context.device();

    unsafe {
        let mut external_info =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(SHARED_HANDLE_TYPE);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FRAME_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(SHARED_IMAGE_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_info);
        let image = device
            .create_image(&image_info, None)
            .map_err(|e| format!("vkCreateImage (shared import) failed: {e}"))?;

        let requirements = device.get_image_memory_requirements(image);

        let Some(external_memory) = context.external_memory() else {
            device.destroy_image(image, None);
            return Err("this Vulkan device was created without D3D11 interop".to_string());
        };
        let mut handle_properties = vk::MemoryWin32HandlePropertiesKHR::default();
        let handle_type_bits = external_memory
            .get_memory_win32_handle_properties(
                SHARED_HANDLE_TYPE,
                handle.0 as vk::HANDLE,
                &mut handle_properties,
            )
            .map(|()| handle_properties.memory_type_bits)
            .unwrap_or(u32::MAX);

        let Some(memory_type_index) = context.find_memory_type(
            requirements.memory_type_bits & handle_type_bits,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) else {
            device.destroy_image(image, None);
            return Err("no Vulkan memory type can back the imported D3D11 texture".to_string());
        };

        let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
            .handle_type(SHARED_HANDLE_TYPE)
            .handle(handle.0 as vk::HANDLE);
        let mut dedicated_info = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated_info);

        let memory = match device.allocate_memory(&allocate_info, None) {
            Ok(memory) => memory,
            Err(e) => {
                device.destroy_image(image, None);
                return Err(format!("vkAllocateMemory (D3D11 import) failed: {e}"));
            }
        };

        if let Err(e) = device.bind_image_memory(image, memory, 0) {
            device.destroy_image(image, None);
            device.free_memory(memory, None);
            return Err(format!("vkBindImageMemory (D3D11 import) failed: {e}"));
        }

        Ok((image, memory))
    }
}
