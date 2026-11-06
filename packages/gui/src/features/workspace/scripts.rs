//! Workspace script controls shared by the setup panel and the shell's workspace header
//! scripts menu (Flutter `setup_panel.dart` `_scriptRow` and `shell.dart` `_scriptsMenu`):
//! start/stop, the read-only log of a script's terminal and its service URL.

use serde_json::{json, Value};

use crate::common::{PanelTarget, TabPlacement, UiEffect};

use super::i18n::{l, ta};
use super::rpc::{str_of, Pending};

/// `proxyUrl`, else `publicProxyUrl`, else `localProxyUrl`.
pub fn service_url(script: &Value) -> &str {
    for key in ["proxyUrl", "publicProxyUrl", "localProxyUrl"] {
        let value = str_of(script, key);
        if !value.is_empty() {
            return value;
        }
    }
    ""
}

pub fn is_running(script: &Value) -> bool {
    str_of(script, "lifecycle") == "running"
}

/// The setup panel's muted state hint: type · lifecycle · health · exit code.
pub fn script_hint(script: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for key in ["type", "lifecycle", "health"] {
        let value = str_of(script, key);
        if !value.is_empty() {
            parts.push(value.to_owned());
        }
    }
    if let Some(code) = script.get("exitCode").filter(|c| !c.is_null()) {
        let code = match code {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        parts.push(ta("workspace.scripts.states.exitCode", &[("code", &code)]));
    }
    parts.join(" · ")
}

/// Starts a stopped script or stops a running one (`workspace.script.start|stop`).
pub fn toggle<K>(pending: &mut Pending<K>, core: &paseo_gui_core::CoreHandle, host_id: &str, workspace_id: &str, script: &Value, key: K) {
    let verb = if is_running(script) { "stop" } else { "start" };
    pending.send(
        core,
        host_id,
        &format!("workspace.script.{verb}.request"),
        json!({ "workspaceId": workspace_id, "scriptName": str_of(script, "scriptName") }),
        key,
    );
}

/// Opens a service URL in the system browser. Only http(s) URLs are allowed.
pub fn open_service(effects: &mut Vec<UiEffect>, url: &str) -> Result<(), String> {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme.to_ascii_lowercase());
    match scheme.as_deref() {
        Some("http") | Some("https") if url.contains("://") => {
            effects.push(UiEffect::OpenExternal { url: url.to_owned() });
            Ok(())
        }
        _ => Err(l("Could not open service URL in the system browser.").to_owned()),
    }
}

/// The tab of a script's read-only execution log (Flutter `PanelTarget` kind
/// `execution_log`, id = terminal id, title = script name).
pub fn log_tab(host_id: &str, workspace_id: &str, cwd: &str, script: &Value) -> Option<UiEffect> {
    let terminal_id = str_of(script, "terminalId");
    if terminal_id.is_empty() {
        return None;
    }
    let target = PanelTarget::new("execution_log", host_id, workspace_id, cwd)
        .with_id(terminal_id)
        .with_title(str_of(script, "scriptName"));
    Some(UiEffect::OpenTab { target, placement: TabPlacement::Main })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_url_precedence_and_scheme_rule() {
        let script = json!({"publicProxyUrl": "https://a", "localProxyUrl": "http://b"});
        assert_eq!(service_url(&script), "https://a");
        let mut effects = Vec::new();
        assert!(open_service(&mut effects, "https://a").is_ok());
        assert!(open_service(&mut effects, "file:///etc/passwd").is_err());
        assert!(open_service(&mut effects, "javascript:alert(1)").is_err());
        assert_eq!(effects.len(), 1);
    }
}
