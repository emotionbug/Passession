use {
    crate::{
        makepad_live_compiler::{
            LiveId,
        },
        makepad_math::*,
        cx::Cx,
        os::{
            CxOsDrawCall,
            CxOsDrawList,
        },
        pass::PassId,
        id_pool::*,
        draw_shader::{
            CxDrawShaderOptions,
            CxDrawShaderMapping,
            CxDrawShader,
            DrawShader,
        },
        draw_vars::{
            DrawVars,
            DRAW_CALL_USER_UNIFORMS,
            DRAW_CALL_TEXTURE_SLOTS
        },
        texture::Texture,
        geometry::{GeometryId}
    }
};


#[derive(Debug)]
pub struct DrawList(PoolId);

impl DrawList{
    pub fn new(cx:&mut Cx)->Self{
        cx.draw_lists.alloc()
    }
}

#[derive(Clone, Debug, PartialEq, Copy, Hash, Ord, PartialOrd, Eq)]
pub struct DrawListId(usize, u64);

impl DrawListId{
    pub fn index(&self)->usize{self.0}
    pub fn generation(&self)->u64{self.1}
}

impl DrawList {
    pub fn id(&self) -> DrawListId {DrawListId(self.0.id, self.0.generation)}
}

/// Window overlay strata. Within a stratum, codeflow children paint after their owners.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum OverlayRank {
    #[default]
    Pane,
    Shell,
    Cursor,
}

#[derive(Default)]
pub struct CxDrawListPool {
    pool: IdPool<CxDrawList>,
    // Reused for composition of each window's overlay.
    overlay_order: Vec<OverlayEntry>,
}

struct OverlayEntry {
    id: DrawListId,
    redraw_id: u64,
    first_child: Option<usize>,
    next: Option<usize>,
}
impl CxDrawListPool {
    pub fn alloc(&mut self) -> DrawList {
        let id = self.pool.alloc();
        if id.generation != 0 {
            // A new owner can reuse the backing buffers, not their paint or viewport.
            let list = &mut self.pool.pool[id.id];
            list.clear_draw_items(0);
            list.draw_list_uniforms = CxDrawListUniforms::default();
            list.draw_list_has_clip = false;
            list.pass_id = None;
            list.codeflow_parent_id = None;
            list.codeflow_parent_redraw_id = 0;
            list.overlay_rank = OverlayRank::default();
            list.overlay_order_index = 0;
            list.debug_id = LiveId::default();
        }
        DrawList(id)
    }

    #[cfg(target_os = "android")]
    pub(crate) fn invalidate_gl_resources(&mut self) {
        for list in &mut self.pool.pool {
            list.os = Default::default();
            // Include unused entries: subsequent draws can reuse these slots.
            for item in &mut list.draw_items.buffer {
                item.os = Default::default();
                if let Some(call) = item.kind.draw_call_mut() {
                    call.uniforms_dirty = true;
                }
            }
        }
    }
    
    pub fn checked_index(&self, index: DrawListId)->Option<&CxDrawList>{
        let d = &self.pool.pool[index.0];
        if d.generation != index.1 {
            return None
        }
        return Some(&d.item)
    }

