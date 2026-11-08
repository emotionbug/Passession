//! The one owner of Ghostty terminal objects and their callbacks.

use std::cell::{Cell as Flag, RefCell};
use std::rc::Rc;

use libghostty_vt::key::{self, Mods};
use libghostty_vt::render::{CursorVisualStyle, RenderState};
use libghostty_vt::screen::{CellContentTag, CellWide, GridRef, Screen};
use libghostty_vt::style::{RgbColor, StyleColor, Underline};
use libghostty_vt::terminal::{
    ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode, Point, PointCoordinate,
    PrimaryDeviceAttributes, SecondaryDeviceAttributes, TertiaryDeviceAttributes,
};
use libghostty_vt::{paste, Error, Terminal, TerminalOptions};

use crate::input::{Key, Modifiers};
use crate::model::{CellStyle, CellWidth, Color, Cursor, CursorShape, Line, Row, RowCell};

/// Colours reported to OSC 10/11/12 queries. Programs use them to pick light
/// or dark output; the client paints with its own theme.
const DEFAULT_FOREGROUND: RgbColor = RgbColor { r: 0xe6, g: 0xe6, b: 0xe6 };
const DEFAULT_BACKGROUND: RgbColor = RgbColor { r: 0x0b, g: 0x0b, b: 0x0b };

/// DA1 `CSI ? 62 ; 4 ; 22 c`: VT220-class with the features Paseo has always
/// reported to programs running in its PTYs.
const PRIMARY_ATTRIBUTES: PrimaryDeviceAttributes =
    PrimaryDeviceAttributes::new(ConformanceLevel::VT220, &[DeviceAttributeFeature::SIXEL, DeviceAttributeFeature::ANSI_COLOR]);

/// Nominal cell size for pixel size reports; Paseo has no pixel geometry for
/// a PTY observed by several clients.
const CELL_WIDTH_PX: u32 = 8;
const CELL_HEIGHT_PX: u32 = 16;

/// Ghostty budgets scrollback in bytes and prunes whole pages. This budget per
/// requested row holds a wide, styled row, so the row limit rather than the
/// byte budget decides what is presented; memory still grows only with output.
const SCROLLBACK_BYTES_PER_ROW: usize = 4096;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub rows: u16,
    pub cols: u16,
    /// Scrollback rows presented above the screen. Ghostty retains at least
    /// this many (rounded up to whole pages); older retained rows are hidden.
    pub scrollback: usize,
    /// Collect query replies (device attributes, cursor and mode reports,
    /// colour queries) for the PTY. An observer that never writes to the
    /// process leaves this off.
    pub respond: bool,
}

/// Input modes a client needs to encode keys and pastes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub kitty_flags: u8,
    pub alternate_screen: bool,
    pub mouse_tracking: bool,
}

#[derive(Default)]
struct Effects {
    replies: RefCell<Vec<u8>>,
    title_changed: Flag<bool>,
}

/// A headless Ghostty terminal: grid, scrollback, modes and the VT parser.
pub struct Emulator {
    terminal: Terminal<'static, 'static>,
    render: RenderState<'static>,
    keys: key::Encoder<'static>,
    key_event: key::Event<'static>,
    effects: Rc<Effects>,
    scrollback: usize,
}

