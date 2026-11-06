//! Read-only observation of an existing daemon execution (Flutter
//! `features/logs/execution_log_session.dart`).
//!
//! `alacritty_terminal` is used only as a headless ANSI emulator (the original's xterm
//! buffer): carriage returns, erasure, cursor movement and alternate screens are interpreted
//! before output is presented as selectable log lines, keeping every SGR attribute the
//! original renders (bold, dim, italic, underline, inverse, strikethrough, hidden). Nothing is
//! ever written back: no input, no resize, and the emulator's replies (`Event::PtyWrite`) are
//! dropped, so terminal status queries in process output cannot turn this observer into an
//! interactive client.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor, Timeout};

use paseo_gui_core::{CoreEvent, CoreHandle};
use serde_json::{json, Value};

use super::snapshot::render_terminal_snapshot;
use crate::features::workspace::i18n::l;
use crate::features::workspace::observe::{ObsEvent, Observation};
use crate::features::workspace::rpc::{int_of, payload, str_of};

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

/// Collects OSC window titles (`onTitleChange`); every other emulator event (replies,
/// bells, clipboard) is dropped.
#[derive(Clone, Default)]
pub struct TitleSink {
    title: Rc<RefCell<Option<String>>>,
}

impl EventListener for TitleSink {
    fn send_event(&self, event: TermEvent) {
        match event {
            TermEvent::Title(title) => *self.title.borrow_mut() = Some(title),
            TermEvent::ResetTitle => *self.title.borrow_mut() = Some(String::new()),
            _ => {}
        }
    }
}

/// Synchronized updates are applied immediately: a read-only log has no tearing to avoid,
/// and a pending update would otherwise hold output back until the next frame.
#[derive(Default)]
struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _duration: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

struct Size {
    rows: usize,
    cols: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The headless emulator: grid, scrollback and the byte parser.
struct Emulator {
    term: Term<TitleSink>,
    processor: Processor<NoSync>,
    titles: TitleSink,
}

impl Emulator {
    fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let titles = TitleSink::default();
        let config = TermConfig { scrolling_history: scrollback, ..TermConfig::default() };
        let size = Size { rows: rows.max(1) as usize, cols: cols.max(1) as usize };
        Self { term: Term::new(config, &size, titles.clone()), processor: Processor::new(), titles }
    }

    fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.processor.advance(&mut self.term, byte);
        }
    }

    fn take_title(&self) -> Option<String> {
        self.titles.title.borrow_mut().take()
    }

    fn size(&self) -> (u16, u16) {
        let grid = self.term.grid();
        (grid.screen_lines() as u16, grid.columns() as u16)
    }

    fn history(&self) -> usize {
        self.term.grid().history_size()
    }

    fn cursor_row(&self) -> usize {
        self.term.grid().cursor.point.line.0.max(0) as usize
    }

    /// Presented row `index`, history first (0 is the oldest kept line).
    fn line(&self, index: usize) -> Line {
        let grid = self.term.grid();
        let line = GridLine(index as i32 - grid.history_size() as i32);
        read_row(&grid[line], grid.columns())
    }
}

/// A terminal colour as the emulator reports it: the default foreground/background, one of
/// the 256 indexed colours, or 24-bit RGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermColor {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl TermColor {
    fn from_ansi(color: AnsiColor) -> Self {
        match color {
            AnsiColor::Indexed(index) => Self::Idx(index),
            AnsiColor::Spec(rgb) => Self::Rgb(rgb.r, rgb.g, rgb.b),
            AnsiColor::Named(named) => {
                let value = named as usize;
                if value < 16 {
                    Self::Idx(value as u8)
                } else if (NamedColor::DimBlack as usize..=NamedColor::DimWhite as usize).contains(&value) {
                    Self::Idx((value - NamedColor::DimBlack as usize) as u8)
                } else {
                    Self::Default
                }
            }
        }
    }
}

/// One run of identically styled cells (`ExecutionLogRun`).
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    /// Width of the run in terminal cells.
    pub cells: usize,
    pub fg: TermColor,
    pub bg: TermColor,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strike: bool,
    pub hidden: bool,
}

/// One presented buffer row.
#[derive(Clone, Debug, Default)]
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
}

pub struct LogSession {
    pub host_id: String,
    pub terminal_id: String,
    scrollback: usize,
    emulator: Emulator,
    pub title: String,
    pub error: Option<String>,
    pub attaching: bool,
    pub exited: bool,
    pub revision: u64,
    line_count: usize,
    visible: bool,
    has_dimensions: bool,
    slot: Option<u8>,
    subscription_id: Option<String>,
    observation: Option<Observation>,
    /// Output arrived since the last publish (coalesced by the panel's 33ms timer).
    pub output_pending: bool,
}

