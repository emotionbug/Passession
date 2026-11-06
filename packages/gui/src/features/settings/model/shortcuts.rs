//! Keyboard shortcut settings rules from `keyboard_shortcut_settings.dart`
//! and `ui/shortcuts.dart` (`ShortcutBinding`, `formatShortcutStep`).

use std::sync::LazyLock;

use makepad_widgets::{KeyCode, KeyModifiers};
use serde_json::Value;

use crate::features::settings::json::{get, obj, str_of};
use crate::features::settings::platform::{APPLE, MOBILE};

/// Shared `keyboard.json` catalog (needs §GuiShell 4).
static CATALOG: LazyLock<Vec<Value>> = LazyLock::new(|| {
    serde_json::from_str::<Vec<Value>>(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/resources/keyboard.json"))).unwrap_or_default()
});

#[derive(Clone, Debug)]
pub struct Binding {
    pub id: String,
    pub label: String,
    pub combo: String,
    pub section: String,
    pub help_id: String,
    pub index_payload: bool,
}

fn matches_platform(when: &Value) -> bool {
    let mac = cfg!(target_os = "macos");
    let mac_ok = when.get("mac").and_then(Value::as_bool).map(|m| m == mac).unwrap_or(true);
    let desktop_ok = when.get("desktop").and_then(Value::as_bool).map(|d| d != MOBILE).unwrap_or(true);
    mac_ok && desktop_ok
}

/// `ShortcutCatalog.load()`: platform-filtered bindings.
pub fn catalog() -> Vec<Binding> {
    CATALOG
        .iter()
        .filter(|binding| matches_platform(get(binding, "when")))
        .map(|binding| {
            let help = get(binding, "help");
            let action = str_of(binding, "action");
            Binding {
                id: str_of(binding, "id").to_string(),
                label: crate::features::settings::json::str_or(help, "label", action).to_string(),
                combo: str_of(binding, "combo").to_string(),
                section: str_of(help, "section").to_string(),
                help_id: crate::features::settings::json::str_or(help, "id", str_of(binding, "id")).to_string(),
                index_payload: str_of(get(binding, "payload"), "type") == "index",
            }
        })
        .collect()
}

/// Original `buildKeyboardShortcutHelpSections` order and title keys.
pub const SECTIONS: &[(&str, &str)] = &[
    ("general", "general"),
    ("workspaces", "workspaces"),
    ("tabs-panes", "tabsPanes"),
    ("layout", "layout"),
    ("agent-input", "agentInput"),
];

/// Bindings for one section, deduped by help id; a binding without a help
/// section stays rebindable under General.
pub fn section_rows<'a>(bindings: &'a [Binding], section: &str) -> Vec<&'a Binding> {
    let mut seen: Vec<&str> = Vec::new();
    bindings
        .iter()
        .filter(|b| b.section == section || (b.section.is_empty() && section == "general"))
        .filter(|b| {
            if seen.contains(&b.help_id.as_str()) {
                false
            } else {
                seen.push(&b.help_id);
                true
            }
        })
        .collect()
}

pub fn effective_combo(overrides: &serde_json::Map<String, Value>, binding: &Binding) -> String {
    match overrides.get(&binding.id).and_then(Value::as_str) {
        Some(combo) => combo.to_string(),
        None => binding.combo.clone(),
    }
}

pub fn overrides(preferences: &Value) -> serde_json::Map<String, Value> {
    obj(preferences, "keyboardShortcuts").clone()
}

/// Original `isShortcutCaptureComplete`.
pub fn capture_complete(binding: &Binding, steps: &[String]) -> bool {
    let Some(last) = steps.last() else { return false };
    if !binding.index_payload {
        return true;
    }
    let key = last.rsplit('+').next().unwrap_or("");
    key == "Digit" || (key.len() == 1 && matches!(key.as_bytes()[0], b'1'..=b'9'))
}

fn is_digit(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Key1 | KeyCode::Key2 | KeyCode::Key3 | KeyCode::Key4 | KeyCode::Key5 | KeyCode::Key6 | KeyCode::Key7 | KeyCode::Key8 | KeyCode::Key9
            | KeyCode::Key0
            | KeyCode::Numpad1 | KeyCode::Numpad2 | KeyCode::Numpad3 | KeyCode::Numpad4 | KeyCode::Numpad5 | KeyCode::Numpad6 | KeyCode::Numpad7
            | KeyCode::Numpad8 | KeyCode::Numpad9
    )
}

