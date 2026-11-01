//! Helpers shared by the GPU and engine tests of the Vulkan path.

#[cfg(feature = "engine-tests")]
use std::sync::OnceLock;

#[cfg(feature = "engine-tests")]
use windows::Win32::Foundation::HMODULE;
#[cfg(feature = "engine-tests")]
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
#[cfg(feature = "engine-tests")]
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

#[cfg(feature = "engine-tests")]
use crate::software_renderer::vk_renderer::interop::VulkanInteropState;

/// Reads back the BGRA bytes of the pixel at (`x`, `y`) of a BGRA8 host texture.
pub fn read_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    x: u32,
    y: u32,
) -> [u8; 4] {
    unsafe {
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut source_desc);
        assert!(
            x < source_desc.Width && y < source_desc.Height,
            "pixel ({x}, {y}) outside {}x{} texture",
            source_desc.Width,
            source_desc.Height
        );

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: source_desc.Width,
            Height: source_desc.Height,
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
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .expect("staging texture");
        let staging = staging.expect("staging texture");
        context.CopyResource(&staging, texture);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .expect("map staging texture");
        let offset = (y * mapped.RowPitch + x * 4) as usize;
        let pixel = std::slice::from_raw_parts((mapped.pData as *const u8).add(offset), 4);
        let bytes = [pixel[0], pixel[1], pixel[2], pixel[3]];
        context.Unmap(&staging, 0);
        bytes
    }
}

/// `Color(0xFF2244AA)`, the background the Flutter test app paints, as BGRA bytes.
#[cfg(feature = "engine-tests")]
pub const APP_BACKGROUND_BGRA: [u8; 4] = [0xAA, 0x44, 0x22, 0xFF];

/// True when `actual` matches `expected` BGRA within one step per channel.
pub fn bgra_matches(actual: [u8; 4], expected: [u8; 4]) -> bool {
    actual
        .iter()
        .zip(expected.iter())
        .all(|(a, e)| a.abs_diff(*e) <= 1)
}

/// Whether this machine can run the Vulkan path at all, probed without an
/// engine so engine tests can tell "cannot run here" (skip) from "broken" (fail).
#[cfg(feature = "engine-tests")]
pub fn vulkan_path_is_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let mut device: Option<ID3D11Device> = None;
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
                None,
            )
        };
        let Some(device) = created.ok().and(device) else {
            eprintln!("SKIP: no hardware D3D11 device");
            return false;
        };
        match VulkanInteropState::new(&device, 64, 64) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("SKIP: Vulkan path unavailable: {e}");
                false
            }
        }
    })
}

/// Asserts `actual` matches `expected` BGRA within one step per channel.
pub fn assert_bgra(actual: [u8; 4], expected: [u8; 4], what: &str) {
    assert!(
        bgra_matches(actual, expected),
        "{what}: BGRA {actual:?} != expected {expected:?}"
    );
}