impl LogSession {
    pub fn new(host_id: &str, terminal_id: &str, scrollback: usize) -> Self {
        Self {
            host_id: host_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            scrollback,
            emulator: Emulator::new(DEFAULT_ROWS, DEFAULT_COLS, scrollback),
            title: String::new(),
            error: None,
            attaching: false,
            exited: false,
            revision: 0,
            line_count: 0,
            visible: false,
            has_dimensions: false,
            slot: None,
            subscription_id: None,
            observation: None,
            output_pending: false,
        }
    }

    /// `ExecutionLogSession.connected`.
    pub fn live(&self, host_connected: bool) -> bool {
        host_connected && self.slot.is_some() && !self.attaching && !self.exited && self.error.is_none()
    }

    pub fn line_count(&self) -> usize {
        self.line_count
    }

    pub fn cols(&self) -> usize {
        self.emulator.size().1 as usize
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Attach while visible; release while hidden (Flutter `setVisible`).
    pub fn set_visible(&mut self, core: &CoreHandle, visible: bool, host_connected: bool) {
        if visible == self.visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.attach(core, host_connected);
        } else {
            self.output_pending = false;
            self.release();
        }
    }

    pub fn attach(&mut self, core: &CoreHandle, host_connected: bool) {
        if !self.visible || self.exited || self.attaching || self.observation.is_some() || !host_connected {
            return;
        }
        self.attaching = true;
        self.error = None;
        self.slot = None;
        // The default JSON snapshot includes the authoritative PTY dimensions. An observer
        // must not request a guessed size or take resize ownership.
        self.observation = Some(Observation::start(
            core,
            &self.host_id,
            "subscribe_terminal_request",
            "subscribe_terminal_response",
            json!({ "terminalId": self.terminal_id }),
        ));
    }

    pub fn retry(&mut self, core: &CoreHandle, host_connected: bool) {
        self.release();
        self.exited = false;
        self.attach(core, host_connected);
    }

    fn release(&mut self) {
        self.observation = None;
        self.slot = None;
        self.subscription_id = None;
        self.attaching = false;
    }

    fn fail(&mut self, message: String) {
        self.error = Some(message);
        self.attaching = false;
    }

    /// Connection changes for this host (`_connectionChanged`). Existing observations are
    /// restored by the core; their restored snapshot remaps the slot before frames arrive.
    /// Returns true when the view should redraw.
    pub fn connection_changed(&mut self, core: &CoreHandle, connected: bool) -> bool {
        if !connected {
            self.slot = None;
            self.subscription_id = None;
            self.attaching = self.visible && !self.exited;
            return self.visible;
        }
        if self.observation.is_none() && self.visible && !self.exited {
            self.attaching = false;
            self.attach(core, connected);
            return true;
        }
        false
    }

    /// Feeds the shell's event batch. Returns true when the presentation changed outside
    /// output coalescing (status, error, snapshot).
    pub fn pump(&mut self, core: &CoreHandle, events: &[CoreEvent]) -> bool {
        let mut changed = false;
        for event in events {
            match event {
                CoreEvent::Connected { host_id, .. } if *host_id == self.host_id => {
                    changed |= self.connection_changed(core, true);
                }
                CoreEvent::Disconnected { host_id, .. } if *host_id == self.host_id => {
                    changed |= self.connection_changed(core, false);
                }
                _ => {}
            }
        }
        let Some(observation) = self.observation.as_mut() else { return changed };
        let items = observation.take(events);
        for item in items {
            changed |= self.receive(item);
        }
        // Terminal stream frames are routed by slot, in wire order with the text messages.
        if self.observation.is_some() {
            for event in events {
                if let CoreEvent::HostEvent { host_id, event } = event && *host_id == self.host_id && str_of(event, "type") == "terminal.frame" {
                    changed |= self.frame(payload(event));
                }
            }
        }
        changed
    }

