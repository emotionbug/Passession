use crate::{
    makepad_shader_compiler::generate_hlsl,
    makepad_math::*,
    os::{
        windows::win32_app::{TRUE, FALSE,},
        windows::win32_window::Win32Window,
    },
    texture::Texture,
    draw_list::DrawListId,
    event::WindowGeom,
    cx::Cx,
    damage::{DamageFrame, DamageRect},
    draw_shader::CxDrawShaderMapping,
    pass::{PassClearColor, PassClearDepth, PassId},
    window::WindowId,
    texture::{ 
        TextureFormat,
        TexturePixel,
        TextureId,
        CxTexture
    },  
    windows::{
        core::{
            PCSTR,
            //ComInterface,
            Interface,
            HRESULT,
        },
        Win32::{
            Foundation::{
                HMODULE,
                HANDLE,
                S_FALSE,
                RECT,
            },
            Graphics::{
                Direct3D11::{
                    D3D11_BIND_INDEX_BUFFER,
                    D3D11_BIND_VERTEX_BUFFER,
                    D3D11_VIEWPORT,
                    D3D11_BUFFER_DESC,
                    D3D11_BOX,
                    D3D11_USAGE_DEFAULT,
                    D3D11_BIND_CONSTANT_BUFFER,
                    D3D11_RESOURCE_MISC_FLAG,
                    D3D11_SUBRESOURCE_DATA,
                    D3D11_CREATE_DEVICE_FLAG,
                    D3D11_FEATURE_D3D11_OPTIONS,
                    D3D11_FEATURE_DATA_D3D11_OPTIONS,
                    D3D11_SDK_VERSION,
                    D3D11_BIND_FLAG,
                    D3D11_BIND_SHADER_RESOURCE,
                    D3D11_TEXTURE2D_DESC,
                    D3D11_BIND_RENDER_TARGET,
                    D3D11_BIND_DEPTH_STENCIL,
                    D3D11_DEPTH_STENCIL_DESC,
                    D3D11_DEPTH_WRITE_MASK_ALL,
                    D3D11_COMPARISON_LESS_EQUAL,
                    D3D11_DEPTH_STENCILOP_DESC,
                    D3D11_STENCIL_OP_REPLACE,
                    D3D11_COMPARISON_ALWAYS,
                    D3D11_DEPTH_STENCIL_VIEW_DESC,
                    D3D11_DSV_DIMENSION_TEXTURE2D,
                    D3D11_CLEAR_DEPTH,
                    D3D11_CLEAR_STENCIL,
                    D3D11_BLEND_DESC,
                    D3D11_RENDER_TARGET_BLEND_DESC,
                    D3D11_BLEND_ONE,
                    D3D11_BLEND_INV_SRC_ALPHA,
                    D3D11_BLEND_OP_ADD,
                    D3D11_COLOR_WRITE_ENABLE_ALL,
                    D3D11_RASTERIZER_DESC,
                    D3D11_CULL_NONE,
                    D3D11_FILL_SOLID,
                    D3D11_INPUT_ELEMENT_DESC,
                    D3D11_INPUT_PER_VERTEX_DATA,
                    D3D11_INPUT_PER_INSTANCE_DATA,
                    D3D11_MAPPED_SUBRESOURCE,
                    D3D11_USAGE_DYNAMIC,
                    D3D11_CPU_ACCESS_WRITE,
                    D3D11_MAP_WRITE_DISCARD,
                    D3D11_QUERY_DESC,
                    D3D11_QUERY_EVENT,
                    ID3D11Device,
                    ID3D11DeviceContext,
                    ID3D11DeviceContext1,
                    ID3D11RenderTargetView,
                    ID3D11Texture2D,
                    ID3D11ShaderResourceView,
                    ID3D11DepthStencilView,
                    ID3D11BlendState,
                    ID3D11RasterizerState,
                    ID3D11DepthStencilState,
                    ID3D11PixelShader,
                    ID3D11VertexShader,
                    ID3D11InputLayout,
                    ID3D11Buffer,
                    D3D11CreateDevice,
                    ID3D11Resource,
                    ID3D11Query,
                },
                Direct3D::{
                    Fxc::D3DCompile,
                    ID3DBlob,
                    D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    D3D_DRIVER_TYPE_WARP,
                    D3D_DRIVER_TYPE,
                    D3D_FEATURE_LEVEL_11_0,
                },
                Dxgi::{
                    IDXGIFactory2,
                    IDXGIResource,
                    IDXGISwapChain1,
                    IDXGIAdapter,
                    IDXGIAdapter1,
                    IDXGIDevice1,
                    DXGI_ADAPTER_FLAG_SOFTWARE,
                    DXGI_PRESENT_TEST,
                    CreateDXGIFactory2,
                    DXGI_CREATE_FACTORY_FLAGS,
                    DXGI_PRESENT,
                    DXGI_PRESENT_PARAMETERS,
                    DXGI_SWAP_CHAIN_FLAG,
                    DXGI_SWAP_CHAIN_DESC1,
                    DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    DXGI_SCALING_NONE,
                    DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                    DXGI_RGBA,
                    Common::{
                        DXGI_FORMAT,
                        DXGI_ALPHA_MODE_IGNORE,
                        DXGI_FORMAT_R8_UNORM, 
                        DXGI_FORMAT_R8G8_UNORM,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        DXGI_SAMPLE_DESC,
                        DXGI_FORMAT_R32G32B32A32_FLOAT,
                        DXGI_FORMAT_R16_FLOAT, 
                        //DXGI_FORMAT_D32_FLOAT_S8X 24_UINT,
                        DXGI_FORMAT_D32_FLOAT,
                        DXGI_FORMAT_R32_UINT,
                        DXGI_FORMAT_R32_FLOAT,
                        DXGI_FORMAT_R32G32_FLOAT,
                        DXGI_FORMAT_R32G32B32_FLOAT,
                    },
                },
            },
        },
    },
};

impl Cx {
    
