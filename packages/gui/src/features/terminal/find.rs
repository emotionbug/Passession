//! Find in terminal and execution output (`_ExecutionLogPanelState._find`, original
//! `PaneFind`): case-insensitive search over logical lines so a match split by the PTY width
//! across soft-wrapped rows is still found, the highlights a view paints, and the floating
//! find card both panes show.
//!
//! Output keeps arriving while the card is open: the panel re-runs the search (debounced
//! like typing) and the active match stays on the nearest match to where it was.

use makepad_widgets::*;

use super::stream::Stream;
use crate::features::workspace::i18n::{t, ta};
use crate::features::workspace::kit::{set_optional, WaButtonWidgetRefExt, WaInputWidgetRefExt};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;

    // Original `PaneFind`: a surface1 card at most 340px wide at the top right, 6px padding,
    // 4px gap, border, 8px radius; the panel narrows it to the pane.
    pub WaPaneFind = <View> {
        width: Fill, height: Fit, align: { x: 1.0 }, padding: 8., visible: false
        find_bar = <Surface> {
            width: 340., height: Fit, flow: Right, spacing: 4., padding: 6.
            align: { y: 0.5 }
            role: Surface1, border_role: Border, radius: 8.
            field = <Surface> {
                width: Fill, height: 28., flow: Right, spacing: 8.
                padding: { left: 8., right: 8. }
                align: { y: 0.5 }
                role: Surface2, border_role: BorderAccent, radius: 6., border_width: 0.
                query = <WaBareInput> { input = { padding: 0. } }
                match_status = <View> { width: Fit, height: Fit, text = <WaSmallFit> {} }
            }
            find_previous = <WaToolButton> { icon: "arrow-up" }
            find_next = <WaToolButton> { icon: "arrow-down" }
            find_close = <WaToolButton> { icon: "x" }
        }
    }
}

/// The card's widest size (`PaneFind`).
const CARD_MAX_WIDTH: f64 = 340.0;
/// Below this field width the previous/next buttons give way; Enter and Shift+Enter still
/// navigate.
const FIELD_MIN_WIDTH: f64 = 96.0;
/// Typing and output settle this long before a search runs.
pub const SEARCH_DEBOUNCE: f64 = 0.18;

/// A match in presented row/char offsets (`_LogMatch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FindMatch {
    pub start_row: usize,
    pub start_offset: usize,
    pub end_row: usize,
    pub end_offset: usize,
}

/// Every match of `query` in the stream's counted rows, ordered by position.
pub fn find_matches(stream: &Stream, query: &str) -> Vec<FindMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let count = stream.line_count();
    // The searchable text and, per char, its (row, char offset within the row).
    let mut text: Vec<char> = Vec::new();
    let mut offsets: Vec<(usize, usize)> = Vec::new();
    let mut previous_wraps = false;
    let mut start = 0;
    while start < count {
        let chunk = stream.screen.read_uncached(start, 1024.min(count - start));
        if chunk.is_empty() {
            break;
        }
        for (index, line) in chunk.iter().enumerate() {
            let row = start + index;
            if row > 0 && !previous_wraps {
                text.push('\n');
                offsets.push((row, 0));
            }
            for (offset, ch) in line.text.chars().enumerate() {
                text.push(ch);
                offsets.push((row, offset));
            }
            previous_wraps = line.wraps_next;
        }
        start += chunk.len();
    }
    // Fold case only when folding keeps offsets aligned (one char maps to one char).
    let folded: Vec<char> = text.iter().map(|c| fold(*c)).collect();
    let needle: Vec<char> = query.chars().map(fold).collect();
    let (source, needle) = if folded.len() == text.len() { (folded, needle) } else { (text, query.chars().collect()) };
    let mut matches = Vec::new();
    if needle.is_empty() || needle.len() > source.len() {
        return matches;
    }
    let mut from = 0;
    while from + needle.len() <= source.len() {
        let Some(found) = source[from..].windows(needle.len()).position(|window| window == needle.as_slice()) else { break };
        let found = from + found;
        let (start_row, start_offset) = offsets[found];
        let (end_row, end_offset) = offsets[found + needle.len() - 1];
        matches.push(FindMatch { start_row, start_offset, end_row, end_offset: end_offset + 1 });
        from = found + needle.len();
    }
    matches
}