impl Emulator {
    pub fn new(options: Options) -> Result<Self, Error> {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: options.cols.max(1),
            rows: options.rows.max(1),
            max_scrollback: options.scrollback.saturating_mul(SCROLLBACK_BYTES_PER_ROW),
        })?;
        terminal
            .set_default_fg_color(Some(DEFAULT_FOREGROUND))?
            .set_default_bg_color(Some(DEFAULT_BACKGROUND))?
            .set_default_cursor_color(Some(DEFAULT_FOREGROUND))?;
        let effects = Rc::new(Effects::default());
        terminal.on_title_changed({
            let effects = effects.clone();
            move |_| effects.title_changed.set(true)
        })?;
        if options.respond {
            terminal.on_pty_write({
                let effects = effects.clone();
                move |_, data| effects.replies.borrow_mut().extend_from_slice(data)
            })?;
            terminal.on_device_attributes(|_| {
                Some(DeviceAttributes {
                    primary: PRIMARY_ATTRIBUTES,
                    secondary: SecondaryDeviceAttributes { device_type: DeviceType::VT220, firmware_version: 0, rom_cartridge: 0 },
                    tertiary: TertiaryDeviceAttributes::default(),
                })
            })?;
        }
        Ok(Self {
            terminal,
            render: RenderState::new()?,
            keys: key::Encoder::new()?,
            key_event: key::Event::new()?,
            effects,
            scrollback: options.scrollback,
        })
    }

    /// Feeds process output through the VT parser.
    pub fn write(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// Bytes the terminal answered to queries since the last call; always
    /// empty unless `Options::respond`.
    pub fn take_replies(&self) -> Vec<u8> {
        std::mem::take(&mut *self.effects.replies.borrow_mut())
    }

    /// The window title after an OSC title change since the last call; an
    /// empty string means the title was cleared.
    pub fn take_title(&self) -> Option<String> {
        self.effects.title_changed.replace(false).then(|| self.terminal.title().unwrap_or_default().to_owned())
    }

    /// The current non-empty window title.
    pub fn title(&self) -> Option<String> {
        self.terminal.title().ok().filter(|title| !title.is_empty()).map(str::to_owned)
    }

    /// Resizes the grid; the primary screen reflows soft-wrapped lines.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        self.terminal.resize(cols.max(1), rows.max(1), CELL_WIDTH_PX, CELL_HEIGHT_PX)
    }

    pub fn rows(&self) -> u16 {
        self.terminal.rows().unwrap_or(1)
    }

    pub fn cols(&self) -> u16 {
        self.terminal.cols().unwrap_or(1)
    }

    /// Presented scrollback rows above the screen of the active buffer (the
    /// alternate screen has none), at most `Options::scrollback`.
    pub fn history(&self) -> usize {
        self.retained_history().min(self.scrollback)
    }

    /// Presented scrollback plus screen rows; the index space of
    /// [`Emulator::row`].
    pub fn total_rows(&self) -> usize {
        self.history() + usize::from(self.rows())
    }

    fn retained_history(&self) -> usize {
        self.terminal.scrollback_rows().unwrap_or(0)
    }

    pub fn cursor(&mut self) -> Cursor {
        let (shape, blinking) = match self.render.update(&self.terminal) {
            Ok(snapshot) => (
                match snapshot.cursor_visual_style() {
                    Ok(CursorVisualStyle::Bar) => CursorShape::Bar,
                    Ok(CursorVisualStyle::Underline) => CursorShape::Underline,
                    Ok(CursorVisualStyle::BlockHollow) => CursorShape::BlockHollow,
                    _ => CursorShape::Block,
                },
                snapshot.cursor_blinking().unwrap_or(false),
            ),
            Err(_) => (CursorShape::Block, false),
        };
        Cursor {
            row: self.terminal.cursor_y().unwrap_or(0),
            col: self.terminal.cursor_x().unwrap_or(0),
            pending_wrap: self.terminal.is_cursor_pending_wrap().unwrap_or(false),
            visible: self.terminal.is_cursor_visible().unwrap_or(true),
            shape,
            blinking,
        }
    }

    pub fn modes(&self) -> Modes {
        let mode = |mode| self.terminal.mode(mode).unwrap_or(false);
        Modes {
            application_cursor: mode(Mode::DECCKM),
            bracketed_paste: mode(Mode::BRACKETED_PASTE),
            kitty_flags: self.terminal.kitty_keyboard_flags().map(|flags| flags.bits()).unwrap_or(0),
            alternate_screen: self.terminal.active_screen().is_ok_and(|screen| screen == Screen::Alternate),
            mouse_tracking: self.terminal.is_mouse_tracking().unwrap_or(false),
        }
    }

    /// Row `index` of presented scrollback plus screen (0 is the oldest
    /// presented row). Rows past the end read as empty.
    pub fn row(&self, index: usize) -> Row {
        let cols = self.cols();
        let hidden = self.retained_history() - self.history();
        let y = u32::try_from(index.saturating_add(hidden)).unwrap_or(u32::MAX);
        let mut wrapped = false;
        let mut cells = Vec::with_capacity(usize::from(cols));
        for x in 0..cols {
            let Ok(grid) = self.terminal.grid_ref(Point::Screen(PointCoordinate { x, y })) else {
                cells.push(RowCell { text: String::new(), width: CellWidth::Narrow, style: CellStyle::default() });
                continue;
            };
            if x == 0 {
                wrapped = grid.row().and_then(|row| row.is_wrapped()).unwrap_or(false);
            }
            cells.push(read_cell(&grid));
        }
        Row { cells, wrapped }
    }

    pub fn line(&self, index: usize) -> Line {
        Line::from_row(&self.row(index))
    }

    /// Encodes a key press for the current modes (application cursor keys,
    /// kitty keyboard flags, modifyOtherKeys). Plain text from an input method
    /// is written as its UTF-8 bytes instead.
    pub fn encode_key(&mut self, key: Key, modifiers: Modifiers) -> Vec<u8> {
        let (ghostty, unshifted) = key.ghostty();
        let mut mods = Mods::empty();
        mods.set(Mods::SHIFT, modifiers.shift);
        mods.set(Mods::CTRL, modifiers.control);
        mods.set(Mods::ALT, modifiers.alt);
        mods.set(Mods::SUPER, modifiers.logo);
        let printable = unshifted != '\0';
        let text = printable.then(|| {
            if modifiers.shift { unshifted.to_uppercase().collect::<String>() } else { unshifted.to_string() }
        });
        self.key_event
            .set_action(key::Action::Press)
            .set_key(ghostty)
            .set_mods(mods)
            .set_consumed_mods(if printable && modifiers.shift { Mods::SHIFT } else { Mods::empty() })
            .set_unshifted_codepoint(unshifted)
            .set_utf8(text);
        let mut bytes = Vec::new();
        if self.keys.set_options_from_terminal(&self.terminal).encode_to_vec(&self.key_event, &mut bytes).is_err() {
            bytes.clear();
        }
        bytes
    }

    /// Encodes pasted text: unsafe control bytes are neutralized and bracketed
    /// paste markers are added when the program enabled them.
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        let mut data = text.as_bytes().to_vec();
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).unwrap_or(false);
        let mut out = vec![0u8; data.len() + 16];
        loop {
            match paste::encode(&mut data, bracketed, &mut out) {
                Ok(written) => {
                    out.truncate(written);
                    return out;
                }
                // The size is unreported (zero) on some error paths; always grow.
                Err(Error::OutOfSpace { required }) => out.resize(required.max(out.len() * 2), 0),
                Err(_) => return text.as_bytes().to_vec(),
            }
        }
    }
}