    fn render_view(
        &mut self,
        pass_id: PassId,
        draw_list_id: DrawListId,
        zbias: &mut f32,
        zbias_step: f32,
        d3d11_cx: &D3d11Cx,
        damage: &mut Option<(&DamageFrame, DamageRect, usize)>,
    ) {
        // tad ugly otherwise the borrow checker locks 'self' and we can't recur
        let draw_items_len = self.draw_lists[draw_list_id].draw_items.len();
        
        {
            let draw_list = &mut self.draw_lists[draw_list_id];
            draw_list.os.draw_list_uniforms.update_with_f32_constant_data(d3d11_cx, draw_list.draw_list_uniforms.as_slice());
        }
        
        for draw_item_id in 0..draw_items_len {
            if let Some(sub_list_id) = self.draw_lists[draw_list_id].draw_items[draw_item_id].kind.sub_list() {
                self.render_view(
                    pass_id,
                    sub_list_id,
                    zbias,
                    zbias_step,
                    d3d11_cx,
                    damage,
                );
            }
            else {
                let draw_list = &mut self.draw_lists[draw_list_id];
                let draw_item = &mut draw_list.draw_items[draw_item_id];
                let draw_call = if let Some(draw_call) = draw_item.kind.draw_call_mut() {
                    draw_call
                } else {
                    continue;
                };
                let cull = damage.as_mut().is_some_and(|(frame, rect, ordinal)| {
                    let cull = frame.should_cull(*ordinal, *rect);
                    *ordinal += 1;
                    cull
                });
                let sh = &self.draw_shaders[draw_call.draw_shader.draw_shader_id];
                if sh.os_shader_id.is_none() { // shader didnt compile somehow
                    continue;
                }
                if sh.mapping.uses_time {
                    self.demo_time_repaint = true;
                }
                let bias = *zbias;
                *zbias += zbias_step;
                if cull || draw_item.instances.as_ref().unwrap().is_empty() {
                    continue;
                }
                let shp = &self.draw_shaders.os_shaders[sh.os_shader_id.unwrap()];

                if draw_call.instance_dirty {
                    draw_call.instance_dirty = false;
                    // update the instance buffer data
                    draw_item.os.inst_vbuf.update_with_f32_vertex_data(d3d11_cx, draw_item.instances.as_ref().unwrap());
                }
                
                let bias_changed = draw_call.draw_call_uniforms.draw_zbias != bias;
                draw_call.draw_call_uniforms.set_zbias(bias);
                
                if draw_call.uniforms_dirty || bias_changed {
                    draw_call.uniforms_dirty = false;
                    draw_item.os.draw_call_uniforms.update_with_f32_constant_data(d3d11_cx, draw_call.draw_call_uniforms.as_slice());
                    
                }
                if draw_call.user_uniforms.len() != 0 {
                    draw_item.os.user_uniforms.update_with_f32_constant_data(d3d11_cx, &mut draw_call.user_uniforms);
                }
                
                let instances = (draw_item.instances.as_ref().unwrap().len() / sh.mapping.instances.total_slots) as u64;
                
                if instances == 0 {
                    continue;
                }
                
                let geometry_id = if let Some(geometry_id) = draw_call.geometry_id {geometry_id}
                else {
                    continue;
                };
                
                let geometry = &mut self.geometries[geometry_id];
                
                if geometry.dirty {
                    geometry.os.geom_ibuf.update_with_u32_index_data(d3d11_cx, &geometry.indices);
                    geometry.os.geom_vbuf.update_with_f32_vertex_data(d3d11_cx, &geometry.vertices);
                    geometry.dirty = false;
                }
                
                unsafe {
                    d3d11_cx.context.VSSetShader(&shp.vertex_shader, None);
                    d3d11_cx.context.PSSetShader(&shp.pixel_shader, None);
                    d3d11_cx.context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    d3d11_cx.context.IASetInputLayout(&shp.input_layout);
                    
                    let geom_ibuf = geometry.os.geom_ibuf.buffer.as_ref().unwrap();
                    d3d11_cx.context.IASetIndexBuffer(geom_ibuf, DXGI_FORMAT_R32_UINT, 0);
                    
                    let geom_slots = sh.mapping.geometries.total_slots;
                    let inst_slots = sh.mapping.instances.total_slots;
                    let strides = [(geom_slots * 4) as u32, (inst_slots * 4) as u32];
                    let offsets = [0u32, 0u32];
                    let buffers = [geometry.os.geom_vbuf.buffer.clone(), draw_item.os.inst_vbuf.buffer.clone()];
                    d3d11_cx.context.IASetVertexBuffers(0, 2, Some(buffers.as_ptr()), Some(strides.as_ptr()), Some(offsets.as_ptr()));
                    
                    fn buffer_slot(d3d11_cx: &D3d11Cx, index: u32, buffer: &Option<ID3D11Buffer>) {
                        unsafe {
                            if let Some(buffer) = buffer.clone() {
                                let buffers = [Some(buffer)];
                                d3d11_cx.context.VSSetConstantBuffers(index, Some(&buffers));
                                d3d11_cx.context.PSSetConstantBuffers(index, Some(&buffers));
                            }
                            else {
                                d3d11_cx.context.VSSetConstantBuffers(index, None);
                                d3d11_cx.context.PSSetConstantBuffers(index, None);
                            }
                        }
                    }
                    buffer_slot(d3d11_cx, 0, &shp.live_uniforms.buffer);
                    buffer_slot(d3d11_cx, 1, &shp.const_table_uniforms.buffer);
                    buffer_slot(d3d11_cx, 2, &draw_item.os.draw_call_uniforms.buffer);
                    buffer_slot(d3d11_cx, 3, &self.passes[pass_id].os.pass_uniforms.buffer);
                    buffer_slot(d3d11_cx, 4, &draw_list.os.draw_list_uniforms.buffer);
                    buffer_slot(d3d11_cx, 5, &draw_item.os.user_uniforms.buffer);
                }
                
                for i in 0..sh.mapping.textures.len() {
                    
                    let texture_id = if let Some(texture) = &draw_call.texture_slots[i] {
                        texture.texture_id()
                    } else {
                        continue;
                    };
                    
                    let cxtexture = &mut self.textures[texture_id];
                    
                    if cxtexture.format.is_shared() {
                        cxtexture.update_shared_texture(
                            &d3d11_cx.device,
                        );
                    }
                    else if cxtexture.format.is_vec(){
                        cxtexture.update_vec_texture(d3d11_cx, self.frame_trace.as_deref());
                    }
                    unsafe {
                        if let Some(sr) = &cxtexture.os.shader_resource_view {
                            d3d11_cx.context.PSSetShaderResources(i as u32, Some(&[Some(sr.clone())]));
                            d3d11_cx.context.VSSetShaderResources(i as u32, Some(&[Some(sr.clone())]));
                        }
                        else {
                            d3d11_cx.context.PSSetShaderResources(i as u32, None);
                            d3d11_cx.context.VSSetShaderResources(i as u32, None);
                        }
                    }
                }
                //if self.passes[pass_id].debug{
                // println!("DRAWING {} {}", geometry.indices.len(), instances);
                //}
                unsafe {
                    d3d11_cx.context.DrawIndexedInstanced(
                        geometry.indices.len() as u32,
                        instances as u32,
                        0,
                        0,
                        0
                    )
                };
            }
        }
    }

    pub fn get_shared_handle(&self, _texture: &Texture) -> HANDLE {
        self.textures[_texture.texture_id()].os.shared_handle
    }

    fn prepare_pass_view(&mut self, pass_id: PassId) -> (Rect, f64) {
        let dpi_factor = self.passes[pass_id].dpi_factor.unwrap();
        
        let pass_rect = self.get_pass_rect(pass_id, dpi_factor).unwrap();
        self.passes[pass_id].set_ortho_matrix(pass_rect.pos, pass_rect.size);
        self.passes[pass_id].paint_dirty = false;

        self.passes[pass_id].set_dpi_factor(dpi_factor);
        (pass_rect, dpi_factor)
    }

