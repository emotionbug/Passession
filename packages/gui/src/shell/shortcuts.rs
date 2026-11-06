//! Port of `lib/ui/shortcuts.dart` (catalog, chords, focus-scope gates, overrides) plus the
//! shortcut label formatting of `command_center.dart` (`formatShortcutStep`).

use makepad_widgets::*;
use serde_json::{Map, Value, json};

use crate::common::json as j;

const CATALOG: &str = include_str!("../../resources/keyboard.json");

const IS_MAC: bool = cfg!(any(target_os = "macos", target_os = "ios"));
const IS_MOBILE: bool = cfg!(any(target_os = "android", target_os = "ios"));

thread_local! {
    static BASE: Vec<Value> = {
        let raw: Value = serde_json::from_str(CATALOG).expect("bundled keyboard.json is valid JSON");
        raw.as_array().cloned().unwrap_or_default().into_iter().filter(matches_platform).collect()
    };
}

fn matches_platform(binding: &Value) -> bool {
    let when = j::object(binding, "when");
    let mac_ok = when.get("mac").and_then(Value::as_bool).is_none_or(|mac| mac == IS_MAC);
    let desktop_ok = when.get("desktop").and_then(Value::as_bool).is_none_or(|desktop| desktop != IS_MOBILE);
    mac_ok && desktop_ok
}

/// `ShortcutBinding.parse`: "Mod+Shift+K  G" → chord steps.
pub fn parse_combo(value: &str) -> Option<Vec<Value>> {
    let mut steps = Vec::new();
    for step in value.split_whitespace() {
        let mut parts: Vec<&str> = step.split('+').collect();
        let key = parts.pop()?;
        if key.is_empty() {
            return None;
        }
        let mut chord = Map::new();
        chord.insert("key".into(), json!(key.to_lowercase()));
        chord.insert("code".into(), json!(key));
        for (name, flag) in [("Ctrl", "ctrl"), ("Alt", "alt"), ("Shift", "shift"), ("Mod", "mod")] {
            if parts.contains(&name) {
                chord.insert(flag.into(), json!(true));
            }
        }
        if parts.contains(&"Cmd") || parts.contains(&"Meta") {
            chord.insert("meta".into(), json!(true));
        }
        steps.push(Value::Object(chord));
    }
    Some(steps)
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub data: Value,
    pub chord: Vec<Value>,
}

impl Binding {
    pub fn id(&self) -> &str {
        j::text(&self.data, "id")
    }
    pub fn action(&self) -> &str {
        j::text(&self.data, "action")
    }
    pub fn label(&self) -> &str {
        let label = j::text(j::object(&self.data, "help"), "label");
        if label.is_empty() { self.action() } else { label }
    }
    pub fn combo(&self) -> &str {
        j::text(&self.data, "combo")
    }
}

/// The catalog with user overrides (`preferences.keyboardShortcuts[id] = "combo"`).
pub fn bindings(overrides: &Value) -> Vec<Binding> {
    BASE.with(|base| {
        base.iter()
            .map(|data| {
                let chord = match overrides.get(j::text(data, "id")).and_then(Value::as_str) {
                    Some(raw) => parse_combo(raw).unwrap_or_default(),
                    None => j::array(data, "parsedChord").to_vec(),
                };
                Binding { data: data.clone(), chord }
            })
            .collect()
    })
}

/// What the router asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatch {
    pub action: String,
    pub args: Value,
    /// `preventDefault == false`: the key still reaches the focused widget.
    pub pass_through: bool,
}

/// A late blur from the old field must not clear the newly focused field's scope.
#[derive(Default)]
pub struct FocusScope(Option<(WidgetUid, String)>);

impl FocusScope {
    pub fn update(&mut self, owner: WidgetUid, scope: Option<String>) {
        if let Some(scope) = scope {
            self.0 = Some((owner, scope));
        } else if self.0.as_ref().is_some_and(|(current, _)| *current == owner) {
            self.0 = None;
        }
    }

    pub fn name(&self) -> &str {
        self.0.as_ref().map_or("workspace", |(_, scope)| scope.as_str())
    }
}

#[derive(Default)]
pub struct Router {
    bindings: Vec<Binding>,
    candidates: Vec<Binding>,
    step: usize,
    overrides_key: String,
    timeout: Timer,
}

impl Router {
    /// Rebuilds bindings when `preferences.keyboardShortcuts` changes.
    pub fn update(&mut self, preferences: &Value) {
        let overrides = preferences.get("keyboardShortcuts").cloned().unwrap_or(Value::Null);
        let key = overrides.to_string();
        if key == self.overrides_key && !self.bindings.is_empty() {
            return;
        }
        self.overrides_key = key;
        self.bindings = bindings(&overrides);
        self.reset_state();
    }

