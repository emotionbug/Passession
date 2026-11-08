//! Keyboard, text and input-method mapping for the interactive terminal.
//!
//! Desktop platforms deliver a `KeyDown` for every key and a separate `TextInput` with the
//! committed text of printable keys and input methods (X11, macOS and Windows deliver only
//! committed text; composition stays in the system IME). Printable keys are therefore sent
//! from `TextInput` as UTF-8, while `KeyDown` encodes only non-text keys and Ctrl/Alt
//! combinations through Ghostty's key encoder (application cursor keys, kitty keyboard,
//! modifyOtherKeys).
//!
//! Clipboard shortcuts follow terminal conventions: Ctrl+Shift+C/V on Linux and Windows,
//! Cmd+C/V on Apple platforms; Ctrl+C with a selection on Linux and Windows copies it like
//! Windows Terminal, without one it is ^C. The platform layer turns Ctrl/Cmd+V into a paste
//! `TextInput` before the key arrives, so Ctrl+V pastes too.
//!
//! Android edits a hidden editor ([`ImeBridge`]); iOS reports Korean syllable updates as
//! replacements of the previous input, rubbed out with DEL like the original native input.

use makepad_widgets::{KeyCode, KeyModifiers};
use paseo_terminal::{Key, Modifiers};

/// What one `KeyDown` does in the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCommand {
    /// Encode and send the key; a `TextInput` the same key produces is not sent again.
    Send(Key, Modifiers),
    Copy,
    SelectAll,
    Find,
    /// Text keys (sent from `TextInput`), modifiers, platform paste and app shortcuts.
    Ignore,
}

pub fn modifiers(m: KeyModifiers) -> Modifiers {
    Modifiers { shift: m.shift, control: m.control, alt: m.alt, logo: m.logo }
}

/// A key with its own terminal encoding, independent of the text it types.
fn named(code: KeyCode) -> Option<Key> {
    Some(match code {
        KeyCode::ReturnKey | KeyCode::NumpadEnter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Escape => Key::Escape,
        KeyCode::ArrowUp => Key::Up,
        KeyCode::ArrowDown => Key::Down,
        KeyCode::ArrowLeft => Key::Left,
        KeyCode::ArrowRight => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Insert => Key::Insert,
        KeyCode::Delete => Key::Delete,
        KeyCode::F1 => Key::F(1),
        KeyCode::F2 => Key::F(2),
        KeyCode::F3 => Key::F(3),
        KeyCode::F4 => Key::F(4),
        KeyCode::F5 => Key::F(5),
        KeyCode::F6 => Key::F(6),
        KeyCode::F7 => Key::F(7),
        KeyCode::F8 => Key::F(8),
        KeyCode::F9 => Key::F(9),
        KeyCode::F10 => Key::F(10),
        KeyCode::F11 => Key::F(11),
        KeyCode::F12 => Key::F(12),
        _ => return None,
    })
}

/// A text key by its unshifted US-layout character.
fn character(code: KeyCode) -> Option<Key> {
    let ch = match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        KeyCode::Key0 | KeyCode::Numpad0 => '0',
        KeyCode::Key1 | KeyCode::Numpad1 => '1',
        KeyCode::Key2 | KeyCode::Numpad2 => '2',
        KeyCode::Key3 | KeyCode::Numpad3 => '3',
        KeyCode::Key4 | KeyCode::Numpad4 => '4',
        KeyCode::Key5 | KeyCode::Numpad5 => '5',
        KeyCode::Key6 | KeyCode::Numpad6 => '6',
        KeyCode::Key7 | KeyCode::Numpad7 => '7',
        KeyCode::Key8 | KeyCode::Numpad8 => '8',
        KeyCode::Key9 | KeyCode::Numpad9 => '9',
        KeyCode::Minus | KeyCode::NumpadSubtract => '-',
        KeyCode::Equals | KeyCode::NumpadEquals => '=',
        KeyCode::LBracket => '[',
        KeyCode::RBracket => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Comma => ',',
        KeyCode::Period | KeyCode::NumpadDecimal => '.',
        KeyCode::Slash | KeyCode::NumpadDivide => '/',
        KeyCode::Backtick => '`',
        KeyCode::Space => return Some(Key::Space),
        _ => return None,
    };
    Some(Key::Char(ch))
}

