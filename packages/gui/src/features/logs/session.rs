//! Read-only observation of an existing daemon execution (Flutter
//! `features/logs/execution_log_session.dart`).
//!
//! The shared terminal [`Stream`] presents the output through the Ghostty emulator (the
//! original's xterm buffer): carriage returns, erasure, cursor movement and alternate screens
//! are interpreted before output is presented as selectable log lines, keeping every SGR
//! attribute the original renders. A log never writes back: no input, no resize, and the
//! emulator answers no terminal query, so status queries in process output cannot turn this
//! observer into an interactive client.

use serde_json::Value;

use crate::features::terminal::stream::{Stream, StreamText};
use crate::features::workspace::i18n::l;

pub type LogSession = Stream;

static TEXT: StreamText = StreamText {
    restore_failed: || l("Execution observation could not be restored").to_owned(),
    invalid_slot: || l("Invalid execution subscription slot").to_owned(),
    invalid_snapshot: || l("Invalid execution snapshot").to_owned(),
    restore_without_size: || l("Execution restore arrived without snapshot dimensions").to_owned(),
};

/// A new, detached observation of `terminal_id` keeping `scrollback` presented lines.
pub fn open(host_id: &str, terminal_id: &str, scrollback: usize) -> Result<LogSession, String> {
    Stream::new(host_id, terminal_id, scrollback, &TEXT)
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
        Some(session) if !session.title().is_empty() => session.title(),
        _ => l("Execution output"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_preference_clamps() {
        assert_eq!(scrollback_preference(&serde_json::json!({"executionLogLines": 5})), 100);
        assert_eq!(scrollback_preference(&serde_json::json!({})), 10_000);
    }

    #[test]
    fn display_title_prefers_target_then_osc_title() {
        let mut session = open("h", "t", 100).unwrap();
        assert_eq!(display_title("Build", Some(&session)), "Build");
        session.screen.write(b"\x1b]2;make\x07");
        session.publish();
        assert_eq!(display_title("", Some(&session)), "make");
    }
}
