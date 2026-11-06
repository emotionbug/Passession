//! The timeline's only link to `features::composer` (chat-composer-api.md).
//! A different final composer signature changes this file alone.

use crate::features::composer as composer;
use makepad_widgets::{Cx, Texture};
use serde_json::Value;

pub use composer::{AgentSnapshot, ComposerAction, ComposerCommand, ComposerKind, ComposerPanelWidgetRefExt, ComposerTarget};

/// stream.ts produceUserMessage "existing" (SentUserMessages.lookup).
pub fn sent_attachments(agent_id: &str, client_message_id: &str) -> Option<Value> {
    if agent_id.is_empty() || client_message_id.is_empty() {
        return None;
    }
    composer::sent_message_attachments(agent_id, client_message_id)
}

/// Optimistic prompts not yet echoed by the timeline (`_streamEntries`).
pub fn outgoing_rows(host_id: &str, workspace_id: &str, agent_id: &str, echoed: &dyn Fn(&str) -> bool) -> Vec<Value> {
    composer::outgoing_rows(host_id, workspace_id, agent_id, echoed)
}

pub fn outgoing_revision() -> u64 {
    composer::outgoing_revision()
}

pub fn has_pending_delivery(host_id: &str, workspace_id: &str, agent_id: &str) -> bool {
    composer::has_pending_delivery(host_id, workspace_id, agent_id)
}

pub fn restore_text_if_empty(cx: &mut Cx, host_id: &str, workspace_id: &str, agent_id: &str, text: &str) {
    composer::restore_text_if_empty(cx, host_id, workspace_id, agent_id, text);
}

/// Retained composer blob or legacy inline image → cached texture.
pub fn image_texture(cx: &mut Cx, image: &Value, thumbnail: bool) -> Option<Texture> {
    composer::image_texture(cx, image, thumbnail)
}


/// AttachmentPill.label: the attachment's title and secondary label.
pub fn attachment_label(attachment: &Value) -> (String, Option<String>) {
    let (label, secondary, _) = composer::attachments::describe(attachment);
    (label, secondary)
}

/// The catalog model `model_id` of `provider` (label, thinkingOptions) in the
/// panel's providers snapshot (AgentControlsModel.modelLabel source).
pub fn model_entry(providers: &[Value], provider: &str, model_id: &str) -> Option<Value> {
    if model_id.is_empty() {
        return None;
    }
    let entry = composer::agent_form::provider_entry(providers, provider)?;
    composer::controls_model::models_of(entry).into_iter().find(|m| m.get("id").and_then(Value::as_str) == Some(model_id))
}

/// The provider snapshot's icon SVG (`iconSvg`) for ProviderIcon.
pub fn provider_svg(providers: &[Value], provider: &str) -> Option<String> {
    composer::agent_form::provider_entry(providers, provider)?.get("iconSvg")?.as_str().map(str::to_owned)
}

/// providerEntries: the snapshot payload's entries (compact snapshots expanded).
pub fn provider_entries(payload: &Value) -> Vec<Value> {
    composer::agent_form::provider_entries(payload)
}

/// agent-panel.tsx findActiveCreateHandoff.
pub fn create_handoff_active(host_id: &str, agent_id: &str) -> bool {
    composer::create_handoff_active(host_id, agent_id)
}

pub fn clear_create_handoff(host_id: &str, agent_id: &str) {
    composer::clear_create_handoff(host_id, agent_id);
}

pub fn target(host_id: &str, workspace_id: &str, cwd: &str, agent_id: &str, subagent: bool, tab_key: &str, data: Value) -> ComposerTarget {
    ComposerTarget {
        kind: if subagent { ComposerKind::ProviderSubagent } else { ComposerKind::Agent },
        host_id: host_id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        cwd: cwd.to_owned(),
        id: agent_id.to_owned(),
        tab_key: tab_key.to_owned(),
        data,
    }
}

pub fn snapshot(agent: std::sync::Arc<Value>, running: bool, execution_authorized: bool, has_pending_permissions: bool) -> AgentSnapshot {
    AgentSnapshot { agent, running, execution_authorized, has_pending_permissions }
}