    /// Turns overlay submissions into paint order: ranks first, then a stable traversal
    /// of each rank's codeflow forest. A renewed ancestor brings its cached descendants
    /// with it. An overlay can raise its own rank but cannot paint below an ancestor.
    pub fn compose_overlays(&mut self, overlay: DrawListId, root: DrawListId) {
        self.overlay_order.clear();
        let len = self[overlay].draw_items.len();
        for i in 0..len {
            let item = &self[overlay].draw_items[i];
            let Some(id) = item.sub_list().filter(|&id| self.checked_index(id).is_some()) else {
                continue;
            };
            let redraw_id = item.redraw_id;
            self[id].overlay_order_index = self.overlay_order.len();
            self.overlay_order.push(OverlayEntry {
                id,
                redraw_id,
                first_child: None,
                next: None,
            });
        }

        let mut ranks = [None; OverlayRank::Cursor as usize + 1];
        // Link in reverse submission order so siblings and unrelated roots keep their order.
        for index in (0..self.overlay_order.len()).rev() {
            let id = self.overlay_order[index].id;
            let Some((parent, rank)) = self.overlay_position(id, root) else {
                continue; // A detached subtree can keep unchanged paint IDs internally.
            };
            let first = match parent {
                Some(parent) => &mut self.overlay_order[parent].first_child,
                None => &mut ranks[rank as usize],
            };
            let next = first.replace(index);
            self.overlay_order[index].next = next;
        }

        let mut written = 0;
        for first in ranks {
            self.write_overlay_branch(overlay, first, &mut written);
        }
        let list = &mut self[overlay];
        list.draw_items.used = written;
        list.find_appendable_draw_shader_id.truncate(written);
    }

    fn overlay_position(
        &self,
        id: DrawListId,
        root: DrawListId,
    ) -> Option<(Option<usize>, OverlayRank)> {
        let rank = self[id].overlay_rank;
        let mut ancestor_rank = OverlayRank::Pane;
        let mut closest = None;
        let mut current = id;
        for _ in 0..self.pool.pool.len() {
            let list = self.checked_index(current)?;
            let parent_id = list.codeflow_parent_id?;
            let parent = self.checked_index(parent_id)?;
            if list.codeflow_parent_redraw_id != parent.redraw_id {
                return None;
            }
            if parent_id == root {
                return Some((
                    if rank <= ancestor_rank { closest } else { None },
                    rank.max(ancestor_rank),
                ));
            }
            let index = parent.overlay_order_index;
            if self.overlay_order.get(index).is_some_and(|entry| entry.id == parent_id) {
                closest = closest.or(Some(index));
                ancestor_rank = ancestor_rank.max(parent.overlay_rank);
            }
            current = parent_id;
        }
        None
    }

    fn write_overlay_branch(
        &mut self,
        overlay: DrawListId,
        mut next: Option<usize>,
        written: &mut usize,
    ) {
        while let Some(index) = next {
            let OverlayEntry { id, redraw_id, first_child, next: sibling } = self.overlay_order[index];
            let item = &mut self[overlay].draw_items[*written];
            if item.sub_list() != Some(id) {
                item.kind = CxDrawKind::SubList(id);
                item.redraw_id = redraw_id;
            }
            *written += 1;
            self.write_overlay_branch(overlay, first_child, written);
            next = sibling;
        }
    }
}

impl std::ops::Index<DrawListId> for CxDrawListPool {
    
    type Output = CxDrawList;
    fn index(&self, index: DrawListId) -> &Self::Output {
        let d = &self.pool.pool[index.0];
        if d.generation != index.1 {
            error!("Drawlist id generation wrong index: {} current gen:{} in pointer:{}", index.0, d.generation, index.1)
        }
        &d.item
    }
}

impl std::ops::IndexMut<DrawListId> for CxDrawListPool {
    fn index_mut(&mut self, index: DrawListId) -> &mut Self::Output {
        let d = &mut self.pool.pool[index.0];
        if d.generation != index.1 {
            error!("Drawlist id generation wrong {} {} {}", index.0, d.generation, index.1)
        }
        &mut d.item
        
    }
}


#[derive(Default, Clone)]
#[repr(C)]
pub struct CxDrawCallUniforms {
    pub draw_zbias: f32,
    pub pad1: f32,
    pub pad2: f32,
    pub pad3: f32,
}


impl CxDrawCallUniforms {
    pub fn as_slice(&self) -> &[f32; std::mem::size_of::<CxDrawCallUniforms>() / std::mem::size_of::<f32>()] {
        unsafe {std::mem::transmute(self)}
    }
    /*
    pub fn get_local_scroll(&self) -> Vec4 {
        self.draw_scroll
    }*/
    
