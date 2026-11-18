use {
    makepad_objc_sys::{
        msg_send,
        sel,
        class,
        sel_impl,
    },
    crate::{
        makepad_objc_sys::objc_block,
        makepad_shader_compiler::{
            generate_metal,
            generate_metal::MetalGeneratedShader,
        },
        makepad_live_id::*,
        os::{
            apple::apple_sys::*,
            apple::apple_util::{
                nsstring_to_string,
                str_to_nsstring,
            },
            cx_stdin::PresentableDraw,
        },
        draw_list::DrawListId,
        cx::Cx,
        pass::{PassClearColor, PassClearDepth, PassId},
        studio::{AppToStudio, GPUSample, StudioScreenshotResponse},
        texture::{
            CxTexture,
            Texture,
            TexturePixel,
            TextureAlloc,
            TextureFormat,
        },
    },
    std::time::{Instant},
    std::sync::{
        Arc,
        Condvar,
        Mutex,
    },
};

#[cfg(target_os = "macos")]
use crate::{
    metal_xpc::store_xpc_service_texture
};

#[cfg(target_os = "macos")]
use crate::{
    damage::{force_full_redraw, DamageFrame, DamageRect},
    os::apple::macos::metal_surface::{SurfaceCompletion, SurfacePresenter},
};

#[cfg(target_os = "macos")]
type DamageCursor<'a> = Option<(&'a DamageFrame, DamageRect, usize)>;

struct MetalDraw<'a> {
    encoder: ObjcId,
    metal_cx: &'a MetalCx,
    zbias: f32,
    zbias_step: f32,
    gpu_read_guards: &'a mut Vec<MetalRwLockGpuReadGuard>,
    #[cfg(target_os = "macos")]
    damage: DamageCursor<'a>,
}


