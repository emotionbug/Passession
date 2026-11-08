//! Plain terminal content read out of the emulator.

/// A cell colour: the terminal default, one of the 256 indexed colours, or
/// 24-bit RGB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The SGR attributes Paseo presents and serializes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[allow(clippy::struct_excessive_bools)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
    pub hidden: bool,
}

impl CellStyle {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellWidth {
    Narrow,
    /// The first cell of a two-column character.
    Wide,
    /// The second column of a wide character, or the padding a wide character
    /// left at the end of a soft-wrapped row. Holds no text.
    Spacer,
}

/// One grid column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowCell {
    /// The grapheme cluster; empty for an erased cell or a spacer.
    pub text: String,
    pub width: CellWidth,
    pub style: CellStyle,
}

impl RowCell {
    /// Erased: no visible text, default background and not inverse, so it
    /// renders exactly like the area past the end of the row.
    pub fn is_erased(&self) -> bool {
        (self.text.is_empty() || self.text == " ") && self.style.bg == Color::Default && !self.style.inverse
    }
}

/// One grid row: exactly `cols` cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub cells: Vec<RowCell>,
    /// The row soft-wraps into the next one.
    pub wrapped: bool,
}

impl Row {
    /// Cell text with erased cells as spaces and spacers omitted.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for cell in &self.cells {
            match cell.width {
                CellWidth::Spacer => {}
                _ if cell.text.is_empty() => text.push(' '),
                _ => text.push_str(&cell.text),
            }
        }
        text
    }
}

/// One run of identically styled cells.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    /// Width of the run in terminal cells.
    pub cells: usize,
    pub style: CellStyle,
}

/// One presented row: styled runs, its plain text and per-char columns.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub runs: Vec<Run>,
    /// Plain text of the row; concatenation of the run texts.
    pub text: String,
    /// Cell column where each char of `text` starts (one entry per char).
    pub columns: Vec<usize>,
    /// The row soft-wraps into the next one.
    pub wraps_next: bool,
}

impl Line {
    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Trailing erased cells are trimmed unless the row soft-wraps; erased
    /// interior cells read as spaces; wide-character spacers widen the run of
    /// their glyph.
    pub fn from_row(row: &Row) -> Self {
        let mut length = row.cells.len();
        if !row.wrapped {
            while length > 0 {
                let cell = &row.cells[length - 1];
                if !cell.is_erased() && cell.width != CellWidth::Spacer {
                    break;
                }
                length -= 1;
            }
        }
        let mut line = Line { wraps_next: row.wrapped, ..Default::default() };
        let mut current: Option<Run> = None;
        for (col, cell) in row.cells[..length].iter().enumerate() {
            if cell.width == CellWidth::Spacer {
                if let Some(run) = current.as_mut() {
                    run.cells += 1;
                }
                continue;
            }
            if current.as_ref().is_none_or(|run| run.style != cell.style) {
                if let Some(run) = current.take() {
                    line.runs.push(run);
                }
                current = Some(Run { text: String::new(), cells: 0, style: cell.style });
            }
            let run = current.as_mut().expect("run started above");
            let text = if cell.text.is_empty() { " " } else { cell.text.as_str() };
            for ch in text.chars() {
                line.columns.push(col);
                run.text.push(ch);
                line.text.push(ch);
            }
            run.cells += 1;
        }
        if let Some(run) = current {
            line.runs.push(run);
        }
        line
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
    BlockHollow,
}

/// The cursor in active-screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    /// The next printed character soft-wraps first; the cursor sits past the
    /// last column.
    pub pending_wrap: bool,
    pub visible: bool,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str, width: CellWidth, style: CellStyle) -> RowCell {
        RowCell { text: text.to_owned(), width, style }
    }

    #[test]
    fn trailing_erased_cells_trim_but_coloured_and_wrapped_cells_stay() {
        let plain = CellStyle::default();
        let red_bg = CellStyle { bg: Color::Indexed(1), ..plain };
        let row = Row {
            cells: vec![cell("a", CellWidth::Narrow, plain), cell("", CellWidth::Narrow, plain), cell("", CellWidth::Narrow, red_bg), cell("", CellWidth::Narrow, plain)],
            wrapped: false,
        };
        let line = Line::from_row(&row);
        assert_eq!(line.text, "a  ");
        assert_eq!(line.columns, vec![0, 1, 2]);
        assert_eq!(line.runs.len(), 2);
        let wrapped = Line::from_row(&Row { wrapped: true, ..row });
        assert_eq!(wrapped.text, "a   ");
        assert!(wrapped.wraps_next);
    }

    #[test]
    fn wide_spacers_widen_their_run_and_combining_marks_share_a_column() {
        let plain = CellStyle::default();
        let row = Row {
            cells: vec![cell("한", CellWidth::Wide, plain), cell("", CellWidth::Spacer, plain), cell("e\u{301}", CellWidth::Narrow, plain)],
            wrapped: false,
        };
        let line = Line::from_row(&row);
        assert_eq!(line.text, "한e\u{301}");
        assert_eq!(line.columns, vec![0, 2, 2]);
        assert_eq!(line.runs[0].cells, 3);
        assert_eq!(row.text(), "한e\u{301}");
    }
}
