//! Composer and delivery (owner: ComposerDelivery). Message admission, durable drafts and
//! retained attachments, exact-ID receipts and queues, agent creation and recovery, agent
//! configuration, suggestions and the native composer surfaces. See
//! `chat-composer-api.md` for the ChatTimeline boundary.

pub mod agent_form;
pub mod attachments;
pub mod autocomplete;
pub mod blobs;
pub mod controller;
pub mod controls_model;
pub mod creation;
pub mod delivery;
pub mod drafts;
pub mod forge;
pub mod i18n;
pub mod json;
pub mod model_picks;
pub mod ops;
pub mod service;
pub mod state_keys;
pub mod ui;

pub use controller::{AgentSnapshot, ComposerCommand, ComposerKind, ComposerTarget};
pub use ui::creation_panel::{
    CreationPanel, CreationPanelRef, CreationPanelWidgetExt, CreationPanelWidgetRefExt,
};
pub use ui::panel::{
    ComposerAction, ComposerPanel, ComposerPanelRef, ComposerPanelWidgetExt,
    ComposerPanelWidgetRefExt,
};

use crate::common::UiContext;
use json::*;
use makepad_widgets::{Cx, Texture};
use serde_json::{Value, json};

pub fn live_design(cx: &mut Cx) {
    ui::live_design(cx);
}

/// Shell hook: once per core event batch, before widget dispatch (Flutter `ChatDelivery`
/// attached per client). Delivery, receipts, creation recovery and draft persistence run
/// here whether or not any composer is visible.
pub fn handle_core_events(cx: &mut Cx, ui: &mut UiContext) {
    service::with(|svc| svc.handle_core_events(cx, ui));
}

/// Shell hook on `Event::Signal`: background blob work (image store/decode, file stats).
/// Owning composer panels see the service revision; other texture consumers poll
/// `texture_revision` after drawing a placeholder.
pub fn poll_background(cx: &mut Cx) {
    service::with(|svc| svc.poll_background(cx));
}

/// Changes whenever a decoded image texture is installed.
pub fn texture_revision() -> u64 {
    service::with(|svc| svc.texture_revision())
}

/// Count of `image_texture` misses so far; a consumer that reads it before and after its
/// draw knows it drew a placeholder and should redraw when `texture_revision` changes.
pub fn texture_misses() -> u64 {
    service::with(|svc| svc.texture_misses())
}

/// Shell hook before quitting/closing a window: persist the drafts now and report a send or
/// upload in flight, or a persistence failure, that must keep the window open.
pub fn flush_drafts(ui: &UiContext) -> Result<(), String> {
    service::with(|svc| svc.flush_for_quit(ui))
}

/// Append local images to this exact conversation, independently of any mounted
/// composer. This edits the draft only; it never enters the delivery queue.
pub(crate) fn append_shared_images(
    host_id: &str,
    workspace_id: &str,
    agent_id: &str,
    images: Vec<Value>,
) -> bool {
    service::with(|svc| {
        if !svc.drafts.is_loaded() {
            return false;
        }
        let key = drafts::draft_key(host_id, workspace_id, agent_id);
        svc.drafts.append_images(&key, images);
        svc.after_mutation();
        true
    })
}

/// Pending and sending outgoing prompts of this agent not yet echoed by the timeline.
pub fn outgoing_rows(
    host_id: &str,
    workspace_id: &str,
    agent_id: &str,
    echoed: &dyn Fn(&str) -> bool,
) -> Vec<Value> {
    let key = drafts::draft_key(host_id, workspace_id, agent_id);
    service::with(|svc| {
        let Some(draft) = svc.drafts.peek(&key).cloned() else {
            return Vec::new();
        };
        let mut outgoing = Vec::new();
        if non_empty_obj(get(&draft, "pending")) {
            outgoing.push(get(&draft, "pending").clone());
        }
        outgoing.extend(
            maps(get(&draft, "queue"))
                .into_iter()
                .filter(|m| s(get(m, "state")) == "sending"),
        );
        let mut seen: Vec<String> = Vec::new();
        let mut rows = Vec::new();
        for message in outgoing {
            let id = owned(get(&message, "messageId"));
            if id.is_empty() || echoed(&id) || seen.contains(&id) {
                continue;
            }
            seen.push(id.clone());
            let timestamp = s_or(get(&message, "createdAt"), "").to_owned();
            let timestamp = if timestamp.is_empty() {
                ops::now_rfc3339()
            } else {
                timestamp
            };
            rows.push(json!({
                "clientMessageId": id,
                "pending": true,
                "timestamp": timestamp,
                "item": {
                    "type": "user_message",
                    "text": owned(get(&message, "text")),
                    "images": list(get(&message, "images")),
                    "attachments": list(get(&message, "attachments")),
                    "clientMessageId": id,
                },
            }));
        }
        rows
    })
}