    fn setup_pass_render_targets(
        &mut self,
        pass_id: PassId,
        first_target: Option<&ID3D11RenderTargetView>,
        d3d11_cx: &D3d11Cx,
        (pass_rect, dpi_factor): (Rect, f64),
        scissor: Option<(&ID3D11DeviceContext1, RECT)>,
    ) {
        
        let viewport = D3D11_VIEWPORT {
            Width: (pass_rect.size.x * dpi_factor) as f32,
            Height: (pass_rect.size.y * dpi_factor) as f32,
            MinDepth: 0.,
            MaxDepth: 1.,
            TopLeftX: 0.0,
            TopLeftY: 0.0
        };
        unsafe {
            d3d11_cx.context.RSSetViewports(Some(&[viewport]));
        }
        if viewport.Width < 1.0 || viewport.Height < 1.0{
            return
        }
        // set up the color texture array
        let mut color_textures = Vec::<Option<ID3D11RenderTargetView>>::new();
        
        if let Some(render_target) = first_target {
            color_textures.push(Some(render_target.clone()));
            let color = self.passes[pass_id].clear_color;
            let color = [color.x, color.y, color.z, color.w];
            unsafe {
                if let Some((context, rect)) = scissor {
                    context.ClearView(render_target, &color, Some(&[rect]));
                    context.RSSetScissorRects(Some(&[rect]));
                } else {
                    d3d11_cx.context.ClearRenderTargetView(render_target, &color);
                }
            }
        }
        else {
            for color_texture in self.passes[pass_id].color_textures.iter() {
                let cxtexture = &mut self.textures[color_texture.texture.texture_id()];
                let size = pass_rect.size * dpi_factor;
                cxtexture.update_render_target(d3d11_cx, size.x as usize, size.y as usize);
                cxtexture.mark_content_changed();
                let is_initial = cxtexture.take_initial();
                let render_target = cxtexture.os.render_target_view.clone();
                color_textures.push(Some(render_target.clone().unwrap()));
                // possibly clear it
                match color_texture.clear_color {
                    PassClearColor::InitWith(color) => {
                        if is_initial {
                            let color = [color.x, color.y, color.z, color.w];
                            unsafe {d3d11_cx.context.ClearRenderTargetView(render_target.as_ref().unwrap(), &color)}
                        }
                    },
                    PassClearColor::ClearWith(color) => {
                        let color = [color.x, color.y, color.z, color.w];
                        unsafe {d3d11_cx.context.ClearRenderTargetView(render_target.as_ref().unwrap(), &color)}
                    }
                }
            }
        }
        
        // attach/clear depth buffers, if any
        if let Some(depth_texture) = &self.passes[pass_id].depth_texture {
            let cxtexture = &mut self.textures[depth_texture.texture_id()];
            let size = pass_rect.size * dpi_factor;
            cxtexture.update_depth_stencil(d3d11_cx, size.x as usize, size.y as usize);
            let is_initial = cxtexture.take_initial();
                        
            match self.passes[pass_id].clear_depth {
                PassClearDepth::InitWith(depth_clear) => {
                    if is_initial {
                        unsafe {d3d11_cx.context.ClearDepthStencilView(
                            cxtexture.os.depth_stencil_view.as_ref().unwrap(),
                            D3D11_CLEAR_DEPTH.0 as u32 | D3D11_CLEAR_STENCIL.0 as u32,
                            depth_clear,
                            0
                        )}
                    }
                },
                PassClearDepth::ClearWith(depth_clear) => {
                    unsafe {d3d11_cx.context.ClearDepthStencilView(
                        cxtexture.os.depth_stencil_view.as_ref().unwrap(),
                        D3D11_CLEAR_DEPTH.0 as u32 | D3D11_CLEAR_STENCIL.0 as u32,
                        depth_clear,
                        0
                    )}
                }
            }
            unsafe {d3d11_cx.context.OMSetRenderTargets(
                Some(&color_textures),
                None, //cxtexture.os.depth_stencil_view.as_ref().unwrap()
            )}
        }
        else {
            unsafe {d3d11_cx.context.OMSetRenderTargets(
                Some(&color_textures),
                None
            )}
        }
        
        // create depth, blend and raster states
        self.passes[pass_id].os.set_states(d3d11_cx, scissor.is_some());
        
        let cxpass = &mut self.passes[pass_id];
        
        cxpass.os.pass_uniforms.update_with_f32_constant_data(&d3d11_cx, cxpass.pass_uniforms.as_slice());
    }
    
    pub fn draw_pass_to_window(&mut self, pass_id: PassId, vsync: bool, d3d11_window: &mut D3d11Window, d3d11_cx: &D3d11Cx) {
        let root = self.passes[pass_id].main_draw_list_id.unwrap();
        let view = self.prepare_pass_view(pass_id);
        let color = self.passes[pass_id].clear_color;
        let pixels = view.0.size * view.1;
        let size = [pixels.x.round() as i32, pixels.y.round() as i32];
        let full = DamageRect::full(size);
        let eligible = d3d11_window.alloc_size == Some((size[0] as u32, size[1] as u32))
            && !full.is_empty()
            && !d3d11_window.is_in_resize
            && self.passes[pass_id].depth_texture.is_none();
        let mut logical = full;
        let mut scissor_rect = None;
        if let Some(damage) = &mut d3d11_window.damage {
            if eligible {
                logical = damage.frame.collect(self, pass_id, root, size, [color.x.to_bits(), color.y.to_bits(), color.z.to_bits(), color.w.to_bits(), 0]);
                if logical != full {
                    scissor_rect = Some(RECT {
                        left: logical.x0, top: size[1] - logical.y1,
                        right: logical.x1, bottom: size[1] - logical.y0,
                    });
                }
            } else {
                damage.frame.invalidate();
            }
        }
        let damage = d3d11_window.damage.as_ref();
        let scissor = scissor_rect.zip(damage).map(|(rect, damage)| (&damage.context, rect));
        let target = d3d11_window.render_target_view.as_ref();
        let step = self.passes[pass_id].zbias_step;
        self.setup_pass_render_targets(pass_id, target, d3d11_cx, view, scissor);
        let mut replay = scissor_rect.zip(damage).map(|(_, damage)| (&damage.frame, logical, 0));
        self.render_view(pass_id, root, &mut 0.0, step, d3d11_cx, &mut replay);
        if let Some(mut rect) = scissor_rect {
            // DXGI reconciles differences outside the declared region with the
            // previous flip-sequential presentation. Only these pixels are written.
            let parameters = DXGI_PRESENT_PARAMETERS {
                DirtyRectsCount: 1, pDirtyRects: &mut rect, ..Default::default()
            };
            let result = unsafe { d3d11_window.swap_chain.Present1(u32::from(vsync), DXGI_PRESENT(0), &parameters) };
            if result.is_err() {
                // An implementation rejecting dirty presents must not see a
                // partially rendered buffer through the ordinary full present.
                self.setup_pass_render_targets(pass_id, target, d3d11_cx, view, None);
                self.render_view(pass_id, root, &mut 0.0, step, d3d11_cx, &mut None);
                d3d11_window.damage = None;
                d3d11_window.present(vsync);
            } else {
                d3d11_window.present_result(result);
            }
        } else {
            d3d11_window.present(vsync);
        }
        if let Some(damage) = &mut d3d11_window.damage {
            if eligible && !d3d11_window.occluded { damage.frame.commit(); }
        }
        if d3d11_window.first_draw {
            d3d11_window.win32_window.show();
            d3d11_window.first_draw = false;
        }
    }
    
