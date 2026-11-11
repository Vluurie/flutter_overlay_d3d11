//! Frame flow between the engine's Vulkan renderer and the host's D3D11 device.
//!
//! ```text
//!  engine (raster thread)                          host (render thread)
//!  ─────────────────────                           ────────────────────
//!  get_next_image ─► private render VkImage
//!        engine draws the frame into it
//!  present_image  ─► vkCmdCopyImage ─► shared image ══ same memory ══ D3D11 texture
//!                    (keyed mutex: acquire W|R, release R)             │
//!                                                   tick: acquire R ◄──┘
//!                                                         CopyResource ─► overlay.texture
//!                                                         release W
//! ```
//!
//! The engine never renders into the shared image directly. It renders into a
//! private image and the embedder copies the finished frame across in its own
//! submit, because that submit is the only place the embedder can attach the
//! keyed-mutex acquire/release (the engine's submits are not ours to decorate).
//! The copy is a same-GPU blit and buys two properties:
//!
//! * The engine is never blocked by the host's frame cadence. If the host did not
//!   pick up the previous frame, the copy re-acquires the mutex with the reader
//!   key and overwrites it; the host simply sees the newest frame.
//! * Impeller performs no host sync before `present_image` and leaves ordering to
//!   the embedder. Submitting the copy on the same queue orders it after the
//!   engine's work, and the keyed mutex orders it against the host's read.
//!
//! [`SharedFrameState`] is the only state touched by both threads; its mutex also
//! makes "which key is the mutex on" a CPU-side fact (`presented > consumed`).

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ash::vk::{self, Handle};
use log::{error, info, warn};
use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3, IDXGIDevice,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::core::Interface;

use crate::bindings::embedder::{
    FlutterFrameInfo, FlutterRendererConfig, FlutterRendererConfig__bindgen_ty_1,
    FlutterRendererType_kVulkan, FlutterVulkanImage, FlutterVulkanRendererConfig,
};
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::vk_renderer::context::{
    FRAME_FORMAT, HandoffMode, VULKAN_API_VERSION, VulkanContext,
};
use crate::software_renderer::vk_renderer::queue_guard::{
    get_instance_proc_address_callback, lock_queue,
};
use crate::software_renderer::vk_renderer::readback::ReadbackTarget;
use crate::software_renderer::vk_renderer::shared_check::shared_texture_works;
use crate::software_renderer::vk_renderer::shared_image::{
    KEY_READER, KEY_WRITER, SharedFrameTarget,
};

/// How long the copy submit may wait for the keyed mutex.
const WRITER_ACQUIRE_TIMEOUT_MS: u32 = 1000;
/// How long the host waits for a presented frame's GPU copy to finish. Kept
/// under a display frame: on timeout the host keeps its previous texture and
/// retries on its next tick.
const HOST_ACQUIRE_TIMEOUT_MS: u32 = 16;
/// How long to wait for the previous copy submit before reusing its command buffer.
const COPY_FENCE_TIMEOUT_NS: u64 = 2_000_000_000;
/// Once a limit was hit, frames are requested again only when this much more is free, so the
/// engine is not asked for a frame on every flicker around the limit.
const RESUME_HEADROOM: u64 = 64 * 1024 * 1024;
/// System commit that must stay available for the engine's allocations. Video memory allocations
/// are charged to the commit as well, so a full commit fails them even with video memory to spare.
const COMMIT_HEADROOM: u64 = 1024 * 1024 * 1024;

const RENDER_IMAGE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
        | vk::ImageUsageFlags::INPUT_ATTACHMENT.as_raw()
        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw()
        | vk::ImageUsageFlags::TRANSFER_DST.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw(),
);

pub(crate) const COLOR_SUBRESOURCE_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

pub(crate) const COLOR_SUBRESOURCE_LAYERS: vk::ImageSubresourceLayers = vk::ImageSubresourceLayers {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    mip_level: 0,
    base_array_layer: 0,
    layer_count: 1,
};

/// The private image the engine renders each frame into.
struct RenderImage {
    image: vk::Image,
    memory: vk::DeviceMemory,
    width: u32,
    height: u32,
}

