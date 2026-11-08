use {
    std::rc::Rc,
    std::cell::RefCell,
    crate::{
        makepad_live_compiler::*,
        live_traits::*,
        makepad_derive_live::*,
        makepad_micro_serde::*,
        cx::Cx,
        event::{
            finger::KeyModifiers,
        },
        area::Area,
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextActionCapabilities {
    pub copy: bool,
    pub cut: bool,
    pub paste: bool,
    pub select_all: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TextActionsAnchor {
    /// Relative to the owner's unaligned origin; resolved after layout.
    #[cfg(any(target_os = "android", test))]
    Rect(crate::makepad_math::Rect),
    /// Painted caret/selection instances, including final alignment and clipping.
    Area(Area),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TextActionsRequest {
    pub token: u64,
    pub area: Area,
    pub anchor: TextActionsAnchor,
    pub capabilities: TextActionCapabilities,
}


#[derive(Default)]
pub struct CxKeyboard {
    pub (crate) prev_key_focus: Area,
    pub (crate) next_key_focus: Area,
    pub (crate) key_focus: Area,
    #[allow(dead_code)]
    pub (crate) keys_down: Vec<KeyEvent>,
    pub (crate) text_ime_dismissed: bool,
    pub (crate) text_selection: Area,
    pub (crate) text_actions: Option<TextActionsRequest>,
    pub(crate) text_handles: Option<super::text_handle::TextHandleRegistration>,
    pub(crate) text_handle_drag: Option<super::text_handle::TextHandleDrag>,
    #[cfg(any(target_os = "android", test))]
    text_action_token: u64,
}

impl CxKeyboard {
    pub fn modifiers(&self)->KeyModifiers{
        if let Some(key) = self.keys_down.first(){
            key.modifiers
        }
        else{
            Default::default()
        }
    }

    pub fn set_key_focus(&mut self, focus_area: Area) {
        self.text_ime_dismissed = false;
        self.next_key_focus = focus_area;
    }

    pub fn revert_key_focus(&mut self) {
        self.next_key_focus = self.prev_key_focus;
    }

    pub fn has_key_focus(&self, focus_area: Area) -> bool {
        focus_area != Area::Empty && self.key_focus == focus_area
    }

    pub fn set_text_ime_dismissed(&mut self) {
        self.text_ime_dismissed = true;
    }

    pub fn reset_text_ime_dismissed(&mut self) {
        self.text_ime_dismissed = false;
    }

    pub(crate) fn claim_text_selection(&mut self, area: Area) {
        if self.text_selection != area {
            self.text_actions = None;
            self.text_handles = None;
            self.text_selection = area;
        }
    }

    pub(crate) fn clear_text_selection(&mut self, area: Area) {
        if self.text_selection == area {
            self.text_selection = Area::Empty;
            self.text_handles = None;
            self.hide_text_actions(area);
        }
    }

    pub(crate) fn hide_text_actions(&mut self, area: Area) {
        if self.text_actions.is_some_and(|request| request.area == area) {
            self.text_actions = None;
        }
    }

    #[cfg(any(target_os = "android", test))]
    pub(crate) fn show_text_actions(
        &mut self,
        area: Area,
        anchor: TextActionsAnchor,
        capabilities: TextActionCapabilities,
    ) {
        self.claim_text_selection(area);
        let token = if let Some(request) = self.text_actions {
            request.token
        } else {
            self.text_action_token += 1;
            self.text_action_token
        };
        self.text_actions = Some(TextActionsRequest { token, area, anchor, capabilities });
    }

    pub (crate) fn update_area(&mut self, old_area: Area, new_area: Area) {
        if self.key_focus == old_area {
            self.key_focus = new_area
        }
        if self.prev_key_focus == old_area {
            self.prev_key_focus = new_area
        }
        if self.next_key_focus == old_area {
            self.next_key_focus = new_area
        }
        if self.text_selection == old_area {
            self.text_selection = new_area;
        }
        if let Some(request) = &mut self.text_actions {
            if request.area == old_area {
                request.area = new_area;
            }
            if request.anchor == TextActionsAnchor::Area(old_area) {
                request.anchor = TextActionsAnchor::Area(new_area);
            }
        }
        if let Some(handles) = &mut self.text_handles {
            if handles.owner == old_area { handles.owner = new_area; }
            for target in handles.targets.iter_mut().flatten() {
                if target.area == old_area { target.area = new_area; }
            }
        }
        if let Some(drag) = &mut self.text_handle_drag {
            if drag.owner == old_area { drag.owner = new_area; }
        }
    }
    /*
    pub (crate) fn all_keys_up(&mut self) -> Vec<KeyEvent> {
        let mut keys_down = Vec::new();
        std::mem::swap(&mut keys_down, &mut self.keys_down);
        keys_down
    }*/

    pub (crate) fn cycle_key_focus_changed(&mut self) -> Option<(Area, Area)> {
        if self.next_key_focus != self.key_focus {
            self.prev_key_focus = self.key_focus;
            self.key_focus = self.next_key_focus;
            return Some((self.prev_key_focus, self.key_focus))
        }
        None
    }
    
    #[allow(dead_code)]
    pub fn is_key_down(&mut self, key_code: KeyCode)->bool{
        if let Some(_) = self.keys_down.iter().position( | k | k.key_code == key_code) {
            return true;
        }
        return false
    }

    #[allow(dead_code)]
    pub (crate) fn process_key_down(&mut self, key_event: KeyEvent) {
        if let Some(_) = self.keys_down.iter().position( | k | k.key_code == key_event.key_code) {
            return;
        }
        self.keys_down.push(key_event);
    }

    #[allow(dead_code)]
    pub (crate) fn process_key_up(&mut self, key_event: KeyEvent) {
        if let Some(pos) = self.keys_down.iter().position( | k | k.key_code == key_event.key_code) {
            self.keys_down.remove(pos);
        }
    }
}

#[cfg(test)]
mod focus_tests {
    use super::*;

    #[test]
    fn empty_area_never_owns_focus_before_or_after_blur() {
        let mut lists = crate::draw_list::CxDrawListPool::default();
        let list = lists.alloc();
        let editor = Area::Rect(crate::area::RectArea {
            draw_list_id: list.id(), rect_id: 0, redraw_id: 1,
        });
        let mut keyboard = CxKeyboard::default();
        assert!(!keyboard.has_key_focus(Area::Empty));
        keyboard.set_key_focus(editor);
        assert!(!keyboard.has_key_focus(editor));
        assert_eq!(keyboard.cycle_key_focus_changed(), Some((Area::Empty, editor)));
        assert!(keyboard.has_key_focus(editor));
        assert!(!keyboard.has_key_focus(Area::Empty));
        keyboard.set_key_focus(Area::Empty);
        assert!(keyboard.has_key_focus(editor));
        assert_eq!(keyboard.cycle_key_focus_changed(), Some((editor, Area::Empty)));
        assert!(!keyboard.has_key_focus(editor));
        assert!(!keyboard.has_key_focus(Area::Empty));
    }

    #[test]
    fn text_actions_refresh_preserves_token_but_new_ownership_retires_it() {
        let mut lists = crate::draw_list::CxDrawListPool::default();
        let list = lists.alloc();
        let first = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        let second = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 1, redraw_id: 1 });
        let mut keyboard = CxKeyboard::default();
        let mut caps = TextActionCapabilities { copy: true, ..Default::default() };
        keyboard.show_text_actions(first, TextActionsAnchor::Rect(Default::default()), caps);
        let first_token = keyboard.text_actions.unwrap().token;
        caps.copy = false;
        caps.paste = true;
        keyboard.show_text_actions(first, TextActionsAnchor::Rect(Default::default()), caps);
        assert_eq!(keyboard.text_actions.unwrap().token, first_token);
        assert_eq!(keyboard.text_actions.unwrap().capabilities, caps);

        keyboard.set_key_focus(second);
        keyboard.cycle_key_focus_changed();
        keyboard.show_text_actions(second, TextActionsAnchor::Rect(Default::default()), caps);
        let second_token = keyboard.text_actions.unwrap().token;
        assert!(second_token > first_token);
        keyboard.clear_text_selection(first);
        assert_eq!(keyboard.text_selection, second);
        assert_eq!(keyboard.text_actions.unwrap().token, second_token);
        assert!(keyboard.has_key_focus(second));

        keyboard.hide_text_actions(second);
        assert_eq!(keyboard.text_selection, second);
        assert!(keyboard.text_actions.is_none());
        keyboard.show_text_actions(second, TextActionsAnchor::Rect(Default::default()), caps);
        assert!(keyboard.text_actions.unwrap().token > second_token);
        keyboard.clear_text_selection(second);
        assert_eq!(keyboard.text_selection, Area::Empty);
        assert!(keyboard.text_actions.is_none());
        assert!(keyboard.has_key_focus(second));
    }

    #[test]
    fn text_actions_redraw_remaps_selection_and_ignores_old_area_cleanup() {
        let mut lists = crate::draw_list::CxDrawListPool::default();
        let list = lists.alloc();
        let old = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        let new = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 2 });
        let mut keyboard = CxKeyboard::default();
        keyboard.set_key_focus(old);
        keyboard.cycle_key_focus_changed();
        keyboard.show_text_actions(old, TextActionsAnchor::Area(old), TextActionCapabilities { copy: true, ..Default::default() });
        let token = keyboard.text_actions.unwrap().token;
        keyboard.update_area(old, new);
        keyboard.clear_text_selection(old);
        assert!(keyboard.has_key_focus(new));
        assert_eq!(keyboard.text_selection, new);
        let request = keyboard.text_actions.unwrap();
        assert_eq!(request.area, new);
        assert_eq!(request.anchor, TextActionsAnchor::Area(new));
        assert_eq!(request.token, token);
    }

    #[test]
    fn handle_registration_follows_redraw_and_stale_owner_cannot_clear_it() {
        use crate::event::{TextHandleEndpoint, TextHandleTarget, text_handle::TextHandleRegistration};
        let mut lists = crate::draw_list::CxDrawListPool::default();
        let list = lists.alloc();
        let old = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 1 });
        let new = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 0, redraw_id: 2 });
        let other = Area::Rect(crate::area::RectArea { draw_list_id: list.id(), rect_id: 1, redraw_id: 2 });
        let mut keyboard = CxKeyboard::default();
        keyboard.claim_text_selection(old);
        keyboard.text_handles = Some(TextHandleRegistration { id: 1, owner: old, viewport: Default::default(),
            targets: [Some(TextHandleTarget { area: old, endpoint: TextHandleEndpoint::Anchor, caret_offset: Default::default() }), None] });
        keyboard.update_area(old, new);
        keyboard.clear_text_selection(old);
        let registration = keyboard.text_handles.unwrap();
        assert_eq!(registration.owner, new);
        assert_eq!(registration.targets[0].unwrap().area, new);
        keyboard.claim_text_selection(other);
        assert!(keyboard.text_handles.is_none());
        keyboard.clear_text_selection(new);
        assert_eq!(keyboard.text_selection, other);
        keyboard.clear_text_selection(other);
        assert_eq!(keyboard.text_selection, Area::Empty);
    }
}

