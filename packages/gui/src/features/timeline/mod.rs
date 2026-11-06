//! Chat timeline (owner: ChatTimeline): the `ChatPanel` chat tab root — the
//! synchronized conversation replica (`session`, `store`), stream projection
//! and painters, native Markdown, permissions, tool details, chat find, the
//! outline, subagent tracks and the conversation actions. The composer is
//! `features::composer::ComposerPanel`, mounted inside the panel.
//!
//! The `ChatPanel` DSL item lives here so `crate::features::timeline::ChatPanel`
//! resolves in other modules' `live_design!`; [`ChatTimelineModules`] registers
//! the submodule DSL (kit, row, panel) before this file's body.

pub mod attention;
pub mod canvas;
pub mod chat_row;
pub mod chrome;
pub mod composer_bridge;
pub mod details;
pub mod find;
pub mod fold;
pub mod format;
pub mod highlight;
pub mod i18n;
pub mod images;
pub mod json;
pub mod layout;
pub mod links;
pub mod markdown;
pub mod outline;
pub mod paint;
pub mod panel;
pub mod permission;
pub mod reveal;
pub mod rows;
pub mod session;
pub mod store;
pub mod tools;
pub mod tool_sheet;
pub mod tracks;

pub use panel::{ChatPanel, ChatPanelRef, ChatPanelWidgetExt, ChatPanelWidgetRefExt, ChatTarget, conversation_actions};
pub use store::{handle_event, release_session, release_subagent};
pub use tool_sheet::{close_tool_call_sheet, draw_tool_call_sheets, handle_tool_call_sheets, show_tool_call_sheet, tool_call_sheet_settled};

use makepad_widgets::*;

live_design! {
    use link::theme::*;
    use crate::features::timeline::panel::ChatPanelRoot;

    ChatTimelineModules = {{ChatTimelineModules}} {}

    pub ChatPanel = <ChatPanelRoot> {}
}

/// Registration hook: `live_design!` calls `live_design_with` (and so
/// `live_register`) of every `{{Type}}` it names before registering this
/// file, which registers the submodules this file's DSL imports.
#[derive(Live, LiveHook)]
pub struct ChatTimelineModules {
    #[rust]
    _registered: bool,
}

impl LiveRegister for ChatTimelineModules {
    fn live_register(cx: &mut Cx) {
        canvas::live_design(cx);
        chat_row::live_design(cx);
        panel::live_design(cx);
    }
}
