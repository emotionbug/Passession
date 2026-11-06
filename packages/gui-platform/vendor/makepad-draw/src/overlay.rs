use {
    /*std::{
        rc::Rc,
        cell::{RefCell},        
    },*/
    crate::{
        makepad_platform::*,
        cx_2d::Cx2d,
    }
};

#[derive(Debug)]
pub struct Overlay { // draw info per UI element
    pub (crate) draw_list: DrawList,
    //pub (crate) sweep_lock: Rc<RefCell<Area>>, 
}

impl LiveHook for Overlay {}
impl LiveNew for Overlay {
    fn live_design_with(_cx:&mut Cx){}
    fn new(cx: &mut Cx) -> Self {
        let draw_list = cx.draw_lists.alloc();
        //cx.draw_lists[draw_list.id()].unclipped = true;
        Self {
            //sweep_lock: Rc::new(RefCell::new(Area::Empty)),
            draw_list,
        }
    }
    
    fn live_type_info(_cx: &mut Cx) -> LiveTypeInfo {
        LiveTypeInfo {
            module_id: LiveModuleId::from_str(&module_path!()).unwrap(),
            live_type: LiveType::of::<Self>(),
            live_ignore: true,
            fields: Vec::new(),
            type_name: id_lut!(Overlay)
        }
    }
}

impl LiveApply for Overlay {
    fn apply(&mut self, _cx: &mut Cx, _applyl: &mut Apply, index: usize, nodes: &[LiveNode]) -> usize {
        nodes.skip_node(index)
    }
}

impl Overlay {
    pub fn handle_event(&self, _cx:&Cx, _event:&Event){
        /*let area = self.sweep_lock.borrow().clone();
        if !area.is_empty(){
            match event{
                Event::MouseMove(e)=>e.sweep_lock.set(area),
                Event::MouseDown(e)=>e.sweep_lock.set(area),
                Event::Scroll(e)=>e.sweep_lock.set(area),
                _=>()
            }
        }*/
    }
    
    pub fn begin(&self, cx:&mut Cx2d){
        // mark our overlay_id on cx
        cx.overlay_id = Some(self.draw_list.id());
       // cx.overlay_sweep_lock = Some(self.sweep_lock.clone());
    }
    
    pub fn end(&self, cx:&mut Cx2d){
        cx.overlay_id = None;
        let parent_id = cx.draw_list_stack.last().cloned().unwrap();
        let redraw_id = cx.redraw_id;
        cx.draw_lists[parent_id].append_sub_list(redraw_id, self.draw_list.id());
        
        self.prune_stale_sublists(&mut cx.draw_lists);
    }

    fn prune_stale_sublists(&self, draw_lists: &mut CxDrawListPool) {
        // Unlike ordinary draw lists, the overlay retains its children across
        // redraws. A dropped widget's slot may now belong to another generation,
        // whose redraw id and parent cannot validate the old retained reference.
        for i in 0..draw_lists[self.draw_list.id()].draw_items.len() {
            if let Some(sub_id) = draw_lists[self.draw_list.id()].draw_items[i].sub_list() {
                let stale = match draw_lists.checked_index(sub_id) {
                    None => true,
                    Some(sub_list) => sub_list.codeflow_parent_id.is_some_and(|parent_id| {
                        draw_lists.checked_index(parent_id)
                            .map_or(true, |parent| parent.redraw_id != sub_list.redraw_id)
                    }),
                };
                if stale {
                    draw_lists[self.draw_list.id()].clear_sub_list(sub_id);
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reused_overlay_child_does_not_retain_the_previous_generation() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let parent = draw_lists.alloc();
        let child = draw_lists.alloc();
        let stale_id = child.id();
        draw_lists[parent.id()].clear_draw_items(7);
        draw_lists[stale_id].codeflow_parent_id = Some(parent.id());
        draw_lists[stale_id].clear_draw_items(7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, stale_id);

        drop(child);
        let replacement = draw_lists.alloc();
        assert_eq!(replacement.id().index(), stale_id.index());
        assert_ne!(replacement.id().generation(), stale_id.generation());
        // The replacement has matching redraw metadata, so comparing only that
        // metadata would keep both generations of the same pool slot.
        draw_lists[replacement.id()].codeflow_parent_id = Some(parent.id());
        draw_lists[replacement.id()].clear_draw_items(7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, replacement.id());

        overlay.prune_stale_sublists(&mut draw_lists);

        let items = &draw_lists[overlay.draw_list.id()].draw_items;
        assert_eq!(items.len(), 2);
        assert!(items[0].kind.is_empty());
        assert_eq!(items[1].sub_list(), Some(replacement.id()));
        assert_eq!(draw_lists[replacement.id()].redraw_id, 7);
    }

    #[test]
    fn reused_codeflow_parent_cannot_keep_an_orphaned_overlay_child() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let parent = draw_lists.alloc();
        let stale_parent_id = parent.id();
        let child = draw_lists.alloc();
        draw_lists[stale_parent_id].clear_draw_items(7);
        draw_lists[child.id()].codeflow_parent_id = Some(stale_parent_id);
        draw_lists[child.id()].clear_draw_items(7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, child.id());

        drop(parent);
        let replacement = draw_lists.alloc();
        assert_eq!(replacement.id().index(), stale_parent_id.index());
        assert_ne!(replacement.id().generation(), stale_parent_id.generation());
        draw_lists[replacement.id()].clear_draw_items(7);

        overlay.prune_stale_sublists(&mut draw_lists);

        assert!(draw_lists[overlay.draw_list.id()].draw_items[0].kind.is_empty());
        assert_eq!(draw_lists[child.id()].redraw_id, 7);
        assert_eq!(draw_lists[replacement.id()].redraw_id, 7);
    }

    #[test]
    fn cached_overlay_survives_until_its_live_parent_redraws_without_it() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let parent = draw_lists.alloc();
        let child = draw_lists.alloc();
        draw_lists[parent.id()].clear_draw_items(7);
        draw_lists[child.id()].codeflow_parent_id = Some(parent.id());
        draw_lists[child.id()].clear_draw_items(7);
        // A newer window frame must not discard a still-cached subtree.
        draw_lists[overlay.draw_list.id()].clear_draw_items(8);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, child.id());

        overlay.prune_stale_sublists(&mut draw_lists);
        assert_eq!(draw_lists[overlay.draw_list.id()].draw_items[0].sub_list(), Some(child.id()));

        draw_lists[parent.id()].clear_draw_items(8);
        overlay.prune_stale_sublists(&mut draw_lists);
        assert!(draw_lists[overlay.draw_list.id()].draw_items[0].kind.is_empty());

        // The same live overlay can be attached again when it is drawn.
        draw_lists[child.id()].clear_draw_items(8);
        draw_lists[overlay.draw_list.id()].store_sub_list(8, child.id());
        overlay.prune_stale_sublists(&mut draw_lists);
        assert_eq!(draw_lists[overlay.draw_list.id()].draw_items[0].sub_list(), Some(child.id()));
    }
}
