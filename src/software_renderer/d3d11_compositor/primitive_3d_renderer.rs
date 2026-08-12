use directx_math::{XMMatrix, XMMatrixTranspose};
use std::{collections::HashMap, mem};
use windows::core::{BOOL, PCSTR};
use windows::Win32::{
    Graphics::{
        Direct3D::{D3D11_PRIMITIVE_TOPOLOGY_LINELIST, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST},
        Direct3D11::*,
        Dxgi::Common::{DXGI_FORMAT_R32G32B32A32_FLOAT, DXGI_FORMAT_R32G32B32_FLOAT},
    },
};

use crate::software_renderer::d3d11_compositor::traits::{FrameParams, Renderer};

/// Maximum number of vertices that can be stored in each vertex buffer.
/// This must match the buffer_capacity used when creating the vertex buffers.
const MAX_VERTEX_BUFFER_CAPACITY: usize = 65536;

#[derive(Clone, Copy, Debug)]
pub enum PrimitiveType {
    Triangles,
    Lines,
}

/// Defines the blending mode for rendering custom primitives.
/// This controls how the primitive's colors blend with the existing pixels on the render target,
/// and also affects depth testing behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[derive(Default)]
pub enum BlendMode {
    /// Standard alpha blending (SRC_ALPHA, INV_SRC_ALPHA) with depth testing enabled (no writes).
    /// Most commonly used for transparent objects.
    /// - Blending: Enabled (alpha blending)
    /// - Depth Testing: Enabled (read-only, no depth writes)
    #[default]
    Transparent,
    /// No blending - pixels are written directly (opaque) with depth testing disabled.
    /// Use this when the camera/player is inside the geometry (e.g., battle royale cylinder).
    /// - Blending: Disabled (opaque rendering)
    /// - Depth Testing: Disabled (always renders, ignores depth buffer)
    Opaque,
}


#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct PrimitiveOptions {
    // Depth options
    pub ignore_depth_stencil: bool,
    pub write_to_depth: bool,
    pub depth_bias: i32,

    // Rasterizer options
    pub cull_back: bool,
    pub wireframe: bool,
    pub scissor_enabled: bool,

    // Blend options
    pub opaque: bool,

    // Stencil options
    pub stencil_ref: u8,
    pub stencil_write: bool,
    pub stencil_test: bool,

    // Ordering
    pub render_priority: i32,
}

