//! Ghostty (`libghostty-vt`) terminal emulation shared by the daemon's PTY
//! screens and the native client's execution logs and interactive terminals.
//!
//! Ghostty objects are `!Send`: an [`Emulator`] stays on the thread that
//! created it. The client keeps it on the UI thread; the daemon owns each one
//! on a dedicated thread through [`ThreadBound`]. Callers read plain [`Row`]s,
//! [`Line`]s and [`Cursor`]s, which are ordinary `Send` data.

mod emulator;
mod input;
mod model;
mod thread;

pub use emulator::{Emulator, Modes, Options};
pub use input::{Key, Modifiers};
pub use libghostty_vt::Error;
pub use model::{CellStyle, CellWidth, Color, Cursor, CursorShape, Line, Row, RowCell, Run};
pub use thread::ThreadBound;
