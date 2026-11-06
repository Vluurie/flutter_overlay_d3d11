//! A check, run once when an overlay starts, that the shared D3D11 texture really carries
//! pixels from Vulkan to the host.
//!
//! A driver stack can accept every step of the import and still never deliver a frame: the
//! host's keyed-mutex acquire never succeeds, or the host reads memory Vulkan did not write.
//! Wine/Proton behaves like this. Vulkan writes a pattern into a texture created exactly like
//! the frame hand-off one, the host reads it back, and the shared texture is only used when
//! every pixel arrived.

use std::slice;

use ash::vk;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::software_renderer::vk_renderer::context::VulkanContext;
use crate::software_renderer::vk_renderer::interop::{
    COLOR_SUBRESOURCE_LAYERS, COLOR_SUBRESOURCE_RANGE,
};
use crate::software_renderer::vk_renderer::queue_guard::lock_queue;
use crate::software_renderer::vk_renderer::shared_image::{
    KEY_READER, KEY_WRITER, SharedFrameTarget,
};

/// Edge of the square the pattern is written into, at the texture's top-left corner.
const PATTERN_EDGE: u32 = 64;
/// How long either side may wait for the keyed mutex. The check runs once per overlay.
const ACQUIRE_TIMEOUT_MS: u32 = 1000;
/// How long the host waits for the pattern upload to finish.
const UPLOAD_TIMEOUT_NS: u64 = 2_000_000_000;

/// Whether a `width`x`height` shared texture delivers what Vulkan writes into it. Runs on the
/// host's render thread, which owns `game_device`'s immediate context.
pub(crate) fn shared_texture_works(
    context: &VulkanContext,
    game_device: &ID3D11Device,
    width: u32,
    height: u32,
) -> bool {
    let edge = PATTERN_EDGE.min(width).min(height);
    if edge == 0 {
        return false;
    }
    let Ok(target) = SharedFrameTarget::new(context, game_device, width, height) else {
        return false;
    };

    let mut objects = CheckObjects::default();
    let works = match unsafe { upload_pattern(context, &target, edge, &mut objects) } {
        // The GPU may still use the texture and the upload objects, so they are leaked.
        Upload::Unfinished => return false,
        Upload::NotSubmitted => false,
        Upload::Finished => host_reads_pattern(game_device, &target, edge),
    };
    objects.release(context);
    target.destroy(context);
    works
}

/// BGRA bytes of the pattern at `x`, `y`. Every pixel of the square differs, and none is
/// all zero, so neither untouched memory nor a reordered copy passes.
fn pattern_pixel(x: u32, y: u32) -> [u8; 4] {
    [(x * 4) as u8, (y * 4) as u8, ((x ^ y) * 4) as u8, 0xff]
}

/// How far the pattern upload got.
enum Upload {
    /// Failed before anything reached the GPU.
    NotSubmitted,
    /// Submitted, but the GPU did not report it finished.
    Unfinished,
    /// The pattern is in the texture and the keyed mutex is on [`KEY_READER`].
    Finished,
}

/// Vulkan objects of one check. Null handles are valid to destroy, so a partly built set
/// is released the same way.
#[derive(Default)]
struct CheckObjects {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    command_pool: vk::CommandPool,
    fence: vk::Fence,
}

impl CheckObjects {
    fn release(self, context: &VulkanContext) {
        let device = context.device();
        unsafe {
            device.destroy_fence(self.fence, None);
            device.destroy_command_pool(self.command_pool, None);
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
    }
}

/// Copies the pattern into the shared image under the keyed mutex, as a frame copy does,
/// and waits for the GPU.
unsafe fn upload_pattern(
    context: &VulkanContext,
    target: &SharedFrameTarget,
    edge: u32,
    objects: &mut CheckObjects,
) -> Upload {
    let device = context.device();
    let pixel_count = (edge * edge) as usize;
    let size = pixel_count as u64 * 4;

    unsafe {
        let buffer_info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let Ok(buffer) = device.create_buffer(&buffer_info, None) else {
            return Upload::NotSubmitted;
        };
        objects.buffer = buffer;

        let requirements = device.get_buffer_memory_requirements(buffer);
        let Some(memory_type_index) = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
            vk::MemoryPropertyFlags::empty(),
        ) else {
            return Upload::NotSubmitted;
        };
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let Ok(memory) = device.allocate_memory(&allocate_info, None) else {
            return Upload::NotSubmitted;
        };
        objects.memory = memory;
        if device.bind_buffer_memory(buffer, memory, 0).is_err() {
            return Upload::NotSubmitted;
        }

        let Ok(mapped) = device.map_memory(memory, 0, size, vk::MemoryMapFlags::empty()) else {
            return Upload::NotSubmitted;
        };
        let pixels = slice::from_raw_parts_mut(mapped.cast::<[u8; 4]>(), pixel_count);
        for y in 0..edge {
            for x in 0..edge {
                pixels[(y * edge + x) as usize] = pattern_pixel(x, y);
            }
        }
        device.unmap_memory(memory);

        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::TRANSIENT)
            .queue_family_index(context.queue_family_index());
        let Ok(command_pool) = device.create_command_pool(&pool_info, None) else {
            return Upload::NotSubmitted;
        };
        objects.command_pool = command_pool;

        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let Ok(command_buffers) = device.allocate_command_buffers(&allocate_info) else {
            return Upload::NotSubmitted;
        };
        let command_buffer = command_buffers[0];