    pub fn set_zbias(&mut self, zbias: f32) {
        self.draw_zbias = zbias;
    }
    /*
    pub fn set_clip(&mut self, clip: (Vec2, Vec2)) {
        self.draw_clip_x1 = clip.0.x;
        self.draw_clip_y1 = clip.0.y;
        self.draw_clip_x2 = clip.1.x;
        self.draw_clip_y2 = clip.1.y;
    }
    
    pub fn set_local_scroll(&mut self, scroll: Vec2, local_scroll: Vec2, options: &CxDrawShaderOptions) {
        self.draw_scroll.x = scroll.x;
        if !options.no_h_scroll {
            self.draw_scroll.x += local_scroll.x;
        }
        self.draw_scroll.y = scroll.y;
        if !options.no_v_scroll {
            self.draw_scroll.y += local_scroll.y;
        }
        self.draw_scroll.z = local_scroll.x;
        self.draw_scroll.w = local_scroll.y;
    }*/
}
 
pub enum CxDrawKind{
    SubList(DrawListId),
    DrawCall(CxDrawCall),
    Empty
}

pub struct CxDrawItem {
    pub redraw_id: u64,
    pub kind: CxDrawKind,
    // these values stick around to reduce buffer churn
    pub draw_item_id: usize,
    pub instances: Option<Vec<f32 >>,
    pub os: CxOsDrawCall
}

impl std::ops::Deref for  CxDrawItem {
    type Target = CxDrawKind;
    fn deref(&self) -> &Self::Target {&self.kind}
}

impl CxDrawKind{
    pub fn is_empty(&self)->bool{
        match self{
            CxDrawKind::Empty=>true,
            _=>false
        }
    }
    
    pub fn sub_list(&self)->Option<DrawListId>{
        match self{
            CxDrawKind::SubList(id)=>Some(*id),
            _=>None
        }
    }
    pub fn draw_call(&self)->Option<&CxDrawCall>{
        match self{
            CxDrawKind::DrawCall(call)=>Some(call),
            _=>None
        }
    }
    pub fn draw_call_mut(&mut self)->Option<&mut CxDrawCall>{
        match self{
            CxDrawKind::DrawCall(call)=>Some(call),
            _=>None
        }
    }
}

pub struct CxDrawCall {
    pub draw_shader: DrawShader, // if shader_id changed, delete gl vao
    pub options: CxDrawShaderOptions,
    pub total_instance_slots: usize,
    pub draw_call_uniforms: CxDrawCallUniforms, // draw uniforms
    pub geometry_id: Option<GeometryId>,
    pub user_uniforms: [f32; DRAW_CALL_USER_UNIFORMS], // user uniforms
    pub texture_slots: [Option<Texture>; DRAW_CALL_TEXTURE_SLOTS],
    pub instance_dirty: bool,
    pub uniforms_dirty: bool,
    pub(crate) visual_revision: u64,
}

impl CxDrawCall {
    pub fn new(mapping: &CxDrawShaderMapping, draw_vars: &DrawVars) -> Self {
        CxDrawCall {
            geometry_id: draw_vars.geometry_id,
            options: draw_vars.options.clone(),
            draw_shader: draw_vars.draw_shader.unwrap(),
            total_instance_slots: mapping.instances.total_slots,
            draw_call_uniforms: CxDrawCallUniforms::default(),
            user_uniforms: draw_vars.user_uniforms,
            texture_slots: draw_vars.texture_slots.clone(),
            instance_dirty: true,
            uniforms_dirty: true,
            visual_revision: 0,
        }
    }
}

impl CxDrawCall {
    /// Instance uploads and presentation are consumed independently (including by different passes).
    pub fn mark_instances_changed(&mut self) {
        self.instance_dirty = true;
        self.visual_revision = self.visual_revision.wrapping_add(1);
    }

