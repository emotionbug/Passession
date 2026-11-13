//! Schedules and heartbeats (Flutter `features/schedules/`): the aggregated list across
//! connected hosts, the create/edit sheet, row actions and run history. Root
//! `SchedulesPanel` (page route `Route::Schedules`).

pub mod form;
pub mod model;
pub mod panel;
pub mod timezone;

use makepad_widgets::*;

pub use panel::{SchedulesPanel, SchedulesPanelRef, SchedulesPanelWidgetExt, SchedulesPanelWidgetRefExt};

/// Registers the submodule DSL; the shell calls it before the macro-generated
/// `live_design`, which defines the `SchedulesPanel` root.
pub fn register_modules(cx: &mut Cx) {
    form::live_design(cx);
}

live_design! {
    use link::theme::*;
    use link::shaders::*;
    use link::widgets::*;
    use crate::theme::*;
    use crate::common::provider_icon::*;
    use crate::common::widgets::*;
    use crate::features::workspace::kit::*;
    use crate::features::workspace::select::*;
    use crate::features::workspace::dialog::*;
    use crate::features::workspace::setup::WaCodeBlock;
    use crate::features::schedules::form::WaScheduleEditor;

    // Original `ScheduleRow`: 16px row, top rule after the first, 16x20 provider slot, title,
    // muted target and small meta 4px apart, badge and kebab trailing; surface2 hover (wide),
    // surface3 press. Tapping edits.
    WaScheduleRow = <View> {
        width: Fill, height: Fit, flow: Down
        rule = <WaRule> {}
        row = <Surface> {
            width: Fill, height: Fit, flow: Right, spacing: 12., padding: 16., align: { y: 0.5 }
            role: Transparent, hover_role: Surface2, interactive: true
            glyph_slot = <View> {
                width: 16., height: 20., align: { x: 0.5, y: 0.5 }
                provider_box = <View> { width: Fit, height: Fit, provider = <ProviderIcon> { width: 16., height: 16. } }
            }
            texts = <View> {
                width: Fill, height: Fit, flow: Down, spacing: 4.
                title = <WaLine> {}
                target = <WaMutedLine> {}
                meta = <WaSmallLine> {}
            }
            trailing = <View> {
                width: Fit, height: Fit, flow: Right, align: { y: 0.5 }
                badge = <WaBadge> {}
                kebab = <WaMenuButton> { margin: { left: 8. } }
            }
        }
    }

    WaRunRow = <View> {
        width: Fill, height: Fit, flow: Down
        rule = <WaRule> {}
        body = <View> {
            width: Fill, height: Fit, flow: Down, spacing: 8., padding: 16.
            head = <View> {
                width: Fill, height: Fit, flow: Right, spacing: 8., align: { y: 0.5 }
                status = <WaBadge> { xs: true }
                started = <WaSmallLine> {}
                open_session = <WaButtonGhost> { size: Xs, icon: "message-square" }
            }
            output_box = <Surface> {
                width: Fill, height: Fit
                role: Surface2, radius: 6.
                output_scroll = <ScrollYView> {
                    width: Fill, height: 120.
                    output = <WaCodeBlock> { input = { padding: { left: 12., right: 12., top: 8., bottom: 8. } } }
                }
            }
            run_error = <View> { width: Fill, height: Fit, visible: false, text = <WaSmall> {} }
        }
    }

    pub SchedulesPanel = {{SchedulesPanel}} {
        width: Fill, height: Fill, flow: Overlay
        wa_root = <Surface> {
            width: Fill, height: Fill, flow: Overlay
            role: Surface0
            page = <View> {
                width: Fill, height: Fill, flow: Down
                // Original `MenuHeader`: wide `ScreenHeader` (36px bordered row, 12px insets,
                // light base title) or the phone `CompactHeader` (56px row, medium lg title).
                header = <View> {
                    width: Fill, height: 36., flow: Right, spacing: 8., align: { y: 0.5 }
                    padding: { left: 12., right: 12. }
                    menu_toggle = <WaIconButton> { icon: "panel-left", radius: 16. }
                    title = <WaLine> {}
                }
                header_rule = <WaRule> {}
                spinner_view = <View> {
                    width: Fill, height: Fill, align: { x: 0.5, y: 0.5 }
                    <WaSpinner> { width: 36., height: 36., stroke: 3. }
                }
                failed = <View> {
                    width: Fill, height: Fill, flow: Down, spacing: 24., align: { x: 0.5, y: 0.5 }, visible: false
                    text = <WaMuted> { width: Fit }
                    try_again = <WaButtonGhost> { size: Md }
                }
                filters = <View> {
                    width: Fill, height: Fit, flow: Right, spacing: 12., align: { y: 0.5 }, visible: false
                    padding: { left: 24., right: 24., top: 16. }
                    chips = <View> {
                        width: Fill, height: Fit, flow: RightWrap, spacing: 12., align: { y: 0.5 }
                        host_filter = <WaFilterButton> {}
                        status_filter = <PSegmented> {}
                    }
                    new_schedule = <WaButtonOutline> { icon: "plus" }
                }
                body = <ScrollYView> {
                    width: Fill, height: Fill, flow: Down, spacing: 12.
                    padding: { left: 24., right: 24., top: 16., bottom: 24. }
                    // Original `ScheduleHostErrorsBanner`: 1px border, 8px radius, 12px padding.
                    banner = <Surface> {
                        width: Fill, height: Fit, padding: 12., visible: false
                        role: Transparent, border_role: Border, radius: 8.
                        text = <WaError> {}
                    }
                    // Hosts whose schedules are hidden because they are not connected.
                    offline_banner = <Surface> {
                        width: Fill, height: Fit, padding: 12., visible: false
                        role: Transparent, border_role: Border, radius: 8.
                        text = <WaSmall> {}
                    }
                    card = <WaCard> { rows = <WaRepeat> { template: <WaScheduleRow> {} } }
                    empty_state = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 16., align: { x: 0.5 }, visible: false
                        padding: { top: 24., bottom: 24. }
                        <WaIcon> { size: 20., icon: "calendar-clock" }
                        texts = <View> {
                            // The panel caps the column at 420px, narrower on phones.
                            width: Fill, height: Fit, flow: Down, spacing: 8., align: { x: 0.5 }
                            title = <WaText> { align: { x: 0.5 } }
                            description = <WaMuted> { align: { x: 0.5 } }
                            docs = <WaLinkButton> { trailing: "arrow-up-right", height: 30. }
                        }
                        empty_new = <WaButtonOutline> { size: Md, icon: "plus" }
                    }
                    ended_empty = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 12., align: { x: 0.5 }, visible: false
                        padding: { top: 24., bottom: 24. }
                        <WaIcon> { size: 20., icon: "calendar-clock" }
                        text = <WaText> { width: Fit }
                    }
                    // Every host offline: nothing can be listed or created.
                    offline_state = <View> {
                        width: Fill, height: Fit, flow: Down, spacing: 16., align: { x: 0.5 }, visible: false
                        padding: { top: 24., bottom: 24. }
                        <WaIcon> { size: 20., icon: "server-off" }
                        texts = <View> {
                            width: Fill, height: Fit, flow: Down, spacing: 8., align: { x: 0.5 }
                            title = <WaText> { align: { x: 0.5 } }
                            description = <WaMuted> { align: { x: 0.5 } }
                        }
                    }
                }
            }
            runs_modal = <WaModal> {
                scroll_body: runs_scroll
                content: {
                    sheet = <WaSheet> {
                        width: 560.
                        subtitle_row = <View> { width: Fill, height: Fit, padding: { left: 24., right: 24., bottom: 12. }, runs_subtitle = <WaMutedLine> {} }
                        runs_scroll = <ScrollYView> {
                            width: Fill, height: 480., flow: Down
                            padding: { left: 24., right: 24., bottom: 20. }
                            runs_empty = <View> { width: Fill, height: Fit, padding: { top: 24., bottom: 24. }, align: { x: 0.5 }, text = <WaMuted> { width: Fit } }
                            runs_card = <WaCard> { run_rows = <WaRepeat> { template: <WaRunRow> {} } }
                        }
                    }
                }
            }
            editor = <WaScheduleEditor> {}
            select = <WaSelect> {}
            dialog = <WaDialog> {}
        }
    }
}