    pub fn draw_pass_to_texture(&mut self, pass_id: PassId,  d3d11_cx: &D3d11Cx,texture_id: Option<TextureId>) {
        // let time1 = Cx::profile_time_ns();
        let draw_list_id = self.passes[pass_id].main_draw_list_id.unwrap();
        let view = self.prepare_pass_view(pass_id);
        
        if let Some(texture_id) = texture_id{
            let render_target_view = self.textures[texture_id].os.render_target_view.clone();
            self.textures[texture_id].mark_content_changed();
            self.setup_pass_render_targets(pass_id, render_target_view.as_ref(), d3d11_cx, view, None);
        }
        else{
            self.setup_pass_render_targets(pass_id, None, d3d11_cx, view, None);
        }
            
        let mut zbias = 0.0;
        let zbias_step = self.passes[pass_id].zbias_step;
        self.render_view(
            pass_id,
            draw_list_id,
            &mut zbias,
            zbias_step,
            &d3d11_cx,
            &mut None,
        );
    }
    
    pub (crate) fn hlsl_compile_shaders(&mut self, d3d11_cx: &D3d11Cx) {
        for draw_shader_ptr in &self.draw_shaders.compile_set {
            if let Some(item) = self.draw_shaders.ptr_to_item.get(&draw_shader_ptr) {
                let cx_shader = &mut self.draw_shaders.shaders[item.draw_shader_id];
                let draw_shader_def = self.shader_registry.draw_shader_defs.get(&draw_shader_ptr);
                let hlsl = generate_hlsl::generate_shader(
                    draw_shader_def.as_ref().unwrap(),
                    &cx_shader.mapping.const_table,
                    &self.shader_registry
                );
                
                if cx_shader.mapping.flags.debug {
                    crate::log!("{}", hlsl);
                }
                // lets see if we have the shader already
                for (index, ds) in self.draw_shaders.os_shaders.iter().enumerate() {
                    if ds.hlsl == hlsl {
                        cx_shader.os_shader_id = Some(index);
                        break;
                    }
                }
                if cx_shader.os_shader_id.is_none() {
                    if let Some(shp) = CxOsDrawShader::new(d3d11_cx, hlsl, &cx_shader.mapping) {
                        cx_shader.os_shader_id = Some(self.draw_shaders.os_shaders.len());
                        self.draw_shaders.os_shaders.push(shp);
                    }
                }
            }
        }
        self.draw_shaders.compile_set.clear();
    }

    pub fn share_texture_for_presentable_image(
        &mut self,
        texture: &Texture,
    ) -> crate::cx_stdin::SharedPresentableImageOsHandle {
        let cxtexture = &mut self.textures[texture.texture_id()];
        cxtexture.update_shared_texture(self.os.d3d11_device.as_ref().unwrap());
        cxtexture.os.shared_handle.0 as u64
    }
}

fn texture_pixel_to_dx11_pixel(pix:&TexturePixel)->DXGI_FORMAT{
    match pix{
        TexturePixel::BGRAu8 => DXGI_FORMAT_B8G8R8A8_UNORM,
        TexturePixel::RGBAf16 => DXGI_FORMAT_R16_FLOAT,
        TexturePixel::RGBAf32 => DXGI_FORMAT_R32G32B32A32_FLOAT,
        TexturePixel::Ru8  => DXGI_FORMAT_R8_UNORM,
        TexturePixel::RGu8  => DXGI_FORMAT_R8G8_UNORM,
        TexturePixel::Rf32  => DXGI_FORMAT_R32_FLOAT,
        TexturePixel::D32 => DXGI_FORMAT_D32_FLOAT,
    }   
}

struct DxgiDamage {
    frame: DamageFrame,
    context: ID3D11DeviceContext1,
}

impl DxgiDamage {
    fn new(cx: &D3d11Cx) -> Option<Self> {
        if crate::damage::force_full_redraw() { return None; }
        let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
        unsafe {
            cx.device.CheckFeatureSupport(
                D3D11_FEATURE_D3D11_OPTIONS,
                (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS).cast(),
                std::mem::size_of_val(&options) as u32,
            ).ok()?;
        }
        options.ClearView.as_bool().then(|| cx.context.cast().ok()).flatten()
            .map(|context| Self { frame: DamageFrame::default(), context })
    }
}

pub struct D3d11Window {
    pub window_id: WindowId,
    pub is_in_resize: bool,
    pub window_geom: WindowGeom,
    pub win32_window: Box<Win32Window>,
    pub render_target_view: Option<ID3D11RenderTargetView >,
    pub swap_texture: Option<ID3D11Texture2D >,
    /// Paseo: the swap chain's buffer size in pixels; `None` forces the next resize.
    pub alloc_size: Option<(u32, u32)>,
    pub first_draw: bool,
    pub swap_chain: IDXGISwapChain1,
    pub(crate) drawable: bool,
    occluded: bool,
    next_visibility_probe: Option<std::time::Instant>,
    damage: Option<DxgiDamage>,
}

impl D3d11Window {
    pub fn new(window_id: WindowId, d3d11_cx: &D3d11Cx, inner_size: DVec2, position: Option<DVec2>, title: &str) -> D3d11Window {

        // create window, and then initialize it; this is needed because
        // GWLP_USERDATA needs to reference a stable and existing window
        let mut win32_window = Box::new(Win32Window::new(window_id, title, position));
        win32_window.init(inner_size);
        
        let wg = win32_window.get_window_geom();
        let (width, height) = swap_chain_pixels(&wg);
        
        let mut damage = DxgiDamage::new(d3d11_cx);
        let mut sc_desc = DXGI_SWAP_CHAIN_DESC1 {
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            BufferCount: 2,
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Flags: 0,
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            SampleDesc: DXGI_SAMPLE_DESC {Count: 1, Quality: 0,},
            Scaling: DXGI_SCALING_NONE,
            Stereo: FALSE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        };
        
        unsafe {
            let create = |description: &DXGI_SWAP_CHAIN_DESC1| d3d11_cx.factory.CreateSwapChainForHwnd(
                &d3d11_cx.device, win32_window.hwnd, description, None, None,
            );
            let sequential = if damage.is_some() {
                sc_desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
                create(&sc_desc).ok()
            } else {
                None
            };
            let swap_chain = sequential.unwrap_or_else(|| {
                damage = None;
                sc_desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
                create(&sc_desc).unwrap()
            });
            
            let swap_texture = swap_chain.GetBuffer(0).unwrap();
            let mut render_target_view = None;
            d3d11_cx.device.CreateRenderTargetView(&swap_texture, None, Some(&mut render_target_view)).unwrap();
            // Cosmetic (the colour behind an undersized swap chain): Wine's DXGI returns
            // E_NOTIMPL, which must not abort window creation.
            let _ = swap_chain.SetBackgroundColor(&mut DXGI_RGBA {
                r: 0.3,
                g: 0.3,
                b: 0.3,
                a: 1.0
            });
            D3d11Window {
                first_draw: true,
                is_in_resize: false,
                window_id: window_id,
                alloc_size: Some((width, height)),
                window_geom: wg,
                win32_window: win32_window,
                swap_texture: Some(swap_texture),
                render_target_view: render_target_view,
                swap_chain: swap_chain,
                drawable: true,
                occluded: false,
                next_visibility_probe: None,
                damage,
            }
        }
    }
    
    pub fn start_resize(&mut self) {
        self.is_in_resize = true;
    }
    
