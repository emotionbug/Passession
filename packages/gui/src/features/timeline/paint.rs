//! The draw context every timeline painter receives: the kit, the widget's
//! canvas (selectable runs + hit regions), the resolved look, and the row's
//! persistent UI state (expansion, scroll offsets, copy feedback, reveal
//! pacing, selection), which lives outside PortalList items so it survives
//! index shifts when older history is prepended.

use super::canvas::{Canvas, ChatKit};
use super::indicator::{Indicator, Indicators};
use super::reveal::Reveal;
use crate::common::transition::tokens;
use crate::theme::{Palette, Typography};
use makepad_widgets::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Original Android/iOS renderer branch (`isNative`); desktop follows web.
pub const MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub palette: Palette,
    pub typo: Typography,
    /// Panel width below 600.
    pub compact: bool,
    pub mobile: bool,
    /// MediaQuery width < 720 → 12, else 16 (listContentContainer).
    pub list_inset: f64,
    /// `preferences.syntaxTheme` (default "one").
    pub syntax: paseo_highlight::SyntaxThemeId,
}

impl Look {
    pub fn dark(&self) -> bool {
        self.palette.is_dark
    }
    pub fn ui(&self, px: f64) -> f64 {
        self.typo.ui_size(px)
    }
    pub fn content(&self) -> f64 {
        self.typo.content_font_size
    }
    pub fn code(&self) -> f64 {
        self.typo.code_font_size
    }
}

/// Per-row UI state keyed by the row's stable identity.
#[derive(Default)]
pub struct RowState {
    /// External presentation changes invalidate retained row measurements and paint.
    pub revision: u64,
    /// Measured timeline rows publish prepared snapshots only during measurement.
    pub managed_preparation: bool,
    pub preparing: bool,
    pub presentation: RowPresentation,
    /// Eviction never invalidates the retained intrinsic height or selection.
    pub presentation_restoring: bool,
    pub measured_width: f64,
    pub presentation_used: u64,
    /// Expanded badges / cards by key (tool, thinking, plan, group…).
    pub expanded: HashMap<String, bool>,
    /// Horizontal / vertical scroll offsets of code panes and capped details.
    pub scroll_x: HashMap<String, f64>,
    pub scroll_y: HashMap<String, f64>,
    /// Content extents measured at the last draw (max scroll).
    pub extent_x: HashMap<String, f64>,
    pub extent_y: HashMap<String, f64>,
    /// Copy glyph shows a check for 1.5s (`TurnCopyButton`).
    pub copied: HashMap<String, Instant>,
    pub hovered: bool,
    pub hover_link: Option<std::sync::Arc<str>>,
    pub hover_hit: Option<String>,
    /// The scroll bar key whose thumb is being dragged (drawn active).
    pub dragging_bar: Option<String>,
    /// Paced reveal of streaming text by key.
    pub reveals: HashMap<String, Reveal>,
    pub selection: Option<(usize, usize)>,
    pub selection_source: Option<u64>,
    /// Permission card / question form state.
    pub permission: Option<super::permission::CardState>,
    /// Keys of pending menus (rewind / fork) that keep their glyph active.
    pub menu_open: HashSet<String>,
    pub menu_busy: HashSet<String>,
    /// Last drawn height (sticky-bottom alignment of a tall last row).
    pub height: f64,
    /// Absolute y of the current find match drawn this frame (scroll-to-match).
    pub find_y: Option<f64>,
}

/// Disposable content ownership, separate from persistent interaction state.
#[derive(Default)]
pub struct RowPresentation {
    pub markdown_worker: super::markdown::worker::MarkdownWorker,
    pub markdown_presentation: super::markdown::view::MarkdownPresentation,
    pub response_copy: super::layout::ResponseCopyCache,
    pub markdown: HashMap<u64, Rc<Vec<super::markdown::model::Block>>>,
    pub stable_head: Option<(u64, Rc<Vec<super::markdown::model::Block>>)>,
    pub markdown_paragraphs: super::markdown::view::ParagraphCache,
    pub markdown_tail: Option<(u64, Rc<Vec<super::markdown::model::Block>>)>,
    pub fence_highlights: HashMap<String, super::highlight::FenceRequest>,
    pub mermaid: HashMap<String, super::mermaid::Request>,
}

impl std::ops::Deref for RowState {
    type Target = RowPresentation;
    fn deref(&self) -> &Self::Target {
        &self.presentation
    }
}

impl std::ops::DerefMut for RowState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.presentation
    }
}

impl RowState {
    pub fn evict_presentation(&mut self) {
        self.presentation = RowPresentation::default();
        self.presentation_restoring = true;
        self.reveals.clear();
        self.extent_x.clear();
        self.extent_y.clear();
        self.hover_link = None;
        self.hover_hit = None;
        self.invalidate();
    }

