//! The remote folder picker (Flutter `RemoteDirectoryBrowser(directoriesOnly: true,
//! onDirectorySelected:)`), shaped like an OS "choose folder" dialog: Back / Forward / Up, a
//! clickable breadcrumb of the absolute path (or an editable path field), a flat list of the
//! shown folder's subfolders that open on click, and a footer naming the folder that
//! "Choose this folder" picks. It lists the chosen host's directories, never the client's.
//! Mounted inside the caller's own modal (WorkspaceAutomation New Tab / Add Project).

use makepad_widgets::*;

use crate::common::overlay::tooltip_at;
use crate::common::paint::{Font, HitMap};
use crate::common::{UiContext, clock};
use crate::theme::{Palette, Typography, fade, with_alpha};

use super::editor::field::{FieldEvent, TextField};
use super::explorer::ExplorerState;
use super::i18n::t;
use super::kit::{self, Kit, rect, spec};
use super::path::Style;
use super::store;
use super::ui;

live_design! {
    use link::theme::*;
    use link::widgets::*;
    use crate::features::files::kit::FilesKit;

    pub RemoteFolderPickerBase = {{RemoteFolderPicker}} {}
    pub RemoteFolderPicker = <RemoteFolderPickerBase> {
        width: Fill, height: Fill
        kit: <FilesKit> {}
    }
}

/// Flutter `RemoteDirectoryBrowser(directoriesOnly: true, onDirectorySelected:)`: lists the
/// chosen host's directories (never the client's file system).
#[derive(Clone, Debug, DefaultNone)]
pub enum RemoteFolderPickerAction {
    None,
    /// The absolute remote directory the user chose.
    Selected(String),
    /// Cancel (or Escape when the host does not claim it).
    Cancelled,
}

/// One breadcrumb segment: its label and the directory it opens.
#[derive(Clone, Debug, PartialEq)]
pub struct Crumb {
    pub label: String,
    pub path: String,
}

/// The breadcrumb of a picker directory, outermost first: the root (`/`, `C:`), then one
/// segment per folder. Resolved paths never shorten to `~`; only unresolved host aliases do.
pub fn breadcrumb(directory: &str) -> Vec<Crumb> {
    let style = Style::of(directory);
    let mut current = if directory.starts_with("~/") { directory.to_owned() } else { style.normalize(directory) };
    let mut crumbs = Vec::new();
    loop {
        if current == "~" {
            crumbs.push(Crumb { label: "~".to_owned(), path: current });
            break;
        }
        if !style.is_absolute(&current) && current == "." {
            break;
        }
        let parent = style.dirname(&current);
        if parent == current {
            let trimmed = current.trim_end_matches(['/', '\\']);
            let label = if trimmed.is_empty() { current.clone() } else { trimmed.to_owned() };
            crumbs.push(Crumb { label, path: current });
            break;
        }
        crumbs.push(Crumb { label: style.basename(&current), path: current });
        current = parent;
    }
    crumbs.reverse();
    crumbs
}

/// How many leading crumbs collapse into the "…" button so the rest fit `available` (crumb
/// `widths`, the "…" button's width, and the separator drawn between items). The last crumb
/// always stays; when even it alone does not fit, every other crumb collapses and it shrinks.
pub fn collapsed_count(widths: &[f64], available: f64, more: f64, separator: f64) -> usize {
    let count = widths.len();
    if count <= 1 {
        return 0;
    }
    let row = |shown: &[f64]| shown.iter().sum::<f64>() + separator * (shown.len() - 1) as f64;
    if row(widths) <= available {
        return 0;
    }
    (1..count).find(|&hidden| more + separator + row(&widths[hidden..]) <= available).unwrap_or(count - 1)
}

/// `text` shortened in the middle to `keep` characters plus "…", keeping more of the end (a
/// path's folder names matter more than its root).
pub fn middle_ellipsis(text: &str, keep: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= keep {
        return text.to_owned();
    }
    let head = keep / 3;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

#[derive(Clone, Debug, PartialEq)]
enum Hot {
    Back,
    Forward,
    Up,
    Crumb(usize),
    More,
    MoreCard,
    MoreItem(usize),
    Edit,
    Field,
    Hidden,
    Refresh,
    List,
    Row(usize),
    ErrorUp,
    ErrorBack,
    Retry,
    Cancel,
    Choose,
}

impl Hot {
    fn clickable(&self) -> bool {
        !matches!(self, Hot::List | Hot::MoreCard | Hot::Field)
    }
}

enum PickerEvent {
    None,
    Selected(String),
    Cancelled,
}

/// The picker's chrome and list (immediate mode over the shared [`ExplorerState`]).
struct PickerView {
    hits: HitMap<Hot>,
    /// Drawn rects of the tooltip-bearing controls (drawn coordinates).
    tips: Vec<(Hot, Rect)>,
    hovered: Option<Hot>,
    pressed: Option<Hot>,
    scroll_y: f64,
    list_rect: Rect,
    row_height: f64,
    thumb_drag: Option<(f64, f64)>,
    touch_scroll: makepad_widgets::scroll_bars::TouchScroll,
    /// The keyboard-highlighted row.
    highlight: Option<usize>,
    /// The path field replaces the breadcrumb.
    editing: bool,
    field: TextField,
    more_open: bool,
    more_anchor: Rect,
    crumbs: Vec<Crumb>,
    hidden_crumbs: usize,
    /// The directory last drawn; a new one starts scrolled to the top.
    shown: String,
    /// The owner moves key focus to the picker (path field opened).
    focus_requested: bool,
}

impl Default for PickerView {
    fn default() -> Self {
        PickerView {
            hits: HitMap::default(),
            tips: Vec::new(),
            hovered: None,
            pressed: None,
            scroll_y: 0.0,
            list_rect: Rect::default(),
            row_height: 36.0,
            thumb_drag: None,
            touch_scroll: Default::default(),
            highlight: None,
            editing: false,
            field: TextField::new(""),
            more_open: false,
            more_anchor: Rect::default(),
            crumbs: Vec::new(),
            hidden_crumbs: 0,
            shown: String::new(),
            focus_requested: false,
        }
    }
}

const LIST_PADDING: f64 = 4.0;

/// Theme and responsive-layout inputs shared by one picker frame.
struct PickerLook<'a> {
    palette: &'a Palette,
    typography: &'a Typography,
    compact: bool,
}

