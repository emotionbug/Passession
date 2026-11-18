//! Reusable column widths and visual row offsets for the source editor.

use std::ops::Range;

/// Tab stops follow the shared text layout (`TAB_STOP_IN_SPACES`).
const TAB_WIDTH: usize = 4;

#[derive(Default)]
pub(super) struct Geometry {
    pub cols: usize,
    font_size: f64,
    lines: Vec<Option<LineGeometry>>,
    /// Cumulative visual rows before each line (len = lines + 1 after update).
    pub prefix: Vec<usize>,
    /// The old prefix remains usable for scroll bounds until it is updated.
    lines_shifted: bool,
    pub max_cols: usize,
    max_dirty: bool,
    dirty: Option<Range<usize>>,
    #[cfg(test)]
    pub measured_lines: usize,
}

struct LineGeometry {
    width: usize,
    /// Empty for a line with no wrapping; otherwise includes the initial 0.
    starts: Vec<usize>,
}

impl Geometry {
    /// Keep the cache aligned to line indices, including when an edit splits or joins lines.
    pub fn invalidate(&mut self, old: Range<usize>, added: usize) {
        if self.lines.is_empty() {
            return;
        }
        if !self.max_dirty {
            self.max_dirty = self.lines[old.clone()]
                .iter()
                .flatten()
                .any(|line| line.width == self.max_cols);
        }
        if old.len() != added {
            self.lines
                .splice(old.clone(), std::iter::repeat_with(|| None).take(added));
            self.lines_shifted = true;
        } else {
            self.lines[old.clone()].fill_with(|| None);
        }
        self.dirty = Some(match self.dirty.take() {
            Some(dirty) => dirty.start.min(old.start)..(dirty.end.max(old.end) - old.len() + added),
            None => old.start..old.start + added,
        });
    }

    pub fn update(&mut self, text: &str, starts: &[usize], cols: usize, font_size: f64) -> bool {
        if self.lines.is_empty() || self.cols != cols || self.font_size != font_size {
            self.cols = cols;
            self.font_size = font_size;
            self.lines.clear();
            self.lines.resize_with(starts.len(), || None);
            self.prefix.clear();
            self.prefix.push(0);
            self.lines_shifted = false;
            self.max_cols = 0;
            self.max_dirty = false;
            self.dirty = Some(0..starts.len());
        }
        let Some(dirty) = self.dirty.take() else {
            return false;
        };
        let previous_end = if std::mem::take(&mut self.lines_shifted) {
            None
        } else {
            self.prefix.get(dirty.end).copied()
        };
        self.prefix.resize(starts.len() + 1, 0);
        let mut rows = self.prefix[dirty.start];
        let mut updated_max = 0;
        for line in dirty.clone() {
            let metrics = self.lines[line].get_or_insert_with(|| {
                #[cfg(test)]
                {
                    self.measured_lines += 1;
                }
                let end = starts.get(line + 1).map_or(text.len(), |next| next - 1);
                let line = &text[starts[line]..end];
                let width = line_cols(line);
                let starts = if cols != 0 && width > cols {
                    wrap_line(line, cols)
                } else {
                    Vec::new()
                };
                LineGeometry { width, starts }
            });
            updated_max = updated_max.max(metrics.width);
            rows += metrics.starts.len().max(1);
            self.prefix[line + 1] = rows;
        }
        if let Some(previous_end) = previous_end {
            if rows != previous_end {
                for prefix in &mut self.prefix[dirty.end + 1..] {
                    *prefix = *prefix - previous_end + rows;
                }
            }
        } else {
            // A split or join changed line indices. Rebuild the suffix sums from
            // cached row counts without touching the surviving lines' text.
            for line in dirty.end..starts.len() {
                rows += self.row_starts(line).len();
                self.prefix[line + 1] = rows;
            }
        }
        self.max_cols = if std::mem::take(&mut self.max_dirty) && updated_max < self.max_cols {
            self.lines
                .iter()
                .flatten()
                .map(|line| line.width)
                .max()
                .unwrap_or(0)
        } else {
            self.max_cols.max(updated_max)
        };
        true
    }

    pub fn row_starts(&self, line: usize) -> &[usize] {
        let starts = &self.lines[line].as_ref().unwrap().starts;
        if starts.is_empty() { &[0] } else { starts }
    }
}

/// Columns `c` takes when it starts at `column` of its row: a tab reaches the next tab stop.
pub(super) fn char_cols(c: char, column: usize) -> usize {
    let code = c as u32;
    let wide = matches!(code,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x1F900..=0x1F9FF | 0x20000..=0x3FFFD);
    if c == '\t' {
        TAB_WIDTH - column % TAB_WIDTH
    } else if wide {
        2
    } else {
        1
    }
}

pub(super) fn line_cols(line: &str) -> usize {
    if line.is_ascii() && !line.contains('\t') {
        return line.len();
    }
    line.chars()
        .fold(0, |column, c| column + char_cols(c, column))
}

/// Row starts of `line` wrapped at `cols` columns, breaking after whitespace when possible.
pub(super) fn wrap_line(line: &str, cols: usize) -> Vec<usize> {
    let mut starts = vec![0];
    if cols == 0 {
        return starts;
    }
    let mut row_cols = 0;
    let mut last_break: Option<usize> = None;
    let mut row_start = 0;
    for (index, c) in line.char_indices() {
        let mut width = char_cols(c, row_cols);
        if row_cols + width > cols && index > row_start {
            let start = match last_break {
                Some(brk) if brk > row_start => brk,
                _ => index,
            };
            starts.push(start);
            row_start = start;
            row_cols = line_cols(&line[start..index]);
            last_break = None;
            width = char_cols(c, row_cols);
        }
        row_cols += width;
        if c == ' ' || c == '\t' {
            last_break = Some(index + c.len_utf8());
        }
    }
    starts
}
