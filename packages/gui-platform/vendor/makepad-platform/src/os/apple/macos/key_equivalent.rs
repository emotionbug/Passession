//! Native Command menu shortcuts before editor/IME dispatch.

use crate::{
    event::{KeyCode, KeyModifiers},
    os::apple::{
        apple_sys::*,
        apple_util::{
            get_event_key_modifier, get_event_keycode, keycode_to_menu_key, nsstring_to_string,
            str_to_nsstring,
        },
    },
};

fn ascii_equivalent(key: KeyCode, modifiers: KeyModifiers, text: &str) -> Option<&'static str> {
    if !modifiers.logo || modifiers.control || text.is_empty() || text.is_ascii() {
        return None;
    }
    let equivalent = keycode_to_menu_key(key, modifiers.shift);
    (!equivalent.is_empty()).then_some(equivalent)
}

pub(super) unsafe fn perform(event: ObjcId) -> bool {
    let modifiers = get_event_key_modifier(event);
    if !modifiers.logo {
        return false;
    }
    let app: ObjcId = msg_send![class!(NSApplication), sharedApplication];
    let menu: ObjcId = msg_send![app, mainMenu];
    if menu == nil {
        return false;
    }
    let handled: BOOL = msg_send![menu, performKeyEquivalent: event];
    if handled == YES {
        return true;
    }

    // A Korean input source can supply ㅂ for physical Q. Try the original
    // equivalent first so custom Unicode and other Latin layouts keep priority.
    let characters: ObjcId = msg_send![event, charactersIgnoringModifiers];
    if characters == nil {
        return false;
    }
    let Some(key) = get_event_keycode(event) else {
        return false;
    };
    let Some(equivalent) = ascii_equivalent(key, modifiers, &nsstring_to_string(characters)) else {
        return false;
    };
    let flags: u64 = msg_send![event, modifierFlags];
    let location: NSPoint = msg_send![event, locationInWindow];
    let timestamp: f64 = msg_send![event, timestamp];
    let window: i64 = msg_send![event, windowNumber];
    let repeat: BOOL = msg_send![event, isARepeat];
    let code: u16 = msg_send![event, keyCode];
    let translated: ObjcId = msg_send![class!(NSEvent), keyEventWithType: NSEventType::NSKeyDown as u64
        location: location modifierFlags: flags timestamp: timestamp windowNumber: window context: nil
        characters: str_to_nsstring(equivalent) charactersIgnoringModifiers: str_to_nsstring(equivalent)
        isARepeat: repeat keyCode: code];
    if translated == nil {
        return false;
    }
    let handled: BOOL = msg_send![menu, performKeyEquivalent: translated];
    handled == YES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_equivalents_resolve_korean_keys_without_rewriting_latin_layouts() {
        let command = KeyModifiers {
            logo: true,
            ..Default::default()
        };
        assert_eq!(ascii_equivalent(KeyCode::KeyQ, command, "ㅂ"), Some("q"));
        assert_eq!(
            ascii_equivalent(
                KeyCode::KeyN,
                KeyModifiers {
                    shift: true,
                    ..command
                },
                "ㅜ"
            ),
            Some("N")
        );
        assert_eq!(ascii_equivalent(KeyCode::KeyY, command, "z"), None);
        assert_eq!(ascii_equivalent(KeyCode::KeyQ, command, "q"), None);
        assert_eq!(ascii_equivalent(KeyCode::KeyQ, command, ""), None);
    }

    #[test]
    fn composition_and_control_keys_do_not_become_command_equivalents() {
        assert_eq!(
            ascii_equivalent(KeyCode::KeyQ, KeyModifiers::default(), "ㅂ"),
            None
        );
        assert_eq!(
            ascii_equivalent(
                KeyCode::KeyQ,
                KeyModifiers {
                    control: true,
                    ..Default::default()
                },
                "ㅂ"
            ),
            None
        );
        assert_eq!(
            ascii_equivalent(
                KeyCode::KeyQ,
                KeyModifiers {
                    logo: true,
                    control: true,
                    ..Default::default()
                },
                "ㅂ"
            ),
            None
        );
    }
}