    // switch back to swapchain
    pub fn stop_resize(&mut self) {
        self.is_in_resize = false;
        self.alloc_size = None;
    }
    
    
    pub fn resize_buffers(&mut self, d3d11_cx: &D3d11Cx) {
        let (width, height) = swap_chain_pixels(&self.window_geom);
        if self.alloc_size == Some((width, height)) {
            return
        }
        if let Some(damage) = &mut self.damage { damage.frame.invalidate(); }
        self.alloc_size = Some((width, height));
        self.swap_texture = None;
        self.render_target_view = None;
        
        unsafe {
            // An occluded present need not release the previous OM binding.
            d3d11_cx.context.OMSetRenderTargets(None, None);
            self.swap_chain.ResizeBuffers(
                2,
                width,
                height,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0)
            ).unwrap();
            
            let swap_texture = self.swap_chain.GetBuffer(0).unwrap();
            let mut render_target_view = None;
            d3d11_cx.device.CreateRenderTargetView(&swap_texture, None, Some(&mut render_target_view)).unwrap();
            
            self.swap_texture = Some(swap_texture);
            self.render_target_view = render_target_view;
        }
    }
    
    pub(crate) fn update_drawable(&mut self) {
        use crate::windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
        let (width, height) = swap_chain_pixels(&self.window_geom);
        if width == 0 || height == 0 || (!self.first_draw && unsafe {
            IsIconic(self.win32_window.hwnd).as_bool() || !IsWindowVisible(self.win32_window.hwnd).as_bool()
        }) {
            if let Some(damage) = &mut self.damage { damage.frame.invalidate(); }
            self.drawable = false;
            return;
        }
        if self.occluded {
            let now = std::time::Instant::now();
            if self.next_visibility_probe.is_none_or(|deadline| now >= deadline) {
                let test = unsafe { self.swap_chain.Present(0, DXGI_PRESENT_TEST) };
                self.occluded = test != crate::windows::Win32::Foundation::S_OK;
                self.next_visibility_probe = Some(now + std::time::Duration::from_millis(100));
            }
        }
        self.drawable = !self.occluded;
    }

    pub fn present(&mut self, vsync: bool) {
        let result = unsafe { self.swap_chain.Present(u32::from(vsync), DXGI_PRESENT(0)) };
        self.present_result(result);
    }

    fn present_result(&mut self, result: HRESULT) {
        if result == crate::windows::Win32::Foundation::DXGI_STATUS_OCCLUDED {
            if let Some(damage) = &mut self.damage { damage.frame.invalidate(); }
            self.occluded = true;
            self.drawable = false;
            self.next_visibility_probe = None;
        } else {
            result.unwrap();
        }
    }
    
}

/// Paseo: the client area in pixels for the swap chain. Upstream compared logical sizes,
/// so a scale change that kept the logical size (100% to 200%) left the buffers at the old
/// pixel size, and truncating `size * dpi` could fall one pixel short of the client area.
fn swap_chain_pixels(geom: &WindowGeom) -> (u32, u32) {
    let pixels = geom.inner_size * geom.dpi_factor;
    (pixels.x.round() as u32, pixels.y.round() as u32)
}

#[cfg(test)]
mod swap_chain_tests {
    use super::*;

    #[test]
    fn same_logical_size_at_double_scale_needs_twice_the_pixels() {
        let at_100 = WindowGeom { inner_size: dvec2(800.0, 600.0), dpi_factor: 1.0, ..Default::default() };
        let at_200 = WindowGeom { inner_size: dvec2(800.0, 600.0), dpi_factor: 2.0, ..Default::default() };
        assert_eq!(swap_chain_pixels(&at_100), (800, 600));
        assert_eq!(swap_chain_pixels(&at_200), (1600, 1200));
    }

    #[test]
    fn fractional_scale_rounds_back_to_the_client_pixels() {
        // A 1559 x 1027 pixel client area at 175% is 890.857... x 586.857... logical.
        let geom = WindowGeom { inner_size: dvec2(1559.0 / 1.75, 1027.0 / 1.75), dpi_factor: 1.75, ..Default::default() };
        assert_eq!(swap_chain_pixels(&geom), (1559, 1027));
    }
}

#[derive(Clone)]
pub struct D3d11Cx {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub query: ID3D11Query,
    pub factory: IDXGIFactory2,
}

impl D3d11Cx {
    
    pub fn new(info: &mut crate::gpu_info::GpuInfo) -> crate::windows::core::Result<Self> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let mut hardware = None;
            let mut index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(index) {
                index += 1;
                if adapter.GetDesc1().is_ok_and(|desc| desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0) {
                    continue;
                }
                match Self::create_device(Some(&adapter), D3D_DRIVER_TYPE_UNKNOWN) {
                    Ok(pair) => {
                        hardware = Some(pair);
                        break;
                    }
                    Err(err) => crate::warning!("Could not initialize D3D11 hardware adapter {}: {err}", index - 1),
                }
            }
            let (device, context, warp) = match hardware {
                Some((device, context)) => (device, context, false),
                None => {
                    crate::warning!("No usable D3D11 hardware adapter; falling back to WARP software rendering");
                    let (device, context) = Self::create_device(None, D3D_DRIVER_TYPE_WARP)?;
                    (device, context, true)
                }
            };
            let dxgi = device.cast::<IDXGIDevice1>()?;
            if let Err(err) = dxgi.SetMaximumFrameLatency(1) {
                crate::warning!("Could not limit D3D11 frame latency: {err}");
            }
            let desc = dxgi.GetAdapter()?.cast::<IDXGIAdapter1>()?.GetDesc1()?;
            let len = desc.Description.iter().position(|&ch| ch == 0).unwrap_or(desc.Description.len());
            let level = device.GetFeatureLevel().0;
            info.record_native_renderer(crate::gpu_info::GpuRenderer {
                api: "D3D11",
                renderer: String::from_utf16_lossy(&desc.Description[..len]),
                version: Some(format!("feature level {}.{}", level >> 12, (level >> 8) & 0xf)),
                warp: Some(warp),
                software: warp || desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
                ..Default::default()
            });
            let mut query = None;
            device.CreateQuery(&D3D11_QUERY_DESC {
                Query: D3D11_QUERY_EVENT,
                MiscFlags: 0,
            }, Some(&mut query))?;
            Ok(Self { device, context, factory, query: query.ok_or(crate::windows::Win32::Foundation::E_FAIL)? })
        }
    }

    unsafe fn create_device(
        adapter: Option<&IDXGIAdapter>, driver: D3D_DRIVER_TYPE,
    ) -> crate::windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            adapter, driver, HMODULE::default(), D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]), D3D11_SDK_VERSION,
            Some(&mut device), None, Some(&mut context),
        )?;
        use crate::windows::Win32::Foundation::E_FAIL;
        Ok((device.ok_or(E_FAIL)?, context.ok_or(E_FAIL)?))
    }

    pub fn start_querying(&self) {

        // QUERY_EVENT signals when rendering is complete
        unsafe { self.context.End(&self.query) };
    }

    pub fn is_gpu_done(&self) -> bool {
        let hresult = unsafe { (Interface::vtable(&self.context).GetData)(Interface::as_raw(&self.context),Interface::as_raw(&self.query),std::ptr::null_mut(),0,0) };
        hresult != S_FALSE
    }
}

#[derive(Clone, Default)]
pub struct CxOsDrawList {
    pub draw_list_uniforms: D3d11Buffer
}

#[derive(Default, Clone)]
pub struct CxOsDrawCall {
    pub draw_call_uniforms: D3d11Buffer,
    pub user_uniforms: D3d11Buffer,
    pub inst_vbuf: D3d11Buffer
}