        let Ok(fence) = device.create_fence(&vk::FenceCreateInfo::default(), None) else {
            return Upload::NotSubmitted;
        };
        objects.fence = fence;

        if record_pattern_upload(context, command_buffer, buffer, target.vk_image, edge).is_err() {
            return Upload::NotSubmitted;
        }

        let memories = [target.vk_memory];
        let acquire_keys = [KEY_WRITER];
        let acquire_timeouts = [ACQUIRE_TIMEOUT_MS];
        let release_keys = [KEY_READER];
        let mut keyed_mutex_info = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
            .acquire_syncs(&memories)
            .acquire_keys(&acquire_keys)
            .acquire_timeouts(&acquire_timeouts)
            .release_syncs(&memories)
            .release_keys(&release_keys);
        let submit_buffers = [command_buffer];
        let submit = vk::SubmitInfo::default()
            .command_buffers(&submit_buffers)
            .push_next(&mut keyed_mutex_info);

        let submitted = {
            let _queue = lock_queue();
            device.queue_submit(context.queue(), &[submit], fence)
        };
        // A failed submit may still have reached the queue (device loss), so nothing is
        // released after it.
        if submitted.is_err() {
            return Upload::Unfinished;
        }

        match device.wait_for_fences(&[fence], true, UPLOAD_TIMEOUT_NS) {
            Ok(()) => Upload::Finished,
            Err(_) => Upload::Unfinished,
        }
    }
}

/// Records: shared image taken over from the external owner → `TRANSFER_DST`, pattern copy
/// into the top-left `edge`x`edge` square, shared image handed back to the external owner.
unsafe fn record_pattern_upload(
    context: &VulkanContext,
    command_buffer: vk::CommandBuffer,
    buffer: vk::Buffer,
    shared_image: vk::Image,
    edge: u32,
) -> Result<(), vk::Result> {
    let device = context.device();
    let family = context.queue_family_index();

    unsafe {
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device.begin_command_buffer(command_buffer, &begin_info)?;

        let take_over = vk::ImageMemoryBarrier::default()
            .image(shared_image)
            .subresource_range(COLOR_SUBRESOURCE_RANGE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(family);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[take_over],
        );

        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(COLOR_SUBRESOURCE_LAYERS)
            .image_extent(vk::Extent3D {
                width: edge,
                height: edge,
                depth: 1,
            });
        device.cmd_copy_buffer_to_image(
            command_buffer,
            buffer,
            shared_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[region],
        );

        let hand_back = vk::ImageMemoryBarrier::default()
            .image(shared_image)
            .subresource_range(COLOR_SUBRESOURCE_RANGE)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::empty())
            .src_queue_family_index(family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::BOTTOM_OF_PIPE,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[hand_back],
        );

        device.end_command_buffer(command_buffer)
    }
}

/// Host side: takes the texture with the reader key, copies the pattern square into a
/// staging texture and compares every pixel.
fn host_reads_pattern(game_device: &ID3D11Device, target: &SharedFrameTarget, edge: u32) -> bool {
    let Ok(immediate) = (unsafe { game_device.GetImmediateContext() }) else {
        return false;
    };
    let Some(staging) = create_staging_texture(game_device, edge) else {
        return false;
    };

    if !target.acquire_for_host(ACQUIRE_TIMEOUT_MS) {
        return false;
    }
    let source_box = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: edge,
        bottom: edge,
        back: 1,
    };
    unsafe {
        immediate.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            &target.d3d_texture,
            0,
            Some(&source_box),
        );
    }
    target.release_from_host();

    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if unsafe { immediate.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_err() {
        return false;
    }
    let matches = (0..edge).all(|y| {
        let row = unsafe {
            let start = mapped.pData.cast::<u8>().add(y as usize * mapped.RowPitch as usize);
            slice::from_raw_parts(start.cast::<[u8; 4]>(), edge as usize)
        };
        row.iter()
            .zip(0..edge)
            .all(|(pixel, x)| *pixel == pattern_pixel(x, y))
    });
    unsafe { immediate.Unmap(&staging, 0) };
    matches
}

fn create_staging_texture(game_device: &ID3D11Device, edge: u32) -> Option<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: edge,
        Height: edge,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { game_device.CreateTexture2D(&desc, None, Some(&mut texture)) }.ok()?;
    texture
}