impl Cx {
    
    
    fn render_view(
        &mut self,
        pass_id: PassId,
        draw_list_id: DrawListId,
        draw: &mut MetalDraw<'_>,
    ) {
        // tad ugly otherwise the borrow checker locks 'self' and we can't recur
        let draw_items_len = self.draw_lists[draw_list_id].draw_items.len();
        //self.views[view_id].set_clipping_uniforms();
        //self.draw_lists[draw_list_id].uniform_view_transform(&Mat4::identity());
        
        for draw_item_id in 0..draw_items_len {
            if let Some(sub_list_id) = self.draw_lists[draw_list_id].draw_items[draw_item_id].kind.sub_list() {
                self.render_view(pass_id, sub_list_id, draw);
            }
            else {
                let draw_list = &mut self.draw_lists[draw_list_id];
                let draw_item = &mut draw_list.draw_items[draw_item_id];
                let draw_call = if let Some(draw_call) = draw_item.kind.draw_call_mut() {
                    draw_call
                }else {
                    continue;
                };

                // Match DamageFrame's paint ordinal, including uncompiled shaders.
                #[cfg(target_os = "macos")]
                let culled = if let Some((frame, repair, ordinal)) = &mut draw.damage {
                    let culled = frame.should_cull(*ordinal, *repair);
                    *ordinal += 1;
                    culled
                } else { false };
                
                let sh = &self.draw_shaders[draw_call.draw_shader.draw_shader_id];
                if sh.os_shader_id.is_none() { // shader didnt compile somehow
                    continue;
                }
                let shp = &self.draw_shaders.os_shaders[sh.os_shader_id.unwrap()];
                
                if sh.mapping.uses_time{
                    self.demo_time_repaint = true;
                }
                // A skipped draw must still consume its original depth bias.
                draw_call.draw_call_uniforms.set_zbias(draw.zbias);
                draw.zbias += draw.zbias_step;
                #[cfg(target_os = "macos")]
                if culled { continue; }
                let MetalDraw { encoder, metal_cx, gpu_read_guards, .. } = draw;
                let encoder = *encoder;
                
                
                if draw_call.instance_dirty {
                    draw_call.instance_dirty = false;
                    // update the instance buffer data
                    self.os.bytes_written += draw_item.instances.as_ref().unwrap().len() * 4;
                    draw_item.os.instance_buffer.next();
                    draw_item.os.instance_buffer.get_mut().cpu_write().update(metal_cx, &draw_item.instances.as_ref().unwrap());
                }
                
                if draw_call.uniforms_dirty {
                    draw_call.uniforms_dirty = false;
                }
                
                // lets verify our instance_offset is not disaligned
                let instances = (draw_item.instances.as_ref().unwrap().len() / sh.mapping.instances.total_slots) as u64;
                
                if instances == 0 {
                    continue;
                }
                let render_pipeline_state = shp.render_pipeline_state.as_id();
                unsafe {let () = msg_send![encoder, setRenderPipelineState: render_pipeline_state];}
                
                let geometry_id = if let Some(geometry_id) = draw_call.geometry_id {geometry_id}
                else {
                    continue;
                };
                
                let geometry = &mut self.geometries[geometry_id];
                
                if geometry.dirty {
                    geometry.os.index_buffer.next();
                    geometry.os.index_buffer.get_mut().cpu_write().update(metal_cx, &geometry.indices);
                    geometry.os.vertex_buffer.next();
                    geometry.os.vertex_buffer.get_mut().cpu_write().update(metal_cx, &geometry.vertices);
                    geometry.dirty = false;
                }
                
                if let Some(inner) = geometry.os.vertex_buffer.get().cpu_read().inner.as_ref() {
                    unsafe {msg_send![
                        encoder,
                        setVertexBuffer: inner.buffer.as_id()
                        offset: 0
                        atIndex: 0
                    ]}
                }
                else {crate::error!("Drawing error: vertex_buffer None")}
                
                if let Some(inner) = draw_item.os.instance_buffer.get().cpu_read().inner.as_ref() {
                    unsafe {msg_send![
                        encoder,
                        setVertexBuffer: inner.buffer.as_id()
                        offset: 0
                        atIndex: 1
                    ]}
                }
                else {crate::error!("Drawing error: instance_buffer None")}
                
                let pass_uniforms = self.passes[pass_id].pass_uniforms.as_slice();
                let draw_list_uniforms = draw_list.draw_list_uniforms.as_slice();
                let draw_call_uniforms = draw_call.draw_call_uniforms.as_slice();
                
                unsafe {
                    
                    let () = msg_send![encoder, setVertexBytes: sh.mapping.live_uniforms_buf.as_ptr() as *const std::ffi::c_void length: (sh.mapping.live_uniforms_buf.len() * 4) as u64 atIndex: 2u64];
                    
                    let () = msg_send![encoder, setFragmentBytes: sh.mapping.live_uniforms_buf.as_ptr() as *const std::ffi::c_void length: (sh.mapping.live_uniforms_buf.len() * 4) as u64 atIndex: 2u64];
                    
                    if let Some(id) = shp.draw_call_uniform_buffer_id {
                        let () = msg_send![encoder, setVertexBytes: draw_call_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_call_uniforms.len() * 4) as u64 atIndex: id];
                        let () = msg_send![encoder, setFragmentBytes: draw_call_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_call_uniforms.len() * 4) as u64 atIndex: id];
                    }
                    if let Some(id) = shp.pass_uniform_buffer_id {
                        let () = msg_send![encoder, setVertexBytes: pass_uniforms.as_ptr() as *const std::ffi::c_void length: (pass_uniforms.len() * 4) as u64 atIndex: id];
                        let () = msg_send![encoder, setFragmentBytes: pass_uniforms.as_ptr() as *const std::ffi::c_void length: (pass_uniforms.len() * 4) as u64 atIndex: id];
                    }
                    if let Some(id) = shp.draw_list_uniform_buffer_id {
                        let () = msg_send![encoder, setVertexBytes: draw_list_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_list_uniforms.len() * 4) as u64 atIndex: id];
                        let () = msg_send![encoder, setFragmentBytes: draw_list_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_list_uniforms.len() * 4) as u64 atIndex: id];
                    }
                    if let Some(id) = shp.user_uniform_buffer_id {
                        let () = msg_send![encoder, setVertexBytes: draw_call.user_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_call.user_uniforms.len() * 4) as u64 atIndex: id];
                        let () = msg_send![encoder, setFragmentBytes: draw_call.user_uniforms.as_ptr() as *const std::ffi::c_void length: (draw_call.user_uniforms.len() * 4) as u64 atIndex: id];
                    }
                    
                    let ct = &sh.mapping.const_table.table;
                    if ct.len()>0 {
                        let () = msg_send![encoder, setVertexBytes: ct.as_ptr() as *const std::ffi::c_void length: (ct.len() * 4) as u64 atIndex: 3u64];
                        let () = msg_send![encoder, setFragmentBytes: ct.as_ptr() as *const std::ffi::c_void length: (ct.len() * 4) as u64 atIndex: 3u64];
                    }
                }
                // lets set our textures
                for i in 0..sh.mapping.textures.len() {
                    
                    let texture_id = if let Some(texture) = &draw_call.texture_slots[i] {
                        texture.texture_id()
                    }else {
                        let () = unsafe {msg_send![
                            encoder,
                            setFragmentTexture: nil
                            atIndex: i as u64
                        ]};
                        let () = unsafe {msg_send![
                            encoder,
                            setVertexTexture: nil
                            atIndex: i as u64
                        ]};
                        continue
                    };
                    
                    let cxtexture = &mut self.textures[texture_id];
                    
                    if cxtexture.format.is_shared() {
                        #[cfg(target_os = "macos")]
                        cxtexture.update_shared_texture(
                            metal_cx.device,
                        );
                    }
                    else if cxtexture.format.is_vec(){
                        cxtexture.update_vec_texture(metal_cx, self.frame_trace.as_deref());
                    }
                    
                    if let Some(texture) = cxtexture.os.texture.as_ref() {
                        let () = unsafe {msg_send![
                            encoder,
                            setFragmentTexture: texture.as_id()
                            atIndex: i as u64
                        ]};
                        let () = unsafe {msg_send![
                            encoder,
                            setVertexTexture: texture.as_id()
                            atIndex: i as u64
                        ]};
                    }
                    
                }
                self.os.draw_calls_done += 1;
                if let Some(inner) = geometry.os.index_buffer.get().cpu_read().inner.as_ref() {
                    
                    let () = unsafe {msg_send![
                        encoder,
                        drawIndexedPrimitives: MTLPrimitiveType::Triangle
                        indexCount: geometry.indices.len() as u64
                        indexType: MTLIndexType::UInt32
                        indexBuffer: inner.buffer.as_id()
                        indexBufferOffset: 0
                        instanceCount: instances
                    ]};
                }
                else {crate::error!("Drawing error: index_buffer None")}
                
                gpu_read_guards.push(draw_item.os.instance_buffer.get().gpu_read());
                gpu_read_guards.push(geometry.os.vertex_buffer.get().gpu_read());
                gpu_read_guards.push(geometry.os.index_buffer.get().gpu_read());
            }
        }
    }
    
    pub fn draw_pass(
        &mut self,
        pass_id: PassId,
        metal_cx: &mut MetalCx,
        mode: DrawPassMode,
    ) {
        self.os.draw_calls_done  = 0;
        let draw_list_id = if let Some(draw_list_id) = self.passes[pass_id].main_draw_list_id{
            draw_list_id
        }
        else{
            crate::error!("Draw pass has no draw list!");
            return
        };
        
        let pool: ObjcId = unsafe {msg_send![class!(NSAutoreleasePool), new]};
        
        let render_pass_descriptor: ObjcId = if let DrawPassMode::MTKView(view) = mode {
            unsafe{msg_send![view, currentRenderPassDescriptor]}
        }
        else{
            unsafe {msg_send![class!(MTLRenderPassDescriptorInternal), renderPassDescriptor]}
        };
        
        let dpi_factor = self.passes[pass_id].dpi_factor.unwrap();
        
        let pass_rect = self.get_pass_rect(pass_id, if mode.is_drawable().is_some() {1.0}else {dpi_factor}).unwrap();
        
        self.passes[pass_id].set_ortho_matrix(
            pass_rect.pos, 
            pass_rect.size
        );
        
        self.passes[pass_id].paint_dirty = false;

        if pass_rect.size.x <0.5 || pass_rect.size.y < 0.5 {
            return
        }
        
        self.passes[pass_id].set_dpi_factor(dpi_factor);
        
        if let DrawPassMode::MTKView(_) = mode{
            let color_attachments:ObjcId = unsafe{msg_send![render_pass_descriptor, colorAttachments]};
            let color_attachment:ObjcId = unsafe{msg_send![color_attachments, objectAtIndexedSubscript: 0]};
            let color = self.passes[pass_id].clear_color;
            unsafe {
                let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Clear];
                let () = msg_send![color_attachment, setClearColor: MTLClearColor {
                    red: color.x as f64,
                    green: color.y as f64,
                    blue: color.z as f64,
                    alpha: color.w as f64
                }];
            }
        } 
        else if let Some(drawable) = mode.is_drawable() {
            let first_texture: ObjcId = unsafe {msg_send![drawable, texture]};
            let color_attachments: ObjcId = unsafe {msg_send![render_pass_descriptor, colorAttachments]};
            let color_attachment: ObjcId = unsafe {msg_send![color_attachments, objectAtIndexedSubscript: 0]};
            
            let () = unsafe {msg_send![
                color_attachment,
                setTexture: first_texture
            ]};
            let color = self.passes[pass_id].clear_color;
            unsafe {
                let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Clear];
                let () = msg_send![color_attachment, setClearColor: MTLClearColor {
                    red: color.x as f64,
                    green: color.y as f64,
                    blue: color.z as f64,
                    alpha: color.w as f64
                }];
            }
        }
        else {
            for (index, color_texture) in self.passes[pass_id].color_textures.iter().enumerate() {
                let color_attachments: ObjcId = unsafe {msg_send![render_pass_descriptor, colorAttachments]};
                let color_attachment: ObjcId = unsafe {msg_send![color_attachments, objectAtIndexedSubscript: index as u64]};
                
                let cxtexture = &mut self.textures[color_texture.texture.texture_id()];
                let size = dpi_factor * pass_rect.size; 
                cxtexture.update_render_target(metal_cx, size.x as usize, size.y as usize);
                #[cfg(target_os = "macos")]
                cxtexture.mark_content_changed();
                
                let is_initial = cxtexture.take_initial();
                
                if let Some(texture) = cxtexture.os.texture.as_ref() {
                    let () = unsafe {msg_send![
                        color_attachment,
                        setTexture: texture.as_id()
                    ]};
                }
                else {
                    crate::error!("draw_pass_to_texture invalid render target");
                }
                
                unsafe {msg_send![color_attachment, setStoreAction: MTLStoreAction::Store]}
                match color_texture.clear_color {
                    PassClearColor::InitWith(color) => {
                        if is_initial {
                            unsafe {
                                let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Clear];
                                let () = msg_send![color_attachment, setClearColor: MTLClearColor {
                                    red: color.x as f64,
                                    green: color.y as f64,
                                    blue: color.z as f64,
                                    alpha: color.w as f64
                                }];
                            }
                        }
                        else {
                            unsafe {let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Load];}
                        }
                    },
                    PassClearColor::ClearWith(color) => {
                        unsafe {
                            let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Clear];
                            let () = msg_send![color_attachment, setClearColor: MTLClearColor {
                                red: color.x as f64,
                                green: color.y as f64,
                                blue: color.z as f64,
                                alpha: color.w as f64
                            }];
                        }
                    }
                }
            }
        }
        // attach depth texture
        if let Some(depth_texture) = &self.passes[pass_id].depth_texture {
            let cxtexture = &mut self.textures[depth_texture.texture_id()];
            let size = dpi_factor * pass_rect.size;
            cxtexture.update_depth_stencil(metal_cx, size.x as usize, size.y as usize);
            #[cfg(target_os = "macos")]
            cxtexture.mark_content_changed();
            let is_initial = cxtexture.take_initial();
            
            let depth_attachment: ObjcId = unsafe {msg_send![render_pass_descriptor, depthAttachment]};
            
            if let Some(texture) = cxtexture.os.texture.as_ref() {
                unsafe {msg_send![depth_attachment, setTexture: texture.as_id()]}
            }
            else {
                crate::error!("draw_pass_to_texture invalid render target");
            }
            let () = unsafe {msg_send![depth_attachment, setStoreAction: MTLStoreAction::Store]};
            
            match self.passes[pass_id].clear_depth {
                PassClearDepth::InitWith(depth) => {
                    if is_initial {
                        let () = unsafe {msg_send![depth_attachment, setLoadAction: MTLLoadAction::Clear]};
                        let () = unsafe {msg_send![depth_attachment, setClearDepth: depth as f64]};
                    }
                    else {
                        let () = unsafe {msg_send![depth_attachment, setLoadAction: MTLLoadAction::Load]};
                    }
                },
                PassClearDepth::ClearWith(depth) => {
                    let () = unsafe {msg_send![depth_attachment, setLoadAction: MTLLoadAction::Clear]};
                    let () = unsafe {msg_send![depth_attachment, setClearDepth: depth as f64]};
                }
            }
            // create depth state
            self.metal_depth_state(pass_id, metal_cx);
        }
        
        let command_buffer: ObjcId = unsafe {msg_send![metal_cx.command_queue, commandBuffer]};
        let encoder: ObjcId = unsafe {msg_send![command_buffer, renderCommandEncoderWithDescriptor: render_pass_descriptor]};
        
        if let Some(depth_state) = self.passes[pass_id].os.mtl_depth_state {
            let () = unsafe {msg_send![encoder, setDepthStencilState: depth_state]};
        }
        
        let pass_width = dpi_factor * pass_rect.size.x;
        let pass_height = dpi_factor * pass_rect.size.y;
        
        set_metal_viewport(encoder, pass_width, pass_height);
        
        let mut gpu_read_guards = Vec::new();
        let mut draw = MetalDraw {
            encoder, metal_cx, zbias: 0.0, zbias_step: self.passes[pass_id].zbias_step,
            gpu_read_guards: &mut gpu_read_guards,
            #[cfg(target_os = "macos")]
            damage: None,
        };
        self.render_view(pass_id, draw_list_id, &mut draw);
        
        let () = unsafe {msg_send![encoder, endEncoding]};
        
        
        match mode {
            DrawPassMode::MTKView(view)=>{
                let drawable:ObjcId = unsafe {msg_send![view, currentDrawable]};
                let first_texture: ObjcId = unsafe {msg_send![drawable, texture]};
                let () = unsafe {msg_send![command_buffer, presentDrawable: drawable]};
                let screenshot = self.build_screenshot_struct(metal_cx, command_buffer, 0, pass_width as usize, pass_height as usize, first_texture, None);
                self.commit_command_buffer(screenshot, None, command_buffer, gpu_read_guards);
            }
            DrawPassMode::Texture => {
                self.commit_command_buffer(None, None, command_buffer, gpu_read_guards);
            }
            DrawPassMode::StdinMain(stdin_frame, kind_id) => {
                let main_texture = &self.passes[pass_id].color_textures[0];
                let tex = &self.textures[main_texture.texture.texture_id()];
                let screenshot = if let Some(texture) = &tex.os.texture{
                    self.build_screenshot_struct(metal_cx, command_buffer, kind_id, pass_width as usize, pass_height as usize, texture.as_id(), tex.alloc.clone())
                }
                else{
                    None
                };
                self.commit_command_buffer(screenshot, Some(stdin_frame), command_buffer, gpu_read_guards);
            }
            DrawPassMode::Drawable(drawable) => {
                let first_texture: ObjcId = unsafe {msg_send![drawable, texture]};
                let () = unsafe {msg_send![command_buffer, presentDrawable: drawable]};
                let screenshot = self.build_screenshot_struct(metal_cx, command_buffer, 0, pass_width as usize, pass_height as usize, first_texture, None);
                self.commit_command_buffer(screenshot, None, command_buffer, gpu_read_guards);
            }
            DrawPassMode::Resizing(drawable) => {
                let first_texture: ObjcId = unsafe {msg_send![drawable, texture]};
                let screenshot = self.build_screenshot_struct(metal_cx, command_buffer, 0, pass_width as usize, pass_height as usize, first_texture, None);
                self.commit_command_buffer(screenshot, None, command_buffer, gpu_read_guards);
                let () = unsafe {msg_send![command_buffer, waitUntilScheduled]};
                let () = unsafe {msg_send![drawable, present]};
            }
        }
        let () = unsafe {msg_send![pool, release]};
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn surface_viewport(&self, pass_id: PassId, size: [i32; 2]) -> Option<crate::makepad_math::Rect> {
        let rect = self.get_pass_rect(pass_id, 1.0)?;
        let dpi = self.passes[pass_id].dpi_factor?;
        ((rect.size.x * dpi - f64::from(size[0])).abs() <= 1e-6
            && (rect.size.y * dpi - f64::from(size[1])).abs() <= 1e-6).then_some(rect)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn draw_surface_pass(
        &mut self, pass_id: PassId, metal_cx: &MetalCx, presenter: &mut SurfacePresenter,
        slot: usize, size: [i32; 2], pass_rect: crate::makepad_math::Rect,
    ) -> Result<(), &'static str> {
        let pass = &mut self.passes[pass_id];
        let Some(root) = pass.main_draw_list_id else { return Err("missing Metal window draw list"); };
        let Some(dpi) = pass.dpi_factor else { return Err("missing Metal window scale"); };
        let (Some(depth_texture), &PassClearDepth::ClearWith(depth)) = (&pass.depth_texture, &pass.clear_depth) else {
            return Err("window depth must be cleared each frame");
        };
        if !(0.0..=1.0).contains(&depth) { return Err("window depth clear value out of range"); }
        let depth_id = depth_texture.texture_id();
        let depth_texture = &mut self.textures[depth_id];
        if !depth_texture.update_depth_stencil(metal_cx, size[0] as usize, size[1] as usize)
            || !depth_texture.alloc.as_ref().is_some_and(|alloc| {
                matches!(alloc.pixel, TexturePixel::D32) && [alloc.width, alloc.height] == size.map(|v| v as usize)
            })
        {
            return Err("window Depth32Float attachment unavailable");
        }
        let initial_depth = depth_texture.take_initial();
        let depth_target = depth_texture.os.texture.as_ref().unwrap().as_id();
        let depth_valid = !initial_depth
            && presenter.depth_baseline() == Some((depth_id, depth_texture.visual_revision));
        depth_texture.mark_content_changed();
        let depth_baseline = (depth_id, depth_texture.visual_revision);
        let color = pass.clear_color;
        pass.set_ortho_matrix(pass_rect.pos, pass_rect.size);
        pass.set_dpi_factor(dpi);
        let full = force_full_redraw();
        let logical = if full {
            DamageRect::full(size)
        } else {
            presenter.scene_mut().collect(self, pass_id, root, size, [
                color.x.to_bits(), color.y.to_bits(), color.z.to_bits(), color.w.to_bits(), depth.to_bits(),
            ])
        };
        let screenshot_requested = self.screenshot_requests.iter().any(|request| request.kind_id == 0);
        let Some(mut paint) = presenter.prepare(
            slot, logical, depth_baseline, full || !depth_valid || screenshot_requested, self.frame_trace.is_some(),
        ) else {
            return Err("Metal scissored clear or oracle allocation failed");
        };
        let command: ObjcId = unsafe { msg_send![metal_cx.command_queue, commandBuffer] };
        if command == nil { return Err("Metal command buffer unavailable"); }
        let desc: ObjcId = unsafe { msg_send![class!(MTLRenderPassDescriptor), renderPassDescriptor] };
        let colors: ObjcId = unsafe { msg_send![desc, colorAttachments] };
        let color_attachment: ObjcId = unsafe { msg_send![colors, objectAtIndexedSubscript: 0usize] };
        let depth_attachment: ObjcId = unsafe { msg_send![desc, depthAttachment] };
        let load = if paint.repair.is_some() { MTLLoadAction::Load } else { MTLLoadAction::Clear };
        let depth_state = self.metal_depth_state(pass_id, metal_cx);
        if depth_state == nil { return Err("Metal depth state unavailable"); }
        unsafe {
            let () = msg_send![color_attachment, setTexture: paint.texture];
            let () = msg_send![color_attachment, setLoadAction: load];
            let () = msg_send![color_attachment, setStoreAction: MTLStoreAction::Store];
            let () = msg_send![color_attachment, setClearColor: MTLClearColor {
                red: color.x as f64, green: color.y as f64, blue: color.z as f64, alpha: color.w as f64
            }];
            let () = msg_send![depth_attachment, setTexture: depth_target];
            let () = msg_send![depth_attachment, setLoadAction: load];
            let () = msg_send![depth_attachment, setStoreAction: MTLStoreAction::Store];
            let () = msg_send![depth_attachment, setClearDepth: depth as f64];
        }
        let encoder: ObjcId = unsafe { msg_send![command, renderCommandEncoderWithDescriptor: desc] };
        if encoder == nil { return Err("Metal render encoder unavailable"); }
        set_metal_viewport(encoder, f64::from(size[0]), f64::from(size[1]));
        // Color is retained per surface. Depth belongs to the window pass and is
        // current outside repair; inside it replay must start from clear depth,
        // not from this pass's previous writes (which could reject moved/removed geometry).
        if let Some((_, rect, clear)) = paint.repair {
            clear.encode(encoder, rect, size[1], color, depth);
        }
        let () = unsafe { msg_send![encoder, setDepthStencilState: depth_state] };
        self.os.draw_calls_done = 0;
        let mut draw = MetalDraw {
            encoder, metal_cx, zbias: 0.0, zbias_step: self.passes[pass_id].zbias_step,
            gpu_read_guards: &mut paint.guards,
            damage: paint.repair.map(|(frame, rect, _)| (frame, rect, 0)),
        };
        self.render_view(pass_id, root, &mut draw);
        let () = unsafe { msg_send![encoder, endEncoding] };
        if let Some(oracle) = paint.oracle {
            // Replay exactly this scene, with the same uniforms and offscreen results,
            // into separate full-clear attachments on the very same command buffer.
            unsafe {
                let () = msg_send![color_attachment, setTexture: oracle.color.as_id()];
                let () = msg_send![color_attachment, setLoadAction: MTLLoadAction::Clear];
                let () = msg_send![depth_attachment, setTexture: oracle.depth.as_id()];
                let () = msg_send![depth_attachment, setLoadAction: MTLLoadAction::Clear];
                let () = msg_send![depth_attachment, setStoreAction: MTLStoreAction::DontCare];
            }
            let encoder: ObjcId = unsafe { msg_send![command, renderCommandEncoderWithDescriptor: desc] };
            if encoder == nil { return Err("Metal oracle encoder unavailable"); }
            set_metal_viewport(encoder, f64::from(size[0]), f64::from(size[1]));
            let () = unsafe { msg_send![encoder, setDepthStencilState: depth_state] };
            draw.encoder = encoder;
            draw.damage = None;
            draw.zbias = 0.0;
            self.render_view(pass_id, root, &mut draw);
            let () = unsafe { msg_send![encoder, endEncoding] };
            if !oracle.encode_readback(command, paint.texture, size) { return Err("Metal oracle blit unavailable"); }
        }
        let screenshot = if screenshot_requested {
            self.build_screenshot_struct(metal_cx, command, 0, size[0] as usize, size[1] as usize, paint.texture, None)
        } else {
            None
        };
        self.passes[pass_id].paint_dirty = false;
        self.commit_metal_command(screenshot, None, command, paint.guards, Some(paint.completion));
        Ok(())
    }

    fn metal_depth_state(&mut self, pass_id: PassId, metal_cx: &MetalCx) -> ObjcId {
        *self.passes[pass_id].os.mtl_depth_state.get_or_insert_with(|| unsafe {
            let desc: ObjcId = msg_send![class!(MTLDepthStencilDescriptor), new];
            let () = msg_send![desc, setDepthCompareFunction: MTLCompareFunction::LessEqual];
            let () = msg_send![desc, setDepthWriteEnabled: YES];
            let state = msg_send![metal_cx.device, newDepthStencilStateWithDescriptor: desc];
            let () = msg_send![desc, release];
            state
        })
    }
    
    fn build_screenshot_struct(&mut self, metal_cx:&MetalCx, command_buffer:ObjcId, kind_id: usize, width: usize, height: usize, in_texture:ObjcId, alloc:Option<TextureAlloc>)->Option<ScreenshotInfo>{
        let mut request_ids = Vec::new();
        self.screenshot_requests.retain(|v|{
            if v.kind_id == kind_id as u32{
                request_ids.push(v.request_id);
                false
            }
            else{
                true
            }
        });
        let (tex_width,tex_height) = if let Some(alloc) = alloc{
            (alloc.width, alloc.height)
        }else{
            (width, height)
        };
        if request_ids.len() > 0{
            let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
                msg_send![class!(MTLTextureDescriptor), new]
            }).unwrap());
            let _: () = unsafe {msg_send![descriptor.as_id(), setTextureType: MTLTextureType::D2]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setDepth: 1u64]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setStorageMode: MTLStorageMode::Shared]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setUsage: MTLTextureUsage::ShaderRead]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setWidth: tex_width as u64]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setHeight: tex_height as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setPixelFormat: MTLPixelFormat::BGRA8Unorm]};
            let texture:ObjcId = unsafe{msg_send![metal_cx.device, newTextureWithDescriptor: descriptor]};
            unsafe{
                let blit_encoder: ObjcId = msg_send![command_buffer, blitCommandEncoder];
                let () = msg_send![blit_encoder, copyFromTexture: in_texture toTexture:texture];
                let () = msg_send![blit_encoder, synchronizeTexture: texture slice:0 level:0];
                let () = msg_send![blit_encoder, endEncoding];
            };
            return Some(ScreenshotInfo{
                request_ids,
                width: width as _, 
                height: height as _,
                texture: texture
            })
        }
        None
    }
        
    fn commit_command_buffer(&self, screenshot_info: Option<ScreenshotInfo>, stdin_frame: Option<PresentableDraw>, command_buffer: ObjcId, gpu_read_guards: Vec<MetalRwLockGpuReadGuard>) {
        self.commit_metal_command(screenshot_info, stdin_frame, command_buffer, gpu_read_guards,
            #[cfg(target_os = "macos")]
            None,
        );
    }

    fn commit_metal_command(
        &self,
        screenshot_info: Option<ScreenshotInfo>,
        stdin_frame: Option<PresentableDraw>,
        command_buffer: ObjcId,
        gpu_read_guards: Vec<MetalRwLockGpuReadGuard>,
        #[cfg(target_os = "macos")] surface_completion: Option<Arc<SurfaceCompletion>>,
    ) {
        let gpu_read_guards = Mutex::new(Some(gpu_read_guards));
        let screenshot_info =  Mutex::new(screenshot_info);
        //let present_index = Arc::clone(&self.os.present_index);
        //Self::stdin_send_draw_complete(&present_index);
        let start_time = self.os.start_time.unwrap();
        let () = unsafe {msg_send![
            command_buffer,
            addCompletedHandler: &objc_block!(move | command_buffer: ObjcId | {
                #[cfg(target_os = "macos")]
                let completed = surface_completion.as_ref().is_none_or(|_| {
                    // MTLCommandBufferStatusCompleted (4) is the only successful terminal state.
                    let status: usize = unsafe { msg_send![command_buffer, status] };
                    status == 4
                });
                #[cfg(not(target_os = "macos"))]
                let completed = true;
                // alright lets grab a texture if need be
                if let Some(sf) = &*screenshot_info.lock().unwrap(){
                    let image = completed.then(|| {
                        let mut buf = vec![0u8; sf.width * sf.height * 4];
                        let region = MTLRegion {
                            origin: MTLOrigin { x: 0, y: 0, z: 0 },
                            size: MTLSize { width: sf.width as u64, height: sf.height as u64, depth: 1 },
                        };
                        let () = unsafe { msg_send![
                            sf.texture, getBytes: buf.as_mut_ptr()
                            bytesPerRow: sf.width * 4 bytesPerImage: sf.width * sf.height * 4
                            fromRegion: region mipmapLevel: 0 slice: 0
                        ] };
                        buf
                    });
                    let () = msg_send![sf.texture, release];
                    Self::send_studio_message(AppToStudio::Screenshot(StudioScreenshotResponse{
                        request_ids: sf.request_ids.clone(),
                        image,
                        width: sf.width as _, 
                        height: sf.height as _,
                    }))
                }
                
                let start:f64 = unsafe {msg_send![command_buffer, GPUStartTime]};
                let end:f64 = unsafe {msg_send![command_buffer, GPUEndTime]};
                if let Some(_stdin_frame) = stdin_frame {
                    #[cfg(target_os = "macos")]
                    Self::stdin_send_draw_complete(_stdin_frame);
                }
                // lets send off our gpu time
                let duration = end - start;
                let start = Instant::now().duration_since(start_time).as_secs_f64() - duration;
                let end = start + duration;
                Cx::send_studio_message(AppToStudio::GPUSample(GPUSample{
                    start, end
                }));
                
                let guards = gpu_read_guards.lock().unwrap().take().unwrap();
                #[cfg(target_os = "macos")]
                if let Some(completion) = &surface_completion {
                    completion.complete(completed, guards);
                    return;
                }
                drop(guards);
            })
        ]};
        let () = unsafe {msg_send![command_buffer, commit]};
    } 
    
    pub (crate) fn mtl_compile_shaders(&mut self, metal_cx: &MetalCx) {
        for draw_shader_ptr in &self.draw_shaders.compile_set {
            if let Some(item) = self.draw_shaders.ptr_to_item.get(&draw_shader_ptr) {
                let cx_shader = &mut self.draw_shaders.shaders[item.draw_shader_id];
                let draw_shader_def = self.shader_registry.draw_shader_defs.get(&draw_shader_ptr);
                let gen = generate_metal::generate_shader(
                    draw_shader_def.as_ref().unwrap(),
                    &cx_shader.mapping.const_table,
                    &self.shader_registry
                );
                
                if cx_shader.mapping.flags.debug {
                    crate::log!("{}", gen.mtlsl);
                }
                // lets see if we have the shader already
                for (index, ds) in self.draw_shaders.os_shaders.iter().enumerate() {
                    if ds.mtlsl == gen.mtlsl {
                        cx_shader.os_shader_id = Some(index);
                        break;
                    }
                }
                if cx_shader.os_shader_id.is_none() {
                    if let Some(shp) = CxOsDrawShader::new(metal_cx, gen) {
                        cx_shader.os_shader_id = Some(self.draw_shaders.os_shaders.len());
                        self.draw_shaders.os_shaders.push(shp);
                    }
                }
            }
        }
        self.draw_shaders.compile_set.clear();
    }
    
    #[cfg(target_os="macos")]
    pub fn share_texture_for_presentable_image(
        &mut self,
        texture: &Texture,
    ) -> crate::cx_stdin::SharedPresentableImageOsHandle {
        let cxtexture = &mut self.textures[texture.texture_id()];
        cxtexture.update_shared_texture(self.os.metal_device.unwrap());

        // HACK(eddyb) macOS has no real `SharedPresentableImageOsHandle` because
        // the texture is actually shared through an XPC helper service instead,
        // based entirely on its `PresentableImageId`.
        crate::cx_stdin::SharedPresentableImageOsHandle {
            _dummy_for_macos: None,
        }
    }
    
    #[cfg(any(target_os="ios", target_os="tvos"))]
    pub fn share_texture_for_presentable_image(
        &mut self,
        _texture: &Texture,
    ) -> crate::cx_stdin::SharedPresentableImageOsHandle {
        crate::cx_stdin::SharedPresentableImageOsHandle {
            _dummy_for_unsupported: None,
        }
    }
}

