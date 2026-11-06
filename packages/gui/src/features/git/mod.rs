//! Git review (Flutter `features/git/`): the `GitPanel` root of the Changes
//! (`changes_tree`), working diff (`working_diff`), commit (`commit_diff`) and
//! pull request (`pull_request`) tabs, plus the workspace header's
//! `GitActionsButton` and `OpenInButton`.
//!
//! Every git widget of a window shares one store (`store.rs`): per-checkout
//! status and PR status, the action runner, invalidations, comparisons and
//! review drafts. Diff summaries come from owned subscriptions; patch bodies
//! load lazily per file.

pub mod actions;
pub mod changes;
pub mod chrome;
pub mod controller;
pub mod diff_doc;
pub mod forge_url;
pub mod header;
pub mod host;
pub mod json;
pub mod model;
pub mod overlay;
pub mod paint;
pub mod panel;
pub mod policy;
pub mod pr_pane;
pub mod pr_view;
pub mod pull_request;
pub mod review;
pub mod store;
pub mod text;
pub mod wire;

pub use header::{
    GitActionsButton, GitActionsButtonRef, GitActionsButtonWidgetRefExt, OpenInButton, OpenInButtonRef,
    OpenInButtonWidgetRefExt,
};
pub use paint::GitPaint;
pub use panel::{GitPanel, GitPanelRef, GitPanelWidgetRefExt};

use makepad_widgets::*;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::features::git::paint::GitPaint;
    use crate::features::timeline::canvas::ChatKit;

    // The review comment field. The sheet paints the field box and its 16/12px
    // insets; colours and type size follow the runtime theme (set on open).
    GitTextInput = <TextInput> {
        width: Fill, height: Fill,
        margin: 0,
        padding: 0,
        empty_text: ""
        draw_bg: {
            fn pixel(self) -> vec4 {
                return vec4(0.0, 0.0, 0.0, 0.0);
            }
        }
        draw_text: {
            // Flutter `height: 1.4` over the family's natural 1.3 line.
            text_style: <PASEO_FONT_REGULAR> { font_size: 10.5, line_spacing: 1.077 }
            fn get_color(self) -> vec4 {
                return mix(self.color, self.color_empty, self.empty);
            }
        }
    }

    pub GitPanel = {{GitPanel}} {
        width: Fill, height: Fill,
        paint: <GitPaint> {}
        overlay_bg: {}
        input: <GitTextInput> {}
        chat_kit: <ChatKit> {}
    }

    pub GitActionsButton = {{GitActionsButton}} {
        width: Fit, height: 28,
        paint: <GitPaint> {}
    }

    pub OpenInButton = {{OpenInButton}} {
        width: Fit, height: 28,
        paint: <GitPaint> {}
        brand_icon: {
            fn get_color(self) -> vec4 {
                return self.color;
            }
        }
    }
}