fn key_name(code: KeyCode) -> Option<String> {
    use KeyCode::*;
    let named = match code {
        Space => "Space",
        ReturnKey | NumpadEnter => "Enter",
        Escape => "Escape",
        Tab => "Tab",
        ArrowLeft => "ArrowLeft",
        ArrowRight => "ArrowRight",
        ArrowUp => "ArrowUp",
        ArrowDown => "ArrowDown",
        PageUp => "PageUp",
        PageDown => "PageDown",
        Backspace => "Backspace",
        Delete => "Delete",
        Home => "Home",
        End => "End",
        Insert => "Insert",
        Minus | NumpadSubtract => "-",
        Equals | NumpadEquals => "=",
        LBracket => "[",
        RBracket => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Comma => ",",
        Period | NumpadDecimal => ".",
        Slash | NumpadDivide => "/",
        Backtick => "`",
        NumpadAdd => "+",
        NumpadMultiply => "*",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        Key0 | Numpad0 => "0",
        Key1 | Numpad1 => "1",
        Key2 | Numpad2 => "2",
        Key3 | Numpad3 => "3",
        Key4 | Numpad4 => "4",
        Key5 | Numpad5 => "5",
        Key6 | Numpad6 => "6",
        Key7 | Numpad7 => "7",
        Key8 | Numpad8 => "8",
        Key9 | Numpad9 => "9",
        _ => {
            return code.to_char(true).map(|c| c.to_string());
        }
    };
    Some(named.to_string())
}

pub enum CaptureKey {
    Ignore,
    RemoveLast,
    Step(String),
}

/// Original `onCaptureKey` + `keyboardEventToComboString` (Ctrl Alt Shift Cmd).
pub fn capture_key(binding: &Binding, code: KeyCode, modifiers: KeyModifiers) -> CaptureKey {
    if matches!(code, KeyCode::Control | KeyCode::Alt | KeyCode::Shift | KeyCode::Logo | KeyCode::Capslock | KeyCode::Unknown) {
        return CaptureKey::Ignore;
    }
    let modified = modifiers.control || modifiers.logo || modifiers.alt || modifiers.shift;
    if code == KeyCode::Backspace && !modified {
        return CaptureKey::RemoveLast;
    }
    let key = if binding.index_payload && is_digit(code) && code != KeyCode::Key0 && code != KeyCode::Numpad0 {
        "Digit".to_string()
    } else {
        match key_name(code) {
            Some(name) if !name.is_empty() => name,
            _ => return CaptureKey::Ignore,
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if modifiers.control {
        parts.push("Ctrl".into());
    }
    if modifiers.alt {
        parts.push("Alt".into());
    }
    if modifiers.shift {
        parts.push("Shift".into());
    }
    if modifiers.logo {
        parts.push("Cmd".into());
    }
    parts.push(key);
    CaptureKey::Step(parts.join("+"))
}

/// `formatShortcutStep`.
pub fn format_step(step: &str) -> String {
    let mut parts: Vec<&str> = step.split('+').collect();
    let mut key = parts.pop().unwrap_or("").to_string();
    if key.is_empty() && parts.last() == Some(&"") {
        parts.pop();
        key = "+".into();
    }
    key = match key.as_str() {
        "Backspace" => "⌫".into(),
        "Enter" => "⏎".into(),
        "Esc" | "Escape" => "Esc".into(),
        "Space" => "␣".into(),
        "Left" | "ArrowLeft" => "←".into(),
        "Right" | "ArrowRight" => "→".into(),
        "Up" | "ArrowUp" => "↑".into(),
        "Down" | "ArrowDown" => "↓".into(),
        other if other.chars().count() == 1 => other.to_uppercase(),
        other => other.to_string(),
    };
    let modifiers: Vec<String> = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| if p.eq_ignore_ascii_case("cmd") { "meta".to_string() } else { p.to_lowercase() })
        .collect();
    if APPLE {
        let order = [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("mod", "⌘"), ("meta", "⌘")];
        let symbols: String = order.iter().filter(|(m, _)| modifiers.iter().any(|x| x == m)).map(|(_, s)| *s).collect();
        return format!("{symbols}{key}");
    }
    let mut out: Vec<String> = modifiers
        .iter()
        .map(|m| match m.as_str() {
            "mod" | "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "meta" => "Win".to_string(),
            other => other.to_string(),
        })
        .collect();
    out.push(key);
    out.join("+")
}

pub fn combo_steps(combo: &str) -> Vec<String> {
    combo.split_whitespace().map(String::from).collect()
}
