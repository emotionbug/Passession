//! The presented screen of one daemon terminal: a Ghostty emulator fed with the stream's
//! output, its title, and the rows the views read.
//!
//! The client never answers terminal queries (`respond: false`): the daemon's own screen
//! replies to the PTY, so a second answer from any observer would corrupt the program's
//! input. Row reads cross the Ghostty FFI once per cell, so presented lines are cached
//! until the next mutation (output, resize, replacement).

use std::collections::HashMap;

use paseo_terminal::{Cursor, Emulator, Error, Key, Line, Modes, Modifiers, Options};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Cached rows beyond this are dropped instead of growing with every scrolled-past row.
const CACHE_LIMIT: usize = 4096;

pub struct Screen {
    emulator: Emulator,
    /// Presented scrollback rows (`Options::scrollback`).
    scrollback: usize,
    /// Presented lines by row index, valid until the next mutation.
    lines: HashMap<usize, Line>,
    title: String,
    revision: u64,
    line_count: usize,
    has_dimensions: bool,
}

fn emulator(rows: u16, cols: u16, scrollback: usize) -> Result<Emulator, Error> {
    Emulator::new(Options { rows, cols, scrollback, respond: false })
}

impl Screen {
    pub fn new(scrollback: usize) -> Result<Self, Error> {
        Ok(Self {
            emulator: emulator(DEFAULT_ROWS, DEFAULT_COLS, scrollback)?,
            scrollback,
            lines: HashMap::new(),
            title: String::new(),
            revision: 0,
            line_count: 0,
            has_dimensions: false,
        })
    }

    /// Feeds process output through the VT parser.
    pub fn write(&mut self, bytes: &[u8]) {
        self.emulator.write(bytes);
        self.lines.clear();
    }

    /// Replaces the screen with a snapshot or restore rendering, at the snapshot's size or
    /// the current one.
    pub fn replace(&mut self, text: &str, size: Option<(u16, u16)>, title: Option<&str>) -> Result<(), Error> {
        let (rows, cols) = size.unwrap_or((self.rows(), self.cols()));
        self.emulator = emulator(rows, cols, self.scrollback)?;
        self.lines.clear();
        self.has_dimensions = true;
        self.emulator.write(text.as_bytes());
        if let Some(title) = title.filter(|title| !title.is_empty()) {
            self.title = title.to_owned();
        }
        self.publish();
        Ok(())
    }

