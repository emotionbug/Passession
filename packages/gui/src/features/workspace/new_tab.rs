//! New session / new workspace / add project (Flutter `new_tab_panel.dart`, original
//! `new-workspace-screen.tsx` + `NewWorkspaceForm`): project, host, base ref and launch
//! fields, the composer card (prompt, attachments, model controls), the add-project
//! directory options, and creation with a persisted immutable intent whose receipt is
//! recovered instead of creating a duplicate.
//!
//! Agent form resolution, attachment helpers, file uploads and forge auto-attach come from
//! the composer module (`crate::features::composer::{agent_form, attachments, ops, forge,
//! controls_model}`); the remote folder picker from the files module.

use std::collections::HashMap;

use base64::Engine;
use makepad_widgets::*;
use super::kit::{WaModalWidgetExt};
use serde_json::{json, Map, Value};

use super::dialog::WaDialogWidgetRefExt;
use super::host::{self, Connection};
use super::i18n::{l, lf, t, ta};
use super::identity::project_view_key;
use super::kit::{
    set_optional, sync_input, TriggerLead, WaButtonWidgetExt, WaButtonWidgetRefExt, WaIconWidgetRefExt, WaInputWidgetExt, WaInputWidgetRefExt,
    WaRepeatWidgetRefExt, WaTriggerWidgetRefExt,
};
use super::new_tab_model::{self as model, BranchItem};
use super::observe::{ObsEvent, Observation};
use super::rpc::{array_of, new_id, object_of, payload, require_success, str_of, str_or, Pending, RpcError};
use super::select::{Leading, SelectOption, SelectRequest, WaSelectWidgetExt, WaSelectWidgetRefExt};
use crate::common::{PanelTarget, TabPlacement, UiContext, UiEffect};
use crate::common::file_drop::FileDropZone;
use crate::common::overlay::{self, MenuItem};
use crate::features::composer::forge::ForgeAutoAttach;
use crate::features::composer::{agent_form, attachments, controls_model, forge, ops};
use crate::features::files::picker::{RemoteFolderPickerAction, RemoteFolderPickerWidgetExt};
use crate::common::paint::{Font, Painter, TextSpec, rect};
use crate::common::widgets::{PSegmentedWidgetRefExt, PSwitchWidgetRefExt, Segment, SurfaceWidgetRefExt};
use crate::common::clock;
use crate::common::provider_icon::ProviderIconWidgetRefExt;
use crate::features::composer::ui::draw::DrawComposerImage;
use crate::theme;
use paseo_gui_platform::{platform, PickFilesOptions, PlatformAction, PlatformValue};

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::common::paint::PaseoPainter;
    use crate::common::file_drop::FileDropZone;
    use crate::common::provider_icon::ProviderIcon;
    use crate::features::composer::ui::draw::DrawComposerImage;
    use crate::features::workspace::kit::*;
    use crate::features::workspace::select::*;
    use crate::features::workspace::dialog::*;
    use crate::features::files::picker::RemoteFolderPicker;

    pub WaAttachmentPill = {{WaAttachmentPill}} {
        width: Fit, height: Fit
        margin: { right: 8., bottom: 8. }
        painter: <PaseoPainter> {}
        disc: <PaseoPainter> {}
        draw_image: <DrawComposerImage> {}
    }
    WaOptionField = <WaField> {
        hint = <View> { width: Fill, height: Fit, visible: false, text = <WaSmall> {} }
    }
    // `_input(suffix: _fieldAction(...))`: an inline ghost xs search action inside the field.
    WaSearchInput = <View> {
        width: Fill, height: Fit, flow: Overlay, align: { x: 1.0, y: 0.5 }
        input_box = <WaInput> { trailing_space: 32. }
        action = <View> {
            width: Fit, height: Fit, margin: { right: 4. }
            search = <WaButtonGhost> { size: Xs, icon: "search" }
        }
    }
    // Original switch rows: base label growing beside the 34x20 switch.
    WaSwitchRow = <View> {
        width: Fill, height: Fit, flow: Right, spacing: 12., align: { y: 0.5 }
        label = <WaText> {}
    }

    pub WaNewTabView = {{WaNewTabView}} {
        width: Fill, height: Fill, flow: Overlay
        file_drop: <FileDropZone> {}
        wa_root = <Surface> {
            width: Fill, height: Fill, flow: Overlay
            role: Surface0
            scroll = <ScrollYView> {
                width: Fill, height: Fill, flow: Down
                align: { x: 0.5, y: 0.5 }
                padding: { top: 0., bottom: 60. }
                column = <View> {
                    width: 820., height: Fit, flow: Down
                    pending = <View> {
                        width: Fill, height: Fit, flow: Down, visible: false, padding: { left: 16., right: 16., bottom: 16. }
                        card = <WaCard> {
                            row = <WaRow> {}
                            buttons = <View> {
                                width: Fill, height: Fit, flow: RightWrap, spacing: 8., padding: { left: 16., right: 16., bottom: 16. }
                                recover = <WaButtonPrimary> {}
                                open_workspace = <WaButtonOutline> {}
                                open_session = <WaButtonOutline> {}
                                new_attempt = <WaButtonGhost> {}
                            }
                        }
                    }
                    notice = <View> { width: Fill, height: Fit, visible: false, padding: { left: 16., right: 16., bottom: 12. }, text = <WaMuted> {} }
                    heading = <View> { width: Fill, height: Fit, padding: { left: 24., right: 16., bottom: 32. }, text = <WaText> { size: 20. } }
                    fields = <View> {
                        width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                        padding: { left: 16., right: 16., bottom: 16. }
                        project_field = <WaTrigger> {}
                        host_field = <WaTrigger> {}
                        isolation_field = <WaTrigger> {}
                        branch_field = <WaTrigger> {}
                        spacer = <View> { width: Fill, height: 1. }
                        launch_field = <WaTrigger> {}
                    }
                    // New worktree isolation: the branch the worktree creates.
                    worktree = <View> {
                        width: Fill, height: Fit, visible: false, padding: { left: 16., right: 16., bottom: 16. }
                        worktree_branch = <WaOptionField> { worktree_branch_input = <WaInput> {} }
                    }
                    composer = <View> {
                        width: Fill, height: Fit, padding: { left: 16., right: 16., bottom: 16. }
                        // Original `inputWrapper`: surface1, 1px borderAccent, 16px radius.
                        card = <Surface> {
                            width: Fill, height: Fit, flow: Down, spacing: 12.
                            padding: { bottom: 12. }
                            role: Surface1, border_role: BorderAccent, radius: 16.
                            tray = <View> {
                                width: Fill, height: Fit, flow: RightWrap, visible: false
                                padding: { left: 8., right: 16., top: 8. }
                                images = <WaRepeat> { width: Fit, flow: Right, template: <WaAttachmentPill> {} }
                                files = <WaRepeat> { width: Fit, flow: RightWrap, template: <WaAttachmentPill> {} }
                                uploading = <WaAttachmentPill> { visible: false }
                            }
                            prompt_box = <View> {
                                width: Fill, height: Fit, flow: Down, padding: { left: 16., right: 16., top: 16. }
                                prompt = <WaBareInput> {
                                    faint_hint: true, content: true
                                    input = {
                                        padding: 0.
                                        draw_text: { text_style: { line_spacing: 1.4 } }
                                    }
                                }
                                title_preview = <View> { width: Fill, height: Fit, visible: false, text = <WaMuted> { content: true } }
                            }
                            toolbar = <View> {
                                width: Fill, height: Fit, flow: Right, spacing: 0., align: { y: 1.0 }
                                padding: { left: 10., right: 10. }
                                // `Expanded(agent controls or SizedBox.shrink)`: submit stays at the end.
                                agent_slot = <View> {
                                width: Fill, height: Fit
                                agent_controls = <View> {
                                    width: Fill, height: Fit, flow: RightWrap, spacing: 4., align: { y: 0.5 }
                                    attach = <WaRoundButton> { icon: "plus" }
                                    model_trigger = <Surface> {
                                        width: Fit, height: 28., flow: Right, spacing: 4., align: { y: 0.5 }
                                        padding: { left: 8., right: 8. }
                                        role: Transparent, hover_role: Surface2, radius: 14., interactive: true
                                        provider_box = <View> { width: Fit, height: Fit, provider = <ProviderIcon> { width: 16., height: 16. } }
                                        label = <WaText> { width: Fit, role: Muted }
                                        thinking_inline = <View> { width: Fit, height: Fit, visible: false, text = <WaText> { width: Fit, role: Muted } }
                                        caret = <WaIcon> { size: 14., icon: "chevron-down" }
                                    }
                                    thinking = <WaToolbarButton> { icon: "brain" }
                                    mode = <WaToolbarButton> { icon: "bot" }
                                    features = <WaRepeat> { width: Fit, flow: Right, spacing: 4., template: <WaToolbarButton> {} }
                                }
                                }
                                submit = <WaSubmitButton> { icon: "corner-down-left" }
                            }
                        }
                    }
                    // A Terminal launch opens a blank shell: no composer, one action.
                    terminal_launch = <View> {
                        width: Fill, height: Fit, visible: false, padding: { left: 16., right: 16., bottom: 16. }
                        open_terminal = <WaButtonPrimary> { width: Fill, size: Lg, icon: "square-terminal" }
                    }
                    error = <View> { width: Fill, height: Fit, visible: false, padding: { left: 16., right: 16., bottom: 16. }, text = <WaDangerText> {} }
                    bottom = <View> {
                        width: Fill, height: Fit, flow: RightWrap, spacing: 8., padding: { left: 16., right: 16. }
                        import_session = <WaButtonGhost> { icon: "import" }
                        options_toggle = <WaButtonGhost> { icon: "sliders-horizontal", trailing: "chevron-down" }
                    }
                    options = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 16., visible: false
                        padding: { left: 16., right: 16., top: 16. }
                        existing_row = <WaSwitchRow> { existing = <WaToggle> {} }
                        source_field = <WaField> { source = <WaFieldButton> {} }
                        directory_field = <WaField> {
                            row = <View> {
                                width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                                path = <WaInput> {}
                                browse = <WaButtonOutline> { size: Md, icon: "folder-open" }
                            }
                        }
                        ref_field = <WaField> { row = <WaSearchInput> {} }
                        branch_name_field = <WaOptionField> { branch_input = <WaInput> {} }
                        pr_field = <WaField> { pr_input = <WaInput> { input = { is_numeric_only: true } } }
                        forge_field = <WaOptionField> { forge_input = <WaInput> {} }
                        project_path_field = <WaOptionField> { project_path_input = <WaInput> {} }
                        repo_field = <WaField> { row = <WaSearchInput> {} }
                        protocol_field = <WaField> { protocol = <PSegmented> {} }
                        dirname_field = <WaField> { dirname_input = <WaInput> {} }
                        unknown = <View> { width: Fill, height: Fit, visible: false, text = <WaMuted> {} }
                        prepare_row = <View> {
                            width: Fill, height: Fit, flow: RightWrap, spacing: 8.
                            prepare = <WaButtonPrimary> { icon: "folder-plus" }
                            inspect_prepared = <WaButtonGhost> {}
                        }
                        title_field = <WaField> { title_input = <WaInput> {} }
                        agent_row = <WaSwitchRow> { with_agent = <WaToggle> {} }
                        profile_field = <WaField> { profile = <WaFieldButton> {} }
                        add_project_row = <View> { width: Fill, height: Fit, add_project = <WaButtonOutline> { icon: "folder-open" } }
                    }
                }
            }
            folder_modal = <WaModal> {
                content: {
                    sheet = <WaSheet> {
                        // About 640×560 on wide windows (the modal clamps it to the window);
                        // `open_browser` makes it a full-height sheet on compact layouts.
                        width: 640.
                        body = <View> { width: Fill, height: 495., picker = <RemoteFolderPicker> {} }
                    }
                }
            }
            select = <WaSelect> {}
            dialog = <WaDialog> {}
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WaAttachmentPill

/// What an attachment pill shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PillBody {
    #[default]
    Empty,
    /// A 48px cover thumbnail of an inline `{data, mimeType}` image (composer image cache).
    Image(Value),
    /// `AttachmentLabel`: glyph, one-line title over the small muted subtitle.
    Label { icon: &'static str, title: String, subtitle: String },
    /// An uploading file: spinner, file name and type label.
    Pending { title: String, subtitle: String },
}

#[derive(Clone, Debug, DefaultNone)]
pub enum WaAttachmentPillAction {
    Remove,
    None,
}

/// Original `components/attachment-pill.tsx`: 48px bodies in a 6px-radius borderAccent frame;
/// removable pills carry a 24px surface2 close disc overhanging the top-left corner by 8px
/// (always on phones and compact layouts, else while the pill or disc is hovered).
#[derive(Live, LiveHook, Widget)]
pub struct WaAttachmentPill {
    #[redraw]
    #[live]
    painter: Painter,
    /// The close disc's own capture instance.
    #[live]
    disc: Painter,
    #[live]
    draw_image: DrawComposerImage,
    #[walk]
    walk: Walk,
    #[visible]
    #[live(true)]
    visible: bool,
    #[rust]
    body: PillBody,
    #[rust]
    removable: bool,
    #[rust]
    enabled: bool,
    #[area]
    #[rust]
    area: Area,
    #[rust]
    disc_area: Area,
    #[rust]
    body_hovered: bool,
    #[rust]
    disc_hovered: bool,
}

const PILL_OVERHANG: f64 = 8.0;

impl Widget for WaAttachmentPill {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if !self.visible {
            return;
        }
        match event.hits(cx, self.area) {
            Hit::FingerHoverIn(_) => {
                self.body_hovered = true;
                self.redraw(cx);
            }
            Hit::FingerHoverOut(_) => {
                self.body_hovered = false;
                self.redraw(cx);
            }
            _ => {}
        }
        if !self.removable {
            return;
        }
        match event.hits(cx, self.disc_area) {
            Hit::FingerHoverIn(_) => {
                cx.set_cursor(MouseCursor::Hand);
                self.disc_hovered = true;
                self.redraw(cx);
            }
            Hit::FingerHoverOut(_) => {
                self.disc_hovered = false;
                self.redraw(cx);
            }
            Hit::FingerUp(fe) if fe.is_over && fe.device.is_primary_hit() && self.enabled && self.disc_shown(cx) && (!fe.device.is_touch() || fe.was_tap()) => {
                cx.widget_action(self.widget_uid(), &scope.path, WaAttachmentPillAction::Remove);
            }
            _ => {}
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        if !self.visible || self.body == PillBody::Empty {
            return DrawStep::done();
        }
        let p = theme::current_palette();
        let t = theme::current_typography();
        let title_spec = TextSpec::new(Font::Regular, t.ui_size(14.0), p.foreground);
        let sub_spec = TextSpec::new(Font::Regular, t.ui_size(12.0), p.muted);
        let body_w = match &self.body {
            PillBody::Image(_) => 48.0,
            PillBody::Label { title, subtitle, .. } | PillBody::Pending { title, subtitle } => {
                let text = self.painter.measure(cx, title_spec, title).x.max(self.painter.measure(cx, sub_spec, subtitle).x);
                (12.0 + 18.0 + 8.0 + text + 12.0).min(260.0)
            }
            PillBody::Empty => 0.0,
        };
        let inset = if self.removable { PILL_OVERHANG } else { 0.0 };
        let walk = Walk { width: Size::Fixed(body_w + 2.0 + inset), height: Size::Fixed(50.0 + inset), ..walk };
        let outer = cx.walk_turtle(walk);
        let frame = rect(outer.pos.x + inset, outer.pos.y + inset, body_w + 2.0, 50.0);
        self.area = self.painter.capture_box(cx, frame, p.surface1, p.border_accent, 1.0, 6.0);
        let body = rect(frame.pos.x + 1.0, frame.pos.y + 1.0, body_w, 48.0);
        match self.body.clone() {
            PillBody::Image(image) => {
                if let Some(texture) = crate::features::composer::image_texture(cx, &image, true) {
                    let size = match texture.get_format(cx) {
                        TextureFormat::VecBGRAu8_32 { width, height, .. } => (*width, *height),
                        _ => (1, 1),
                    };
                    self.draw_image.paint(cx, body, &texture, size, 5.0, false);
                }
            }
            PillBody::Label { icon, title, subtitle } => {
                self.painter.icon(cx, rect(body.pos.x + 12.0, body.pos.y + 17.0, 18.0, 14.0), icon, 14.0, p.muted);
                self.paint_texts(cx, body, title_spec, sub_spec, &title, &subtitle);
            }
            PillBody::Pending { title, subtitle } => {
                self.painter.ring(cx, rect(body.pos.x + 12.0, body.pos.y + 15.0, 18.0, 18.0), p.muted, 2.0, 0.3, clock::phase(900));
                clock::register(cx, self.area);
                self.paint_texts(cx, body, title_spec, sub_spec, &title, &subtitle);
            }
            PillBody::Empty => {}
        }
        if self.removable {
            let shown = self.disc_shown(cx);
            let disc = rect(outer.pos.x, outer.pos.y, 24.0, 24.0);
            let (fill, border, glyph) = if shown { (p.surface2, p.border, p.muted) } else { (Vec4::default(), Vec4::default(), Vec4::default()) };
            self.disc_area = self.disc.capture_box(cx, disc, fill, border, 1.0, 12.0);
            if shown {
                self.disc.icon(cx, disc, "x", 12.0, glyph);
            }
        }
        DrawStep::done()
    }
}

impl WaAttachmentPill {
    fn disc_shown(&self, cx: &Cx) -> bool {
        crate::common::is_compact_layout(cx) || super::host::is_mobile() || self.body_hovered || self.disc_hovered
    }

    fn paint_texts(&mut self, cx: &mut Cx2d, body: Rect, title_spec: TextSpec, sub_spec: TextSpec, title: &str, subtitle: &str) {
        let x = body.pos.x + 12.0 + 18.0 + 8.0;
        let w = (body.pos.x + body.size.x - 12.0 - x).max(0.0);
        let title_h = self.painter.line_height(cx, title_spec);
        let sub_h = self.painter.line_height(cx, sub_spec);
        let top = body.pos.y + (body.size.y - title_h - sub_h) * 0.5;
        self.painter.text_in(cx, rect(x, top, w, title_h), title_spec, title, 0.0);
        self.painter.text_in(cx, rect(x, top + title_h, w, sub_h), sub_spec, subtitle, 0.0);
    }
}

impl WaAttachmentPillRef {
    pub fn set(&self, cx: &mut Cx, body: PillBody, removable: bool, enabled: bool) {
        if let Some(mut pill) = self.borrow_mut()
            && (pill.body != body || pill.removable != removable || pill.enabled != enabled)
        {
            pill.body = body;
            pill.removable = removable;
            pill.enabled = enabled;
            pill.redraw(cx);
        }
    }

    pub fn removed(&self, actions: &Actions) -> bool {
        self.borrow().is_some_and(|pill| matches!(actions.find_widget_action(pill.widget_uid()).cast_ref(), WaAttachmentPillAction::Remove))
    }
}

/// What a new-tab target carries (shell `WorkspaceTarget::NewTab` + `ui.target.data`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewTabTarget {
    pub host_id: String,
    pub workspace_id: String,
    pub cwd: String,
    pub tab_key: String,
    pub source: Option<String>,
    pub data: Value,
}