#[derive(Default, Clone)]
pub struct D3d11Buffer {
    pub last_size: usize,
    pub buffer: Option<ID3D11Buffer>
}

impl D3d11Buffer {
    fn create_buffer_or_update(&mut self, d3d11_cx: &D3d11Cx, buffer_desc: &D3D11_BUFFER_DESC, sub_data: &D3D11_SUBRESOURCE_DATA, len_slots: usize, data: *const std::ffi::c_void) {
        if self.last_size == len_slots {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            let p_mapped : *mut _ = &mut mapped;
            unsafe {
                d3d11_cx.context.Map(self.buffer.as_ref().unwrap(),
                        0, D3D11_MAP_WRITE_DISCARD, 0, Some(p_mapped)).unwrap();

                std::ptr::copy_nonoverlapping(data, mapped.pData, len_slots * 4);
                d3d11_cx.context.Unmap(self.buffer.as_ref().unwrap(), 0);
            }
        } else {
            self.last_size = len_slots;
            unsafe { d3d11_cx.device.CreateBuffer(buffer_desc, Some(sub_data), Some(&mut self.buffer)).unwrap() }
        }
    }

    pub fn update_with_data(&mut self, d3d11_cx: &D3d11Cx, bind_flags: D3D11_BIND_FLAG, len_slots: usize, data: *const std::ffi::c_void) {
        let buffer_desc = D3D11_BUFFER_DESC {
            Usage: D3D11_USAGE_DYNAMIC,
            ByteWidth: (len_slots * 4) as u32,
            BindFlags: bind_flags.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0
        };
        
        let sub_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: data,
            SysMemPitch: 0,
            SysMemSlicePitch: 0
        };
        self.create_buffer_or_update(d3d11_cx, &buffer_desc, &sub_data, len_slots, data);
    }
    
    pub fn update_with_u32_index_data(&mut self, d3d11_cx: &D3d11Cx, data: &[u32]) {
        self.update_with_data(d3d11_cx, D3D11_BIND_INDEX_BUFFER, data.len(), data.as_ptr() as *const _);
    }
    
    pub fn update_with_f32_vertex_data(&mut self, d3d11_cx: &D3d11Cx, data: &[f32]) {
        self.update_with_data(d3d11_cx, D3D11_BIND_VERTEX_BUFFER, data.len(), data.as_ptr() as *const _);
    }
    
    pub fn update_with_f32_constant_data(&mut self, d3d11_cx: &D3d11Cx, data: &[f32]) {
        if data.len() == 0 {
            return
        }
        if (data.len() & 3) != 0 { // we have to align the data at the end
            let mut new_data = data.to_vec();
            let steps = 4 - (data.len() & 3);
            for _ in 0..steps {
                new_data.push(0.0);
            };
            return self.update_with_f32_constant_data(d3d11_cx, &new_data);
        }
        let sub_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr() as *const _,
            SysMemPitch: 0,
            SysMemSlicePitch: 0
        };
        let len_slots = data.len();
        
        let buffer_desc = D3D11_BUFFER_DESC {
            Usage: D3D11_USAGE_DYNAMIC,
            ByteWidth: (len_slots * 4) as u32,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
            MiscFlags: 0,
            StructureByteStride: 0
        };
        let data = unsafe { core::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of::<f32>() * data.len()).as_ptr() as *const _ };
        self.create_buffer_or_update(d3d11_cx, &buffer_desc, &sub_data, len_slots, data);
    }
}


#[derive(Default)]
pub struct CxOsTexture {
    texture: Option<ID3D11Texture2D >,
    pub shared_handle: HANDLE,
    shader_resource_view: Option<ID3D11ShaderResourceView >,
    render_target_view: Option<ID3D11RenderTargetView >,
    depth_stencil_view: Option<ID3D11DepthStencilView >,
}

impl CxTexture {
    
    fn update_vec_texture(
        &mut self,
        d3d11_cx: &D3d11Cx,
        trace: Option<&crate::frame_trace::FrameTrace>,
    ) {
        let updated = self.updated();
        if updated.is_empty() {
            return;
        }

        let alloc = self.format.as_vec_alloc().unwrap();
        let reallocate = self.alloc.as_ref() != Some(&alloc) || self.os.texture.is_none();
        let Some(upload) = self.format.vec_upload(updated, reallocate) else { return };
        assert!(!matches!(self.format, TextureFormat::VecMipBGRAu8_32 { .. }),
            "mipmapped vector textures are not supported on D3D11");
        let data = upload.bytes.as_ptr().cast();
        let row_pitch = u32::try_from(upload.row_stride).expect("D3D11 texture row too wide");

        if reallocate {
            let sub_data = D3D11_SUBRESOURCE_DATA {
                pSysMem: data,
                SysMemPitch: row_pitch,
                SysMemSlicePitch: 0,
            };
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: alloc.width as u32,
                Height: alloc.height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: texture_pixel_to_dx11_pixel(&alloc.pixel),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe { d3d11_cx.device.CreateTexture2D(&texture_desc, Some(&sub_data), Some(&mut texture)).unwrap() };
            let mut shader_resource_view = None;
            unsafe {
                d3d11_cx.device.CreateShaderResourceView(
                    texture.as_ref().unwrap(), None, Some(&mut shader_resource_view),
                ).unwrap();
            }
            self.os.texture = texture;
            self.os.shader_resource_view = shader_resource_view;
            self.alloc = Some(alloc);
        } else {
            let end = upload.rect.max();
            let region = D3D11_BOX {
                left: upload.rect.origin.x as u32,
                top: upload.rect.origin.y as u32,
                front: 0,
                right: end.x as u32,
                bottom: end.y as u32,
                back: 1,
            };
            unsafe {
                d3d11_cx.context.UpdateSubresource(
                    self.os.texture.as_ref().unwrap(), 0, Some(&region), data, row_pitch, 0,
                );
            }
        }
        if let Some(trace) = trace {
            trace.texture_upload(upload.rect.size.width, upload.rect.size.height, upload.bytes_per_pixel);
        }
        self.take_updated();
    }
    