    pub fn mark_uniforms_changed(&mut self) {
        self.uniforms_dirty = true;
        self.visual_revision = self.visual_revision.wrapping_add(1);
    }
}

#[derive(Clone)]
#[repr(C)]
pub struct CxDrawListUniforms {
    pub view_transform: Mat4,
    pub view_clip: Vec4,
    pub view_shift: Vec2,
    pub pad1: f32,
    pub pad2: f32       
}

impl Default for CxDrawListUniforms{
    fn default()->Self{
        Self{
            view_transform: Mat4::identity(),
            view_clip: vec4(-100000.0, -100000.0, 100000.0, 100000.0),
            view_shift: vec2(0.0,0.0),
            pad1: 0.0,
            pad2: 0.0,
        }
    }
}

impl CxDrawListUniforms {
    pub fn as_slice(&self) -> &[f32; std::mem::size_of::<CxDrawListUniforms>() / std::mem::size_of::<f32>()] {
        unsafe {std::mem::transmute(self)}
    }
}

#[derive(Default)]
pub struct CxDrawItems {
    pub(crate) buffer: Vec<CxDrawItem>,
    used: usize
}

impl std::ops::Index<usize> for CxDrawItems {
    type Output = CxDrawItem;
    fn index(&self, index: usize) -> &Self::Output {
        &self.buffer[index]
    }
}

impl std::ops::IndexMut<usize> for CxDrawItems {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.buffer[index]
    }
}

impl CxDrawItems{
    pub fn len(&self)->usize{self.used}
    pub fn clear(&mut self){self.used = 0}
    pub fn push_item(&mut self, redraw_id: u64, kind:CxDrawKind)->&mut CxDrawItem{
        let draw_item_id = self.used;
        if self.used >= self.buffer.len() {
            self.buffer.push(CxDrawItem {
                draw_item_id,
                redraw_id,
                instances: Some(Vec::new()),
                os: CxOsDrawCall::default(),
                kind: kind,
            });
        }
        else{
            // reuse an older one, keeping all GPU resources attached
            let draw_item = &mut self.buffer[draw_item_id];
            draw_item.instances.as_mut().unwrap().clear();
            draw_item.kind = kind;
            draw_item.redraw_id = redraw_id;
        }
        self.used += 1;
        &mut self.buffer[draw_item_id]
    }
}

#[derive(Default)]
pub struct CxDrawList {
    pub debug_id: LiveId,
    
    pub codeflow_parent_id: Option<DrawListId>, // the id of the parent we nest in, codeflow wise
    /// The parent's paint that last attached this list, including cached replays.
    pub codeflow_parent_redraw_id: u64,
    pub overlay_rank: OverlayRank,
    overlay_order_index: usize,
    
    pub redraw_id: u64,
    pub pass_id: Option<PassId>,
    
    pub draw_items: CxDrawItems,
    
    pub draw_list_uniforms: CxDrawListUniforms,
    pub draw_list_has_clip: bool,
    
    pub os: CxOsDrawList,
    pub rect_areas: Vec<CxRectArea>,
    pub find_appendable_draw_shader_id: Vec<u64>
}

pub struct CxRectArea{
    pub rect: Rect,
    pub draw_clip: (DVec2,DVec2)
}

impl CxDrawList {
/*
    pub fn intersect_clip(&mut self, clip: (Vec2, Vec2)) -> (Vec2, Vec2) {
        if !self.unclipped {
            let min_x = self.rect.pos.x - self.parent_scroll.x;
            let min_y = self.rect.pos.y - self.parent_scroll.y;
            let max_x = self.rect.pos.x + self.rect.size.x - self.parent_scroll.x;
            let max_y = self.rect.pos.y + self.rect.size.y - self.parent_scroll.y;
            
            let ret = (Vec2 {
                x: min_x.max(clip.0.x),
                y: min_y.max(clip.0.y)
            }, Vec2 {
                x: max_x.min(clip.1.x),
                y: max_y.min(clip.1.y)
            });
            self.clip_points = ret;
            ret
        }
        else {
            self.clip_points = clip;
            clip
        }
    }*/
    
