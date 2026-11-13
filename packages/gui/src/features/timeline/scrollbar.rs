//! Scroll indicators of the timeline's clipped regions (code panes, tables,
//! detail blocks, capped sheets): a draggable thumb per scrollable axis whose
//! track pages on click, and edge fades that show more content sideways.
//! Painters call [`draw`] after the region's content (and after popping its
//! clip) so the bar hits win over the content and stay inside the outer clip.

use super::canvas::*;
use super::paint::{Draw, RowState};
use makepad_widgets::*;

/// Thickness of the bar strip (hit area and reserved space).
pub const BAR: f64 = 10.0;
/// Inset of the track from the region edges.
const INSET: f64 = 2.0;
const MIN_THUMB: f64 = 24.0;
/// Width of the side fades.
const FADE: f64 = 16.0;
const FADE_STEPS: usize = 8;

/// One scrollable axis of a region: its state key, offset and maximum.
pub struct Axis<'a> {
    pub key: &'a str,
    pub offset: f64,
    pub max: f64,
}

/// Bar geometry along its axis, relative to the canvas origin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    pub start: f64,
    pub len: f64,
    pub thumb_start: f64,
    pub thumb_len: f64,
    pub max: f64,
    /// Visible extent: a track click pages by this much.
    pub page: f64,
}

impl Track {
    fn new(start: f64, len: f64, visible: f64, offset: f64, max: f64) -> Self {
        let len = len.max(1.0);
        let thumb_len = (len * visible / (visible + max).max(1.0)).clamp(MIN_THUMB.min(len), len);
        let travel = (len - thumb_len).max(0.0);
        let thumb_start = start + if max > 0.0 { travel * (offset / max).clamp(0.0, 1.0) } else { 0.0 };
        Track { start, len, thumb_start, thumb_len, max, page: visible }
    }

    /// The offset that puts the thumb's grab point under `along`.
    pub fn offset_at(&self, along: f64, grab: f64) -> f64 {
        let travel = (self.len - self.thumb_len).max(1.0);
        ((along - grab - self.start) / travel * self.max).clamp(0.0, self.max)
    }

    /// A press at `along`: on the thumb it grabs it where pressed; on the
    /// track it pages toward the press. Returns (new offset, grab).
    pub fn press(&self, along: f64, current: f64) -> (f64, f64) {
        // Layout clamps the painted offset after resize/content shrink. Interact
        // with that visible thumb, not an older out-of-range stored offset.
        let current = current.clamp(0.0, self.max);
        if along >= self.thumb_start && along <= self.thumb_start + self.thumb_len {
            return (current, along - self.thumb_start);
        }
        let next = if along < self.thumb_start { current - self.page } else { current + self.page }.clamp(0.0, self.max);
        let travel = (self.len - self.thumb_len).max(0.0);
        let thumb_start = self.start + if self.max > 0.0 { travel * next / self.max } else { 0.0 };
        (next, along - thumb_start)
    }
}

/// An active thumb drag.
#[derive(Clone, Debug, PartialEq)]
pub struct BarDrag {
    pub key: String,
    pub vertical: bool,
    pub track: Track,
    pub grab: f64,
}

impl BarDrag {
    /// Starts a drag for a press at `rel` on a `ScrollBar` hit and applies
    /// any paging to `state`. Returns None for other targets.
    pub fn press(target: &HitTarget, rel: DVec2, state: &mut RowState) -> Option<Self> {
        let HitTarget::ScrollBar { key, vertical, track } = target else { return None };
        let along = if *vertical { rel.y } else { rel.x };
        let offsets = if *vertical { &mut state.scroll_y } else { &mut state.scroll_x };
        let current = offsets.get(key).copied().unwrap_or(0.0);
        let (next, grab) = track.press(along, current);
        offsets.insert(key.clone(), next);
        state.dragging_bar = Some(key.clone());
        Some(BarDrag { key: key.clone(), vertical: *vertical, track: *track, grab })
    }

    /// Moves the thumb to the pointer at `rel`; returns true when the offset changed.
    pub fn drag_to(&self, rel: DVec2, state: &mut RowState) -> bool {
        let along = if self.vertical { rel.y } else { rel.x };
        let next = self.track.offset_at(along, self.grab);
        let offsets = if self.vertical { &mut state.scroll_y } else { &mut state.scroll_x };
        let current = offsets.get(&self.key).copied().unwrap_or(0.0);
        if (next - current).abs() < 0.01 {
            return false;
        }
        offsets.insert(self.key.clone(), next);
        true
    }
}

/// The hover key of a bar (`RowState::hover_hit`).
pub fn hover_key(key: &str) -> String {
    format!("scrollbar:{key}")
}

