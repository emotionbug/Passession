//! The Command Center (Flutter `ui/command_center.dart`, original
//! `command-center/command-center.tsx`): a top-anchored palette up to 640×560 that fits its
//! rows (bottom sheet on phones) over actions, sessions and directory files. Root
//! `CommandCenterPanel`.

pub mod matching;
pub mod panel;
pub mod results;

use chrono::{DateTime, Local, Utc};
use makepad_widgets::*;

use self::panel::{WaFileIcon, WaMatchText, WaShortcutBadge};
use serde_json::Value;

use crate::features::workspace::i18n::{l, lf};

pub use panel::{
    CommandCenterAction, CommandCenterPanel, CommandCenterPanelRef, CommandCenterPanelWidgetExt,
    CommandCenterPanelWidgetRefExt,
};

/// Nothing beyond this module's DSL; kept so the shell registers every feature the same way.
pub fn register_modules(_cx: &mut Cx) {}

/// RFC 3339 text to ms since epoch.
pub fn parse_ms(value: Option<&Value>) -> Option<i64> {
    let text = value?.as_str()?;
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// `describeTimeAgo` (`paseoTimeAgo`): never sub-minute, then the date after a week.
pub fn time_ago(at: Option<i64>, now: DateTime<Utc>) -> String {
    let Some(at) = at else { return String::new() };
    let elapsed = now.timestamp_millis().saturating_sub(at);
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
    let local = DateTime::from_timestamp_millis(at).map(|t| t.with_timezone(&Local));
    local
        .map(|t| {
            use chrono::Datelike;
            crate::common::month_day(t.month0(), t.day())
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crate::common::clock::relative_time_delay;
    use std::time::Duration;

    #[test]
    fn indicator_clock_relative_time_uses_display_granularity() {
        for (elapsed, delay) in [
            (59_999, 1),
            (61_200, 58_800),
            (3_600_001, 3_599_999),
            (86_400_007, 86_399_993),
        ] {
            assert_eq!(
                relative_time_delay(Some(0), elapsed),
                Some(Duration::from_millis(delay))
            );
        }
        assert_eq!(relative_time_delay(Some(0), 7 * 86_400_000), None);
        assert_eq!(relative_time_delay(None, 1), None);
    }
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

    // A 16x20 icon slot: a Lucide glyph, a material file icon or a provider
    // glyph, with the status mark knocked out at the corner.
    WaIconSlot = <View> {
        width: 16., height: 20., flow: Overlay, align: { x: 0.5, y: 0.5 }
        glyph = <WaIcon> { visible: false }
        file = <WaFileIconBase> { visible: false }
        provider_box = <View> { width: Fit, height: Fit, visible: false, provider = <ProviderIcon> { width: 16., height: 16. } }
        mark_box = <View> {
            width: 16., height: 20., visible: false, align: { x: 1.0, y: 1.0 }
            mark = <StatusMark> { width: 8., height: 8. }
        }
    }

    // A section's rule (8px either side) and/or its small muted label on a 32px line
    // (`results::row_height`).
    WaSectionRow = <View> {
        width: Fill, height: Fit, flow: Down
        divider = <View> { width: Fill, height: 17., padding: { top: 8., bottom: 8. }, <WaRule> {} }
        title = <View> { width: Fill, height: 32., align: { y: 0.5 }, padding: { left: 16., right: 16. }, text = <WaSmallLine> {} }
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

    // Desktop: a top-anchored dialog that fits its rows up to a maximum. Phones: a bottom
    // sheet of fixed height above the keyboard. The list's height is set per frame from the
    // fixed row heights; `chrome_top` and `chrome_bottom` are the measured chrome around it.
    pub CommandCenterPanel = {{CommandCenterPanel}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <View> {
            width: Fill, height: Fill, flow: Overlay
            modal = <WaModal> {
                align: { x: 0.5, y: 0.0 }
                content: {
                    // The modal content is an Overlay `Fit` view, which sizes (and clips) to
                    // its child without the child's margin: the offset from the top and the
                    // safe insets are this frame's padding, or the panel's bottom is cut.
                    frame = <View> {
                        width: Fit, height: Fit, flow: Down
                        padding: { top: 48. }
                        panel = <Surface> {
                            width: 640., height: Fit, flow: Down
                            role: Surface0, border_role: Border, radius: 8.
                            chrome_top = <View> {
                                width: Fill, height: Fit, flow: Down
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
                            }
                            list = <PortalList> {
                                width: Fill, height: 0.
                                SectionRow = <WaSectionRow> {}
                                ActionRow = <WaActionRow> {}
                                TwoLineRow = <WaTwoLineRow> {}
                                FileRow = <WaFileRow> {}
                            }
                            chrome_bottom = <View> {
                                width: Fill, height: Fit, flow: Down
                                // Keeps the last row off the rule or the rounded edge.
                                <View> { width: Fill, height: 6. }
                                // Desktop key hints (hidden on phones and compact layouts).
                                footer_rule = <WaRule> { visible: false }
                                footer = <View> {
                                    width: Fill, height: Fit, visible: false
                                    padding: { left: 16., right: 16., top: 8., bottom: 8. }
                                    text = <WaSmallLine> {}
                                }
                            }
                        }
                    }
                }
            }
        }
    }

}