#[derive(Clone, Copy, Debug, Default, SerBin, DeBin, SerJson, DeJson, PartialEq)]
pub struct KeyEvent {
    pub key_code: KeyCode,
    pub is_repeat: bool,
    pub modifiers: KeyModifiers,
    pub time: f64
}

#[derive(Clone, Debug)]
pub struct KeyFocusEvent {
    pub prev: Area,
    pub focus: Area,
}

#[derive(Clone, Debug, SerBin, DeBin, SerJson, DeJson, PartialEq)]
pub struct TextInputEvent {
    pub input: String,
    pub replace_last: bool,
    pub was_paste: bool
}

/// Android full-editor snapshot. Offsets are UTF-16 code units.
#[derive(Clone, Debug)]
pub struct TextImeEvent {
    pub session: u64,
    pub revision: u64,
    pub text: String,
    pub selection_start: usize,
    pub selection_end: usize,
    pub composing: Option<(usize, usize)>,
}

impl TextImeEvent {
    pub fn selection_utf8(&self) -> (usize, usize) {
        (utf16_to_utf8(&self.text, self.selection_start),
            utf16_to_utf8(&self.text, self.selection_end))
    }

    /// Smallest scalar-aligned replacement, keeping native history/selection.
    pub fn replacement<'a>(&'a self, previous: &str) -> (usize, usize, &'a str) {
        let prefix = previous.chars().zip(self.text.chars())
            .take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum::<usize>();
        let suffix = previous[prefix..].chars().rev().zip(self.text[prefix..].chars().rev())
            .take_while(|(a, b)| a == b).map(|(c, _)| c.len_utf8()).sum::<usize>();
        (prefix, previous.len() - suffix, &self.text[prefix..self.text.len() - suffix])
    }
}

fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > offset { return index; }
        units += ch.len_utf16();
    }
    text.len()
}

#[cfg(test)]
mod android_ime_tests {
    use super::*;

    fn snapshot(text: &str, start: usize, end: usize) -> TextImeEvent {
        TextImeEvent { session: 1, revision: 1, text: text.into(),
            selection_start: start, selection_end: end, composing: None }
    }

    #[test]
    fn hangul_replacement_commit_and_deletion() {
        let mut text = String::new();
        for value in ["ㅎ", "하", "한", "한", "하", "", "한국어", "한국어 paste"] {
            let edit = snapshot(value, value.encode_utf16().count(), value.encode_utf16().count());
            let (start, end, replacement) = edit.replacement(&text);
            text.replace_range(start..end, replacement);
            assert_eq!(text, value);
            assert_eq!(edit.selection_utf8(), (text.len(), text.len()));
        }
    }

    #[test]
    fn utf16_selection_and_middle_replacement_preserve_emoji() {
        let edit = snapshot("A😀한Z", 3, 4);
        assert_eq!(edit.selection_utf8(), (5, 8));
        assert_eq!(edit.replacement("A😀하Z"), (5, 8, "한"));
        assert_eq!(snapshot("😀", 1, 2).selection_utf8(), (0, 4));
        assert_eq!(snapshot("한", 99, 99).selection_utf8(), (3, 3));
    }
}

