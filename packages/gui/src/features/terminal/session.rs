//! An interactive terminal's stream: the shared observation with terminal copy and the
//! original app's scrollback setting.

use serde_json::Value;

use super::stream::{Stream, StreamText};
use crate::features::workspace::i18n::t;

static TEXT: StreamText = StreamText {
    restore_failed: || t("workspace.terminal.restoreFailed"),
    invalid_slot: || t("workspace.terminal.unableToSubscribe"),
    invalid_snapshot: || t("workspace.terminal.invalidSnapshot"),
    restore_without_size: || t("workspace.terminal.invalidSnapshot"),
};

/// A new, detached stream of `terminal_id` keeping `scrollback` rows.
pub fn open(host_id: &str, terminal_id: &str, scrollback: usize) -> Result<Stream, String> {
    Stream::new(host_id, terminal_id, scrollback, &TEXT)
}

/// `settings.terminalScrollbackLines`: 0..1000000, default 10000.
pub fn scrollback_preference(preferences: &Value) -> usize {
    match preferences.get("terminalScrollbackLines").and_then(Value::as_f64) {
        Some(value) if value.is_finite() => value.clamp(0.0, 1_000_000.0) as usize,
        _ => 10_000,
    }
}

/// The header title: the program's OSC title, then the tab's name, then "Terminal".
pub fn display_title(target_title: &str, session: Option<&Stream>) -> String {
    match session.map(Stream::title).filter(|title| !title.is_empty()) {
        Some(title) => title.to_owned(),
        None if !target_title.is_empty() => target_title.to_owned(),
        None => t("workspace.tabs.fallback.terminal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_preference_clamps() {
        assert_eq!(scrollback_preference(&serde_json::json!({"terminalScrollbackLines": 2_000_000})), 1_000_000);
        assert_eq!(scrollback_preference(&serde_json::json!({"terminalScrollbackLines": 42_000})), 42_000);
        assert_eq!(scrollback_preference(&serde_json::json!({})), 10_000);
    }

    #[test]
    fn osc_title_wins_over_the_tab_name() {
        let mut stream = open("h", "t", 100).unwrap();
        assert_eq!(display_title("zsh", Some(&stream)), "zsh");
        stream.screen.write(b"\x1b]0;vim\x07");
        stream.publish();
        assert_eq!(display_title("zsh", Some(&stream)), "vim");
    }
}