impl Default for PrimitiveOptions {
    fn default() -> Self {
        Self {
            ignore_depth_stencil: false,
            write_to_depth: false,
            depth_bias: 0,
            cull_back: false,
            wireframe: false,
            scissor_enabled: true,
            opaque: false,
            stencil_ref: 0,
            stencil_write: false,
            stencil_test: false,
            render_priority: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Vertex3D {
    pub position: [f32; 3],
    pub color: [f32; 4],
}

#[repr(C)]
struct TimeConstants {
    g_time: f32,
    _padding: [f32; 3],
}

#[repr(C)]
struct SceneConstants {
    view_projection: XMMatrix,
}

#[derive(Clone)]
struct CustomEffectResources {
    vertex_shader: Option<ID3D11VertexShader>,
    pixel_shader: ID3D11PixelShader,
    textures: HashMap<u32, ID3D11ShaderResourceView>,
    samplers: HashMap<u32, ID3D11SamplerState>,
    constant_buffer: Option<ID3D11Buffer>,
    constant_data: Vec<u8>,
    blend_mode: BlendMode,
}

#[derive(Clone)]
pub struct Primitive3DRenderer {
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    constant_buffer: ID3D11Buffer,
    time_constant_buffer: ID3D11Buffer,

    vertex_buffer_triangles: ID3D11Buffer,
    vertex_buffer_lines: ID3D11Buffer,

    submit_groups_triangles: HashMap<String, (Vec<Vertex3D>, PrimitiveOptions)>,
    submit_groups_lines: HashMap<String, (Vec<Vertex3D>, PrimitiveOptions)>,
    render_buffer_triangles: HashMap<PrimitiveOptions, Vec<Vertex3D>>,
    render_buffer_lines: HashMap<PrimitiveOptions, Vec<Vertex3D>>,

    blend_state_transparent: ID3D11BlendState,
    blend_state_opaque: ID3D11BlendState,
    depth_stencil_state: ID3D11DepthStencilState,
    depth_stencil_state_transparent: ID3D11DepthStencilState,
    depth_stencil_state_disabled: ID3D11DepthStencilState,
    rasterizer_state_cull_back: ID3D11RasterizerState,
    rasterizer_state_cull_none: ID3D11RasterizerState,
    rasterizer_state_wireframe: ID3D11RasterizerState,
    rasterizer_state_cull_back_wireframe: ID3D11RasterizerState,

    device: ID3D11Device,

    submit_groups_triangles_custom: HashMap<String, (String, Vec<Vertex3D>, PrimitiveOptions)>,

    submit_groups_lines_custom: HashMap<String, (String, Vec<Vertex3D>, PrimitiveOptions)>,

    render_buffer_triangles_custom: HashMap<(String, PrimitiveOptions), Vec<Vertex3D>>,

    render_buffer_lines_custom: HashMap<(String, PrimitiveOptions), Vec<Vertex3D>>,

    custom_effects: HashMap<String, CustomEffectResources>,
}

impl Primitive3DRenderer {
    pub fn new(device: &ID3D11Device) -> Self {
        let vs_bytes = include_bytes!("./shaders/primitive_vs.cso");
        let ps_bytes = include_bytes!("./shaders/primitive_ps.cso");

        let mut vertex_shader: Option<ID3D11VertexShader> = None;
        unsafe {
            device
                .CreateVertexShader(vs_bytes, None, Some(&mut vertex_shader))
                .expect("Failed to create primitive VS");
        }

        let mut pixel_shader: Option<ID3D11PixelShader> = None;
        unsafe {
            device
                .CreatePixelShader(ps_bytes, None, Some(&mut pixel_shader))
                .expect("Failed to create primitive PS");
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
                SemanticName: PCSTR(c"COLOR".as_ptr().cast()),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 12,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];

        let mut input_layout: Option<ID3D11InputLayout> = None;
        unsafe {
            device
                .CreateInputLayout(&input_element_descs, vs_bytes, Some(&mut input_layout))
                .expect("Failed to create primitive input layout");
        }

        let buffer_capacity = MAX_VERTEX_BUFFER_CAPACITY;
        let vertex_buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: (mem::size_of::<Vertex3D>() * buffer_capacity) as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };

        let mut vertex_buffer_triangles: Option<ID3D11Buffer> = None;
        unsafe {
            device
                .CreateBuffer(
                    &vertex_buffer_desc,
                    None,
                    Some(&mut vertex_buffer_triangles),
                )
                .expect("Failed to create triangle vertex buffer");
        }

        let mut vertex_buffer_lines: Option<ID3D11Buffer> = None;
        unsafe {
            device
                .CreateBuffer(&vertex_buffer_desc, None, Some(&mut vertex_buffer_lines))
                .expect("Failed to create line vertex buffer");
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
                .expect("Failed to create constant buffer");
        }

        let time_constant_buffer_desc = D3D11_BUFFER_DESC {
            ByteWidth: mem::size_of::<TimeConstants>() as u32,
            Usage: D3D11_USAGE_DYNAMIC,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            ..Default::default()
        };

        let mut time_constant_buffer: Option<ID3D11Buffer> = None;
        unsafe {
            device
                .CreateBuffer(
                    &time_constant_buffer_desc,
                    None,
                    Some(&mut time_constant_buffer),
                )
                .expect("Failed to create time constant buffer");
        }

        let mut blend_desc_transparent = D3D11_BLEND_DESC::default();
        let rt_blend_desc = D3D11_RENDER_TARGET_BLEND_DESC {
            BlendEnable: BOOL(1),
            SrcBlend: D3D11_BLEND_SRC_ALPHA,
            DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
            BlendOp: D3D11_BLEND_OP_ADD,
            SrcBlendAlpha: D3D11_BLEND_ZERO,
            DestBlendAlpha: D3D11_BLEND_ONE,
            BlendOpAlpha: D3D11_BLEND_OP_ADD,
            RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
        };
        blend_desc_transparent.RenderTarget[0] = rt_blend_desc;

        let mut blend_state_transparent: Option<ID3D11BlendState> = None;
        unsafe {
            device
                .CreateBlendState(&blend_desc_transparent, Some(&mut blend_state_transparent))
                .expect("Failed to create transparent blend state");
        }

        let blend_desc_opaque = D3D11_BLEND_DESC::default();
        let mut blend_state_opaque: Option<ID3D11BlendState> = None;
        unsafe {
            device
                .CreateBlendState(&blend_desc_opaque, Some(&mut blend_state_opaque))
                .expect("Failed to create opaque blend state");
        }

        let depth_desc = D3D11_DEPTH_STENCIL_DESC {
            DepthEnable: BOOL(1),
            DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL,
            DepthFunc: D3D11_COMPARISON_LESS,
            StencilEnable: BOOL(0),
            ..Default::default()
        };

        let mut depth_stencil_state: Option<ID3D11DepthStencilState> = None;
        unsafe {
            device
                .CreateDepthStencilState(&depth_desc, Some(&mut depth_stencil_state))
                .expect("Failed to create depth stencil state");
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
                .expect("Failed to create transparent depth stencil state");
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
                .expect("Failed to create disabled depth stencil state");
        }

        let mut rast_desc = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_BACK,
            ScissorEnable: BOOL(1),
            ..Default::default()
        };
        let mut rasterizer_state_cull_back: Option<ID3D11RasterizerState> = None;
        unsafe {
            device
                .CreateRasterizerState(&rast_desc, Some(&mut rasterizer_state_cull_back))
                .expect("Failed to create cull-back rasterizer state");
        }

        rast_desc.CullMode = D3D11_CULL_NONE;
        let mut rasterizer_state_cull_none: Option<ID3D11RasterizerState> = None;
        unsafe {
            device
                .CreateRasterizerState(&rast_desc, Some(&mut rasterizer_state_cull_none))
                .expect("Failed to create cull-none rasterizer state");
        }

        let rast_desc_wireframe = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_WIREFRAME,
            CullMode: D3D11_CULL_NONE,
            ScissorEnable: BOOL(1),
            ..Default::default()
        };
        let mut rasterizer_state_wireframe: Option<ID3D11RasterizerState> = None;
        unsafe {
            device
                .CreateRasterizerState(&rast_desc_wireframe, Some(&mut rasterizer_state_wireframe))
                .expect("Failed to create wireframe rasterizer state");
        }

        let rast_desc_cull_back_wireframe = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_WIREFRAME,
            CullMode: D3D11_CULL_BACK,
            ScissorEnable: BOOL(1),
            ..Default::default()
        };
        let mut rasterizer_state_cull_back_wireframe: Option<ID3D11RasterizerState> = None;
        unsafe {
            device
                .CreateRasterizerState(&rast_desc_cull_back_wireframe, Some(&mut rasterizer_state_cull_back_wireframe))
                .expect("Failed to create cull-back wireframe rasterizer state");
        }