fn read_cell(grid: &GridRef<'_>) -> RowCell {
    let Ok(cell) = grid.cell() else {
        return RowCell { text: String::new(), width: CellWidth::Narrow, style: CellStyle::default() };
    };
    let width = match cell.wide() {
        Ok(CellWide::Wide) => CellWidth::Wide,
        Ok(CellWide::SpacerTail | CellWide::SpacerHead) => CellWidth::Spacer,
        _ => CellWidth::Narrow,
    };
    let mut style = if cell.has_styling().unwrap_or(false) { grid.style().map(cell_style).unwrap_or_default() } else { CellStyle::default() };
    let mut text = String::new();
    match cell.content_tag() {
        Ok(CellContentTag::Codepoint) => {
            if let Some(ch) = cell.codepoint().ok().and_then(char::from_u32).filter(|ch| *ch != '\0') {
                text.push(ch);
            }
        }
        Ok(CellContentTag::CodepointGrapheme) => {
            let mut buffer = ['\0'; 16];
            let count = match grid.graphemes(&mut buffer) {
                Ok(count) => count,
                Err(Error::OutOfSpace { required }) => {
                    let mut large = vec!['\0'; required];
                    let count = grid.graphemes(&mut large).unwrap_or(0);
                    text.extend(&large[..count.min(large.len())]);
                    0
                }
                Err(_) => 0,
            };
            text.extend(&buffer[..count.min(buffer.len())]);
        }
        // Erased with a background colour: no text, colour held in the cell.
        Ok(CellContentTag::BgColorPalette) => {
            if let Ok(index) = cell.bg_color_palette() {
                style.bg = Color::Indexed(index.0);
            }
        }
        Ok(CellContentTag::BgColorRgb) => {
            if let Ok(rgb) = cell.bg_color_rgb() {
                style.bg = Color::Rgb(rgb.r, rgb.g, rgb.b);
            }
        }
        Err(_) => {}
    }
    if width == CellWidth::Spacer {
        text.clear();
    }
    RowCell { text, width, style }
}

