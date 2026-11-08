//! Native terminal grid and the Paseo wire projection of it.
//!
//! Ghostty (`paseo_terminal`) owns VT parsing, query replies, scrolling,
//! character sets, reflow and both screen buffers. A second, incremental VTE
//! parser observes only the Paseo shell protocol and input-mode extensions
//! Ghostty does not model; it never attempts to emulate the display.

use std::fmt::Write as _;

use paseo_terminal::{
    CellStyle, CellWidth, Color, Cursor, CursorShape, Emulator, Options, Row, RowCell,
    ThreadBound,
};
use serde_json::{Value, json};

const HISTORY_LINES: usize = 1000;
const MAX_VISIBLE_HISTORY: usize = 500;

#[derive(Debug, Default)]
pub struct ScreenEffects {
    pub replies: Vec<u8>,
    /// OSC 633;D without a status is a completion with an unknown exit code.
    pub command_finished: Vec<Option<i32>>,
    /// An empty title explicitly clears the previous title.
    pub title: Option<String>,
}

/// Ghostty objects are `!Send`; the emulator lives on its own thread and every
/// call is serialized there. The screen thread never locks anything of the
/// caller's, so calling it under the terminal's display lock cannot deadlock.
pub struct Screen {
    state: ThreadBound<State>,
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Result<Self, paseo_terminal::Error> {
        Ok(Self {
            state: ThreadBound::spawn("paseo-screen", move || State::new(rows, cols))?,
        })
    }

    pub fn process(&mut self, bytes: &[u8]) -> ScreenEffects {
        let bytes = bytes.to_vec();
        self.state.call(move |state| state.process(&bytes))
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), paseo_terminal::Error> {
        let (rows, cols) = size(rows, cols);
        self.state
            .call(move |state| state.emulator.resize(rows, cols))
    }

    pub fn snapshot(&self, wrapped: bool) -> Value {
        self.state.call(move |state| state.snapshot(wrapped))
    }

    /// Matches TerminalInputModeTracker.getPreamble, including its ordering and
    /// replacement (rather than bitwise) semantics for CSI = flags ; mode u.
    pub fn replay_preamble(&self) -> String {
        self.state.call(|state| state.replay_preamble())
    }

    /// ANSI rendering follows protocol/terminal-snapshot.ts. The caller resets
    /// its rendering surface before applying a snapshot; live restores carry
    /// only the input preamble. The caller supplies 200 for an absent visible
    /// scrollback limit. Full snapshots always contain all retained history.
    pub fn restore(&self, mode: &str, scrollback: usize) -> Vec<u8> {
        let mode = mode.to_owned();
        self.state
            .call(move |state| state.restore(&mode, scrollback))
    }

    /// Source capture is plain rendered cell text even with stripAnsi=false:
    /// SGR is not synthesized. Bounds address physical history+viewport rows,
    /// are inclusive, and are compared before JavaScript slice truncation.
    pub fn capture(
        &self,
        start: Option<f64>,
        end: Option<f64>,
        _strip: bool,
    ) -> (Vec<String>, usize) {
        self.state.call(move |state| state.capture(start, end))
    }

    pub(crate) fn exit_lines(&self, limit: usize) -> Vec<String> {
        self.state.call(move |state| state.exit_lines(limit))
    }
}

fn size(rows: u16, cols: u16) -> (u16, u16) {
    // Like xterm, keep space for a full-width character even for a 1-column
    // size request. Empty dimensions are never valid terminal geometry.
    (rows.max(1), cols.max(2))
}

/// Presented rows addressed physically: `0..history` is scrollback (at most
/// `HISTORY_LINES`), `history..history + rows` the screen.
#[derive(Clone, Copy)]
struct Extent {
    history: usize,
    rows: usize,
}

impl Extent {
    fn total(self) -> usize {
        self.history + self.rows
    }
}

struct State {
    emulator: Emulator,
    parser: vte::Parser,
    extensions: Extensions,
    title: Option<String>,
}

impl State {
    fn new(rows: u16, cols: u16) -> Result<Self, paseo_terminal::Error> {
        let (rows, cols) = size(rows, cols);
        Ok(Self {
            emulator: Emulator::new(Options {
                rows,
                cols,
                scrollback: HISTORY_LINES,
                respond: true,
            })?,
            parser: vte::Parser::new(),
            extensions: Extensions::default(),
            title: None,
        })
    }