        Self {
            vertex_shader: vertex_shader.unwrap(),
            pixel_shader: pixel_shader.unwrap(),
            input_layout: input_layout.unwrap(),
            vertex_buffer_triangles: vertex_buffer_triangles.unwrap(),
            vertex_buffer_lines: vertex_buffer_lines.unwrap(),
            constant_buffer: constant_buffer.unwrap(),
            time_constant_buffer: time_constant_buffer.unwrap(),
            submit_groups_triangles: HashMap::new(),
            submit_groups_lines: HashMap::new(),
            render_buffer_triangles: HashMap::new(),
            render_buffer_lines: HashMap::new(),
            blend_state_transparent: blend_state_transparent.unwrap(),
            blend_state_opaque: blend_state_opaque.unwrap(),
            depth_stencil_state: depth_stencil_state.unwrap(),
            depth_stencil_state_transparent: depth_stencil_state_transparent.unwrap(),
            depth_stencil_state_disabled: depth_stencil_state_disabled.unwrap(),
            rasterizer_state_cull_back: rasterizer_state_cull_back.unwrap(),
            rasterizer_state_cull_none: rasterizer_state_cull_none.unwrap(),
            rasterizer_state_wireframe: rasterizer_state_wireframe.unwrap(),
            rasterizer_state_cull_back_wireframe: rasterizer_state_cull_back_wireframe.unwrap(),
            device: device.clone(),
            submit_groups_triangles_custom: HashMap::new(),
            submit_groups_lines_custom: HashMap::new(),
            render_buffer_triangles_custom: HashMap::new(),
            render_buffer_lines_custom: HashMap::new(),
            custom_effects: HashMap::new(),
        }
    }

    pub fn set_primitives(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
    ) {
        self.set_primitives_ex(group_id, triangles, lines, PrimitiveOptions::default());
    }

    pub fn set_primitives_ex(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        options: PrimitiveOptions,
    ) {
        if triangles.is_empty() {
            self.submit_groups_triangles.remove(group_id);
        } else {
            self.submit_groups_triangles
                .insert(group_id.to_string(), (triangles.to_vec(), options));
        }

        if lines.is_empty() {
            self.submit_groups_lines.remove(group_id);
        } else {
            self.submit_groups_lines
                .insert(group_id.to_string(), (lines.to_vec(), options));
        }
    }

    pub fn register_custom_pixel_shader(
        &mut self,
        device: &ID3D11Device,
        effect_id: &str,
        vs_bytes: Option<&[u8]>,
        ps_bytes: &[u8],
        constant_buffer_size: Option<u32>,
        blend_mode: BlendMode,
    ) {
        if self.custom_effects.contains_key(effect_id) {
            return;
        }

        let vertex_shader = if let Some(vs_data) = vs_bytes {
            let mut vs: Option<ID3D11VertexShader> = None;
            unsafe {
                device
                    .CreateVertexShader(vs_data, None, Some(&mut vs))
                    .expect("Failed to create custom VS");
            }
            vs
        } else {
            None
        };

        let mut pixel_shader: Option<ID3D11PixelShader> = None;
        unsafe {
            device
                .CreatePixelShader(ps_bytes, None, Some(&mut pixel_shader))
                .expect("Failed to create custom PS");
        }

        let constant_buffer = if let Some(size) = constant_buffer_size {
            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: size,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut cb: Option<ID3D11Buffer> = None;
            unsafe {
                device
                    .CreateBuffer(&cb_desc, None, Some(&mut cb))
                    .expect("Failed to create custom constant buffer");
            }
            cb
        } else {
            None
        };

        self.custom_effects.insert(
            effect_id.to_string(),
            CustomEffectResources {
                vertex_shader,
                pixel_shader: pixel_shader.unwrap(),
                textures: HashMap::new(),
                samplers: HashMap::new(),
                constant_buffer,
                constant_data: Vec::new(),
                blend_mode,
            },
        );
    }

    pub fn set_custom_effect_texture_at_slot(
        &mut self,
        effect_id: &str,
        slot: u32,
        texture: ID3D11ShaderResourceView,
        sampler: ID3D11SamplerState,
    ) {
        if let Some(effect) = self.custom_effects.get_mut(effect_id) {
            effect.textures.insert(slot, texture);
            effect.samplers.insert(slot, sampler);
        }
    }

    pub fn clear_custom_effect_texture_at_slot(&mut self, effect_id: &str, slot: u32) {
        if let Some(effect) = self.custom_effects.get_mut(effect_id) {
            effect.textures.remove(&slot);
            effect.samplers.remove(&slot);
        }
    }

    pub fn set_custom_effect_textures_bulk(
        &mut self,
        effect_id: &str,
        textures: Vec<(u32, ID3D11ShaderResourceView, ID3D11SamplerState)>,
    ) {
        if let Some(effect) = self.custom_effects.get_mut(effect_id) {
            for (slot, texture, sampler) in textures {
                effect.textures.insert(slot, texture);
                effect.samplers.insert(slot, sampler);
            }
        }
    }

    pub fn update_custom_effect_constants(&mut self, effect_id: &str, data: &[u8]) {
        if let Some(effect) = self.custom_effects.get_mut(effect_id) {
            effect.constant_data = data.to_vec();
        }
    }

    pub fn set_custom_primitives(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
    ) {
        self.set_custom_primitives_ex(
            group_id,
            triangles,
            lines,
            effect_id,
            PrimitiveOptions::default(),
        );
    }

    pub fn set_custom_primitives_ex(
        &mut self,
        group_id: &str,
        triangles: &[Vertex3D],
        lines: &[Vertex3D],
        effect_id: &str,
        options: PrimitiveOptions,
    ) {
        if triangles.is_empty() {
            self.submit_groups_triangles_custom.remove(group_id);
        } else {
            self.submit_groups_triangles_custom.insert(
                group_id.to_string(),
                (effect_id.to_string(), triangles.to_vec(), options),
            );
        }

        if lines.is_empty() {
            self.submit_groups_lines_custom.remove(group_id);
        } else {
            self.submit_groups_lines_custom.insert(
                group_id.to_string(),
                (effect_id.to_string(), lines.to_vec(), options),
            );
        }
    }

    pub fn clear_primitives(&mut self, group_id: &str) {
        self.submit_groups_triangles.remove(group_id);
        self.submit_groups_lines.remove(group_id);
        self.submit_groups_triangles_custom.remove(group_id);
        self.submit_groups_lines_custom.remove(group_id);
    }

    pub fn clear_all_primitives(&mut self) {
        self.submit_groups_triangles.clear();
        self.submit_groups_lines.clear();
        self.submit_groups_triangles_custom.clear();
        self.submit_groups_lines_custom.clear();
    }

    pub fn latch_buffers(&mut self) {
        self.render_buffer_triangles.clear();
        for (group_vertices, options) in self.submit_groups_triangles.values() {
            let buffer = self.render_buffer_triangles.entry(*options).or_default();
            let remaining_capacity = MAX_VERTEX_BUFFER_CAPACITY.saturating_sub(buffer.len());
            let vertices_to_add = group_vertices.len().min(remaining_capacity);
            buffer.extend_from_slice(&group_vertices[..vertices_to_add]);
        }

        self.render_buffer_lines.clear();
        for (group_vertices, options) in self.submit_groups_lines.values() {
            let buffer = self.render_buffer_lines.entry(*options).or_default();
            let remaining_capacity = MAX_VERTEX_BUFFER_CAPACITY.saturating_sub(buffer.len());
            let vertices_to_add = group_vertices.len().min(remaining_capacity);
            buffer.extend_from_slice(&group_vertices[..vertices_to_add]);
        }

        self.render_buffer_triangles_custom.clear();
        for (effect_id, group_vertices, options) in self.submit_groups_triangles_custom.values() {
            let buffer = self
                .render_buffer_triangles_custom
                .entry((effect_id.clone(), *options))
                .or_default();
            let remaining_capacity = MAX_VERTEX_BUFFER_CAPACITY.saturating_sub(buffer.len());
            let vertices_to_add = group_vertices.len().min(remaining_capacity);
            buffer.extend_from_slice(&group_vertices[..vertices_to_add]);
        }
        self.render_buffer_lines_custom.clear();
        for (effect_id, group_vertices, options) in self.submit_groups_lines_custom.values() {
            let buffer = self
                .render_buffer_lines_custom
                .entry((effect_id.clone(), *options))
                .or_default();
            let remaining_capacity = MAX_VERTEX_BUFFER_CAPACITY.saturating_sub(buffer.len());
            let vertices_to_add = group_vertices.len().min(remaining_capacity);
            buffer.extend_from_slice(&group_vertices[..vertices_to_add]);
        }
    }
}