    fn reset_state(&mut self) {
        self.step = 0;
        self.candidates.clear();
    }

    pub fn reset(&mut self, cx: &mut Cx) {
        cx.stop_timer(self.timeout);
        self.reset_state();
    }

    /// Chord timeout (1500ms) handling.
    pub fn handle_timer(&mut self, event: &Event) {
        if self.timeout.is_event(event).is_some() {
            self.reset_state();
        }
    }

    fn enabled(binding: &Binding, focus_scope: &str, command_center: bool) -> bool {
        let when = j::object(&binding.data, "when");
        if when.get("editable") == Some(&Value::Bool(false)) && (focus_scope == "editable" || focus_scope == "message-input") {
            return false;
        }
        if when.get("terminal") == Some(&Value::Bool(false)) && focus_scope == "terminal" {
            return false;
        }
        if when.get("commandCenter") == Some(&Value::Bool(false)) && command_center {
            return false;
        }
        match when.get("focusScope") {
            None | Some(Value::Null) => true,
            Some(Value::String(scope)) => scope == focus_scope,
            Some(Value::Array(scopes)) => scopes.iter().any(|s| s.as_str() == Some(focus_scope)),
            _ => false,
        }
    }

    /// `PaseoShortcutRouter.handle`. Returns `Some` when the key was consumed.
    pub fn handle_key(
        &mut self,
        cx: &mut Cx,
        event: &KeyEvent,
        focus_scope: &str,
        command_center: bool,
    ) -> Option<Option<Dispatch>> {
        if matches!(event.key_code, KeyCode::Control | KeyCode::Alt | KeyCode::Shift | KeyCode::Logo | KeyCode::Capslock) {
            return None;
        }
        let initial = self.step == 0;
        let step = self.step;
        let pool = if initial { &self.bindings } else { &self.candidates };
        let matches: Vec<Binding> = pool
            .iter()
            .filter(|b| {
                b.chord.len() > step
                    && step_matches(&b.chord[step], event)
                    && !(event.is_repeat && b.data.get("repeat") == Some(&Value::Bool(false)) && b.chord.len() == step + 1)
                    && Self::enabled(b, focus_scope, command_center)
            })
            .cloned()
            .collect();
        let advancing: Vec<Binding> = matches.iter().filter(|b| b.chord.len() > step + 1).cloned().collect();
        let completed = matches.iter().find(|b| b.chord.len() == step + 1).cloned();
        if let Some(binding) = completed
            && (!initial || advancing.is_empty())
        {
            let payload = j::object(&binding.data, "payload");
            let mut args = Map::new();
            match j::text(payload, "type") {
                "index" => {
                    if let Some(digit) = digit(event.key_code) {
                        args.insert("index".into(), json!(digit));
                    }
                }
                "delta" => {
                    args.insert("delta".into(), payload.get("delta").cloned().unwrap_or(json!(1)));
                }
                _ => {}
            }
            let mut action = binding.action().to_string();
            if j::text(payload, "type") == "message-input" {
                action = format!("message-input.{}", j::text(payload, "kind"));
            }
            self.reset(cx);
            return Some(Some(Dispatch {
                action,
                args: Value::Object(args),
                pass_through: binding.data.get("preventDefault") == Some(&Value::Bool(false)),
            }));
        }
        if advancing.is_empty() {
            let was_chording = !initial;
            self.reset(cx);
            // A key that does not continue a pending chord only resets it.
            return if was_chording { Some(None) } else { None };
        }
        self.candidates = advancing;
        self.step += 1;
        cx.stop_timer(self.timeout);
        self.timeout = cx.start_timeout(1.5);
        Some(None)
    }
}

fn digit(code: KeyCode) -> Option<i64> {
    Some(match code {
        KeyCode::Key1 => 1,
        KeyCode::Key2 => 2,
        KeyCode::Key3 => 3,
        KeyCode::Key4 => 4,
        KeyCode::Key5 => 5,
        KeyCode::Key6 => 6,
        KeyCode::Key7 => 7,
        KeyCode::Key8 => 8,
        KeyCode::Key9 => 9,
        _ => return None,
    })
}

