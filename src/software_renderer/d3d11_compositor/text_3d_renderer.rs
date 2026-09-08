use directx_math::{XMMatrix, XMMatrixTranspose};
use std::{collections::HashMap, mem};
use windows::core::{BOOL, PCSTR};
use windows::Win32::{
    Graphics::{
        Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        Direct3D11::*,
        Dxgi::Common::{
            DXGI_FORMAT_R32G32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT, DXGI_FORMAT_R32G32B32A32_FLOAT,
        },
    },
};

use super::primitive_3d_renderer::PrimitiveOptions;
use crate::software_renderer::d3d11_compositor::traits::{FrameParams, Renderer};

const MAX_TEXT_VERTEX_BUFFER_CAPACITY: usize = 65536;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TexturedVertex3D {
    pub position: [f32; 3],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

#[derive(Clone, Copy, Debug, Default)]
pub struct GlyphInfo {
    pub uv_rect: [f32; 4],
    pub bearing_x: f32,
    pub bearing_y: f32,
    pub width: f32,
    pub height: f32,
    pub advance: f32,
}

#[derive(Clone)]
pub struct FontAtlas {
    pub texture: ID3D11ShaderResourceView,
    pub sampler: ID3D11SamplerState,
    pub glyphs: HashMap<char, GlyphInfo>,
    pub line_height: f32,
    pub base_font_size: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct TextOptions {
    pub scale: f32,
    pub color: [f32; 4],
    pub align_h: f32,
    pub align_v: f32,
    pub billboard: bool,
    pub primitive_options: PrimitiveOptions,
}

impl Default for TextOptions {
    fn default() -> Self {
        Self {
            scale: 1.0,
            color: [1.0, 1.0, 1.0, 1.0],
            align_h: -1.0,
            align_v: 0.0,
            billboard: false,
            primitive_options: PrimitiveOptions::default(),
        }
    }
}

#[repr(C)]
struct SceneConstants {
    view_projection: XMMatrix,
}

#[derive(Clone)]
pub struct Text3DRenderer {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    constant_buffer: ID3D11Buffer,
    vertex_buffer: ID3D11Buffer,

    font_atlases: HashMap<String, FontAtlas>,

    submit_groups: HashMap<String, HashMap<String, (Vec<TexturedVertex3D>, PrimitiveOptions)>>,

    render_buffers: HashMap<String, HashMap<PrimitiveOptions, Vec<TexturedVertex3D>>>,

    blend_state_transparent: ID3D11BlendState,
    blend_state_opaque: ID3D11BlendState,
    depth_stencil_state_transparent: ID3D11DepthStencilState,
    depth_stencil_state_disabled: ID3D11DepthStencilState,
    rasterizer_state: ID3D11RasterizerState,

    device: ID3D11Device,
}

impl Text3DRenderer {
    pub fn new(device: &ID3D11Device) -> Self {
        let vs_bytes = include_bytes!("./shaders/text_vs.cso");
        let ps_bytes = include_bytes!("./shaders/text_ps.cso");

        let mut vertex_shader: Option<ID3D11VertexShader> = None;
        unsafe {
            device
                .CreateVertexShader(vs_bytes, None, Some(&mut vertex_shader))
                .expect("Failed to create text VS");
        }

        let mut pixel_shader: Option<ID3D11PixelShader> = None;
        unsafe {
            device
                .CreatePixelShader(ps_bytes, None, Some(&mut pixel_shader))
                .expect("Failed to create text PS");
        }

        let input_element_descs = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"POSITION".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"TEXCOORD".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 12,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(c"COLOR".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 20,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];

        let mut input_layout: Option<ID3D11InputLayout> = None;
        unsafe {
            device
                .CreateInputLayout(&input_element_descs, vs_bytes, Some(&mut input_layout))
                .expect("Failed to create text input layout");
        }

        let vertex_buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: (mem::size_of::<TexturedVertex3D>() * MAX_TEXT_VERTEX_BUFFER_CAPACITY)
                as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };

        let mut vertex_buffer: Option<ID3D11Buffer> = None;
        unsafe {
            device
                .CreateBuffer(&vertex_buffer_desc, None, Some(&mut vertex_buffer))
                .expect("Failed to create text vertex buffer");
        }

        let constant_buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: mem::size_of::<SceneConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };

        let mut constant_buffer: Option<ID3D11Buffer> = None;
        unsafe {
            device
                .CreateBuffer(&constant_buffer_desc, None, Some(&mut constant_buffer))
                .expect("Failed to create text constant buffer");
        }

        let mut blend_desc_transparent = D3D11_BLEND_DESC::default();
        let rt_blend_desc = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: BOOL(1),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ONE,
            DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        blend_desc_transparent.RenderTarget[0] = rt_blend_desc;

        let mut blend_state_transparent: Option<ID3D11BlendState> = None;
        unsafe {
            device
                .CreateBlendState(&blend_desc_transparent, Some(&mut blend_state_transparent))
                .expect("Failed to create text transparent blend state");
        }

        let blend_desc_opaque = D3D11_BLEND_DESC::default();
        let mut blend_state_opaque: Option<ID3D11BlendState> = None;
        unsafe {
            device
                .CreateBlendState(&blend_desc_opaque, Some(&mut blend_state_opaque))
                .expect("Failed to create text opaque blend state");
        }

        let depth_desc_transparent = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: BOOL(1),
            DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
            DepthFunc: D3D11_COMPARISON_LESS,
            ..Default::default()
        };

        let mut depth_stencil_state_transparent: Option<ID3D11DepthStencilState> = None;
        unsafe {
            device
                .CreateDepthStencilState(
                    &depth_desc_transparent,
                    Some(&mut depth_stencil_state_transparent),
                )
                .expect("Failed to create text transparent depth stencil state");
        }

        let depth_desc_disabled = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: BOOL(0),
            ..Default::default()
        };

        let mut depth_stencil_state_disabled: Option<ID3D11DepthStencilState> = None;
        unsafe {
            device
                .CreateDepthStencilState(
                    &depth_desc_disabled,
                    Some(&mut depth_stencil_state_disabled),
                )
                .expect("Failed to create text disabled depth stencil state");
        }

        let rast_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: BOOL(1),
            ..Default::default()
        };

        let mut rasterizer_state: Option<ID3D11RasterizerState> = None;
        unsafe {
            device
                .CreateRasterizerState(&rast_desc, Some(&mut rasterizer_state))
                .expect("Failed to create text rasterizer state");
        }

        Self {
            vertex_shader: vertex_shader.unwrap(),
            pixel_shader: pixel_shader.unwrap(),
            input_layout: input_layout.unwrap(),
            constant_buffer: constant_buffer.unwrap(),
            vertex_buffer: vertex_buffer.unwrap(),
            font_atlases: HashMap::new(),
            submit_groups: HashMap::new(),
            render_buffers: HashMap::new(),
            blend_state_transparent: blend_state_transparent.unwrap(),
            blend_state_opaque: blend_state_opaque.unwrap(),
            depth_stencil_state_transparent: depth_stencil_state_transparent.unwrap(),
            depth_stencil_state_disabled: depth_stencil_state_disabled.unwrap(),
            rasterizer_state: rasterizer_state.unwrap(),
            device: device.clone(),
        }
    }

    pub fn register_font_atlas(
        &mut self,
        font_id: &str,
        texture: ID3D11ShaderResourceView,
        sampler: ID3D11SamplerState,
        glyphs: HashMap<char, GlyphInfo>,
        line_height: f32,
        base_font_size: f32,
    ) {
        self.font_atlases.insert(
            font_id.to_string(),
            FontAtlas {
                texture,
                sampler,
                glyphs,
                line_height,
                base_font_size,
            },
        );
    }

    pub fn unregister_font_atlas(&mut self, font_id: &str) {
        self.font_atlases.remove(font_id);
        self.submit_groups.remove(font_id);
    }

    pub fn has_renderable_content(&self) -> bool {
        !self.submit_groups.is_empty() || !self.render_buffers.is_empty()
    }

    pub fn set_text(
        &mut self,
        font_id: &str,
        group_id: &str,
        vertices: &[TexturedVertex3D],
        options: PrimitiveOptions,
    ) {
        if vertices.is_empty() {
            if let Some(font_groups) = self.submit_groups.get_mut(font_id) {
                font_groups.remove(group_id);
            }
            return;
        }

        let font_groups = self.submit_groups.entry(font_id.to_string()).or_default();
        font_groups.insert(group_id.to_string(), (vertices.to_vec(), options));
    }

    pub fn clear_text(&mut self, font_id: &str, group_id: &str) {
        if let Some(font_groups) = self.submit_groups.get_mut(font_id) {
            font_groups.remove(group_id);
        }
    }

    pub fn clear_font_text(&mut self, font_id: &str) {
        self.submit_groups.remove(font_id);
    }

    pub fn clear_all_text(&mut self) {
        self.submit_groups.clear();
    }

    pub fn get_font_atlas(&self, font_id: &str) -> Option<&FontAtlas> {
        self.font_atlases.get(font_id)
    }

    pub fn latch_buffers(&mut self) {
        self.render_buffers.clear();

        for (font_id, groups) in &self.submit_groups {
            let font_render_buffer = self.render_buffers.entry(font_id.clone()).or_default();

            for (vertices, options) in groups.values() {
                let buffer = font_render_buffer.entry(*options).or_default();
                let remaining_capacity =
                    MAX_TEXT_VERTEX_BUFFER_CAPACITY.saturating_sub(buffer.len());
                let vertices_to_add = vertices.len().min(remaining_capacity);
                buffer.extend_from_slice(&vertices[..vertices_to_add]);
            }
        }
    }
}