    fn process(&mut self, bytes: &[u8]) -> ScreenEffects {
        self.parser.advance(&mut self.extensions, bytes);
        self.emulator.write(bytes);
        let current = self
            .emulator
            .title()
            .filter(|title| !title.trim().is_empty());
        // A reset clears Ghostty's title without a change notification.
        let title = self
            .emulator
            .take_title()
            .or_else(|| (current != self.title).then(|| current.clone().unwrap_or_default()));
        self.title = current;
        ScreenEffects {
            replies: self.emulator.take_replies(),
            command_finished: std::mem::take(&mut self.extensions.command_finished),
            title,
        }
    }

    fn extent(&self) -> Extent {
        Extent {
            history: self.emulator.history(),
            rows: usize::from(self.emulator.rows()),
        }
    }

    fn snapshot(&mut self, wrapped: bool) -> Value {
        let extent = self.extent();
        let scrollback: Vec<Row> = (0..extent.history)
            .map(|row| self.emulator.row(row))
            .collect();
        let grid: Vec<Row> = (extent.history..extent.total())
            .map(|row| self.emulator.row(row))
            .collect();
        let mut state = json!({
            "rows": extent.rows,
            "cols": self.emulator.cols(),
            "grid": grid.iter().map(row_json).collect::<Vec<_>>(),
            "scrollback": scrollback.iter().map(row_json).collect::<Vec<_>>(),
            "cursor": self.cursor_json(),
        });
        if let Some(title) = &self.title {
            state["title"] = json!(title);
        }
        if wrapped {
            state["gridWrapped"] = json!(grid.iter().map(|row| row.wrapped).collect::<Vec<_>>());
            state["scrollbackWrapped"] =
                json!(scrollback.iter().map(|row| row.wrapped).collect::<Vec<_>>());
        }
        state
    }

    fn cursor_json(&mut self) -> Value {
        let cursor = self.emulator.cursor();
        let (row, col) = cursor_position(&cursor);
        let mut value = json!({ "row": row, "col": col });
        if !cursor.visible {
            value["hidden"] = json!(true);
        }
        if let Some(style) = self.cursor_style(&cursor) {
            value["style"] = json!(style);
        }
        if let Some(blink) = self.cursor_blink(&cursor) {
            value["blink"] = json!(blink);
        }
        value
    }