/// State only the engine's raster thread touches.
struct RasterState {
    render_image: Option<RenderImage>,
    command_pool: vk::CommandPool,
    command_buffer: vk::CommandBuffer,
    copy_fence: vk::Fence,
    copy_in_flight: bool,
}

/// Environment variable that forces [`HandoffMode::Readback`] when set to a
/// non-empty value, for diagnosing the shared-texture path in the field.
pub const FORCE_READBACK_ENV: &str = "FLUTTER_EMBEDDER_VK_FORCE_READBACK";

/// Where finished frames are handed to the host, per [`HandoffMode`].
enum FrameTarget {
    Shared(SharedFrameTarget),
    Readback(ReadbackTarget),
}

impl FrameTarget {
    fn new(
        context: &VulkanContext,
        game_device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        match context.handoff() {
            HandoffMode::SharedTexture => {
                SharedFrameTarget::new(context, game_device, width, height).map(Self::Shared)
            }
            HandoffMode::Readback => {
                ReadbackTarget::new(context, width, height).map(Self::Readback)
            }
        }
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Self::Shared(target) => (target.width, target.height),
            Self::Readback(target) => (target.width, target.height),
        }
    }

    fn destroy(self, context: &VulkanContext) {
        match self {
            Self::Shared(target) => target.destroy(context),
            Self::Readback(target) => target.destroy(context),
        }
    }
}

/// State shared between the raster thread (writer) and the host thread (reader).
struct SharedFrameState {
    target: Option<FrameTarget>,
    presented: u64,
    consumed: u64,
}

/// Which keyed-mutex key the writer must acquire: the reader key when the host
/// has not consumed the last presented frame, the writer key otherwise.
pub(crate) fn writer_acquire_key(presented: u64, consumed: u64) -> u64 {
    if presented > consumed {
        KEY_READER
    } else {
        KEY_WRITER
    }
}

/// Clamps a host copy to the area both textures cover. `None` when empty.
pub(crate) fn host_copy_extent(source: (u32, u32), destination: (u32, u32)) -> Option<(u32, u32)> {
    let width = source.0.min(destination.0);
    let height = source.1.min(destination.1);
    (width > 0 && height > 0).then_some((width, height))
}

/// Everything the Vulkan renderer of one overlay owns.
pub struct VulkanInteropState {
    game_device: ID3D11Device,
    /// The host adapter, for its video-memory budget. `None` when DXGI cannot report one.
    adapter: Option<IDXGIAdapter3>,
    raster: Mutex<RasterState>,
    shared: Mutex<SharedFrameState>,
    frames_presented: AtomicU64,
    device_lost: AtomicBool,
    /// The adapter's local memory is at its budget; no frame is requested from the engine.
    budget_exhausted: AtomicBool,
    context: VulkanContext,
}

unsafe impl Send for VulkanInteropState {}
unsafe impl Sync for VulkanInteropState {}

impl VulkanInteropState {
    /// Brings up Vulkan on `game_device`'s adapter and allocates the first
    /// render + shared images at `width`x`height`, so every interop failure
    /// surfaces here (where the caller can still fall back to another renderer)
    /// rather than on the raster thread.
    pub fn new(game_device: &ID3D11Device, width: u32, height: u32) -> Result<Box<Self>, String> {
        let force_readback = std::env::var(FORCE_READBACK_ENV).is_ok_and(|value| !value.is_empty());
        Self::with_handoff(game_device, width, height, force_readback)
    }

    /// Like [`new`](Self::new), with the readback hand-off forced on request
    /// instead of read from [`FORCE_READBACK_ENV`].
    pub(crate) fn with_handoff(
        game_device: &ID3D11Device,
        width: u32,
        height: u32,
        force_readback: bool,
    ) -> Result<Box<Self>, String> {
        let mut context = VulkanContext::new(game_device, force_readback)?;
        if context.handoff() == HandoffMode::SharedTexture
            && !shared_texture_works(&context, game_device, width, height)
        {
            context.use_readback();
        }

        let (command_pool, command_buffer, copy_fence) = create_copy_objects(&context)?;
        let state = Box::new(Self {
            game_device: game_device.clone(),
            adapter: dxgi_adapter(game_device),
            raster: Mutex::new(RasterState {
                render_image: None,
                command_pool,
                command_buffer,
                copy_fence,
                copy_in_flight: false,
            }),
            shared: Mutex::new(SharedFrameState {
                target: None,
                presented: 0,
                consumed: 0,
            }),
            frames_presented: AtomicU64::new(0),
            device_lost: AtomicBool::new(false),
            budget_exhausted: AtomicBool::new(false),
            context,
        });

        state.next_image(width, height)?;
        info!(
            "[VulkanInterop] Vulkan renderer ready at {width}x{height}, hand-off {:?}.",
            state.handoff()
        );
        Ok(state)
    }

