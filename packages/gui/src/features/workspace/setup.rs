//! Workspace setup (Flutter `features/workspace/setup_panel.dart`, original
//! `panels/setup-panel.tsx`): the owned `workspace.setup.subscribe` observation, the blocked
//! notice for untrusted PR sources, setup command rows with their read-only logs, the retry
//! action, and the workspace scripts with run/stop, their read-only terminal and service URL.

use std::collections::HashSet;

use makepad_widgets::*;
use serde_json::{json, Value};

use super::i18n::{l, t, ta};
use super::kit::{clicked_in, set_alert, set_optional, tapped_in, WaButtonWidgetExt, WaButtonWidgetRefExt, WaIconWidgetRefExt, WaInputWidgetRefExt, WaRepeatWidgetExt, WaRepeatWidgetRefExt};
use super::observe::{ObsEvent, Observation};
use super::rpc::{array_of, int_of, object_of, payload, require_success, str_of, Pending, RpcError};
use super::scripts;
use crate::common::UiContext;
use paseo_gui_core::CoreEvent;

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;

    // Read-only, selectable mono text (setup logs, 12px).
    pub WaCodeBlock = <WaBareInput> {
        mono: true
        input = {
            padding: 12.
            is_read_only: true
            draw_text: { text_style: <PASEO_FONT_MONO> { line_spacing: 1.6 } }
        }
    }

    // Original `SetupCommandRow`: 8px bordered item, surface1 header, surface0 detail.
    WaSetupCommand = <Surface> {
        width: Fill, height: Fit, flow: Down
        role: Transparent, border_role: Border, radius: 8.
        padding: 1.
        header = <View> {
            width: Fill, height: Fit, cursor: Hand
            fill = <Surface> {
                width: Fill, height: Fit, flow: Right, spacing: 8.
                padding: { left: 12., right: 12., top: 8., bottom: 8. }
                align: { y: 0.5 }
                role: Surface1, radius: 7.
                status_slot = <View> {
                    width: 18., height: 18., align: { x: 0.5, y: 0.5 }, flow: Overlay
                    spinner = <View> { width: 14., height: 14., visible: false, <WaSpinner> { width: 14., height: 14., stroke: 2., color_role: Small } }
                    glyph = <View> { width: Fit, height: Fit, icon = <WaIcon> { size: 14. } }
                }
                command = <WaLine> {}
                duration = <View> { width: Fit, height: Fit, text = <WaSmallFit> {} }
                chevron = <WaIcon> { size: 14., icon: "chevron-right" }
            }
        }
        detail = <Surface> {
            width: Fill, height: Fit, flow: Down, visible: false
            role: Surface0
            rule = <WaRule> {}
            log_scroll = <ScrollYView> {
                width: Fill, height: 120.
                log = <WaCodeBlock> {}
            }
            no_output = <View> {
                width: Fill, height: Fit, padding: 12., visible: false
                text = <WaMuted> {}
            }
            error = <WaErrorCard> { visible: false }
        }
    }

    WaScriptRow = <WaRow> {
        controls = {
            toggle = <WaButtonGhost> {}
            view_log = <WaButtonGhost> { icon: "square-terminal" }
            open_service = <WaButtonGhost> { icon: "external-link" }
        }
    }

    pub WaSetupView = {{WaSetupView}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <Surface> {
            width: Fill, height: Fill, flow: Overlay
            role: Surface0
            scroll = <ScrollYView> {
                width: Fill, height: Fill, flow: Down, spacing: 8.
                padding: 16.
                load_error = <WaAlert> {
                    visible: false
                    padding: { left: 24., right: 24., top: 16., bottom: 16. }
                    body = { actions = { load_retry = <WaButtonOutlineSmall> {} } }
                }
                blocked = <WaWarningAlert> {
                    visible: false
                    body = { actions = { run_blocked = <WaButtonPrimarySmall> {} } }
                }
                waiting = <View> {
                    width: Fill, height: 240., flow: Down, spacing: 12., visible: false
                    align: { x: 0.5, y: 0.5 }
                    <WaSpinner> { width: 36., height: 36. }
                    text = <WaMuted> { width: Fit }
                }
                empty = <View> {
                    width: Fill, height: 240., visible: false
                    align: { x: 0.5, y: 0.5 }
                    text = <WaMuted> { width: Fit }
                }
                commands = <WaRepeat> { spacing: 8., template: <WaSetupCommand> {} }
                detail_log = <Surface> {
                    width: Fill, height: Fit, visible: false
                    role: Surface1, border_role: Border, radius: 8.
                    log = <WaCodeBlock> {}
                }
                snapshot_error = <WaErrorCard> { visible: false }
                truncated = <View> { width: Fill, height: Fit, visible: false, text = <WaSmall> {} }
                retry_row = <View> { width: Fill, height: Fit, visible: false, retry_setup = <WaButtonOutline> { icon: "rotate-cw" } }
                run_error = <WaAlert> { visible: false }
                scripts_section = <View> {
                    width: Fill, height: Fit, flow: Down, visible: false
                    padding: { top: 24. }
                    title = <WaSectionTitle> {}
                    card = <WaCard> {
                        scripts = <WaRepeat> { template: <WaScriptRow> {} }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Req {
    Scripts,
    Run,
    Script,
}

#[derive(Live, LiveHook, Widget)]
pub struct WaSetupView {
    #[deref]
    view: View,
    #[rust]
    host_id: String,
    #[rust]
    workspace_id: String,
    #[rust]
    cwd: String,
    #[rust]
    started: bool,
    #[rust]
    observation: Option<Observation>,
    #[rust]
    snapshot: Option<Value>,
    #[rust]
    scripts: Vec<Value>,
    #[rust]
    error: Option<String>,
    #[rust]
    loading: bool,
    #[rust]
    busy: bool,
    #[rust]
    pending: Pending<Req>,
    /// `useExpandedSetupCommands`: opened rows and auto-expanded rows the user collapsed.
    #[rust]
    expanded: HashSet<i64>,
    #[rust]
    collapsed: HashSet<i64>,
}

/// `resolveAutoExpandIndex`: the running command, else the last one.
fn auto_expand_index(commands: &[Value]) -> Option<i64> {
    commands
        .iter()
        .find(|c| str_of(c, "status") == "running")
        .or_else(|| commands.last())
        .map(|c| int_of(c, "index"))
}

/// `formatDuration`.
pub fn format_duration(ms: i64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms / 1000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// `processCarriageReturns`: progress output keeps only the last CR segment of each line.
pub fn process_log(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    text.split('\n').map(|line| line.rsplit('\r').next().unwrap_or("")).collect::<Vec<_>>().join("\n")
}

impl WaSetupView {
    /// The content root as a handle (the deref `View` is not `Clone`).
    fn root(&self) -> WidgetRef {
        self.view.widget(id!(wa_root))
    }

    pub fn set_workspace(&mut self, cx: &mut Cx, host_id: &str, workspace_id: &str, cwd: &str) {
        if self.host_id == host_id && self.workspace_id == workspace_id {
            return;
        }
        self.host_id = host_id.to_owned();
        self.workspace_id = workspace_id.to_owned();
        self.cwd = cwd.to_owned();
        self.started = false;
        self.observation = None;
        self.snapshot = None;
        self.loading = true;
        self.error = None;
        self.scripts.clear();
        self.expanded.clear();
        self.collapsed.clear();
        self.view.redraw(cx);
    }

    fn load(&mut self, ctx: &mut UiContext) {
        self.started = true;
        self.loading = true;
        self.error = None;
        self.observation = Some(Observation::start(
            &ctx.core,
            &self.host_id,
            "workspace.setup.subscribe.request",
            "workspace.setup.subscribe.response",
            json!({ "workspaceId": self.workspace_id }),
        ));
    }

    fn load_scripts(&mut self, ctx: &UiContext) {
        self.pending.send(&ctx.core, &self.host_id, "workspace.script.list.request", json!({ "workspaceId": self.workspace_id }), Req::Scripts);
    }

    /// `_receive`.
    fn receive(&mut self, message: &Value, restored: bool, ctx: &UiContext) {
        let body = payload(message);
        if let Some(id) = body.get("workspaceId").and_then(Value::as_str) && id != self.workspace_id {
            return;
        }
        if let Some(snapshot) = body.get("snapshot") {
            self.snapshot = if snapshot.is_null() { None } else { Some(snapshot.clone()) };
        } else if body.get("status").is_some() {
            self.snapshot = Some(body.clone());
        }
        if restored {
            self.load_scripts(ctx);
        }
    }

    fn handle_core(&mut self, cx: &mut Cx, ctx: &mut UiContext) {
        if self.host_id.is_empty() {
            return;
        }
        if !self.started {
            self.load(ctx);
            self.view.redraw(cx);
        }
        let mut changed = false;
        if let Some(observation) = self.observation.as_mut() {
            let items = observation.take(&ctx.events);
            for item in items {
                changed = true;
                match item {
                    ObsEvent::Snapshot { payload, restored } => {
                        let first = self.loading;
                        self.loading = false;
                        self.receive(&payload, restored, ctx);
                        if first {
                            self.load_scripts(ctx);
                        }
                    }
                    ObsEvent::Event(message) => self.receive(&message, false, ctx),
                    ObsEvent::Failed(error) => {
                        self.loading = false;
                        self.error = Some(error.to_string());
                        self.observation = None;
                    }
                    ObsEvent::RestoreFailed(error) => {
                        self.error = Some(if error.is_empty() { l("Setup observation failed.").to_owned() } else { error });
                    }
                }
            }
        }
        for event in &ctx.events {
            if let CoreEvent::HostEvent { host_id, event } = event && *host_id == self.host_id && str_of(event, "type") == "script_status_update" {
                let body = payload(event);
                if str_of(body, "workspaceId") == self.workspace_id {
                    self.scripts = array_of(body, "scripts").to_vec();
                    changed = true;
                }
            }
        }
        for (req, result) in self.pending.take(&ctx.events) {
            changed = true;
            match (req, result) {
                (Req::Scripts, Ok(body)) => match require_success(&body) {
                    Ok(()) => self.scripts = array_of(&body, "scripts").to_vec(),
                    Err(error) => self.error = Some(error.to_string()),
                },
                (Req::Scripts, Err(error)) => self.error = Some(error.to_string()),
                (Req::Run, result) => {
                    self.busy = false;
                    let outcome = result.and_then(|body| {
                        require_success(&body)?;
                        if body.get("started") != Some(&Value::Bool(true)) {
                            return Err(RpcError::new(l("Setup did not start. Refresh its status before retrying.")));
                        }
                        Ok(())
                    });
                    if let Err(error) = outcome {
                        self.error = Some(error.to_string());
                    }
                }
                (Req::Script, result) => {
                    self.busy = false;
                    match result.and_then(|body| require_success(&body)) {
                        Ok(()) => self.load_scripts(ctx),
                        Err(error) => self.error = Some(error.to_string()),
                    }
                }
            }
        }
        if changed {
            self.view.redraw(cx);
        }
    }

    fn run_setup(&mut self, ctx: &UiContext) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.error = None;
        self.pending.send(&ctx.core, &self.host_id, "workspace.setup.run.request", json!({ "workspaceId": self.workspace_id }), Req::Run);
    }

    fn commands(&self) -> Vec<Value> {
        self.snapshot
            .as_ref()
            .map(|s| array_of(object_of(s, "detail"), "commands").to_vec())
            .unwrap_or_default()
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions, ctx: &mut UiContext) {
        if self.view.wa_button(id!(load_retry)).clicked(actions) && !self.loading {
            self.load(ctx);
            self.view.redraw(cx);
        }
        if self.view.wa_button(id!(run_blocked)).clicked(actions) || self.view.wa_button(id!(retry_setup)).clicked(actions) {
            self.run_setup(ctx);
            self.view.redraw(cx);
        }
        let commands = self.commands();
        let auto = auto_expand_index(&commands);
        let repeat = self.view.wa_repeat(id!(commands));
        if let Some(position) = tapped_in(&repeat, id!(header), actions) && let Some(command) = commands.get(position) {
            let index = int_of(command, "index");
            let auto_expanded = Some(index) == auto && !self.collapsed.contains(&index);
            if self.expandable(command) {
                if self.expanded.contains(&index) || auto_expanded {
                    self.expanded.remove(&index);
                    if auto_expanded {
                        self.collapsed.insert(index);
                    }
                } else {
                    self.expanded.insert(index);
                    self.collapsed.remove(&index);
                }
                self.view.redraw(cx);
            }
        }
        let scripts_list = self.view.wa_repeat(id!(scripts));
        if let Some(index) = clicked_in(&scripts_list, id!(toggle), actions) && let Some(script) = self.scripts.get(index).cloned() && !self.busy {
            self.busy = true;
            self.error = None;
            scripts::toggle(&mut self.pending, &ctx.core, &self.host_id, &self.workspace_id, &script, Req::Script);
            self.view.redraw(cx);
        }
        if let Some(index) = clicked_in(&scripts_list, id!(view_log), actions) && let Some(effect) = self.scripts.get(index).and_then(|s| scripts::log_tab(&self.host_id, &self.workspace_id, &self.cwd, s)) {
            ctx.effects.push(effect);
        }
        if let Some(index) = clicked_in(&scripts_list, id!(open_service), actions) && let Some(script) = self.scripts.get(index) && let Err(error) = scripts::open_service(&mut ctx.effects, scripts::service_url(script)) {
            self.error = Some(error);
            self.view.redraw(cx);
        }
    }

    fn expandable(&self, command: &Value) -> bool {
        let status = str_of(command, "status");
        let snapshot_error = self.snapshot.as_ref().map(|s| str_of(s, "error")).unwrap_or("");
        let has_log = !str_of(command, "log").trim().is_empty();
        let has_error = status == "failed" && !snapshot_error.is_empty();
        status != "running" || has_log || has_error
    }

    fn run_error_text(&self) -> String {
        match &self.error {
            Some(error) if !error.trim().is_empty() => error.clone(),
            _ => t("workspace.setup.blocked.runFailed"),
        }
    }

    fn present(&mut self, cx: &mut Cx) {
        let view = self.root();
        let snapshot = self.snapshot.clone();
        let status = snapshot.as_ref().map(|s| str_of(s, "status").to_owned()).unwrap_or_default();
        let detail = snapshot.as_ref().map(|s| object_of(s, "detail").clone()).unwrap_or(Value::Null);
        let commands = self.commands();
        let snapshot_error = snapshot.as_ref().map(|s| str_of(s, "error").to_owned()).unwrap_or_default();
        // A successful observation explicitly carries `snapshot: null` when there is no
        // setup. Only the pending subscription (or a running setup) is a waiting state.
        let waiting = !self.started || self.loading || (status == "running" && commands.is_empty());
        let load_failed = self.error.is_some() && snapshot.is_none();
        let blocked = !load_failed && status == "blocked" && snapshot.as_ref().is_some_and(|s| s.get("blockedSource").is_some_and(|b| !b.is_null()));
        let empty = !load_failed && !blocked && !waiting && (snapshot.is_none() || (status == "completed" && commands.is_empty()));
        let list = !load_failed && !blocked && !waiting && !empty;

        let alert = view.widget(id!(load_error));
        alert.set_visible(cx, load_failed);
        if load_failed {
            set_alert(cx, &alert, self.error.as_deref().unwrap_or(""), "");
            let retry = view.wa_button(id!(load_retry));
            retry.set_text(cx, &t("common.actions.retry"));
            retry.set_enabled(cx, !self.loading);
        }
        let blocked_view = view.widget(id!(blocked));
        blocked_view.set_visible(cx, blocked);
        if blocked {
            let repository = snapshot
                .as_ref()
                .map(|s| str_of(object_of(s, "blockedSource"), "headRepository").to_owned())
                .unwrap_or_default();
            set_alert(
                cx,
                &blocked_view,
                &t("workspace.setup.blocked.title"),
                &ta("workspace.setup.blocked.description", &[("repository", &repository)]),
            );
            let run = view.wa_button(id!(run_blocked));
            run.set_text(cx, &t("workspace.setup.blocked.run"));
            run.set_enabled(cx, !self.busy);
        }
        let waiting_view = view.widget(id!(waiting));
        waiting_view.set_visible(cx, !load_failed && !blocked && waiting);
        waiting_view.widget(id!(text)).set_text(cx, &t("workspace.setup.waiting"));
        let empty_view = view.widget(id!(empty));
        empty_view.set_visible(cx, empty);
        empty_view.widget(id!(text)).set_text(cx, &t("workspace.setup.empty.noCommands"));

        let repeat = view.wa_repeat(id!(commands));
        repeat.set_count(cx, if list { commands.len() } else { 0 });
        let auto = auto_expand_index(&commands);
        if list {
            for (position, command) in commands.iter().enumerate() {
                let item = repeat.item(cx, position);
                let index = int_of(command, "index");
                let status = str_of(command, "status");
                let log = str_of(command, "log");
                let has_log = !log.trim().is_empty();
                let has_error = status == "failed" && !snapshot_error.is_empty();
                let auto_expanded = Some(index) == auto && !self.collapsed.contains(&index);
                let open = self.expanded.contains(&index) || auto_expanded;
                item.widget(id!(spinner)).set_visible(cx, status == "running");
                let glyph = item.widget(id!(glyph));
                glyph.set_visible(cx, status != "running");
                // Original green-500 / red-500 status icons (theme independent).
                let (icon, color) = match status {
                    "completed" => ("circle-check", crate::theme::argb(0xff22c55e)),
                    _ => ("circle-alert", crate::theme::argb(0xffef4444)),
                };
                let status_icon = glyph.wa_icon(id!(icon));
                status_icon.set_icon(cx, icon);
                status_icon.set_color(cx, Some(color));
                item.widget(id!(command)).set_text(cx, str_of(command, "command"));
                let duration = command.get("durationMs").filter(|v| !v.is_null()).map(|_| format_duration(int_of(command, "durationMs")));
                set_optional(cx, &item.widget(id!(duration)), duration.as_deref().unwrap_or(""));
                // Original `chevronExpanded`: an instant 90° turn.
                item.wa_icon(id!(chevron)).set_icon(cx, if open { "chevron-down" } else { "chevron-right" });
                let detail_view = item.widget(id!(detail));
                detail_view.set_visible(cx, open);
                if open {
                    let text = process_log(log);
                    item.widget(id!(log_scroll)).set_visible(cx, has_log);
                    if has_log {
                        super::kit::sync_input(cx, &item.wa_input(id!(log)), &text);
                        let height = (text.lines().count() as f64 * 20.0 + 24.0).clamp(40.0, 400.0);
                        item.widget(id!(log_scroll)).apply_over(cx, live! { height: (height) });
                    }
                    let no_output = item.widget(id!(no_output));
                    no_output.set_visible(cx, !has_log);
                    no_output.widget(id!(text)).set_text(cx, &t("workspace.setup.log.noOutput"));
                    super::kit::set_error_card(cx, &item.widget(id!(detail.error)), if has_error { &snapshot_error } else { "" });
                }
            }
        }
        let detail_text = str_of(&detail, "log");
        let show_detail_log = list && commands.is_empty() && !detail_text.is_empty();
        let detail_log = view.widget(id!(detail_log));
        detail_log.set_visible(cx, show_detail_log);
        if show_detail_log {
            super::kit::sync_input(cx, &detail_log.wa_input(id!(log)), &process_log(detail_text));
        }
        let show_snapshot_error = list && !snapshot_error.is_empty() && !commands.iter().any(|c| str_of(c, "status") == "failed");
        super::kit::set_error_card(cx, &view.widget(id!(snapshot_error)), if show_snapshot_error { &snapshot_error } else { "" });
        let truncated = list && detail.get("truncated") == Some(&Value::Bool(true));
        set_optional(cx, &view.widget(id!(truncated)), if truncated { l("Earlier setup output was truncated by the host.") } else { "" });
        let retry_row = view.widget(id!(retry_row));
        retry_row.set_visible(cx, list && status == "failed");
        let retry = view.wa_button(id!(retry_setup));
        retry.set_text(cx, &t("common.actions.retry"));
        retry.set_busy(cx, self.busy);
        let run_error = view.widget(id!(run_error));
        let show_run_error = !load_failed && self.error.is_some() && (blocked || list);
        run_error.set_visible(cx, show_run_error);
        if show_run_error {
            set_alert(cx, &run_error, "", &self.run_error_text());
        }

        let section = view.widget(id!(scripts_section));
        section.set_visible(cx, !self.scripts.is_empty());
        section.widget(id!(title)).set_text(cx, &t("workspace.scripts.title"));
        let scripts_list = view.wa_repeat(id!(scripts));
        scripts_list.set_count(cx, self.scripts.len());
        let compact = crate::common::is_compact_layout(cx);
        for (index, script) in self.scripts.iter().enumerate() {
            let row = scripts_list.item(cx, index);
            // Keep script metadata above the actions on narrow panes; long localized
            // action labels can wrap without consuming the hint's available width.
            if compact {
                row.apply_over(cx, live! {
                    flow: Down, align: { y: 0.0 }
                    controls = { width: Fill, flow: RightWrap }
                });
            } else {
                row.apply_over(cx, live! {
                    flow: Right, align: { y: 0.5 }
                    controls = { width: Fit, flow: Right }
                });
            }
            row.widget(id!(body.label)).set_text(cx, str_of(script, "scriptName"));
            set_optional(cx, &row.widget(id!(body.hint)), &scripts::script_hint(script));
            let toggle = row.wa_button(id!(toggle));
            let running = scripts::is_running(script);
            let label = if running { t("workspace.scripts.actions.stop") } else { t("workspace.scripts.actions.run") };
            toggle.set_text(cx, &label);
            toggle.set_icon(cx, if running { "square" } else { "play" });
            toggle.set_enabled(cx, !self.busy);
            let view_log = row.wa_button(id!(view_log));
            view_log.set_visible(cx, !str_of(script, "terminalId").is_empty());
            view_log.set_text(cx, &t("workspace.scripts.actions.view"));
            let open = row.wa_button(id!(open_service));
            open.set_visible(cx, !scripts::service_url(script).is_empty());
            open.set_text(cx, &t("workspace.scripts.actions.openService"));
        }
    }
}

impl Widget for WaSetupView {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        if let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_core(cx, ctx);
        }
        self.view.handle_event(cx, event, scope);
        if let Event::Actions(actions) = event && let Some(ctx) = scope.data.get_mut::<UiContext>() {
            self.handle_actions(cx, actions, ctx);
        }
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        self.present(cx);
        self.view.draw_walk(cx, scope, walk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_and_carriage_returns() {
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(61_000), "1m 1s");
        assert_eq!(process_log("a 10%\ra 100%\nb"), "a 100%\nb");
    }

    #[test]
    fn auto_expand_prefers_running() {
        let commands = vec![json!({"index": 0, "status": "completed"}), json!({"index": 1, "status": "running"}), json!({"index": 2, "status": "failed"})];
        assert_eq!(auto_expand_index(&commands), Some(1));
        assert_eq!(auto_expand_index(&commands[2..]), Some(2));
    }
}
