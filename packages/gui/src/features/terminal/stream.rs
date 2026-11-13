//! One `subscribe_terminal_request` observation of a daemon terminal (Flutter
//! `execution_log_session.dart`, original `terminal-stream-controller.ts`), shared by the
//! read-only execution logs and the interactive terminal.
//!
//! The observation is held only while the view is visible. The core restores it after a
//! reconnect; its response remaps the slot before frames arrive. Frames are routed by slot:
//! opcode-4 JSON snapshots carry the authoritative PTY size, opcode-5 restores keep the
//! last one, and output is written as it arrives (the owner coalesces publishing).

use paseo_gui_core::{CoreEvent, CoreHandle};
use serde_json::{json, Value};

use super::screen::Screen;
use super::snapshot::render_terminal_snapshot;
use crate::features::workspace::observe::{ObsEvent, Observation};
use crate::features::workspace::rpc::{int_of, payload, str_of};

/// Failure copy of one stream owner (logs say "execution", terminals "terminal").
pub struct StreamText {
    pub restore_failed: fn() -> String,
    pub invalid_slot: fn() -> String,
    pub invalid_snapshot: fn() -> String,
    pub restore_without_size: fn() -> String,
}

pub struct Stream {
    pub host_id: String,
    pub terminal_id: String,
    pub screen: Screen,
    pub error: Option<String>,
    pub attaching: bool,
    pub exited: bool,
    /// The process's exit status, when the daemon reports it (`terminal_stream_exit.exitCode`).
    pub exit_code: Option<i64>,
    visible: bool,
    slot: Option<u8>,
    subscription_id: Option<String>,
    observation: Option<Observation>,
    /// Output arrived since the last publish.
    pub output_pending: bool,
    /// A slot was acknowledged or a snapshot replaced the screen since the last
    /// [`Stream::take_attached`].
    attached: bool,
    text: &'static StreamText,
}

impl Stream {
    pub fn new(host_id: &str, terminal_id: &str, scrollback: usize, text: &'static StreamText) -> Result<Self, String> {
        Ok(Self {
            host_id: host_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            screen: Screen::new(scrollback).map_err(|error| error.to_string())?,
            error: None,
            attaching: false,
            exited: false,
            exit_code: None,
            visible: false,
            slot: None,
            subscription_id: None,
            observation: None,
            output_pending: false,
            attached: false,
            text,
        })
    }

    /// `ExecutionLogSession.connected`: frames are flowing.
    pub fn live(&self, host_connected: bool) -> bool {
        host_connected && self.slot.is_some() && !self.attaching && !self.exited && self.error.is_none()
    }

    pub fn line_count(&self) -> usize {
        self.screen.line_count()
    }

    pub fn cols(&self) -> usize {
        usize::from(self.screen.cols())
    }

    pub fn revision(&self) -> u64 {
        self.screen.revision()
    }

    pub fn title(&self) -> &str {
        self.screen.title()
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Whether the stream (re)attached or its screen was replaced since the last call.
    pub fn take_attached(&mut self) -> bool {
        std::mem::take(&mut self.attached)
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
        // No restore options: the default opcode-4 snapshot carries the authoritative PTY
        // dimensions and the input-mode preamble follows it.
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
        self.exit_code = None;
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

    /// Connection changes for this host (`_connectionChanged`). Returns true when the view
    /// should redraw.
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
            // Process observation messages and binary frames in the same pass. In
            // particular, the final output must be consumed before Exit releases its slot.
            if let Some(observation) = self.observation.as_mut() {
                for item in observation.take(std::slice::from_ref(event)) {
                    changed |= self.receive(item);
                }
            }
            if self.observation.is_some()
                && let CoreEvent::HostEvent { host_id, event } = event
                && *host_id == self.host_id
                && str_of(event, "type") == "terminal.frame"
            {
                changed |= self.frame(payload(event));
            }
        }
        changed
    }