    /// How this overlay's frames reach the host.
    pub fn handoff(&self) -> HandoffMode {
        self.context.handoff()
    }

    /// Number of frames handed to the host side so far.
    pub fn frames_presented(&self) -> u64 {
        self.frames_presented.load(Ordering::Acquire)
    }

    /// True once a submit reported `VK_ERROR_DEVICE_LOST`.
    pub fn is_device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Acquire)
    }

    /// True while the adapter's local memory is at its budget or the system commit is nearly
    /// gone, as of the last [`Self::update_memory_state`]. The engine's own allocator throws
    /// when an allocation fails, which the embedder cannot catch, so no frame is handed out
    /// while one would fail.
    pub fn memory_exhausted(&self) -> bool {
        self.budget_exhausted.load(Ordering::Acquire)
    }

    /// Measures the adapter's local memory against its budget and the free system commit and
    /// stores the result for [`Self::memory_exhausted`]. Rendering counts as fitting again
    /// once [`RESUME_HEADROOM`] more is free. Returns the new state on a transition, `None`
    /// while it holds; each transition is logged once.
    pub fn update_memory_state(&self) -> Option<bool> {
        let was = self.budget_exhausted.load(Ordering::Acquire);
        let resume = if was { RESUME_HEADROOM } else { 0 };

        let video = self.adapter.as_ref().and_then(|adapter| {
            let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            unsafe { adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info) }
                .ok()?;
            (info.Budget > 0).then_some((info.CurrentUsage, info.Budget))
        });
        let video_full = video
            .is_some_and(|(usage, budget)| usage.saturating_add(resume) >= budget);

        let commit_free = commit_available();
        let commit_full =
            commit_free.is_some_and(|free| free < COMMIT_HEADROOM.saturating_add(resume));

        let now = video_full || commit_full;
        if now == was {
            return None;
        }
        self.budget_exhausted.store(now, Ordering::Release);
        let (usage_mb, budget_mb) = video.map_or((0, 0), |(u, b)| (u >> 20, b >> 20));
        let commit_mb = commit_free.map_or(0, |free| free >> 20);
        if now {
            warn!(
                "[VulkanInterop] Memory exhausted (video {usage_mb} MB of {budget_mb} MB, commit free {commit_mb} MB); overlay frames paused."
            );
        } else {
            info!(
                "[VulkanInterop] Memory available again (video {usage_mb} MB of {budget_mb} MB, commit free {commit_mb} MB); overlay frames resume."
            );
        }
        Some(now)
    }

    /// The Vulkan objects lent to the engine.
    pub(crate) fn context(&self) -> &VulkanContext {
        &self.context
    }

    /// Returns the image the engine should render a `width`x`height` frame into,
    /// (re)allocating the render and shared images when the size changed.
    pub(crate) fn next_image(&self, width: u32, height: u32) -> Result<vk::Image, String> {
        if width == 0 || height == 0 {
            return Err(format!("empty frame size {width}x{height}"));
        }

        let mut raster = self.raster.lock();
        if let Some(current) = &raster.render_image
            && current.width == width
            && current.height == height
        {
            return Ok(current.image);
        }

        self.context
            .wait_idle()
            .map_err(|e| self.note_failure("vkDeviceWaitIdle before resize", e))?;
        raster.copy_in_flight = false;

        if let Some(old) = raster.render_image.take() {
            destroy_render_image(&self.context, old);
        }

        let render_image = create_render_image(&self.context, width, height)?;
        let image = render_image.image;
        raster.render_image = Some(render_image);

        let new_target = FrameTarget::new(&self.context, &self.game_device, width, height)?;
        let old_target = {
            let mut shared = self.shared.lock();
            shared.consumed = shared.presented;
            shared.target.replace(new_target)
        };
        if let Some(old_target) = old_target {
            old_target.destroy(&self.context);
        }

        Ok(image)
    }

    /// Copies the finished frame in `image` into the shared image and publishes it.
    /// `image` must be in `COLOR_ATTACHMENT_OPTIMAL`, as both engine backends leave it.
    pub(crate) fn present(&self, image: vk::Image) -> Result<(), String> {
        let mut raster = self.raster.lock();
        let (render, width, height) = match &raster.render_image {
            Some(current) if current.image == image => {
                (current.image, current.width, current.height)
            }
            _ => return Err("engine presented an image the embedder did not hand out".to_string()),
        };

        let device = self.context.device();
        unsafe {
            if raster.copy_in_flight {
                device
                    .wait_for_fences(&[raster.copy_fence], true, COPY_FENCE_TIMEOUT_NS)
                    .map_err(|e| self.note_failure("vkWaitForFences (previous copy)", e))?;
                raster.copy_in_flight = false;
            }
            device
                .reset_fences(&[raster.copy_fence])
                .map_err(|e| self.note_failure("vkResetFences", e))?;
        }

        match self.context.handoff() {
            HandoffMode::SharedTexture => {
                self.present_to_shared_texture(&mut raster, render, width, height)?;
            }
            HandoffMode::Readback => self.present_to_readback(&raster, render, width, height)?,
        }
        self.frames_presented.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Shared-texture hand-off: copy into the imported D3D11 texture under the
    /// keyed mutex. Asynchronous; the keyed mutex orders the host's read.
    fn present_to_shared_texture(
        &self,
        raster: &mut RasterState,
        render: vk::Image,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let device = self.context.device();
        let mut shared = self.shared.lock();
        let (shared_image, shared_memory) = match &shared.target {
            Some(FrameTarget::Shared(target))
                if target.width == width && target.height == height =>
            {
                (target.vk_image, target.vk_memory)
            }
            _ => return Err("shared frame target does not match the render image".to_string()),
        };

        unsafe {
            record_frame_copy(
                &self.context,
                raster.command_buffer,
                render,
                shared_image,
                width,
                height,
            )
            .map_err(|e| self.note_failure("recording the frame copy", e))?;

            let submit_description = format!(
                "vkQueueSubmit (frame copy, {width}x{height}, presented {}, consumed {})",
                shared.presented, shared.consumed
            );
            let memories = [shared_memory];
            let acquire_keys = [writer_acquire_key(shared.presented, shared.consumed)];
            let acquire_timeouts = [WRITER_ACQUIRE_TIMEOUT_MS];
            let release_keys = [KEY_READER];
            let mut keyed_mutex_info = vk::Win32KeyedMutexAcquireReleaseInfoKHR::default()
                .acquire_syncs(&memories)
                .acquire_keys(&acquire_keys)
                .acquire_timeouts(&acquire_timeouts)
                .release_syncs(&memories)
                .release_keys(&release_keys);
            let command_buffers = [raster.command_buffer];
            let submit = vk::SubmitInfo::default()
                .command_buffers(&command_buffers)
                .push_next(&mut keyed_mutex_info);

            {
                let _queue = lock_queue();
                device.queue_submit(self.context.queue(), &[submit], raster.copy_fence)
            }
            .map_err(|e| self.note_failure(&submit_description, e))?;
        }

        raster.copy_in_flight = true;
        shared.presented += 1;
        Ok(())
    }

    /// Readback hand-off: copy into the back buffer, wait for the GPU, then
    /// publish it as the front buffer. Synchronous on the raster thread; the
    /// frame lock is only held for the swap, never across the GPU wait.
    fn present_to_readback(
        &self,
        raster: &RasterState,
        render: vk::Image,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let device = self.context.device();
        let (back_index, back_buffer) = match &self.shared.lock().target {
            Some(FrameTarget::Readback(target))
                if target.width == width && target.height == height =>
            {
                (target.back_index(), target.buffer(target.back_index()))
            }
            _ => return Err("readback target does not match the render image".to_string()),
        };

        unsafe {
            record_frame_readback(
                &self.context,
                raster.command_buffer,
                render,
                back_buffer,
                width,
                height,
            )
            .map_err(|e| self.note_failure("recording the frame readback", e))?;

            let command_buffers = [raster.command_buffer];
            let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
            {
                let _queue = lock_queue();
                device.queue_submit(self.context.queue(), &[submit], raster.copy_fence)
            }
            .map_err(|e| self.note_failure("vkQueueSubmit (frame readback)", e))?;

            device
                .wait_for_fences(&[raster.copy_fence], true, COPY_FENCE_TIMEOUT_NS)
                .map_err(|e| self.note_failure("vkWaitForFences (frame readback)", e))?;
        }

        let mut shared = self.shared.lock();
        if let Some(FrameTarget::Readback(target)) = &mut shared.target {
            target
                .sync_to_host(&self.context, back_index)
                .map_err(|e| self.note_failure("vkInvalidateMappedMemoryRanges", e))?;
            target.set_front(back_index);
        }
        shared.presented += 1;
        Ok(())
    }

    /// Host side: copies the newest presented frame into `destination`
    /// (`destination_size` pixels) on the host's immediate context. Returns
    /// `false` when there is no new frame or its GPU copy has not finished yet.
    pub fn copy_latest_frame(
        &self,
        context: &ID3D11DeviceContext,
        destination: &ID3D11Texture2D,
        destination_size: (u32, u32),
    ) -> bool {
        let mut shared = self.shared.lock();
        if shared.presented == shared.consumed {
            return false;
        }
        let Some(target) = &shared.target else {
            return false;
        };
        let copy_box = host_copy_extent(target.size(), destination_size).map(|(width, height)| {
            D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            }
        });

        match target {
            FrameTarget::Shared(target) => {
                if !target.acquire_for_host(HOST_ACQUIRE_TIMEOUT_MS) {
                    return false;
                }
                unsafe {
                    if (target.width, target.height) == destination_size {
                        context.CopyResource(destination, &target.d3d_texture);
                    } else if let Some(source_box) = &copy_box {
                        context.CopySubresourceRegion(
                            destination,
                            0,
                            0,
                            0,
                            0,
                            &target.d3d_texture,
                            0,
                            Some(source_box),
                        );
                    }
                }
                target.release_from_host();
            }
            FrameTarget::Readback(target) => {
                if let Some(destination_box) = &copy_box {
                    unsafe {
                        context.UpdateSubresource(
                            destination,
                            0,
                            Some(destination_box),
                            target.front_pixels(),
                            target.row_pitch(),
                            0,
                        );
                    }
                }
            }
        }

        shared.consumed = shared.presented;
        true
    }

    fn note_failure(&self, what: &str, result: vk::Result) -> String {
        if result == vk::Result::ERROR_DEVICE_LOST {
            self.device_lost.store(true, Ordering::Release);
        }
        format!("{what} failed: {result}")
    }
}

