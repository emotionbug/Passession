//! Port of `lib/ui/client_activity.dart`: `client_heartbeat` to every connected host every
//! 15s and on focus/visibility changes (input at most every 5s), desktop OS idle sampling
//! every 5s (not on Wayland: no fabricated idle value), the attention badge count and
//! attention notifications through the platform policy.

use std::time::{Duration, Instant, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use makepad_widgets::*;
use paseo_gui_core::CoreEvent;
use paseo_gui_platform::{AttentionView, PlatformAction, PlatformRequestId, PlatformValue};
use serde_json::{Value, json};

use crate::common::{PanelTarget, UiContext, json as j};
use crate::shell::hosts;

const HEARTBEAT: f64 = 15.0;
const IDLE_POLL: f64 = 5.0;

pub struct Activity {
    last_activity: SystemTime,
    visibility_changed: SystemTime,
    last_immediate: Option<Instant>,
    visible: bool,
    target: Option<PanelTarget>,
    heartbeat: Timer,
    idle_poll: Timer,
    idle_request: Option<PlatformRequestId>,
    idle_supported: bool,
    badge: Option<u32>,
    started: bool,
}

impl Default for Activity {
    fn default() -> Self {
        Self {
            last_activity: SystemTime::now(),
            visibility_changed: SystemTime::now(),
            last_immediate: None,
            visible: true,
            target: None,
            heartbeat: Timer::empty(),
            idle_poll: Timer::empty(),
            idle_request: None,
            idle_supported: !cfg!(any(target_os = "android", target_os = "ios"))
                && std::env::var("XDG_SESSION_TYPE").map_or(true, |v| v != "wayland"),
            badge: None,
            started: false,
        }
    }
}

fn iso(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Activity {
    fn start(&mut self, cx: &mut Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.heartbeat = cx.start_interval(HEARTBEAT);
        if self.idle_supported && crate::shell::platform::desktop_host() {
            self.idle_poll = cx.start_interval(IDLE_POLL);
        }
    }

    /// `updateView`: the focused tab and app visibility.
    pub fn update_view(&mut self, ui: &UiContext, target: Option<&PanelTarget>, visible: bool) {
        let same_target = self.target.as_ref().map(PanelTarget::key) == target.map(PanelTarget::key);
        if self.visible == visible && same_target {
            return;
        }
        if self.visible != visible {
            self.visibility_changed = SystemTime::now();
        }
        self.visible = visible;
        self.target = target.cloned();
        self.last_activity = SystemTime::now();
        self.last_immediate = Some(Instant::now());
        self.send_all(ui);
    }

    /// `recordInput` from pointer and key events.
    pub fn record_input(&mut self, ui: &UiContext) {
        self.last_activity = SystemTime::now();
        if self.last_immediate.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            self.last_immediate = Some(Instant::now());
            self.send_all(ui);
        }
    }

    fn send(&self, ui: &UiContext, server_id: &str) {
        let focused = self.visible && self.target.as_ref().is_some_and(|t| t.server_id == server_id);
        let kind = self.target.as_ref().map(|t| t.kind.as_str());
        let id = self.target.as_ref().and_then(|t| t.id.clone());
        ui.core.notify(
            server_id,
            json!({
                "type": "client_heartbeat",
                "deviceType": if cfg!(any(target_os = "android", target_os = "ios")) { "mobile" } else { "web" },
                "focusedAgentId": if focused && kind == Some("agent") { id.clone() } else { None },
                "focusedTerminalId": if focused && kind == Some("execution_log") { id } else { None },
                "lastActivityAt": iso(self.last_activity),
                "appVisible": self.visible,
                "appVisibilityChangedAt": iso(self.visibility_changed),
            }),
        );
    }

    fn send_all(&mut self, ui: &UiContext) {
        for host in hosts::hosts(&ui.state) {
            if host.connected() {
                self.send(ui, host.server_id());
            }
        }
        self.update_badge(ui);
    }

    fn update_badge(&mut self, ui: &UiContext) {
        if !crate::shell::platform::desktop_host() {
            return;
        }
        let agents = hosts::hosts(&ui.state).into_iter().flat_map(|host| host.agents().collect::<Vec<_>>());
        let count = paseo_gui_platform::notification_policy::attention_badge_count(agents);
        if self.badge == Some(count) {
            return;
        }
        if let Some(platform) = paseo_gui_platform::platform() {
            platform.set_badge_count(count);
            self.badge = Some(count);
        }
    }

    pub fn handle_event(&mut self, cx: &mut Cx, event: &Event, ui: &UiContext) {
        self.start(cx);
        if self.heartbeat.is_event(event).is_some() {
            self.send_all(ui);
        }
        if self.idle_poll.is_event(event).is_some() && self.idle_request.is_none() {
            self.idle_request = paseo_gui_platform::platform().map(|p| p.idle_milliseconds());
        }
        match event {
            Event::MouseDown(_) | Event::TouchUpdate(_) | Event::Scroll(_) | Event::KeyDown(_) => self.record_input(ui),
            Event::Actions(actions) => {
                for action in crate::shell::platform::actions(actions) {
                    if let PlatformAction::Completed { request, result } = action {
                        if Some(request) != self.idle_request {
                            continue;
                        }
                        self.idle_request = None;
                        match result {
                            Ok(PlatformValue::IdleMilliseconds(ms)) => {
                                let system = SystemTime::now() - Duration::from_millis(ms);
                                if system > self.last_activity {
                                    self.last_activity = system;
                                }
                            }
                            _ => {
                                cx.stop_timer(self.idle_poll);
                                self.idle_supported = false;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        for core_event in &ui.events {
            match core_event {
                CoreEvent::Connected { host_id, .. } => self.send(ui, host_id),
                CoreEvent::HostEvent { host_id, event } => {
                    let kind = j::text(event, "type");
                    if kind == "agent_attention_required" || kind == "terminal_attention_required" {
                        self.notify(host_id, event);
                    }
                }
                _ => {}
            }
        }
    }

    fn notify(&self, server_id: &str, event: &Value) {
        let view = AttentionView {
            visible: self.visible,
            server_id: self.target.as_ref().map(|t| t.server_id.clone()),
            kind: self.target.as_ref().map(|t| t.kind.clone()),
            id: self.target.as_ref().and_then(|t| t.id.clone()),
        };
        if let Some(request) = paseo_gui_platform::notification_policy::attention_notification(server_id, event, &view)
            && let Some(platform) = paseo_gui_platform::platform()
        {
            platform.notify(request);
        }
    }
}