    fn receive(&mut self, event: ObsEvent) -> bool {
        if !self.visible {
            return false;
        }
        match event {
            ObsEvent::Snapshot { payload, .. } => {
                self.acknowledge(&payload);
                true
            }
            ObsEvent::Failed(error) => {
                self.observation = None;
                self.fail(error.to_string());
                true
            }
            ObsEvent::RestoreFailed(error) => {
                self.slot = None;
                let message = if error.is_empty() { l("Execution observation could not be restored").to_owned() } else { error };
                self.fail(message);
                true
            }
            ObsEvent::Event(message) => {
                let body = payload(&message).clone();
                match str_of(&message, "type") {
                    "subscribe_terminal_response" => {
                        self.acknowledge(&body);
                        true
                    }
                    "terminal_stream_exit" => self.exit(&body),
                    _ => false,
                }
            }
        }
    }

    fn exit(&mut self, body: &Value) -> bool {
        if str_of(body, "terminalId") != self.terminal_id {
            return false;
        }
        if let (Some(own), Some(other)) = (&self.subscription_id, body.get("subscriptionId").and_then(Value::as_str)) && own != other {
            return false;
        }
        match body.get("error").filter(|e| !e.is_null()) {
            // An observer failure does not mean the process exited.
            Some(error) => self.fail(value_message(error)),
            None => {
                self.exited = true;
                self.attaching = false;
                self.slot = None;
                self.output_pending = false;
                self.publish();
            }
        }
        self.release();
        true
    }

    fn acknowledge(&mut self, body: &Value) {
        if !self.visible || str_of(body, "terminalId") != self.terminal_id {
            return;
        }
        if let Some(error) = body.get("error").filter(|e| !e.is_null()) {
            self.fail(value_message(error));
            return;
        }
        let slot = int_of(body, "slot");
        if body.get("slot").and_then(Value::as_i64).is_none() || !(0..=255).contains(&slot) {
            self.fail(l("Invalid execution subscription slot").to_owned());
            return;
        }
        self.slot = Some(slot as u8);
        self.subscription_id = body.get("subscriptionId").and_then(Value::as_str).map(str::to_owned);
        self.attaching = false;
        self.error = None;
    }

    /// One core `terminal.frame` (`{slot, opcode, text | snapshot}`).
    fn frame(&mut self, frame: &Value) -> bool {
        let Some(slot) = self.slot else { return false };
        if frame.get("slot").and_then(Value::as_u64) != Some(slot as u64) {
            return false;
        }
        match str_of(frame, "opcode") {
            // Output: UTF-8 already decoded incrementally per slot by the core.
            "output" => {
                self.emulator.process(str_of(frame, "text").as_bytes());
                self.output_pending = true;
                false
            }
            // Snapshot: a JSON cell grid with authoritative dimensions.
            "snapshot" => match frame.get("snapshot") {
                Some(state) if state.is_object() => {
                    let text = render_terminal_snapshot(state);
                    let state = state.clone();
                    self.replace(&text, Some(&state));
                    true
                }
                _ => {
                    self.fail(l("Invalid execution snapshot").to_owned());
                    true
                }
            },
            // Restore: ANSI without size fields keeps the last authoritative size.
            "restore" => {
                if !self.has_dimensions {
                    self.fail(l("Execution restore arrived without snapshot dimensions").to_owned());
                    return true;
                }
                let text = str_of(frame, "text").to_owned();
                self.replace(&text, None);
                true
            }
            _ => false,
        }
    }

    fn replace(&mut self, text: &str, state: Option<&Value>) {
        let (rows, cols) = self.emulator.size();
        let cols = state.map(|s| int_of(s, "cols")).filter(|c| *c > 0).map(|c| c.min(u16::MAX as i64) as u16).unwrap_or(cols);
        let rows = state.map(|s| int_of(s, "rows")).filter(|r| *r > 0).map(|r| r.min(u16::MAX as i64) as u16).unwrap_or(rows);
        self.output_pending = false;
        self.emulator = Emulator::new(rows, cols, self.scrollback);
        self.has_dimensions = true;
        self.emulator.process(text.as_bytes());
        if let Some(title) = state.map(|s| str_of(s, "title")).filter(|t| !t.is_empty()) {
            self.title = title.to_owned();
        }
        self.publish();
    }

    /// Applies coalesced output (`_publishOutput`).
    pub fn publish(&mut self) {
        self.output_pending = false;
        if let Some(title) = self.emulator.take_title() {
            self.title = title;
        }
        self.line_count = self.count_lines();
        self.revision += 1;
    }

    fn total_rows(&self) -> (usize, usize) {
        (self.emulator.history(), self.emulator.size().0 as usize)
    }

    /// `_countLines`: every row through the cursor, then trailing blank rows dropped.
    fn count_lines(&mut self) -> usize {
        let (history, rows) = self.total_rows();
        let total = history + rows;
        let minimum = total.min(history + self.emulator.cursor_row() + 1);
        let mut count = total;
        while count > minimum {
            if !self.emulator.line(count - 1).is_blank() {
                break;
            }
            count -= 1;
        }
        count
    }