/// A form text field's model string.
type TextField = fn(&mut WaNewTabView) -> &mut String;

#[derive(Clone, Debug)]
enum Req {
    Profiles,
    DefaultRef(String),
    Providers(u64),
    Features(String),
    AddProject,
    Prepare,
    SearchRepos,
    Branches,
    Validate,
    CreateFeatures,
    Submit,
    ForgeSearch { lookup: u64, index: usize, cwd: String },
    AttachmentForge((String, String)),
    ForgePicker,
    Images,
    Upload,
    /// Dropped files were inspected off the GUI thread.
    Dropped,
    /// `create_terminal_request` of a Terminal launch.
    CreateTerminal { workspace_id: String, cwd: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlatformReq {
    PickImages,
    PickFiles,
    Paste,
}

/// The form values frozen when Create is pressed.
#[derive(Clone, Debug)]
struct CreateDraft {
    cwd: String,
    with_agent: bool,
    has_input: bool,
    effective: Value,
    local_features: Value,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaNewTabView {
    #[deref]
    view: View,
    #[rust]
    target: NewTabTarget,
    #[rust]
    initialized: bool,
    #[rust]
    path: String,
    #[rust]
    title: String,
    #[rust]
    prompt: String,
    #[rust]
    branch: String,
    #[rust]
    ref_name: String,
    #[rust]
    pr: String,
    #[rust]
    forge_name: String,
    #[rust]
    project_path: String,
    #[rust]
    repo: String,
    #[rust]
    directory_name: String,
    #[rust]
    source: String,
    #[rust]
    clone_protocol: String,
    #[rust]
    profile_id: Option<String>,
    #[rust]
    catalog_cwd: String,
    #[rust]
    with_agent: bool,
    /// Launch choice (`LaunchControl`): Terminal opens a daemon shell instead of a chat.
    #[rust]
    launch_terminal: bool,
    /// A workspace a Terminal launch already created whose terminal failed: a retry only
    /// creates the terminal (`ensureWorkspace` reuses it). (workspace id, directory)
    #[rust]
    terminal_workspace: Option<(String, String)>,
    #[rust]
    existing: bool,
    #[rust]
    busy: bool,
    #[rust]
    preparation_unknown: bool,
    #[rust]
    finishing: bool,
    #[rust]
    opened: bool,
    #[rust]
    project_added: bool,
    #[rust]
    options_open: bool,
    #[rust]
    config: Value,
    #[rust]
    initial_config: Value,
    #[rust]
    config_touched: bool,
    #[rust]
    providers: Vec<Value>,
    #[rust]
    providers_loading: bool,
    #[rust]
    provider_generation: u64,
    #[rust]
    provider_scope: Option<String>,
    #[rust]
    profiles: Vec<Value>,
    #[rust]
    profiles_error: Option<String>,
    #[rust]
    features: Vec<Value>,
    #[rust]
    features_key: String,
    #[rust]
    default_ref: Option<BranchItem>,
    #[rust]
    default_ref_cwd: Option<String>,
    #[rust]
    ref_label: Option<String>,
    #[rust]
    remote_url: Option<String>,
    #[rust]
    intent: Option<Value>,
    #[rust]
    snapshot: Option<Value>,
    #[rust]
    error: Option<String>,
    #[rust]
    observation: Option<Observation>,
    #[rust]
    replay_if_missing: bool,
    #[rust]
    create: Option<CreateDraft>,
    #[rust]
    images: Vec<Value>,
    #[rust]
    attachments: Vec<Value>,
    #[rust]
    attachment_menu: Option<u64>,
    #[rust]
    attachment_forge: String,
    #[rust]
    attachment_forge_scope: Option<(String, String)>,
    #[rust]
    upload_name: Option<String>,
    /// new-workspace-screen.tsx `FileDropZone`: the whole screen takes dropped files for
    /// the composer card.
    #[live]
    file_drop: FileDropZone,
    /// A drop is being inspected (`Req::Dropped`).
    #[rust]
    drop_pending: bool,
    #[rust]
    forge_auto: ForgeAutoAttach,
    #[rust]
    forge_timer: Timer,
    #[rust]
    forge_items: Vec<Value>,
    #[rust]
    branch_items: Vec<BranchItem>,
    #[rust]
    repositories: Vec<Value>,
    #[rust]
    pending: Pending<Req>,
    #[rust]
    platform_requests: HashMap<u64, PlatformReq>,
    #[rust]
    directory_timer: Timer,
    #[rust]
    last_status: Option<Connection>,
    #[rust]
    connected: bool,
    #[rust]
    legacy_forge: bool,
    #[rust]
    compact: bool,
    #[rust]
    column_width: f64,
    /// The vertical alignment last applied to the form's scroll view.
    #[rust]
    scroll_align: Option<f64>,
    #[rust]
    focus_prompt: bool,
    #[rust]
    synced: bool,
}

fn project_root(project: &Value) -> &str {
    host::project_root(project)
}

/// `BLANK_TERMINAL_PROFILE_ID`: the persisted `launchTarget.profileId` of the default shell.
const BLANK_TERMINAL_PROFILE_ID: &str = "blank";

impl WaNewTabView {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    pub fn set_new_tab(&mut self, cx: &mut Cx, target: NewTabTarget) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.initialized = false;
        self.view.redraw(cx);
    }

    /// `initState` + `didChangeDependencies`.
    fn initialize(&mut self, ctx: &mut UiContext) {
        self.initialized = true;
        let data = self.target.data.clone();
        self.path = if self.target.cwd.is_empty() { "~".into() } else { self.target.cwd.clone() };
        self.catalog_cwd = self.path.clone();
        self.existing = !self.target.workspace_id.is_empty();
        self.source = str_or(&data, "source", self.target.source.as_deref().unwrap_or("directory")).to_owned();
        self.clone_protocol = "https".into();
        let add_project = data.get("addProject") == Some(&Value::Bool(true));
        self.with_agent = !add_project;
        self.options_open = add_project;
        let draft = object_of(&data, "creationDraft");
        self.title = str_of(draft, "title").to_owned();
        self.prompt = str_of(draft, "prompt").to_owned();
        self.config = object_of(draft, "config").clone();
        self.initial_config = self.config.clone();
        // `resolveLaunchTarget`: a carried draft's choice, else the remembered one; only the blank
        // terminal exists here (no terminal profiles), anything else is Chat.
        self.terminal_workspace = None;
        self.launch_terminal = !add_project
            && match str_of(draft, "launch") {
                "" => {
                    let target = object_of(&Self::form_preferences(ctx), "launchTarget").clone();
                    str_of(&target, "kind") == "terminal" && str_of(&target, "profileId") == BLANK_TERMINAL_PROFILE_ID
                }
                launch => launch == "terminal",
            };
        self.synced = false;
        self.focus_prompt = !self.compact;
        let saved = object_of(host::preferences(&ctx.state), "workspaceCreationIntents");
        let intent = object_of(saved, &self.target.tab_key);
        if intent.as_object().is_some_and(|m| !m.is_empty()) {
            if str_of(intent, "launch") == "terminal" {
                self.launch_terminal = true;
            }
            self.intent = Some(intent.clone());
        }
        self.connected = self.host(ctx).is_some_and(|h| h.connected());
        self.last_status = self.host(ctx).map(|h| h.status());
        self.legacy_forge = self.host(ctx).is_some_and(|h| attachments::uses_legacy_forge(h.server_info()));
        if self.connected {
            self.pending.send(&ctx.core, &self.target.host_id, "get_daemon_config_request", json!({}), Req::Profiles);
        }
        self.load_default_ref(ctx);
        self.load_providers(ctx);
        if self.intent.is_some() {
            self.recover(ctx);
        }
    }

    fn host<'a>(&self, ctx: &'a UiContext) -> Option<host::Host<'a>> {
        host::host(&ctx.state, &self.target.host_id)
    }

    fn add_project(&self) -> bool {
        self.target.data.get("addProject") == Some(&Value::Bool(true))
    }

    /// The submission creates a new worktree (branch off, existing branch or pull request).
    fn worktree(&self) -> bool {
        !self.existing && matches!(self.source.as_str(), "branch-off" | "checkout" | "pr")
    }

    /// Terminal is the launch choice (never for Add project, which has no launch field).
    fn terminal(&self) -> bool {
        self.launch_terminal && !self.add_project()
    }

    /// `updateFormPreferences({ launchTarget })` on submit, applied to the preferences about to
    /// be remembered so a later write in the same submit keeps it.
    fn with_launch(&self, mut preferences: Value) -> Value {
        if self.add_project() {
            return preferences;
        }
        let target = if self.terminal() { json!({ "kind": "terminal", "profileId": BLANK_TERMINAL_PROFILE_ID }) } else { json!({ "kind": "chat" }) };
        if let Some(map) = preferences.as_object_mut() {
            map.insert("launchTarget".into(), target);
        }
        preferences
    }

    fn load_default_ref(&mut self, ctx: &UiContext) {
        let cwd = self.path.trim().to_owned();
        if !self.connected || cwd.is_empty() || self.default_ref_cwd.as_deref() == Some(cwd.as_str()) {
            return;
        }
        self.default_ref_cwd = Some(cwd.clone());
        self.default_ref = None;
        self.pending.send(&ctx.core, &self.target.host_id, "checkout_status_request", json!({ "cwd": cwd }), Req::DefaultRef(cwd));
    }

    fn load_providers(&mut self, ctx: &UiContext) {
        self.provider_generation += 1;
        self.provider_scope = None;
        if !self.connected {
            return;
        }
        let cwd = self.catalog_cwd.trim();
        let fields = if cwd.is_empty() { json!({}) } else { json!({ "cwd": cwd }) };
        self.providers_loading = true;
        self.pending.send(&ctx.core, &self.target.host_id, "get_providers_snapshot_request", fields, Req::Providers(self.provider_generation));
    }

    fn form_preferences(ctx: &UiContext) -> Value {
        agent_form::parse_form_preferences(host::preferences(&ctx.state).get(agent_form::FORM_PREFERENCES_KEY).unwrap_or(&Value::Null))
    }

    fn remember(ctx: &mut UiContext, next: Value) {
        host::set_preference(&mut ctx.effects, agent_form::FORM_PREFERENCES_KEY, next);
    }

    /// Offered provider features for the current selection (`use-draft-agent-features`).
    fn load_features(&mut self, ctx: &UiContext) {
        if !self.with_agent || self.config.get("provider").and_then(Value::as_str).unwrap_or("").is_empty() {
            self.features.clear();
            self.features_key.clear();
            return;
        }
        let mut effective = agent_form::effective_submit_config(&self.config, &self.providers);
        if let Some(map) = effective.as_object_mut() {
            map.remove("featureValues");
            map.insert("cwd".into(), self.catalog_cwd.clone().into());
        }
        let key = effective.to_string();
        if key == self.features_key {
            return;
        }
        self.features_key = key.clone();
        self.pending.send(&ctx.core, &self.target.host_id, "list_provider_features_request", json!({ "draftConfig": effective }), Req::Features(key));
    }

    /// `_changeConfig` (`setProviderAndModelFromUser` & co).
    fn change_config(&mut self, ctx: &mut UiContext, value: Value) {
        let preferences = Self::form_preferences(ctx);
        let provider = str_of(&value, "provider").to_owned();
        let mut next = value.clone();
        if !provider.is_empty() && (Some(provider.as_str()) != self.config.get("provider").and_then(Value::as_str) || value.get("model") != self.config.get("model")) {
            next = agent_form::apply_user_model_choice(&self.config, &provider, str_of(&value, "model"), &preferences, &self.providers);
        }
        Self::remember(ctx, agent_form::remember_user_selection(&preferences, &self.config, &next));
        self.config = next;
        self.config_touched = true;
        self.profile_id = None;
        self.load_features(ctx);
    }

    fn set_directory(&mut self, cx: &mut Cx, ctx: &UiContext, directory: &str) {
        self.path = directory.to_owned();
        self.catalog_cwd = directory.to_owned();
        self.terminal_workspace = None;
        self.ref_name.clear();
        self.ref_label = None;
        sync_input(cx, &self.view.wa_input(id!(path)), directory);
        sync_input(cx, &self.view.wa_input(id!(ref_field.row.input_box)), &self.ref_name);
        self.load_default_ref(ctx);
        self.load_providers(ctx);
        self.sync_forge(cx, ctx);
    }

    fn forge_input(&self) -> forge::ForgeInput {
        forge::ForgeInput {
            text: self.prompt.clone(),
            remote_url: self.remote_url.clone(),
            attachments: self.attachments.clone(),
            connected: self.connected,
            server_id: self.target.host_id.clone(),
            cwd: self.path.trim().to_owned(),
        }
    }

    fn apply_forge(&mut self, cx: &mut Cx, ctx: &UiContext, effects: forge::ForgeEffects) {
        if effects.cancel_timer || effects.start_timer {
            cx.stop_timer(self.forge_timer);
            self.forge_timer = Timer::default();
        }
        if effects.start_timer {
            self.forge_timer = cx.start_timeout(0.3);
        }
        for search in effects.searches {
            let (message, response) = forge::forge_search_request(self.legacy_forge, &search.cwd, &search.query);
            let id = ctx.core.request(&self.target.host_id, message, response);
            self.pending.track(id, "forge.search", Req::ForgeSearch { lookup: search.lookup, index: search.index, cwd: search.cwd });
        }
        let mut attached = false;
        for item in effects.attach {
            let key = attachments::forge_item_key(&item);
            if self.attachments.iter().any(|a| forge::forge_attachment_key(a).as_deref() == Some(key.as_str())) {
                continue;
            }
            self.attachments.push(attachments::forge_attachment(&item, self.legacy_forge));
            attached = true;
        }
        if attached {
            self.sync_forge(cx, ctx);
            self.view.redraw(cx);
        }
    }

    fn sync_forge(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let effects = self.forge_auto.update(self.forge_input());
        self.apply_forge(cx, ctx, effects);
    }

    fn forge_resolving(&self) -> bool {
        !self.forge_timer.is_empty() || self.pending.any(|req| matches!(req, Req::ForgeSearch { .. }))
    }

    /// `_persistIntent`: the exact immutable intent, saved before submission.
    fn persist_intent(&self, ctx: &mut UiContext, intent: Option<&Value>) {
        let mut values = object_of(host::preferences(&ctx.state), "workspaceCreationIntents").as_object().cloned().unwrap_or_default();
        match intent {
            Some(intent) => values.insert(self.target.tab_key.clone(), intent.clone()),
            None => values.remove(&self.target.tab_key),
        };
        host::set_preference(&mut ctx.effects, "workspaceCreationIntents", Value::Object(values));
    }

    fn fail(&mut self, ctx: &mut UiContext, message: String, toast: bool) {
        if toast {
            ctx.effects.push(UiEffect::Toast { message: message.clone(), is_error: true });
        }
        self.error = Some(message);
        self.busy = false;
        self.create = None;
    }

