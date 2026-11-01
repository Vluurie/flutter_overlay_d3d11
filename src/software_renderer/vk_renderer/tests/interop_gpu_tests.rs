//! Real-GPU tests of the Vulkan → D3D11 hand-off, without a Flutter engine.
//!
//! The test plays the engine's part: it asks for the render image, fills it with
//! a known colour, leaves it in `COLOR_ATTACHMENT_OPTIMAL` and calls `present`,
//! exactly the contract of `get_next_image` / `present_image`. It then plays the
//! host's part and reads the pixels back through D3D11. This isolates the interop
//! (import, keyed mutex, resize) from engine behaviour; the engine-driven
//! counterpart lives in `engine_tests`.
//!
//! Every test skips (passes with a note) when the machine has no hardware D3D11
//! device, no Vulkan driver for that adapter, or no D3D11 interop extensions.

use ash::vk;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};

use crate::software_renderer::overlay::d3d::create_compositing_texture;
use crate::software_renderer::vk_renderer::context::VulkanContext;
use crate::software_renderer::vk_renderer::interop::VulkanInteropState;
use crate::software_renderer::vk_renderer::queue_guard::{lock_queue, resolve_for_tests};
use crate::software_renderer::vk_renderer::tests::support::{assert_bgra, read_pixel};

/// A colour as the engine would clear to (RGBA float) and the BGRA bytes the
/// host must read back for it.
struct TestColor {
    rgba: [f32; 4],
    bgra: [u8; 4],
}

const ORANGE: TestColor = TestColor {
    rgba: [1.0, 0.5, 0.0, 1.0],
    bgra: [0, 128, 255, 255],
};
const TEAL: TestColor = TestColor {
    rgba: [0.0, 0.5, 1.0, 1.0],
    bgra: [255, 128, 0, 255],
};

const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

struct Fixture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    state: Box<VulkanInteropState>,
}

/// Runs `test` once per hand-off mode: the one this machine picks on its own
/// (shared texture on a native Windows stack) and readback, forced.
fn for_each_handoff(width: u32, height: u32, test: impl Fn(&Fixture)) {
    for force_readback in [false, true] {
        let Some(fixture) = Fixture::with_handoff(width, height, force_readback) else {
            return;
        };
        eprintln!("STEP: hand-off {:?}", fixture.state.handoff());
        test(&fixture);
    }
}

impl Fixture {
    /// Host device + Vulkan state, or `None` when this machine cannot run the path.
    fn new(width: u32, height: u32) -> Option<Self> {
        Self::with_handoff(width, height, false)
    }

    fn with_handoff(width: u32, height: u32, force_readback: bool) -> Option<Self> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let created = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        let (Ok(()), Some(device), Some(context)) = (created, device, context) else {
            eprintln!("SKIP: no hardware D3D11 device");
            return None;
        };

        match VulkanInteropState::with_handoff(&device, width, height, force_readback) {
            Ok(state) => Some(Self {
                device,
                context,
                state,
            }),
            Err(e) => {
                eprintln!("SKIP: Vulkan renderer unavailable: {e}");
                None
            }
        }
    }

    fn vulkan(&self) -> &VulkanContext {
        self.state.context()
    }

    /// Plays the engine for one frame: render `color`, then present.
    fn render_and_present(&self, image: vk::Image, color: &TestColor) {
        render_solid_frame(self.vulkan(), image, color);
        self.state.present(image).expect("present");
    }

    /// Plays the host: retries until the presented frame's GPU copy has
    /// finished, as consecutive `tick()` calls would.
    fn host_copy(&self, destination: &ID3D11Texture2D, destination_size: (u32, u32)) -> bool {
        (0..200).any(|_| {
            self.state
                .copy_latest_frame(&self.context, destination, destination_size)
        })
    }

    fn pixel(&self, texture: &ID3D11Texture2D, x: u32, y: u32) -> [u8; 4] {
        read_pixel(&self.device, &self.context, texture, x, y)
    }
}

/// Fills `image` with `color` and leaves it in `COLOR_ATTACHMENT_OPTIMAL`.
fn render_solid_frame(context: &VulkanContext, image: vk::Image, color: &TestColor) {
    let device = context.device();

    unsafe {
        let pool_info =
            vk::CommandPoolCreateInfo::default().queue_family_index(context.queue_family_index());
        let pool = device.create_command_pool(&pool_info, None).unwrap();
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command_buffer = device.allocate_command_buffers(&allocate_info).unwrap()[0];

        let begin_info = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        device
            .begin_command_buffer(command_buffer, &begin_info)
            .unwrap();

        let to_transfer = vk::ImageMemoryBarrier::default()
            .image(image)
            .subresource_range(COLOR_RANGE)
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
        device.cmd_pipeline_barrier(
            command_buffer,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[to_transfer],
        );

        let clear = vk::ClearColorValue {
            float32: color.rgba,
        };
        device.cmd_clear_color_image(
            command_buffer,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &clear,
            &[COLOR_RANGE],
        );

        let to_attachment = vk::ImageMemoryBarrier::default()
            .image(image)
            .subresource_range(COLOR_RANGE)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
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
            &[to_attachment],
        );

        device.end_command_buffer(command_buffer).unwrap();

        let command_buffers = [command_buffer];
        let submit = vk::SubmitInfo::default().command_buffers(&command_buffers);
        {
            let _queue = lock_queue();
            device
                .queue_submit(context.queue(), &[submit], vk::Fence::null())
                .unwrap();
        }
        context.wait_idle().unwrap();
        device.destroy_command_pool(pool, None);
    }
}