    pub fn find_appendable_drawcall(&mut self, sh: &CxDrawShader, draw_vars: &DrawVars) -> Option<usize> {
        // find our drawcall to append to the current layer
        if draw_vars.draw_shader.is_none(){
            return None
        }
        let draw_shader_id = draw_vars.draw_shader.as_ref().unwrap().false_compare_id();
        
        //let mut found = 0;
        for i in (0..self.draw_items.len()).rev() {
            if self.find_appendable_draw_shader_id[i] == draw_shader_id{
                let draw_item = &mut self.draw_items[i];
                if let Some(draw_call) = &draw_item.draw_call() {
                    // TODO! figure out why this can happen
                    if draw_call.draw_shader != draw_vars.draw_shader.unwrap(){
                        continue
                    }
                    // lets compare uniforms and textures..
                    if !sh.mapping.flags.draw_call_nocompare {
                        if draw_call.geometry_id != draw_vars.geometry_id {
                            continue
                        }
                        let mut diff = false;
                        for i in 0..sh.mapping.user_uniforms.total_slots {
                            if draw_call.user_uniforms[i] != draw_vars.user_uniforms[i] {
                                diff = true;
                                break;
                            }
                        }
                        if diff {continue}
                                
                        for i in 0..sh.mapping.textures.len() {
                            fn neq(a:&Option<Texture>, b:&Option<Texture>)->bool{
                                if let Some(a) = a{
                                    if let Some(b) = b{
                                        return a.texture_id() != b.texture_id()
                                    }
                                    return true
                                }
                                return false
                            }
                            if neq(&draw_call.texture_slots[i], &draw_vars.texture_slots[i]) {
                                diff = true;
                                break;
                            }
                        }
                        if diff {continue}
                    }
                    if !draw_call.options._appendable_drawcall(&draw_vars.options) {
                        continue
                    }
                    return Some(i)
                }
                else{
                    break;
                }
            }
        }
        None
    }
    
    pub fn append_draw_call(&mut self, redraw_id: u64, sh: &CxDrawShader, draw_vars: &DrawVars) -> &mut CxDrawItem {
        
        if let Some(ds) = &draw_vars.draw_shader{
            self.find_appendable_draw_shader_id.push(ds.false_compare_id());
        }
        else{
            self.find_appendable_draw_shader_id.push(0);
        }
        self.draw_items.push_item(
            redraw_id,
            CxDrawKind::DrawCall(CxDrawCall::new(&sh.mapping, draw_vars))
        )
    }
    
    pub fn clear_draw_items(&mut self, redraw_id: u64) {
        self.redraw_id = redraw_id;
        self.draw_items.clear();
        self.rect_areas.clear();
        self.find_appendable_draw_shader_id.clear();
    }
    
    pub fn append_sub_list(&mut self, redraw_id: u64, sub_list_id: DrawListId) {
        // see if we need to add a new one
        self.draw_items.push_item(redraw_id, CxDrawKind::SubList(sub_list_id));
        self.find_appendable_draw_shader_id.push(0);
    }

    /// Omit a retained child from composition without discarding its paint or ancestry.
    pub fn detach_sub_list(&mut self, sub_list_id: DrawListId) {
        for index in (0..self.draw_items.len()).rev() {
            let item = &mut self.draw_items[index];
            if item.sub_list() == Some(sub_list_id) {
                item.kind = CxDrawKind::Empty;
                return;
            }
        }
    }
    