impl PickerView {
    fn interaction(&self, hot: &Hot, enabled: bool, selected: bool) -> ui::Interaction {
        ui::Interaction {
            hovered: self.hovered.as_ref() == Some(hot),
            pressed: self.pressed.as_ref() == Some(hot),
            enabled,
            selected,
        }
    }

    fn content_height(&self, rows: usize) -> f64 {
        LIST_PADDING * 2.0 + rows as f64 * self.row_height
    }

    #[allow(clippy::too_many_arguments)]
    fn nav_button(&mut self, cx: &mut Cx2d, kit: &mut Kit, frame: Rect, icon: &str, hot: Hot, enabled: bool, selected: bool, compact: bool, p: &Palette) {
        let st = self.interaction(&hot, enabled, selected);
        if st.selected || (st.enabled && (st.hovered || st.pressed)) {
            kit.fill(cx, frame, p.interaction_highlight, 6.0);
        }
        let color = if enabled { p.muted } else { fade(p.muted, 0.4) };
        kit.icon(cx, frame, icon, if compact { 20.0 } else { 16.0 }, color);
        if enabled {
            self.hits.push(frame, hot.clone());
        }
        self.tips.push((hot, frame));
    }

    /// Adopt a resolved location without overwriting a path the user is currently typing.
    fn sync_directory(&mut self, directory: &str) {
        if self.shown == directory {
            return;
        }
        if self.editing && self.field.text() == self.shown {
            self.field.set_text(directory);
            self.field.select_all();
        }
        self.shown = directory.to_owned();
        self.scroll_y = 0.0;
        self.highlight = None;
        self.more_open = false;
    }