/// Binding `code`/`key` → Makepad key code (physical key, layout independent).
fn key_code_for(code: &str, key: &str) -> Option<KeyCode> {
    let name = if code.is_empty() { key } else { code };
    if let Some(letter) = name.strip_prefix("Key").filter(|l| l.len() == 1) {
        return letter_code(letter.chars().next()?);
    }
    if let Some(d) = name.strip_prefix("Digit").filter(|l| l.len() == 1) {
        return digit_code(d.chars().next()?);
    }
    if name.len() == 1 {
        let c = name.chars().next()?;
        if c.is_ascii_alphabetic() {
            return letter_code(c);
        }
        if c.is_ascii_digit() {
            return digit_code(c);
        }
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => KeyCode::ReturnKey,
        "escape" | "esc" => KeyCode::Escape,
        "space" | " " => KeyCode::Space,
        "backspace" => KeyCode::Backspace,
        "tab" => KeyCode::Tab,
        "delete" => KeyCode::Delete,
        "backquote" | "`" => KeyCode::Backtick,
        "backslash" | "\\" => KeyCode::Backslash,
        "bracketleft" | "[" => KeyCode::LBracket,
        "bracketright" | "]" => KeyCode::RBracket,
        "comma" | "," => KeyCode::Comma,
        "period" | "." => KeyCode::Period,
        "slash" | "/" => KeyCode::Slash,
        "semicolon" | ";" => KeyCode::Semicolon,
        "quote" | "'" => KeyCode::Quote,
        "minus" | "-" => KeyCode::Minus,
        "equal" | "=" => KeyCode::Equals,
        "insert" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "arrowleft" | "arrow left" | "left" => KeyCode::ArrowLeft,
        "arrowright" | "arrow right" | "right" => KeyCode::ArrowRight,
        "arrowup" | "arrow up" | "up" => KeyCode::ArrowUp,
        "arrowdown" | "arrow down" | "down" => KeyCode::ArrowDown,
        "f1" => KeyCode::F1,
        "f2" => KeyCode::F2,
        "f3" => KeyCode::F3,
        "f4" => KeyCode::F4,
        "f5" => KeyCode::F5,
        "f6" => KeyCode::F6,
        "f7" => KeyCode::F7,
        "f8" => KeyCode::F8,
        "f9" => KeyCode::F9,
        "f10" => KeyCode::F10,
        "f11" => KeyCode::F11,
        "f12" => KeyCode::F12,
        _ => return None,
    })
}

fn letter_code(c: char) -> Option<KeyCode> {
    Some(match c.to_ascii_lowercase() {
        'a' => KeyCode::KeyA,
        'b' => KeyCode::KeyB,
        'c' => KeyCode::KeyC,
        'd' => KeyCode::KeyD,
        'e' => KeyCode::KeyE,
        'f' => KeyCode::KeyF,
        'g' => KeyCode::KeyG,
        'h' => KeyCode::KeyH,
        'i' => KeyCode::KeyI,
        'j' => KeyCode::KeyJ,
        'k' => KeyCode::KeyK,
        'l' => KeyCode::KeyL,
        'm' => KeyCode::KeyM,
        'n' => KeyCode::KeyN,
        'o' => KeyCode::KeyO,
        'p' => KeyCode::KeyP,
        'q' => KeyCode::KeyQ,
        'r' => KeyCode::KeyR,
        's' => KeyCode::KeyS,
        't' => KeyCode::KeyT,
        'u' => KeyCode::KeyU,
        'v' => KeyCode::KeyV,
        'w' => KeyCode::KeyW,
        'x' => KeyCode::KeyX,
        'y' => KeyCode::KeyY,
        'z' => KeyCode::KeyZ,
        _ => return None,
    })
}

fn digit_code(c: char) -> Option<KeyCode> {
    Some(match c {
        '0' => KeyCode::Key0,
        '1' => KeyCode::Key1,
        '2' => KeyCode::Key2,
        '3' => KeyCode::Key3,
        '4' => KeyCode::Key4,
        '5' => KeyCode::Key5,
        '6' => KeyCode::Key6,
        '7' => KeyCode::Key7,
        '8' => KeyCode::Key8,
        '9' => KeyCode::Key9,
        _ => return None,
    })
}

/// `_matches`: modifiers exact (Mod = Cmd on Apple, Ctrl elsewhere), then the key.
fn step_matches(expected: &Value, event: &KeyEvent) -> bool {
    let flag = |name: &str| expected.get(name) == Some(&Value::Bool(true));
    let m = event.modifiers;
    let wants_mod = flag("mod");
    if m.control != (flag("ctrl") || (wants_mod && !IS_MAC))
        || m.logo != (flag("meta") || (wants_mod && IS_MAC))
        || m.alt != flag("alt")
        || m.shift != flag("shift")
    {
        return false;
    }
    let code = j::text(expected, "code");
    if code == "Digit" {
        return digit(event.key_code).is_some();
    }
    key_code_for(code, j::text(expected, "key")) == Some(event.key_code)
}

