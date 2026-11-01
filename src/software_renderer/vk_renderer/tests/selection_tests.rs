//! Pure decision logic of the Vulkan path: no GPU required.

use ash::vk;

use crate::software_renderer::api::{RendererPreference, RendererType};
use crate::software_renderer::vk_renderer::context::{luid_to_bytes, select_memory_type};
use crate::software_renderer::vk_renderer::interop::{host_copy_extent, writer_acquire_key};
use crate::software_renderer::vk_renderer::shared_image::{KEY_READER, KEY_WRITER};

fn memory_properties(flags: &[vk::MemoryPropertyFlags]) -> vk::PhysicalDeviceMemoryProperties {
    let mut properties = vk::PhysicalDeviceMemoryProperties {
        memory_type_count: flags.len() as u32,
        ..Default::default()
    };
    for (index, property_flags) in flags.iter().enumerate() {
        properties.memory_types[index].property_flags = *property_flags;
    }
    properties
}

#[test]
fn writer_takes_writer_key_when_host_is_caught_up() {
    assert_eq!(writer_acquire_key(0, 0), KEY_WRITER);
    assert_eq!(writer_acquire_key(7, 7), KEY_WRITER);
}

#[test]
fn writer_retakes_reader_key_when_host_has_not_consumed() {
    assert_eq!(writer_acquire_key(1, 0), KEY_READER);
    assert_eq!(writer_acquire_key(9, 3), KEY_READER);
}

#[test]
fn host_copy_is_clamped_to_the_smaller_texture() {
    assert_eq!(host_copy_extent((800, 600), (800, 600)), Some((800, 600)));
    assert_eq!(host_copy_extent((1920, 1080), (800, 600)), Some((800, 600)));
    assert_eq!(host_copy_extent((640, 1080), (800, 600)), Some((640, 600)));
}

#[test]
fn host_copy_is_skipped_for_empty_extent() {
    assert_eq!(host_copy_extent((0, 600), (800, 600)), None);
    assert_eq!(host_copy_extent((800, 600), (800, 0)), None);
}

#[test]
fn luid_is_serialised_low_part_first_little_endian() {
    assert_eq!(
        luid_to_bytes(0x0403_0201, 0x0807_0605),
        [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
    );
    assert_eq!(
        luid_to_bytes(0xFFFF_FFFF, -1),
        [0xFF; vk::LUID_SIZE]
    );
}

#[test]
fn memory_type_prefers_device_local_among_allowed() {
    let properties = memory_properties(&[
        vk::MemoryPropertyFlags::HOST_VISIBLE,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    ]);
    let none = vk::MemoryPropertyFlags::empty();
    assert_eq!(
        select_memory_type(&properties, 0b111, none, vk::MemoryPropertyFlags::DEVICE_LOCAL),
        Some(1)
    );
    assert_eq!(
        select_memory_type(&properties, 0b100, none, vk::MemoryPropertyFlags::DEVICE_LOCAL),
        Some(2)
    );
}

#[test]
fn memory_type_never_violates_a_requirement() {
    let host = vk::MemoryPropertyFlags::HOST_VISIBLE;
    let cached = vk::MemoryPropertyFlags::HOST_CACHED;
    let properties = memory_properties(&[
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
        host,
        host | cached,
    ]);
    assert_eq!(select_memory_type(&properties, 0b111, host, cached), Some(2));
    assert_eq!(select_memory_type(&properties, 0b011, host, cached), Some(1));
    assert_eq!(select_memory_type(&properties, 0b001, host, cached), None);
}

#[test]
fn memory_type_falls_back_to_any_allowed_type() {
    let properties = memory_properties(&[
        vk::MemoryPropertyFlags::HOST_VISIBLE,
        vk::MemoryPropertyFlags::DEVICE_LOCAL,
    ]);
    assert_eq!(
        select_memory_type(
            &properties,
            0b01,
            vk::MemoryPropertyFlags::empty(),
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        ),
        Some(0)
    );
}

#[test]
fn memory_type_is_none_when_nothing_is_allowed() {
    let properties = memory_properties(&[vk::MemoryPropertyFlags::DEVICE_LOCAL]);
    let none = vk::MemoryPropertyFlags::empty();
    assert_eq!(
        select_memory_type(&properties, 0, none, vk::MemoryPropertyFlags::DEVICE_LOCAL),
        None
    );
    assert_eq!(
        select_memory_type(&properties, 0b10, none, vk::MemoryPropertyFlags::DEVICE_LOCAL),
        None
    );
}

#[test]
fn auto_and_vulkan_try_vulkan_then_opengl_then_software() {
    let expected = [
        RendererType::Vulkan,
        RendererType::OpenGL,
        RendererType::Software,
    ];
    assert_eq!(RendererPreference::Auto.attempt_order(), expected);
    assert_eq!(RendererPreference::Vulkan.attempt_order(), expected);
    assert_eq!(RendererPreference::default(), RendererPreference::Auto);
}

#[test]
fn opengl_preference_never_attempts_vulkan() {
    assert_eq!(
        RendererPreference::OpenGL.attempt_order(),
        [RendererType::OpenGL, RendererType::Software]
    );
}

#[test]
fn every_preference_ends_on_the_software_floor() {
    for preference in [
        RendererPreference::Auto,
        RendererPreference::Vulkan,
        RendererPreference::OpenGL,
        RendererPreference::Software,
    ] {
        assert_eq!(
            preference.attempt_order().last(),
            Some(&RendererType::Software)
        );
    }
}