    /// Presentation is reported only once a program chose it (DECSCUSR, or
    /// DECSET 12 for blinking) since the last reset.
    fn cursor_style(&self, cursor: &Cursor) -> Option<&'static str> {
        self.extensions.cursor_style.then_some(match cursor.shape {
            CursorShape::Underline => "underline",
            CursorShape::Bar => "bar",
            CursorShape::Block | CursorShape::BlockHollow => "block",
        })
    }

    fn cursor_blink(&self, cursor: &Cursor) -> Option<bool> {
        self.extensions.cursor_blink.then_some(cursor.blinking)
    }

    fn replay_preamble(&self) -> String {
        let modes = self.emulator.modes();
        let mut result = String::new();
        if modes.kitty_flags > 0 {
            let _ = write!(result, "\x1b[={};1u", modes.kitty_flags);
        }
        if self.extensions.win32 {
            result.push_str("\x1b[?9001h");
        }
        if modes.application_cursor {
            result.push_str("\x1b[?1h");
        }
        if modes.bracketed_paste {
            result.push_str("\x1b[?2004h");
        }
        result
    }

    fn restore(&mut self, mode: &str, scrollback: usize) -> Vec<u8> {
        if mode == "live" {
            return self.replay_preamble().into_bytes();
        }
        let extent = self.extent();
        let history = if mode == "visible-snapshot" {
            extent.history.min(scrollback.min(MAX_VISIBLE_HISTORY))
        } else {
            extent.history
        };
        let mut output = String::new();
        let last = extent.total() - 1;
        for physical in extent.history - history..=last {
            let row = self.emulator.row(physical);
            render_row(&row, &mut output);
            if physical < last && !row.wrapped {
                output.push_str("\r\n");
            }
        }
        output.push_str("\x1b[0m");
        let cursor = self.emulator.cursor();
        if let Some(style) = self.cursor_style(&cursor) {
            let base = match style {
                "underline" => 3,
                "bar" => 5,
                _ => 1,
            };
            let code = base + usize::from(self.cursor_blink(&cursor) == Some(false));
            let _ = write!(output, "\x1b[{code} q");
        }
        let (row, col) = cursor_position(&cursor);
        let _ = write!(output, "\x1b[{};{}H", row + 1, col + 1);
        output.push_str(if cursor.visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        });
        output.push_str(&self.replay_preamble());
        output.into_bytes()
    }

    fn capture(&self, start: Option<f64>, end: Option<f64>) -> (Vec<String>, usize) {
        let extent = self.extent();
        let total = extent.total();
        let resolve = |number: Option<f64>, fallback: usize| {
            let number = number.unwrap_or(fallback as f64);
            let index = if number < 0.0 {
                total as f64 + number
            } else {
                number
            };
            if index < 0.0 {
                0.0
            } else if index >= total as f64 {
                (total - 1) as f64
            } else {
                index
            }
        };
        let start = resolve(start, 0);
        let end = resolve(end, total - 1);
        if start > end {
            return (Vec::new(), total);
        }
        let end_exclusive = ((end + 1.0) as usize).min(total);
        let lines = (start as usize..end_exclusive)
            .map(|physical| row_text(&self.emulator.row(physical)))
            .collect();
        (lines, total)
    }

    fn exit_lines(&self, limit: usize) -> Vec<String> {
        let extent = self.extent();
        let mut lines: Vec<String> = Vec::new();
        let mut continues = false;
        for physical in 0..extent.total() {
            let row = self.emulator.row(physical);
            let text = row_text(&row);
            match lines.last_mut() {
                Some(line) if continues => line.push_str(&text),
                _ => lines.push(text),
            }
            continues = row.wrapped;
        }
        while lines
            .last()
            .is_some_and(|line| line.trim_matches(js_whitespace).is_empty())
        {
            lines.pop();
        }
        let first = lines
            .iter()
            .position(|line| !line.trim_matches(js_whitespace).is_empty())
            .unwrap_or(lines.len());
        let start = first.max(lines.len().saturating_sub(limit));
        lines.drain(..start);
        lines
    }
}

/// Cursor cell including a pending wrap past the last column.
fn cursor_position(cursor: &Cursor) -> (usize, usize) {
    (
        usize::from(cursor.row),
        usize::from(cursor.col) + usize::from(cursor.pending_wrap),
    )
}

fn js_whitespace(c: char) -> bool {
    matches!(c, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}'
        | '\u{3000}' | '\u{feff}')
}

/// Wire text of one column: a wide character's trailing spacer is empty, an
/// erased cell or the padding before a wrapped wide character is a space.
fn cell_text(cells: &[RowCell], column: usize) -> &str {
    let cell = &cells[column];
    if cell.width == CellWidth::Spacer
        && column > 0
        && cells[column - 1].width == CellWidth::Wide
    {
        ""
    } else if cell.text.is_empty() {
        " "
    } else {
        &cell.text
    }
}

fn row_text(row: &Row) -> String {
    let mut text: String = (0..row.cells.len())
        .map(|column| cell_text(&row.cells, column))
        .collect();
    text.truncate(text.trim_end_matches(js_whitespace).len());
    text
}

fn row_json(row: &Row) -> Value {
    Value::Array(
        row.cells
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let style = wire_style(cell.style);
                let mut value = json!({
                    "char": cell_text(&row.cells, column),
                    "bold": style.bold,
                    "italic": style.italic,
                    "underline": style.underline,
                    "dim": style.dim,
                    "inverse": style.inverse,
                    "strikethrough": style.strike,
                });
                for (field, mode_field, color) in
                    [("fg", "fgMode", style.fg), ("bg", "bgMode", style.bg)]
                {
                    if let Some((mode, color)) = wire_color(color) {
                        value[field] = json!(color);
                        value[mode_field] = json!(mode);
                    }
                }
                value
            })
            .collect(),
    )
}

