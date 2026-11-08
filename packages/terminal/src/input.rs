//! Platform-neutral key input mapped onto Ghostty's key encoder.

use libghostty_vt::key::Key as GhosttyKey;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    /// Command on macOS, Windows key elsewhere.
    pub logo: bool,
}

/// A key the client encodes itself. Committed text from the keyboard or an
/// input method is written as UTF-8 instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 through F12.
    F(u8),
    /// A US-layout character key by its unshifted character: `a`–`z`,
    /// `0`–`9` or one of `` -=[]\;',./` ``.
    Char(char),
}

impl Key {
    /// The Ghostty key and the unshifted codepoint the kitty protocol uses to
    /// identify it (NUL for keys without one).
    pub(crate) fn ghostty(self) -> (GhosttyKey, char) {
        let named = |key| (key, '\0');
        match self {
            Key::Enter => named(GhosttyKey::Enter),
            Key::Tab => named(GhosttyKey::Tab),
            Key::Backspace => named(GhosttyKey::Backspace),
            Key::Escape => named(GhosttyKey::Escape),
            Key::Space => (GhosttyKey::Space, ' '),
            Key::Up => named(GhosttyKey::ArrowUp),
            Key::Down => named(GhosttyKey::ArrowDown),
            Key::Left => named(GhosttyKey::ArrowLeft),
            Key::Right => named(GhosttyKey::ArrowRight),
            Key::Home => named(GhosttyKey::Home),
            Key::End => named(GhosttyKey::End),
            Key::PageUp => named(GhosttyKey::PageUp),
            Key::PageDown => named(GhosttyKey::PageDown),
            Key::Insert => named(GhosttyKey::Insert),
            Key::Delete => named(GhosttyKey::Delete),
            Key::F(number) => named(match number {
                1 => GhosttyKey::F1,
                2 => GhosttyKey::F2,
                3 => GhosttyKey::F3,
                4 => GhosttyKey::F4,
                5 => GhosttyKey::F5,
                6 => GhosttyKey::F6,
                7 => GhosttyKey::F7,
                8 => GhosttyKey::F8,
                9 => GhosttyKey::F9,
                10 => GhosttyKey::F10,
                11 => GhosttyKey::F11,
                12 => GhosttyKey::F12,
                _ => GhosttyKey::Unidentified,
            }),
            Key::Char(ch) => {
                let ch = ch.to_ascii_lowercase();
                let key = match ch {
                    'a' => GhosttyKey::A,
                    'b' => GhosttyKey::B,
                    'c' => GhosttyKey::C,
                    'd' => GhosttyKey::D,
                    'e' => GhosttyKey::E,
                    'f' => GhosttyKey::F,
                    'g' => GhosttyKey::G,
                    'h' => GhosttyKey::H,
                    'i' => GhosttyKey::I,
                    'j' => GhosttyKey::J,
                    'k' => GhosttyKey::K,
                    'l' => GhosttyKey::L,
                    'm' => GhosttyKey::M,
                    'n' => GhosttyKey::N,
                    'o' => GhosttyKey::O,
                    'p' => GhosttyKey::P,
                    'q' => GhosttyKey::Q,
                    'r' => GhosttyKey::R,
                    's' => GhosttyKey::S,
                    't' => GhosttyKey::T,
                    'u' => GhosttyKey::U,
                    'v' => GhosttyKey::V,
                    'w' => GhosttyKey::W,
                    'x' => GhosttyKey::X,
                    'y' => GhosttyKey::Y,
                    'z' => GhosttyKey::Z,
                    '0' => GhosttyKey::Digit0,
                    '1' => GhosttyKey::Digit1,
                    '2' => GhosttyKey::Digit2,
                    '3' => GhosttyKey::Digit3,
                    '4' => GhosttyKey::Digit4,
                    '5' => GhosttyKey::Digit5,
                    '6' => GhosttyKey::Digit6,
                    '7' => GhosttyKey::Digit7,
                    '8' => GhosttyKey::Digit8,
                    '9' => GhosttyKey::Digit9,
                    '-' => GhosttyKey::Minus,
                    '=' => GhosttyKey::Equal,
                    '[' => GhosttyKey::BracketLeft,
                    ']' => GhosttyKey::BracketRight,
                    '\\' => GhosttyKey::Backslash,
                    ';' => GhosttyKey::Semicolon,
                    '\'' => GhosttyKey::Quote,
                    ',' => GhosttyKey::Comma,
                    '.' => GhosttyKey::Period,
                    '/' => GhosttyKey::Slash,
                    '`' => GhosttyKey::Backquote,
                    ' ' => GhosttyKey::Space,
                    _ => GhosttyKey::Unidentified,
                };
                (key, ch)
            }
        }
    }
}