    fn draw(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, state: &ExplorerState, look: &PickerLook, widget: Area) {
        let p = look.palette;
        let ty = look.typography;
        let compact = look.compact;
        self.hits.clear();
        self.tips.clear();
        // Boxes (button fills, hover chips) must sit under icons and text drawn earlier in
        // the frame: open this painter's box batch first.
        kit.painter.new_layer(cx);
        let directory = state.current_directory();
        self.sync_directory(&directory);
        let pad = if compact { 12.0 } else { 16.0 };
        let button = if compact { 40.0 } else { 28.0 };
        let base = ty.ui_size(14.0);
        let small = ty.ui_size(12.0);
        let crumb_font = ty.ui_size(13.0);

        // Navigation bar: Back / Forward / Up, the breadcrumb (or path field), Edit / Hidden /
        // Refresh. Narrow widths move the breadcrumb to its own line.
        let two_rows = compact || area.size.x < 480.0;
        let row_h = button + 12.0;
        let y = area.pos.y + 6.0;
        let mut x = area.pos.x + pad - 6.0;
        for (icon, hot, enabled) in [
            ("arrow-left", Hot::Back, state.can_go_back()),
            ("arrow-right", Hot::Forward, state.can_go_forward()),
            ("arrow-up", Hot::Up, state.can_go_up()),
        ] {
            self.nav_button(cx, kit, rect(x, y, button, button), icon, hot, enabled, false, compact, p);
            x += button + 2.0;
        }
        let leading_end = x;
        let mut right = area.pos.x + area.size.x - pad + 6.0;
        let busy = state.refreshing() || state.is_loading(state.path());
        right -= button;
        let refresh = rect(right, y, button, button);
        if busy {
            ui::spinner(cx, kit, refresh, if compact { 18.0 } else { 14.0 }, p.muted, clock::phase(900));
            clock::register(cx, widget);
            self.tips.push((Hot::Refresh, refresh));
        } else {
            self.nav_button(cx, kit, refresh, "rotate-cw", Hot::Refresh, true, false, compact, p);
        }
        right -= button + 2.0;
        let hidden_icon = if state.show_hidden() { "eye" } else { "eye-off" };
        self.nav_button(cx, kit, rect(right, y, button, button), hidden_icon, Hot::Hidden, true, false, compact, p);
        right -= button + 2.0;
        let editing = self.editing;
        self.nav_button(cx, kit, rect(right, y, button, button), "pencil", Hot::Edit, true, editing, compact, p);
        let path_region = if two_rows {
            rect(area.pos.x + pad, area.pos.y + row_h, area.size.x - pad * 2.0, button)
        } else {
            rect(leading_end + 8.0, y, right - 8.0 - (leading_end + 8.0), button)
        };
        let nav_h = if two_rows { row_h + button + 8.0 } else { row_h };
        if self.editing {
            self.field.draw(cx, kit, path_region, &t("workspace.folderPicker.pathPlaceholder"), None, crumb_font, p, widget);
            self.hits.push(path_region, Hot::Field);
        } else {
            self.draw_breadcrumb(cx, kit, path_region, state, p, crumb_font, compact);
        }
        let nav_bottom = area.pos.y + nav_h;
        kit.hline(cx, area.pos.x, nav_bottom - 1.0, area.size.x, p.border);

        // Footer: the folder "Choose" picks, always visible below the list.
        let name_h = (base * 1.4).ceil();
        let path_h = (small * 1.4).ceil();
        let action_h = if compact { 44.0 } else { ui::ButtonSize::Sm.height() };
        // Keep the footer above the system bar / keyboard if the host sheet reaches under it.
        let occluded = if compact {
            let insets = crate::shell::platform::safe_insets();
            let visible_bottom = cx.current_pass_size().y - insets.bottom_occlusion();
            (area.pos.y + area.size.y - visible_bottom).max(0.0)
        } else {
            0.0
        };
        let footer_h = if compact {
            12.0 + name_h + path_h + 10.0 + action_h + 12.0 + occluded
        } else {
            (name_h + path_h).max(action_h) + 24.0
        };
        let footer_top = (area.pos.y + area.size.y - footer_h).max(nav_bottom);
        kit.hline(cx, area.pos.x, footer_top, area.size.x, p.border);

        // List.
        let list = rect(area.pos.x, nav_bottom, area.size.x, (footer_top - nav_bottom).max(0.0));
        self.list_rect = list;
        self.hits.push(list, Hot::List);
        self.row_height = if compact { 48.0 } else { (base * 1.4 + 14.0).max(36.0).round() };
        let mut list_top = list.pos.y;
        if let Some(error) = state.stale_root_error() {
            let width = (list.size.x - pad * 2.0).max(1.0);
            let h = kit.paragraph_height(cx, width, spec(Font::Regular, small, p.destructive), error);
            kit.paragraph(cx, dvec2(list.pos.x + pad, list_top + 8.0), width, spec(Font::Regular, small, p.destructive), error);
            list_top += h + 12.0;
        }
        let list = rect(list.pos.x, list_top, list.size.x, (list.pos.y + list.size.y - list_top).max(0.0));
        self.list_rect = list;
        if let Some(error) = state.root_error() {
            self.draw_error(cx, kit, list, state, error, p, ty, compact);
        } else if state.initial_loading() {
            let cy = list.pos.y + list.size.y / 2.0;
            ui::spinner(cx, kit, rect(list.pos.x + list.size.x / 2.0 - 10.0, cy - 26.0, 20.0, 20.0), 20.0, p.muted, clock::phase(900));
            clock::register(cx, widget);
            kit.text_in(cx, rect(list.pos.x + pad, cy + 2.0, list.size.x - pad * 2.0, base * 1.4), spec(Font::Regular, base, p.muted), &t("workspace.folderPicker.loading"), 0.5);
        } else if state.rows().is_empty() {
            let title = t(if state.show_hidden() { "workspace.folderPicker.emptyTitle" } else { "workspace.folderPicker.emptyHiddenTitle" });
            let hint = t("workspace.folderPicker.emptyHint");
            let cy = list.pos.y + list.size.y / 2.0;
            kit.icon(cx, rect(list.pos.x, cy - 50.0, list.size.x, 28.0), "folder-open", 28.0, p.extra_muted);
            kit.text_in(cx, rect(list.pos.x + pad, cy - 16.0, list.size.x - pad * 2.0, base * 1.4), spec(Font::Medium, base, p.foreground), &title, 0.5);
            kit.text_in(cx, rect(list.pos.x + pad, cy - 16.0 + base * 1.4 + 4.0, list.size.x - pad * 2.0, small * 1.4), spec(Font::Regular, small, p.muted), &hint, 0.5);
        } else {
            self.draw_rows(cx, kit, list, state, p, base, pad, compact);
        }

        self.draw_footer(cx, kit, rect(area.pos.x, footer_top, area.size.x, footer_h - occluded), &directory, p, ty, compact, pad, action_h);

        if self.more_open {
            self.draw_more_menu(cx, kit, area, p, crumb_font, compact);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_breadcrumb(&mut self, cx: &mut Cx2d, kit: &mut Kit, region: Rect, state: &ExplorerState, p: &Palette, font: f64, compact: bool) {
        self.crumbs = breadcrumb(&state.current_directory());
        let count = self.crumbs.len();
        let separator = 16.0;
        let more_w = if compact { 36.0 } else { 28.0 };
        let last_font = |index: usize| if index + 1 == count { Font::Medium } else { Font::Regular };
        let widths: Vec<f64> = self
            .crumbs
            .iter()
            .enumerate()
            .map(|(index, crumb)| kit.measure(cx, spec(last_font(index), font, p.foreground), &crumb.label).x.ceil() + 12.0)
            .collect();
        self.hidden_crumbs = collapsed_count(&widths, region.size.x, more_w, separator);
        let end = region.pos.x + region.size.x;
        let mut x = region.pos.x;
        let chip_h = region.size.y.min(if compact { 36.0 } else { 26.0 });
        let chip_y = region.pos.y + (region.size.y - chip_h) / 2.0;
        if self.hidden_crumbs > 0 {
            let r = rect(x, chip_y, more_w, chip_h);
            let st = self.interaction(&Hot::More, true, self.more_open);
            if st.selected || st.hovered || st.pressed {
                kit.fill(cx, r, p.interaction_highlight, 6.0);
            }
            kit.icon(cx, r, "ellipsis", 16.0, p.muted);
            self.hits.push(r, Hot::More);
            self.tips.push((Hot::More, r));
            self.more_anchor = r;
            x += more_w;
            kit.icon(cx, rect(x, region.pos.y, separator, region.size.y), "chevron-right", 12.0, p.extra_muted);
            x += separator;
        }
        for (index, width) in widths.iter().enumerate().skip(self.hidden_crumbs) {
            let last = index + 1 == count;
            let w = width.min(end - x);
            if w <= 12.0 {
                break;
            }
            let r = rect(x, chip_y, w, chip_h);
            let hot = Hot::Crumb(index);
            let st = self.interaction(&hot, !last, false);
            if !last && (st.hovered || st.pressed) {
                kit.fill(cx, r, p.interaction_highlight, 6.0);
            }
            let color = if last || st.hovered { p.foreground } else { p.muted };
            kit.text_in(cx, rect(r.pos.x + 6.0, r.pos.y, r.size.x - 12.0, r.size.y), spec(last_font(index), font, color), &self.crumbs[index].label, 0.0);
            if !last {
                self.hits.push(r, hot);
            }
            x += w;
            if !last {
                if x + separator > end {
                    break;
                }
                kit.icon(cx, rect(x, region.pos.y, separator, region.size.y), "chevron-right", 12.0, p.extra_muted);
                x += separator;
            }
        }
    }

    /// The "…" popover: the collapsed ancestors, nearest first.
    fn draw_more_menu(&mut self, cx: &mut Cx2d, kit: &mut Kit, area: Rect, p: &Palette, font: f64, compact: bool) {
        let item_h = if compact { 44.0 } else { (font * 1.4 + 14.0).max(32.0) };
        let w = 260.0_f64.min(area.size.x - 16.0).max(0.0);
        let x = self.more_anchor.pos.x.min(area.pos.x + area.size.x - 8.0 - w).max(area.pos.x + 8.0);
        let y = self.more_anchor.pos.y + self.more_anchor.size.y + 4.0;
        let room = ((area.pos.y + area.size.y - 8.0 - y - 8.0) / item_h).floor().max(1.0) as usize;
        let items: Vec<usize> = (0..self.hidden_crumbs).rev().take(room).collect();
        let card = rect(x, y, w, items.len() as f64 * item_h + 8.0);
        ui::card(cx, kit, card, p, 8.0);
        self.hits.push(card, Hot::MoreCard);
        let mut iy = y + 4.0;
        for index in items {
            let r = rect(x + 4.0, iy, w - 8.0, item_h);
            let hot = Hot::MoreItem(index);
            if self.hovered.as_ref() == Some(&hot) || self.pressed.as_ref() == Some(&hot) {
                kit.fill(cx, r, p.interaction_highlight, 6.0);
            }
            kit.icon(cx, rect(r.pos.x + 8.0, r.pos.y, 16.0, item_h), "folder", 16.0, p.muted);
            kit.text_in(cx, rect(r.pos.x + 32.0, r.pos.y, r.size.x - 40.0, item_h), spec(Font::Regular, font, p.foreground), &self.crumbs[index].label, 0.0);
            self.hits.push(r, hot);
            iy += item_h;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_rows(&mut self, cx: &mut Cx2d, kit: &mut Kit, list: Rect, state: &ExplorerState, p: &Palette, base: f64, pad: f64, compact: bool) {
        let row_h = self.row_height;
        let rows = state.rows();
        if self.highlight.is_some_and(|index| index >= rows.len()) {
            self.highlight = None;
        }
        let content_h = self.content_height(rows.len());
        let max = (content_h - list.size.y).max(0.0);
        self.scroll_y = self.scroll_y.clamp(0.0, max);
        cx.begin_turtle(Walk::abs_rect(list), Layout::default());
        self.hits.set_clip(Some(list));
        let first = ((self.scroll_y - LIST_PADDING) / row_h).floor().max(0.0) as usize;
        let last = (first + (list.size.y / row_h).ceil() as usize + 2).min(rows.len());
        let icon = if compact { 20.0 } else { 18.0 };
        for (index, row) in rows.iter().enumerate().take(last).skip(first) {
            let y = list.pos.y + LIST_PADDING + index as f64 * row_h - self.scroll_y;
            let r = rect(list.pos.x, y, list.size.x, row_h);
            let hot = Hot::Row(index);
            let hovered = self.hovered.as_ref() == Some(&hot);
            let pressed = self.pressed.as_ref() == Some(&hot);
            let chip = rect(r.pos.x + pad / 2.0, r.pos.y + 1.0, r.size.x - pad, r.size.y - 2.0);
            if self.highlight == Some(index) {
                kit.fill(cx, chip, p.interaction_highlight, 6.0);
            } else if hovered || pressed {
                kit.fill(cx, chip, p.surface2, 6.0);
            }
            let x = r.pos.x + pad;
            kit.icon(cx, rect(x, y, icon, row_h), "folder", icon, p.muted);
            let name_x = x + icon + 10.0;
            let chevron_x = r.pos.x + r.size.x - pad - 16.0;
            kit.text_in(cx, rect(name_x, y, chevron_x - 8.0 - name_x, row_h), spec(Font::Regular, base, p.foreground), &row.entry.name, 0.0);
            kit.icon(cx, rect(chevron_x, y, 16.0, row_h), "chevron-right", 16.0, p.extra_muted);
            self.hits.push(r, hot);
        }
        self.hits.set_clip(None);
        if max > 0.0 {
            let thumb_h = (list.size.y * list.size.y / content_h).max(24.0);
            let thumb_y = list.pos.y + (list.size.y - thumb_h) * (self.scroll_y / max);
            kit.fill(cx, rect(list.pos.x + list.size.x - 6.0, thumb_y, 4.0, thumb_h), with_alpha(p.muted, 0.35), 2.0);
        }
        cx.end_turtle();
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_error(&mut self, cx: &mut Cx2d, kit: &mut Kit, list: Rect, state: &ExplorerState, error: &str, p: &Palette, ty: &Typography, compact: bool) {
        let base = ty.ui_size(14.0);
        let small = ty.ui_size(12.0);
        let width = (list.size.x - 48.0).max(1.0);
        let message = spec(Font::Regular, small, p.muted);
        let message_h = kit.paragraph_height(cx, width, message, error);
        let pill_h = if compact { 40.0 } else { ui::ButtonSize::Sm.height() };
        let total = 24.0 + 8.0 + base * 1.4 + 4.0 + message_h + 16.0 + pill_h;
        let mut y = list.pos.y + ((list.size.y - total) / 2.0).max(8.0);
        kit.icon(cx, rect(list.pos.x, y, list.size.x, 24.0), "circle-alert", 24.0, p.destructive);
        y += 32.0;
        kit.text_in(cx, rect(list.pos.x + 24.0, y, width, base * 1.4), spec(Font::Medium, base, p.foreground), &t("workspace.folderPicker.errorTitle"), 0.5);
        y += base * 1.4 + 4.0;
        let message_w = kit.measure(cx, message, error).x.min(width);
        kit.paragraph(cx, dvec2(list.pos.x + (list.size.x - message_w) / 2.0, y), width, message, error);
        y += message_h + 16.0;
        let mut actions: Vec<(Hot, String, &str)> = Vec::new();
        if state.can_go_up() {
            actions.push((Hot::ErrorUp, t("workspace.folderPicker.up"), "arrow-up"));
        }
        actions.push((Hot::Retry, t("workspace.folderPicker.retry"), "rotate-cw"));
        if state.can_go_back() {
            actions.push((Hot::ErrorBack, t("workspace.folderPicker.back"), "arrow-left"));
        }
        let widths: Vec<f64> =
            actions.iter().map(|(_, label, icon)| ui::button_width(cx, kit, label, Some(*icon), ui::ButtonSize::Sm, ty)).collect();
        let total_w = widths.iter().sum::<f64>() + 8.0 * (actions.len() - 1) as f64;
        let mut x = list.pos.x + ((list.size.x - total_w) / 2.0).max(8.0);
        for ((hot, label, icon), w) in actions.into_iter().zip(widths) {
            let w = w.min(list.pos.x + list.size.x - 8.0 - x);
            if w <= 0.0 {
                break;
            }
            let r = rect(x, y, w, pill_h);
            let st = self.interaction(&hot, true, false);
            ui::button(cx, kit, r, &label, Some(icon), ui::ButtonVariant::Outline, ui::ButtonSize::Sm, st, p, ty);
            self.hits.push(r, hot);
            x += w + 8.0;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_footer(&mut self, cx: &mut Cx2d, kit: &mut Kit, footer: Rect, directory: &str, p: &Palette, ty: &Typography, compact: bool, pad: f64, action_h: f64) {
        let base = ty.ui_size(14.0);
        let small = ty.ui_size(12.0);
        let name_h = (base * 1.4).ceil();
        let path_h = (small * 1.4).ceil();
        let name = breadcrumb(directory).last().map(|crumb| crumb.label.clone()).unwrap_or_else(|| directory.to_owned());
        let choose = t("workspace.folderPicker.choose");
        let cancel = t("workspace.folderPicker.cancel");
        let (text_x, text_w, text_y);
        if compact {
            text_x = footer.pos.x + pad;
            text_w = footer.size.x - pad * 2.0;
            text_y = footer.pos.y + 12.0;
            let r = rect(text_x, text_y + name_h + path_h + 10.0, text_w, action_h);
            let st = self.interaction(&Hot::Choose, true, false);
            ui::button(cx, kit, r, &choose, Some("check"), ui::ButtonVariant::Primary, ui::ButtonSize::Sm, st, p, ty);
            self.hits.push(r, Hot::Choose);
        } else {
            let choose_w = ui::button_width(cx, kit, &choose, Some("check"), ui::ButtonSize::Sm, ty);
            let cancel_w = ui::button_width(cx, kit, &cancel, None, ui::ButtonSize::Sm, ty);
            let by = footer.pos.y + (footer.size.y - action_h) / 2.0;
            let choose_r = rect(footer.pos.x + footer.size.x - pad - choose_w, by, choose_w, action_h);
            let cancel_r = rect(choose_r.pos.x - 8.0 - cancel_w, by, cancel_w, action_h);
            let st = self.interaction(&Hot::Cancel, true, false);
            ui::button(cx, kit, cancel_r, &cancel, None, ui::ButtonVariant::Outline, ui::ButtonSize::Sm, st, p, ty);
            self.hits.push(cancel_r, Hot::Cancel);
            let st = self.interaction(&Hot::Choose, true, false);
            ui::button(cx, kit, choose_r, &choose, Some("check"), ui::ButtonVariant::Primary, ui::ButtonSize::Sm, st, p, ty);
            self.hits.push(choose_r, Hot::Choose);
            text_x = footer.pos.x + pad;
            text_w = (cancel_r.pos.x - 16.0 - text_x).max(0.0);
            text_y = footer.pos.y + (footer.size.y - name_h - path_h) / 2.0;
        }
        kit.text_in(cx, rect(text_x, text_y, text_w, name_h), spec(Font::Medium, base, p.foreground), &name, 0.0);
        let path_spec = spec(Font::Regular, small, p.muted);
        let shown = fit_middle(cx, kit, path_spec, directory, text_w);
        kit.text_in(cx, rect(text_x, text_y + name_h, text_w, path_h), path_spec, &shown, 0.0);
    }

    fn scroll_to(&mut self, index: usize) {
        let top = LIST_PADDING + index as f64 * self.row_height;
        let bottom = top + self.row_height;
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if bottom > self.scroll_y + self.list_rect.size.y {
            self.scroll_y = bottom - self.list_rect.size.y;
        }
    }

    fn begin_edit(&mut self, state: &ExplorerState) {
        self.editing = true;
        self.more_open = false;
        self.field.set_text(&state.current_directory());
        self.field.select_all();
        self.field.focused = true;
        self.focus_requested = true;
    }

    fn end_edit(&mut self, cx: &mut Cx) {
        self.editing = false;
        self.field.clear_selection(cx);
        self.field.focused = false;
        self.focus_requested = false;
        cx.hide_text_ime();
    }

    fn handle_selection_handles(&mut self, cx: &mut Cx, kit: &mut Kit, event: &Event) -> bool {
        if !self.editing || !self.field.focused || self.more_open {
            self.field.cancel_selection_handles(cx);
            return false;
        }
        if !self.field.handle_selection_handles(cx, kit, event) {
            return false;
        }
        self.touch_scroll.reset();
        self.thumb_drag = None;
        self.pressed = None;
        true
    }

    fn animate_scroll(&mut self, cx: &mut Cx, event: &Event, state: &ExplorerState, enabled: bool) -> bool {
        if !enabled || self.more_open {
            self.touch_scroll.reset();
            return false;
        }
        let Some(delta) = self.touch_scroll.frame(cx, event) else { return false };
        let max = self.content_height(state.rows().len()) - self.list_rect.size.y;
        self.touch_scroll.apply_delta(&mut self.scroll_y, delta, max)
    }

    fn wants_touch_focus(&self, hit: &Hit) -> bool {
        let abs = match hit {
            Hit::FingerUp(fe) => fe.abs,
            Hit::FingerLongPress(fe) => fe.abs,
            _ => return false,
        };
        self.editing && self.pressed == Some(Hot::Field) && self.hits.at(abs) == Some(Hot::Field) && self.field.wants_touch_focus(hit)
    }

    fn tooltip(&self, hot: &Hot, state: &ExplorerState) -> Option<String> {
        let key = match hot {
            Hot::Back => "workspace.folderPicker.back",
            Hot::Forward => "workspace.folderPicker.forward",
            Hot::Up => "workspace.folderPicker.up",
            Hot::Edit => "workspace.folderPicker.editPath",
            Hot::Hidden if state.show_hidden() => "workspace.folderPicker.hideHidden",
            Hot::Hidden => "workspace.folderPicker.showHidden",
            Hot::Refresh => "workspace.folderPicker.refresh",
            Hot::More => "workspace.folderPicker.moreAncestors",
            _ => return None,
        };
        Some(t(key))
    }

    /// Pointer and keyboard input. `shift` maps drawn rects to window coordinates (tooltips).
    #[allow(clippy::too_many_arguments)]
    fn handle_hit(&mut self, cx: &mut Cx, kit: &mut Kit, hit: &Hit, state: &mut ExplorerState, ui: &UiContext, shift: DVec2, changed: &mut bool) -> PickerEvent {
        match hit {
            Hit::FingerHoverIn(fe) | Hit::FingerHoverOver(fe) => {
                let hot = self.hits.at(fe.abs);
                if hot != self.hovered {
                    let tip = hot.as_ref().and_then(|hot| {
                        let text = self.tooltip(hot, state)?;
                        let anchor = self.tips.iter().find(|(h, _)| h == hot)?.1;
                        Some((text, anchor))
                    });
                    match tip {
                        Some((text, anchor)) => tooltip_at(ui, Some(&text), kit::shifted(anchor, shift), 0.5),
                        None => tooltip_at(ui, None, Rect::default(), 0.5),
                    }
                    self.hovered = hot;
                    *changed = true;
                }
                let cursor = match &self.hovered {
                    Some(Hot::Field) => MouseCursor::Text,
                    Some(hot) if hot.clickable() => MouseCursor::Hand,
                    _ => MouseCursor::Default,
                };
                cx.set_cursor(cursor);
            }
            Hit::FingerHoverOut(_) => {
                if self.hovered.take().is_some() {
                    tooltip_at(ui, None, Rect::default(), 0.5);
                    *changed = true;
                }
            }
            Hit::FingerScroll(fe) => {
                self.touch_scroll.reset();
                if !self.more_open && self.list_rect.contains(fe.abs) {
                    self.scroll_y += fe.scroll.y;
                    *changed = true;
                }
            }
            Hit::FingerDown(fe) => {
                self.touch_scroll.reset();
                let hot = self.hits.at(fe.abs);
                if self.more_open && !matches!(hot, Some(Hot::MoreItem(_) | Hot::MoreCard | Hot::More)) {
                    self.more_open = false;
                    *changed = true;
                    return PickerEvent::None;
                }
                self.pressed = hot.clone();
                *changed = true;
                if self.editing && hot == Some(Hot::Field) {
                    if !fe.device.is_touch() {
                        self.field.focused = true;
                    }
                    self.field.handle_hit(cx, kit, hit);
                    return PickerEvent::None;
                }
                if self.editing && hot != Some(Hot::Edit) {
                    self.end_edit(cx);
                }
                if fe.device.mouse_button().is_some_and(|b| b.is_secondary()) {
                    self.pressed = None;
                    return PickerEvent::None;
                }
                let list = self.list_rect;
                if self.content_height(state.rows().len()) > list.size.y && fe.abs.x > list.pos.x + list.size.x - 10.0 && list.contains(fe.abs) {
                    self.thumb_drag = Some((fe.abs.y, self.scroll_y));
                    self.pressed = None;
                    return PickerEvent::None;
                }
                if list.contains(fe.abs) && !self.more_open {
                    self.touch_scroll.handle_hit(hit);
                }
            }
            Hit::FingerMove(fe) => {
                if let Some((start, scroll)) = self.thumb_drag {
                    let content_h = self.content_height(state.rows().len());
                    self.scroll_y = scroll + (fe.abs.y - start) * content_h / self.list_rect.size.y.max(1.0);
                    *changed = true;
                } else if self.pressed == Some(Hot::Field) {
                    self.field.handle_hit(cx, kit, hit);
                    *changed = true;
                } else if let Some(dy) = self.touch_scroll.handle_hit(hit)
                    && self.list_rect.contains(fe.abs_start) {
                        let max = self.content_height(state.rows().len()) - self.list_rect.size.y;
                        self.touch_scroll.apply_delta(&mut self.scroll_y, dy, max);
                        self.pressed = None;
                        *changed = true;
                    }
            }
            Hit::FingerLongPress(fe) => {
                self.touch_scroll.reset();
                *changed = true;
                if self.pressed == Some(Hot::Field) {
                    if self.wants_touch_focus(hit) {
                        self.field.focused = true;
                        self.field.handle_hit(cx, kit, hit);
                    }
                    return PickerEvent::None;
                }
                // Touch: a long press chooses that folder directly.
                if let Some(Hot::Row(index)) = self.hits.at(fe.abs)
                    && self.pressed == Some(Hot::Row(index))
                    && let Some(row) = state.rows().get(index)
                {
                    self.pressed = None;
                    return PickerEvent::Selected(state.child_directory(&row.entry));
                }
            }
            Hit::FingerUp(fe) => {
                self.touch_scroll.release(fe.time);
                self.touch_scroll.schedule(cx);
                self.thumb_drag = None;
                let pressed = self.pressed.take();
                *changed = true;
                if pressed == Some(Hot::Field) {
                    if self.hits.at(fe.abs) != Some(Hot::Field) {
                        self.field.cancel_touch_focus();
                    } else if self.field.wants_touch_focus(hit) {
                        self.field.focused = true;
                    }
                    self.field.handle_hit(cx, kit, hit);
                    return PickerEvent::None;
                }
                if !(fe.is_over && fe.was_tap()) || fe.has_long_press_occurred {
                    return PickerEvent::None;
                }
                let hot = self.hits.at(fe.abs);
                if hot.is_none() || hot != pressed {
                    return PickerEvent::None;
                }
                return self.activate(cx, hot.unwrap(), state);
            }
            Hit::KeyDown(_) | Hit::TextInput(_) | Hit::TextIme(_) | Hit::TextCopy(_) | Hit::TextCut(_)
                if self.editing && self.field.focused =>
            {
                *changed = true;
                match self.field.handle_hit(cx, kit, hit) {
                    FieldEvent::Submitted { .. } => {
                        let text = self.field.text().to_owned();
                        self.end_edit(cx);
                        state.go_to(&text);
                    }
                    FieldEvent::Escaped => self.end_edit(cx),
                    _ => {}
                }
            }
            Hit::KeyDown(ke) => {
                *changed = true;
                return self.handle_key(cx, ke, state);
            }
            Hit::KeyFocusLost(_) => {
                if self.field.focused {
                    self.field.handle_hit(cx, kit, hit);
                    *changed = true;
                }
            }
            _ => {}
        }
        PickerEvent::None
    }

    fn handle_key(&mut self, cx: &mut Cx, ke: &KeyEvent, state: &mut ExplorerState) -> PickerEvent {
        let m = ke.modifiers;
        let count = state.rows().len();
        match ke.key_code {
            KeyCode::Escape if self.more_open => self.more_open = false,
            KeyCode::Escape if self.editing => self.end_edit(cx),
            KeyCode::Escape => return PickerEvent::Cancelled,
            KeyCode::KeyL if m.control || m.logo => self.begin_edit(state),
            KeyCode::ArrowUp if m.alt => state.parent(),
            KeyCode::ArrowLeft if m.alt => state.back(),
            KeyCode::ArrowRight if m.alt => state.forward(),
            KeyCode::Backspace => state.parent(),
            KeyCode::ArrowDown if count > 0 => {
                let next = self.highlight.map_or(0, |index| (index + 1).min(count - 1));
                self.highlight = Some(next);
                self.scroll_to(next);
            }
            KeyCode::ArrowUp if count > 0 => {
                let next = self.highlight.map_or(count - 1, |index| index.saturating_sub(1));
                self.highlight = Some(next);
                self.scroll_to(next);
            }
            KeyCode::ReturnKey | KeyCode::NumpadEnter if m.control || m.logo => {
                return PickerEvent::Selected(state.current_directory());
            }
            KeyCode::ReturnKey | KeyCode::NumpadEnter => {
                if let Some(row) = self.highlight.and_then(|index| state.rows().get(index)) {
                    let target = state.child_directory(&row.entry);
                    state.navigate(&target, true);
                }
            }
            _ => {}
        }
        PickerEvent::None
    }

    fn activate(&mut self, cx: &mut Cx, hot: Hot, state: &mut ExplorerState) -> PickerEvent {
        match hot {
            Hot::Back | Hot::ErrorBack => state.back(),
            Hot::Forward => state.forward(),
            Hot::Up | Hot::ErrorUp => state.parent(),
            Hot::Crumb(index) | Hot::MoreItem(index) => {
                self.more_open = false;
                if let Some(crumb) = self.crumbs.get(index).filter(|_| index + 1 < self.crumbs.len()) {
                    let path = crumb.path.clone();
                    state.navigate(&path, true);
                }
            }
            Hot::More => self.more_open = !self.more_open,
            Hot::Edit => {
                if self.editing {
                    self.end_edit(cx);
                } else {
                    self.begin_edit(state);
                }
            }
            Hot::Hidden => state.toggle_hidden(),
            Hot::Refresh if !state.refreshing() => state.refresh(),
            Hot::Row(index) => {
                if let Some(row) = state.rows().get(index) {
                    let target = state.child_directory(&row.entry);
                    state.navigate(&target, true);
                }
            }
            Hot::Retry => {
                let path = state.path().to_owned();
                state.reload(&path);
            }
            Hot::Cancel => return PickerEvent::Cancelled,
            Hot::Choose => return PickerEvent::Selected(state.current_directory()),
            Hot::Refresh | Hot::List | Hot::MoreCard | Hot::Field => {}
        }
        PickerEvent::None
    }
}

/// `text` middle-ellipsized to fit `width`.
fn fit_middle(cx: &mut Cx, kit: &mut Kit, spec: crate::common::paint::TextSpec, text: &str, width: f64) -> String {
    if width <= 0.0 || kit.measure(cx, spec, text).x <= width {
        return text.to_owned();
    }
    let (mut low, mut high) = (0usize, text.chars().count());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if kit.measure(cx, spec, &middle_ellipsis(text, mid)).x <= width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    middle_ellipsis(text, low)
}

#[derive(Live, LiveHook, Widget)]
pub struct RemoteFolderPicker {
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[redraw]
    #[rust]
    area: Area,
    #[live]
    kit: Kit,
    #[rust]
    scope: Option<(String, String)>,
    #[rust]
    owner: String,
    #[rust]
    view: PickerView,
    #[rust]
    seen_epoch: u64,
    /// Top-left of the last draw (before the hosting modal centred it).
    #[rust]
    drawn_at: DVec2,
    /// The frame after `open`: desktop keyboard focus moves to the picker.
    #[rust]
    next_frame: NextFrame,
}

impl Drop for RemoteFolderPicker {
    fn drop(&mut self) {
        if !self.owner.is_empty() {
            store::release(&self.owner);
            store::unregister_owner(&self.owner);
        }
    }
}

impl RemoteFolderPicker {
    fn owner(&mut self) -> String {
        if self.owner.is_empty() {
            self.owner = format!("picker:{:?}", self.widget_uid());
        }
        self.owner.clone()
    }
}

impl Widget for RemoteFolderPicker {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.field.observe_event(event);
        let Some((host_id, cwd)) = self.scope.clone() else { return };
        let owner = self.owner();
        let uid = self.widget_uid();
        let mut action = None;
        let mut changed = self.view.field.blink.handle_event(event);
        if let Some(ui) = scope.data.get_mut::<UiContext>() {
            store::register_picker(&owner, &host_id);
            store::with(|s| {
                s.explorer(ui, &owner, &cwd, ".", true, None);
            });
            if self.next_frame.is_event(event).is_some() && !ui.compact {
                cx.set_key_focus(self.area);
            }
            let enabled = self.area.is_valid(cx);
            let handle_consumed = if enabled {
                self.view.handle_selection_handles(cx, &mut self.kit, event)
            } else {
                self.view.field.cancel_selection_handles(cx);
                false
            };
            changed |= handle_consumed;
            changed |= store::with(|s| {
                s.explorer_mut(&owner).is_some_and(|state| self.view.animate_scroll(cx, event, state, enabled))
            });
            let shift = if enabled { self.area.rect(cx).pos - self.drawn_at } else { DVec2::default() };
            let hit = kit::unshift_hit(event.hits(cx, self.area), shift);
            if !handle_consumed && !matches!(hit, Hit::Nothing) {
                if matches!(&hit, Hit::FingerDown(fe) if !fe.device.is_touch()) || self.view.wants_touch_focus(&hit) {
                    cx.set_key_focus(self.area);
                }
                let view = &mut self.view;
                let kit = &mut self.kit;
                let ui: &UiContext = ui;
                let result = store::with(|s| {
                    let state = s.explorer_mut(&owner)?;
                    Some(view.handle_hit(cx, kit, &hit, state, ui, shift, &mut changed))
                });
                match result {
                    Some(PickerEvent::Selected(path)) => action = Some(RemoteFolderPickerAction::Selected(path)),
                    Some(PickerEvent::Cancelled) => action = Some(RemoteFolderPickerAction::Cancelled),
                    _ => {}
                }
                if std::mem::take(&mut self.view.focus_requested) {
                    cx.set_key_focus(self.area);
                }
            }
            store::with(|s| s.pump(cx, ui));
        }
        if let Some(action) = action {
            cx.widget_action(uid, &scope.path, action);
        }
        let epoch = store::with(|s| s.epoch);
        if changed || epoch != self.seen_epoch {
            self.seen_epoch = epoch;
            self.area.redraw(cx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        let rect = cx.turtle().rect();
        cx.add_aligned_rect_area(&mut self.area, rect);
        let compact = scope.data.get::<UiContext>().is_some_and(|ui| ui.compact);
        let owner = self.owner();
        let area = self.area;
        self.drawn_at = rect.pos;
        let p = crate::theme::current_palette();
        let ty = crate::theme::current_typography();
        let look = PickerLook { palette: &p, typography: &ty, compact };
        let view = &mut self.view;
        let kit = &mut self.kit;
        // The hosting sheet supplies the surface; the picker draws only its content.
        store::with(|s| {
            if let Some(state) = s.explorer_mut(&owner) {
                view.draw(cx, kit, rect, state, &look, area);
            }
        });
        cx.end_turtle();
        DrawStep::done()
    }
}

impl RemoteFolderPickerRef {
    /// Lists `cwd` (remote path, `~` for the host home, also when empty) on `host_id`,
    /// directories only. Every open starts a fresh browser (history, location, selection).
    pub fn open(&self, cx: &mut Cx, host_id: &str, cwd: &str) {
        if let Some(mut inner) = self.borrow_mut() {
            let cwd = if cwd.trim().is_empty() { "~".to_owned() } else { cwd.trim().to_owned() };
            let owner = inner.owner();
            store::release(&owner);
            inner.view.field.clear_selection(cx);
            inner.view = PickerView::default();
            inner.scope = Some((host_id.to_owned(), cwd));
            inner.next_frame = cx.new_next_frame();
            inner.area.redraw(cx);
        }
    }

    pub fn selected(&self, actions: &Actions) -> Option<String> {
        let uid = self.widget_uid();
        actions.filter_widget_actions_cast::<RemoteFolderPickerAction>(uid).find_map(|action| match action {
            RemoteFolderPickerAction::Selected(path) => Some(path),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(crumbs: &[Crumb]) -> Vec<&str> {
        crumbs.iter().map(|crumb| crumb.label.as_str()).collect()
    }

    #[test]
    fn breadcrumb_segments_from_root() {
        let crumbs = breadcrumb("/srv/app/data");
        assert_eq!(labels(&crumbs), vec!["/", "srv", "app", "data"]);
        assert_eq!(crumbs[0].path, "/");
        assert_eq!(crumbs[2].path, "/srv/app");
        assert_eq!(labels(&breadcrumb("/")), vec!["/"]);
    }

    #[test]
    fn unresolved_breadcrumb_retains_the_host_alias() {
        let crumbs = breadcrumb("~/work");
        assert_eq!(labels(&crumbs), vec!["~", "work"]);
        assert_eq!(crumbs[1].path, "~/work");
        let crumbs = breadcrumb("~/..");
        assert_eq!(labels(&crumbs), vec!["~", ".."]);
        assert_eq!(crumbs[1].path, "~/..");
    }

    #[test]
    fn resolving_a_location_updates_an_untouched_path_field_but_keeps_user_edits() {
        use serde_json::json;

        let state = ExplorerState::new("~", ".", true, None, &json!({}));
        let mut view = PickerView::default();
        view.sync_directory(&state.current_directory());
        view.begin_edit(&state);
        view.sync_directory("/home/me");
        assert_eq!(view.field.text(), "/home/me");
        view.field.set_text("~/work");
        view.sync_directory("/home/me/work");
        assert_eq!(view.field.text(), "~/work");
    }

    #[test]
    fn resolved_home_display_and_path_field_do_not_depend_on_open_route() {
        use super::super::explorer::Listing;
        use serde_json::json;

        // Each open creates a fresh state, even when reopening a selected absolute directory.
        for cwd in ["~", "~/work/..", "/home/me"] {
            let mut state = ExplorerState::new(cwd, ".", true, None, &json!({}));
            state.start(&json!({}));
            state.on_listed(".", 0, Ok(Listing { entries: Vec::new(), absolute: Some("/home/me".to_owned()) }));
            let selected = state.current_directory();
            assert_eq!(selected, "/home/me");
            let crumbs = breadcrumb(&selected);
            assert_eq!(labels(&crumbs), vec!["/", "home", "me"]);
            assert_eq!(crumbs.last().unwrap().path, selected);
            assert_eq!(crumbs.last().unwrap().label, "me");
            let mut view = PickerView::default();
            view.begin_edit(&state);
            assert_eq!(view.field.text(), selected);
            state.refresh();
            state.on_listed("/home/me", 0, Ok(Listing { entries: Vec::new(), absolute: Some(selected.clone()) }));
            assert_eq!(breadcrumb(&state.current_directory()), crumbs);
        }
    }

    #[test]
    fn breadcrumb_windows_paths() {
        let crumbs = breadcrumb("C:\\Users\\me\\src");
        assert_eq!(labels(&crumbs), vec!["C:", "Users", "me", "src"]);
        assert_eq!(crumbs[2].path, "C:\\Users\\me");
        let crumbs = breadcrumb("D:\\data\\logs");
        assert_eq!(labels(&crumbs), vec!["D:", "data", "logs"]);
        assert_eq!(crumbs[0].path, "D:\\");
        assert_eq!(crumbs[1].path, "D:\\data");
        assert_eq!(labels(&breadcrumb("C:\\")), vec!["C:"]);
        let crumbs = breadcrumb("\\\\server\\share\\work");
        assert_eq!(labels(&crumbs), vec!["\\\\server\\share", "work"]);
        assert_eq!(crumbs[0].path, "\\\\server\\share\\");
        assert_eq!(crumbs[1].path, "\\\\server\\share\\work");
    }

    #[test]
    fn collapse_hides_leading_crumbs_when_narrow() {
        let widths = [20.0, 40.0, 40.0, 60.0];
        // Fits: 160 + 3 separators of 10.
        assert_eq!(collapsed_count(&widths, 190.0, 24.0, 10.0), 0);
        // "…" + 10 + (40 + 40 + 60 + 20) = 194; then 144; then 94.
        assert_eq!(collapsed_count(&widths, 189.0, 24.0, 10.0), 2);
        assert_eq!(collapsed_count(&widths, 150.0, 24.0, 10.0), 2);
        assert_eq!(collapsed_count(&widths, 100.0, 24.0, 10.0), 3);
        // Even the last alone overflows: it stays (shrunk), everything else collapses.
        assert_eq!(collapsed_count(&widths, 30.0, 24.0, 10.0), 3);
        assert_eq!(collapsed_count(&[500.0], 30.0, 24.0, 10.0), 0);
    }

    #[test]
    fn middle_ellipsis_keeps_both_ends() {
        assert_eq!(middle_ellipsis("/srv/app", 20), "/srv/app");
        assert_eq!(middle_ellipsis("/home/me/projects/paseo", 9), "/ho…/paseo");
    }
}