#[derive(Clone, Debug)]
pub struct TextClipboardEvent {
    pub response: Rc<RefCell<Option<String>>>
}

impl Default for KeyCode {
    fn default() -> Self {KeyCode::Unknown}
}


// lowest common denominator keymap between desktop and web
#[derive(Live, LiveHook, Clone, Copy, Debug, SerBin, DeBin, SerJson, DeJson, Eq, PartialEq)]
pub enum KeyCode {
    #[pick] Escape,

    Back,

    Backtick,
    Key0,
    Key1,
    Key2,
    Key3,
    Key4,
    Key5,
    Key6,
    Key7,
    Key8,
    Key9,
    Minus,
    Equals,

    Backspace,
    Tab,

    KeyQ,
    KeyW,
    KeyE,
    KeyR,
    KeyT,
    KeyY,
    KeyU,
    KeyI,
    KeyO,
    KeyP,
    LBracket,
    RBracket,
    ReturnKey,

    KeyA,
    KeyS,
    KeyD,
    KeyF,
    KeyG,
    KeyH,
    KeyJ,
    KeyK,
    KeyL,
    Semicolon,
    Quote,
    Backslash,

    KeyZ,
    KeyX,
    KeyC,
    KeyV,
    KeyB,
    KeyN,
    KeyM,
    Comma,
    Period,
    Slash,

    Control,
    Alt,
    Shift,
    Logo,

    Space,
    Capslock,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,

    PrintScreen,
    ScrollLock,
    Pause,

    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,

    Numpad0,
    Numpad1,
    Numpad2,
    Numpad3,
    Numpad4,
    Numpad5,
    Numpad6,
    Numpad7,
    Numpad8,
    Numpad9,

    NumpadEquals,
    NumpadSubtract,
    NumpadAdd,
    NumpadDecimal,
    NumpadMultiply,
    NumpadDivide,
    Numlock,
    NumpadEnter,

    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,

    Unknown,
}