    pub(crate) fn receive(&mut self, event: ObsEvent) -> bool {
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
                let message = if error.is_empty() { (self.text.restore_failed)() } else { error };
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
                self.exit_code = body.get("exitCode").and_then(Value::as_i64);
                self.attaching = false;
                self.slot = None;
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
            self.fail((self.text.invalid_slot)());
            return;
        }
        self.slot = Some(slot as u8);
        self.subscription_id = body.get("subscriptionId").and_then(Value::as_str).map(str::to_owned);
        self.attaching = false;
        self.error = None;
        self.attached = true;
    }

    /// One core `terminal.frame` (`{slot, opcode, text | snapshot}`).
    fn frame(&mut self, frame: &Value) -> bool {
        let Some(slot) = self.slot else { return false };
        if frame.get("slot").and_then(Value::as_u64) != Some(u64::from(slot)) {
            return false;
        }
        match str_of(frame, "opcode") {
            // Output: UTF-8 already decoded incrementally per slot by the core.
            "output" => {
                self.screen.write(str_of(frame, "text").as_bytes());
                self.output_pending = true;
                false
            }
            // Snapshot: a JSON cell grid with authoritative dimensions.
            "snapshot" => match frame.get("snapshot") {
                Some(state) if state.is_object() => {
                    let size = |key| u16::try_from(int_of(state, key).clamp(0, i64::from(u16::MAX))).ok().filter(|v| *v > 0);
                    let size = size("rows").zip(size("cols"));
                    let text = render_terminal_snapshot(state);
                    let title = str_of(state, "title").to_owned();
                    self.replace(&text, size, Some(&title));
                    true
                }
                _ => {
                    self.fail((self.text.invalid_snapshot)());
                    true
                }
            },
            // Restore: ANSI without size fields keeps the last authoritative size.
            "restore" => {
                if !self.screen.has_dimensions() {
                    self.fail((self.text.restore_without_size)());
                    return true;
                }
                let text = str_of(frame, "text").to_owned();
                self.replace(&text, None, None);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn replace(&mut self, text: &str, size: Option<(u16, u16)>, title: Option<&str>) {
        self.output_pending = false;
        match self.screen.replace(text, size, title) {
            Ok(()) => self.attached = true,
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Applies coalesced output (`_publishOutput`).
    pub fn publish(&mut self) {
        self.output_pending = false;
        self.screen.publish();
    }

    /// Reads presented rows `start..start+count` (scrollback first, then the screen).
    pub fn read(&mut self, start: usize, count: usize) -> Vec<&paseo_terminal::Line> {
        self.screen.read(start, count)
    }

    /// `renderedText`: every counted row as logical lines.
    pub fn rendered_text(&self) -> String {
        self.screen.rendered_text(self.screen.line_count())
    }

    /// Input is binary: legacy mouse coordinates are arbitrary bytes, not UTF-8.
    /// The core resolves the slot from this subscription, fencing reconnects.
    pub fn send_input(&self, core: &CoreHandle, data: &[u8]) {
        if data.is_empty() || self.exited || !self.visible {
            return;
        }
        let Some(subscription) = self.subscription_id.as_deref() else { return };
        core.terminal_input(&self.host_id, subscription, data);
    }

    /// Requests the PTY size: `claim` takes size ownership (the focused pane), `update`
    /// only applies while this client already owns it.
    pub fn send_resize(&self, core: &CoreHandle, rows: u16, cols: u16, claim: bool) {
        if self.exited || rows == 0 || cols == 0 {
            return;
        }
        core.notify(
            &self.host_id,
            json!({
                "type": "terminal_input",
                "terminalId": self.terminal_id,
                "message": { "type": "resize", "rows": rows, "cols": cols, "intent": if claim { "claim" } else { "update" } },
            }),
        );
    }

    #[cfg(test)]
    pub(crate) fn set_visible_for_test(&mut self) {
        self.visible = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    static TEXT: StreamText = StreamText {
        restore_failed: || "restore failed".into(),
        invalid_slot: || "invalid slot".into(),
        invalid_snapshot: || "invalid snapshot".into(),
        restore_without_size: || "no size".into(),
    };

    fn stream() -> Stream {
        let mut stream = Stream::new("h", "t", 100, &TEXT).unwrap();
        stream.set_visible_for_test();
        stream
    }

    #[test]
    fn exit_with_error_is_a_failure_not_an_exit() {
        let mut stream = stream();
        stream.receive(ObsEvent::Event(json!({"type": "terminal_stream_exit", "payload": {"terminalId": "t", "error": "gone"}})));
        assert!(!stream.exited);
        assert_eq!(stream.error.as_deref(), Some("gone"));
        stream.receive(ObsEvent::Event(json!({"type": "terminal_stream_exit", "payload": {"terminalId": "t", "exitCode": 2}})));
        assert!(stream.exited);
        assert_eq!(stream.exit_code, Some(2));
        // Older daemons send no code.
        let mut stream = self::stream();
        stream.receive(ObsEvent::Event(json!({"type": "terminal_stream_exit", "payload": {"terminalId": "t"}})));
        assert!(stream.exited);
        assert_eq!(stream.exit_code, None);
    }

    #[test]
    fn snapshots_replace_the_screen_at_their_size() {
        let mut stream = stream();
        stream.receive(ObsEvent::Event(json!({"type": "subscribe_terminal_response", "payload": {"terminalId": "t", "slot": 3}})));
        assert!(stream.take_attached());
        let snapshot = json!({
            "slot": 3, "opcode": "snapshot",
            "snapshot": {"rows": 5, "cols": 30, "grid": [[{"char": "h"}, {"char": "i"}]], "scrollback": [], "cursor": {"row": 0, "col": 2}, "title": "shell"}
        });
        assert!(stream.frame(&snapshot));
        assert!(stream.take_attached());
        assert_eq!((stream.screen.rows(), stream.screen.cols()), (5, 30));
        assert_eq!(stream.title(), "shell");
        assert!(!stream.frame(&json!({"slot": 3, "opcode": "output", "text": "!"})));
        assert!(stream.output_pending);
        stream.publish();
        assert_eq!(stream.read(0, 1)[0].text, "hi!");
        assert!(!stream.frame(&json!({"slot": 4, "opcode": "output", "text": "x"})));
    }

    #[test]
    fn restore_needs_snapshot_dimensions() {
        let mut stream = stream();
        stream.receive(ObsEvent::Event(json!({"type": "subscribe_terminal_response", "payload": {"terminalId": "t", "slot": 0}})));
        stream.frame(&json!({"slot": 0, "opcode": "restore", "text": "x"}));
        assert_eq!(stream.error.as_deref(), Some("no size"));
    }

    #[test]
    fn batched_final_output_precedes_stream_exit() {
        use paseo_gui_core::{CoreConfig, GuiCore};
        use std::sync::Arc;
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("paseo-terminal-order-{}-{unique}", std::process::id()));
        let core = GuiCore::start(
            CoreConfig { state_dir: dir.join("state"), daemon_home: Some(dir.join("daemon")), default_endpoint: None },
            Arc::new(|| {}),
        ).unwrap();
        let handle = core.handle();
        let mut stream = stream();
        stream.attach(&handle, true);
        let request_id = stream.observation.as_ref().unwrap().id.clone();
        let events = [
            CoreEvent::Response {
                host_id: "h".into(), request_id: request_id.clone(),
                result: Ok(json!({"terminalId":"t","slot":3,"subscriptionId":"sub"})),
            },
            CoreEvent::HostEvent {
                host_id: "h".into(),
                event: json!({"type":"terminal.frame","payload":{"slot":3,"opcode":"output","text":"last output"}}),
            },
            CoreEvent::HostEvent {
                host_id: "h".into(),
                event: json!({"type":"terminal_stream_exit","observations":[request_id],"payload":{"terminalId":"t","exitCode":7,"subscriptionId":"sub"}}),
            },
        ];
        stream.pump(&handle, &events);
        assert_eq!(stream.rendered_text(), "last output");
        assert!(stream.exited);
        assert_eq!(stream.exit_code, Some(7));
        drop(stream);
        drop(handle);
        drop(core);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