/// The terminal meaning of a `KeyDown` on a platform (`apple`: Cmd is the command key;
/// `android`: Ctrl+A is the text toolbar's synthetic Select all).
pub fn key_command(code: KeyCode, m: KeyModifiers, has_selection: bool, apple: bool, android: bool) -> KeyCommand {
    if matches!(code, KeyCode::Control | KeyCode::Alt | KeyCode::Shift | KeyCode::Logo | KeyCode::Capslock | KeyCode::Unknown) {
        return KeyCommand::Ignore;
    }
    let mods = modifiers(m);
    if apple && m.logo {
        // Command shortcuts belong to the app; the terminal takes copy, select all and find.
        return match code {
            KeyCode::KeyC => KeyCommand::Copy,
            KeyCode::KeyA => KeyCommand::SelectAll,
            KeyCode::KeyF => KeyCommand::Find,
            _ => KeyCommand::Ignore,
        };
    }
    if m.control && !m.alt && code == KeyCode::KeyV {
        // The platform already pasted (`TextInput { was_paste }`).
        return KeyCommand::Ignore;
    }
    if !apple && m.control && !m.alt {
        match code {
            KeyCode::KeyC if m.shift || has_selection => return KeyCommand::Copy,
            KeyCode::KeyF if m.shift => return KeyCommand::Find,
            KeyCode::KeyA if m.shift || android => return KeyCommand::SelectAll,
            _ => {}
        }
    }
    if let Some(key) = named(code) {
        return KeyCommand::Send(key, mods);
    }
    match character(code) {
        Some(key) if m.control || m.alt || (m.logo && !apple) => KeyCommand::Send(key, mods),
        _ => KeyCommand::Ignore,
    }
}

/// Committed text as terminal input: a typed newline is Enter (CR).
pub fn text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

/// A single typed character combined with sticky key-bar modifiers, as an encodable key.
pub fn sticky_key(text: &str, mods: Modifiers) -> Option<(Key, Modifiers)> {
    let mut chars = text.chars();
    let (Some(ch), None) = (chars.next(), chars.next()) else { return None };
    if ch == ' ' {
        return Some((Key::Space, mods));
    }
    let lower = ch.to_ascii_lowercase();
    let mappable = lower.is_ascii_lowercase() || lower.is_ascii_digit() || "-=[]\\;',./`".contains(lower);
    mappable.then_some((Key::Char(lower), Modifiers { shift: mods.shift || ch.is_ascii_uppercase(), ..mods }))
}

/// The hidden editor Android's input method edits. Each snapshot's committed text (the
/// editor minus its composing region) is diffed against the previous snapshot's: removals
/// become DEL, insertions are sent; the composing region (Hangul syllables, predicted words)
/// is sent only once committed. The editor starts as one no-break space, which makes
/// Backspace on an "empty" terminal line observable and does not start a sentence (no
/// auto-capitalization).
///
/// The bridge never rewrites the editor while edits are in flight: an echo the platform
/// refuses (the IME already sent a newer revision) would make every later snapshot repeat
/// text. Instead it asks for a reset after Enter, an emptied editor or a long line, which the
/// view publishes as a new input session; that session fences edits of the old editor.
pub struct ImeBridge {
    /// The committed text the editor holds.
    base: String,
}

pub const IME_SENTINEL: &str = "\u{a0}";

/// Committed characters after which an idle editor is reset, bounding the text the input
/// method keeps.
const IME_RESET_AFTER: usize = 1024;

/// One input-method edit as terminal input.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImeEdit {
    /// Characters to rub out with DEL.
    pub removed: usize,
    pub inserted: String,
    /// Publish [`IME_SENTINEL`] again in a new input session.
    pub reset: bool,
}

impl Default for ImeBridge {
    fn default() -> Self {
        Self { base: IME_SENTINEL.to_owned() }
    }
}

fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > offset {
            return index;
        }
        units += ch.len_utf16();
    }
    text.len()
}

impl ImeBridge {
    /// The editor was republished as [`IME_SENTINEL`].
    pub fn reset(&mut self) {
        self.base.clear();
        self.base.push_str(IME_SENTINEL);
    }

    /// Applies an editor snapshot; `composing` is the UTF-16 composing range.
    pub fn apply(&mut self, text: &str, composing: Option<(usize, usize)>) -> ImeEdit {
        let composing = composing.filter(|(start, end)| start < end);
        let committed = match composing {
            Some((start, end)) => {
                let (start, end) = (utf16_to_utf8(text, start), utf16_to_utf8(text, end));
                format!("{}{}", &text[..start], &text[end..])
            }
            None => text.to_owned(),
        };
        let prefix = self.base.chars().zip(committed.chars()).take_while(|(a, b)| a == b).count();
        let removed = self.base.chars().count() - prefix;
        let inserted: String = committed.chars().skip(prefix).collect();
        let reset = composing.is_none()
            && (text.is_empty() || inserted.contains('\n') || committed.chars().count() > IME_RESET_AFTER);
        self.base = committed;
        ImeEdit { removed, inserted, reset }
    }
}

/// iOS input: the previous input is replaced (`replace_last`) while a Korean syllable is
/// composed (`ㅎ` → `하` → `한`); rub it out and send the replacement.
#[derive(Default)]
pub struct ReplaceTracker {
    last: String,
}

impl ReplaceTracker {
    /// The DEL count and the text for one `TextInput`.
    pub fn apply(&mut self, input: &str, replace_last: bool) -> usize {
        let removed = if replace_last { self.last.chars().count() } else { 0 };
        self.last.clear();
        self.last.push_str(input);
        removed
    }