    pub fn update_render_target(
        &mut self,
        d3d11_cx: &D3d11Cx,
        width: usize,
        height: usize
    ) {
        if self.alloc_render(width, height){
            let alloc = self.alloc.as_ref().unwrap();
            let misc_flags = D3D11_RESOURCE_MISC_FLAG(0);
            let format = texture_pixel_to_dx11_pixel(&alloc.pixel);
            
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: width as u32,
                Height: height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {Count: 1, Quality: 0},
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: misc_flags.0 as u32,
            };
            
            let mut texture = None;
            unsafe {d3d11_cx.device.CreateTexture2D(&texture_desc, None, Some(&mut texture)).unwrap()};
            let resource: ID3D11Resource = texture.clone().unwrap().cast().unwrap();
            let mut shader_resource_view = None;
            unsafe {d3d11_cx.device.CreateShaderResourceView(&resource, None, Some(&mut shader_resource_view)).unwrap()};
            let mut render_target_view = None;
            unsafe {d3d11_cx.device.CreateRenderTargetView(&resource, None, Some(&mut render_target_view)).unwrap()};
            
            self.os.texture = texture;
            self.os.shader_resource_view = shader_resource_view;
            self.os.render_target_view = render_target_view;
        }
    }
    
    pub fn update_depth_stencil(
        &mut self,
        d3d11_cx: &D3d11Cx,
        width: usize,
        height: usize
    ) {
        if self.alloc_depth(width, height){
            let alloc = self.alloc.as_ref().unwrap();
            let format;
            match alloc.pixel {
                TexturePixel::D32 => {
                    format = DXGI_FORMAT_D32_FLOAT;
                }
                _ => {
                    panic!("Wrong format for update_depth_stencil");
                }
            }
            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: width as u32,
                Height: height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {Count: 1, Quality: 0},
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32, // | D3D11_BIND_SHADER_RESOURCE,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            
            let mut texture = None;
            unsafe {d3d11_cx.device.CreateTexture2D(&texture_desc, None, Some(&mut texture)).unwrap()};
            let resource: ID3D11Resource = texture.clone().unwrap().cast().unwrap();
            //let shader_resource_view = unsafe {d3d11_cx.device.CreateShaderResourceView(&texture, None).unwrap()};
            
            let dsv_desc = D3D11_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT_D32_FLOAT,
                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D,
                Flags: 0,
                ..Default::default()
            };
            
            let mut depth_stencil_view = None;
            unsafe {d3d11_cx.device.CreateDepthStencilView(&resource, Some(&dsv_desc), Some(&mut depth_stencil_view)).unwrap()};
            
            self.os.depth_stencil_view = depth_stencil_view;
            self.os.texture = texture;
            self.os.shader_resource_view = None; //Some(shader_resource_view);
        }
    }
    
    fn update_shared_texture(
        &mut self,
        d3d11_device: &ID3D11Device,
    ) {
        if self.alloc_shared(){
            let alloc = self.alloc.as_ref().unwrap();

            let texture_desc = D3D11_TEXTURE2D_DESC {
                Width: alloc.width as u32,
                Height: alloc.height as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 2,  // D3D11_RESOURCE_MISC_SHARED
            };
            
            let mut texture = None;
            unsafe {d3d11_device.CreateTexture2D(&texture_desc, None, Some(&mut texture)).unwrap()};
            let resource: ID3D11Resource = texture.clone().unwrap().cast().unwrap();
            let mut shader_resource_view = None;
            unsafe {d3d11_device.CreateShaderResourceView(&resource, None, Some(&mut shader_resource_view)).unwrap()};

            // get IDXGIResource interface on newly created texture object
            let dxgi_resource: IDXGIResource = resource.cast().unwrap();
            //let mut dxgi_resource_ptr = None;
            //unsafe { resource.query(IDXGIResource::IID,Some(&mut dxgi_resource_ptr)).unwrap() };
            //let dxgi_resource = dxgi_resource_ptr.as_ref().unwrap().into();

            // get shared handle of this resource
            let handle = unsafe { dxgi_resource.GetSharedHandle().unwrap() };
            //log!("created new shared texture with handle {:?}",handle);

            self.os.texture = texture;
            self.os.shader_resource_view = shader_resource_view;
            self.os.shared_handle = handle;
        }
    }

    pub fn update_from_shared_handle(&mut self,
        d3d11_cx: &D3d11Cx,
        handle: HANDLE,
    ) {
        if self.alloc_shared(){
            let mut texture: Option<ID3D11Texture2D> = None;
            if let Ok(()) = unsafe { d3d11_cx.device.OpenSharedResource(handle,&mut texture) } {
    
                let resource: ID3D11Resource = texture.clone().unwrap().cast().unwrap();
                let mut shader_resource_view = None;
                unsafe {d3d11_cx.device.CreateShaderResourceView(&resource, None, Some(&mut shader_resource_view)).unwrap()};
                let mut render_target_view = None;
                unsafe {d3d11_cx.device.CreateRenderTargetView(&resource, None, Some(&mut render_target_view)).unwrap()};
                self.os.texture = texture;
                self.os.render_target_view = render_target_view;
                self.os.shader_resource_view = shader_resource_view;
            }
        }
    }
}

impl CxOsPass {
    fn set_states(&mut self, d3d11_cx: &D3d11Cx, scissor: bool) {
        
        if self.blend_state.is_none() {
            
            let mut blend_desc: D3D11_BLEND_DESC = Default::default();
            blend_desc.AlphaToCoverageEnable = FALSE;
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: TRUE,
                SrcBlend: D3D11_BLEND_ONE,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            unsafe {d3d11_cx.device.CreateBlendState(&blend_desc, Some(&mut self.blend_state)).unwrap()}
        }
        
        let raster_state = if scissor { &mut self.scissor_state } else { &mut self.raster_state };
        if raster_state.is_none() {
            let raster_desc = D3D11_RASTERIZER_DESC {
                AntialiasedLineEnable: FALSE,
                CullMode: D3D11_CULL_NONE,
                DepthBias: 0,
                DepthBiasClamp: 0.0,
                DepthClipEnable: TRUE,
                FillMode: D3D11_FILL_SOLID,
                FrontCounterClockwise: FALSE,
                MultisampleEnable: FALSE,
                ScissorEnable: if scissor { TRUE } else { FALSE },
                SlopeScaledDepthBias: 0.0,
            };
            unsafe {d3d11_cx.device.CreateRasterizerState(&raster_desc, Some(raster_state)).unwrap()}
        }
        
        if self.depth_stencil_state.is_none() {
            let ds_desc = D3D11_DEPTH_STENCIL_DESC {
                DepthEnable: TRUE,
                DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D11_COMPARISON_LESS_EQUAL,
                StencilEnable: FALSE,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: D3D11_DEPTH_STENCILOP_DESC {
                    StencilFailOp: D3D11_STENCIL_OP_REPLACE,
                    StencilDepthFailOp: D3D11_STENCIL_OP_REPLACE,
                    StencilPassOp: D3D11_STENCIL_OP_REPLACE,
                    StencilFunc: D3D11_COMPARISON_ALWAYS,
                },
                BackFace: D3D11_DEPTH_STENCILOP_DESC {
                    StencilFailOp: D3D11_STENCIL_OP_REPLACE,
                    StencilDepthFailOp: D3D11_STENCIL_OP_REPLACE,
                    StencilPassOp: D3D11_STENCIL_OP_REPLACE,
                    StencilFunc: D3D11_COMPARISON_ALWAYS,
                },
            };
            unsafe {d3d11_cx.device.CreateDepthStencilState(&ds_desc, Some(&mut self.depth_stencil_state)).unwrap()}
        }
        
        unsafe {
            d3d11_cx.context.RSSetState(raster_state.as_ref().unwrap());
            let blend_factor = [0., 0., 0., 0.];
            d3d11_cx.context.OMSetBlendState(self.blend_state.as_ref().unwrap(), Some(&blend_factor), 0xffffffff);
            d3d11_cx.context.OMSetDepthStencilState(self.depth_stencil_state.as_ref().unwrap(), 0);
        }
    }
}

#[derive(Default, Clone)]
pub struct CxOsPass {
    pass_uniforms: D3d11Buffer,
    blend_state: Option<ID3D11BlendState >,
    raster_state: Option<ID3D11RasterizerState >,
    scissor_state: Option<ID3D11RasterizerState>,
    depth_stencil_state: Option<ID3D11DepthStencilState >
}

