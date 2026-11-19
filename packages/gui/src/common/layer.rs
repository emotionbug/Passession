//! Retained display layers: the shell's equivalent of Android `RenderNode`s and iOS
//! `CALayer`s.
//!
//! Each shell region (chrome, sidebar, every pane root, the page) paints into its own
//! [`Layer`], a sibling under the window's draw list. Invalidation stays local:
//!
//! - A widget that redraws an area it drew (`self.redraw(cx)`, `area.redraw(cx)`) repaints
//!   only the layer that holds it. The window list re-runs its cheap composition; every
//!   other layer replays its retained draw calls, shell hit regions and overlay children.
//! - A layer also repaints when its rect or its `inputs` key changes, when the shell
//!   invalidates it for a shared-state change it presents (`common::state_delta`), after it
//!   was hidden for a frame, and on the theme/font/locale `Cx::redraw_all`.
//!
//! Widgets keep the ordinary Makepad contract — redraw yourself when your own data changes —
//! and never call `Cx::redraw_all` for local changes.

use std::collections::HashMap;

use makepad_widgets::*;

use super::paint::{HitMap, PaintedHit};

#[derive(Default)]
pub struct Layer {
    list: Option<DrawList2d>,
    inputs: u64,
    attached: bool,
    visited: bool,
    hit_mark: usize,
    hits: Vec<PaintedHit<String>>,
    tooltips: Vec<(String, String)>,
}

impl Layer {
    /// Repaints the layer on the next frame.
    #[track_caller]
    pub fn invalidate(&self, cx: &mut Cx) {
        if let Some(list) = &self.list {
            list.redraw(cx);
        }
    }

    /// Replays retained geometry in the shell after a paint-only viewport move.
    pub(crate) fn redraw_parent(&self, cx: &mut Cx) {
        if let Some(parent) = self
            .list
            .as_ref()
            .and_then(|list| cx.draw_lists[list.id()].codeflow_parent_id)
        {
            cx.redraw_list(parent);
        }
    }

    /// Shell hit ids recorded by the last paint.
    pub fn owns_hit(&self, id: &str) -> bool {
        self.hits.iter().any(|hit| hit.item == id)
    }

    /// Opens the layer at `rect`. `inputs` digests values the paint reads that do not reach it
    /// through its own areas (focus, activity); a different key repaints.
    ///
    /// True: paint now, then close with [`Layer::end`]. False: the retained paint is still
    /// valid and its hit regions and tooltips were replayed into `hits`/`tooltips`.
    #[track_caller]
    pub fn begin(
        &mut self,
        cx: &mut Cx2d,
        rect: Rect,
        inputs: u64,
        hits: &mut HitMap<String>,
        tooltips: &mut HashMap<String, String>,
    ) -> bool {
        self.visited = true;
        self.attached = true;
        let inputs_changed = std::mem::replace(&mut self.inputs, inputs) != inputs;
        let list = self.list.get_or_insert_with(|| DrawList2d::new(cx));
        // Always evaluated: it also records the rect the next frame compares against.
        let dirty = cx.will_redraw(list, Walk::abs_rect(rect));
        if list.begin_maybe(cx, dirty || inputs_changed).is_ok() {
            self.hit_mark = hits.len();
            cx.frame_trace_layer(std::panic::Location::caller(), true);
            return true;
        }
        cx.frame_trace_layer(std::panic::Location::caller(), false);
        hits.replay(&self.hits);
        for (id, tip) in &self.tooltips {
            tooltips.insert(id.clone(), tip.clone());
        }
        false
    }

    /// Closes a paint opened by [`Layer::begin`] and keeps its hit regions for replay.
    pub fn end(
        &mut self,
        cx: &mut Cx2d,
        hits: &HitMap<String>,
        tooltips: &HashMap<String, String>,
    ) {
        if let Some(list) = &mut self.list {
            list.end(cx);
        }
        self.hits.clear();
        self.hits.extend_from_slice(hits.since(self.hit_mark));
        self.tooltips.clear();
        for hit in &self.hits {
            if let Some(tip) = tooltips.get(&hit.item) {
                self.tooltips.push((hit.item.clone(), tip.clone()));
            }
        }
    }

    /// Moves the existing draw calls inside a fixed viewport without a Draw event.
    pub(crate) fn place_viewport(&self, cx: &mut Cx, clip: Rect, shift: DVec2) {
        if let Some(list) = &self.list {
            place_viewport(cx, list.id(), clip, shift);
        }
    }

    /// Ends the shell frame: a layer the frame did not open is detached, so its overlay
    /// children disappear with it and it repaints when shown again.
    pub fn finish_frame(&mut self, cx: &mut Cx2d) {
        if std::mem::take(&mut self.visited) {
            return;
        }
        if std::mem::take(&mut self.attached)
            && let Some(list) = &mut self.list
        {
            list.release(cx);
            self.hits.clear();
            self.tooltips.clear();
        }
    }
}

/// Applies view uniforms to one paint-only draw list (not its child lists).
pub(crate) fn place_viewport(cx: &mut Cx, id: DrawListId, clip: Rect, shift: DVec2) {
    let list = &mut cx.draw_lists[id];
    let uniforms = &mut list.draw_list_uniforms;
    let end = clip.pos + clip.size;
    let bounds = vec4(
        clip.pos.x as f32,
        clip.pos.y as f32,
        end.x as f32,
        end.y as f32,
    );
    let shift: Vec2 = shift.into();
    if list.draw_list_has_clip && uniforms.view_clip == bounds && uniforms.view_shift == shift {
        return;
    }
    list.draw_list_has_clip = true;
    uniforms.view_clip = bounds;
    uniforms.view_shift = shift;
    if let Some(pass) = list.pass_id {
        cx.repaint_pass(pass);
    }
}

/// Digest of the values a layer's paint reads outside its own areas.
pub fn inputs(values: impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(values)
}