    /// Resizes the grid (the primary screen reflows); returns whether the size changed.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<bool, Error> {
        if rows == self.rows() && cols == self.cols() {
            return Ok(false);
        }
        self.emulator.resize(rows, cols)?;
        self.lines.clear();
        Ok(true)
    }

    /// Applies the effects of written output: the OSC title, the line count and a new
    /// revision for the views.
    pub fn publish(&mut self) {
        if let Some(title) = self.emulator.take_title() {
            self.title = title;
        }
        self.line_count = self.count_lines();
        self.revision += 1;
    }

    pub fn rows(&self) -> u16 {
        self.emulator.rows()
    }

    pub fn cols(&self) -> u16 {
        self.emulator.cols()
    }

    /// A snapshot or restore set the size; output alone never does.
    pub fn has_dimensions(&self) -> bool {
        self.has_dimensions
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Presented rows through the cursor and the last non-blank row (`_countLines`), as of
    /// the last publish.
    pub fn line_count(&self) -> usize {
        self.line_count
    }

    /// Presented scrollback rows above the screen.
    pub fn history(&self) -> usize {
        self.emulator.history()
    }

    /// Presented scrollback plus screen rows.
    pub fn total_rows(&self) -> usize {
        self.emulator.total_rows()
    }

    pub fn cursor(&mut self) -> Cursor {
        self.emulator.cursor()
    }

    pub fn modes(&self) -> Modes {
        self.emulator.modes()
    }

    pub fn encode_key(&mut self, key: Key, modifiers: Modifiers) -> Vec<u8> {
        self.emulator.encode_key(key, modifiers)
    }

    pub fn encode_mouse_wheel(&mut self, up: bool, col: u16, row: u16, modifiers: Modifiers, count: usize) -> Vec<u8> {
        self.emulator.encode_mouse_wheel(up, col, row, modifiers, count)
    }

    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        self.emulator.encode_paste(text)
    }

    fn count_lines(&mut self) -> usize {
        let history = self.history();
        let total = self.total_rows();
        let cursor = usize::from(self.emulator.cursor().row);
        let minimum = total.min(history + cursor + 1);
        let mut count = total;
        while count > minimum {
            if !self.read(count - 1, 1).first().is_some_and(|line| line.is_blank()) {
                break;
            }
            count -= 1;
        }
        count
    }

    /// Presented rows `start..start+count` (scrollback first, then the screen), cached.
    pub fn read(&mut self, start: usize, count: usize) -> Vec<&Line> {
        let end = start.saturating_add(count).min(self.total_rows());
        if self.lines.len() > CACHE_LIMIT {
            self.lines.clear();
        }
        for row in start..end {
            self.lines.entry(row).or_insert_with(|| self.emulator.line(row));
        }
        (start..end).map(|row| &self.lines[&row]).collect()
    }

    /// Presented rows `start..start+count` read past the cache: bulk readers (find, copy
    /// all) visit every row once.
    pub fn read_uncached(&self, start: usize, count: usize) -> Vec<Line> {
        let end = start.saturating_add(count).min(self.total_rows());
        (start..end).map(|row| self.lines.get(&row).cloned().unwrap_or_else(|| self.emulator.line(row))).collect()
    }

    /// `renderedText`: the first `count` presented rows as logical lines, soft wraps
    /// rejoined.
    pub fn rendered_text(&self, count: usize) -> String {
        let mut text = String::new();
        let mut previous_wraps = false;
        let mut start = 0;
        while start < count {
            let chunk = self.read_uncached(start, 512.min(count - start));
            if chunk.is_empty() {
                break;
            }
            for (index, line) in chunk.iter().enumerate() {
                if start + index > 0 && !previous_wraps {
                    text.push('\n');
                }
                text.push_str(&line.text);
                previous_wraps = line.wraps_next;
            }
            start += chunk.len();
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_with(text: &str) -> Screen {
        let mut screen = Screen::new(100).unwrap();
        screen.write(text.as_bytes());
        screen.publish();
        screen
    }

    #[test]
    fn carriage_returns_and_erasure_are_interpreted() {
        let mut screen = screen_with("progress 10%\rprogress 99%\r\nnext\x1b[2K\rdone\r\n");
        assert_eq!(screen.line_count(), 3);
        let lines = screen.read(0, 2);
        assert_eq!(lines[0].text, "progress 99%");
        assert_eq!(lines[1].text, "done");
    }

    #[test]
    fn history_rows_precede_screen_rows() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("line {i}\r\n"));
        }
        let mut screen = screen_with(&text);
        assert_eq!(screen.line_count(), 41);
        let lines = screen.read(0, 41);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[39].text, "line 39");
        assert!(screen.rendered_text(41).starts_with("line 0\nline 1\n"));
    }

    #[test]
    fn cached_rows_follow_new_output() {
        let mut screen = screen_with("old");
        assert_eq!(screen.read(0, 1)[0].text, "old");
        screen.write(b"\rnew");
        assert_eq!(screen.read(0, 1)[0].text, "new");
    }

    #[test]
    fn styled_runs_split_on_sgr() {
        let mut screen = screen_with("a\x1b[31mb\x1b[0mc");
        let line = screen.read(0, 1)[0].clone();
        assert_eq!(line.runs.len(), 3);
        assert_eq!(line.runs[1].style.fg, paseo_terminal::Color::Indexed(1));
        assert_eq!(line.text, "abc");
    }

    #[test]
    fn strikethrough_hidden_and_wide_cells_are_kept() {
        let mut screen = screen_with("a\x1b[9mb\x1b[29;8mc\x1b[0m好d");
        let line = screen.read(0, 1)[0].clone();
        let runs = &line.runs;
        assert!(runs[1].style.strike && !runs[1].style.hidden);
        assert!(runs[2].style.hidden && !runs[2].style.strike);
        assert_eq!(runs[3].text, "好d");
        assert_eq!(runs[3].cells, 3);
        assert_eq!(line.columns, vec![0, 1, 2, 3, 5]);
    }

    #[test]
    fn all_sgr_attributes_survive_restore_and_reset() {
        let mut screen = screen_with("");
        screen.replace("\x1b[1;2;3;4;7;8;9mstyled\x1b[0mnormal", Some((3, 40)), None).unwrap();
        let line = screen.read(0, 1)[0].clone();
        let styled = &line.runs[0].style;
        assert!(styled.bold && styled.dim && styled.italic && styled.underline);
        assert!(styled.inverse && styled.hidden && styled.strike);
        let normal = &line.runs[1].style;
        assert!(normal.is_default());
        assert_eq!(screen.rendered_text(screen.line_count()), "stylednormal");
        assert_eq!((screen.rows(), screen.cols()), (3, 40));
        screen.replace("new", None, None).unwrap();
        assert_eq!((screen.rows(), screen.cols()), (3, 40));
        assert_eq!(screen.rendered_text(screen.line_count()), "new");
    }

    #[test]
    fn presented_history_is_bounded_and_alternate_screen_restores_primary() {
        let mut screen = screen_with("");
        screen.replace("", Some((2, 16)), None).unwrap();
        for i in 0..400 {
            screen.write(format!("{i}\r\n").as_bytes());
        }
        screen.publish();
        assert!(screen.history() <= 100);
        assert_eq!(screen.read(screen.total_rows() - 2, 1)[0].text, "399");
        let primary = screen.rendered_text(screen.line_count());
        screen.write(b"\x1b[?1049h\x1b[Halt");
        screen.publish();
        assert_eq!(screen.read(0, 1)[0].text, "alt");
        screen.write(b"\x1b[?1049l");
        screen.publish();
        assert_eq!(screen.rendered_text(screen.line_count()), primary);
    }

    #[test]
    fn titles_come_from_osc() {
        let screen = screen_with("\x1b]2;build\x07ok");
        assert_eq!(screen.title(), "build");
    }

    #[test]
    fn queries_are_never_answered() {
        let mut screen = screen_with("\x1b[c\x1b[6n");
        assert_eq!(screen.read(0, 1)[0].text, "");
        assert!(screen.emulator.take_replies().is_empty());
    }
}