fn fold(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

/// The next active index after a search (`_matchIndex` update).
pub fn next_index(current: Option<usize>, total: usize, previous: bool, reset: bool) -> Option<usize> {
    if total == 0 {
        return None;
    }
    if reset {
        return Some(if previous { total - 1 } else { 0 });
    }
    let Some(current) = current else {
        return Some(if previous { total - 1 } else { 0 });
    };
    let step = if previous { -1 } else { 1 };
    Some(((current as isize + step).rem_euclid(total as isize)) as usize)
}

/// The match closest to `anchor` (rows first, then the offset in the row): new output
/// keeps the active match where the reader was.
pub fn nearest_index(matches: &[FindMatch], anchor: FindMatch) -> Option<usize> {
    matches
        .iter()
        .enumerate()
        .min_by_key(|(_, m)| (m.start_row.abs_diff(anchor.start_row), m.start_offset.abs_diff(anchor.start_offset)))
        .map(|(index, _)| index)
}

/// Highlight ranges of `row` (`_rowHighlights`): every match and the active one.
pub fn row_highlights(matches: &[FindMatch], active: Option<usize>, row: usize, length: usize) -> Vec<(usize, usize, bool)> {
    let first = matches.partition_point(|m| m.end_row < row);
    let mut out = Vec::new();
    for (index, m) in matches.iter().enumerate().skip(first) {
        if m.start_row > row {
            break;
        }
        let start = if row == m.start_row { m.start_offset } else { 0 }.min(length);
        let end = if row == m.end_row { m.end_offset } else { length }.clamp(start, length);
        if end > start {
            out.push((start, end, Some(index) == active));
        }
    }
    out
}

/// How a search picks the active match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchMode {
    /// A new query: the first match (the last one with `previous`).
    Reset { previous: bool },
    /// Enter, Shift+Enter, previous/next: one match on from the active one.
    Step { previous: bool },
    /// The output changed: the same query again, the active match kept nearby, no scrolling.
    Refresh,
}

/// The matches a view paints.
#[derive(Default)]
pub struct FindHits {
    pub matches: Vec<FindMatch>,
    pub active: Option<usize>,
    /// The output revision the matches were found in.
    pub revision: u64,
}

impl FindHits {
    pub fn clear(&mut self) {
        self.matches.clear();
        self.active = None;
    }

    pub fn row(&self, row: usize, length: usize) -> Vec<(usize, usize, bool)> {
        row_highlights(&self.matches, self.active, row, length)
    }

    /// Searches `stream` for `query`; returns the match to reveal.
    pub fn search(&mut self, stream: &Stream, query: &str, mode: SearchMode) -> Option<FindMatch> {
        let revision = stream.revision();
        let anchor = self.active.and_then(|index| self.matches.get(index)).copied();
        let same_output = self.revision == revision;
        self.revision = revision;
        if query.is_empty() {
            self.clear();
            return None;
        }
        let matches = find_matches(stream, query);
        let nearest = || anchor.and_then(|anchor| nearest_index(&matches, anchor));
        let active = match mode {
            SearchMode::Reset { previous } => next_index(None, matches.len(), previous, true),
            SearchMode::Step { previous } => {
                let base = if same_output { self.active.filter(|index| *index < matches.len()) } else { nearest() };
                next_index(base, matches.len(), previous, false)
            }
            SearchMode::Refresh => nearest(),
        };
        self.matches = matches;
        self.active = active;
        match mode {
            SearchMode::Refresh => None,
            _ => active.map(|index| self.matches[index]),
        }
    }
}

/// What the find card asked its panel to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindCommand {
    Search(SearchMode),
    Closed,
}

/// The panel side of the find card: open state, query, debounce and status.
#[derive(Default)]
pub struct FindCard {
    pub open: bool,
    pub query: String,
    /// A search ran for the current query (the status shows its counts).
    searched: bool,
    /// The query changed or output arrived; a search is due.
    searching: bool,
    timer: Timer,
    focus_query: bool,
}

impl FindCard {
    pub fn open(&mut self) {
        self.open = true;
        self.focus_query = true;
    }