#[derive(Default, Clone)]
pub struct CxOsGeometry {
    pub geom_vbuf: D3d11Buffer,
    pub geom_ibuf: D3d11Buffer,
}

#[derive(Clone)]
pub struct CxOsDrawShader {
    pub hlsl: String,
    pub const_table_uniforms: D3d11Buffer,
    pub live_uniforms: D3d11Buffer,
    pub pixel_shader: ID3D11PixelShader,
    pub vertex_shader: ID3D11VertexShader,
    pub pixel_shader_blob: ID3DBlob,
    pub vertex_shader_blob: ID3DBlob,
    pub input_layout: ID3D11InputLayout
}

impl CxOsDrawShader {
    
    fn new(d3d11_cx: &D3d11Cx, hlsl: String, mapping: &CxDrawShaderMapping) -> Option<Self> {
        
        fn compile_shader(target: &str, entry: &str, shader: &str) -> Result<ID3DBlob, String> {
            unsafe {
                let shader_bytes = shader.as_bytes();
                let mut blob = None;
                let mut errors = None;
                if D3DCompile(
                    shader_bytes.as_ptr() as *const _,
                    shader_bytes.len(),
                    PCSTR("makepad_shader\0".as_ptr()), // sourcename
                    None, // defines
                    None, // include
                    PCSTR(entry.as_ptr()), // entry point
                    PCSTR(target.as_ptr()), // target
                    0, // flags1
                    0, // flags2
                    &mut blob,
                    Some(&mut errors)
                ).is_ok() {
                    return Ok(blob.unwrap());
                };
                let error = errors.unwrap();
                let pointer = error.GetBufferPointer();
                let size = error.GetBufferSize();
                let slice = std::slice::from_raw_parts(pointer as *const u8, size as usize);
                return Err(String::from_utf8_lossy(slice).into_owned());
            }
        }
        fn split_source(src: &str) -> String {
            let mut r = String::new();
            let split = src.split("\n");
            for (line, chunk) in split.enumerate() {
                r.push_str(&(line + 1).to_string());
                r.push_str(":");
                r.push_str(chunk);
                r.push_str("\n");
            }
            return r
        }
        
        fn slots_to_dxgi_format(slots: usize) -> DXGI_FORMAT {
            
            match slots {
                1 => DXGI_FORMAT_R32_FLOAT,
                2 => DXGI_FORMAT_R32G32_FLOAT,
                3 => DXGI_FORMAT_R32G32B32_FLOAT,
                4 => DXGI_FORMAT_R32G32B32A32_FLOAT,
                _ => panic!("slots_to_dxgi_format unsupported slotcount {}", slots)
            }
        }
        
        let vs_blob = match compile_shader("vs_5_0\0", "vertex_main\0", &hlsl) {
            Err(msg) => {
                println!("Cannot compile vertexshader\n{}\n{}", msg, split_source(&hlsl));
                return None
            },
            Ok(blob) => {
                blob
            }
        };
        
        let ps_blob = match compile_shader("ps_5_0\0", "pixel_main\0", &hlsl) {
            Err(msg) => {
                println!("Cannot compile pixelshader\n{}\n{}", msg, split_source(&hlsl));
                return None
            },
            Ok(blob) => {
                blob
            }
        };
        
        let mut vs = None;
        unsafe {d3d11_cx.device.CreateVertexShader(
            std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize() as usize),
            None,
            Some(&mut vs)
        ).unwrap()};
        
        let mut ps = None;
        unsafe {d3d11_cx.device.CreatePixelShader(
            std::slice::from_raw_parts(ps_blob.GetBufferPointer() as *const u8, ps_blob.GetBufferSize() as usize),
            None,
            Some(&mut ps)
        ).unwrap()};
        
        let mut layout_desc = Vec::new();
        let mut strings = Vec::new();
        
        for (index, geom) in mapping.geometries.inputs.iter().enumerate() {
            
            strings.push(format!("GEOM{}\0", generate_hlsl::index_to_char(index))); //std::char::from_u32(index as u32 + 65).unwrap())).unwrap());
            
            layout_desc.push(D3D11_INPUT_ELEMENT_DESC {
                SemanticName: PCSTR(strings.last().unwrap().as_ptr()),
                SemanticIndex: 0,
                Format: slots_to_dxgi_format(geom.slots),
                InputSlot: 0,
                AlignedByteOffset: (geom.offset * 4) as u32,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0
            })
        }
        
        let mut index = 0;
        for inst in &mapping.instances.inputs {
            if inst.slots == 16 {
                for i in 0..4 {
                    strings.push(format!("INST{}\0", generate_hlsl::index_to_char(index))); //std::char::from_u32(index as u32 + 65).unwrap())).unwrap());
                    layout_desc.push(D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: PCSTR(strings.last().unwrap().as_ptr()),
                        SemanticIndex: 0,
                        Format: slots_to_dxgi_format(4),
                        InputSlot: 1,
                        AlignedByteOffset: (inst.offset * 4 + i * 16) as u32,
                        InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                        InstanceDataStepRate: 1
                    });
                    index += 1;
                }
            }
            else if inst.slots == 9 {
                for i in 0..3 {
                    strings.push(format!("INST{}\0", generate_hlsl::index_to_char(index))); //std::char::from_u32(index as u32 + 65).unwrap())).unwrap());
                    layout_desc.push(D3D11_INPUT_ELEMENT_DESC {
                        SemanticName: PCSTR(strings.last().unwrap().as_ptr()),
                        SemanticIndex: 0,
                        Format: slots_to_dxgi_format(3),
                        InputSlot: 1,
                        AlignedByteOffset: (inst.offset * 4 + i * 9) as u32,
                        InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                        InstanceDataStepRate: 1
                    });
                    index += 1;
                }
            }
            else {
                strings.push(format!("INST{}\0", generate_hlsl::index_to_char(index))); //std::char::from_u32(index as u32 + 65).unwrap())).unwrap());
                layout_desc.push(D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: PCSTR(strings.last().unwrap().as_ptr()),
                    SemanticIndex: 0,
                    Format: slots_to_dxgi_format(inst.slots),
                    InputSlot: 1,
                    AlignedByteOffset: (inst.offset * 4) as u32,
                    InputSlotClass: D3D11_INPUT_PER_INSTANCE_DATA,
                    InstanceDataStepRate: 1
                });
                index += 1;
            }
        }
        
        let mut input_layout = None;
        unsafe {
            d3d11_cx.device.CreateInputLayout(
                &layout_desc,
                std::slice::from_raw_parts(vs_blob.GetBufferPointer() as *const u8, vs_blob.GetBufferSize() as usize),
                Some(&mut input_layout)
            ).unwrap()
        };
        
        let mut live_uniforms = D3d11Buffer::default();
        live_uniforms.update_with_f32_constant_data(d3d11_cx, mapping.live_uniforms_buf.as_ref());
        
        let mut const_table_uniforms = D3d11Buffer::default();
        const_table_uniforms.update_with_f32_constant_data(d3d11_cx, mapping.const_table.table.as_ref());
        
        Some(Self {
            hlsl,
            const_table_uniforms,
            live_uniforms,
            pixel_shader: ps.unwrap(),
            vertex_shader: vs.unwrap(),
            pixel_shader_blob: ps_blob,
            vertex_shader_blob: vs_blob,
            input_layout: input_layout.unwrap()
        })
    }
}
