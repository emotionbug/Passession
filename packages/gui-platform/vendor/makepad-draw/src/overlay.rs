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
        cx.draw_lists.compose_overlays(self.draw_list.id(), parent_id);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn attach(lists: &mut CxDrawListPool, parent: DrawListId, child: DrawListId, redraw: u64) {
        let parent_redraw = lists[parent].redraw_id;
        let list = &mut lists[child];
        list.codeflow_parent_id = Some(parent);
        list.codeflow_parent_redraw_id = parent_redraw;
        list.clear_draw_items(redraw);
    }

    fn order(lists: &CxDrawListPool, overlay: DrawListId) -> Vec<DrawListId> {
        let items = &lists[overlay].draw_items;
        (0..items.len()).map(|i| items[i].sub_list().unwrap()).collect()
    }

    #[test]
    fn reused_overlay_child_does_not_retain_the_previous_generation() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let parent = draw_lists.alloc();
        let child = draw_lists.alloc();
        let stale_id = child.id();
        draw_lists[parent.id()].clear_draw_items(7);
        attach(&mut draw_lists, parent.id(), stale_id, 7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, stale_id);

        drop(child);
        let replacement = draw_lists.alloc();
        assert_eq!(replacement.id().index(), stale_id.index());
        assert_ne!(replacement.id().generation(), stale_id.generation());
        // The replacement has matching redraw metadata, so comparing only that
        // metadata would keep both generations of the same pool slot.
        attach(&mut draw_lists, parent.id(), replacement.id(), 7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, replacement.id());

        draw_lists.compose_overlays(overlay.draw_list.id(), parent.id());

        assert_eq!(order(&draw_lists, overlay.draw_list.id()), [replacement.id()]);
        assert_eq!(draw_lists[replacement.id()].redraw_id, 7);
    }

    #[test]
    fn reused_codeflow_parent_cannot_keep_an_orphaned_overlay_child() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let root = draw_lists.alloc();
        let parent = draw_lists.alloc();
        let stale_parent_id = parent.id();
        let child = draw_lists.alloc();
        draw_lists[root.id()].clear_draw_items(7);
        attach(&mut draw_lists, root.id(), stale_parent_id, 7);
        attach(&mut draw_lists, stale_parent_id, child.id(), 7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, child.id());

        drop(parent);
        let replacement = draw_lists.alloc();
        assert_eq!(replacement.id().index(), stale_parent_id.index());
        assert_ne!(replacement.id().generation(), stale_parent_id.generation());
        draw_lists[replacement.id()].clear_draw_items(7);

        draw_lists.compose_overlays(overlay.draw_list.id(), root.id());

        assert!(order(&draw_lists, overlay.draw_list.id()).is_empty());
        assert_eq!(draw_lists[child.id()].redraw_id, 7);
        assert_eq!(draw_lists[replacement.id()].redraw_id, 7);
    }

    #[test]
    fn cached_overlay_survives_replay_but_not_detachment_of_its_subtree() {
        let mut draw_lists = CxDrawListPool::default();
        let overlay = Overlay { draw_list: draw_lists.alloc() };
        let root = draw_lists.alloc();
        let parent = draw_lists.alloc();
        let intermediate = draw_lists.alloc();
        let child = draw_lists.alloc();
        draw_lists[root.id()].clear_draw_items(7);
        attach(&mut draw_lists, root.id(), parent.id(), 7);
        attach(&mut draw_lists, parent.id(), intermediate.id(), 7);
        attach(&mut draw_lists, intermediate.id(), child.id(), 7);
        draw_lists[overlay.draw_list.id()].store_sub_list(7, child.id());

        // A newer window frame can reattach the parent without repainting its contents.
        draw_lists[root.id()].clear_draw_items(8);
        draw_lists[parent.id()].codeflow_parent_redraw_id = 8;

        draw_lists.compose_overlays(overlay.draw_list.id(), root.id());
        assert_eq!(order(&draw_lists, overlay.draw_list.id()), [child.id()]);

        // The window omits the parent. The child's immediate attachment is still current.
        draw_lists[root.id()].clear_draw_items(9);
        draw_lists.compose_overlays(overlay.draw_list.id(), root.id());
        assert!(order(&draw_lists, overlay.draw_list.id()).is_empty());

        attach(&mut draw_lists, root.id(), parent.id(), 10);
        attach(&mut draw_lists, parent.id(), intermediate.id(), 10);
        attach(&mut draw_lists, intermediate.id(), child.id(), 10);
        draw_lists[overlay.draw_list.id()].store_sub_list(10, child.id());
        draw_lists.compose_overlays(overlay.draw_list.id(), root.id());
        assert_eq!(order(&draw_lists, overlay.draw_list.id()), [child.id()]);
    }

    #[test]
    fn overlay_promotions_stay_within_their_rank_and_keep_branches_together() {
        let mut lists = CxDrawListPool::default();
        let handles: [_; 12] = std::array::from_fn(|_| lists.alloc());
        let [root, overlay, parent, intermediate, child, left, right, chrome, shell, shell_child,
            cursor, new_child] = handles.each_ref().map(DrawList::id);

        lists[root].clear_draw_items(7);
        for id in [left, parent, right, chrome] {
            attach(&mut lists, root, id, 7);
        }
        attach(&mut lists, parent, intermediate, 7);
        attach(&mut lists, intermediate, child, 7);
        for id in [shell, cursor] {
            attach(&mut lists, chrome, id, 7);
        }
        attach(&mut lists, shell, shell_child, 7);
        lists[shell].overlay_rank = OverlayRank::Shell;
        lists[cursor].overlay_rank = OverlayRank::Cursor;
        for id in [left, parent, child, right, shell, shell_child, cursor] {
            lists[overlay].store_sub_list_last(7, id);
        }
        lists.compose_overlays(overlay, root);
        assert_eq!(order(&lists, overlay), [left, parent, child, right, shell, shell_child, cursor]);

        // Repaint only the pane overlay and replay its cached content; shell and cursor
        // are replayed through chrome without resubmitting their overlay lists.
        lists[root].clear_draw_items(8);
        for id in [left, right, chrome, intermediate] {
            lists[id].codeflow_parent_redraw_id = 8;
        }
        attach(&mut lists, root, parent, 8);
        lists[overlay].store_sub_list_last(8, parent);
        lists.compose_overlays(overlay, root);
        let promoted = [left, right, parent, child, shell, shell_child, cursor];
        assert_eq!(order(&lists, overlay), promoted);

        // Reuse can submit a descendant in a hole below its parent. It still composes on top.
        lists[overlay].store_sub_list_last(8, left);
        attach(&mut lists, intermediate, new_child, 8);
        lists[overlay].store_sub_list(8, new_child);
        lists.compose_overlays(overlay, root);
        assert_eq!(order(&lists, overlay), [right, parent, new_child, child, left, shell, shell_child, cursor]);

        lists[parent].clear_draw_items(9);
        lists[parent].codeflow_parent_id = None;
        lists.compose_overlays(overlay, root);
        assert_eq!(order(&lists, overlay), [right, left, shell, shell_child, cursor]);
    }
}