fn render_row(row: &Row, output: &mut String) {
    let default = CellStyle::default();
    let length = if row.wrapped {
        row.cells.len()
    } else {
        row.cells
            .iter()
            .rposition(|cell| {
                cell.width == CellWidth::Spacer
                    || (!cell.text.is_empty() && cell.text != " ")
                    || wire_style(cell.style) != default
            })
            .map_or(0, |column| column + 1)
    };
    let mut previous = default;
    for column in 0..length {
        let text = cell_text(&row.cells, column);
        if text.is_empty() {
            continue;
        }
        let style = wire_style(row.cells[column].style);
        if style != previous {
            write_ansi(style, output);
            previous = style;
        }
        output.push_str(text);
    }
    if previous != default {
        output.push_str("\x1b[0m");
    }
}

/// The attributes carried on the wire; concealment is not one of them.
fn wire_style(style: CellStyle) -> CellStyle {
    CellStyle {
        hidden: false,
        ..style
    }
}

/// Wire colour modes: 1 = palette 0..16, 2 = palette 16..256, 3 = RGB packed
/// as 0xRRGGBB.
fn wire_color(color: Color) -> Option<(u8, u32)> {
    match color {
        Color::Default => None,
        Color::Indexed(index) if index < 16 => Some((1, u32::from(index))),
        Color::Indexed(index) => Some((2, u32::from(index))),
        Color::Rgb(r, g, b) => Some((
            3,
            (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        )),
    }
}

fn write_ansi(style: CellStyle, output: &mut String) {
    output.push_str("\x1b[0");
    for (enabled, code) in [
        (style.bold, 1),
        (style.dim, 2),
        (style.italic, 3),
        (style.underline, 4),
        (style.inverse, 7),
        (style.strike, 9),
    ] {
        if enabled {
            let _ = write!(output, ";{code}");
        }
    }
    for (background, color) in [(false, style.fg), (true, style.bg)] {
        match wire_color(color) {
            Some((1, value)) => {
                let base = match (background, value >= 8) {
                    (false, false) => 30,
                    (true, false) => 40,
                    (false, true) => 90,
                    (true, true) => 100,
                };
                let _ = write!(output, ";{}", base + value % 8);
            }
            Some((2, value)) => {
                let _ = write!(output, ";{};5;{value}", if background { 48 } else { 38 });
            }
            Some((_, value)) => {
                let _ = write!(
                    output,
                    ";{};2;{};{};{}",
                    if background { 48 } else { 38 },
                    (value >> 16) & 255,
                    (value >> 8) & 255,
                    value & 255
                );
            }
            None => {}
        }
    }
    output.push('m');
}

/// Paseo protocol state Ghostty does not model: shell command completion,
/// the win32 input mode, and whether a program has chosen the cursor
/// presentation (Ghostty holds the values themselves).
#[derive(Default)]
struct Extensions {
    command_finished: Vec<Option<i32>>,
    win32: bool,
    cursor_style: bool,
    cursor_blink: bool,
}

impl vte::Perform for Extensions {
    fn osc_dispatch(&mut self, params: &[&[u8]], _: bool) {
        match params {
            [b"633", b"D"] => self.command_finished.push(None),
            [b"633", b"D", status] => {
                let digits = status.strip_prefix(b"-").unwrap_or(status);
                if !digits.is_empty()
                    && digits.iter().all(u8::is_ascii_digit)
                    && let Ok(status) = std::str::from_utf8(status).unwrap_or("").parse::<i32>()
                {
                    self.command_finished.push(Some(status));
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore || params.iter().any(|p| p.len() != 1) {
            return;
        }
        match (action, intermediates) {
            ('h' | 'l', [b'?']) => {
                for mode in params.iter().filter_map(|p| p.first()) {
                    match mode {
                        12 => self.cursor_blink = true,
                        9001 => self.win32 = action == 'h',
                        _ => {}
                    }
                }
            }
            ('q', [b' ']) => {
                let style = params.iter().next().and_then(|p| p.first()).copied();
                if style.unwrap_or(0) <= 6 {
                    self.cursor_style = true;
                    self.cursor_blink = true;
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            *self = Self {
                command_finished: std::mem::take(&mut self.command_finished),
                ..Self::default()
            };
        }
    }
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod tests;