impl Primitive3DRenderer {
    fn get_rasterizer_state(&self, options: &PrimitiveOptions) -> &ID3D11RasterizerState {
        match (options.cull_back, options.wireframe) {
            (true, true) => &self.rasterizer_state_cull_back_wireframe,
            (true, false) => &self.rasterizer_state_cull_back,
            (false, true) => &self.rasterizer_state_wireframe,
            (false, false) => &self.rasterizer_state_cull_none,
        }
    }

    fn get_or_create_depth_stencil_state(
        &self,
        options: &PrimitiveOptions,
        has_depth_view: bool,
    ) -> ID3D11DepthStencilState {
        if options.ignore_depth_stencil || !has_depth_view {
            return self.depth_stencil_state_disabled.clone();
        }

        if options.write_to_depth {
            return self.depth_stencil_state.clone();
        }

        self.depth_stencil_state_transparent.clone()
    }
}

impl Renderer for Primitive3DRenderer {
    fn draw(&mut self, params: &FrameParams) {
        if self.render_buffer_triangles.is_empty()
            && self.render_buffer_lines.is_empty()
            && self.render_buffer_triangles_custom.is_empty()
            && self.render_buffer_lines_custom.is_empty()
        {
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

            let time_constants = TimeConstants {
                g_time: params.time,
                _padding: [0.0; 3],
            };
            let mut mapped_cb = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(
                    &self.time_constant_buffer,
                    0,
                    D3D11_MAP_WRITE_DISCARD,
                    0,
                    Some(&mut mapped_cb),
                )
                .unwrap();
            *(mapped_cb.pData as *mut TimeConstants) = time_constants;
            context.Unmap(&self.time_constant_buffer, 0);

            context.IASetInputLayout(&self.input_layout);
            context.VSSetShader(&self.vertex_shader, None);
            context.VSSetConstantBuffers(0, Some(&[Some(self.constant_buffer.clone())]));
            context.PSSetConstantBuffers(1, Some(&[Some(self.time_constant_buffer.clone())]));

            if !self.render_buffer_triangles.is_empty() {
                context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                context.PSSetShader(&self.pixel_shader, None);

                let mut sorted_batches: Vec<_> = self.render_buffer_triangles.iter().collect();
                sorted_batches.sort_by_key(|(options, _)| options.render_priority);

                for (options, vertices) in sorted_batches {
                    if vertices.is_empty() {
                        continue;
                    }

                    let rasterizer_state = self.get_rasterizer_state(options);
                    context.RSSetState(rasterizer_state);

                    let blend_state = if options.opaque {
                        &self.blend_state_opaque
                    } else {
                        &self.blend_state_transparent
                    };
                    context.OMSetBlendState(blend_state, None, 0xffffffff);

                    let depth_state = self.get_or_create_depth_stencil_state(options, params.depth_stencil_view.is_some());
                    context.OMSetDepthStencilState(&depth_state, options.stencil_ref as u32);

                    let vertex_count = vertices.len() as u32;
                    let mut mapped_vb = D3D11_MAPPED_SUBRESOURCE::default();
                    context
                        .Map(
                            &self.vertex_buffer_triangles,
                            0,
                            D3D11_MAP_WRITE_DISCARD,
                            0,
                            Some(&mut mapped_vb),
                        )
                        .unwrap();
                    std::ptr::copy_nonoverlapping(
                        vertices.as_ptr(),
                        mapped_vb.pData as *mut Vertex3D,
                        vertex_count as usize,
                    );
                    context.Unmap(&self.vertex_buffer_triangles, 0);

                    let stride = mem::size_of::<Vertex3D>() as u32;
                    let offset = 0;
                    context.IASetVertexBuffers(
                        0,
                        1,
                        Some(&Some(self.vertex_buffer_triangles.clone())),
                        Some(&stride),
                        Some(&offset),
                    );
                    context.Draw(vertex_count, 0);
                }
            }

            if !self.render_buffer_lines.is_empty() {
                context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_LINELIST);
                context.PSSetShader(&self.pixel_shader, None);

                let mut sorted_batches: Vec<_> = self.render_buffer_lines.iter().collect();
                sorted_batches.sort_by_key(|(options, _)| options.render_priority);

                for (options, vertices) in sorted_batches {
                    if vertices.is_empty() {
                        continue;
                    }

                    let rasterizer_state = self.get_rasterizer_state(options);
                    context.RSSetState(rasterizer_state);

                    let blend_state = if options.opaque {
                        &self.blend_state_opaque
                    } else {
                        &self.blend_state_transparent
                    };
                    context.OMSetBlendState(blend_state, None, 0xffffffff);

                    let depth_state = self.get_or_create_depth_stencil_state(options, params.depth_stencil_view.is_some());
                    context.OMSetDepthStencilState(&depth_state, options.stencil_ref as u32);

                    let vertex_count = vertices.len() as u32;
                    let mut mapped_vb = D3D11_MAPPED_SUBRESOURCE::default();
                    context
                        .Map(
                            &self.vertex_buffer_lines,
                            0,
                            D3D11_MAP_WRITE_DISCARD,
                            0,
                            Some(&mut mapped_vb),
                        )
                        .unwrap();
                    std::ptr::copy_nonoverlapping(
                        vertices.as_ptr(),
                        mapped_vb.pData as *mut Vertex3D,
                        vertex_count as usize,
                    );
                    context.Unmap(&self.vertex_buffer_lines, 0);

                    let stride = mem::size_of::<Vertex3D>() as u32;
                    let offset = 0;
                    context.IASetVertexBuffers(
                        0,
                        1,
                        Some(&Some(self.vertex_buffer_lines.clone())),
                        Some(&stride),
                        Some(&offset),
                    );
                    context.Draw(vertex_count, 0);
                }
            }

            if !self.render_buffer_triangles_custom.is_empty() {
                context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

                let mut sorted_batches: Vec<_> = self.render_buffer_triangles_custom.iter().collect();
                sorted_batches.sort_by_key(|((_, options), _)| options.render_priority);

                for ((effect_id, options), vertices) in sorted_batches {
                    if vertices.is_empty() {
                        continue;
                    }

                    if let Some(effect) = self.custom_effects.get(effect_id) {
                        let rasterizer_state = self.get_rasterizer_state(options);
                        context.RSSetState(rasterizer_state);

                        let blend_state = if options.opaque {
                            &self.blend_state_opaque
                        } else {
                            match effect.blend_mode {
                                BlendMode::Transparent => &self.blend_state_transparent,
                                BlendMode::Opaque => &self.blend_state_opaque,
                            }
                        };
                        context.OMSetBlendState(blend_state, None, 0xffffffff);

                        let depth_state = self.get_or_create_depth_stencil_state(options, params.depth_stencil_view.is_some());
                        context.OMSetDepthStencilState(&depth_state, options.stencil_ref as u32);

                        if let Some(vs) = &effect.vertex_shader {
                            context.VSSetShader(vs, None);
                        } else {
                            context.VSSetShader(&self.vertex_shader, None);
                        }

                        context.PSSetShader(&effect.pixel_shader, None);

                        for (&slot, texture) in &effect.textures {
                            context.PSSetShaderResources(slot, Some(&[Some(texture.clone())]));
                        }

                        for (&slot, sampler) in &effect.samplers {
                            context.PSSetSamplers(slot, Some(&[Some(sampler.clone())]));
                        }

                        if let Some(cb) = &effect.constant_buffer {
                            if !effect.constant_data.is_empty() {
                                let mut mapped_cb = D3D11_MAPPED_SUBRESOURCE::default();
                                context
                                    .Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped_cb))
                                    .unwrap();
                                std::ptr::copy_nonoverlapping(
                                    effect.constant_data.as_ptr(),
                                    mapped_cb.pData as *mut u8,
                                    effect.constant_data.len(),
                                );
                                context.Unmap(cb, 0);
                            }
                            context.PSSetConstantBuffers(2, Some(&[Some(cb.clone())]));
                        }

                        let vertex_count = vertices.len() as u32;
                        let mut mapped_vb = D3D11_MAPPED_SUBRESOURCE::default();
                        context
                            .Map(
                                &self.vertex_buffer_triangles,
                                0,
                                D3D11_MAP_WRITE_DISCARD,
                                0,
                                Some(&mut mapped_vb),
                            )
                            .unwrap();
                        std::ptr::copy_nonoverlapping(
                            vertices.as_ptr(),
                            mapped_vb.pData as *mut Vertex3D,
                            vertex_count as usize,
                        );
                        context.Unmap(&self.vertex_buffer_triangles, 0);

                        let stride = mem::size_of::<Vertex3D>() as u32;
                        let offset = 0;
                        context.IASetVertexBuffers(
                            0,
                            1,
                            Some(&Some(self.vertex_buffer_triangles.clone())),
                            Some(&stride),
                            Some(&offset),
                        );
                        context.Draw(vertex_count, 0);
                    }
                }
            }

            if !self.render_buffer_lines_custom.is_empty() {
                context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_LINELIST);

                let mut sorted_batches: Vec<_> = self.render_buffer_lines_custom.iter().collect();
                sorted_batches.sort_by_key(|((_, options), _)| options.render_priority);

                for ((effect_id, options), vertices) in sorted_batches {
                    if vertices.is_empty() {
                        continue;
                    }

                    if let Some(effect) = self.custom_effects.get(effect_id) {
                        let rasterizer_state = self.get_rasterizer_state(options);
                        context.RSSetState(rasterizer_state);

                        let blend_state = if options.opaque {
                            &self.blend_state_opaque
                        } else {
                            match effect.blend_mode {
                                BlendMode::Transparent => &self.blend_state_transparent,
                                BlendMode::Opaque => &self.blend_state_opaque,
                            }
                        };
                        context.OMSetBlendState(blend_state, None, 0xffffffff);

                        let depth_state = self.get_or_create_depth_stencil_state(options, params.depth_stencil_view.is_some());
                        context.OMSetDepthStencilState(&depth_state, options.stencil_ref as u32);

                        context.PSSetShader(&effect.pixel_shader, None);

                        for (&slot, texture) in &effect.textures {
                            context.PSSetShaderResources(slot, Some(&[Some(texture.clone())]));
                        }

                        for (&slot, sampler) in &effect.samplers {
                            context.PSSetSamplers(slot, Some(&[Some(sampler.clone())]));
                        }

                        if let Some(cb) = &effect.constant_buffer {
                            if !effect.constant_data.is_empty() {
                                let mut mapped_cb = D3D11_MAPPED_SUBRESOURCE::default();
                                context
                                    .Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped_cb))
                                    .unwrap();
                                std::ptr::copy_nonoverlapping(
                                    effect.constant_data.as_ptr(),
                                    mapped_cb.pData as *mut u8,
                                    effect.constant_data.len(),
                                );
                                context.Unmap(cb, 0);
                            }
                            context.PSSetConstantBuffers(2, Some(&[Some(cb.clone())]));
                        }

                        let vertex_count = vertices.len() as u32;
                        let mut mapped_vb = D3D11_MAPPED_SUBRESOURCE::default();
                        context
                            .Map(
                                &self.vertex_buffer_lines,
                                0,
                                D3D11_MAP_WRITE_DISCARD,
                                0,
                                Some(&mut mapped_vb),
                            )
                            .unwrap();
                        std::ptr::copy_nonoverlapping(
                            vertices.as_ptr(),
                            mapped_vb.pData as *mut Vertex3D,
                            vertex_count as usize,
                        );
                        context.Unmap(&self.vertex_buffer_lines, 0);

                        let stride = mem::size_of::<Vertex3D>() as u32;
                        let offset = 0;
                        context.IASetVertexBuffers(
                            0,
                            1,
                            Some(&Some(self.vertex_buffer_lines.clone())),
                            Some(&stride),
                            Some(&offset),
                        );
                        context.Draw(vertex_count, 0);
                    }
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