    /// Reads presented rows `start..start+count` (history first, then the screen).
    pub fn read(&mut self, start: usize, count: usize) -> Vec<Line> {
        let (history, rows) = self.total_rows();
        let end = (start + count).min(history + rows);
        (start..end).map(|row| self.emulator.line(row)).collect()
    }

    /// `renderedText`: logical lines joined, soft wraps rejoined.
    pub fn rendered_text(&mut self) -> String {
        let mut text = String::new();
        let count = self.line_count;
        let mut previous_wraps = false;
        let mut start = 0;
        while start < count {
            let chunk = self.read(start, 512.min(count - start));
            for (index, line) in chunk.iter().enumerate() {
                if start + index > 0 && !previous_wraps {
                    text.push('\n');
                }
                text.push_str(&line.text);
                previous_wraps = line.wraps_next;
            }
            start += chunk.len().max(1);
        }
        text
    }
}

fn value_message(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => {
            let message = str_of(value, "message");
            if message.is_empty() { str_of(value, "code").to_owned() } else { message.to_owned() }
        }
        other => other.to_string(),
    }
}

/// Builds one presented row: trailing erased cells trimmed unless the row soft-wraps, erased
/// interior cells shown as spaces, wide-character spacers folded into their glyph's run.
fn read_row(row: &alacritty_terminal::grid::Row<alacritty_terminal::term::cell::Cell>, cols: usize) -> Line {
    let wraps_next = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
    let mut length = cols;
    if !wraps_next {
        while length > 0 {
            let cell = &row[Column(length - 1)];
            let erased = cell.c == ' '
                && cell.zerowidth().is_none()
                && cell.bg == AnsiColor::Named(NamedColor::Background)
                && !cell.flags.contains(Flags::INVERSE);
            if !erased && !cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                break;
            }
            length -= 1;
        }
    }
    let mut line = Line { wraps_next, ..Default::default() };
    let mut current: Option<Run> = None;
    for col in 0..length {
        let cell = &row[Column(col)];
        if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
            if let Some(run) = current.as_mut() {
                run.cells += 1;
            }
            continue;
        }
        let flags = cell.flags;
        let style = Run {
            text: String::new(),
            cells: 0,
            fg: TermColor::from_ansi(cell.fg),
            bg: TermColor::from_ansi(cell.bg),
            bold: flags.contains(Flags::BOLD),
            dim: flags.contains(Flags::DIM),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.intersects(Flags::ALL_UNDERLINES),
            inverse: flags.contains(Flags::INVERSE),
            strike: flags.contains(Flags::STRIKEOUT),
            hidden: flags.contains(Flags::HIDDEN),
        };
        let same = current.as_ref().is_some_and(|run| {
            run.fg == style.fg
                && run.bg == style.bg
                && run.bold == style.bold
                && run.dim == style.dim
                && run.italic == style.italic
                && run.underline == style.underline
                && run.inverse == style.inverse
                && run.strike == style.strike
                && run.hidden == style.hidden
        });
        if !same {
            if let Some(run) = current.take() {
                line.runs.push(run);
            }
            current = Some(style);
        }
        let run = current.as_mut().expect("run started above");
        let mut push = |ch: char, run: &mut Run| {
            line.columns.push(col);
            run.text.push(ch);
            line.text.push(ch);
        };
        push(cell.c, run);
        for &mark in cell.zerowidth().unwrap_or(&[]) {
            push(mark, run);
        }
        run.cells += 1;
    }
    if let Some(run) = current {
        line.runs.push(run);
    }
    line
}

/// Scrollback lines kept for logs: `preferences.executionLogLines`, 100..100000, default
/// 10000.
pub fn scrollback_preference(preferences: &Value) -> usize {
    match preferences.get("executionLogLines").and_then(Value::as_f64) {
        Some(value) if value.is_finite() => value.clamp(100.0, 100_000.0) as usize,
        _ => 10_000,
    }
}