/// `formatShortcutStep` (Apple glyphs ⌃⌥⇧⌘ run into the key; elsewhere `Ctrl+Shift+F`).
pub fn format_step(step: &str) -> String {
    let mut parts: Vec<&str> = step.split('+').collect();
    let mut key = parts.pop().unwrap_or("").to_string();
    if key.is_empty() && parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
        key = "+".to_string();
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
    if IS_MAC {
        let symbols: String = ["ctrl", "alt", "shift", "mod", "meta"]
            .iter()
            .filter(|m| modifiers.iter().any(|x| x == *m))
            .map(|m| match *m {
                "ctrl" => "⌃",
                "alt" => "⌥",
                "shift" => "⇧",
                _ => "⌘",
            })
            .collect();
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

/// Display combo of a whole chord.
pub fn format_combo(combo: &str) -> String {
    combo.split_whitespace().map(format_step).collect::<Vec<_>>().join(" ")
}

/// The effective combo of the first binding with `help.id == help_id`.
pub fn label_for_help_id(preferences: &Value, help_id: &str) -> Option<String> {
    let overrides = preferences.get("keyboardShortcuts").cloned().unwrap_or(Value::Null);
    BASE.with(|base| {
        let binding = base.iter().find(|b| j::text(j::object(b, "help"), "id") == help_id)?;
        let combo = overrides
            .get(j::text(binding, "id"))
            .and_then(Value::as_str)
            .unwrap_or(j::text(binding, "combo"));
        (!combo.is_empty()).then(|| format_combo(combo))
    })
}

/// Shortcut help sections (`_shortcutSections`): (catalog section, locale key).
pub const SECTIONS: [(&str, &str); 5] = [
    ("general", "general"),
    ("workspaces", "workspaces"),
    ("tabs-panes", "tabsPanes"),
    ("layout", "layout"),
    ("agent-input", "agentInput"),
];

/// Rows of the shortcut help dialog: (section title key, [(label, effective combo)]).
pub fn help_rows(preferences: &Value, query: &str) -> Vec<(String, Vec<(String, String)>)> {
    let overrides = preferences.get("keyboardShortcuts").cloned().unwrap_or(Value::Null);
    let query = query.trim().to_lowercase();
    BASE.with(|base| {
        let mut sections = Vec::new();
        for (id, key) in SECTIONS {
            let title = crate::common::tr(&format!("settings.shortcuts.sections.{key}"));
            let mut seen = Vec::new();
            let mut rows = Vec::new();
            for binding in base {
                let help = j::object(binding, "help");
                if j::text(help, "section") != id {
                    continue;
                }
                let help_id = if j::text(help, "id").is_empty() { j::text(binding, "action") } else { j::text(help, "id") };
                if seen.contains(&help_id) {
                    continue;
                }
                seen.push(help_id);
                let label = j::text_or(help, "label", j::text(binding, "action"));
                if !query.is_empty() && !label.to_lowercase().contains(&query) && !title.to_lowercase().contains(&query) {
                    continue;
                }
                let combo = overrides
                    .get(j::text(binding, "id"))
                    .and_then(Value::as_str)
                    .unwrap_or(j::text(binding, "combo"))
                    .to_string();
                rows.push((label.to_string(), combo));
            }
            if !rows.is_empty() {
                sections.push((title, rows));
            }
        }
        sections
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_scope_survives_old_owner_blur_in_either_action_order() {
        for blur_first in [true, false] {
            let mut scope = FocusScope::default();
            scope.update(WidgetUid(1), Some("editable".into()));
            if blur_first {
                scope.update(WidgetUid(1), None);
            }
            scope.update(WidgetUid(2), Some("message-input".into()));
            if !blur_first {
                scope.update(WidgetUid(1), None);
            }
            assert_eq!(scope.name(), "message-input");
            scope.update(WidgetUid(2), None);
            assert_eq!(scope.name(), "workspace");
        }
    }

    fn key(code: KeyCode, control: bool, shift: bool) -> KeyEvent {
        KeyEvent {
            key_code: code,
            is_repeat: false,
            modifiers: KeyModifiers { shift, control, alt: false, logo: false },
            time: 0.0,
        }
    }

    #[test]
    fn parse_combo_reads_modifiers_and_chords() {
        let steps = parse_combo("Mod+Shift+K G").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["mod"], json!(true));
        assert_eq!(steps[0]["shift"], json!(true));
        assert_eq!(steps[0]["code"], json!("K"));
        assert_eq!(steps[1]["key"], json!("g"));
    }

    #[test]
    fn step_matching_requires_exact_modifiers() {
        let step = json!({"code": "KeyN", "key": "n", "ctrl": true});
        assert!(step_matches(&step, &key(KeyCode::KeyN, true, false)));
        assert!(!step_matches(&step, &key(KeyCode::KeyN, true, true)));
        assert!(!step_matches(&step, &key(KeyCode::KeyM, true, false)));
    }

    #[test]
    fn format_step_uses_platform_labels() {
        let formatted = format_step("Mod+Shift+K");
        if IS_MAC {
            assert_eq!(formatted, "⇧⌘K");
        } else {
            assert_eq!(formatted, "Ctrl+Shift+K");
        }
        assert!(format_step("Backspace").ends_with('⌫'));
    }
}