    pub fn store_sub_list_last(&mut self, redraw_id: u64, sub_list_id: DrawListId) {
        // use an empty slot if we have them to insert our subview
        let len = self.draw_items.len();
        for i in 0..len{
            let item = &mut self.draw_items[i];
            if let Some(id) = item.kind.sub_list(){
                if id == sub_list_id{
                    item.kind = CxDrawKind::Empty;
                    break
                }
            }
        }
        if len > 0{
            let item = &mut self.draw_items[len - 1];
            if item.kind.is_empty(){
                item.redraw_id = redraw_id;
                item.kind = CxDrawKind::SubList(sub_list_id);
                return
            }
            if let CxDrawKind::SubList(id) = item.kind{
                if id == sub_list_id{
                    item.redraw_id = redraw_id;
                    return
                }
            }
        }
        self.append_sub_list(redraw_id, sub_list_id);
    }

    pub fn store_sub_list(&mut self, redraw_id: u64, sub_list_id: DrawListId) {
        // use an empty slot if we have them to insert our subview
        for i in 0..self.draw_items.len(){
            let item = &mut self.draw_items[i];
            if let Some(id) = item.kind.sub_list(){
                if id == sub_list_id{
                    return
                }
            }
        }
        for i in 0..self.draw_items.len(){
            let item = &mut self.draw_items[i];
            if item.kind.is_empty(){
                item.redraw_id = redraw_id;
                item.kind = CxDrawKind::SubList(sub_list_id);
                return
            }
        }
        self.append_sub_list(redraw_id, sub_list_id);
    }
    
    /*
    pub fn get_local_scroll(&self) -> Vec2 {
        let xs = if self.no_v_scroll {0.} else {self.snapped_scroll.x};
        let ys = if self.no_h_scroll {0.} else {self.snapped_scroll.y};
        Vec2 {x: xs, y: ys}
    }*/
    /*
    pub fn uniform_view_transform(&mut self, v: &Mat4) {
        //dump in uniforms
        self.draw_list_uniforms.view_transform = *v;
    }
    
    pub fn get_view_transform(&self) -> Mat4 {
        self.draw_list_uniforms.view_transform
    }*/
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycled_list_does_not_clip_the_next_owners_hit_area() {
        use crate::area::{Area, RectArea};

        let mut cx = Cx::new(Box::new(|_, _| {}));
        let shimmer = DrawList::new(&mut cx);
        let old = shimmer.id();
        cx.draw_lists[old].draw_list_has_clip = true;
        cx.draw_lists[old].draw_list_uniforms.view_clip = vec4(5.0, 0.0, 8.0, 20.0);
        drop(shimmer);

        let sheet = DrawList::new(&mut cx);
        let id = sheet.id();
        assert_eq!(old.index(), id.index());
        let rect = Rect { pos: dvec2(20.0, 40.0), size: dvec2(100.0, 30.0) };
        cx.draw_lists[id].rect_areas.push(CxRectArea {
            rect, draw_clip: (rect.pos, rect.pos + rect.size),
        });
        let hit = Area::Rect(RectArea { draw_list_id: id, rect_id: 0, redraw_id: 0 });
        assert_eq!(hit.clipped_rect(&cx), rect);
        assert_eq!(cx.draw_lists[id].draw_list_uniforms.view_clip, CxDrawListUniforms::default().view_clip);
    }

    #[test]
    fn draw_call_uniform_slice_covers_only_its_storage() {
        let uniforms = CxDrawCallUniforms {
            draw_zbias: 0.125,
            pad1: 0.0,
            pad2: 0.0,
            pad3: 0.0,
        };
        assert_eq!(std::mem::size_of_val(uniforms.as_slice()), std::mem::size_of_val(&uniforms));
        assert_eq!(uniforms.as_slice(), &[0.125, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn draw_list_uniform_slice_covers_only_its_storage() {
        let uniforms = CxDrawListUniforms::default();
        assert_eq!(std::mem::size_of_val(uniforms.as_slice()), std::mem::size_of_val(&uniforms));
        assert_eq!(uniforms.as_slice().len(), 24);
        assert_eq!(&uniforms.as_slice()[16..], &[-100000.0, -100000.0, 100000.0, 100000.0, 0.0, 0.0, 0.0, 0.0]);
    }
}
