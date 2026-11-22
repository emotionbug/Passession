//! Android Sharesheet handoff: choose an exact draft before importing any images.

use std::sync::Arc;

use makepad_widgets::*;
use paseo_gui_core::CoreState;
use paseo_gui_platform::{ImageShareError, SharedImages};
use serde_json::{Value, json};

use crate::common::paint::{Font, rect};
use crate::common::{PanelTarget, TabPlacement, UiContext, json as j, tr, tr_count};
use crate::features::composer::{self, blobs::BlobJob, service};
use crate::features::workspace::WorkspacePanelWidgetRefExt;
use crate::theme::ControlSize;

use super::commands::{close_page, navigation_agent_target, open_tab, toast};
use super::sheets::{Sheet, SheetFlow, SheetOps};
use super::ui::{Ui, Variant};
use super::{PaseoShell, ShellRegion, hosts, overlays, panels};

pub(super) fn receive(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    share: Result<Arc<SharedImages>, ImageShareError>,
) {
    let error = match share {
        Ok(share) => {
            shell.data.pending_image_share = Some(share);
            present_pending(shell, cx);
            return;
        }
        Err(error) => error,
    };
    let key = match error {
        ImageShareError::Invalid => "shell.imageShare.invalid",
        ImageShareError::Unsupported => "shell.imageShare.unsupported",
        ImageShareError::Limits => "shell.imageShare.limits",
        ImageShareError::Unreadable => "shell.imageShare.unreadable",
        ImageShareError::Busy => "shell.imageShare.busy",
    };
    toast(shell, cx, tr(key), true);
}