#[test]
fn queue_entry_points_are_swapped_for_locking_wrappers() {
    let Some(fixture) = Fixture::new(64, 64) else {
        return;
    };
    let instance = fixture.vulkan().instance().handle();
    let resolve = |name| resolve_for_tests(instance, name).map(|function| function as usize);
    let driver_submit = fixture.vulkan().device().fp_v1_0().queue_submit as usize;

    let submit = resolve(c"vkQueueSubmit");
    assert!(submit.is_some());
    assert_eq!(submit, resolve(c"vkQueueSubmit"));
    assert_ne!(submit, Some(driver_submit));
    assert_ne!(submit, resolve(c"vkQueueWaitIdle"));
    assert!(resolve(c"vkQueueWaitIdle").is_some());
    assert!(resolve(c"vkDeviceWaitIdle").is_some());
    assert!(resolve(c"vkGetDeviceProcAddr").is_some());
    assert!(resolve(c"vkCreateImage").is_some());
    assert!(resolve(c"vkNotARealFunction").is_none());
}

#[test]
fn presented_frame_reaches_the_host_texture() {
    let size = (128, 96);
    for_each_handoff(size.0, size.1, |fixture| {
        let destination = create_compositing_texture(&fixture.device, size.0, size.1);

        assert!(
            !fixture
                .state
                .copy_latest_frame(&fixture.context, &destination, size)
        );
        assert_eq!(fixture.state.frames_presented(), 0);

        let image = fixture.state.next_image(size.0, size.1).unwrap();
        fixture.render_and_present(image, &ORANGE);

        assert_eq!(fixture.state.frames_presented(), 1);
        assert!(fixture.host_copy(&destination, size));
        assert_bgra(fixture.pixel(&destination, 0, 0), ORANGE.bgra, "top-left");
        assert_bgra(
            fixture.pixel(&destination, size.0 - 1, size.1 - 1),
            ORANGE.bgra,
            "bottom-right",
        );
        assert!(
            !fixture
                .state
                .copy_latest_frame(&fixture.context, &destination, size),
            "a consumed frame must not be delivered twice"
        );
    });
}

#[test]
fn unconsumed_frames_are_overwritten_without_blocking_the_writer() {
    let size = (64, 64);
    for_each_handoff(size.0, size.1, |fixture| {
        let destination = create_compositing_texture(&fixture.device, size.0, size.1);
        let image = fixture.state.next_image(size.0, size.1).unwrap();

        for color in [&ORANGE, &TEAL, &ORANGE, &TEAL] {
            fixture.render_and_present(image, color);
        }

        assert_eq!(fixture.state.frames_presented(), 4);
        assert!(fixture.host_copy(&destination, size));
        assert_bgra(
            fixture.pixel(&destination, 10, 10),
            TEAL.bgra,
            "newest frame wins",
        );
    });
}

#[test]
fn writer_and_host_alternate_across_many_frames() {
    let size = (64, 64);
    for_each_handoff(size.0, size.1, |fixture| {
        let destination = create_compositing_texture(&fixture.device, size.0, size.1);
        let image = fixture.state.next_image(size.0, size.1).unwrap();

        for frame in 0..24 {
            let color = if frame % 2 == 0 { &ORANGE } else { &TEAL };
            fixture.render_and_present(image, color);
            assert!(fixture.host_copy(&destination, size), "frame {frame}");
            assert_bgra(
                fixture.pixel(&destination, 32, 32),
                color.bgra,
                &format!("frame {frame}"),
            );
        }
        assert!(!fixture.state.is_device_lost());
    });
}

#[test]
fn resize_reallocates_both_images_and_keeps_delivering() {
    let small = (64, 48);
    let large = (200, 150);
    for_each_handoff(small.0, small.1, |fixture| {
        let first = fixture.state.next_image(small.0, small.1).unwrap();
        assert_eq!(fixture.state.next_image(small.0, small.1).unwrap(), first);
        fixture.render_and_present(first, &ORANGE);

        let resized = fixture.state.next_image(large.0, large.1).unwrap();
        fixture.render_and_present(resized, &TEAL);

        let destination = create_compositing_texture(&fixture.device, large.0, large.1);
        assert!(fixture.host_copy(&destination, large));
        assert_bgra(
            fixture.pixel(&destination, large.0 - 1, large.1 - 1),
            TEAL.bgra,
            "bottom-right after resize",
        );
    });
}

#[test]
fn presenting_an_image_that_was_not_handed_out_is_rejected() {
    let Some(fixture) = Fixture::new(32, 32) else {
        return;
    };
    assert!(fixture.state.present(vk::Image::null()).is_err());
    assert_eq!(fixture.state.frames_presented(), 0);
}

#[test]
fn host_texture_of_a_stale_size_gets_a_clamped_copy() {
    let frame = (120, 90);
    let stale = (80, 60);
    for_each_handoff(frame.0, frame.1, |fixture| {
        let destination = create_compositing_texture(&fixture.device, stale.0, stale.1);

        let image = fixture.state.next_image(frame.0, frame.1).unwrap();
        fixture.render_and_present(image, &ORANGE);

        assert!(fixture.host_copy(&destination, stale));
        assert_bgra(
            fixture.pixel(&destination, stale.0 - 1, stale.1 - 1),
            ORANGE.bgra,
            "clamped copy",
        );
    });
}

#[test]
fn empty_frame_size_is_rejected() {
    let Some(fixture) = Fixture::new(32, 32) else {
        return;
    };
    assert!(fixture.state.next_image(0, 32).is_err());
    assert!(fixture.state.next_image(32, 0).is_err());
}