/// Changes whenever any draft's pending/queue changes (timeline row-model cache key).
pub fn outgoing_revision() -> u64 {
    service::with(|svc| svc.drafts.outgoing_revision())
}

/// The running turn footer's delivery note (`draft.pending`, or a sending/unacknowledged
/// queue entry).
pub fn has_pending_delivery(host_id: &str, workspace_id: &str, agent_id: &str) -> bool {
    let key = drafts::draft_key(host_id, workspace_id, agent_id);
    service::with(|svc| {
        svc.drafts.peek(&key).is_some_and(|draft| {
            non_empty_obj(get(draft, "pending"))
                || maps(get(draft, "queue"))
                    .iter()
                    .any(|q| matches!(s(get(q, "state")), "sending" | "unacknowledged"))
        })
    })
}

/// composer-restore.tsx restoreComposerTextIfEmpty (after a conversation rewind).
pub fn restore_text_if_empty(
    cx: &mut Cx,
    host_id: &str,
    workspace_id: &str,
    agent_id: &str,
    text: &str,
) {
    let key = drafts::draft_key(host_id, workspace_id, agent_id);
    let restored = service::with(|svc| {
        if !s(&svc.drafts.field(&key, "text")).is_empty() {
            return false;
        }
        svc.drafts.update(&key, json!({"text": text}));
        svc.after_mutation();
        true
    });
    if restored {
        // The revision bump reaches the owning ComposerPanel in its `handle_event` sync;
        // guarantee it an event so it re-syncs and redraws itself.
        cx.new_next_frame();
    }
}

/// SentUserMessages.lookup.
pub fn sent_message_attachments(
    host: &str,
    agent_id: &str,
    client_message_id: &str,
) -> Option<Value> {
    service::with(|svc| svc.sent_lookup(host, agent_id, client_message_id))
}

/// A stable cached texture for a composer image reference (`{blobId}` or legacy inline
/// `{data}`); None while it decodes off the GUI thread.
pub fn image_texture(_cx: &mut Cx, image: &Value, thumbnail: bool) -> Option<Texture> {
    let identity = blobs::ImageContent::from_value(image).map(blobs::ImageContent::identity);
    service::with(|svc| {
        let texture = svc
            .blobs
            .texture(image, identity.as_ref(), thumbnail)
            .ok()
            .flatten()
            .map(|(texture, _)| texture);
        if texture.is_none() {
            svc.note_texture_miss();
        }
        texture
    })
}

pub fn create_handoff_active(host_id: &str, agent_id: &str) -> bool {
    service::with(|svc| svc.handoff_active(host_id, agent_id))
}

pub fn clear_create_handoff(host_id: &str, agent_id: &str) {
    service::with(|svc| svc.clear_handoff(host_id, agent_id));
}

/// Files "attach to chat" (Flutter `ChatDrafts.addAttachments`): the workspace's selected
/// composer takes them; `intent_id` makes a repeated intent idempotent.
pub fn add_workspace_attachments(
    host_id: &str,
    workspace_id: &str,
    attachments: Vec<Value>,
    intent_id: Option<String>,
) {
    service::with(|svc| {
        svc.drafts
            .add_attachments(host_id, workspace_id, attachments, intent_id.as_deref());
        svc.after_mutation();
    });
}