impl Drop for VulkanInteropState {
    fn drop(&mut self) {
        let _ = self.context.wait_idle();

        let raster = self.raster.get_mut();
        if let Some(render_image) = raster.render_image.take() {
            destroy_render_image(&self.context, render_image);
        }
        unsafe {
            let device = self.context.device();
            device.destroy_fence(raster.copy_fence, None);
            device.destroy_command_pool(raster.command_pool, None);
        }

        if let Some(target) = self.shared.get_mut().target.take() {
            target.destroy(&self.context);
        }
    }
}

fn create_copy_objects(
    context: &VulkanContext,
) -> Result<(vk::CommandPool, vk::CommandBuffer, vk::Fence), String> {
    let device = context.device();
    unsafe {
        let pool_info = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(context.queue_family_index());
        let command_pool = device
            .create_command_pool(&pool_info, None)
            .map_err(|e| format!("vkCreateCommandPool failed: {e}"))?;

        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = match device.allocate_command_buffers(&allocate_info) {
            Ok(buffers) => buffers[0],
            Err(e) => {
                device.destroy_command_pool(command_pool, None);
                return Err(format!("vkAllocateCommandBuffers failed: {e}"));
            }
        };

        let copy_fence = match device.create_fence(&vk::FenceCreateInfo::default(), None) {
            Ok(fence) => fence,
            Err(e) => {
                device.destroy_command_pool(command_pool, None);
                return Err(format!("vkCreateFence failed: {e}"));
            }
        };

        Ok((command_pool, command_buffer, copy_fence))
    }
}

