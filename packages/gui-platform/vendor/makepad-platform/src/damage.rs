//! Conservative paint damage from the *render* tree, independent of pending GPU uploads.
#![cfg_attr(not(any(target_os = "linux", target_os = "android", target_os = "windows", target_os = "macos", target_os = "ios")), allow(dead_code))]
use crate::{
    cx::Cx,
    draw_list::{CxDrawListUniforms, DrawListId},
    draw_shader::{CxDrawShaderMapping, DrawShader},
    geometry::{CxGeometry, GeometryId},
    makepad_math::{Mat4, Vec4},
    pass::{PassId, PassUniforms},
};
use std::cell::Cell;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "android", target_os = "macos", target_os = "ios"))]
pub(crate) fn force_full_redraw() -> bool {
    static FULL_REDRAW: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("PASEO_FULL_REDRAW").is_some_and(|value| value == "1")
    });
    *FULL_REDRAW
}

thread_local! {
    static RESOURCE_REVISION: Cell<u64> = const { Cell::new(0) };
}

// Renderer resources belong to a Cx's UI thread. With an increasing shared stamp,
// the newest revision of a set of resources changes whenever any member changes.
pub(crate) fn next_resource_revision() -> u64 {
    RESOURCE_REVISION.with(|revision| {
        let next = revision.get().wrapping_add(1);
        revision.set(next);
        next
    })
}

/// Framebuffer pixels in GL's bottom-left coordinate system, half-open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl DamageRect {
    pub fn full([width, height]: [i32; 2]) -> Self {
        Self { x0: 0, y0: 0, x1: width, y1: height }
    }

    pub fn is_empty(self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    pub fn union(self, other: Self) -> Self {
        if self.is_empty() { return other; }
        if other.is_empty() { return self; }
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        self.x0.max(other.x0) < self.x1.min(other.x1)
            && self.y0.max(other.y0) < self.y1.min(other.y1)
    }

    /// Origin and size, as EGL damage and Metal scissors take them.
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", target_os = "android", test))]
    pub fn egl(self) -> [i32; 4] {
        [self.x0, self.y0, self.x1 - self.x0, self.y1 - self.y0]
    }
}

/// The changes between successfully presented frames, not the repairs used to
/// bring individual buffers up to date. Age one means the immediately previous
/// presentation. Zero/negative age or missing history requires a full repair.
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", target_os = "android", test))]
#[derive(Clone, Default)]
pub(crate) struct DamageHistory {
    frames: [DamageRect; 4],
    next: usize,
    len: usize,
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios", target_os = "android", test))]
impl DamageHistory {
    pub fn repair(&self, age: i32, current: DamageRect) -> Option<DamageRect> {
        let count = usize::try_from(age.checked_sub(1)?).ok()?;
        if count > self.len {
            return None;
        }
        let len = self.frames.len();
        Some((1..=count).fold(current, |rect, index| {
            rect.union(self.frames[(self.next + len - index) % len])
        }))
    }

    pub fn push(&mut self, rect: DamageRect) {
        self.frames[self.next] = rect;
        self.next = (self.next + 1) % self.frames.len();
        self.len = (self.len + 1).min(self.frames.len());
    }