    /// A key or paste ended the replaceable input.
    pub fn clear(&mut self) {
        self.last.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(control: bool, shift: bool, alt: bool, logo: bool) -> KeyModifiers {
        KeyModifiers { shift, control, alt, logo }
    }

    #[test]
    fn printable_keys_wait_for_text_and_combos_encode() {
        assert_eq!(key_command(KeyCode::KeyA, m(false, false, false, false), false, false, false), KeyCommand::Ignore);
        assert_eq!(key_command(KeyCode::KeyA, m(false, true, false, false), false, false, false), KeyCommand::Ignore);
        assert!(matches!(key_command(KeyCode::KeyA, m(true, false, false, false), false, false, false), KeyCommand::Send(Key::Char('a'), _)));
        assert!(matches!(key_command(KeyCode::KeyB, m(false, false, true, false), false, true, false), KeyCommand::Send(Key::Char('b'), _)));
        assert!(matches!(key_command(KeyCode::ArrowUp, m(false, true, false, false), false, false, false), KeyCommand::Send(Key::Up, _)));
        assert_eq!(key_command(KeyCode::Control, m(true, false, false, false), false, false, false), KeyCommand::Ignore);
    }

    #[test]
    fn ctrl_c_interrupts_unless_copying() {
        let ctrl = m(true, false, false, false);
        assert!(matches!(key_command(KeyCode::KeyC, ctrl, false, false, false), KeyCommand::Send(Key::Char('c'), _)));
        assert_eq!(key_command(KeyCode::KeyC, ctrl, true, false, false), KeyCommand::Copy);
        assert_eq!(key_command(KeyCode::KeyC, m(true, true, false, false), false, false, false), KeyCommand::Copy);
        // macOS: Ctrl+C is always ^C, Cmd+C copies.
        assert!(matches!(key_command(KeyCode::KeyC, ctrl, true, true, false), KeyCommand::Send(Key::Char('c'), _)));
        assert_eq!(key_command(KeyCode::KeyC, m(false, false, false, true), true, true, false), KeyCommand::Copy);
        assert_eq!(key_command(KeyCode::KeyW, m(false, false, false, true), false, true, false), KeyCommand::Ignore);
        assert_eq!(key_command(KeyCode::KeyV, ctrl, false, false, false), KeyCommand::Ignore);
        assert_eq!(key_command(KeyCode::KeyA, ctrl, false, false, true), KeyCommand::SelectAll);
    }

    #[test]
    fn sticky_modifiers_map_typed_characters() {
        let ctrl = Modifiers { control: true, ..Modifiers::default() };
        assert_eq!(sticky_key("c", ctrl), Some((Key::Char('c'), ctrl)));
        assert_eq!(sticky_key("C", ctrl), Some((Key::Char('c'), Modifiers { shift: true, ..ctrl })));
        assert_eq!(sticky_key("한", ctrl), None);
        assert_eq!(sticky_key("ab", ctrl), None);
        assert_eq!(text_bytes("a\nb"), b"a\rb");
    }

    #[test]
    fn android_composition_is_sent_only_when_committed() {
        let mut ime = ImeBridge::default();
        let s = IME_SENTINEL;
        assert_eq!(ime.apply(&format!("{s}ㅎ"), Some((1, 2))), ImeEdit::default());
        assert_eq!(ime.apply(&format!("{s}한"), Some((1, 2))), ImeEdit::default());
        assert_eq!(ime.apply(&format!("{s}한ㄱ"), Some((2, 3))), ImeEdit { removed: 0, inserted: "한".into(), reset: false });
        assert_eq!(ime.apply(&format!("{s}한글 "), None), ImeEdit { removed: 0, inserted: "글 ".into(), reset: false });
        // Later snapshots continue from the editor's text, never from an assumed reset.
        assert_eq!(ime.apply(&format!("{s}한글 ab"), Some((5, 6))), ImeEdit { removed: 0, inserted: "a".into(), reset: false });
        assert_eq!(ime.apply(&format!("{s}한글 a"), None), ImeEdit { removed: 0, inserted: String::new(), reset: false });
        // Backspacing past the sentinel rubs out the shell's text and asks for a reset.
        assert_eq!(ime.apply("", None), ImeEdit { removed: 5, inserted: String::new(), reset: true });
        ime.reset();
        assert_eq!(ime.apply(&format!("{s}ls\n"), None), ImeEdit { removed: 0, inserted: "ls\n".into(), reset: true });
    }

    #[test]
    fn ios_replacements_rub_out_the_previous_input() {
        let mut tracker = ReplaceTracker::default();
        assert_eq!(tracker.apply("ㅎ", false), 0);
        assert_eq!(tracker.apply("하", true), 1);
        assert_eq!(tracker.apply("한", true), 1);
        assert_eq!(tracker.apply("a", false), 0);
    }
}