fn create_render_image(
    context: &VulkanContext,
    width: u32,
    height: u32,
) -> Result<RenderImage, String> {
    let device = context.device();
    unsafe {
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
            .usage(RENDER_IMAGE_USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = device
            .create_image(&image_info, None)
            .map_err(|e| format!("vkCreateImage (render, {width}x{height}) failed: {e}"))?;

        let requirements = device.get_image_memory_requirements(image);
        let Some(memory_type_index) = context.find_memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) else {
            device.destroy_image(image, None);
            return Err("no Vulkan memory type for the render image".to_string());
        };

        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index);
        let memory = match device.allocate_memory(&allocate_info, None) {
            Ok(memory) => memory,
            Err(e) => {
                device.destroy_image(image, None);
                return Err(format!("vkAllocateMemory (render image) failed: {e}"));
            }
        };

        if let Err(e) = device.bind_image_memory(image, memory, 0) {
            device.destroy_image(image, None);
            device.free_memory(memory, None);
            return Err(format!("vkBindImageMemory (render image) failed: {e}"));
        }

        Ok(RenderImage {
            image,
            memory,
            width,
            height,
        })
    }
}

fn destroy_render_image(context: &VulkanContext, render_image: RenderImage) {
    unsafe {
        context.device().destroy_image(render_image.image, None);
        context.device().free_memory(render_image.memory, None);
    }
}