    pub fn preparing_height(&mut self, height: f64, width: f64, pending: bool) -> f64 {
        let height = if self.presentation_restoring && pending && self.measured_width == width {
            self.height.max(height)
        } else {
            height
        };
        if !pending {
            self.presentation_restoring = false;
        }
        self.measured_width = width;
        self.height = height;
        height
    }

    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// The shown expansion; the first read records `default` so a later tap
    /// flips exactly what was displayed.
    pub fn is_expanded(&mut self, key: &str, default: bool) -> bool {
        *self.expanded.entry(key.to_owned()).or_insert(default)
    }

    pub fn toggle(&mut self, key: &str) {
        let next = !self.expanded.get(key).copied().unwrap_or(false);
        self.expanded.insert(key.to_owned(), next);
        self.markdown_presentation.invalidate_interaction();
        self.invalidate();
    }

    pub fn copy_expiry(&self, key: &str, hold: Duration) -> Option<Instant> {
        let expiry = *self.copied.get(key)? + hold;
        (expiry > Instant::now()).then_some(expiry)
    }

    /// The conversation returns to the screen (tab, pane or app shown again):
    /// text received meanwhile is shown whole, like a first sight. Only
    /// growth the reader watches is paced.
    pub fn release_reveal_backlog(&mut self) {
        self.markdown_worker.release_backlog();
        self.reveals.clear();
        // Unconditional: a worker or image result that redrew this row's list
        // while the conversation was off screen was consumed by a draw that
        // skipped the row, so its retained height and paint are stale.
        self.invalidate();
    }
}

/// One draw call's context.
pub struct Draw<'a> {
    pub kit: &'a mut ChatKit,
    pub canvas: &'a mut Canvas,
    pub look: Look,
    pub state: &'a mut RowState,
    /// Wall-clock seconds for streaming preparation and motion.
    pub now: f64,
    /// Owner area for asynchronous presentation.
    pub area: Area,
    /// Explicit row owner, including measurement before the first area exists.
    pub owner: Option<DrawListId>,
    /// Streaming preparation or sheet motion needs another layout.
    pub animating: bool,
    pub(crate) indicators: Option<&'a mut Indicators>,
}

impl Draw<'_> {
    pub(super) fn indicator(&mut self, cx: &mut Cx2d, indicator: Indicator) {
        if self.dry() {
            return;
        }
        let inner = self
            .canvas
            .clip_stack
            .last()
            .map(|r| r.translate(self.canvas.origin));
        let clip = match (inner, self.canvas.viewport) {
            (Some(inner), Some(outer)) => Some(inner.clip((outer.pos, outer.pos + outer.size))),
            (inner, outer) => inner.or(outer),
        };
        if let Some(indicators) = &mut self.indicators {
            indicators.push(cx, indicator, clip);
        } else {
            indicator.paint(cx, self.kit, clip, &mut Area::Empty, None);
        }
    }

    pub(super) fn spinner(&mut self, cx: &mut Cx2d, rect: Rect, color: Vec4) {
        self.kit.spinner(cx, rect, color);
    }

    pub(super) fn copy_icon(
        &mut self,
        cx: &mut Cx2d,
        key: &str,
        rect: Rect,
        size: f64,
        color: Vec4,
    ) {
        if self.dry() {
            return;
        }
        if let Some(expiry) = self.state.copy_expiry(key, tokens::COPY_FEEDBACK) {
            self.indicator(
                cx,
                Indicator::Copy {
                    rect,
                    size,
                    color,
                    expiry,
                },
            );
        } else {
            self.kit.icon(cx, "copy", rect, size, color);
        }
    }

    /// Runs `f` as a measuring pass (layout only; nothing drawn/registered).
    pub fn measure<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let (kit, canvas) = (self.kit.dry, self.canvas.dry);
        self.kit.dry = true;
        self.canvas.dry = true;
        let result = f(self);
        self.kit.dry = kit;
        self.canvas.dry = canvas;
        result
    }

    pub fn dry(&self) -> bool {
        self.kit.dry
    }

    /// The draw list a background result repaints: the explicit row owner,
    /// its area, or the current list before either is available.
    pub fn result_owner(&self, cx: &Cx2d) -> Option<DrawListId> {
        self.owner
            .or_else(|| self.area.draw_list_id())
            .or_else(|| cx.get_current_draw_list_id())
    }
}

pub fn now_seconds() -> f64 {
    crate::common::clock::now_ms() as f64 / 1000.0
}

#[cfg(test)]
mod presentation_tests {
    use super::*;

    #[test]
    fn eviction_and_restore_preserve_measured_height_and_interaction() {
        let mut state = RowState::default();
        state.preparing_height(512.0, 600.0, false);
        state.expanded.insert("thinking".into(), true);
        state.selection = Some((4, 10));
        state.selection_source = Some(123);
        state.markdown.insert(1, Rc::new(Vec::new()));
        state.evict_presentation();
        assert!(state.markdown.is_empty());
        assert_eq!(state.preparing_height(0.0, 600.0, true), 512.0);
        assert_eq!(state.preparing_height(512.0, 600.0, false), 512.0);
        assert!(!state.presentation_restoring);
        assert_eq!(state.expanded.get("thinking"), Some(&true));
        assert_eq!(state.selection, Some((4, 10)));
        assert_eq!(state.selection_source, Some(123));
    }
}