fn cell_style(style: libghostty_vt::style::Style) -> CellStyle {
    CellStyle {
        fg: color(style.fg_color),
        bg: color(style.bg_color),
        bold: style.bold,
        dim: style.faint,
        italic: style.italic,
        underline: style.underline != Underline::None,
        inverse: style.inverse,
        strike: style.strikethrough,
        hidden: style.invisible,
    }
}

fn color(color: StyleColor) -> Color {
    match color {
        StyleColor::None => Color::Default,
        StyleColor::Palette(index) => Color::Indexed(index.0),
        StyleColor::Rgb(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emulator(rows: u16, cols: u16) -> Emulator {
        Emulator::new(Options { rows, cols, scrollback: 100, respond: true }).unwrap()
    }

    #[test]
    fn carriage_return_and_erase_rewrite_cells() {
        let mut term = emulator(4, 20);
        term.write(b"progress 10%\rprogress 99%\r\nnext\x1b[2K\rdone\r\n");
        assert_eq!(term.line(0).text, "progress 99%");
        assert_eq!(term.line(1).text, "done");
        assert_eq!(term.cursor().row, 2);
    }

    #[test]
    fn styles_wide_text_and_erased_backgrounds_are_read() {
        let mut term = emulator(3, 10);
        term.write("\x1b[1;31m한\x1b[0m\x1b[38;2;1;2;3mx\x1b[0m\r\n\x1b[44m\x1b[K".as_bytes());
        let row = term.row(0);
        assert_eq!(row.cells[0].text, "한");
        assert_eq!(row.cells[0].width, CellWidth::Wide);
        assert_eq!(row.cells[1].width, CellWidth::Spacer);
        assert!(row.cells[0].style.bold);
        assert_eq!(row.cells[0].style.fg, Color::Indexed(1));
        assert_eq!(row.cells[2].style.fg, Color::Rgb(1, 2, 3));
        assert!(term.row(1).cells.iter().all(|cell| cell.style.bg == Color::Indexed(4)));
    }

    #[test]
    fn history_precedes_screen_and_wraps_are_flagged() {
        let mut term = emulator(2, 4);
        term.write(b"abcdefgh\r\nlast");
        assert_eq!(term.total_rows(), term.history() + 2);
        let first = term.row(0);
        assert!(first.wrapped);
        assert_eq!(first.text(), "abcd");
    }

    #[test]
    fn device_queries_answer_only_when_responding() {
        let mut term = emulator(5, 10);
        term.write(b"\x1b[c\x1b[3;4H\x1b[6n");
        let replies = String::from_utf8(term.take_replies()).unwrap();
        assert!(replies.contains("\x1b[?62;4;22c"), "{replies:?}");
        assert!(replies.contains("\x1b[3;4R"), "{replies:?}");
        let mut quiet = Emulator::new(Options { rows: 5, cols: 10, scrollback: 0, respond: false }).unwrap();
        quiet.write(b"\x1b[c\x1b[6n");
        assert!(quiet.take_replies().is_empty());
    }

    #[test]
    fn titles_report_changes_once() {
        let mut term = emulator(2, 10);
        term.write(b"\x1b]2;build\x07");
        assert_eq!(term.take_title().as_deref(), Some("build"));
        assert_eq!(term.take_title(), None);
        assert_eq!(term.title().as_deref(), Some("build"));
    }

    #[test]
    fn keys_follow_application_cursor_mode_and_paste_brackets() {
        let mut term = emulator(2, 10);
        assert_eq!(term.encode_key(Key::Up, Modifiers::default()), b"\x1b[A");
        assert_eq!(term.encode_key(Key::Char('c'), Modifiers { control: true, ..Modifiers::default() }), b"\x03");
        term.write(b"\x1b[?1h\x1b[?2004h");
        assert_eq!(term.encode_key(Key::Up, Modifiers::default()), b"\x1bOA");
        let modes = term.modes();
        assert!(modes.application_cursor && modes.bracketed_paste);
        assert_eq!(term.encode_paste("ls"), b"\x1b[200~ls\x1b[201~");
    }
}