/// Records: render image → `TRANSFER_SRC`, shared image taken over from the
/// external owner → `TRANSFER_DST`, full-frame copy, shared image handed back to
/// the external owner, render image → `COLOR_ATTACHMENT_OPTIMAL` (the layout both
/// engine backends leave it in at present, so the invariant holds every frame).
unsafe fn record_frame_copy(
    context: &VulkanContext,
    command_buffer: vk::CommandBuffer,
    render_image: vk::Image,
    shared_image: vk::Image,
    width: u32,
    height: u32,
) -> Result<(), vk::Result> {
    let device = context.device();
    let family = context.queue_family_index();

    unsafe {
        device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())?;
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device.begin_command_buffer(command_buffer, &begin_info)?;

        let before_copy = [
            vk::ImageMemoryBarrier::default()
                .image(render_image)
                .subresource_range(COLOR_SUBRESOURCE_RANGE)
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED),
            vk::ImageMemoryBarrier::default()
                .image(shared_image)
                .subresource_range(COLOR_SUBRESOURCE_RANGE)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                .dst_queue_family_index(family),
        ];
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &before_copy,
        );

        let region = vk::ImageCopy::default()
            .src_subresource(COLOR_SUBRESOURCE_LAYERS)
            .dst_subresource(COLOR_SUBRESOURCE_LAYERS)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        device.cmd_copy_image(
            command_buffer,
            render_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            shared_image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[region],
        );

        let after_copy = [
            vk::ImageMemoryBarrier::default()
                .image(render_image)
                .subresource_range(COLOR_SUBRESOURCE_RANGE)
                .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_READ)
                .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED),
            vk::ImageMemoryBarrier::default()
                .image(shared_image)
                .subresource_range(COLOR_SUBRESOURCE_RANGE)
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .src_queue_family_index(family)
                .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL),
        ];
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &after_copy,
        );

        device.end_command_buffer(command_buffer)
    }
}

/// Records: render image → `TRANSFER_SRC`, full-frame copy into `buffer`
/// (tightly packed BGRA8 rows), render image → `COLOR_ATTACHMENT_OPTIMAL`.
unsafe fn record_frame_readback(
    context: &VulkanContext,
    command_buffer: vk::CommandBuffer,
    render_image: vk::Image,
    buffer: vk::Buffer,
    width: u32,
    height: u32,
) -> Result<(), vk::Result> {
    let device = context.device();

    unsafe {
        device.reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())?;
        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device.begin_command_buffer(command_buffer, &begin_info)?;

        let to_transfer_source = vk::ImageMemoryBarrier::default()
            .image(render_image)
            .subresource_range(COLOR_SUBRESOURCE_RANGE)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer_source],
        );

        let region = vk::BufferImageCopy::default()
            .buffer_offset(0)
            .buffer_row_length(0)
            .buffer_image_height(0)
            .image_subresource(COLOR_SUBRESOURCE_LAYERS)
            .image_extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            });
        device.cmd_copy_image_to_buffer(
            command_buffer,
            render_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &[region],
        );

        let to_color_attachment = vk::ImageMemoryBarrier::default()
            .image(render_image)
            .subresource_range(COLOR_SUBRESOURCE_RANGE)
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_color_attachment],
        );

        device.end_command_buffer(command_buffer)
    }
}