fn set_metal_viewport(encoder: ObjcId, width: f64, height: f64) {
    let () = unsafe { msg_send![encoder, setViewport: MTLViewport {
        originX: 0.0, originY: 0.0, width, height, znear: 0.0, zfar: 1.0,
    }] };
}

#[derive(Clone)]
struct ScreenshotInfo{
    width: usize,
    height: usize,
    request_ids: Vec<u64>,
    texture: ObjcId,
}

pub enum DrawPassMode {
    Texture,
    MTKView(ObjcId),
    StdinMain(PresentableDraw, usize),
    Drawable(ObjcId),
    Resizing(ObjcId)
}

impl DrawPassMode {
    fn is_drawable(&self) -> Option<ObjcId> {
        match self {
            Self::Drawable(obj) | Self::Resizing(obj) => Some(*obj),
            Self::StdinMain(_,_) | Self::Texture | Self::MTKView(_) => None
        }
    }
}

pub struct MetalCx {
    pub device: ObjcId,
    command_queue: ObjcId
}


#[derive(Clone, Default)]
pub struct CxOsDrawList {
}

#[derive(Default, Clone)]
pub struct CxOsPass {
    mtl_depth_state: Option<ObjcId>
}

pub enum PackType {
    Packed,
    Unpacked
}
/*
pub struct SlErr {
    _msg: String
}*/