    /// `_create`: the synchronous checks, then branch validation and offered features.
    fn start_create(&mut self, ctx: &mut UiContext) {
        if self.busy {
            return;
        }
        self.error = None;
        let cwd = self.path.trim().to_owned();
        if cwd.is_empty() {
            return self.fail(ctx, l("Choose a remote directory.").into(), true);
        }
        let terminal = self.terminal();
        let launch = self.with_launch(Self::form_preferences(ctx));
        Self::remember(ctx, launch);
        if terminal {
            // `runCreateTerminalWorkspace`: an existing workspace (or the one an earlier attempt
            // created) only needs its terminal.
            let workspace = if self.existing {
                let directory = self
                    .host(ctx)
                    .and_then(|h| h.workspace(&self.target.workspace_id).map(|w| str_of(w, "workspaceDirectory").to_owned()))
                    .filter(|dir| !dir.is_empty())
                    .unwrap_or_else(|| cwd.clone());
                Some((self.target.workspace_id.clone(), directory))
            } else {
                self.terminal_workspace.clone()
            };
            if let Some((workspace_id, directory)) = workspace {
                return self.create_terminal(ctx, workspace_id, directory);
            }
        }
        // `isEmptyWorkspaceSubmission`: no prompt and no attachments creates the workspace alone.
        // A Terminal launch creates it without an agent or a naming context.
        let has_input = !terminal && (!self.prompt.trim().is_empty() || !self.images.is_empty() || !self.attachments.is_empty());
        let with_agent = !terminal && self.with_agent && (self.existing || has_input);
        if with_agent && self.catalog_cwd != cwd {
            forget_timer(&mut self.directory_timer);
            self.catalog_cwd = cwd;
            self.load_providers(ctx);
            return self.fail(ctx, l("The directory changed. Review the refreshed directory-scoped provider configuration before creating.").into(), true);
        }
        if with_agent && str_of(&self.config, "provider").is_empty() {
            return self.fail(ctx, t("newWorkspace.errors.selectModel"), true);
        }
        if !self.existing && matches!(self.source.as_str(), "clone" | "new-directory") {
            return self.fail(ctx, l("Prepare the directory before creating a session.").into(), true);
        }
        if !self.existing && self.source == "checkout" && self.ref_name.trim().is_empty() {
            return self.fail(ctx, l("Choose the branch to check out.").into(), true);
        }
        if !self.existing && self.source == "pr" && !model::valid_pr_number(&self.pr) {
            return self.fail(ctx, l("Enter a positive pull/merge request number.").into(), true);
        }
        // new-workspace-screen.tsx createWorkspaceChatAgent: the effective model, mode and
        // thinking option; the selection is remembered on submit.
        let mut effective = agent_form::effective_submit_config(&self.config, &self.providers);
        let local_features = effective.as_object_mut().and_then(|m| m.remove("featureValues")).unwrap_or(Value::Null);
        self.busy = true;
        self.create = Some(CreateDraft { cwd: cwd.clone(), with_agent, has_input, effective, local_features });
        if !self.existing && matches!(self.source.as_str(), "branch-off" | "checkout") && !self.ref_name.trim().is_empty() {
            self.pending.send(
                &ctx.core,
                &self.target.host_id,
                "validate_branch_request",
                json!({ "cwd": cwd, "branchName": self.ref_name.trim() }),
                Req::Validate,
            );
            return;
        }
        self.create_features(ctx);
    }

    fn create_features(&mut self, ctx: &mut UiContext) {
        let Some(draft) = self.create.clone() else { return };
        if !draft.with_agent {
            return self.create_submit(ctx, Value::Null);
        }
        let preferences = Self::form_preferences(ctx);
        Self::remember(ctx, self.with_launch(agent_form::remember_submitted_selection(&preferences, &draft.effective, &self.providers)));
        let mut config = draft.effective.clone();
        config["cwd"] = draft.cwd.clone().into();
        self.pending.send(&ctx.core, &self.target.host_id, "list_provider_features_request", json!({ "draftConfig": config }), Req::CreateFeatures);
    }

    fn create_submit(&mut self, ctx: &mut UiContext, offered: Value) {
        let Some(draft) = self.create.take() else { return };
        let mut effective = draft.effective.clone();
        if draft.with_agent {
            let offered: Vec<Value> = offered.as_array().cloned().unwrap_or_default();
            let remembered = agent_form::remembered_feature_values(&Self::form_preferences(ctx), str_of(&effective, "provider"));
            let values = agent_form::resolve_feature_values(&offered, &remembered, &draft.local_features);
            if values.as_object().is_some_and(|m| !m.is_empty()) {
                effective["featureValues"] = values;
            }
        }
        let mut config = effective.as_object().cloned().unwrap_or_default();
        config.insert("cwd".into(), draft.cwd.clone().into());
        if !self.title.trim().is_empty() {
            config.insert("title".into(), self.title.trim().into());
        }
        let terminal = self.terminal();
        let prompt = self.prompt.trim().to_owned();
        let mut agent = Map::new();
        agent.insert("config".into(), Value::Object(config));
        if !prompt.is_empty() {
            agent.insert("initialPrompt".into(), prompt.clone().into());
        }
        if draft.has_input {
            agent.insert("clientMessageId".into(), new_id().into());
        }
        if !self.images.is_empty() {
            agent.insert(
                "images".into(),
                Value::Array(self.images.iter().map(|i| json!({ "data": i["data"], "mimeType": i["mimeType"] })).collect()),
            );
        }
        let wire: Vec<Value> = if terminal { Vec::new() } else { self.attachments.iter().map(attachments::wire_attachment).collect() };
        if !wire.is_empty() {
            agent.insert("attachments".into(), Value::Array(wire.clone()));
        }
        let kind = if self.existing { "agent" } else { "workspace" };
        let mut request = Map::new();
        request.insert("idempotencyKey".into(), new_id().into());
        request.insert("subscribe".into(), false.into());
        if self.existing {
            request.extend(agent);
            request.insert("workspaceId".into(), self.target.workspace_id.clone().into());
        } else {
            let default_ref = self.default_ref.as_ref().map(|d| d.ref_name.clone());
            let source = model::workspace_source(&model::SourceFields {
                source: &self.source,
                cwd: &draft.cwd,
                ref_name: &self.ref_name,
                default_ref: default_ref.as_deref(),
                branch: &self.branch,
                pr: &self.pr,
                forge: &self.forge_name,
                project_path: &self.project_path,
            });
            request.insert("source".into(), source);
            if draft.with_agent {
                request.insert("agent".into(), Value::Object(agent));
            }
            if !self.title.trim().is_empty() {
                request.insert("title".into(), self.title.trim().into());
            }
            // buildFirstAgentContext: the trimmed prompt and attachments name the workspace.
            if draft.has_input {
                let mut context = Map::new();
                if !prompt.is_empty() {
                    context.insert("prompt".into(), prompt.into());
                }
                context.insert("attachments".into(), Value::Array(wire));
                request.insert("firstAgentContext".into(), Value::Object(context));
            }
        }
        let mut intent = json!({ "kind": kind, "request": Value::Object(request) });
        if terminal {
            // Local only (never sent): the finished workspace opens a blank terminal.
            intent["launch"] = json!("terminal");
        }
        // Persist the exact immutable intent before submission. Recovery never creates
        // another key or silently replays a changed form after an uncertain outcome.
        self.persist_intent(ctx, Some(&intent));
        self.intent = Some(intent);
        self.snapshot = None;
        self.opened = false;
        self.submit(ctx);
    }

    /// `_submit`: `<kind>.create.request` with the intent's request, 10 minute deadline.
    fn submit(&mut self, ctx: &UiContext) {
        let Some(intent) = self.intent.clone() else { return };
        let kind = str_of(&intent, "kind").to_owned();
        let message = super::rpc::message(&format!("{kind}.create.request"), object_of(&intent, "request").clone());
        let id = ctx.core.request_with_timeout(
            &self.target.host_id,
            message,
            &format!("{kind}.create.response"),
            Some(std::time::Duration::from_secs(600)),
        );
        self.busy = true;
        self.pending.track(id, "create", Req::Submit);
    }

    fn idempotency_key(&self) -> String {
        self.intent.as_ref().map(|i| str_of(object_of(i, "request"), "idempotencyKey").to_owned()).unwrap_or_default()
    }

    /// `_receive`: a creation receipt snapshot, ordered by revision.
    fn receive(&mut self, ctx: &mut UiContext, message: &Value) {
        if self.intent.is_none() {
            return;
        }
        let body = payload(message);
        let snapshot = if body.get("snapshot").is_some() { object_of(body, "snapshot") } else { body };
        if snapshot.as_object().is_none_or(|m| m.is_empty()) || str_of(snapshot, "idempotencyKey") != self.idempotency_key() {
            return;
        }
        if let Some(previous) = &self.snapshot && super::rpc::int_of(snapshot, "revision") < super::rpc::int_of(previous, "revision") {
            return;
        }
        let snapshot = snapshot.clone();
        self.snapshot = Some(snapshot.clone());
        match str_of(&snapshot, "phase") {
            "completed" => {
                let result = json!({ "workspace": snapshot["workspace"], "agent": snapshot["agent"], "creation": snapshot });
                self.finish(ctx, &result);
            }
            "failed" => {
                self.error = Some(str_or(&snapshot, "error", l("Creation failed. Inspect any created workspace before starting again.")).to_owned());
            }
            _ => {}
        }
    }

    fn recover(&mut self, ctx: &UiContext) {
        if self.intent.is_none() || self.finishing {
            return;
        }
        self.busy = true;
        self.observe_receipt(ctx, true);
    }

    fn observe_receipt(&mut self, ctx: &UiContext, replay_if_missing: bool) {
        let Some(intent) = &self.intent else { return };
        self.replay_if_missing = replay_if_missing;
        self.observation = Some(Observation::start(
            &ctx.core,
            &self.target.host_id,
            "creation.subscribe.request",
            "creation.subscribe.response",
            json!({ "kind": str_of(intent, "kind"), "idempotencyKey": self.idempotency_key(), "subscribe": true }),
        ));
    }

    /// Completion replaces this form's tab; opening through the focused pane would leave
    /// the creation surface behind and could navigate a sibling while its request finishes.
    fn finish(&mut self, ctx: &mut UiContext, result: &Value) {
        if self.finishing || self.opened || self.intent.is_none() {
            return;
        }
        let receipt = object_of(result, "creation");
        if receipt.as_object().is_some_and(|m| !m.is_empty()) && str_of(receipt, "phase") != "completed" {
            return;
        }
        let pick = |key: &str| {
            let value = object_of(result, key);
            if value.as_object().is_some_and(|m| !m.is_empty()) { value.clone() } else { object_of(receipt, key).clone() }
        };
        let workspace = pick("workspace");
        let agent = pick("agent");
        let workspace_id = str_or(&workspace, "id", str_or(receipt, "workspaceId", str_or(&agent, "workspaceId", &self.target.workspace_id))).to_owned();
        let agent_id = str_or(&agent, "id", str_of(receipt, "agentId")).to_owned();
        let expected_agent = self
            .intent
            .as_ref()
            .is_some_and(|i| str_of(i, "kind") == "agent" || object_of(i, "request").get("agent").is_some());
        if workspace_id.is_empty() || (agent_id.is_empty() && expected_agent) {
            return;
        }
        self.finishing = true;
        ctx.core.refresh_host(&self.target.host_id);
        self.persist_intent(ctx, None);
        self.observation = None;
        let fallback_cwd = if self.target.cwd.is_empty() { self.path.trim().to_owned() } else { self.target.cwd.clone() };
        let cwd = str_or(&agent, "cwd", str_or(&workspace, "workspaceDirectory", &fallback_cwd)).to_owned();
        let terminal = self.intent.as_ref().is_some_and(|i| str_of(i, "launch") == "terminal");
        self.opened = true;
        self.intent = None;
        self.busy = false;
        if terminal {
            self.terminal_workspace = Some((workspace_id.clone(), cwd.clone()));
            self.create_terminal(ctx, workspace_id, cwd);
        } else if agent_id.is_empty() {
            let target = PanelTarget::new("setup", &self.target.host_id, &workspace_id, &cwd)
                .with_title(str_or(&workspace, "name", l("Session")))
                .with_data(json!({ "repoRoot": str_or(&workspace, "projectRootPath", &cwd) }));
            ctx.effects.push(UiEffect::ReplaceTab { tab_key: self.target.tab_key.clone(), target });
        } else {
            let target = PanelTarget::agent(&self.target.host_id, &workspace_id, &cwd, &agent_id, str_of(&agent, "title"))
                .with_data(json!({ "project": object_of(&agent, "project") }));
            ctx.effects.push(UiEffect::ReplaceTab { tab_key: self.target.tab_key.clone(), target });
        }
        self.finishing = false;
    }

    /// `createTerminal` of `runCreateTerminalWorkspace`: a blank shell in the workspace.
    fn create_terminal(&mut self, ctx: &UiContext, workspace_id: String, cwd: String) {
        self.busy = true;
        self.error = None;
        let fields = json!({ "cwd": cwd, "workspaceId": workspace_id });
        self.pending.send(&ctx.core, &self.target.host_id, "create_terminal_request", fields, Req::CreateTerminal { workspace_id, cwd });
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        if self.target.host_id.is_empty() {
            return;
        }
        host::learn(&ctx.events);
        if !self.initialized {
            self.initialize(ctx);
            self.view.redraw(cx);
        }
        let status = self.host(ctx).map(|h| h.status());
        if status != self.last_status {
            let reconnected = status == Some(Connection::Connected);
            self.last_status = status;
            self.connected = reconnected;
            self.legacy_forge = self.host(ctx).is_some_and(|h| attachments::uses_legacy_forge(h.server_info()));
            if reconnected && self.intent.is_some() && !self.finishing && self.observation.is_none() {
                self.recover(ctx);
            }
            if reconnected && self.default_ref.is_none() {
                self.default_ref_cwd = None;
                self.load_default_ref(ctx);
            }
            if reconnected {
                self.pending.send(&ctx.core, &self.target.host_id, "get_daemon_config_request", json!({}), Req::Profiles);
                self.load_providers(ctx);
            }
            self.sync_forge(cx, ctx);
            self.view.redraw(cx);
        }
        // Creation receipt observation.
        if let Some(observation) = self.observation.as_mut() {
            let items = observation.take(&ctx.events);
            for item in items {
                self.view.redraw(cx);
                match item {
                    ObsEvent::Snapshot { payload, restored } => {
                        if restored {
                            self.receive(ctx, &payload);
                            continue;
                        }
                        self.busy = false;
                        if let Err(error) = require_success(&payload) {
                            self.error = Some(error.to_string());
                            continue;
                        }
                        if payload.get("snapshot").is_none_or(Value::is_null) {
                            if self.replay_if_missing {
                                self.submit(ctx);
                            }
                        } else {
                            self.receive(ctx, &payload);
                        }
                    }
                    ObsEvent::Event(message) => self.receive(ctx, &message),
                    ObsEvent::Failed(error) => {
                        self.busy = false;
                        self.observation = None;
                        self.error = Some(error.to_string());
                    }
                    ObsEvent::RestoreFailed(error) => {
                        self.error = Some(if error.is_empty() {
                            l("Creation observation failed. Recover the receipt to continue.").to_owned()
                        } else {
                            error
                        });
                    }
                }
            }
        }
        let answers = self.pending.take_raw(&ctx.events);
        for (req, result) in answers {
            self.view.redraw(cx);
            self.answer(cx, ctx, req, result);
        }
        // Loading snapshots complete through host events. Apply only this tab's
        // host/cwd and the latest update, without issuing another discovery pull.
        if let Some((_, event)) = ctx.host_events().filter(|(host, event)| {
            *host == self.target.host_id
                && str_of(event, "type") == "providers_snapshot_update"
                && self.provider_scope.as_deref()
                    == Some(str_of(object_of(event, "payload"), "cwd").trim().trim_end_matches('/'))
        }).last() {
            let body = object_of(event, "payload");
            if body.get("entries").is_some() || body.get("compactSnapshot").is_some() {
                self.provider_generation += 1;
                self.apply_providers(cx, ctx, body);
            } else {
                self.load_providers(ctx);
            }
            self.view.redraw(cx);
        }
    }

