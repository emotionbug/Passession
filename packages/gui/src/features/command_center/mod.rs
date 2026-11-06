//! The Command Center (Flutter `ui/command_center.dart`, original
//! `command-center/command-center.tsx`): a top-anchored 640×560 palette (bottom sheet on
//! phones) over actions, workspaces, agents and workspace files. Root `CommandCenterPanel`.

pub mod matching;
pub mod panel;
pub mod results;

use chrono::{DateTime, Local, Utc};
use makepad_widgets::*;

use self::panel::{WaFileIcon, WaMatchText, WaShortcutBadge};
use serde_json::Value;

use crate::features::workspace::i18n::{l, lf};

pub use panel::{CommandCenterAction, CommandCenterPanel, CommandCenterPanelRef, CommandCenterPanelWidgetExt, CommandCenterPanelWidgetRefExt};

/// Nothing beyond this module's DSL; kept so the shell registers every feature the same way.
pub fn register_modules(_cx: &mut Cx) {}

/// RFC 3339 text to ms since epoch.
pub fn parse_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis())
}

/// `describeTimeAgo` (`paseoTimeAgo`): never sub-minute, then the date after a week.
pub fn time_ago(at: Option<i64>, now: DateTime<Utc>) -> String {
    let Some(at) = at else { return String::new() };
    let elapsed = now.timestamp_millis() - at;
    if elapsed < 60_000 {
        return l("just now").to_owned();
    }
    if elapsed < 3_600_000 {
        return lf("{n}m ago", &[("n", &(elapsed / 60_000).to_string())]);
    }
    if elapsed < 86_400_000 {
        return lf("{n}h ago", &[("n", &(elapsed / 3_600_000).to_string())]);
    }
    if elapsed < 7 * 86_400_000 {
        return lf("{n}d ago", &[("n", &(elapsed / 86_400_000).to_string())]);
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let local = DateTime::from_timestamp_millis(at).map(|t| t.with_timezone(&Local));
    local.map(|t| { use chrono::Datelike; format!("{} {}", MONTHS[t.month0() as usize], t.day()) }).unwrap_or_default()
}

/// `shortenPath`: the home directory as `~`.
pub fn shorten(path: &str) -> String {
    crate::features::schedules::model::shorten_path(path)
}

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::widgets::*;
    use crate::common::paint::PaseoPainter;
    use crate::common::provider_icon::*;
    use crate::features::files::kit::DrawFilesImage;
    use crate::features::workspace::kit::*;

    WaShortcutBadge = {{WaShortcutBadge}} {
        width: Fit, height: Fit
        painter: <PaseoPainter> {}
    }
    WaScopeChip = <WaButtonBase> { kind: Chip, icon: "folder", trailing: "x", margin: { right: 4. } }
    WaMatchTextBase = {{WaMatchText}} {
        width: Fill, height: Fit
        painter: <PaseoPainter> {}
    }
    WaFileIconBase = {{WaFileIcon}} {
        width: 16., height: 16.
        image: <DrawFilesImage> {}
    }

    // A 16x20 icon slot: a Lucide glyph, a material file icon, a project square or a provider
    // glyph, with the status mark knocked out at the corner.
    WaIconSlot = <View> {
        width: 16., height: 20., flow: Overlay, align: { x: 0.5, y: 0.5 }
        glyph = <WaIcon> { visible: false }
        file = <WaFileIconBase> { visible: false }
        project = <WaProjectIcon> { visible: false }
        provider_box = <View> { width: Fit, height: Fit, visible: false, provider = <ProviderIcon> { width: 16., height: 16. } }
        mark_box = <View> {
            width: 16., height: 20., visible: false, align: { x: 1.0, y: 1.0 }
            mark = <StatusMark> { width: 8., height: 8. }
        }
    }

    // A section's rule (8px either side) and/or its small muted label (8+16+8).
    WaSectionRow = <View> {
        width: Fill, height: Fit, flow: Down
        divider = <View> { width: Fill, height: Fit, padding: { top: 8., bottom: 8. }, <WaRule> {} }
        title = <View> { width: Fill, height: Fit, padding: { left: 16., right: 16., top: 8., bottom: 8. }, text = <WaSmallLine> {} }
    }

    // Original `ResultRow`: 16px side padding, surface1 while active, hovered or pressed.
    WaResultBase = <Surface> {
        width: Fill, height: Fit, flow: Right, spacing: 12., align: { y: 0.0 }
        padding: { left: 16., right: 16., top: 6., bottom: 6. }
        role: Transparent, hover_role: Surface1, interactive: true
        <WaIconSlot> {}
    }

    WaActionRow = <WaResultBase> {
        align: { y: 0.5 }
        texts = <View> {
            width: Fill, height: Fit, flow: Down
            title = <WaMatchTextBase> {}
            subtitle = <WaMatchTextBase> { size: 12., line_height: 16. }
        }
        // Original `ShortcutBadge`: small muted text on a half-opacity surface3.
        shortcut = <View> {
            width: Fit, height: Fit, visible: false
            badge = <WaShortcutBadge> {}
        }
    }

    WaTwoLineRow = <WaResultBase> {
        texts = <View> {
            width: Fill, height: Fit, flow: Down
            title = <WaMatchTextBase> { line_height: 18. }
            subtitle = <WaMatchTextBase> { size: 12., line_height: 16. }
        }
    }

    WaFileRow = <WaResultBase> {
        texts = <View> {
            width: Fill, height: Fit
            title = <WaMatchTextBase> {}
        }
    }

    pub CommandCenterPanel = {{CommandCenterPanel}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <View> {
            width: Fill, height: Fill, flow: Overlay
            modal = <WaModal> {
                align: { x: 0.5, y: 0.0 }
                content: {
                    panel = <Surface> {
                        width: 640., height: 560., flow: Down
                        margin: { top: 48. }
                        role: Surface0, border_role: Border, radius: 8.
                        header = <View> {
                            width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                            padding: { left: 16., right: 16., top: 12., bottom: 12. }
                            // The Files scope chip: 28px, borderAccent, surface2, 6px radius.
                            scope_chip = <WaScopeChip> { visible: false }
                            query = <WaBareInput> { input = { padding: { top: 4., bottom: 4., left: 0., right: 0. } } }
                            file_spinner = <View> { width: 14., height: 14., visible: false, <WaSpinner> { width: 14., height: 14. } }
                        }
                        <WaRule> {}
                        file_error = <View> {
                            width: Fill, height: Fit, visible: false, flow: Down
                            padding: { left: 16., right: 16., top: 12., bottom: 12. }
                            text = <WaText> {}
                        }
                        file_error_rule = <WaRule> { visible: false }
                        no_matches = <View> {
                            width: Fill, height: Fit, visible: false, align: { x: 0.5 }
                            padding: { left: 16., right: 16., top: 24., bottom: 24. }
                            text = <WaMuted> { width: Fit }
                        }
                        list = <PortalList> {
                            width: Fill, height: Fill
                            SectionRow = <WaSectionRow> {}
                            ActionRow = <WaActionRow> {}
                            TwoLineRow = <WaTwoLineRow> {}
                            FileRow = <WaFileRow> {}
                        }
                    }
                }
            }
        }
    }

}
