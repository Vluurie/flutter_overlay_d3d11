//! The hand-off surface for stacks without Vulkan↔D3D11 interop: host memory.
//!
//! Wine/Proton and MoltenVK run Vulkan fine but cannot share a texture with the
//! host's D3D11 device. There the finished frame is copied into a host-visible
//! buffer and the host uploads it into its own texture. Rasterisation stays on
//! the GPU (so Impeller and fragment shaders work), only the transfer crosses the
//! CPU — the same upload the software renderer already pays, without its CPU
//! rasterisation.
//!
//! Two buffers alternate. The GPU only ever writes the *back* buffer; once that
//! copy has finished the raster thread swaps it to *front* under the frame lock,
//! which the host also holds while it reads. So the host never sees a buffer the
//! GPU is writing and no cross-API synchronisation is needed.

use std::ffi::c_void;

use ash::vk;

use crate::software_renderer::vk_renderer::context::VulkanContext;

const BYTES_PER_PIXEL: u64 = 4;

struct ReadbackBuffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *mut c_void,
}

/// Two persistently mapped BGRA8 frame buffers of `width`x`height`.
pub(crate) struct ReadbackTarget {
    buffers: [ReadbackBuffer; 2],
    front: usize,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

unsafe impl Send for ReadbackTarget {}

impl ReadbackTarget {
    pub(crate) fn new(context: &VulkanContext, width: u32, height: u32) -> Result<Self, String> {
        let size = u64::from(width) * u64::from(height) * BYTES_PER_PIXEL;
        let first = create_buffer(context, size)?;
        let second = match create_buffer(context, size) {
            Ok(buffer) => buffer,
            Err(e) => {
                destroy_buffer(context, first);
                return Err(e);
            }
        };
        Ok(Self {
            buffers: [first, second],
            front: 0,
            width,
            height,
        })
    }

    /// Releases both buffers. The caller guarantees the GPU no longer uses them.
    pub(crate) fn destroy(self, context: &VulkanContext) {
        for buffer in self.buffers {
            destroy_buffer(context, buffer);
        }
    }

    /// Index of the buffer the GPU may write next.
    pub(crate) fn back_index(&self) -> usize {
        1 - self.front
    }

    pub(crate) fn buffer(&self, index: usize) -> vk::Buffer {
        self.buffers[index].buffer
    }

    /// Makes the CPU view of buffer `index` current after the GPU wrote it.
    pub(crate) fn sync_to_host(&self, context: &VulkanContext, index: usize) -> Result<(), vk::Result> {
        let range = vk::MappedMemoryRange::default()
            .memory(self.buffers[index].memory)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        unsafe { context.device().invalidate_mapped_memory_ranges(&[range]) }
    }

    /// Publishes buffer `index` as the frame the host reads.
    pub(crate) fn set_front(&mut self, index: usize) {
        self.front = index;
    }

    /// Pixels of the published frame: tightly packed BGRA8, `width * 4` bytes per row.
    pub(crate) fn front_pixels(&self) -> *const c_void {
        self.buffers[self.front].mapped
    }

    pub(crate) fn row_pitch(&self) -> u32 {
        self.width * BYTES_PER_PIXEL as u32
    }
}

fn create_buffer(context: &VulkanContext, size: u64) -> Result<ReadbackBuffer, String> {
    let device = context.device();
    unsafe {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = device
            .create_buffer(&buffer_info, None)
            .map_err(|e| format!("vkCreateBuffer (readback, {size} bytes) failed: {e}"))?;

        let requirements = device.get_buffer_memory_requirements(buffer);
        let Some(memory_type_index) = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_CACHED,
        ) else {
            device.destroy_buffer(buffer, None);
            return Err("no host-visible Vulkan memory type for the readback buffer".to_string());
        };

        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match device.allocate_memory(&allocate_info, None) {
            Ok(memory) => memory,
            Err(e) => {
                device.destroy_buffer(buffer, None);
                return Err(format!("vkAllocateMemory (readback buffer) failed: {e}"));
            }
        };

        let mapped = device
            .bind_buffer_memory(buffer, memory, 0)
            .and_then(|()| device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()));
        match mapped {
            Ok(mapped) => Ok(ReadbackBuffer {
                buffer,
                memory,
                mapped,
            }),
            Err(e) => {
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
                Err(format!("binding/mapping the readback buffer failed: {e}"))
            }
        }
    }
}

fn destroy_buffer(context: &VulkanContext, buffer: ReadbackBuffer) {
    unsafe {
        context.device().destroy_buffer(buffer.buffer, None);
        context.device().free_memory(buffer.memory, None);
    }
}