    fn answer(&mut self, cx: &mut Cx, ctx: &mut UiContext, req: Req, result: Result<Value, RpcError>) {
        let checked = |result: Result<Value, RpcError>, request: &str| -> Result<Value, RpcError> {
            let body = result?;
            if let Some(error) = super::rpc::domain_error(&body, request) {
                return Err(error);
            }
            require_success(&body)?;
            Ok(body)
        };
        match req {
            Req::Profiles => match checked(result, "get_daemon_config_request") {
                Ok(body) => {
                    self.profiles = array_of(object_of(&body, "config"), "agentProfiles").to_vec();
                    self.profiles_error = None;
                }
                Err(error) => self.profiles_error = Some(error.to_string()),
            },
            Req::DefaultRef(cwd) => {
                if self.default_ref_cwd.as_deref() != Some(cwd.as_str()) {
                    return;
                }
                match checked(result, "checkout_status_request") {
                    Ok(status) => {
                        let remote = str_of(&status, "remoteUrl");
                        self.remote_url = (!remote.is_empty()).then(|| remote.to_owned());
                        self.default_ref = model::default_ref(&status);
                        self.sync_forge(cx, ctx);
                    }
                    // Without status the trigger keeps the original "main" fallback.
                    Err(_) => self.default_ref = None,
                }
            }
            Req::AttachmentForge(scope) => {
                if self.attachment_forge_scope.as_ref() != Some(&scope) {
                    return;
                }
                match result {
                    // As in loadCheckoutForge, a domain error may still identify the forge.
                    Ok(body) => self.attachment_forge = str_or(&body, "forge", "github").to_owned(),
                    Err(_) => self.attachment_forge_scope = None,
                }
            }
            Req::Providers(generation) => {
                if generation != self.provider_generation {
                    return;
                }
                self.providers_loading = false;
                if let Ok(body) = checked(result, "get_providers_snapshot_request") {
                    self.provider_scope = Some(str_of(&body, "cwd").trim().trim_end_matches('/').to_owned());
                    self.apply_providers(cx, ctx, &body);
                }
            }
            Req::Features(key) => {
                if key != self.features_key {
                    return;
                }
                self.features = result.ok().map(|b| array_of(&b, "features").to_vec()).unwrap_or_default();
            }
            Req::AddProject => {
                self.busy = false;
                match checked(result, "project.add.request") {
                    Ok(body) if body.get("project").is_some_and(|p| !p.is_null()) => {
                        ctx.core.refresh_host(&self.target.host_id);
                        self.project_added = true;
                    }
                    Ok(_) => self.fail(ctx, l("The host did not return a registered project.").into(), false),
                    Err(error) => self.fail(ctx, error.to_string(), false),
                }
            }
            Req::Prepare => {
                self.busy = false;
                match result {
                    Ok(body) => {
                        self.preparation_unknown = false;
                        if let Err(error) = checked(Ok(body.clone()), "project.prepare") {
                            return self.fail(ctx, error.to_string(), false);
                        }
                        let directory = str_of(&body, if self.source == "clone" { "checkoutPath" } else { "directoryPath" }).to_owned();
                        if directory.is_empty() {
                            return self.fail(ctx, l("The host did not return the prepared directory. Browse to inspect its state.").into(), false);
                        }
                        self.set_directory(cx, ctx, &directory);
                        self.source = "directory".into();
                        self.existing = false;
                        ctx.core.refresh_host(&self.target.host_id);
                    }
                    // The outcome is unknown: keep the notice and the inspect path.
                    Err(error) => self.fail(ctx, error.to_string(), false),
                }
            }
            Req::SearchRepos => {
                self.busy = false;
                match checked(result, "workspace.github.search_repositories.request") {
                    Ok(body) => {
                        self.repositories = array_of(&body, "repositories").to_vec();
                        if self.repositories.is_empty() {
                            ctx.effects.push(UiEffect::Toast { message: l("No repositories found.").into(), is_error: false });
                            return;
                        }
                        let options = self
                            .repositories
                            .iter()
                            .map(|repo| SelectOption::new(str_of(repo, "nameWithOwner"), str_of(repo, "nameWithOwner")).description(str_of(repo, "description")))
                            .collect();
                        let request = SelectRequest::new(l("GitHub repositories"), options).selected(Some(self.repo.trim())).searchable("");
                        self.view.wa_select(id!(select)).open(cx, "repo", request);
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Req::Branches => {
                self.busy = false;
                match checked(result, "branch_suggestions_request") {
                    Ok(body) => {
                        self.branch_items = model::build_branch_items(&model::branch_details(&body));
                        self.open_ref_picker(cx);
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Req::Validate => match checked(result, "validate_branch_request") {
                Ok(body) if body.get("exists") == Some(&Value::Bool(true)) => self.create_features(ctx),
                Ok(_) => self.fail(ctx, l("The selected branch or base ref does not exist.").into(), true),
                Err(error) => self.fail(ctx, error.to_string(), true),
            },
            // Offered features; none when the list cannot be loaded.
            Req::CreateFeatures => {
                let offered = result.ok().map(|b| Value::Array(array_of(&b, "features").to_vec())).unwrap_or(Value::Array(Vec::new()));
                self.create_submit(ctx, offered);
            }
            Req::Submit => {
                // A receipt can complete before the request response. In particular, do
                // not clear `busy` while the terminal handoff is still pending.
                if self.intent.is_none() {
                    return;
                }
                self.busy = false;
                match result {
                    Ok(body) => {
                        if let Some(creation) = body.get("creation").filter(|c| c.is_object()) {
                            self.receive(ctx, creation);
                        }
                        if let Err(error) = checked(Ok(body.clone()), "create") {
                            self.error = Some(error.to_string());
                            ctx.effects.push(UiEffect::Toast { message: error.to_string(), is_error: true });
                            return;
                        }
                        self.finish(ctx, &body);
                        let failed = self.snapshot.as_ref().is_some_and(|s| str_of(s, "phase") == "failed");
                        if self.intent.is_some() && !self.finishing && !self.opened && self.observation.is_none() && !failed {
                            self.observe_receipt(ctx, false);
                        }
                    }
                    Err(error) => {
                        self.error = Some(error.to_string());
                        ctx.effects.push(UiEffect::Toast { message: error.to_string(), is_error: true });
                    }
                }
            }
            Req::ForgeSearch { lookup, index, cwd } => {
                let items = result.map(|body| forge::forge_search_items(self.legacy_forge, &body)).map_err(|e| e.to_string());
                let effects = self.forge_auto.search_finished(lookup, index, &cwd, items);
                self.apply_forge(cx, ctx, effects);
            }
            Req::ForgePicker => {
                self.busy = false;
                match result {
                    Ok(body) => {
                        self.forge_items = forge::forge_search_items(self.legacy_forge, &body);
                        self.open_forge_picker(cx);
                    }
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
            Req::Images => match result.map(|v| ops::open_envelope(&v)) {
                Ok(Ok(body)) => self.images.extend(array_of(&body, "images").iter().cloned()),
                Ok(Err(error)) => self.toast_upload_error(ctx, str_of(&error, "message")),
                Err(error) => self.toast_upload_error(ctx, &error.to_string()),
            },
            Req::Dropped => {
                self.drop_pending = false;
                match result.map(|v| ops::open_envelope(&v)) {
                    Ok(Ok(body)) => self.attach_dropped(ctx, array_of(&body, "files")),
                    Ok(Err(error)) => self.toast_upload_error(ctx, str_of(&error, "message")),
                    Err(error) => self.toast_upload_error(ctx, &error.to_string()),
                }
            }
            Req::Upload => {
                self.upload_name = None;
                match result.map(|v| ops::open_envelope(&v)) {
                    Ok(Ok(body)) => {
                        self.attachments.extend(array_of(&body, "files").iter().cloned());
                        self.sync_forge(cx, ctx);
                    }
                    Ok(Err(error)) => self.toast_upload_error(ctx, str_of(&error, "message")),
                    Err(error) => self.toast_upload_error(ctx, &error.to_string()),
                }
            }
            Req::CreateTerminal { workspace_id, cwd } => {
                if self.opened && self.terminal_workspace.is_none() {
                    return;
                }
                self.busy = false;
                // `createTerminal` surfaces `payload.error` with a null terminal (e.g. a missing
                // shell) instead of treating the create as a no-op success.
                let terminal = match checked(result, "create_terminal_request") {
                    Ok(body) => object_of(&body, "terminal").clone(),
                    Err(error) => {
                        self.opened = false;
                        return self.fail(ctx, error.to_string(), true);
                    }
                };
                let terminal_id = str_of(&terminal, "id").to_owned();
                if terminal_id.is_empty() {
                    self.opened = false;
                    return self.fail(ctx, t("newWorkspace.errors.createWorktreeFailed"), true);
                }
                self.terminal_workspace = None;
                self.opened = true;
                let target = PanelTarget::new("terminal", &self.target.host_id, str_or(&terminal, "workspaceId", &workspace_id), str_or(&terminal, "cwd", &cwd))
                    .with_id(terminal_id);
                ctx.effects.push(UiEffect::ReplaceTab { tab_key: self.target.tab_key.clone(), target });
            }
        }
    }

    fn toast_upload_error(&self, ctx: &mut UiContext, message: &str) {
        let message = if message.trim().is_empty() { t("composer.errors.uploadFailed") } else { message.to_owned() };
        ctx.effects.push(UiEffect::Toast { message, is_error: true });
    }

    fn open_ref_picker(&mut self, cx: &mut Cx) {
        let checkout = self.source == "checkout";
        let selected = if !self.ref_name.trim().is_empty() {
            Some(self.ref_name.trim().to_owned())
        } else if checkout {
            None
        } else {
            self.default_ref.as_ref().map(|d| d.ref_name.clone())
        };
        let options = self
            .branch_items
            .iter()
            .map(|item| {
                let value = if checkout { &item.branch_name } else { &item.ref_name };
                SelectOption::new(value.clone(), item.name.clone()).glyph("git-branch").trailing(item.divergence.clone().unwrap_or_default())
            })
            .collect();
        let request = SelectRequest::new(t("newWorkspace.refPicker.title"), options)
            .selected(selected.as_deref())
            .searchable(t("newWorkspace.refPicker.searchPlaceholder"))
            .empty(t("newWorkspace.refPicker.noMatchingRefs"));
        self.view.wa_select(id!(select)).open(cx, "ref", request);
    }

    fn open_forge_picker(&mut self, cx: &mut Cx) {
        let attached: Vec<String> = self
            .attachments
            .iter()
            .filter(|a| attachments::forge_labels(a).is_some())
            .map(attachments::forge_item_key)
            .collect();
        let options = self
            .forge_items
            .iter()
            .map(|item| {
                let key = attachments::forge_item_key(item);
                let number = super::rpc::int_of(item, "number");
                let prefix = if str_of(item, "kind") == "change_request" { "git-pull-request" } else { "circle-dot" };
                let mut option = SelectOption::new(key.clone(), format!("#{number} {}", str_of(item, "title"))).glyph(prefix);
                option = option.checked(attached.contains(&key));
                option
            })
            .collect();
        let title = if self.gitlab() { t("composer.attachments.addIssueOrPr_mr") } else { t("composer.attachments.addIssueOrPr") };
        self.view.wa_select(id!(select)).open(cx, "forge", SelectRequest::new(title, options).searchable(""));
    }

    fn gitlab(&self) -> bool {
        self.attachment_forge == "gitlab"
    }

    fn ensure_attachment_forge(&mut self, ctx: &mut UiContext) {
        let cwd = self.path.trim();
        if cwd.is_empty() || !self.connected {
            return;
        }
        let scope = (self.target.host_id.clone(), cwd.to_owned());
        if self.attachment_forge_scope.as_ref() == Some(&scope) {
            return;
        }
        let id = ctx.core.request(
            &scope.0,
            json!({"type": "checkout_pr_status_request", "cwd": scope.1}),
            "checkout_pr_status_response",
        );
        self.attachment_forge_scope = Some(scope.clone());
        self.pending.track(id, "forge.kind", Req::AttachmentForge(scope));
    }

    fn apply_providers(&mut self, cx: &mut Cx, ctx: &UiContext, body: &Value) {
        self.providers_loading = false;
        self.providers = agent_form::provider_entries(body);
        if !self.config_touched {
            self.config = agent_form::resolve_initial_agent_config(&self.initial_config, &Self::form_preferences(ctx), &self.providers);
        }
        self.load_features(ctx);
        self.view.wa_select(id!(select)).refresh(cx, "model", || self.model_request());
    }

    fn model_request(&self) -> SelectRequest {
        let mut options = Vec::new();
        for entry in self.providers.iter().filter(|e| e.get("enabled") != Some(&Value::Bool(false))) {
            let id = str_of(entry, "provider");
            let label = str_or(entry, "label", id).to_owned();
            let models = agent_form::selectable_models(entry);
            if models.is_empty() && str_of(entry, "status") != "ready" {
                let status = if str_of(entry, "status") == "loading" {
                    t("providerSelection.loading")
                } else {
                    str_or(entry, "error", &t("providerSelection.unavailable")).to_owned()
                };
                let separated = !options.is_empty();
                options.push(SelectOption::new(format!("{id}\u{0}"), label).description(status).enabled(false).separated(separated));
                continue;
            }
            let separated = !options.is_empty();
            for (index, row) in models.iter().enumerate() {
                let model_id = str_of(row, "id");
                options.push(
                    SelectOption::new(format!("{id}\u{0}{model_id}"), controls_model::model_label(row))
                        .description(format!("{label} · {model_id}"))
                        .separated(index == 0 && separated),
                );
            }
        }
        let selected = format!("{}\u{0}{}", str_of(&self.config, "provider"), str_of(&self.config, "model"));
        let empty = if self.providers_loading { t("modelSelector.loadingSelector") } else { t("providerSelection.readiness.noProviders") };
        SelectRequest::new(t("providerSelection.selectModel"), options)
            .selected(Some(&selected))
            .searchable(t("modelSelector.searchAllPlaceholder"))
            .empty(empty)
    }

    fn pick_models(&mut self, cx: &mut Cx) {
        self.view.wa_select(id!(select)).open(cx, "model", self.model_request());
    }

    fn pick_mode(&mut self, cx: &mut Cx) {
        let modes = self.entry().map(|e| array_of(e, "modes").to_vec()).unwrap_or_default();
        let options = modes.iter().map(|m| SelectOption::new(str_of(m, "id"), controls_model::format_agent_mode_label(m))).collect();
        self.view.wa_select(id!(select)).open(cx, "mode", SelectRequest::new(l("Select mode"), options).selected(Some(str_of(&self.config, "modeId"))));
    }

    fn pick_thinking(&mut self, cx: &mut Cx) {
        let options_list = self.current_model().map(|m| array_of(&m, "thinkingOptions").to_vec()).unwrap_or_default();
        let extra = t("agentControls.thinking.extraHigh");
        let options = options_list
            .iter()
            .map(|o| SelectOption::new(str_of(o, "id"), controls_model::format_thinking_option_label(o, &extra)))
            .collect();
        self.view.wa_select(id!(select)).open(cx, "thinking", SelectRequest::new(l("Select thinking"), options).selected(Some(str_of(&self.config, "thinkingOptionId"))));
    }

    /// A toggle feature flips at once; a select feature opens its options.
    fn pick_feature(&mut self, cx: &mut Cx, ctx: &mut UiContext, feature: &Value) {
        let id = str_of(feature, "id").to_owned();
        if str_of(feature, "type") == "toggle" {
            let current = model::feature_value(&self.config, feature) == &Value::Bool(true);
            let next = model::with_feature(&self.config, &id, Value::Bool(!current));
            self.change_config(ctx, next);
        } else {
            let options = array_of(feature, "options").iter().map(|o| SelectOption::new(str_of(o, "id"), str_or(o, "label", str_of(o, "id")))).collect();
            let selected = model::feature_value(&self.config, feature).as_str().map(str::to_owned);
            let title = str_or(feature, "label", &id).to_owned();
            self.view.wa_select(id!(select)).open(cx, &format!("feature:{id}"), SelectRequest::new(title, options).selected(selected.as_deref()));
        }
        self.view.redraw(cx);
    }

    /// Compact agent settings behind the model trigger (the composer's compact
    /// controls sheet): model, thinking, mode and the provider's features, each
    /// opening its own picker. `None` when the model is the only setting.
    fn controls_request(&self) -> Option<SelectRequest> {
        let mut options = Vec::new();
        let thinking_options = self.current_model().map(|m| array_of(&m, "thinkingOptions").to_vec()).unwrap_or_default();
        if !thinking_options.is_empty() {
            let thinking_id = str_of(&self.config, "thinkingOptionId");
            let extra = t("agentControls.thinking.extraHigh");
            let value = thinking_options
                .iter()
                .find(|o| str_of(o, "id") == thinking_id)
                .or(thinking_options.first())
                .map(|o| controls_model::format_thinking_option_label(o, &extra))
                .unwrap_or_default();
            options.push(SelectOption::new("thinking", t("agentControls.thinking.title")).trailing(value).glyph("brain"));
        }
        let modes = self.entry().map(|e| array_of(e, "modes").to_vec()).unwrap_or_default();
        if !modes.is_empty() {
            let selected = modes.iter().find(|m| str_of(m, "id") == str_of(&self.config, "modeId"));
            let value = selected.map(controls_model::format_agent_mode_label).unwrap_or_else(|| l("Default mode").to_owned());
            options.push(SelectOption::new("mode", t("agentControls.mode.title")).trailing(value).glyph(controls_model::mode_icon(selected)));
        }
        for feature in self.visible_features() {
            let current = model::feature_value(&self.config, &feature);
            let value = if str_of(&feature, "type") == "toggle" {
                t(if current == &Value::Bool(true) { "agentControls.features.on" } else { "agentControls.features.off" })
            } else {
                array_of(&feature, "options")
                    .iter()
                    .find(|o| Some(str_of(o, "id")) == current.as_str())
                    .map(|o| str_or(o, "label", str_of(o, "id")).to_owned())
                    .unwrap_or_default()
            };
            let label = str_or(&feature, "label", str_of(&feature, "id")).to_owned();
            options.push(SelectOption::new(format!("feature:{}", str_of(&feature, "id")), label).trailing(value).glyph(controls_model::feature_icon(&feature)));
        }
        if options.is_empty() {
            return None;
        }
        let model = match self.current_model() {
            Some(model) => controls_model::model_label(&model),
            None => t("providerSelection.selectModel"),
        };
        options.insert(0, SelectOption::new("model", t("modelSelector.model")).trailing(model).glyph("box"));
        // The composer's compact controls sheet carries the same title.
        Some(SelectRequest::new(t("modelSelector.selectModel"), options))
    }

    fn entry(&self) -> Option<&Value> {
        agent_form::provider_entry(&self.providers, str_of(&self.config, "provider"))
    }

    fn current_model(&self) -> Option<Value> {
        let entry = self.entry()?;
        let models = agent_form::selectable_models(entry);
        let wanted = str_of(&self.config, "model");
        agent_form::find_model_by_reference(Some(&models), wanted).cloned().or_else(|| {
            let default = agent_form::resolve_default_model_id(Some(&models));
            models.iter().find(|m| str_of(m, "id") == default).cloned()
        })
    }

    fn handle_platform(&mut self, cx: &mut Cx, ctx: &mut UiContext, actions: &Actions) {
        for action in actions.iter() {
            let Some(PlatformAction::Completed { request, result }) = action.downcast_ref::<PlatformAction>() else { continue };
            let Some(kind) = self.platform_requests.remove(request) else { continue };
            match (kind, result) {
                (PlatformReq::PickImages, Ok(PlatformValue::Files(files))) if !files.is_empty() => {
                    let files: Vec<(std::path::PathBuf, String)> = files
                        .iter()
                        .map(|f| (f.path.clone(), attachments::raster_image_mime_type(&f.name).map(str::to_owned).or(f.mime_type.clone()).unwrap_or_else(|| "image/png".into())))
                        .collect();
                    let id = ctx.core.operation(&self.target.host_id, move |_client| async move { Ok(encode_images(files)) });
                    self.pending.track(id, "images", Req::Images);
                }
                (PlatformReq::PickFiles, Ok(PlatformValue::Files(files))) if !files.is_empty() => {
                    let files = files
                        .iter()
                        .map(|f| UploadFile {
                            path: f.path.clone(),
                            name: f.name.clone(),
                            mime: f.mime_type.clone().unwrap_or_else(|| attachments::mime_type_for_path(&f.name).to_owned()),
                            size: f.size,
                        })
                        .collect();
                    self.upload_files(ctx, files);
                }
                (PlatformReq::Paste, Ok(PlatformValue::Clipboard(content))) => {
                    if content.images.is_empty() {
                        ctx.effects.push(UiEffect::Toast { message: t("composer.errors.noClipboardImage"), is_error: true });
                    }
                    for image in &content.images {
                        self.images.push(json!({
                            "data": base64::engine::general_purpose::STANDARD.encode(image.bytes.as_slice()),
                            "mimeType": image.mime_type,
                        }));
                    }
                }
                (PlatformReq::Paste, Err(_)) => {
                    ctx.effects.push(UiEffect::Toast { message: t("composer.errors.pasteImageFailed"), is_error: true });
                }
                (_, Err(error)) if error.code != "cancelled" => {
                    self.toast_upload_error(ctx, &error.message);
                }
                _ => {}
            }
            self.view.redraw(cx);
        }
    }

    /// actions.ts uploadFileAttachments: every file is size-checked before any upload starts.
    fn upload_files(&mut self, ctx: &mut UiContext, files: Vec<UploadFile>) {
        if let Err(too_large) = attachments::check_upload_sizes(files.iter().map(|f| (f.name.as_str(), f.size))) {
            let message = ta("composer.errors.fileTooLarge", &[("size", "50MB"), ("fileName", &too_large.file_name)]);
            ctx.effects.push(UiEffect::Toast { message, is_error: true });
            return;
        }
        if !self.connected {
            ctx.effects.push(UiEffect::Toast { message: t("composer.errors.daemonClientDisconnected"), is_error: true });
            return;
        }
        self.upload_name = files.last().map(|f| f.name.clone());
        let list: Vec<_> = files.into_iter().map(|f| (f.path, f.name, f.mime)).collect();
        let id = ctx.core.operation(&self.target.host_id, move |client| ops::upload_files(client, list));
        self.pending.track(id, "upload", Req::Upload);
    }

    /// Whether the composer card can take dropped files now (composer/index.tsx
    /// `useFileDrop` `disabled`): it is shown and neither submitting nor uploading.
    fn accepts_file_drop(&self) -> bool {
        !self.terminal() && !self.busy && self.intent.is_none() && !self.drop_pending && self.upload_name.is_none()
    }

    /// Files dropped anywhere on the screen: inspected off the GUI thread, then attached.
    fn drop_files(&mut self, ctx: &mut UiContext, paths: Vec<std::path::PathBuf>) {
        self.drop_pending = true;
        let id = ctx.core.operation(&self.target.host_id, move |_client| async move { Ok(stat_files(paths)) });
        self.pending.track(id, "dropped", Req::Dropped);
    }

    /// `useFileDrop`: raster images join the tray (`onFiles`), every other file uploads
    /// (`onGenericFiles`).
    fn attach_dropped(&mut self, ctx: &mut UiContext, files: &[Value]) {
        let mut images = Vec::new();
        let mut uploads = Vec::new();
        for file in files {
            let (path, name) = (std::path::PathBuf::from(str_of(file, "path")), str_of(file, "name"));
            match attachments::raster_image_mime_type(name) {
                Some(mime) => images.push((path, mime.to_owned())),
                None => uploads.push(UploadFile {
                    path,
                    name: name.to_owned(),
                    mime: attachments::mime_type_for_path(name).to_owned(),
                    size: file.get("size").and_then(Value::as_u64).unwrap_or(0),
                }),
            }
        }
        if !images.is_empty() {
            let id = ctx.core.operation(&self.target.host_id, move |_client| async move { Ok(encode_images(images)) });
            self.pending.track(id, "images", Req::Images);
        }
        if !uploads.is_empty() {
            self.upload_files(ctx, uploads);
        }
    }

    fn platform_call(&mut self, kind: PlatformReq, call: impl FnOnce(&paseo_gui_platform::PlatformHandle) -> u64) {
        if let Some(handle) = platform() {
            let id = call(&handle);
            self.platform_requests.insert(id, kind);
        }
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        self.handle_platform(cx, ctx, actions);
        let view = self.root();
        let pending_intent = self.intent.is_some();
        let locked = self.busy || pending_intent;
        // Text fields.
        let text_fields: [(&[LiveId], TextField); 10] = [
            (id!(ref_field.row.input_box), |s| &mut s.ref_name),
            (id!(branch_input), |s| &mut s.branch),
            (id!(pr_input), |s| &mut s.pr),
            (id!(forge_input), |s| &mut s.forge_name),
            (id!(project_path_input), |s| &mut s.project_path),
            (id!(repo_field.row.input_box), |s| &mut s.repo),
            (id!(dirname_input), |s| &mut s.directory_name),
            (id!(title_input), |s| &mut s.title),
            (id!(path), |s| &mut s.path),
            (id!(worktree_branch_input), |s| &mut s.branch),
        ];
        for (index, (path, field)) in text_fields.into_iter().enumerate() {
            if let Some(text) = view.wa_input(path).changed(actions) {
                *field(self) = text;
                // The inline worktree field and advanced options edit one branch.
                if index == 1 {
                    sync_input(cx, &view.wa_input(id!(worktree_branch_input)), &self.branch);
                } else if index == 9 {
                    sync_input(cx, &view.wa_input(id!(branch_input)), &self.branch);
                }
                // 0: the ref field, 8: the directory.
                if index == 0 {
                    self.ref_label = None;
                }
                if index == 8 {
                    // Directory edits refresh the catalog after 350ms.
                    cx.stop_timer(self.directory_timer);
                    self.directory_timer = cx.start_timeout(0.35);
                }
                self.view.redraw(cx);
            }
        }
        if let Some((text, _)) = view.wa_input(id!(path)).returned(actions) {
            self.set_directory(cx, ctx, text.trim());
        }
        let prompt = view.wa_input(id!(prompt));
        if let Some(text) = prompt.changed(actions) {
            self.prompt = text;
            self.sync_forge(cx, ctx);
            self.view.redraw(cx);
        }
        if let Some((_, modifiers)) = prompt.returned(actions) {
            // Desktop: Enter submits, Shift+Enter inserts (handled by TextInput); phones only
            // use the send button, so Enter inserts a newline there.
            if self.compact || host::is_mobile() || modifiers.shift {
                self.prompt = super::kit::insert_newline(cx, &prompt);
            } else if self.can_create() {
                self.start_create(ctx);
            }
            self.view.redraw(cx);
        }
        let open_terminal = view.wa_button(id!(open_terminal)).clicked(actions);
        if (view.wa_button(id!(submit)).clicked(actions) || open_terminal) && self.can_create() {
            self.start_create(ctx);
            self.view.redraw(cx);
        }
        // Field triggers.
        if view.wa_trigger(id!(project_field)).clicked(actions) && !self.existing && !pending_intent {
            self.open_project_picker(cx, ctx);
        }
        if view.wa_trigger(id!(host_field)).clicked(actions) && !self.existing && !pending_intent {
            let mut options: Vec<SelectOption> = host::hosts(&ctx.state)
                .iter()
                .map(|h| SelectOption::new(h.id(), h.label()).leading(Leading::Status(h.status())))
                .collect();
            options.push(SelectOption::new("\u{0}add-host", l("Add host")).glyph("plus"));
            let request = SelectRequest::new(t("newWorkspace.fields.host"), options).selected(Some(&self.target.host_id));
            view.wa_select(id!(select)).open(cx, "host", request);
        }
        if view.wa_trigger(id!(isolation_field)).clicked(actions) && !pending_intent && !self.busy {
            let options = vec![
                SelectOption::new("directory", t("newWorkspace.isolation.local")).glyph("folder"),
                SelectOption::new("branch-off", t("newWorkspace.isolation.worktree")).glyph("git-branch"),
            ];
            let selected = if self.worktree() { "branch-off" } else { "directory" };
            let request = SelectRequest::new(t("newWorkspace.isolation.label"), options).selected(Some(selected));
            view.wa_select(id!(select)).open(cx, "isolation", request);
        }
        if view.wa_trigger(id!(branch_field)).clicked(actions) && !pending_intent && !self.path.trim().is_empty() {
            self.request_branches(ctx);
        }
        if view.wa_button(id!(ref_field.search)).clicked(actions) && !locked {
            self.request_branches(ctx);
        }
        if view.wa_trigger(id!(launch_field)).clicked(actions) && !pending_intent && !self.busy {
            // `LaunchControl`: Chat and the blank Terminal. Terminal profiles and their Manage
            // profiles row are not offered (no profile support in this client).
            let options = vec![
                SelectOption::new("chat", t("newWorkspace.launch.chat")).glyph("message-circle"),
                SelectOption::new("terminal", t("newWorkspace.launch.terminal")).glyph("square-terminal"),
            ];
            let selected = if self.terminal() { "terminal" } else { "chat" };
            let request = SelectRequest::new(t("newWorkspace.launch.title"), options).selected(Some(selected));
            view.wa_select(id!(select)).open(cx, "launch", request);
        }
        // Composer toolbar.
        if view.wa_button(id!(attach)).clicked(actions) && self.connected && !locked {
            self.ensure_attachment_forge(ctx);
            let mut entries = vec![MenuItem::new("image", t("composer.attachments.addImage")).icon("image").entry()];
            if host::is_mobile() {
                entries.push(MenuItem::new("paste", t("composer.attachments.pasteImage")).icon("clipboard-paste").entry());
            }
            let forge_label = if self.gitlab() { t("composer.attachments.addIssueOrPr_mr") } else { t("composer.attachments.addIssueOrPr") };
            let forge = MenuItem::new("forge", forge_label);
            let forge = match crate::common::icons::forge_brand_svg(&self.attachment_forge) {
                Some(svg) => forge.svg(svg),
                None => forge.icon("git-pull-request"),
            };
            entries.push(forge.entry());
            entries.push(MenuItem::new("file", t("composer.attachments.addFile")).icon("paperclip").entry());
            let anchor = view.wa_button(id!(attach)).rect(cx);
            let owner = overlay::widget_owner(self.widget_uid());
            self.attachment_menu = Some(overlay::with_owner(ctx, &owner, |ctx| {
                overlay::open_menu(ctx, anchor, Some(t("composer.input.addAttachment")), entries)
            }));
        }
        if view.surface(id!(model_trigger)).clicked(actions) && !locked {
            // Compact hides the thinking, mode and feature buttons: the trigger lists them.
            match self.compact.then(|| self.controls_request()).flatten() {
                Some(request) => view.wa_select(id!(select)).open(cx, "controls", request),
                None => self.pick_models(cx),
            }
        }
        if view.wa_button(id!(mode)).clicked(actions) && !locked {
            self.pick_mode(cx);
        }
        if view.wa_button(id!(thinking)).clicked(actions) && !locked {
            self.pick_thinking(cx);
        }
        let features = view.wa_repeat(id!(features));
        if let Some(index) = features.items().iter().position(|item| item.as_wa_button().clicked(actions)) && let Some(feature) = self.visible_features().get(index).cloned() {
            self.pick_feature(cx, ctx, &feature);
        }
        let images = view.wa_repeat(id!(images));
        if let Some(index) = images.items().iter().position(|item| item.as_wa_attachment_pill().removed(actions)) && !locked && index < self.images.len() {
            self.images.remove(index);
            self.view.redraw(cx);
        }
        let files = view.wa_repeat(id!(files));
        if let Some(index) = files.items().iter().position(|item| item.as_wa_attachment_pill().removed(actions)) && !locked && index < self.attachments.len() {
            let removed = self.attachments.remove(index);
            self.forge_auto.mark_removed(&removed);
            self.sync_forge(cx, ctx);
            self.view.redraw(cx);
        }
        // Bottom row and options.
        if view.wa_button(id!(import_session)).clicked(actions) {
            ctx.effects.push(UiEffect::RunCommand { action: "session.import".into(), args: json!({ "serverId": self.target.host_id }) });
        }
        if view.wa_button(id!(options_toggle)).clicked(actions) {
            self.options_open = !self.options_open;
            self.view.redraw(cx);
        }
        if let Some(active) = view.pswitch(id!(existing)).changed(actions) {
            self.existing = active;
            if active {
                self.with_agent = true;
            }
            self.view.redraw(cx);
        }
        if let Some(active) = view.pswitch(id!(with_agent)).changed(actions) {
            self.with_agent = active;
            self.load_features(ctx);
            self.view.redraw(cx);
        }
        if view.wa_button(id!(source)).clicked(actions) {
            let glyphs = ["folder", "git-branch", "git-branch", "git-pull-request", "download", "folder-plus"];
            let labels = [
                l("Existing checkout or directory"),
                l("New worktree: branch off"),
                l("New worktree: existing branch"),
                l("New worktree: pull / merge request"),
                l("Clone GitHub repository"),
                l("Create remote directory"),
            ];
            let options = model::SOURCES.iter().zip(labels).zip(glyphs).map(|((id, label), glyph)| SelectOption::new(*id, label).glyph(glyph)).collect();
            view.wa_select(id!(select)).open(cx, "source", SelectRequest::new(t("newWorkspace.isolation.label"), options).selected(Some(&self.source)));
        }
        if view.wa_button(id!(browse)).clicked(actions) && !self.existing {
            self.open_browser(cx, ctx.compact);
        }
        if view.wa_button(id!(inspect_prepared)).clicked(actions) {
            self.open_browser(cx, ctx.compact);
        }
        if view.wa_button(id!(repo_field.search)).clicked(actions) && !locked {
            self.busy = true;
            self.error = None;
            self.pending.send(&ctx.core, &self.target.host_id, "workspace.github.search_repositories.request", json!({ "query": self.repo.trim(), "limit": 30 }), Req::SearchRepos);
        }
        if let Some(protocol) = view.psegmented(id!(protocol)).changed(actions) {
            self.clone_protocol = protocol;
            self.view.redraw(cx);
        }
        if view.wa_button(id!(prepare)).clicked(actions) && !self.preparation_unknown && !locked {
            self.prepare_directory(ctx);
        }
        if view.wa_button(id!(profile)).clicked(actions) {
            let mut options = vec![SelectOption::new("", l("Custom configuration"))];
            options.extend(self.profiles.iter().map(|p| SelectOption::new(str_of(p, "id"), str_of(p, "name"))));
            view.wa_select(id!(select)).open(cx, "profile", SelectRequest::new(l("Agent profile"), options).selected(Some(self.profile_id.as_deref().unwrap_or(""))));
        }
        if view.wa_button(id!(add_project)).clicked(actions) && !locked {
            if self.path.trim().is_empty() {
                self.fail(ctx, l("Choose a remote directory.").into(), false);
            } else {
                self.busy = true;
                self.error = None;
                self.pending.send(&ctx.core, &self.target.host_id, "project.add.request", json!({ "cwd": self.path.trim() }), Req::AddProject);
            }
        }
        // Pending card.
        if view.wa_button(id!(recover)).clicked(actions) && !self.busy {
            self.recover(ctx);
        }
        if view.wa_button(id!(open_workspace)).clicked(actions) && let Some(snapshot) = &self.snapshot {
            let workspace = object_of(snapshot, "workspace");
            let cwd = str_or(workspace, "workspaceDirectory", &self.path).to_owned();
            let target = PanelTarget::new("setup", &self.target.host_id, str_of(snapshot, "workspaceId"), &cwd)
                .with_data(json!({ "repoRoot": str_or(workspace, "projectRootPath", &self.path) }));
            ctx.effects.push(UiEffect::OpenTab { target, placement: TabPlacement::Main });
        }
        if view.wa_button(id!(open_session)).clicked(actions) && let Some(snapshot) = &self.snapshot {
            ctx.effects.push(UiEffect::OpenSession { host_id: self.target.host_id.clone(), agent_id: str_of(snapshot, "agentId").to_owned() });
        }
        if view.wa_button(id!(new_attempt)).clicked(actions) && !self.busy {
            view.wa_dialog(id!(dialog)).confirm(
                cx,
                "new-attempt",
                l("Start a new creation attempt?"),
                l("The host reported failure. Any partial workspace or agent remains available below; a new attempt uses a new identity."),
                l("New attempt"),
                false,
            );
        }
        if view.wa_dialog(id!(dialog)).confirmed(actions).as_deref() == Some("new-attempt")
            && self.snapshot.as_ref().is_some_and(|s| str_of(s, "phase") == "failed")
        {
            self.persist_intent(ctx, None);
            self.observation = None;
            self.intent = None;
            self.snapshot = None;
            self.error = None;
            self.view.redraw(cx);
        }
        self.handle_picks(cx, actions, ctx);
        self.handle_folder(cx, actions, ctx);
    }

    fn request_branches(&mut self, ctx: &mut UiContext) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        self.pending.send(&ctx.core, &self.target.host_id, "branch_suggestions_request", json!({ "cwd": self.path.trim(), "query": "", "limit": 100 }), Req::Branches);
    }

    fn open_project_picker(&mut self, cx: &mut Cx, ctx: &UiContext) {
        let Some(host) = self.host(ctx) else { return };
        let mut options: Vec<SelectOption> = host
            .projects()
            .iter()
            .filter(|p| !project_root(p).is_empty())
            .map(|p| {
                let name = host::project_name(p);
                SelectOption::new(project_root(p), name.clone())
                    .description(project_root(p))
                    .leading(Leading::Project(name, project_view_key(&self.target.host_id, p)))
            })
            .collect();
        let separated = !options.is_empty();
        options.push(SelectOption::new("\u{0}browse", t("sidebar.actions.addProject")).glyph("folder-plus").separated(separated));
        let request = SelectRequest::new(t("newWorkspace.fields.project"), options)
            .selected(Some(self.path.trim()))
            .searchable(l("Search projects"))
            .empty(l("No projects available."));
        self.view.wa_select(id!(select)).open(cx, "project", request);
    }

    fn open_browser(&mut self, cx: &mut Cx, compact: bool) {
        let cwd = if self.path.trim().is_empty() { "~".to_owned() } else { self.path.trim().to_owned() };
        self.view.widget(id!(folder_modal.sheet.header.title)).set_text(cx, &t("workspace.folderPicker.title"));
        // The modal clamps the body to the window: an oversized body fills a phone's height.
        let body_height = if compact { 10_000.0 } else { 495.0 };
        self.view.widget(id!(folder_modal.sheet.body)).apply_over(cx, live! { height: (body_height) });
        self.view.remote_folder_picker(id!(picker)).open(cx, &self.target.host_id, &cwd);
        self.view.wa_modal(id!(folder_modal)).open(cx);
    }

    fn handle_folder(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        let modal = self.view.wa_modal(id!(folder_modal));
        if self.view.wa_button(id!(folder_modal.sheet.header.close)).clicked(actions) {
            modal.close(cx);
        }
        for action in actions.filter_widget_actions(self.view.remote_folder_picker(id!(picker)).widget_uid()) {
            match action.cast::<RemoteFolderPickerAction>() {
                RemoteFolderPickerAction::Selected(path) => {
                    modal.close(cx);
                    self.set_directory(cx, ctx, &path);
                    if self.preparation_unknown {
                        self.source = "directory".into();
                        self.preparation_unknown = false;
                    }
                    self.view.redraw(cx);
                }
                RemoteFolderPickerAction::Cancelled => modal.close(cx),
                _ => {}
            }
        }
    }

    fn prepare_directory(&mut self, ctx: &mut UiContext) {
        self.error = None;
        if self.path.trim().is_empty() {
            return self.fail(ctx, l("Choose a remote parent directory.").into(), false);
        }
        if self.source == "new-directory" && self.directory_name.trim().is_empty() {
            return self.fail(ctx, l("Enter a directory name.").into(), false);
        }
        if self.source == "clone" && self.repo.trim().is_empty() {
            return self.fail(ctx, l("Choose a repository or enter owner/repository.").into(), false);
        }
        self.preparation_unknown = true;
        self.busy = true;
        let (request, fields) = if self.source == "clone" {
            ("project.github.clone", json!({ "repo": self.repo.trim(), "cloneProtocol": self.clone_protocol, "targetDirectory": self.path.trim() }))
        } else {
            ("project.create_directory", json!({ "parentPath": self.path.trim(), "name": self.directory_name.trim() }))
        };
        let id = ctx.core.request_with_timeout(
            &self.target.host_id,
            super::rpc::message(&format!("{request}.request"), fields),
            &format!("{request}.response"),
            Some(std::time::Duration::from_secs(600)),
        );
        self.pending.track(id, request, Req::Prepare);
    }

    fn handle_picks(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        let Some((token, value)) = self.view.wa_select(id!(select)).picked(actions) else { return };
        self.handle_pick(cx, &token, value, ctx);
    }

    fn handle_pick(&mut self, cx: &mut Cx, token: &str, value: String, ctx: &mut UiContext) {
        match token {
            "project" if value == "\u{0}browse" => self.open_browser(cx, ctx.compact),
            "project" => self.set_directory(cx, ctx, &value),
            "host" if value == "\u{0}add-host" => ctx.effects.push(UiEffect::OpenHostDialog),
            "host" if value != self.target.host_id && !self.busy && self.intent.is_none() => {
                // Replace the owning form, carrying its draft to the newly selected host.
                let mut data = self.target.data.as_object().cloned().unwrap_or_default();
                let launch = if self.terminal() { "terminal" } else { "chat" };
                data.insert(
                    "creationDraft".into(),
                    json!({ "title": self.title, "prompt": self.prompt, "config": self.config, "launch": launch }),
                );
                let target = PanelTarget::new("new_tab", &value, "", "").with_id(new_id()).with_title(t("sessionNavigation.newSession")).with_data(Value::Object(data));
                ctx.effects.push(UiEffect::ReplaceTab { tab_key: self.target.tab_key.clone(), target });
            }
            "ref" => {
                let checkout = self.source == "checkout";
                if let Some(item) = self.branch_items.iter().find(|i| (if checkout { &i.branch_name } else { &i.ref_name }) == &value) {
                    self.ref_label = Some(item.name.clone());
                }
                self.ref_name = value;
                sync_input(cx, &self.view.wa_input(id!(ref_field.row.input_box)), &self.ref_name);
            }
            "launch" => {
                let terminal = value == "terminal";
                if terminal != self.launch_terminal {
                    self.launch_terminal = terminal;
                    self.focus_prompt = !terminal && !self.compact;
                }
            }
            "repo" => {
                self.repo = value;
                sync_input(cx, &self.view.wa_input(id!(repo_field.row.input_box)), &self.repo);
            }
            "source" => {
                // `clearPickerSelectionForTargetChange`.
                if value != self.source {
                    self.ref_name.clear();
                    self.ref_label = None;
                    sync_input(cx, &self.view.wa_input(id!(ref_field.row.input_box)), &self.ref_name);
                }
                self.source = value;
            }
            // The new-session row's Local / New worktree choice. Local keeps the tab's
            // workspace when it has one; New worktree branches off into a new workspace.
            "isolation" => {
                let worktree = value == "branch-off";
                if worktree != self.worktree() {
                    self.ref_name.clear();
                    self.ref_label = None;
                    sync_input(cx, &self.view.wa_input(id!(ref_field.row.input_box)), &self.ref_name);
                }
                self.existing = !worktree && !self.target.workspace_id.is_empty();
                self.source = if worktree { "branch-off" } else { "directory" }.into();
                if self.existing {
                    self.with_agent = true;
                }
            }
            "profile" if value.is_empty() => self.profile_id = Some(String::new()),
            "profile" => {
                if let Some(profile) = self.profiles.iter().find(|p| str_of(p, "id") == value).cloned() {
                    let preferences = Self::form_preferences(ctx);
                    let mut fields = Map::new();
                    for key in ["provider", "model", "modeId", "thinkingOptionId"] {
                        fields.insert(key.into(), str_of(&profile, key).trim().into());
                    }
                    fields.insert("featureValues".into(), object_of(&profile, "featureValues").clone());
                    let next = agent_form::apply_profile_choice(&Value::Object(fields), &preferences, &self.providers);
                    let remembered = agent_form::remember_profile_selection(
                        &preferences,
                        str_of(&self.config, "provider"),
                        &self.providers,
                        &next,
                        object_of(&profile, "featureValues"),
                    );
                    Self::remember(ctx, remembered);
                    self.profile_id = Some(value);
                    self.config = next;
                    self.config_touched = true;
                    self.load_features(ctx);
                }
            }
            "model" => {
                if let Some((provider, model_id)) = value.split_once('\u{0}') {
                    let mut next = self.config.as_object().cloned().unwrap_or_default();
                    next.insert("provider".into(), provider.into());
                    next.insert("model".into(), model_id.into());
                    self.change_config(ctx, Value::Object(next));
                }
            }
            "mode" | "thinking" => {
                let key = if token == "mode" { "modeId" } else { "thinkingOptionId" };
                let mut next = self.config.clone();
                next[key] = value.into();
                self.change_config(ctx, next);
            }
            "controls" => match value.as_str() {
                "model" => self.pick_models(cx),
                "thinking" => self.pick_thinking(cx),
                "mode" => self.pick_mode(cx),
                other => {
                    let id = other.strip_prefix("feature:").unwrap_or_default();
                    if let Some(feature) = self.visible_features().into_iter().find(|f| str_of(f, "id") == id) {
                        self.pick_feature(cx, ctx, &feature);
                    }
                }
            },
            "attach" => match value.as_str() {
                "image" => self.platform_call(PlatformReq::PickImages, |p| {
                    p.pick_files(PickFilesOptions { multiple: true, images_only: true, title: Some(t("composer.attachments.addImage")) })
                }),
                "paste" => self.platform_call(PlatformReq::Paste, |p| p.read_clipboard(true)),
                "file" => self.platform_call(PlatformReq::PickFiles, |p| {
                    p.pick_files(PickFilesOptions { multiple: true, images_only: false, title: Some(t("composer.attachments.addFile")) })
                }),
                "forge" => {
                    self.busy = true;
                    let (message, response) = forge::forge_search_request(self.legacy_forge, self.path.trim(), "");
                    let id = ctx.core.request(&self.target.host_id, message, response);
                    self.pending.track(id, "forge.picker", Req::ForgePicker);
                }
                _ => {}
            },
            // `handleToggleGithubItem`: an attached item is removed, any other attached.
            "forge" => {
                if let Some(item) = self.forge_items.iter().find(|i| attachments::forge_item_key(i) == value).cloned() {
                    let position = self
                        .attachments
                        .iter()
                        .position(|a| attachments::forge_labels(a).is_some() && attachments::forge_item_key(a) == value);
                    match position {
                        // A picker removal never re-attaches.
                        Some(index) => {
                            let removed = self.attachments.remove(index);
                            self.forge_auto.mark_removed(&removed);
                        }
                        None => self.attachments.push(attachments::forge_attachment(&item, self.legacy_forge)),
                    }
                    self.sync_forge(cx, ctx);
                }
            }
            token if token.starts_with("feature:") => {
                let id = &token["feature:".len()..];
                let next = model::with_feature(&self.config, id, Value::String(value));
                self.change_config(ctx, next);
            }
            _ => {}
        }
        self.view.redraw(cx);
    }

    fn visible_features(&self) -> Vec<Value> {
        self.features
            .iter()
            .filter(|f| matches!(str_of(f, "type"), "toggle" | "select"))
            .cloned()
            .collect()
    }

    fn can_create(&self) -> bool {
        !self.busy
            && self.intent.is_none()
            && (self.existing || !matches!(self.source.as_str(), "clone" | "new-directory"))
            // waitForForgeAutoAttachOnSubmit: pasted forge links resolve first.
            && !self.forge_resolving()
    }

    fn present(&mut self, cx: &mut Cx, ctx: &UiContext, width: f64) {
        let view = self.root();
        if width > 0.0 {
            self.compact = host::is_compact(width);
            let column = width.min(820.0);
            if (column - self.column_width).abs() > 0.5 {
                self.column_width = column;
                view.widget(id!(column)).apply_over(cx, live! { width: (column) });
                // Original `inputWrapper` below the 720px breakpoint: 8 top/bottom and 12
                // side insets, the button row 6 from the border (12 − 6), no gap between
                // "+" and the model trigger; the compact dock is bottom-anchored with no
                // reserved space under the card.
                let (card_bottom, box_top, box_side, bar_side, gap, scroll_bottom) =
                    if self.compact { (8.0, 8.0, 12.0, 6.0, 0.0, 0.0) } else { (12.0, 16.0, 16.0, 10.0, 4.0, 60.0) };
                if self.compact {
                    view.widget(id!(fields)).apply_over(cx, live! { flow: Down });
                } else {
                    view.widget(id!(fields)).apply_over(cx, live! { flow: Right });
                }
                view.widget(id!(composer.card)).apply_over(cx, live! { padding: { bottom: (card_bottom) } });
                view.widget(id!(prompt_box)).apply_over(cx, live! { padding: { left: (box_side), right: (box_side), top: (box_top) } });
                view.widget(id!(toolbar)).apply_over(cx, live! { padding: { left: (bar_side), right: (bar_side) } });
                view.widget(id!(agent_controls)).apply_over(cx, live! { spacing: (gap) });
                view.widget(id!(scroll)).apply_over(cx, live! { padding: { bottom: (scroll_bottom) } });
            }
        }
        if !self.synced {
            self.synced = true;
            for (path, text) in [
                (id!(path), self.path.clone()),
                (id!(title_input), self.title.clone()),
                (id!(prompt), self.prompt.clone()),
            ] {
                sync_input(cx, &view.wa_input(path), &text);
            }
            // Native composer: 30px minimum input (6px vertical inset) and 20px glyphs.
            if host::is_mobile() {
                view.widget(id!(prompt.input)).apply_over(cx, live! { padding: { top: 6., bottom: 6. } });
                view.widget(id!(provider)).apply_over(cx, live! { width: 20., height: 20. });
            }
        }
        let pending_intent = self.intent.is_some();
        let add_project = self.add_project();
        let host = self.host(ctx);
        // Pending creation card.
        let card = view.widget(id!(pending));
        card.set_visible(cx, pending_intent);
        if let Some(intent) = &self.intent {
            let phase = self.snapshot.as_ref().map(|s| str_of(s, "phase").to_owned()).unwrap_or_else(|| l("awaiting confirmation").to_owned());
            card.widget(id!(row.body.label)).set_text(cx, &lf("Creation: {phase}", &[("phase", &phase)]));
            let mut hint = vec![
                lf("Receipt: {key}", &[("key", str_of(object_of(intent, "request"), "idempotencyKey"))]),
                l("The original request is retained. Recover its receipt instead of creating a duplicate.").to_owned(),
            ];
            if self.snapshot.as_ref().is_some_and(|s| super::rpc::flag(s, "outcomeUnknown")) {
                hint.push(l("The host reports an unknown outcome. Inspect the created resources before proceeding.").to_owned());
            }
            set_optional(cx, &card.widget(id!(row.body.hint)), &hint.join("\n"));
            let recover = view.wa_button(id!(recover));
            recover.set_text(cx, l("Recover / refresh creation"));
            recover.set_busy(cx, self.busy);
            let snapshot = self.snapshot.clone().unwrap_or(Value::Null);
            let open_ws = view.wa_button(id!(open_workspace));
            open_ws.set_visible(cx, !str_of(&snapshot, "workspaceId").is_empty());
            open_ws.set_text(cx, l("Open created workspace"));
            let open_session = view.wa_button(id!(open_session));
            open_session.set_visible(cx, !str_of(&snapshot, "agentId").is_empty());
            open_session.set_text(cx, l("Open created session"));
            let attempt = view.wa_button(id!(new_attempt));
            attempt.set_visible(cx, str_of(&snapshot, "phase") == "failed");
            attempt.set_text(cx, l("Start new attempt"));
            attempt.set_enabled(cx, !self.busy);
        }
        let notice = if self.opened {
            l("Session created. It is open in the tab bar.")
        } else if self.project_added {
            l("Project added. It is available in the sidebar.")
        } else {
            ""
        };
        set_optional(cx, &view.widget(id!(notice)), notice);
        // Compact shows the New session title in the header; Add project names itself here.
        let heading = if add_project { t("sidebar.actions.addProject") } else if self.compact { String::new() } else { t("sessionNavigation.newSession") };
        set_optional(cx, &view.widget(id!(heading)), &heading);

        // Field triggers.
        let projects: Vec<Value> = host.map(|h| h.projects().iter().filter(|p| !project_root(p).is_empty()).cloned().collect()).unwrap_or_default();
        let selected = projects.iter().find(|p| project_root(p) == self.path.trim());
        let project_name = match selected {
            Some(project) => host::project_name(project),
            None if self.path.is_empty() => l("Choose project").to_owned(),
            None => host::last_segment(&self.path),
        };
        let tip = |key: &str| if self.compact { String::new() } else { t(key) };
        let project_lead = match selected {
            Some(project) => TriggerLead::Project {
                fill: super::identity::rgb(super::identity::identity_fill(&project_view_key(&self.target.host_id, project))),
                initial: super::identity::project_initial(&project_name),
            },
            None => TriggerLead::Icon("folder"),
        };
        let project_trigger = view.wa_trigger(id!(project_field));
        // Add project's options carry the remote directory field; a second selector for the
        // same directory only shows while those options are collapsed.
        let show_project = !(add_project && self.options_open);
        project_trigger.set_visible(cx, show_project);
        project_trigger.set(cx, &t("newWorkspace.fields.project"), &project_name, &tip("newWorkspace.tooltips.project"), project_lead);
        project_trigger.set_enabled(cx, !self.existing && !pending_intent);
        let host_status = host.map(|h| h.status()).unwrap_or(Connection::Disconnected);
        let host_trigger = view.wa_trigger(id!(host_field));
        let host_label = host.map(|h| h.label()).unwrap_or_else(|| l("Host").to_owned());
        host_trigger.set(cx, &t("newWorkspace.fields.host"), &host_label, &tip("newWorkspace.tooltips.host"), TriggerLead::Status(host_status));
        host_trigger.set_enabled(cx, !self.existing && !pending_intent);
        // New session: Local or New worktree, next to Branch (Add project picks its source
        // among all of them in the options).
        let worktree = self.worktree();
        let isolation_trigger = view.wa_trigger(id!(isolation_field));
        isolation_trigger.set_visible(cx, !add_project);
        let (isolation_label, isolation_icon) =
            if worktree { (t("newWorkspace.isolation.worktree"), "git-branch") } else { (t("newWorkspace.isolation.local"), "folder") };
        isolation_trigger.set(cx, &t("newWorkspace.isolation.label"), &isolation_label, &tip("newWorkspace.tooltips.isolation"), TriggerLead::Icon(isolation_icon));
        isolation_trigger.set_enabled(cx, !pending_intent && !self.busy);
        let worktree_row = view.widget(id!(worktree));
        let show_worktree_name = !add_project && worktree && self.source == "branch-off";
        worktree_row.set_visible(cx, show_worktree_name);
        if show_worktree_name {
            let field = worktree_row.widget(id!(worktree_branch));
            field.widget(id!(label)).set_text(cx, l("New branch name"));
            set_optional(cx, &field.widget(id!(hint)), l("Automatic if empty"));
            view.wa_input(id!(worktree_branch_input)).set_is_read_only(cx, pending_intent || self.busy);
        }
        let show_branch = !self.existing && matches!(self.source.as_str(), "branch-off" | "checkout");
        let branch_trigger = view.wa_trigger(id!(branch_field));
        branch_trigger.set_visible(cx, show_branch);
        let branch_value = if !self.ref_name.trim().is_empty() {
            self.ref_label.clone().unwrap_or_else(|| self.ref_name.trim().to_owned())
        } else if self.source == "checkout" {
            t("newWorkspace.refPicker.startingRef")
        } else {
            self.default_ref.as_ref().map(|d| d.name.clone()).unwrap_or_else(|| "main".into())
        };
        branch_trigger.set(cx, &t("newWorkspace.fields.branch"), &branch_value, &tip("newWorkspace.tooltips.startingRef"), TriggerLead::Icon("git-branch"));
        branch_trigger.set_enabled(cx, !pending_intent && !self.path.trim().is_empty());
        let terminal = self.terminal();
        let launch = view.wa_trigger(id!(launch_field));
        launch.set_visible(cx, !add_project);
        let (launch_label, launch_icon) = if terminal { (t("newWorkspace.launch.terminal"), "square-terminal") } else { (t("newWorkspace.launch.chat"), "message-circle") };
        launch.set(cx, &t("newWorkspace.launch.title"), &launch_label, &tip("newWorkspace.tooltips.launch"), TriggerLead::Icon(launch_icon));
        launch.set_enabled(cx, !pending_intent && !self.busy);
        view.widget(id!(fields.spacer)).set_visible(cx, !self.compact);
        if !self.compact && self.column_width > 0.0 {
            // Launch keeps its badge at the row's end; the selectors share the rest
            // (16px row insets, 8px between the selectors, spacer and launch).
            let launch_w = if add_project { 0.0 } else { launch.badge_width(cx).min(240.0) };
            let selectors = [(&project_trigger, show_project), (&host_trigger, true), (&isolation_trigger, !add_project), (&branch_trigger, show_branch)];
            let naturals: Vec<f64> = selectors.iter().filter(|(_, shown)| *shown).map(|(trigger, _)| trigger.badge_width(cx)).collect();
            let gaps = naturals.len() + usize::from(!add_project);
            let available = self.column_width - 32.0 - launch_w - 8.0 * gaps as f64;
            let cap = shared_cap(&naturals, available, 240.0);
            for (trigger, _) in selectors {
                trigger.set_max_width(cx, Some(cap));
            }
        }

        // Composer card (original `inputWrapper`: 16/16/12 insets at ≥720px, else 8/12).
        // A Terminal launch takes no attachments (`NO_TERMINAL_ATTACHMENTS`).
        let has_tray = !terminal && (!self.images.is_empty() || !self.attachments.is_empty() || self.upload_name.is_some());
        view.widget(id!(tray)).set_visible(cx, has_tray);
        let locked = self.busy || pending_intent;
        let images = view.wa_repeat(id!(images));
        images.set_count(cx, self.images.len());
        for (index, image) in self.images.iter().enumerate() {
            images.item(cx, index).as_wa_attachment_pill().set(cx, PillBody::Image(image.clone()), true, !locked);
        }
        let files = view.wa_repeat(id!(files));
        files.set_count(cx, self.attachments.len());
        for (index, attachment) in self.attachments.iter().enumerate() {
            let (title, subtitle, glyph) = attachments::describe(attachment);
            let icon = match glyph {
                attachments::AttachmentGlyph::FileText => "file-text",
                attachments::AttachmentGlyph::CircleDot => "circle-dot",
                attachments::AttachmentGlyph::GitPullRequest => "git-pull-request",
            };
            let subtitle = subtitle.unwrap_or_else(|| t("message.attachments.file"));
            files.item(cx, index).as_wa_attachment_pill().set(cx, PillBody::Label { icon, title, subtitle }, true, !locked);
        }
        let uploading = view.wa_attachment_pill(id!(uploading));
        uploading.set_visible(cx, self.upload_name.is_some());
        if let Some(name) = &self.upload_name {
            let subtitle = attachments::file_type_label(name).unwrap_or_default();
            uploading.set(cx, PillBody::Pending { title: name.clone(), subtitle }, false, false);
        }
        // A Terminal launch replaces the composer with one Open terminal action.
        view.widget(id!(composer)).set_visible(cx, !terminal);
        view.widget(id!(terminal_launch)).set_visible(cx, terminal);
        let prompt = view.wa_input(id!(prompt));
        prompt.set_visible(cx, self.with_agent);
        prompt.set_is_read_only(cx, locked);
        prompt.set_empty_text(cx, t(if self.compact { "composer.placeholders.mobile" } else { "composer.placeholders.desktop" }));
        let preview = if self.with_agent { String::new() } else if self.title.is_empty() { t("newWorkspace.title") } else { self.title.clone() };
        set_optional(cx, &view.widget(id!(title_preview)), &preview);
        view.widget(id!(agent_controls)).set_visible(cx, self.with_agent);
        let attach = view.wa_button(id!(attach));
        attach.set_enabled(cx, self.connected && !locked);
        attach.set_tooltip(cx, &t("composer.input.addAttachment"));
        // Original `AgentControls`: the model trigger (provider glyph, short model label; the
        // compact trigger appends the thinking level extra-muted), then thinking, mode and
        // feature controls with carets on wide layouts.
        let provider = str_of(&self.config, "provider").to_owned();
        let model_label = match self.current_model() {
            Some(model) if !provider.is_empty() => controls_model::short_model_label(&controls_model::model_label(&model)),
            _ if self.providers_loading => t("providerSelection.loading"),
            _ => t("providerSelection.selectModel"),
        };
        let trigger = view.widget(id!(model_trigger));
        let provider_icon = trigger.provider_icon(id!(provider));
        trigger.widget(id!(provider_box)).set_visible(cx, !provider.trim().is_empty());
        if !provider.trim().is_empty() {
            let svg = self.entry().map(|e| str_of(e, "iconSvg").to_owned()).filter(|svg| !svg.is_empty());
            provider_icon.set_provider(cx, &provider, svg.as_deref());
        }
        trigger.widget(id!(label)).set_text(cx, &model_label);
        trigger.wa_icon(id!(caret)).set_visible(cx, !self.compact);
        let modes = self.entry().map(|e| array_of(e, "modes").to_vec()).unwrap_or_default();
        let mode_id = str_of(&self.config, "modeId");
        let selected_mode = modes.iter().find(|m| str_of(m, "id") == mode_id);
        let mode = view.wa_button(id!(mode));
        mode.set_visible(cx, !modes.is_empty() && !self.compact);
        let mode_label = selected_mode.map(controls_model::format_agent_mode_label).unwrap_or_else(|| l("Default mode").to_owned());
        mode.set_text(cx, &mode_label);
        mode.set_icon(cx, controls_model::mode_icon(selected_mode));
        mode.set_trailing(cx, "chevron-down");
        mode.set_tooltip(cx, &t("agentControls.hints.mode"));
        let thinking_options = self.current_model().map(|m| array_of(&m, "thinkingOptions").to_vec()).unwrap_or_default();
        let thinking = view.wa_button(id!(thinking));
        thinking.set_visible(cx, !thinking_options.is_empty() && !self.compact);
        let thinking_id = str_of(&self.config, "thinkingOptionId");
        let extra = t("agentControls.thinking.extraHigh");
        let thinking_label = thinking_options
            .iter()
            .find(|o| str_of(o, "id") == thinking_id)
            .or(thinking_options.first())
            .map(|o| controls_model::format_thinking_option_label(o, &extra))
            .unwrap_or_default();
        thinking.set_text(cx, &thinking_label);
        thinking.set_trailing(cx, "chevron-down");
        thinking.set_tooltip(cx, &t("agentControls.hints.thinking"));
        let inline = trigger.widget(id!(thinking_inline));
        set_optional(cx, &inline, if self.compact && !thinking_options.is_empty() { &thinking_label } else { "" });
        let visible_features = if self.compact { Vec::new() } else { self.visible_features() };
        let features = view.wa_repeat(id!(features));
        features.set_count(cx, visible_features.len());
        for (index, feature) in visible_features.iter().enumerate() {
            let button = features.item(cx, index).as_wa_button();
            let label = str_or(feature, "label", str_of(feature, "id")).to_owned();
            let value = model::feature_value(&self.config, feature);
            let tint = controls_model::feature_icon_tint(feature, value).map(|[r, g, b, a]| vec4(r, g, b, a));
            button.set_icon(cx, controls_model::feature_icon(feature));
            button.set_icon_color(cx, tint);
            let tooltip = str_or(feature, "tooltip", &label).to_owned();
            if str_of(feature, "type") == "toggle" || str_of(feature, "desktopTrigger") == "icon" {
                button.set_text(cx, "");
                let option = array_of(feature, "options").iter().find(|o| Some(str_of(o, "id")) == value.as_str());
                let hint = match option {
                    Some(option) if str_of(feature, "type") != "toggle" => format!("{label}: {}", str_or(option, "label", str_of(option, "id"))),
                    _ => tooltip,
                };
                button.set_tooltip(cx, &hint);
            } else {
                let option = array_of(feature, "options").iter().find(|o| Some(str_of(o, "id")) == value.as_str());
                button.set_text(cx, option.map(|o| str_or(o, "label", str_of(o, "id"))).unwrap_or(&label));
                button.set_tooltip(cx, &tooltip);
            }
        }
        let submit = view.wa_button(id!(submit));
        let creating = self.busy || (pending_intent && !self.opened);
        submit.set_busy(cx, creating);
        submit.set_text(cx, &if self.with_agent || creating { String::new() } else { t("newWorkspace.create") });
        submit.set_icon(cx, if self.with_agent { "corner-down-left" } else { "" });
        submit.set_enabled(cx, self.can_create());
        submit.set_tooltip(cx, &t("newWorkspace.create"));
        let open_terminal = view.wa_button(id!(open_terminal));
        open_terminal.set_busy(cx, terminal && creating);
        open_terminal.set_text(cx, &if creating { String::new() } else { t("newWorkspace.launch.openTerminal") });
        open_terminal.set_enabled(cx, self.can_create());
        set_optional(cx, &view.widget(id!(error)), self.error.as_deref().or(self.profiles_error.as_deref()).unwrap_or(""));
        let import = view.wa_button(id!(import_session));
        import.set_visible(cx, !self.compact);
        import.set_text(cx, &t("importSession.title"));
        import.set_pill(cx, !self.compact);
        let toggle = view.wa_button(id!(options_toggle));
        toggle.set_visible(cx, add_project);
        toggle.set_text(cx, l("Workspace options"));
        toggle.set_trailing(cx, if self.options_open { "chevron-up" } else { "chevron-down" });
        toggle.set_pill(cx, !self.compact);
        view.widget(id!(bottom)).set_visible(cx, !self.compact || add_project);
        // Options.
        let options = view.widget(id!(options));
        let show_options = add_project && self.options_open;
        options.set_visible(cx, show_options);
        if show_options {
            self.present_options(cx);
        }
        let unlocked = !(self.busy || pending_intent);
        for path in [id!(mode), id!(thinking)] {
            view.wa_button(path).set_enabled(cx, unlocked);
        }
    }

    fn present_options(&mut self, cx: &mut Cx) {
        let view = self.root();
        let existing_row = view.widget(id!(existing_row));
        existing_row.set_visible(cx, !self.target.workspace_id.is_empty());
        existing_row.widget(id!(label)).set_text(cx, l("Use this existing workspace"));
        view.pswitch(id!(existing)).set_value(cx, self.existing);
        let source_field = view.widget(id!(source_field));
        source_field.set_visible(cx, !self.existing);
        source_field.widget(id!(label)).set_text(cx, &t("newWorkspace.isolation.label"));
        let source_label = match self.source.as_str() {
            "branch-off" => l("New worktree: branch off"),
            "checkout" => l("New worktree: existing branch"),
            "pr" => l("New worktree: pull / merge request"),
            "clone" => l("Clone GitHub repository"),
            "new-directory" => l("Create remote directory"),
            _ => l("Existing checkout or directory"),
        };
        view.wa_button(id!(source)).set_text(cx, source_label);
        let source_icon = match self.source.as_str() {
            "branch-off" | "checkout" => "git-branch",
            "pr" => "git-pull-request",
            "clone" => "download",
            "new-directory" => "folder-plus",
            _ => "folder",
        };
        view.wa_button(id!(source)).set_icon(cx, source_icon);
        let directory_label = match self.source.as_str() {
            "new-directory" => l("Parent directory"),
            "clone" => l("Clone target directory"),
            _ => l("Remote directory"),
        };
        view.widget(id!(directory_field.label)).set_text(cx, directory_label);
        view.wa_input(id!(path)).set_is_read_only(cx, self.existing);
        let browse = view.wa_button(id!(browse));
        browse.set_enabled(cx, !self.existing);
        browse.set_tooltip(cx, l("Browse remote directories"));
        let show_ref = !self.existing && matches!(self.source.as_str(), "branch-off" | "checkout");
        view.widget(id!(ref_field)).set_visible(cx, show_ref);
        view.widget(id!(ref_field.label)).set_text(cx, &if self.source == "checkout" { l("Branch to check out").to_owned() } else { t("newWorkspace.refPicker.startingRef") });
        let branch_field = view.widget(id!(branch_name_field));
        branch_field.set_visible(cx, !self.existing && self.source == "branch-off");
        branch_field.widget(id!(label)).set_text(cx, l("New branch name"));
        set_optional(cx, &branch_field.widget(id!(hint)), l("Automatic if empty"));
        let pr = !self.existing && self.source == "pr";
        view.widget(id!(pr_field)).set_visible(cx, pr);
        view.widget(id!(pr_field.label)).set_text(cx, l("Pull / merge request number"));
        let forge_field = view.widget(id!(forge_field));
        forge_field.set_visible(cx, pr);
        forge_field.widget(id!(label)).set_text(cx, l("Forge"));
        set_optional(cx, &forge_field.widget(id!(hint)), l("Optional; the host detects the repository"));
        let path_field = view.widget(id!(project_path_field));
        path_field.set_visible(cx, pr);
        path_field.widget(id!(label)).set_text(cx, l("Forge project path"));
        set_optional(cx, &path_field.widget(id!(hint)), l("Optional"));
        let clone = !self.existing && self.source == "clone";
        view.widget(id!(repo_field)).set_visible(cx, clone);
        view.widget(id!(repo_field.label)).set_text(cx, l("Repository or search query"));
        view.widget(id!(protocol_field)).set_visible(cx, clone);
        view.widget(id!(protocol_field.label)).set_text(cx, l("Clone protocol"));
        let segment = |value: &str, label: &str| Segment { value: value.into(), label: label.into(), icon: None, enabled: true };
        view.psegmented(id!(protocol)).set_options(cx, vec![segment("https", "HTTPS"), segment("ssh", "SSH")], &self.clone_protocol);
        view.wa_button(id!(ref_field.search)).set_tooltip(cx, &t("newWorkspace.refPicker.searchPlaceholder"));
        view.wa_button(id!(repo_field.search)).set_tooltip(cx, l("Search GitHub repositories"));
        let new_dir = !self.existing && self.source == "new-directory";
        view.widget(id!(dirname_field)).set_visible(cx, new_dir);
        view.widget(id!(dirname_field.label)).set_text(cx, l("New directory name"));
        let prepare_visible = clone || new_dir;
        set_optional(
            cx,
            &view.widget(id!(unknown)),
            if prepare_visible && self.preparation_unknown {
                l("The directory operation has an unknown outcome. Browse the target and select the existing directory if it completed; do not blindly repeat the operation.")
            } else {
                ""
            },
        );
        view.widget(id!(prepare_row)).set_visible(cx, prepare_visible);
        let prepare = view.wa_button(id!(prepare));
        prepare.set_text(cx, if clone { l("Clone repository") } else { l("Create directory") });
        prepare.set_busy(cx, self.busy);
        prepare.set_enabled(cx, !self.preparation_unknown);
        let inspect = view.wa_button(id!(inspect_prepared));
        inspect.set_visible(cx, self.preparation_unknown);
        inspect.set_text(cx, l("Inspect and select prepared directory"));
        view.widget(id!(title_field.label)).set_text(cx, &t("newWorkspace.titlePlaceholder"));
        let agent_row = view.widget(id!(agent_row));
        agent_row.set_visible(cx, !self.existing);
        agent_row.widget(id!(label)).set_text(cx, &t("newWorkspace.launch.chat"));
        view.pswitch(id!(with_agent)).set_value(cx, self.with_agent);
        let profile_field = view.widget(id!(profile_field));
        profile_field.set_visible(cx, self.with_agent);
        profile_field.widget(id!(label)).set_text(cx, l("Agent profile"));
        let profile_label = self
            .profile_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .and_then(|id| self.profiles.iter().find(|p| str_of(p, "id") == id))
            .map(|p| str_of(p, "name").to_owned())
            .unwrap_or_else(|| l("Custom configuration").to_owned());
        view.wa_button(id!(profile)).set_text(cx, &profile_label);
        let add_row = view.widget(id!(add_project_row));
        add_row.set_visible(cx, !self.existing && self.source == "directory");
        let add = view.wa_button(id!(add_project));
        add.set_text(cx, l("Add project without a workspace"));
        add.set_enabled(cx, !self.busy && self.intent.is_none());
    }
}

/// Reads and encodes picked images off the GUI thread (`{"ok", "payload": {"images"}}`).
fn encode_images(files: Vec<(std::path::PathBuf, String)>) -> Value {
    let mut images = Vec::with_capacity(files.len());
    for (path, mime) in files {
        match std::fs::read(&path) {
            Ok(bytes) => images.push(json!({
                "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
                "mimeType": attachments::sniff_image_mime(&bytes).unwrap_or(mime.as_str()),
            })),
            Err(error) => return json!({ "ok": false, "error": { "message": format!("{}: {error}", path.display()) } }),
        }
    }
    json!({ "ok": true, "payload": { "images": images } })
}

/// A local file about to upload: its path, name, MIME type and size.
struct UploadFile {
    path: std::path::PathBuf,
    name: String,
    mime: String,
    size: u64,
}

/// Inspects dropped files off the GUI thread (`{"ok", "payload": {"files": [{"path", "name",
/// "size"}]}}`).
fn stat_files(paths: Vec<std::path::PathBuf>) -> Value {
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned());
        match std::fs::metadata(&path) {
            Ok(metadata) => files.push(json!({ "path": path.to_string_lossy(), "name": name, "size": metadata.len() })),
            Err(error) => return json!({ "ok": false, "error": { "message": format!("{name}: {error}") } }),
        }
    }
    json!({ "ok": true, "payload": { "files": files } })
}

/// Forgets a pending timeout; its event is then ignored.
fn forget_timer(timer: &mut Timer) {
    *timer = Timer::default();
}

/// The per-badge width cap at which badges of `naturals` widths fit
/// `available` side by side, each also at most `max`: short badges keep their
/// natural width and the longer ones share what remains equally.
fn shared_cap(naturals: &[f64], available: f64, max: f64) -> f64 {
    let mut widths: Vec<f64> = naturals.iter().map(|w| w.min(max)).collect();
    widths.sort_by(f64::total_cmp);
    let mut left = available.max(0.0);
    for (i, width) in widths.iter().enumerate() {
        let share = left / (widths.len() - i) as f64;
        if *width > share {
            return share;
        }
        left -= width;
    }
    max
}

#[cfg(test)]
mod shared_cap_tests {
    use super::shared_cap;

    #[test]
    fn fitting_badges_keep_the_maximum() {
        assert_eq!(shared_cap(&[100.0, 120.0], 400.0, 240.0), 240.0);
    }

    #[test]
    fn short_badges_keep_their_width_and_long_ones_share_the_rest() {
        // 80 fits; the two 240px badges split the remaining 320px.
        assert_eq!(shared_cap(&[300.0, 80.0, 240.0], 400.0, 240.0), 160.0);
        assert_eq!(shared_cap(&[200.0, 200.0], -10.0, 240.0), 0.0);
    }
}

impl Widget for WaNewTabView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
            let owner = overlay::widget_owner(self.widget_uid());
            for (id, answer) in overlay::take_answers(&owner) {
                if self.attachment_menu == Some(id) {
                    self.attachment_menu = None;
                    if let Some(value) = answer.get("value").and_then(Value::as_str) {
                        self.handle_pick(cx, "attach", value.to_owned(), ctx);
                    }
                }
            }
            // file-drop-zone.tsx: files dropped anywhere on the screen go to the composer card.
            let drop_ready = self.accepts_file_drop();
            if let Some(paths) = self.file_drop.handle_event(cx, event, drop_ready) {
                self.drop_files(ctx, paths);
                self.view.redraw(cx);
            }
            if self.directory_timer.is_event(event).is_some() {
                self.directory_timer = Timer::default();
                self.catalog_cwd = self.path.trim().to_owned();
                self.load_default_ref(ctx);
                self.load_providers(ctx);
                self.sync_forge(cx, ctx);
                self.view.redraw(cx);
            }
            if self.forge_timer.is_event(event).is_some() {
                self.forge_timer = Timer::default();
                let effects = self.forge_auto.timer_fired();
                self.apply_forge(cx, ctx, effects);
                self.view.redraw(cx);
            }
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        // Size from this allocation, not the previous content-sized root rectangle.
        let width = cx.peek_walk_turtle(walk).size.x;
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.present(cx, ctx, width);
        }
        let step = self.view.draw_walk(cx, scope, walk);
        // Align after measurement: a form taller than its viewport must start at the
        // top, while a fitting compact form is bottom-docked.
        let view = self.root();
        let viewport = view.widget(id!(scroll)).area().rect(cx).size.y;
        let content = view.widget(id!(column)).area().rect(cx).size.y + 60.0;
        let align_y = if viewport > 0.0 && content > viewport { 0.0 } else if self.compact { 1.0 } else { 0.5 };
        if self.scroll_align != Some(align_y) {
            self.scroll_align = Some(align_y);
            view.widget(id!(scroll)).apply_over(cx, live! { align: { y: (align_y) } });
            cx.redraw_area_in_draw(self.view.area());
        }
        if self.focus_prompt {
            self.focus_prompt = false;
            if self.with_agent && !self.terminal() {
                self.view.wa_input(id!(prompt)).set_key_focus(cx);
            }
        }
        // The drop backdrop covers the whole screen.
        let zone = self.view.area().rect(cx);
        self.file_drop.draw(cx, zone);
        step
    }
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use crate::shell::layout::Layout;
    use paseo_gui_core::{CoreConfig, GuiCore};
    use std::{path::PathBuf, sync::Arc};

    struct Harness {
        cx: Cx,
        ctx: UiContext,
        view: WaNewTabView,
        layout: Layout,
        owner: String,
        sibling: String,
        sibling_pane: String,
        core: Option<GuiCore>,
        dir: PathBuf,
    }

    impl Harness {
        fn new(agent: bool, terminal: bool) -> Self {
            let dir = std::env::temp_dir().join(format!("paseo-creation-{}", new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let core = GuiCore::start(
                CoreConfig { state_dir: dir.join("state"), daemon_home: Some(dir.join("daemon")), default_endpoint: None },
                Arc::new(|| {}),
            ).unwrap();
            let mut cx = Cx::new(Box::new(|_, _| {}));
            let mut view = WaNewTabView::new(&mut cx);
            let creation = PanelTarget::new("new_tab", "owner-host", "", "/project").with_id("creation");
            let owner = creation.key();
            let mut layout = Layout::default();
            layout.open(creation, false);
            let sibling = PanelTarget::agent("other-host", "other-workspace", "/other", "existing", "");
            let sibling_key = sibling.key();
            layout.open(sibling.clone(), true);
            let sibling_pane = layout.focused_pane.clone();
            let mut ctx = UiContext::new(core.handle());
            ctx.window_id = "creation-window".into();
            // The retained form must use its own identity, not current selection.
            ctx.tab_key = Some(sibling_key.clone());
            ctx.target = Some(sibling);
            view.target = NewTabTarget { host_id: "owner-host".into(), cwd: "/project".into(), tab_key: owner.clone(), ..Default::default() };
            view.path = "/project".into();
            view.prompt = "Keep my prompt".into();
            view.title = "Keep my title".into();
            view.config = json!({ "provider": "kiro", "model": "selected-model" });
            view.intent = Some(json!({
                "kind": "workspace",
                "request": { "idempotencyKey": "receipt", "source": { "kind": "directory", "cwd": "/project" } }
            }));
            if agent {
                view.intent.as_mut().unwrap()["request"]["agent"] = json!({ "prompt": view.prompt });
            }
            if terminal {
                view.intent.as_mut().unwrap()["launch"] = json!("terminal");
            }
            Self { cx, ctx, view, layout, owner, sibling: sibling_key, sibling_pane, core: Some(core), dir }
        }

        fn receipt(agent: bool) -> Value {
            let mut receipt = json!({
                "idempotencyKey": "receipt", "phase": "completed", "revision": 3,
                "workspaceId": "created-workspace",
                "workspace": { "id": "created-workspace", "workspaceDirectory": "/created", "projectRootPath": "/project", "name": "Created workspace" }
            });
            if agent {
                receipt["agentId"] = json!("created-agent");
                receipt["agent"] = json!({
                    "id": "created-agent", "workspaceId": "created-workspace", "cwd": "/created",
                    "title": "Created session", "project": { "projectRootPath": "/project" }
                });
            }
            receipt
        }

        fn response(receipt: &Value) -> Value {
            json!({ "creation": receipt, "workspace": receipt["workspace"], "agent": receipt["agent"] })
        }

        fn apply(&mut self) -> usize {
            let mut replacements = 0;
            for effect in std::mem::take(&mut self.ctx.effects) {
                match effect {
                    UiEffect::ReplaceTab { tab_key, target } => {
                        self.layout.replace_tab(&tab_key, target);
                        replacements += 1;
                    }
                    UiEffect::SetPreference { .. } | UiEffect::Toast { .. } => {}
                    _ => panic!("creation must replace its owner, never open another destination"),
                }
            }
            replacements
        }

        fn assert_destination(&self, kind: &str) {
            assert!(self.layout.target(&self.owner).is_none());
            let pane = self.layout.panes().into_iter().find(|pane| pane.id != self.sibling_pane).unwrap();
            assert_eq!(pane.tabs.len(), 1);
            assert_eq!(pane.selected.as_ref(), pane.tabs.first());
            let destination = self.layout.target(&pane.tabs[0]).unwrap();
            assert_eq!(destination.kind, kind);
            assert_eq!(destination.server_id, "owner-host");
            assert_eq!(destination.workspace_id, "created-workspace");
            assert_eq!(destination.cwd, "/created");
            assert_eq!(self.layout.tabs.len(), 2);
            assert_eq!(self.layout.focused_pane, self.sibling_pane);
            assert_eq!(self.layout.selected().unwrap().key(), self.sibling);
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            drop(self.core.take());
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    #[test]
    fn creation_completion_replaces_owner_once_for_receipt_and_response_in_either_order() {
        for agent in [true, false] {
            for receipt_first in [true, false] {
                let mut h = Harness::new(agent, false);
                let receipt = Harness::receipt(agent);
                let response = Harness::response(&receipt);
                let other_window = h.layout.clone();
                let other_window_before = other_window.to_json();
                if receipt_first {
                    h.view.receive(&mut h.ctx, &receipt);
                }
                h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Ok(response.clone()));
                h.view.receive(&mut h.ctx, &receipt);
                h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Ok(response));
                assert_eq!(h.apply(), 1);
                h.assert_destination(if agent { "agent" } else { "setup" });
                assert!(h.view.intent.is_none());
                assert_eq!(other_window.to_json(), other_window_before);
            }
        }
    }

    #[test]
    fn creation_completion_keeps_pending_failed_and_incomplete_receipts_recoverable() {
        let mut h = Harness::new(true, false);
        let before = h.layout.to_json();
        let intent = h.view.intent.clone();
        for phase in ["pending", "failed", "completed"] {
            let mut receipt = Harness::receipt(false);
            receipt["phase"] = json!(phase);
            h.view.receive(&mut h.ctx, &receipt);
            h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Ok(Harness::response(&receipt)));
            assert_eq!(h.apply(), 0);
            assert_eq!(h.layout.to_json(), before);
            assert_eq!(h.view.intent, intent);
            assert_eq!(h.view.prompt, "Keep my prompt");
            assert_eq!(h.view.title, "Keep my title");
            assert_eq!(h.view.config["model"], "selected-model");
        }
        h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Err(RpcError::new("Disconnected")));
        assert_eq!(h.apply(), 0);
        assert_eq!(h.view.intent, intent);
        assert_eq!(h.view.error.as_deref(), Some("Disconnected"));
        assert!(!h.view.busy);
        h.view.receive(&mut h.ctx, &Harness::receipt(true));
        assert_eq!(h.apply(), 1);
        h.assert_destination("agent");
    }

    #[test]
    fn creation_completion_waits_for_terminal_and_preserves_failed_handoff_for_retry() {
        let mut h = Harness::new(false, true);
        let receipt = Harness::receipt(false);
        let response = Harness::response(&receipt);
        h.view.receive(&mut h.ctx, &receipt);
        h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Ok(response.clone()));
        assert!(h.view.busy);
        assert_eq!(h.apply(), 0);
        assert!(h.layout.target(&h.owner).is_some());
        let terminal_request = || Req::CreateTerminal { workspace_id: "created-workspace".into(), cwd: "/created".into() };
        h.view.answer(&mut h.cx, &mut h.ctx, terminal_request(), Err(RpcError::new("Missing shell")));
        assert!(!h.view.busy);
        assert!(h.view.terminal_workspace.is_some());
        assert_eq!(h.view.prompt, "Keep my prompt");
        h.view.answer(&mut h.cx, &mut h.ctx, Req::Submit, Ok(response));
        assert_eq!(h.apply(), 0);
        assert!(!h.view.busy);
        h.view.create_terminal(&h.ctx, "created-workspace".into(), "/created".into());
        assert!(h.view.busy);
        let terminal = json!({ "terminal": { "id": "terminal", "workspaceId": "created-workspace", "cwd": "/created" } });
        h.view.answer(&mut h.cx, &mut h.ctx, terminal_request(), Ok(terminal.clone()));
        h.view.answer(&mut h.cx, &mut h.ctx, terminal_request(), Ok(terminal));
        assert_eq!(h.apply(), 1);
        h.assert_destination("terminal");
    }

    #[test]
    fn creation_completion_host_switch_replaces_only_owner_and_carries_draft() {
        let mut h = Harness::new(false, false);
        h.view.handle_pick(&mut h.cx, "host", "chosen-host".into(), &mut h.ctx);
        assert_eq!(h.apply(), 0);
        assert!(h.layout.target(&h.owner).is_some());
        h.view.intent = None;
        h.view.handle_pick(&mut h.cx, "host", "chosen-host".into(), &mut h.ctx);
        assert_eq!(h.apply(), 1);
        let pane = h.layout.panes().into_iter().find(|pane| pane.id != h.sibling_pane).unwrap();
        let target = h.layout.target(pane.selected.as_deref().unwrap()).unwrap();
        assert_eq!(target.kind, "new_tab");
        assert_eq!(target.server_id, "chosen-host");
        assert_eq!(target.data["creationDraft"]["prompt"], "Keep my prompt");
        assert_eq!(target.data["creationDraft"]["title"], "Keep my title");
        assert_eq!(target.data["creationDraft"]["config"]["model"], "selected-model");
        assert_eq!(h.layout.tabs.len(), 2);
        assert!(h.layout.target(&h.owner).is_none());
        assert_eq!(h.layout.selected().unwrap().key(), h.sibling);
    }
}