impl MetalCx {
    
    pub(crate) fn new(info: &mut crate::gpu_info::GpuInfo) -> MetalCx {
        let device = get_default_metal_device().expect("Cannot get default metal device");
        let name: ObjcId = unsafe { msg_send![device, name] };
        info.record_native_renderer(crate::gpu_info::GpuRenderer {
            api: "Metal",
            renderer: nsstring_to_string(name),
            ..Default::default()
        });
        MetalCx {
            command_queue: unsafe {msg_send![device, newCommandQueue]},
            device: device
        }
    }
}

/**************************************************************************************************/

pub struct CxOsDrawShader {
    _library: RcObjcId,
    render_pipeline_state: RcObjcId,
    draw_call_uniform_buffer_id: Option<u64>,
    pass_uniform_buffer_id: Option<u64>,
    draw_list_uniform_buffer_id: Option<u64>,
    user_uniform_buffer_id: Option<u64>,
    mtlsl: String,
}

impl CxOsDrawShader {
    pub (crate) fn new(
        metal_cx: &MetalCx,
        shader: MetalGeneratedShader,
    ) -> Option<Self> {
        let options = RcObjcId::from_owned(unsafe {msg_send![class!(MTLCompileOptions), new]});
        unsafe {
            let _: () = msg_send![options.as_id(), setFastMathEnabled: YES];
        };
        
        let mut error: ObjcId = nil;
        //std::env::set_var("MTL_IGNORE_WARNINGS","-W");
        let library = RcObjcId::from_owned(match NonNull::new(unsafe {
            msg_send![
                metal_cx.device,
                newLibraryWithSource: str_to_nsstring(&shader.mtlsl)
                options: options
                error: &mut error
            ]
        }) {
            Some(library) => library,
            None => {
                let description: ObjcId = unsafe {msg_send![error, localizedDescription]};
                let string = nsstring_to_string(description);
                let mut out = format!("{}\n", string);
                for (index, line) in shader.mtlsl.split("\n").enumerate() {
                    out.push_str(&format!("{}: {}\n", index + 1, line));
                }
                crate::error!("{}", out);
                panic!("{}", string);
            }
        });
        
        let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
            msg_send![class!(MTLRenderPipelineDescriptor), new]
        }).unwrap());
        
        let vertex_function = RcObjcId::from_owned(NonNull::new(unsafe {
            msg_send![library.as_id(), newFunctionWithName: str_to_nsstring("vertex_main")]
        }).unwrap());
        
        let fragment_function = RcObjcId::from_owned(NonNull::new(unsafe {
            msg_send![library.as_id(), newFunctionWithName: str_to_nsstring("fragment_main")]
        }).unwrap());
        
        let render_pipeline_state = RcObjcId::from_owned(NonNull::new(unsafe {
            let _: () = msg_send![descriptor.as_id(), setVertexFunction: vertex_function];
            let _: () = msg_send![descriptor.as_id(), setFragmentFunction: fragment_function];
            
            let color_attachments: ObjcId = msg_send![descriptor.as_id(), colorAttachments];
            let color_attachment: ObjcId = msg_send![color_attachments, objectAtIndexedSubscript: 0];
            let () = msg_send![color_attachment, setPixelFormat: MTLPixelFormat::BGRA8Unorm];
            let () = msg_send![color_attachment, setBlendingEnabled: YES];
            let () = msg_send![color_attachment, setRgbBlendOperation: MTLBlendOperation::Add];
            let () = msg_send![color_attachment, setAlphaBlendOperation: MTLBlendOperation::Add];
            let () = msg_send![color_attachment, setSourceRGBBlendFactor: MTLBlendFactor::One];
            let () = msg_send![color_attachment, setSourceAlphaBlendFactor: MTLBlendFactor::One];
            let () = msg_send![color_attachment, setDestinationRGBBlendFactor: MTLBlendFactor::OneMinusSourceAlpha];
            let () = msg_send![color_attachment, setDestinationAlphaBlendFactor: MTLBlendFactor::OneMinusSourceAlpha];
            
            let () = msg_send![descriptor.as_id(), setDepthAttachmentPixelFormat: MTLPixelFormat::Depth32Float];
            
            let mut error: ObjcId = nil;
            msg_send![
                metal_cx.device,
                newRenderPipelineStateWithDescriptor: descriptor
                error: &mut error
            ]
        }).unwrap());
        
        let mut draw_call_uniform_buffer_id = None;
        let mut pass_uniform_buffer_id = None;
        let mut draw_list_uniform_buffer_id = None;
        let mut user_uniform_buffer_id = None;
        
        let mut buffer_id = 4;
        for (field, _) in shader.fields_as_uniform_blocks {
            match field.0 {
                live_id!(draw_list) => draw_list_uniform_buffer_id = Some(buffer_id),
                live_id!(draw_call) => draw_call_uniform_buffer_id = Some(buffer_id),
                live_id!(pass) => pass_uniform_buffer_id = Some(buffer_id),
                live_id!(user) => user_uniform_buffer_id = Some(buffer_id),
                _ => panic!()
            }
            buffer_id += 1;
        }
        
        return Some(Self {
            _library: library,
            render_pipeline_state,
            draw_call_uniform_buffer_id,
            pass_uniform_buffer_id,
            draw_list_uniform_buffer_id,
            user_uniform_buffer_id,
            mtlsl: shader.mtlsl
        });
    }
}