    pub fn invalidate(&mut self) {
        self.len = 0;
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;

    fn rect(x: i32) -> DamageRect {
        DamageRect { x0: x, y0: 0, x1: x + 1, y1: 1 }
    }

    #[test]
    fn repairs_include_only_intervening_presentations() {
        let mut history = DamageHistory::default();
        assert_eq!(history.repair(0, rect(9)), None);
        assert_eq!(history.repair(1, rect(9)), Some(rect(9)));
        assert_eq!(history.repair(2, rect(9)), None);
        history.push(rect(1));
        // This computed repair was dropped, not presented.
        assert_eq!(history.repair(2, rect(0)), Some(rect(0).union(rect(1))));
        // Coalesced input can go straight from the last presented scene to the
        // latest one. The dropped repair never expands the history.
        history.push(rect(3).union(rect(4)));
        assert_eq!(history.repair(1, rect(9)), Some(rect(9)));
        assert_eq!(history.repair(2, rect(9)), Some(rect(3).union(rect(9))));
        assert_eq!(history.repair(3, rect(9)), Some(rect(1).union(rect(9))));
    }

    #[test]
    fn lost_history_and_resize_require_full_repair() {
        let mut history = DamageHistory::default();
        for i in 0..8 {
            history.push(rect(i));
        }
        assert_eq!(history.repair(3, rect(9)), Some(rect(6).union(rect(9))));
        assert_eq!(history.repair(5, rect(9)), Some(rect(4).union(rect(9))));
        assert_eq!(history.repair(6, rect(9)), None);
        history.invalidate();
        assert_eq!(history.repair(2, rect(9)), None);
        // Resize also resets buffer identities: the first acquired buffer has age 0.
        assert_eq!(history.repair(0, rect(9)), None);
        history.push(rect(10));
        assert_eq!(history.repair(2, rect(11)), Some(rect(10).union(rect(11))));
    }
}

const PASS_VIEW_FLOATS: usize = std::mem::size_of::<PassUniforms>() / 4 - 2; // exclude time and padding

#[derive(Clone, PartialEq)]
struct PaintState {
    size: [i32; 2],
    pass: PassId,
    clear: [u32; 5],
    dont_clear: bool,
    uniforms: [f32; PASS_VIEW_FLOATS],
    zbias_step: u32,
    shader_generation: u64,
    damage_epoch: u64,
}

#[derive(Clone, Copy, PartialEq)]
struct View {
    transform: Mat4,
    clip: Vec4,
    shift: [f32; 2],
}

impl From<&CxDrawListUniforms> for View {
    fn from(value: &CxDrawListUniforms) -> Self {
        Self {
            transform: value.view_transform,
            clip: value.view_clip,
            shift: [value.view_shift.x, value.view_shift.y],
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
struct ItemState {
    list: DrawListId,
    slot: usize,
    redraw_id: u64,
    revision: u64,
    resource_revision: u64,
    geometry: Option<GeometryId>,
    shader: DrawShader,
    compiled: Option<usize>,
    zbias: u32,
    instance_len: usize,
    view: View,
}

#[derive(Clone, Copy)]
struct Item {
    state: ItemState,
    // None means unknown fragment coverage, never empty and never cullable.
    bounds: Option<DamageRect>,
}

#[derive(Clone, Default)]
pub(crate) struct DamageFrame {
    previous_state: Option<PaintState>,
    pending_state: Option<PaintState>,
    previous: Vec<Item>,
    pending: Vec<Item>,
}

impl DamageFrame {
    pub fn invalidate(&mut self) {
        self.previous_state = None;
    }

    pub fn commit(&mut self) {
        std::mem::swap(&mut self.previous, &mut self.pending);
        self.previous_state = self.pending_state.take();
    }

    pub fn should_cull(&self, index: usize, repair: DamageRect) -> bool {
        self.pending.get(index).and_then(|item| item.bounds).is_some_and(|bounds| !bounds.intersects(repair))
    }

    pub fn collect(&mut self, cx: &Cx, pass: PassId, root: DrawListId, size: [i32; 2], clear: [u32; 5]) -> DamageRect {
        let draw_pass = &cx.passes[pass];
        let state = PaintState {
            size,
            pass,
            clear,
            dont_clear: draw_pass.dont_clear,
            uniforms: draw_pass.pass_uniforms.as_slice()[..PASS_VIEW_FLOATS].try_into().unwrap(),
            zbias_step: draw_pass.zbias_step.to_bits(),
            shader_generation: cx.draw_shaders.generation,
            damage_epoch: draw_pass.damage_epoch,
        };
        let full = DamageRect::full(size);
        let has_baseline = self.previous_state.as_ref() == Some(&state);
        let mut damage = if has_baseline { DamageRect::default() } else { full };
        self.pending.clear();
        self.collect_list(cx, pass, root, full, has_baseline, &mut damage, &mut 0.0);
        if has_baseline {
            // Order is significant even if only depth bias changed. Comparison at the
            // paint ordinal conservatively damages the entire changed part of the sequence.
            for old in self.previous.iter().skip(self.pending.len()) {
                damage = damage.union(old.bounds.unwrap_or(full));
            }
        }
        self.pending_state = Some(state);
        // A repaint request with no tracked mutation can have come from a native or
        // other untracked source. Do not assume it is a no-op.
        if draw_pass.dont_clear || damage.is_empty() { full } else { damage }
    }

    fn collect_list(&mut self, cx: &Cx, pass_id: PassId, id: DrawListId, full: DamageRect, has_baseline: bool, damage: &mut DamageRect, zbias: &mut f32) {
        let list = &cx.draw_lists[id];
        let view = View::from(&list.draw_list_uniforms);
        let pass = &cx.passes[pass_id];
        let uniforms = &pass.pass_uniforms;
        let mut unknown_in_list = false;
        let mut list_changed = false;
        for slot in 0..list.draw_items.len() {
            let item = &list.draw_items[slot];
            if let Some(child) = item.sub_list() {
                self.collect_list(cx, pass_id, child, full, has_baseline, damage, zbias);
                continue;
            }
            let Some(call) = item.draw_call() else { continue; };
            let geometry = call.geometry_id.map(|id| &cx.geometries[id]);
            let mut resource_revision = geometry.map_or(0, |geometry| geometry.visual_revision);
            let mut external = false;
            let shader = &cx.draw_shaders[call.draw_shader.draw_shader_id];
            for texture in call.texture_slots.iter().take(shader.mapping.textures.len()).flatten() {
                let texture = &cx.textures[texture.texture_id()];
                resource_revision = resource_revision.max(texture.visual_revision);
                external |= texture.format.is_video() || texture.format.is_shared() || texture.format.is_depth();
            }
            let instances = item.instances.as_deref().unwrap_or_default();
            let state = ItemState {
                list: id, slot, redraw_id: item.redraw_id, revision: call.visual_revision,
                resource_revision, geometry: call.geometry_id, shader: call.draw_shader,
                compiled: shader.os_shader_id, zbias: zbias.to_bits(), instance_len: instances.len(), view,
            };
            if shader.os_shader_id.is_some() {
                *zbias += pass.zbias_step;
            }
            let old = has_baseline.then(|| self.previous.get(self.pending.len())).flatten();
            let changed = old.is_none_or(|old| old.state != state);
            let bounds = if changed {
                if instances.is_empty() || geometry.is_none() || shader.os_shader_id.is_none() {
                    Some(DamageRect::default())
                } else {
                    item_bounds(&shader.mapping, instances, geometry.unwrap(), view, uniforms, full)
                }
            } else {
                old.unwrap().bounds
            };
            if external || shader.mapping.uses_time {
                *damage = full;
            } else if changed {
                *damage = damage.union(bounds.unwrap_or(full));
                if let Some(old) = old { *damage = damage.union(old.bounds.unwrap_or(full)); }
            }
            unknown_in_list |= bounds.is_none();
            list_changed |= changed;
            self.pending.push(Item { state, bounds });
        }
        if unknown_in_list && list_changed { *damage = full; }
    }
}

fn item_bounds(mapping: &CxDrawShaderMapping, instances: &[f32], geometry: &CxGeometry, view: View, pass: &PassUniforms, full: DamageRect) -> Option<DamageRect> {
    if !mapping.fragments_bounded_by_clip { return None; }
    let clip = mapping.draw_clip?;
    let position = mapping.rect_pos?;
    let size = mapping.rect_size?;
    let stride = mapping.instances.total_slots;
    if stride == 0 || instances.len() % stride != 0 { return None; }
    // Each audited vertex shader clamps the vertex to draw_clip, followed by
    // view_shift and view_clip. The unit quad further bounds it by the instance rect.
    let unit_quad = mapping.geometries.total_slots == 2
        && geometry.vertices.iter().all(|v| (0.0..=1.0).contains(v));
    let [sx, sy] = view.shift;
    let [vx0, vy0, vx1, vy1] = [view.clip.x, view.clip.y, view.clip.z, view.clip.w];
    if ![sx, sy, vx0, vy0, vx1, vy1].into_iter().all(f32::is_finite)
        || vx0 > vx1 || vy0 > vy1
    {
        return None;
    }
    let matrices = [&view.transform, &pass.camera_view, &pass.camera_projection];
    if !matrices.iter().all(|matrix| xy_affine(matrix)) { return None; }

    let mut lo = [f32::INFINITY; 2];
    let mut hi = [f32::NEG_INFINITY; 2];
    for instance in instances.chunks_exact(stride) {
        let [mut x0, mut y0, mut x1, mut y1] = *<&[f32; 4]>::try_from(instance.get(clip..clip + 4)?).ok()?;
        if ![x0, y0, x1, y1].into_iter().all(f32::is_finite) || x0 > x1 || y0 > y1 {
            return None;
        }
        if unit_quad {
            let x = *instance.get(position)?;
            let y = *instance.get(position + 1)?;
            let right = x + instance.get(size)?;
            let bottom = y + instance.get(size + 1)?;
            if ![x, y, right, bottom].into_iter().all(bounded_coordinate) { return None; }
            (x0, x1) = (x.min(right).clamp(x0, x1), x.max(right).clamp(x0, x1));
            (y0, y1) = (y.min(bottom).clamp(y0, y1), y.max(bottom).clamp(y0, y1));
        }
        lo[0] = lo[0].min((x0 + sx).clamp(vx0, vx1));
        lo[1] = lo[1].min((y0 + sy).clamp(vy0, vy1));
        hi[0] = hi[0].max((x1 + sx).clamp(vx0, vx1));
        hi[1] = hi[1].max((y1 + sy).clamp(vy0, vy1));
    }
    let mut min = [f32::INFINITY; 2];
    let mut max = [f32::NEG_INFINITY; 2];
    for x in [lo[0], hi[0]] {
        for y in [lo[1], hi[1]] {
            let mut p = Vec4 { x, y, z: 0.0, w: 1.0 };
            for matrix in matrices {
                if !bounded_coordinate(p.x) || !bounded_coordinate(p.y) { return None; }
                // The checked matrices cannot couple depth to x, y, or w.
                p = matrix.transform_vec4(p);
            }
            let x = (p.x + 1.0) * full.x1 as f32 * 0.5;
            let y = (p.y + 1.0) * full.y1 as f32 * 0.5;
            if !bounded_coordinate(x) || !bounded_coordinate(y) { return None; }
            min[0] = min[0].min(x);
            min[1] = min[1].min(y);
            max[0] = max[0].max(x);
            max[1] = max[1].max(y);
        }
    }
    // Cover rasterization and small f32/driver rounding differences at the edge.
    const GUARD: i32 = 2;
    Some(DamageRect {
        x0: (min[0].floor() as i32 - GUARD).clamp(0, full.x1),
        y0: (min[1].floor() as i32 - GUARD).clamp(0, full.y1),
        x1: (max[0].ceil() as i32 + GUARD).clamp(0, full.x1),
        y1: (max[1].ceil() as i32 + GUARD).clamp(0, full.y1),
    })
}

fn bounded_coordinate(value: f32) -> bool {
    value.abs() <= 32768.0
}

fn xy_affine(matrix: &Mat4) -> bool {
    let m = &matrix.v;
    m[3] == 0.0 && m[7] == 0.0 && m[8] == 0.0 && m[9] == 0.0 && m[11] == 0.0 && m[15] == 1.0
        && [m[0], m[1], m[4], m[5]].into_iter().all(|v| v.abs() <= 4.0)
        && m.iter().copied().all(bounded_coordinate)
}