pub(super) fn present_pending(shell: &mut PaseoShell, cx: &mut Cx) {
    if shell.data.restored && let Some(share) = shell.data.pending_image_share.take() {
        shell.data.overlays.open_sheet(Box::new(ShareSheet {
            share,
            rows: Vec::new(),
            import: None,
            poll_reply: false,
        }));
        shell.invalidate_region(cx, ShellRegion::Overlays);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Destination {
    Conversation { host: String, agent: String },
    NewSession { host: String },
}

impl Destination {
    fn target(&self, state: &CoreState) -> Option<PanelTarget> {
        match self {
            Self::Conversation { host, agent } => {
                let session = hosts::host(state, host)?.agent(agent)?;
                eligible_session(session).then(|| navigation_agent_target(host, session, None))
            }
            Self::NewSession { host } => {
                hosts::host(state, host)?;
                Some(PanelTarget::new("new_tab", host, "", "").with_data(json!({
                    "creationDraft": {"launch": "chat"}
                })))
            }
        }
    }
}

fn eligible_session(agent: &Value) -> bool {
    !j::text(agent, "id").is_empty()
        && !hosts::is_archived(agent)
        && hosts::parent_agent_id(agent).is_empty()
}

struct Row {
    key: String,
    destination: Destination,
    title: String,
    detail: String,
}

type ImportReply = ToUIReceiver<Result<Vec<Value>, String>>;

struct ShareSheet {
    share: Arc<SharedImages>,
    rows: Vec<Row>,
    import: Option<(Destination, ImportReply)>,
    poll_reply: bool,
}

impl Sheet for ShareSheet {
    fn title(&self) -> String {
        tr("shell.imageShare.title")
    }

    fn sync(&mut self, _shell: &PaseoShell, state: &CoreState) {
        self.rows.clear();
        if self.import.is_some() {
            return;
        }
        let all_hosts = hosts::hosts(state);
        let new_session = tr("sessionNavigation.newSession");
        let offline = tr("shell.imageShare.offline");
        for host in &all_hosts {
            let id = host.server_id();
            if id.is_empty() {
                continue;
            }
            let label = host.display_label();
            let mut label = if label.is_empty() { id.to_owned() } else { label };
            if all_hosts.iter().any(|other| {
                other.server_id() != id && crate::common::host_display_name(other.0) == label
            }) {
                label = format!("{label} ({id})");
            }
            if !host.connected() {
                label.push_str(" · ");
                label.push_str(&offline);
            }
            self.rows.push(Row {
                key: format!("share:new:{id}"),
                destination: Destination::NewSession { host: id.into() },
                title: new_session.clone(),
                detail: label.clone(),
            });
            for agent in host.agents().filter(|agent| eligible_session(agent)) {
                let agent_id = j::text(agent, "id");
                let cwd = j::text(agent, "cwd");
                self.rows.push(Row {
                    // Use a length-delimited host/session pair, not the visual position;
                    // refreshes may reorder rows during a gesture.
                    key: format!("share:agent:{}:{id}{agent_id}", id.len()),
                    destination: Destination::Conversation {
                        host: id.into(),
                        agent: agent_id.into(),
                    },
                    title: agent
                        .get("title")
                        .and_then(Value::as_str)
                        .filter(|title| !title.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| tr("sessionNavigation.untitled")),
                    detail: if cwd.is_empty() {
                        label.clone()
                    } else {
                        format!("{label} · {cwd}")
                    },
                });
            }
        }
    }

    fn draw(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut Ui,
        _scope: &mut Scope,
        x: f64,
        y: f64,
        w: f64,
        interactive: bool,
    ) -> f64 {
        let message = if self.import.is_some() {
            tr("shell.imageShare.adding")
        } else if self.rows.is_empty() {
            tr("shell.imageShare.noDestination")
        } else {
            tr_count("shell.imageShare.choose", self.share.files().len() as i64, &[])
        };
        let hint = ui.spec(Font::Regular, 14.0, ui.p.muted);
        let mut top = y + ui.painter.paragraph_height(cx, w, hint, &message) + 12.0;
        ui.painter.paragraph(cx, dvec2(x, y), w, hint, &message);
        let title = ui.spec(Font::Regular, 14.0, ui.p.foreground);
        let detail = ui.spec(Font::Regular, 12.0, ui.p.muted);
        let title_height = ui.painter.line_height(cx, title);
        let detail_height = ui.painter.line_height(cx, detail);
        let row_height = 22.0 + title_height + detail_height;
        for row in &self.rows {
            let bounds = rect(x, top, w, row_height);
            if ui.is_hover(&row.key) || ui.is_pressed(&row.key) {
                ui.fill(cx, bounds, ui.p.surface1, 8.0);
            }
            ui.text(
                cx,
                rect(x + 8.0, top + 10.0, w - 16.0, title_height),
                title,
                &row.title,
            );
            ui.text(
                cx,
                rect(x + 8.0, top + 10.0 + title_height, w - 16.0, detail_height),
                detail,
                &row.detail,
            );
            if interactive {
                ui.control(
                    cx,
                    bounds,
                    &row.key,
                    &format!("{}, {}", row.title, row.detail),
                    AccessibilityRole::ListItem,
                    AccessibilityState::default(),
                );
            }
            top += row_height;
        }
        top - y
    }

    fn footer_height(&self) -> f64 {
        44.0
    }

    fn draw_footer(
        &mut self,
        cx: &mut Cx2d,
        ui: &mut Ui,
        x: f64,
        y: f64,
        w: f64,
        interactive: bool,
    ) {
        ui.button(
            cx,
            rect(x, y, w, 44.0),
            "share:cancel",
            &tr("common.actions.cancel"),
            None,
            Variant::Secondary,
            ControlSize::Sm,
            interactive,
        );
    }

    fn click(&mut self, _cx: &mut Cx, id: &str, ops: &mut SheetOps) -> SheetFlow {
        if id == "share:cancel" {
            return SheetFlow::Close;
        }
        if self.import.is_some() {
            return SheetFlow::Stay;
        }
        let Some(row) = self.rows.iter().find(|row| row.key == id) else {
            return SheetFlow::Stay;
        };
        if row.destination.target(&ops.state).is_none() {
            ops.toast(tr("shell.imageShare.destinationUnavailable"), true);
            return SheetFlow::Stay;
        }
        let destination = row.destination.clone();
        let reply = ImportReply::default();
        let submitted = service::with(|svc| {
            svc.blobs.submit(BlobJob::ImportSharedImages {
                share: self.share.clone(),
                inline: matches!(destination, Destination::NewSession { .. }),
                reply: reply.sender(),
            })
        });
        if !submitted {
            ops.toast(tr("shell.imageShare.unreadable"), true);
            return SheetFlow::Close;
        }
        self.import = Some((destination, reply));
        SheetFlow::Stay
    }

    fn handle_event(&mut self, _cx: &mut Cx, event: &Event, _scope: &mut Scope) -> SheetFlow {
        // Don't let a queued worker reply jump ahead of a Cancel release.
        self.poll_reply = !crate::common::is_user_input(event);
        SheetFlow::Stay
    }

    fn after_event(&mut self, _cx: &mut Cx, ops: &mut SheetOps) -> SheetFlow {
        if !self.poll_reply {
            return SheetFlow::Stay;
        }
        let Some((_, reply)) = self.import.as_ref() else {
            return SheetFlow::Stay;
        };
        let Ok(images) = reply.try_recv() else {
            return SheetFlow::Stay;
        };
        let (destination, _) = self.import.take().unwrap();
        match images {
            Ok(images) => ops.run(move |shell, cx, ui| add_to_draft(shell, cx, ui, destination, images)),
            Err(_) => ops.toast(tr("shell.imageShare.unreadable"), true),
        }
        SheetFlow::Close
    }
}

fn add_to_draft(
    shell: &mut PaseoShell,
    cx: &mut Cx,
    ui: &mut UiContext,
    destination: Destination,
    images: Vec<Value>,
) {
    // Recheck after local IO. A removed host or newly archived session cannot
    // redirect the images to another tab, even if the focus changed meanwhile.
    let Some(mut target) = destination.target(&ui.state) else {
        toast(shell, cx, tr("shell.imageShare.destinationUnavailable"), true);
        return;
    };
    let added = match destination {
        Destination::Conversation { host, agent } => {
            composer::append_shared_images(&host, &target.workspace_id, &agent, images)
        }
        Destination::NewSession { .. } => {
            target = target.with_id(uuid::Uuid::new_v4().to_string());
            if let Some(panel) = panels::mount(cx, &shell.templates, &target)
                && panel.widget.as_workspace_panel().add_shared_images(images)
            {
                shell.data.panels.insert(target.key(), panel);
                true
            } else {
                false
            }
        }
    };
    if !added {
        toast(shell, cx, tr("shell.imageShare.destinationUnavailable"), true);
        return;
    }
    overlays::dismiss_all(shell, cx, ui);
    if shell.data.command_center_open
        && let Some(center) = &shell.data.command_center
    {
        panels::close_command_center(cx, center);
        shell.data.command_center_open = false;
        shell.data.router.reset(cx);
    }
    if shell.data.page.is_some() {
        close_page(shell, cx);
    }
    open_tab(shell, cx, ui, target, TabPlacement::Main);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_share_resolves_the_requested_host_not_a_matching_session_id() {
        use paseo_gui_core::DirectorySnapshot;

        let mut state = CoreState::new(json!({"hosts": [
            {"serverId": "a", "directoryKey": 1, "status": "connected"},
            {"serverId": "b", "directoryKey": 2, "status": "disconnected"}
        ]}));
        for (directory_key, workspace) in [(1, "a-workspace"), (2, "b-workspace")] {
            Arc::make_mut(&mut state.directories).insert(
                directory_key,
                Arc::new(DirectorySnapshot {
                    agents: [Arc::new(json!({
                        "id": "same-id",
                        "workspaceId": workspace,
                        "cwd": "/same-path"
                    }))]
                    .into_iter()
                    .collect(),
                    ..Default::default()
                }),
            );
        }
        let selected = Destination::Conversation {
            host: "b".into(),
            agent: "same-id".into(),
        };
        let target = selected.target(&state).unwrap();
        assert_eq!(target.server_id, "b");
        assert_eq!(target.workspace_id, "b-workspace");
        Arc::make_mut(&mut state.directories).remove(&2);
        assert!(selected.target(&state).is_none());
    }

    #[test]
    fn image_share_excludes_archived_and_child_sessions() {
        assert!(eligible_session(&json!({"id": "independent"})));
        assert!(!eligible_session(&json!({"id": "archived", "archivedAt": "2026-01-01"})));
        assert!(!eligible_session(&json!({"id": "nested", "parentAgentId": "parent"})));
        assert!(!eligible_session(&json!({"id": "nested", "labels": {"paseo.parent-agent-id": "parent"}})));
        assert!(!eligible_session(&json!({})));
    }
}