#[derive(Default)]
pub struct CxOsDrawCall {
    //pub uni_dr: MetalBuffer,
    instance_buffer: MetalBufferQueue,
}

#[derive(Default)]
pub struct CxOsGeometry {
    vertex_buffer: MetalBufferQueue,
    index_buffer: MetalBufferQueue,
}

#[derive(Default)]
struct MetalBufferQueue {
    queue: [MetalRwLock<MetalBuffer>; 3],
    index: usize,
}

impl MetalBufferQueue {
    fn get(&self) -> &MetalRwLock<MetalBuffer> {
        &self.queue[self.index]
    }
    
    fn get_mut(&mut self) -> &mut MetalRwLock<MetalBuffer> {
        &mut self.queue[self.index]
    }
    
    fn next(&mut self) {
        self.index = (self.index + 1) % self.queue.len();
    }
}

#[derive(Default)]
struct MetalBuffer {
    inner: Option<MetalBufferInner>,
}

impl MetalBuffer {
    fn update<T>(&mut self, metal_cx: &MetalCx, data: &[T]) where T: std::fmt::Debug {
        let len = data.len() * std::mem::size_of::<T>();
        if len == 0 {
            self.inner = None;
            return;
        }
        if self.inner.as_ref().map_or(0, | inner | inner.len) < len {
            self.inner = Some(MetalBufferInner {
                len,
                buffer: RcObjcId::from_owned(NonNull::new(unsafe {
                    msg_send![
                        metal_cx.device,
                        newBufferWithLength: len as u64
                        options: nil
                    ]
                }).unwrap())
            });
        }
        let inner = self.inner.as_ref().unwrap();
        unsafe {
            let contents: *mut u8 = msg_send![inner.buffer.as_id(), contents];
            
            //println!("Buffer write {} buf {} data {:?}", command_buffer as *const _ as u64, inner.buffer.as_id() as *const _ as u64, data);
            
            std::ptr::copy(data.as_ptr() as *const u8, contents, len);
            /*
            let _: () = msg_send![
                inner.buffer.as_id(),
                didModifyRange: NSRange {
                    location: 0,
                    length: len as u64
                }
            ];*/
        }
    }
}