    pub fn close(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.timer);
        self.timer = Timer::default();
        self.searching = false;
        self.open = false;
    }

    fn schedule(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.timer);
        self.timer = cx.start_timeout(SEARCH_DEBOUNCE);
    }

    /// The debounce elapsed: the search to run.
    pub fn timer_fired(&mut self, event: &Event) -> Option<SearchMode> {
        self.timer.is_event(event)?;
        self.timer = Timer::default();
        Some(if std::mem::take(&mut self.searching) && !self.searched { SearchMode::Reset { previous: false } } else { SearchMode::Refresh })
    }

    /// New output while the card shows matches: search again once it settles. A pending
    /// search is not pushed back, so steady output still refreshes every debounce.
    pub fn output_changed(&mut self, cx: &mut Cx) {
        if self.open && !self.query.is_empty() && self.timer.is_empty() {
            self.timer = cx.start_timeout(SEARCH_DEBOUNCE);
        }
    }

    /// A search for the current query ran.
    pub fn searched(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.timer);
        self.timer = Timer::default();
        self.searched = true;
        self.searching = false;
    }

    /// The card's buttons and query field.
    pub fn handle_actions(&mut self, cx: &mut Cx, root: &WidgetRef, actions: &Actions) -> Vec<FindCommand> {
        let mut commands = Vec::new();
        let query = root.wa_input(id!(find_bar.query));
        if let Some(text) = query.changed(actions) {
            self.query = text;
            self.searched = false;
            self.searching = !self.query.is_empty();
            if self.query.is_empty() {
                cx.stop_timer(self.timer);
                self.timer = Timer::default();
                commands.push(FindCommand::Search(SearchMode::Reset { previous: false }));
            } else {
                self.schedule(cx);
            }
        }
        if let Some((_, modifiers)) = query.returned(actions) {
            let previous = modifiers.shift;
            commands.push(FindCommand::Search(if self.searched { SearchMode::Step { previous } } else { SearchMode::Reset { previous } }));
            self.focus_query = true;
        }
        if root.wa_button(id!(find_bar.find_previous)).clicked(actions) {
            commands.push(FindCommand::Search(if self.searched { SearchMode::Step { previous: true } } else { SearchMode::Reset { previous: true } }));
        }
        if root.wa_button(id!(find_bar.find_next)).clicked(actions) {
            commands.push(FindCommand::Search(if self.searched { SearchMode::Step { previous: false } } else { SearchMode::Reset { previous: false } }));
        }
        if query.escaped(actions) || root.wa_button(id!(find_bar.find_close)).clicked(actions) {
            self.close(cx);
            commands.push(FindCommand::Closed);
        }
        commands
    }

    fn status(&self, total: usize, active: Option<usize>) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        if self.searching && !self.searched {
            return t("paneFind.searching");
        }
        if !self.searched {
            return String::new();
        }
        if total == 0 {
            return t("paneFind.noMatches");
        }
        match active {
            None => ta("paneFind.total", &[("total", &total.to_string())]),
            Some(index) => ta("paneFind.position", &[("current", &(index + 1).to_string()), ("total", &total.to_string())]),
        }
    }

    /// Presents the card in `wrap` (a `WaPaneFind`) over a body `body_width` wide.
    pub fn present(&self, cx: &mut Cx, wrap: &WidgetRef, body_width: f64, compact: bool, total: usize, active: Option<usize>) {
        wrap.set_visible(cx, self.open);
        if !self.open {
            return;
        }
        let ty = crate::theme::current_typography();
        let field_h = 28.0_f64.max(ty.ui_size(14.0) * 1.4 + 8.0);
        wrap.widget(id!(find_bar.field)).apply_over(cx, live! { height: (field_h) });
        // `min(body - 16, 340)`: the card never spills past the pane's left edge.
        let width = if body_width > 0.0 { (body_width - 16.0).clamp(0.0, CARD_MAX_WIDTH) } else { CARD_MAX_WIDTH };
        wrap.widget(id!(find_bar)).apply_over(cx, live! { width: (width) });
        let button = if compact { 32.0 } else { 26.0 };
        let field = |buttons: f64| width - 12.0 - buttons * (button + 4.0);
        let show_steps = field(3.0) >= FIELD_MIN_WIDTH;
        let query = wrap.wa_input(id!(find_bar.query));
        query.set_empty_text(cx, t("paneFind.placeholder"));
        set_optional(cx, &wrap.widget(id!(find_bar.match_status)), &self.status(total, active));
        let can_navigate = total > 0;
        for (path, tooltip) in [(id!(find_bar.find_previous), "paneFind.previous"), (id!(find_bar.find_next), "paneFind.next")] {
            let step = wrap.wa_button(path);
            step.set_visible(cx, show_steps);
            step.set_enabled(cx, can_navigate);
            step.set_tooltip(cx, &t(tooltip));
        }
        wrap.wa_button(id!(find_bar.find_close)).set_tooltip(cx, &t("paneFind.close"));
        // The query field's borderAccent outline while focused.
        let focused = query.key_focus(cx);
        wrap.widget(id!(find_bar.field)).apply_over(cx, live! { border_width: (if focused { 1.0 } else { 0.0 }) });
    }

    /// Focuses the query after the card drew (opening, Enter).
    pub fn take_focus(&mut self, cx: &mut Cx, wrap: &WidgetRef) {
        if self.open && std::mem::take(&mut self.focus_query) {
            wrap.wa_input(id!(find_bar.query)).set_key_focus(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::terminal::session;

    fn stream(text: &str) -> Stream {
        let mut stream = session::open("h", "t", 100).unwrap();
        stream.screen.write(text.as_bytes());
        stream.publish();
        stream
    }

    #[test]
    fn next_index_wraps_both_ways() {
        assert_eq!(next_index(None, 3, false, true), Some(0));
        assert_eq!(next_index(None, 3, true, true), Some(2));
        assert_eq!(next_index(Some(2), 3, false, false), Some(0));
        assert_eq!(next_index(Some(0), 3, true, false), Some(2));
        assert_eq!(next_index(Some(0), 0, true, false), None);
        assert_eq!(next_index(None, 3, true, false), Some(2));
        assert_eq!(next_index(None, 3, false, false), Some(0));
    }

    #[test]
    fn fold_keeps_single_char_mappings() {
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('İ'), 'İ');
    }

    #[test]
    fn highlights_cover_matches_crossing_rows() {
        let matches = [
            FindMatch { start_row: 0, start_offset: 2, end_row: 1, end_offset: 3 },
            FindMatch { start_row: 1, start_offset: 5, end_row: 1, end_offset: 7 },
        ];
        assert_eq!(row_highlights(&matches, Some(1), 0, 4), vec![(2, 4, false)]);
        assert_eq!(row_highlights(&matches, Some(1), 1, 10), vec![(0, 3, false), (5, 7, true)]);
        assert!(row_highlights(&matches, None, 2, 10).is_empty());
    }

    #[test]
    fn terminal_and_log_streams_are_searched_alike() {
        let mut terminal = stream("Error one\r\nok\r\nerror two\r\n");
        let mut hits = FindHits::default();
        let first = hits.search(&terminal, "ERROR", SearchMode::Reset { previous: false });
        assert_eq!(first, Some(FindMatch { start_row: 0, start_offset: 0, end_row: 0, end_offset: 5 }));
        assert_eq!(hits.search(&terminal, "error", SearchMode::Step { previous: false }).map(|m| m.start_row), Some(2));
        // New output keeps the active match on the same row instead of jumping to the first.
        terminal.screen.write(b"error three\r\n");
        terminal.publish();
        assert_eq!(hits.search(&terminal, "error", SearchMode::Refresh), None);
        assert_eq!(hits.matches.len(), 3);
        assert_eq!(hits.active, Some(1));
        let log = crate::features::logs::session::open("h", "t", 100).unwrap();
        assert!(find_matches(&log, "error").is_empty());
    }

    #[test]
    fn nearest_prefers_the_same_row() {
        let at = |row, offset| FindMatch { start_row: row, start_offset: offset, end_row: row, end_offset: offset + 1 };
        assert_eq!(nearest_index(&[at(0, 0), at(4, 9), at(5, 0)], at(4, 2)), Some(1));
        assert_eq!(nearest_index(&[], at(4, 2)), None);
    }
}