/// Exposed for the panel's header (`target.title ?? session.title ?? 'Execution output'`).
pub fn display_title<'a>(target_title: &'a str, session: Option<&'a LogSession>) -> &'a str {
    if !target_title.is_empty() {
        return target_title;
    }
    match session {
        Some(session) if !session.title.is_empty() => &session.title,
        _ => l("Execution output"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with(text: &str) -> LogSession {
        let mut session = LogSession::new("h", "t", 100);
        session.visible = true;
        session.emulator.process(text.as_bytes());
        session.publish();
        session
    }

    #[test]
    fn carriage_returns_and_erasure_are_interpreted() {
        let mut session = session_with("progress 10%\rprogress 99%\r\nnext\x1b[2K\rdone\r\n");
        assert_eq!(session.line_count(), 3);
        let lines = session.read(0, 2);
        assert_eq!(lines[0].text, "progress 99%");
        assert_eq!(lines[1].text, "done");
    }

    #[test]
    fn history_rows_precede_screen_rows() {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("line {i}\r\n"));
        }
        let mut session = session_with(&text);
        assert_eq!(session.line_count(), 41);
        let lines = session.read(0, 41);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[39].text, "line 39");
        assert!(session.rendered_text().starts_with("line 0\nline 1\n"));
    }

    #[test]
    fn styled_runs_split_on_sgr() {
        let mut session = session_with("a\x1b[31mb\x1b[0mc");
        let lines = session.read(0, 1);
        assert_eq!(lines[0].runs.len(), 3);
        assert_eq!(lines[0].runs[1].fg, TermColor::Idx(1));
        assert_eq!(lines[0].text, "abc");
    }

    #[test]
    fn strikethrough_hidden_and_wide_cells_are_kept() {
        let mut session = session_with("a\x1b[9mb\x1b[29;8mc\x1b[0m好d");
        let lines = session.read(0, 1);
        let runs = &lines[0].runs;
        assert!(runs[1].strike && !runs[1].hidden);
        assert!(runs[2].hidden && !runs[2].strike);
        assert_eq!(runs[3].text, "好d");
        assert_eq!(runs[3].cells, 3);
        assert_eq!(lines[0].columns, vec![0, 1, 2, 3, 5]);
    }

    #[test]
    fn all_sgr_attributes_survive_restore_and_reset() {
        let mut session = session_with("");
        session.replace(
            "\x1b[1;2;3;4;7;8;9mstyled\x1b[0mnormal",
            Some(&json!({"rows": 3, "cols": 40})),
        );
        let line = session.read(0, 1).remove(0);
        let styled = &line.runs[0];
        assert!(styled.bold && styled.dim && styled.italic && styled.underline);
        assert!(styled.inverse && styled.hidden && styled.strike);
        let normal = &line.runs[1];
        assert!(!normal.bold && !normal.dim && !normal.italic && !normal.underline);
        assert!(!normal.inverse && !normal.hidden && !normal.strike);
        assert_eq!(session.rendered_text(), "stylednormal");
        assert_eq!(session.emulator.size(), (3, 40));
        session.replace("new", None);
        assert_eq!(session.emulator.size(), (3, 40));
        assert_eq!(session.rendered_text(), "new");
    }

    #[test]
    fn bounded_history_and_alternate_screen_restore_primary_buffer() {
        let mut session = session_with("");
        session.replace("", Some(&json!({"rows": 2, "cols": 16})));
        for i in 0..120 {
            session.emulator.process(format!("{i}\r\n").as_bytes());
        }
        session.publish();
        assert_eq!(session.emulator.history(), 100);
        assert_eq!(session.read(0, 1)[0].text, "19");
        let primary = session.rendered_text();
        session.emulator.process(b"\x1b[?1049h\x1b[Halt");
        session.publish();
        assert_eq!(session.read(0, 1)[0].text, "alt");
        session.emulator.process(b"\x1b[?1049l");
        session.publish();
        assert_eq!(session.rendered_text(), primary);
    }

    #[test]
    fn titles_come_from_osc() {
        let session = session_with("\x1b]2;build\x07ok");
        assert_eq!(session.title, "build");
    }

    #[test]
    fn exit_with_error_is_a_failure_not_an_exit() {
        let mut session = LogSession::new("h", "t", 100);
        session.visible = true;
        session.receive(ObsEvent::Event(json!({"type": "terminal_stream_exit", "payload": {"terminalId": "t", "error": "gone"}})));
        assert!(!session.exited);
        assert_eq!(session.error.as_deref(), Some("gone"));
        session.receive(ObsEvent::Event(json!({"type": "terminal_stream_exit", "payload": {"terminalId": "t"}})));
        assert!(session.exited);
    }

    #[test]
    fn scrollback_preference_clamps() {
        assert_eq!(scrollback_preference(&serde_json::json!({"executionLogLines": 5})), 100);
        assert_eq!(scrollback_preference(&serde_json::json!({})), 10_000);
    }
}