/// Draws the bars and side fades of `view` (absolute) above the region's content and
/// registers the bar hits. `bg` is the region's background for the fades (None: no fades).
pub fn draw(cx: &mut Cx2d, d: &mut Draw, view: Rect, bg: Option<Vec4>, x: Option<Axis>, y: Option<Axis>) {
    let x = x.filter(|a| a.max > 0.0);
    let y = y.filter(|a| a.max > 0.0);
    if x.is_none() && y.is_none() {
        return;
    }
    let mut rects: Vec<(Rect, Vec4, f64)> = Vec::new();
    let mut hits: Vec<(Rect, HitTarget)> = Vec::new();
    if let (Some(bg), Some(axis)) = (bg, &x) {
        // 1px inside the region so a bordered pane keeps its border.
        let (top, h) = (view.pos.y + 1.0, (view.size.y - 2.0).max(0.0));
        if axis.offset < axis.max - 0.5 {
            fade(&mut rects, rect_xywh(view.pos.x + view.size.x - 1.0 - FADE, top, FADE, h), bg, true);
        }
        if axis.offset > 0.5 {
            fade(&mut rects, rect_xywh(view.pos.x + 1.0, top, FADE, h), bg, false);
        }
    }
    if let Some(axis) = &x {
        let reserve = if y.is_some() { BAR } else { 0.0 };
        let strip = rect_xywh(view.pos.x, view.pos.y + view.size.y - BAR, view.size.x - reserve, BAR);
        let track = Track::new(strip.pos.x + INSET - d.canvas.origin.x, strip.size.x - 2.0 * INSET, view.size.x, axis.offset, axis.max);
        bar(d, &mut rects, &mut hits, strip, axis.key, false, track);
    }
    if let Some(axis) = &y {
        let reserve = if x.is_some() { BAR } else { 0.0 };
        let strip = rect_xywh(view.pos.x + view.size.x - BAR, view.pos.y, BAR, view.size.y - reserve);
        let track = Track::new(strip.pos.y + INSET - d.canvas.origin.y, strip.size.y - 2.0 * INSET, view.size.y, axis.offset, axis.max);
        bar(d, &mut rects, &mut hits, strip, axis.key, true, track);
    }
    d.kit.rects_on_top(cx, |kit, cx| {
        for (r, color, radius) in &rects {
            kit.rect(cx, *r, *color, *radius);
        }
    });
    for (r, target) in hits {
        d.canvas.add_hit(r, target);
    }
}

fn bar(d: &Draw, rects: &mut Vec<(Rect, Vec4, f64)>, hits: &mut Vec<(Rect, HitTarget)>, strip: Rect, key: &str, vertical: bool, track: Track) {
    let active = d.state.hover_hit.as_deref() == Some(hover_key(key).as_str()) || d.state.dragging_bar.as_deref() == Some(key);
    let thickness = if active { 6.0 } else { 4.0 };
    let alpha = if active { 0.8 } else { 0.5 };
    let muted = d.look.palette.muted;
    let start = track.thumb_start + if vertical { d.canvas.origin.y } else { d.canvas.origin.x };
    let thumb = if vertical {
        rect_xywh(strip.pos.x + strip.size.x - INSET - thickness, start, thickness, track.thumb_len)
    } else {
        rect_xywh(start, strip.pos.y + strip.size.y - INSET - thickness, track.thumb_len, thickness)
    };
    if active {
        rects.push((strip, with_alpha(muted, 0.12), 0.0));
    }
    rects.push((thumb, with_alpha(muted, alpha), thickness * 0.5));
    hits.push((strip, HitTarget::ScrollBar { key: key.to_owned(), vertical, track }));
}

/// A horizontal fade to `bg`: opaque at the region edge (`right`: the right edge).
fn fade(rects: &mut Vec<(Rect, Vec4, f64)>, r: Rect, bg: Vec4, right: bool) {
    let step = r.size.x / FADE_STEPS as f64;
    for i in 0..FADE_STEPS {
        let t = (i + 1) as f32 / FADE_STEPS as f32;
        let x = if right { r.pos.x + step * i as f64 } else { r.pos.x + r.size.x - step * (i + 1) as f64 };
        rects.push((rect_xywh(x, r.pos.y, step + 0.5, r.size.y), with_alpha(bg, bg.w * t * t * 0.9), 0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thumb_spans_the_track_in_proportion_to_the_view() {
        let track = Track::new(0.0, 100.0, 100.0, 0.0, 100.0);
        assert_eq!(track.thumb_len, 50.0);
        assert_eq!(track.thumb_start, 0.0);
        let end = Track::new(0.0, 100.0, 100.0, 100.0, 100.0);
        assert_eq!(end.thumb_start, 50.0);
        // A huge content keeps a grabbable thumb.
        assert_eq!(Track::new(0.0, 100.0, 10.0, 0.0, 10_000.0).thumb_len, MIN_THUMB);
    }

    #[test]
    fn dragging_maps_the_thumb_travel_onto_the_offset() {
        let track = Track::new(10.0, 100.0, 100.0, 0.0, 100.0);
        // Grabbed 5px into the thumb; moving the grab point 25px is half the travel.
        assert_eq!(track.offset_at(10.0 + 5.0 + 25.0, 5.0), 50.0);
        assert_eq!(track.offset_at(-50.0, 5.0), 0.0);
        assert_eq!(track.offset_at(500.0, 5.0), 100.0);
    }

    #[test]
    fn track_clicks_page_and_thumb_clicks_grab() {
        let track = Track::new(0.0, 100.0, 40.0, 0.0, 200.0);
        let (offset, grab) = track.press(track.thumb_start + 3.0, 0.0);
        assert_eq!((offset, grab), (0.0, 3.0));
        let (offset, _) = track.press(95.0, 0.0);
        assert_eq!(offset, 40.0);
        let (offset, _) = Track::new(0.0, 100.0, 40.0, 200.0, 200.0).press(1.0, 200.0);
        assert_eq!(offset, 160.0);
    }

    #[test]
    fn pressing_after_content_shrinks_uses_the_visible_offset() {
        let track = Track::new(0.0, 100.0, 40.0, 500.0, 200.0);
        assert_eq!(track.press(track.thumb_start + 3.0, 500.0), (200.0, 3.0));
        assert_eq!(track.press(1.0, 500.0).0, 160.0);
    }
}