struct MetalBufferInner {
    len: usize,
    buffer: RcObjcId,
}

#[derive(Default)]
pub struct CxOsTexture {
    texture: Option<RcObjcId>
}
fn texture_pixel_to_mtl_pixel(pix:&TexturePixel)-> MTLPixelFormat {
     match pix{
         TexturePixel::BGRAu8 => MTLPixelFormat::BGRA8Unorm,
         TexturePixel::RGBAf16 => MTLPixelFormat::RGBA16Float,
         TexturePixel::RGBAf32 => MTLPixelFormat::RGBA32Float,
         TexturePixel::Ru8  => MTLPixelFormat::R8Unorm,
         TexturePixel::RGu8  => MTLPixelFormat::RG8Unorm,
         TexturePixel::Rf32  => MTLPixelFormat::R32Float,
         TexturePixel::D32 => MTLPixelFormat::Depth32Float,
     }   
}
impl CxTexture {
    /*
    pub fn copy_to_system_ram(
        &self,
        _metal_cx: &MetalCx
    )->Option<Vec<u8>>{
        if let Some(alloc) = &self.alloc{
            if let Some(texture) = &self.os.texture{
                let mut buf = Vec::new();
                buf.resize(alloc.width * alloc.height * 4, 0u8);
                let region = MTLRegion {
                    origin: MTLOrigin {x: 0, y: 0, z: 0},
                    size: MTLSize {width: alloc.width as u64, height: alloc.height as u64, depth: 1}
                };
                let _:() = unsafe{msg_send![
                    texture.as_id(), 
                    getBytes: buf.as_ptr()
                    bytesPerRow: alloc.width *4
                    bytesPerImage: alloc.width * alloc.height * 4
                    fromRegion: region
                    mipmapLevel: 0
                    slice: 0
                ]};
                return Some(buf);
            }
        }
        None
    }*/
    