fn dxgi_adapter(device: &ID3D11Device) -> Option<IDXGIAdapter3> {
    let dxgi_device: IDXGIDevice = device.cast().ok()?;
    unsafe { dxgi_device.GetAdapter() }.ok()?.cast().ok()
}

/// System commit still available to every process, in bytes.
fn commit_available() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    Some(status.ullAvailPageFile)
}

fn vulkan_state(user_data: *mut c_void) -> Option<&'static VulkanInteropState> {
    if user_data.is_null() {
        return None;
    }
    let overlay = unsafe { &*(user_data as *const FlutterOverlay) };
    overlay.vulkan_state.as_deref()
}

const NO_IMAGE: FlutterVulkanImage = FlutterVulkanImage {
    struct_size: size_of::<FlutterVulkanImage>(),
    image: 0,
    format: 0,
};

/// `FlutterVulkanRendererConfig.get_next_image_callback`.
extern "C" fn get_next_image_callback(
    user_data: *mut c_void,
    frame_info: *const FlutterFrameInfo,
) -> FlutterVulkanImage {
    let Some(state) = vulkan_state(user_data) else {
        return NO_IMAGE;
    };
    if frame_info.is_null() || state.is_device_lost() || state.memory_exhausted() {
        return NO_IMAGE;
    }
    let size = unsafe { (*frame_info).size };

    match state.next_image(size.width, size.height) {
        Ok(image) => FlutterVulkanImage {
            struct_size: size_of::<FlutterVulkanImage>(),
            image: image.as_raw(),
            format: FRAME_FORMAT.as_raw() as u32,
        },
        Err(e) => {
            error!("[VulkanInterop] get_next_image failed: {e}");
            NO_IMAGE
        }
    }
}

/// `FlutterVulkanRendererConfig.present_image_callback`.
extern "C" fn present_image_callback(
    user_data: *mut c_void,
    image: *const FlutterVulkanImage,
) -> bool {
    let Some(state) = vulkan_state(user_data) else {
        return false;
    };
    if image.is_null() {
        return false;
    }
    let image = vk::Image::from_raw(unsafe { (*image).image });

    match state.present(image) {
        Ok(()) => true,
        Err(e) => {
            error!("[VulkanInterop] present_image failed: {e}");
            false
        }
    }
}

/// Builds the engine renderer config that lends `state`'s device to the engine.
/// `state` must outlive the engine.
pub fn build_vulkan_renderer_config(state: &VulkanInteropState) -> FlutterRendererConfig {
    let context = &state.context;
    let instance_extensions = context.instance_extension_ptrs();
    let device_extensions = context.device_extension_ptrs();

    FlutterRendererConfig {
        type_: FlutterRendererType_kVulkan,
        __bindgen_anon_1: FlutterRendererConfig__bindgen_ty_1 {
            vulkan: FlutterVulkanRendererConfig {
                struct_size: size_of::<FlutterVulkanRendererConfig>(),
                version: VULKAN_API_VERSION,
                instance: context.instance().handle().as_raw() as *mut c_void,
                physical_device: context.physical_device().as_raw() as *mut c_void,
                device: context.device().handle().as_raw() as *mut c_void,
                queue_family_index: context.queue_family_index(),
                queue: context.queue().as_raw() as *mut c_void,
                enabled_instance_extension_count: instance_extensions.len(),
                enabled_instance_extensions: instance_extensions.as_ptr().cast_mut(),
                enabled_device_extension_count: device_extensions.len(),
                enabled_device_extensions: device_extensions.as_ptr().cast_mut(),
                get_instance_proc_address_callback: Some(get_instance_proc_address_callback),
                get_next_image_callback: Some(get_next_image_callback),
                present_image_callback: Some(present_image_callback),
                external_texture_frame_callback: None,
            },
        },
    }
}
