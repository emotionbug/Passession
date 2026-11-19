//! Find in terminal and execution output (`_ExecutionLogPanelState._find`, original
//! `PaneFind`): case-insensitive search over logical lines so a match split by the PTY width
//! across soft-wrapped rows is still found, the highlights a view paints, and the floating
//! find card both panes show.
//!
//! Output keeps arriving while the card is open: the panel refreshes the search after a
//! debounce and keeps the active match nearby. Navigation reuses the current matches.

use makepad_widgets::*;
use paseo_terminal::HistoryStart;

use super::stream::Stream;
use crate::features::workspace::i18n::{t, ta};
use crate::features::workspace::kit::{WaButtonWidgetRefExt, WaInputWidgetRefExt, set_optional};

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
        find_bar = <PointerShield> {
            width: 340., height: Fit
            content = <Surface> {
                width: Fill, height: Fit, flow: Right, spacing: 4., padding: 6.
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
    position: usize,
}

/// One word per physical row. A hard break belongs to the following row at
/// offset zero, just like its first character.
#[derive(Clone, Copy, PartialEq, Eq)]
struct RowStart(usize);

impl RowStart {
    fn new(position: usize, has_break: bool) -> Self {
        Self((position << 1) | usize::from(has_break))
    }

    fn position(self) -> usize {
        self.0 >> 1
    }

    fn has_break(self) -> bool {
        self.0 & 1 != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct IndexStamp {
    epoch: u64,
    row_revision: u64,
    count: usize,
}

#[derive(Default)]
struct SearchIndex {
    // Single-character folds keep search positions in presented character units.
    text: Vec<char>,
    rows: Vec<RowStart>,
    last_wraps: bool,
    stamp: Option<IndexStamp>,
    history: usize,
    history_start: Option<HistoryStart>,
    #[cfg(test)]
    rows_read: usize,
}

impl SearchIndex {
    /// Updates the indexed rows and returns the first position whose text or
    /// (row, offset) mapping changed.
    fn sync(&mut self, stream: &Stream) -> Option<usize> {
        let screen = &stream.screen;
        let stamp = IndexStamp {
            epoch: screen.search_epoch(),
            row_revision: screen.row_revision(),
            count: if screen.is_retained() {
                stream.line_count()
            } else {
                0
            },
        };
        if self.stamp == Some(stamp) {
            return None;
        }
        let same_layout = self.stamp.is_some_and(|old| old.epoch == stamp.epoch);
        let same_rows = same_layout
            && self
                .stamp
                .is_some_and(|old| old.row_revision == stamp.row_revision);
        let same_history = same_layout
            && self
                .history_start
                .as_ref()
                .is_some_and(|start| screen.has_history_start(start));
        let history = screen.history();
        // VT output can rewrite any active row, not only the last one. Previously
        // scrolled rows are immutable as long as their history origin survives.
        let stable_rows = if same_rows {
            self.rows.len()
        } else if same_history {
            self.history.min(history)
        } else {
            0
        };
        self.stamp = Some(stamp);
        self.history = history;
        if !same_history {
            self.history_start = screen.history_start();
        }

        let mut row = stable_rows.min(self.rows.len()).min(stamp.count);
        let mut position = self
            .rows
            .get(row)
            .map_or(self.text.len(), |start| start.position());
        let mut previous_wraps = row > 0
            && self
                .rows
                .get(row)
                .map_or(self.last_wraps, |start| !start.has_break());
        let mut changed = usize::MAX;
        while row < stamp.count {
            let chunk = screen.read_uncached(row, 1024.min(stamp.count - row));
            if chunk.is_empty() {
                break;
            }
            #[cfg(test)]
            {
                self.rows_read += chunk.len();
            }
            for line in chunk {
                let has_break = row > 0 && !previous_wraps;
                let start = RowStart::new(position, has_break);
                if let Some(old) = self.rows.get_mut(row) {
                    if *old != start {
                        changed = changed.min(position.min(old.position()));
                        *old = start;
                    }
                } else {
                    self.rows.push(start);
                }
                let separator = has_break.then_some('\n');
                for ch in separator.into_iter().chain(line.text.chars().map(fold)) {
                    if let Some(old) = self.text.get_mut(position) {
                        if *old != ch {
                            changed = changed.min(position);
                            *old = ch;
                        }
                    } else {
                        changed = changed.min(position);
                        self.text.push(ch);
                    }
                    position += 1;
                }
                previous_wraps = line.wraps_next;
                row += 1;
            }
        }
        if position < self.text.len() {
            changed = changed.min(position);
            self.text.truncate(position);
        }
        self.rows.truncate(row);
        self.last_wraps = previous_wraps;
        (changed != usize::MAX).then_some(changed)
    }

    fn coordinate(&self, row: &mut usize, position: usize) -> (usize, usize) {
        // Positions increase, but extension can leave sparse hits. Exponentially
        // bound the lookup so nearby hits are cheap and gaps need no row scan.
        let remaining = &self.rows[*row + 1..];
        if remaining
            .first()
            .is_some_and(|start| start.position() <= position)
        {
            let mut bound = 2;
            while remaining
                .get(bound - 1)
                .is_some_and(|start| start.position() <= position)
            {
                bound *= 2;
            }
            *row += remaining[..bound.min(remaining.len())]
                .partition_point(|start| start.position() <= position);
        }
        let start = self.rows[*row];
        (
            *row,
            position.saturating_sub(start.position() + usize::from(start.has_break())),
        )
    }
}

#[derive(Default)]
struct Needle {
    chars: Vec<char>,
    // Longest proper prefix also ending at each character (KMP failure links).
    prefixes: Vec<usize>,
}

impl Needle {
    fn replace(&mut self, chars: Vec<char>) {
        self.chars = chars;
        self.prefixes.clear();
        self.prefixes.push(0);
        let mut matched = 0;
        for &ch in &self.chars[1..] {
            matched = self.advance(matched, ch);
            self.prefixes.push(matched);
        }
    }

    fn advance(&self, mut matched: usize, ch: char) -> usize {
        while matched > 0 && self.chars[matched] != ch {
            matched = self.prefixes[matched - 1];
        }
        matched + usize::from(self.chars[matched] == ch)
    }

    fn scan(&self, text: &[char], from: usize, starts: &mut Vec<usize>) {
        let len = self.chars.len();
        let mut matched = 0;
        for (offset, &ch) in text[from..].iter().enumerate() {
            matched = self.advance(matched, ch);
            if matched == len {
                starts.push(from + offset + 1 - len);
                matched = self.prefixes[len - 1];
            }
        }
    }
}

fn fold(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

/// The next active index after a search (`_matchIndex` update).
pub fn next_index(
    current: Option<usize>,
    total: usize,
    previous: bool,
    reset: bool,
) -> Option<usize> {
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
    let middle = matches.partition_point(|m| m.start_row < anchor.start_row);
    let first = middle.checked_sub(1).map_or(0, |index| {
        let row = matches[index].start_row;
        matches[..middle].partition_point(|m| m.start_row < row)
    });
    let last = matches.get(middle).map_or(middle, |m| {
        middle + matches[middle..].partition_point(|other| other.start_row == m.start_row)
    });
    (first..last).min_by_key(|&index| {
        let m = matches[index];
        (
            m.start_row.abs_diff(anchor.start_row),
            m.start_offset.abs_diff(anchor.start_offset),
        )
    })
}

/// Highlight ranges of `row` (`_rowHighlights`): every match and the active one.
pub fn row_highlights(
    matches: &[FindMatch],
    active: Option<usize>,
    row: usize,
    length: usize,
) -> Vec<(usize, usize, bool)> {
    let first = matches.partition_point(|m| m.end_row < row);
    let mut out = Vec::new();
    for (index, m) in matches.iter().enumerate().skip(first) {
        if m.start_row > row {
            break;
        }
        let start = if row == m.start_row {
            m.start_offset
        } else {
            0
        }
        .min(length);
        let end = if row == m.end_row {
            m.end_offset
        } else {
            length
        }
        .clamp(start, length);
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
    revision: u64,
    index: SearchIndex,
    query: String,
    needle: Needle,
    // Keep overlapping candidates as well. Extending "aa" to "aab" in "aaab"
    // must find the start that the shorter query's visible results skipped.
    starts: Vec<usize>,
    #[cfg(test)]
    chars_scanned: usize,
}

impl FindHits {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn clear_query(&mut self) {
        self.query.clear();
        self.needle.chars.clear();
        self.starts.clear();
        self.matches.clear();
        self.active = None;
    }

    pub fn row(&self, row: usize, length: usize) -> Vec<(usize, usize, bool)> {
        row_highlights(&self.matches, self.active, row, length)
    }

    /// Searches `stream` for `query`; returns the match to reveal.
    pub fn search(&mut self, stream: &Stream, query: &str, mode: SearchMode) -> Option<FindMatch> {
        if query.is_empty() {
            self.clear_query();
            return None;
        }
        let revision = stream.revision();
        let same_output = self.revision == revision;
        self.revision = revision;
        let anchor = self
            .active
            .and_then(|index| self.matches.get(index))
            .copied();
        let changed = self.index.sync(stream);
        if self.query != query {
            self.set_query(query, changed.is_none());
        } else if let Some(changed) = changed {
            let from = changed.saturating_sub(self.needle.chars.len() - 1);
            let keep = self.starts.partition_point(|&start| start < from);
            self.starts.truncate(keep);
            self.matches
                .truncate(self.matches.partition_point(|m| m.position < from));
            self.scan_from(from);
            self.project_from(keep);
        }
        let total = self.matches.len();
        let nearest = || anchor.and_then(|anchor| nearest_index(&self.matches, anchor));
        let active = match mode {
            SearchMode::Reset { previous } => next_index(None, total, previous, true),
            SearchMode::Step { previous } => {
                let base = if same_output {
                    self.active.filter(|index| *index < total)
                } else {
                    nearest()
                };
                next_index(base, total, previous, false)
            }
            SearchMode::Refresh => nearest(),
        };
        self.active = active;
        match mode {
            SearchMode::Refresh => None,
            _ => active.map(|index| self.matches[index]),
        }
    }

    fn set_query(&mut self, query: &str, same_index: bool) {
        let chars: Vec<char> = query.chars().map(fold).collect();
        let previous_len = self.needle.chars.len();
        let extends = same_index && previous_len != 0 && chars.starts_with(&self.needle.chars);
        self.query.clear();
        self.query.push_str(query);
        if extends && chars.len() == previous_len {
            return;
        }
        self.needle.replace(chars);
        self.matches.clear();
        if extends {
            let suffix = &self.needle.chars[previous_len..];
            self.starts
                .retain(|&start| self.index.text[start + previous_len..].starts_with(suffix));
        } else {
            self.starts.clear();
            self.scan_from(0);
        }
        self.project_from(0);
    }

    fn scan_from(&mut self, from: usize) {
        if self.index.text.len() - from < self.needle.chars.len() {
            return;
        }
        #[cfg(test)]
        {
            self.chars_scanned += self.index.text.len() - from;
        }
        self.needle.scan(&self.index.text, from, &mut self.starts);
    }

    fn project_from(&mut self, candidate: usize) {
        let len = self.needle.chars.len();
        let mut next = self.matches.last().map_or(0, |m| m.position + len);
        let starts = &self.starts[candidate..];
        let first = starts.partition_point(|&position| position < next);
        let Some(&position) = starts.get(first) else {
            return;
        };
        let mut row = self
            .index
            .rows
            .partition_point(|start| start.position() <= position)
            - 1;
        for &position in &starts[first..] {
            if position < next {
                continue;
            }
            let (start_row, start_offset) = self.index.coordinate(&mut row, position);
            next = position + len;
            let (end_row, end_offset) = self.index.coordinate(&mut row, next - 1);
            self.matches.push(FindMatch {
                start_row,
                start_offset,
                end_row,
                end_offset: end_offset + 1,
                position,
            });
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
        Some(if std::mem::take(&mut self.searching) && !self.searched {
            SearchMode::Reset { previous: false }
        } else {
            SearchMode::Refresh
        })
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
    pub fn handle_actions(
        &mut self,
        cx: &mut Cx,
        root: &WidgetRef,
        actions: &Actions,
    ) -> Vec<FindCommand> {
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
            commands.push(FindCommand::Search(if self.searched {
                SearchMode::Step { previous }
            } else {
                SearchMode::Reset { previous }
            }));
            self.focus_query = true;
        }
        if root.wa_button(id!(find_bar.find_previous)).clicked(actions) {
            commands.push(FindCommand::Search(if self.searched {
                SearchMode::Step { previous: true }
            } else {
                SearchMode::Reset { previous: true }
            }));
        }
        if root.wa_button(id!(find_bar.find_next)).clicked(actions) {
            commands.push(FindCommand::Search(if self.searched {
                SearchMode::Step { previous: false }
            } else {
                SearchMode::Reset { previous: false }
            }));
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
            Some(index) => ta(
                "paneFind.position",
                &[
                    ("current", &(index + 1).to_string()),
                    ("total", &total.to_string()),
                ],
            ),
        }
    }

    /// Presents the card in `wrap` (a `WaPaneFind`) over a body `body_width` wide.
    pub fn present(
        &self,
        cx: &mut Cx,
        wrap: &WidgetRef,
        body_width: f64,
        compact: bool,
        total: usize,
        active: Option<usize>,
    ) {
        wrap.set_visible(cx, self.open);
        if !self.open {
            return;
        }
        let ty = crate::theme::current_typography();
        let field_h = 28.0_f64.max(ty.ui_size(14.0) * 1.4 + 8.0);
        wrap.widget(id!(find_bar.field))
            .apply_over(cx, live! { height: (field_h) });
        // `min(body - 16, 340)`: the card never spills past the pane's left edge.
        let width = if body_width > 0.0 {
            (body_width - 16.0).clamp(0.0, CARD_MAX_WIDTH)
        } else {
            CARD_MAX_WIDTH
        };
        wrap.widget(id!(find_bar))
            .apply_over(cx, live! { width: (width) });
        let button = if compact { 32.0 } else { 26.0 };
        let field = |buttons: f64| width - 12.0 - buttons * (button + 4.0);
        let show_steps = field(3.0) >= FIELD_MIN_WIDTH;
        let query = wrap.wa_input(id!(find_bar.query));
        query.set_empty_text(cx, t("paneFind.placeholder"));
        set_optional(
            cx,
            &wrap.widget(id!(find_bar.match_status)),
            &self.status(total, active),
        );
        let can_navigate = total > 0;
        for (path, tooltip) in [
            (id!(find_bar.find_previous), "paneFind.previous"),
            (id!(find_bar.find_next), "paneFind.next"),
        ] {
            let step = wrap.wa_button(path);
            step.set_visible(cx, show_steps);
            step.set_enabled(cx, can_navigate);
            step.set_tooltip(cx, &t(tooltip));
        }
        wrap.wa_button(id!(find_bar.find_close))
            .set_tooltip(cx, &t("paneFind.close"));
        // The query field's borderAccent outline while focused.
        let focused = query.key_focus(cx);
        wrap.widget(id!(find_bar.field)).apply_over(
            cx,
            live! { border_width: (if focused { 1.0 } else { 0.0 }) },
        );
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

    const RESET: SearchMode = SearchMode::Reset { previous: false };

    fn fresh_matches(stream: &Stream, query: &str) -> Vec<FindMatch> {
        let mut hits = FindHits::default();
        hits.search(stream, query, RESET);
        hits.matches
    }

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
            FindMatch {
                start_row: 0,
                start_offset: 2,
                end_row: 1,
                end_offset: 3,
                position: 2,
            },
            FindMatch {
                start_row: 1,
                start_offset: 5,
                end_row: 1,
                end_offset: 7,
                position: 9,
            },
        ];
        assert_eq!(row_highlights(&matches, Some(1), 0, 4), vec![(2, 4, false)]);
        assert_eq!(
            row_highlights(&matches, Some(1), 1, 10),
            vec![(0, 3, false), (5, 7, true)]
        );
        assert!(row_highlights(&matches, None, 2, 10).is_empty());
    }

    #[test]
    fn terminal_and_log_streams_are_searched_alike() {
        let mut terminal = stream("Error one\r\nok\r\nerror two\r\n");
        let mut hits = FindHits::default();
        let first = hits.search(&terminal, "ERROR", SearchMode::Reset { previous: false });
        assert_eq!(
            first,
            Some(FindMatch {
                start_row: 0,
                start_offset: 0,
                end_row: 0,
                end_offset: 5,
                position: 0,
            })
        );
        assert_eq!(
            hits.search(&terminal, "error", SearchMode::Step { previous: false })
                .map(|m| m.start_row),
            Some(2)
        );
        // New output keeps the active match on the same row instead of jumping to the first.
        terminal.screen.write(b"error three\r\n");
        terminal.publish();
        assert_eq!(hits.search(&terminal, "error", SearchMode::Refresh), None);
        assert_eq!(hits.matches.len(), 3);
        assert_eq!(hits.active, Some(1));
        let log = crate::features::logs::session::open("h", "t", 100).unwrap();
        assert!(fresh_matches(&log, "error").is_empty());
    }

    #[test]
    fn find_cache_reuses_steps_and_refreshes_after_append_and_reflow() {
        let mut terminal = stream("0123456needle!\r\nneEdle");
        let mut hits = FindHits::default();
        let reset = RESET;
        let next = SearchMode::Step { previous: false };
        let previous = SearchMode::Step { previous: true };
        let first = hits.search(&terminal, "needle", reset).unwrap();
        let second = hits.search(&terminal, "needle", next).unwrap();
        assert_ne!(first, second);
        let before_step = (hits.index.rows_read, hits.chars_scanned);
        assert_eq!(hits.search(&terminal, "needle", previous), Some(first));
        assert_eq!((hits.index.rows_read, hits.chars_scanned), before_step);

        terminal.screen.write(b"\r\x1b[2Knot found");
        assert_eq!(hits.search(&terminal, "needle", SearchMode::Refresh), None);
        assert_eq!(hits.matches, vec![first]);
        assert!(hits.chars_scanned > before_step.1);

        terminal.screen.write(b"\r\nNEEDLE again");
        terminal.publish();
        assert_eq!(hits.search(&terminal, "needle", SearchMode::Refresh), None);
        let before_reflow = fresh_matches(&terminal, "needle");
        assert_eq!(hits.matches, before_reflow);
        assert_eq!(hits.matches.len(), 2);
        let before_reflow_reads = hits.index.rows_read;

        assert!(terminal.screen.resize(24, 8).unwrap());
        terminal.publish();
        let after_reflow = fresh_matches(&terminal, "needle");
        assert_ne!(before_reflow, after_reflow);
        assert!(after_reflow.iter().any(|m| m.end_row > m.start_row));
        assert_eq!(
            hits.search(&terminal, "needle", reset),
            after_reflow.first().copied(),
        );
        assert_eq!(hits.matches, after_reflow);
        assert_eq!(
            hits.index.rows_read - before_reflow_reads,
            terminal.line_count(),
        );
        let after_reflow_work = (hits.index.rows_read, hits.chars_scanned);
        hits.search(&terminal, "needle", next);
        assert_eq!(
            (hits.index.rows_read, hits.chars_scanned),
            after_reflow_work
        );

        hits.search(&terminal, "found", reset);
        assert_eq!(hits.matches, fresh_matches(&terminal, "found"));
        assert_eq!(hits.index.rows_read, after_reflow_work.0);
        assert_eq!(
            hits.chars_scanned - after_reflow_work.1,
            hits.index.text.len(),
        );
    }

    #[test]
    fn extending_query_filters_overlapping_candidates_without_reading_rows() {
        let terminal = stream("aaAb aaAc");
        let mut hits = FindHits::default();
        hits.search(&terminal, "aa", RESET);
        let offsets: Vec<_> = hits.matches.iter().map(|m| m.start_offset).collect();
        assert_eq!(offsets, [0, 5]);
        let work = (hits.index.rows_read, hits.chars_scanned);

        let extended = hits.search(&terminal, "AaB", RESET).unwrap();
        assert_eq!((extended.start_offset, extended.end_offset), (1, 4));
        assert_eq!(hits.matches, [extended]);
        assert_eq!((hits.index.rows_read, hits.chars_scanned), work);

        hits.search(&terminal, "", RESET);
        let unrelated = hits.search(&terminal, "ac", RESET).unwrap();
        assert_eq!((unrelated.start_offset, unrelated.end_offset), (7, 9));
        assert_eq!(hits.index.rows_read, work.0);
        assert_eq!(hits.chars_scanned - work.1, hits.index.text.len());
    }

    #[test]
    fn rewriting_an_earlier_active_row_preserves_immutable_history() {
        let mut terminal = session::open("h", "t", 100).unwrap();
        terminal.screen.resize(3, 80).unwrap();
        terminal
            .screen
            .write(b"target\r\nold target\r\nquiet\r\nlast");
        terminal.publish();
        let mut hits = FindHits::default();
        hits.search(&terminal, "target", RESET);
        assert_eq!(hits.matches.len(), 2);
        let first = hits.matches[0];
        let reads = hits.index.rows_read;

        terminal.screen.write(b"\x1b[H\x1b[2Kchanged target");
        terminal.publish();
        hits.search(&terminal, "target", SearchMode::Refresh);
        assert_eq!(hits.matches.len(), 2);
        assert_eq!(hits.matches[0], first);
        assert_eq!(
            (hits.matches[1].start_row, hits.matches[1].start_offset),
            (1, 8)
        );
        assert_eq!(hits.index.rows_read - reads, 3);
    }

    #[test]
    fn append_searches_only_tail_and_finds_match_across_old_boundary() {
        let mut terminal = session::open("h", "t", 400).unwrap();
        terminal.screen.write(b"tail\r\nboundary\r\n");
        terminal.screen.write(&b"ordinary line\r\n".repeat(150));
        terminal.screen.write(b"tail");
        terminal.publish();
        let mut hits = FindHits::default();
        let query = "tail\nboundary";
        let first = hits.search(&terminal, query, RESET).unwrap();
        let old_count = terminal.line_count();
        let old_history = terminal.screen.history();
        let old_len = hits.index.text.len();
        let old_work = (hits.index.rows_read, hits.chars_scanned);

        terminal.screen.write(b"\r\nboundary\r\nnext");
        terminal.publish();
        hits.search(&terminal, query, SearchMode::Refresh);
        assert_eq!(
            hits.matches,
            [
                first,
                FindMatch {
                    start_row: old_count - 1,
                    start_offset: 0,
                    end_row: old_count,
                    end_offset: 8,
                    position: old_len - 4,
                },
            ]
        );
        assert_eq!(hits.active, Some(0));
        assert_eq!(
            hits.index.rows_read - old_work.0,
            terminal.line_count() - old_history
        );
        assert_eq!(
            hits.chars_scanned - old_work.1,
            hits.index.text.len() - old_len + query.len() - 1,
        );
    }

    #[test]
    fn trim_and_erase_then_regrow_rebuild_history_coordinates() {
        let mut terminal = session::open("h", "t", 4).unwrap();
        terminal.screen.resize(3, 80).unwrap();
        terminal
            .screen
            .write(b"target\r\nkeep target\r\n2\r\n3\r\n4\r\n5\r\n6");
        terminal.publish();
        let mut hits = FindHits::default();
        hits.search(&terminal, "target", RESET);
        assert_eq!(hits.matches.len(), 2);
        let reads = hits.index.rows_read;

        terminal.screen.write(b"\r\n7");
        terminal.publish();
        hits.search(&terminal, "target", SearchMode::Refresh);
        let [kept] = hits.matches.as_slice() else {
            panic!("expected only the retained target");
        };
        assert_eq!((kept.start_row, kept.start_offset), (0, 5));
        assert_eq!(hits.index.rows_read - reads, terminal.line_count());
        let reads = hits.index.rows_read;

        terminal
            .screen
            .write(b"\x1b[3J\r\nnew target\r\nrow\r\nrow\r\nrow");
        terminal.publish();
        hits.search(&terminal, "target", SearchMode::Refresh);
        let [new] = hits.matches.as_slice() else {
            panic!("expected only the new target");
        };
        assert_eq!((new.start_row, new.start_offset), (3, 4));
        assert_eq!(hits.index.rows_read - reads, terminal.line_count());
    }

    #[test]
    fn screens_with_equal_revisions_do_not_share_an_index() {
        let first = stream("old target");
        let second = stream("new");
        assert_eq!(first.revision(), second.revision());
        assert_eq!(first.screen.row_revision(), second.screen.row_revision());
        let mut hits = FindHits::default();
        assert!(hits.search(&first, "target", RESET).is_some());
        assert!(hits.search(&second, "target", RESET).is_none());
        assert!(hits.search(&second, "new", RESET).is_some());
    }

    #[test]
    fn growing_last_wrapped_line_preserves_cross_row_offsets() {
        let mut terminal = session::open("h", "t", 100).unwrap();
        terminal.screen.resize(3, 6).unwrap();
        terminal.screen.write(b"012abc");
        terminal.publish();
        let mut hits = FindHits::default();
        assert!(hits.search(&terminal, "cDef", RESET).is_none());

        terminal.screen.write(b"DE");
        terminal.publish();
        assert!(hits.search(&terminal, "cDef", RESET).is_none());
        terminal.screen.write(b"F");
        terminal.publish();
        assert_eq!(
            hits.search(&terminal, "cDef", RESET),
            Some(FindMatch {
                start_row: 0,
                start_offset: 5,
                end_row: 1,
                end_offset: 3,
                position: 5,
            }),
        );
    }

    #[test]
    fn publication_and_title_changes_do_not_rescan_matches() {
        let mut terminal = stream("target");
        let mut hits = FindHits::default();
        let first = hits.search(&terminal, "target", RESET).unwrap();
        let work = (hits.index.rows_read, hits.chars_scanned);

        terminal.publish();
        hits.search(&terminal, "target", SearchMode::Refresh);
        assert_eq!((hits.index.rows_read, hits.chars_scanned), work);
        terminal.screen.write(b"\x1b]2;new title\x07");
        terminal.publish();
        hits.search(&terminal, "target", SearchMode::Refresh);
        assert_eq!(hits.matches, [first]);
        assert_eq!(hits.chars_scanned, work.1);
    }

    #[test]
    fn hard_breaks_keep_the_following_rows_zero_offset() {
        let terminal = stream("x\r\ny");
        let mut hits = FindHits::default();
        let before = hits.search(&terminal, "x\n", RESET).unwrap();
        assert_eq!((before.end_row, before.end_offset), (1, 1));
        let after = hits.search(&terminal, "\ny", RESET).unwrap();
        assert_eq!((after.start_row, after.start_offset), (1, 0));
        assert_eq!((after.end_row, after.end_offset), (1, 1));
    }

    #[test]
    #[ignore = "large scrollback timing benchmark"]
    fn perf_terminal_find_cold_extension_and_append() {
        use std::time::Instant;

        for count in [10_000, 100_000] {
            let mut terminal = session::open("h", "t", count + 128).unwrap();
            let mut output = String::with_capacity(count * 20);
            for row in 0..count {
                output.push_str(if row % 1000 == 0 {
                    "needle found\r\n"
                } else {
                    "background output\r\n"
                });
            }
            terminal.screen.write(output.as_bytes());
            terminal.publish();
            assert_eq!(terminal.line_count(), count + 1);

            let mut hits = FindHits::default();
            let now = Instant::now();
            hits.search(&terminal, "need", RESET);
            let fresh = now.elapsed();
            let now = Instant::now();
            hits.search(&terminal, "needle", RESET);
            let extended = now.elapsed();
            let before = hits.matches.len();

            terminal.screen.write(&b"additional output\r\n".repeat(99));
            terminal.screen.write(b"needle found\r\n");
            terminal.publish();
            let now = Instant::now();
            hits.search(&terminal, "needle", SearchMode::Refresh);
            let refreshed = now.elapsed();
            assert_eq!(hits.matches.len(), before + 1);
            // A different term reuses the folded index instead of rereading rows.
            let now = Instant::now();
            hits.search(&terminal, "found", RESET);
            let unrelated = now.elapsed();
            assert_eq!(hits.matches.len(), before + 1);
            eprintln!(
                "{count} rows: fresh {fresh:?}, extended {extended:?}, refresh {refreshed:?}, unrelated {unrelated:?}"
            );
        }
    }

    #[test]
    fn nearest_prefers_the_same_row() {
        let at = |row, offset| FindMatch {
            start_row: row,
            start_offset: offset,
            end_row: row,
            end_offset: offset + 1,
            position: 0,
        };
        assert_eq!(
            nearest_index(&[at(0, 0), at(4, 9), at(5, 0)], at(4, 2)),
            Some(1)
        );
        assert_eq!(nearest_index(&[at(2, 6), at(4, 4)], at(3, 5)), Some(0));
        assert_eq!(nearest_index(&[], at(4, 2)), None);
    }
}