impl Renderer for Text3DRenderer {
    fn draw(&mut self, params: &FrameParams) {
        if self.render_buffers.is_empty() {
            return;
        }

        let context = params.context;

        unsafe {
            let mut original_rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
            let mut original_dsv: Option<ID3D11DepthStencilView> = None;
            context.OMGetRenderTargets(Some(&mut original_rtvs), Some(&mut original_dsv));

            let original_rs_state: Option<ID3D11RasterizerState> = context.RSGetState().ok();
            let mut original_blend_state: Option<ID3D11BlendState> = None;
            let mut original_blend_factor = [0.0; 4];
            let mut original_sample_mask = 0;
            context.OMGetBlendState(
                Some(&mut original_blend_state),
                Some(&mut original_blend_factor),
                Some(&mut original_sample_mask),
            );

            let mut original_depth_state: Option<ID3D11DepthStencilState> = None;
            let mut original_stencil_ref = 0;
            context.OMGetDepthStencilState(
                Some(&mut original_depth_state),
                Some(&mut original_stencil_ref),
            );

            context.OMSetRenderTargets(Some(&original_rtvs), params.depth_stencil_view.as_ref());

            let constants = SceneConstants {
                view_projection: XMMatrix(XMMatrixTranspose(params.view_projection_matrix.0)),
            };
            let mut mapped_cb = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(
                    &self.constant_buffer,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped_cb),
                )
                .unwrap();
            *(mapped_cb.pData as *mut SceneConstants) = constants;
            context.Unmap(&self.constant_buffer, 0);

            context.IASetInputLayout(&self.input_layout);
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            context.PSSetShader(&self.pixel_shader, None);
            context.RSSetState(&self.rasterizer_state);

            for (font_id, options_buffers) in &self.render_buffers {
                let Some(font_atlas) = self.font_atlases.get(font_id) else {
                    continue;
                };

                context.PSSetShaderResources(0, Some(&[Some(font_atlas.texture.clone())]));
                context.PSSetSamplers(0, Some(&[Some(font_atlas.sampler.clone())]));

                let mut sorted_batches: Vec<_> = options_buffers.iter().collect();
                sorted_batches.sort_by_key(|(options, _)| options.render_priority);

                for (options, vertices) in sorted_batches {
                    if vertices.is_empty() {
                        continue;
                    }

                    let blend_state = if options.opaque {
                        &self.blend_state_opaque
                    } else {
                        &self.blend_state_transparent
                    };
                    context.OMSetBlendState(blend_state, None, 0xffffffff);

                    let depth_state =
                        if options.ignore_depth_stencil || params.depth_stencil_view.is_none() {
                            &self.depth_stencil_state_disabled
                        } else {
                            &self.depth_stencil_state_transparent
                        };
                    context.OMSetDepthStencilState(depth_state, options.stencil_ref as u32);

                    let vertex_count = vertices.len() as u32;
                    let mut mapped_vb = D3D11_MAPPED_SUBRESOURCE::default();
                    context
                        .Map(
                            &self.vertex_buffer,
                            0,
                            D3D11_MAP_WRITE_DISCARD,
                            0,
                            Some(&mut mapped_vb),
                        )
                        .unwrap();
                    std::ptr::copy_nonoverlapping(
                        vertices.as_ptr(),
                        mapped_vb.pData as *mut TexturedVertex3D,
                        vertex_count as usize,
                    );
                    context.Unmap(&self.vertex_buffer, 0);

                    let stride = mem::size_of::<TexturedVertex3D>() as u32;
                    let offset = 0;
                    context.IASetVertexBuffers(
                        0,
                        1,
                        Some(&Some(self.vertex_buffer.clone())),
                        Some(&stride),
                        Some(&offset),
                    );
                    context.Draw(vertex_count, 0);
                }
            }

            context.RSSetState(original_rs_state.as_ref());
            context.OMSetBlendState(
                original_blend_state.as_ref(),
                Some(&original_blend_factor),
                original_sample_mask,
            );
            context.OMSetDepthStencilState(original_depth_state.as_ref(), original_stencil_ref);
            context.OMSetRenderTargets(Some(&original_rtvs), original_dsv.as_ref());
        }
    }
}
