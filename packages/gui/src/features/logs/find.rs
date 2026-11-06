//! Find in execution output (`_ExecutionLogPanelState._find`): case-insensitive search over
//! logical lines so a match split by the PTY width across soft-wrapped rows is still found.

use super::session::LogSession;
use super::view::LogMatch;

/// Every match of `query` in the presented rows, ordered by position.
pub fn find_matches(session: &mut LogSession, query: &str) -> Vec<LogMatch> {
    if query.is_empty() {
        return Vec::new();
    }
    let count = session.line_count();
    // The searchable text and, per char, its (row, char offset within the row).
    let mut text: Vec<char> = Vec::new();
    let mut offsets: Vec<(usize, usize)> = Vec::new();
    let mut previous_wraps = false;
    let mut start = 0;
    while start < count {
        let chunk = session.read(start, 1024.min(count - start));
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
        matches.push(LogMatch { start_row, start_offset, end_row, end_offset: end_offset + 1 });
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
    let current = current.map(|c| c as isize).unwrap_or(-1);
    let step = if previous { -1 } else { 1 };
    Some(((current + step).rem_euclid(total as isize)) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_index_wraps_both_ways() {
        assert_eq!(next_index(None, 3, false, true), Some(0));
        assert_eq!(next_index(None, 3, true, true), Some(2));
        assert_eq!(next_index(Some(2), 3, false, false), Some(0));
        assert_eq!(next_index(Some(0), 3, true, false), Some(2));
        assert_eq!(next_index(Some(0), 0, true, false), None);
    }

    #[test]
    fn fold_keeps_single_char_mappings() {
        assert_eq!(fold('A'), 'a');
        assert_eq!(fold('İ'), 'İ');
    }
}