    fn update_vec_texture(
        &mut self,
        metal_cx: &MetalCx,
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
            "mipmapped vector textures are not supported on Metal");
        if reallocate {
            let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
                msg_send![class!(MTLTextureDescriptor), new]
            }).unwrap());
            let _: () = unsafe {msg_send![descriptor.as_id(), setTextureType: MTLTextureType::D2]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setDepth: 1u64]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setStorageMode: MTLStorageMode::Shared]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setUsage: MTLTextureUsage::ShaderRead]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setWidth: alloc.width as u64]};
            let _: () = unsafe {msg_send![descriptor.as_id(), setHeight: alloc.height as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setPixelFormat: texture_pixel_to_mtl_pixel(&alloc.pixel)]};
            let texture:ObjcId = unsafe{msg_send![metal_cx.device, newTextureWithDescriptor: descriptor]};
            self.os.texture = Some(RcObjcId::from_owned(NonNull::new(texture).unwrap()));
            self.alloc = Some(alloc);
        }

        let rect = upload.rect;
        let region = MTLRegion {
            origin: MTLOrigin { x: rect.origin.x as u64, y: rect.origin.y as u64, z: 0 },
            size: MTLSize { width: rect.size.width as u64, height: rect.size.height as u64, depth: 1 },
        };
        let _: () = unsafe { msg_send![
            self.os.texture.as_ref().unwrap().as_id(),
            replaceRegion: region
            mipmapLevel: 0
            withBytes: upload.bytes.as_ptr()
            bytesPerRow: upload.row_stride as u64
        ] };
        if let Some(trace) = trace {
            trace.texture_upload(rect.size.width, rect.size.height, upload.bytes_per_pixel);
        }
        self.take_updated();
    }
    
    #[cfg(target_os = "macos")]
    fn update_shared_texture(
        &mut self,
        metal_device: ObjcId,
    ) {
        // we need a width/height for this one.
        if !self.alloc_shared(){
            return
        }
        let alloc = self.alloc.as_ref().unwrap();
        let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
            msg_send![class!(MTLTextureDescriptor), new]
        }).unwrap());
            
        let _: () = unsafe{msg_send![descriptor.as_id(), setTextureType: MTLTextureType::D2]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setWidth: alloc.width as u64]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setHeight: alloc.height as u64]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setDepth: 1u64]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setStorageMode: MTLStorageMode::Private]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setUsage: MTLTextureUsage::RenderTarget]};
        let _: () = unsafe{msg_send![descriptor.as_id(), setPixelFormat: texture_pixel_to_mtl_pixel(&alloc.pixel)]};
        match &self.format {
            TextureFormat::SharedBGRAu8{id, ..} => {
                let texture: ObjcId = unsafe{msg_send![metal_device, newSharedTextureWithDescriptor: descriptor]};
                let shared: ObjcId = unsafe{msg_send![texture, newSharedTextureHandle]};
                store_xpc_service_texture(*id, shared);
                let _: () = unsafe{msg_send![shared, release]};
                self.os.texture = Some(RcObjcId::from_owned(NonNull::new(texture).unwrap()));
            }
            _ => panic!(),
        }
    }
    
    #[cfg(target_os = "macos")]
    pub fn update_from_shared_handle(
        &mut self,
        metal_cx: &MetalCx,
        shared_handle: ObjcId,
    ) -> bool {
        // we need a width/height for this one.
        if !self.alloc_shared(){
            return true
        }
        let alloc = self.alloc.as_ref().unwrap();
    
        let texture = RcObjcId::from_owned(NonNull::new(unsafe {
            msg_send![metal_cx.device, newSharedTextureWithHandle: shared_handle]
        }).unwrap());
        let width: u64 = unsafe{msg_send![texture.as_id(), width]};
        let height: u64 = unsafe{msg_send![texture.as_id(), height]};
        // FIXME(eddyb) can these be an assert now?
        if width != alloc.width as u64|| height != alloc.height as u64{
            return false
        }
        self.os.texture = Some(texture);
        true
    }
    
    fn update_render_target(
        &mut self,
        metal_cx: &MetalCx,
        width: usize,
        height: usize
    ) {
        if self.alloc_render(width, height){
            let alloc = self.alloc.as_ref().unwrap();
            let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
                msg_send![class!(MTLTextureDescriptor), new]
            }).unwrap());
            
            let _: () = unsafe{msg_send![descriptor.as_id(), setTextureType: MTLTextureType::D2]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setWidth: alloc.width as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setHeight: alloc.height as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setDepth: 1u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setStorageMode: MTLStorageMode::Private]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setUsage: MTLTextureUsage::RenderTarget]};
            let _: () = unsafe{msg_send![descriptor.as_id(),setPixelFormat: texture_pixel_to_mtl_pixel(&alloc.pixel)]};
            let texture = RcObjcId::from_owned(NonNull::new(unsafe {
                msg_send![metal_cx.device, newTextureWithDescriptor: descriptor]
            }).unwrap());
            
            self.os.texture = Some(texture); 
        }
    }
    
    
    fn update_depth_stencil(
        &mut self,
        metal_cx: &MetalCx,
        width: usize,
        height: usize
    ) -> bool {
        if self.alloc_depth(width, height){
       
            let alloc = self.alloc.as_ref().unwrap();
            let descriptor = RcObjcId::from_owned(NonNull::new(unsafe {
                msg_send![class!(MTLTextureDescriptor), new]
            }).unwrap());
                        
            let _: () = unsafe{msg_send![descriptor.as_id(), setTextureType: MTLTextureType::D2]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setWidth: alloc.width as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setHeight: alloc.height as u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setDepth: 1u64]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setStorageMode: MTLStorageMode::Private]};
            let _: () = unsafe{msg_send![descriptor.as_id(), setUsage: MTLTextureUsage::RenderTarget]};
            let _: () = unsafe{msg_send![
                descriptor.as_id(),
                setPixelFormat: texture_pixel_to_mtl_pixel(&alloc.pixel)
            ]};
            self.os.texture = NonNull::new(unsafe { msg_send![metal_cx.device, newTextureWithDescriptor: descriptor] })
                .map(RcObjcId::from_owned);
        }
        self.os.texture.is_some()
    }    
}