impl KeyCode{
    pub fn is_unknown(&self)->bool{
        match self{
            Self::Unknown=>true,
            _=>false
        }
    }
    pub fn to_char(&self, uc:bool)->Option<char>{
        match self {
            KeyCode::KeyA => if uc {Some('A')}else {Some('a')},
            KeyCode::KeyB => if uc {Some('B')}else {Some('b')},
            KeyCode::KeyC => if uc {Some('C')}else {Some('c')},
            KeyCode::KeyD => if uc {Some('D')}else {Some('d')},
            KeyCode::KeyE => if uc {Some('E')}else {Some('e')},
            KeyCode::KeyF => if uc {Some('F')}else {Some('f')},
            KeyCode::KeyG => if uc {Some('G')}else {Some('g')},
            KeyCode::KeyH => if uc {Some('H')}else {Some('h')},
            KeyCode::KeyI => if uc {Some('I')}else {Some('i')},
            KeyCode::KeyJ => if uc {Some('J')}else {Some('j')},
            KeyCode::KeyK => if uc {Some('K')}else {Some('k')},
            KeyCode::KeyL => if uc {Some('L')}else {Some('l')},
            KeyCode::KeyM => if uc {Some('M')}else {Some('m')},
            KeyCode::KeyN => if uc {Some('N')}else {Some('n')},
            KeyCode::KeyO => if uc {Some('O')}else {Some('o')},
            KeyCode::KeyP => if uc {Some('P')}else {Some('p')},
            KeyCode::KeyQ => if uc {Some('Q')}else {Some('q')},
            KeyCode::KeyR => if uc {Some('R')}else {Some('r')},
            KeyCode::KeyS => if uc {Some('S')}else {Some('s')},
            KeyCode::KeyT => if uc {Some('T')}else {Some('t')},
            KeyCode::KeyU => if uc {Some('U')}else {Some('u')},
            KeyCode::KeyV => if uc {Some('V')}else {Some('v')},
            KeyCode::KeyW => if uc {Some('W')}else {Some('w')},
            KeyCode::KeyX => if uc {Some('X')}else {Some('x')},
            KeyCode::KeyY => if uc {Some('Y')}else {Some('y')},
            KeyCode::KeyZ => if uc {Some('Z')}else {Some('z')},
            KeyCode::Key0 => if uc {Some(')')}else {Some('0')},
            KeyCode::Key1 => if uc {Some('!')}else {Some('1')},
            KeyCode::Key2 => if uc {Some('@')}else {Some('2')},
            KeyCode::Key3 => if uc {Some('#')}else {Some('3')},
            KeyCode::Key4 => if uc {Some('$')}else {Some('4')},
            KeyCode::Key5 => if uc {Some('%')}else {Some('5')},
            KeyCode::Key6 => if uc {Some('^')}else {Some('6')},
            KeyCode::Key7 => if uc {Some('&')}else {Some('7')},
            KeyCode::Key8 => if uc {Some('*')}else {Some('8')},
            KeyCode::Key9 => if uc {Some('(')}else {Some('9')},
            KeyCode::Equals => if uc {Some('+')}else {Some('=')},
            KeyCode::Minus => if uc {Some('_')}else {Some('-')},
            KeyCode::RBracket => if uc {Some('{')}else {Some('[')},
            KeyCode::LBracket => if uc {Some('}')}else {Some(']')},
            KeyCode::ReturnKey => Some('\n'),
            KeyCode::Backtick => if uc {Some('~')}else {Some('`')},
            KeyCode::Semicolon => if uc {Some(':')}else {Some(';')},
            KeyCode::Backslash => if uc {Some('|')}else {Some('\\')},
            KeyCode::Comma => if uc {Some('<')}else {Some(',')},
            KeyCode::Slash => if uc {Some('?')}else {Some('/')},
            KeyCode::Period => if uc {Some('>')}else {Some('.')},
            KeyCode::Tab => Some('\t'),
            KeyCode::Space => Some(' '),
            KeyCode::NumpadDecimal => Some('.'),
            KeyCode::NumpadMultiply => Some('*'),
            KeyCode::NumpadAdd => Some('+'),
            KeyCode::NumpadDivide => Some('/'),
            KeyCode::NumpadEnter => Some('\n'),
            KeyCode::NumpadSubtract => Some('-'),
            KeyCode::Numpad0 => Some('0'),
            KeyCode::Numpad1 => Some('1'),
            KeyCode::Numpad2 => Some('2'),
            KeyCode::Numpad3 => Some('3'),
            KeyCode::Numpad4 => Some('4'),
            KeyCode::Numpad5 => Some('5'),
            KeyCode::Numpad6 => Some('6'),
            KeyCode::Numpad7 => Some('7'),
            KeyCode::Numpad8 => Some('8'),
            KeyCode::Numpad9 => Some('9'),
            _ => None
        }
    }
}
