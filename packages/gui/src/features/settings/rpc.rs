//! Settings' request plumbing: continuations keyed by core request id,
//! desktop bridge token and timer, plus the per-dispatch environment.

use std::rc::Rc;

use makepad_widgets::*;
use serde_json::{Map, Value};

use super::host::{self, HostView};
use super::i18n::t;
use super::platform::Bridge;
use super::state::Settings;
use super::ui::field::FieldHost;
use crate::common::{Route, UiContext, UiEffect};
use paseo_gui_core::CoreError;

/// A failed settings request (Flutter `SettingsRequestError` / `DaemonException`).
#[derive(Clone, Debug)]
pub struct ReqError {
    pub code: String,
    pub message: String,
    pub outcome_unknown: bool,
}

impl ReqError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self { code: String::new(), message: message.into(), outcome_unknown: false }
    }

    pub fn from_core(error: &CoreError) -> Self {
        Self { code: error.code.clone(), message: error.message.clone(), outcome_unknown: error.outcome_unknown }
    }

    /// Flutter `_isReconnectFailure` / `isDaemonUpdateReconnectFailure`.
    pub fn is_reconnect(&self) -> bool {
        self.outcome_unknown
            || matches!(self.code.as_str(), "DAEMON_CONNECTION_LOST" | "DAEMON_REQUEST_TIMEOUT" | "DAEMON_NOT_CONNECTED")
    }

    /// Flutter `errorMessage`.
    pub fn text(&self) -> String {
        self.message.clone()
    }
}

/// Flutter `settingsRequest`: an `error` field or `success: false` fails.
pub fn check_response(result: Value) -> Result<Value, ReqError> {
    let failed = !result.get("error").map(Value::is_null).unwrap_or(true)
        || result.get("success").and_then(Value::as_bool) == Some(false);
    if !failed {
        return Ok(result);
    }
    let message = match result.get("error") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(map)) => map
            .get("message")
            .and_then(Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| serde_json::to_string(&result).unwrap_or_default()),
        _ => serde_json::to_string(&result).unwrap_or_default(),
    };
    let code = result.get("error").and_then(|e| e.get("code")).and_then(Value::as_str).unwrap_or("").to_string();
    Err(ReqError { code, message, outcome_unknown: false })
}

/// A UI handler: zones, menu items and field submits run one of these.
pub type Handler = Rc<dyn Fn(&mut Settings, &mut Env)>;
/// A request continuation.
pub type Then = Box<dyn FnOnce(&mut Settings, &mut Env, Result<Value, ReqError>)>;
/// A timer continuation.
pub type Later = Box<dyn FnOnce(&mut Settings, &mut Env)>;

pub fn handler(f: impl Fn(&mut Settings, &mut Env) + 'static) -> Handler {
    Rc::new(f)
}

/// Everything a handler may touch besides settings state.
pub struct Env<'a> {
    pub cx: &'a mut Cx,
    pub ctx: &'a mut UiContext,
    pub fields: &'a mut FieldHost,
    pub bridge: &'a mut Bridge,
}

impl Env<'_> {
    pub fn hosts(&self) -> Vec<HostView> {
        host::hosts(&self.ctx.state)
    }

    pub fn host(&self, server_id: &str) -> Option<HostView> {
        self.hosts().into_iter().find(|h| h.server_id == server_id)
    }

    pub fn preferences(&self) -> Value {
        host::preferences(&self.ctx.state)
    }

    pub fn toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.ctx.effects.push(UiEffect::Toast { message: message.into(), is_error });
    }

    /// Flutter `showError`.
    pub fn error(&mut self, error: &ReqError) {
        let message = error.text();
        self.toast(message, true);
    }

    pub fn open_url(&mut self, url: &str) {
        self.ctx.effects.push(UiEffect::OpenExternal { url: url.to_string() });
    }

    /// Clipboard text through the shell (platform `copy_text`), with an optional toast.
    pub fn copy(&mut self, text: &str, toast: Option<String>) {
        self.ctx.effects.push(UiEffect::CopyText { text: text.to_string(), toast });
    }

    pub fn navigate(&mut self, route: Route) {
        self.ctx.effects.push(UiEffect::Navigate(route));
    }
}

/// `{type, ...fields}` request message.
pub fn message(kind: &str, fields: Value) -> Value {
    let mut map = match fields {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert("type".into(), Value::String(kind.to_string()));
    Value::Object(map)
}

pub fn not_manageable() -> ReqError {
    ReqError::msg(t("settingsNative.noManagePermission"))
}