#[derive(Default)]
struct MetalRwLock<T> {
    inner: Arc<MetalRwLockInner>,
    value: T
}

impl<T> MetalRwLock<T> {
    fn cpu_read(&self) -> &T {
        &self.value
    }
    
    fn gpu_read(&self) -> MetalRwLockGpuReadGuard {
        let mut reader_count = self.inner.reader_count.lock().unwrap();
        *reader_count += 1;
        MetalRwLockGpuReadGuard {
            inner: self.inner.clone()
        }
    }
    
    fn cpu_write(&mut self) -> &mut T {
        let mut reader_count = self.inner.reader_count.lock().unwrap();
        while *reader_count != 0 {
            reader_count = self.inner.condvar.wait(reader_count).unwrap();
        }
        &mut self.value
    }
}

#[derive(Default)]
struct MetalRwLockInner {
    reader_count: Mutex<usize>,
    condvar: Condvar,
}

pub(crate) struct MetalRwLockGpuReadGuard {
    inner: Arc<MetalRwLockInner>
}

impl Drop for MetalRwLockGpuReadGuard {
    fn drop(&mut self) {
        let mut reader_count = self.inner.reader_count.lock().unwrap();
        *reader_count -= 1;
        if *reader_count == 0 {
            self.inner.condvar.notify_one();
        }
    }
}

pub fn get_default_metal_device() -> Option<ObjcId> {
    unsafe {
        let dev = MTLCreateSystemDefaultDevice();
        if dev == nil {None} else {Some(dev)}
    }
}

pub fn get_all_metal_devices() -> Vec<ObjcId> {
    #[cfg(any(target_os = "ios", target_os = "tvos"))]
    unsafe {
        vec![MTLCreateSystemDefaultDevice()]
    }
    #[cfg(target_os = "macos")]
    unsafe {
        let array = MTLCopyAllDevices();
        let count: u64 = msg_send![array, count];
        let ret = (0..count)
            .map( | i | msg_send![array, objectAtIndex: i])
        // The elements of this array are references---we convert them to owned references
        // (which just means that we increment the reference count here, and it is
        // decremented in the `Drop` impl for `Device`)
            .map( | device: *mut Object | msg_send![device, retain])
            .collect();
        let () = msg_send![array, release];
        ret
    }
}


