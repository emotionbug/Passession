//! The draw context every timeline painter receives: the kit, the widget's
//! canvas (selectable runs + hit regions), the resolved look, and the row's
//! persistent UI state (expansion, scroll offsets, copy feedback, reveal
//! pacing, selection), which lives outside PortalList items so it survives
//! index shifts when older history is prepended.

use super::canvas::{Canvas, ChatKit};
use super::reveal::Reveal;
use crate::theme::{Palette, Typography};
use makepad_widgets::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

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
    /// Measured timeline rows publish prepared snapshots only during measurement.
    pub managed_preparation: bool,
    pub preparing: bool,
    pub markdown_worker: super::markdown::worker::MarkdownWorker,
    pub markdown_presentation: super::markdown::view::MarkdownPresentation,
    /// Expanded badges / cards by key (tool, thinking, plan, group…).
    pub expanded: HashMap<String, bool>,
    /// Horizontal / vertical scroll offsets of code panes and capped details.
    pub scroll_x: HashMap<String, f64>,
    pub scroll_y: HashMap<String, f64>,
    /// Content extents measured at the last draw (max scroll).
    pub extent_x: HashMap<String, f64>,
    pub extent_y: HashMap<String, f64>,
    /// Copy glyph shows a check for 1.5s (`TurnCopyButton`).
    pub copied: HashMap<String, f64>,
    pub response_copy: super::layout::ResponseCopyCache,
    pub hovered: bool,
    pub hover_link: Option<std::sync::Arc<str>>,
    pub hover_hit: Option<String>,
    /// Paced reveal of streaming text by key.
    pub reveals: HashMap<String, Reveal>,
    /// Parsed Markdown keyed by source text hash.
    pub markdown: HashMap<u64, Rc<Vec<super::markdown::model::Block>>>,
    /// Stable split head cache (streaming).
    pub stable_head: Option<(u64, Rc<Vec<super::markdown::model::Block>>)>,
    /// Prepared paragraph geometry retained by immutable parsed-block identity.
    pub markdown_paragraphs: super::markdown::view::ParagraphCache,
    /// Only the current growing parse is retained, never older live snapshots.
    pub markdown_tail: Option<(u64, Rc<Vec<super::markdown::model::Block>>)>,
    /// Worker results belong to this exact row and sequential code-block key.
    pub fence_highlights: HashMap<String, super::highlight::FenceRequest>,
    pub selection: Option<(usize, usize)>,
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

impl RowState {
    /// The shown expansion; the first read records `default` so a later tap
    /// flips exactly what was displayed.
    pub fn is_expanded(&mut self, key: &str, default: bool) -> bool {
        *self.expanded.entry(key.to_owned()).or_insert(default)
    }

    pub fn toggle(&mut self, key: &str) {
        let next = !self.expanded.get(key).copied().unwrap_or(false);
        self.expanded.insert(key.to_owned(), next);
    }

    pub fn copied_now(&self, key: &str, now: f64) -> bool {
        self.copied.get(key).is_some_and(|at| now - at < 1.5)
    }

    /// The conversation returns to the screen (tab, pane or app shown again):
    /// text received meanwhile is shown whole, like a first sight. Only
    /// growth the reader watches is paced.
    pub fn release_reveal_backlog(&mut self) {
        self.markdown_worker.release_backlog();
        self.reveals.clear();
    }
}

/// One draw call's context.
pub struct Draw<'a> {
    pub kit: &'a mut ChatKit,
    pub canvas: &'a mut Canvas,
    pub look: Look,
    pub state: &'a mut RowState,
    /// Seconds (Cx::seconds_since_app_start) for copy feedback / pacing.
    pub now: f64,
    /// Area that a pacing/feedback redraw targets.
    pub area: Area,
    /// Whether anything animated was drawn (spinner, pacing, copy check).
    pub animating: bool,
}

impl Draw<'_> {
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
}

pub fn now_seconds() -> f64 {
    crate::common::clock::now_ms() as f64 / 1000.0
}
