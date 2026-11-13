//! Clickable links in terminal and execution output: `http(s)://` URLs found in the
//! presented text (soft-wrapped rows joined into their logical line), opened with
//! Ctrl/Cmd+click on desktop and a tap on touch screens.

use paseo_terminal::Line;
use unicode_width::UnicodeWidthChar;

use super::stream::Stream;

const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Rows a wrapped logical line is followed across, either way, when resolving a link.
pub const MAX_WRAPPED_ROWS: usize = 32;

/// Characters that end a URL in terminal output.
fn ends_url(ch: char) -> bool {
    ch.is_whitespace() || ch.is_control() || matches!(ch, '<' | '>' | '"' | '\'' | '`' | '{' | '}' | '|' | '\\' | '^')
}

/// Trailing punctuation that belongs to the sentence, and closers without an opener in the
/// URL (`(see https://x.dev)`).
fn trim_end(chars: &[char]) -> usize {
    let mut end = chars.len();
    loop {
        let Some(&last) = chars[..end].last() else { return end };
        let unbalanced = |open: char, close: char| {
            last == close && chars[..end].iter().filter(|c| **c == close).count() > chars[..end].iter().filter(|c| **c == open).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '*') || unbalanced('(', ')') || unbalanced('[', ']') {
            end -= 1;
        } else {
            return end;
        }
    }
}

/// Every URL in `text` as char ranges `start..end`.
pub fn url_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let scheme = SCHEMES.iter().find(|scheme| {
            let len = scheme.len();
            index + len <= chars.len() && chars[index..index + len].iter().zip(scheme.chars()).all(|(a, b)| a.to_ascii_lowercase() == b)
        });
        let Some(scheme) = scheme else {
            index += 1;
            continue;
        };
        // A scheme glued to a word (`xhttp://`) is not a link start.
        if index > 0 && chars[index - 1].is_alphanumeric() {
            index += 1;
            continue;
        }
        let body_start = index + scheme.len();
        let mut end = body_start;
        while end < chars.len() && !ends_url(chars[end]) {
            end += 1;
        }
        let end = index + trim_end(&chars[index..end]);
        if end > body_start {
            ranges.push((index, end));
        }
        index = end.max(index + 1);
    }
    ranges
}

/// The URL covering char `offset` of `text`.
pub fn url_at(text: &str, offset: usize) -> Option<String> {
    let (start, end) = url_ranges(text).into_iter().find(|(start, end)| (*start..*end).contains(&offset))?;
    Some(text.chars().skip(start).take(end - start).collect())
}

/// The char index of `line` drawn in cell column `col` (a wide glyph covers two columns).
pub fn char_at_column(line: &Line, col: usize) -> Option<usize> {
    let index = line.columns.partition_point(|c| *c <= col).checked_sub(1)?;
    let start = line.columns[index];
    let next = match line.columns.get(index + 1) {
        Some(next) => *next,
        None => start + line.text.chars().nth(index).map(|ch| ch.width().unwrap_or(1)).unwrap_or(1),
    };
    (col < next.max(start + 1)).then_some(index)
}

/// The link under cell `col` of `rows[target]`, where `rows` are consecutive presented rows of
/// one logical line (each but the last wrapping into the next).
pub fn link_in_rows(rows: &[Line], target: usize, col: usize) -> Option<String> {
    let line = rows.get(target)?;
    let index = char_at_column(line, col)?;
    let before: usize = rows[..target].iter().map(|row| row.text.chars().count()).sum();
    let text: String = rows.iter().map(|row| row.text.as_str()).collect();
    url_at(&text, before + index)
}

/// The link at presented `row`, cell `col` of a stream, following the row's soft wraps.
pub fn link_at(stream: &mut Stream, row: usize, col: usize) -> Option<String> {
    let total = stream.screen.total_rows();
    if row >= total {
        return None;
    }
    let wraps = |stream: &mut Stream, row: usize| stream.read(row, 1).first().is_some_and(|line| line.wraps_next);
    let mut first = row;
    while first > 0 && row - first < MAX_WRAPPED_ROWS && wraps(stream, first - 1) {
        first -= 1;
    }
    let mut last = row;
    while last + 1 < total && last - row < MAX_WRAPPED_ROWS && wraps(stream, last) {
        last += 1;
    }
    let rows: Vec<Line> = stream.read(first, last - first + 1).into_iter().cloned().collect();
    link_in_rows(&rows, row - first, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_terminal::{CellStyle, Run};

    fn line(text: &str, wraps_next: bool) -> Line {
        Line {
            text: text.into(),
            columns: (0..text.chars().count()).collect(),
            runs: vec![Run { text: text.into(), cells: text.chars().count(), style: CellStyle::default() }],
            wraps_next,
        }
    }

    #[test]
    fn urls_end_at_spaces_and_sentence_punctuation() {
        let text = "Local:   http://localhost:5173/ (or https://example.com/a_(b)), see https://x.dev.";
        let urls: Vec<String> = url_ranges(text).into_iter().map(|(s, e)| text.chars().skip(s).take(e - s).collect()).collect();
        assert_eq!(urls, ["http://localhost:5173/", "https://example.com/a_(b)", "https://x.dev"]);
        assert_eq!(url_at(text, 12).as_deref(), Some("http://localhost:5173/"));
        assert_eq!(url_at(text, 2), None);
        assert!(url_ranges("xhttp://nope https://").is_empty());
        assert_eq!(url_at("<HTTPS://A.B/c>", 3).as_deref(), Some("HTTPS://A.B/c"));
    }

    #[test]
    fn links_span_soft_wrapped_rows_and_wide_cells() {
        let rows = [line("open https://exa", true), line("mple.com/x now", false)];
        assert_eq!(link_in_rows(&rows, 1, 2).as_deref(), Some("https://example.com/x"));
        assert_eq!(link_in_rows(&rows, 0, 7).as_deref(), Some("https://example.com/x"));
        assert_eq!(link_in_rows(&rows, 1, 12), None);
        let wide = Line { text: "한 http://a.b".into(), columns: vec![0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12], runs: Vec::new(), wraps_next: false };
        assert_eq!(char_at_column(&wide, 1), Some(0));
        assert_eq!(char_at_column(&wide, 3), Some(2));
        assert_eq!(link_in_rows(&[wide], 0, 5).as_deref(), Some("http://a.b"));
    }
}
