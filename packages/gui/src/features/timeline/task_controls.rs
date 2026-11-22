//! Live provider task actions in the Tasks track, separate from transcript todos.

use super::{
    canvas::{Font, HitTarget, rect_xywh},
    i18n::t,
    json::{get, list, str_of},
    paint::Draw,
};
use makepad_widgets::*;
use serde_json::Value;
use std::collections::HashSet;

pub(super) const HIT_PREFIX: &str = "provider-task-stop:";

pub(super) fn offered(agent: &Value) -> &[Value] {
    list(get(agent, "liveProviderTasks"))
}

pub(super) fn has_offer(offers: &[Value], token: &str) -> bool {
    !token.is_empty() && offers.iter().any(|task| get(task, "token") == token)
}

pub(super) fn draw(
    cx: &mut Cx2d,
    d: &mut Draw,
    tasks: &[Value],
    requested: &HashSet<String>,
    rect: Rect,
) -> f64 {
    let p = d.look.palette;
    let size = d.look.ui(14.0);
    let small = d.look.ui(12.0);
    let stop = t("claudeTasks.stop");
    let stopping = t("claudeTasks.stopping");
    let text_width = [stop.as_str(), stopping.as_str()]
        .into_iter()
        .map(|text| d.kit.measure(cx, Font::Regular, small, 1.3, text, None).x)
        .fold(0.0, f64::max);
    let button_width = (text_width + 20.0).max(52.0).min(rect.size.x * 0.5);
    let row_height = rect.size.y.max(if d.look.compact { 44.0 } else { 36.0 });
    let mut y = rect.pos.y;
    for task in tasks {
        let token = str_of(get(task, "token"));
        if token.is_empty() {
            continue;
        }
        let kind = t(match str_of(get(task, "kind")) {
            "local_bash" => "claudeTasks.shell",
            "local_agent" => "claudeTasks.subagent",
            "local_workflow" => "claudeTasks.workflow",
            "monitor_mcp" | "monitor_ws" => "claudeTasks.monitor",
            "mcp_task" => "claudeTasks.mcpTask",
            "in_process_teammate" => "claudeTasks.teammate",
            _ => "claudeTasks.task",
        });
        let description = str_of(get(task, "description")).trim();
        let label = if description.is_empty() {
            kind
        } else {
            format!("{kind}: {description}")
        };
        let button = rect_xywh(
            rect.pos.x + rect.size.x - button_width,
            y + 2.0,
            button_width,
            row_height - 4.0,
        );
        let submitted = requested.contains(token);
        let hit = format!("{HIT_PREFIX}{token}");
        let over = !submitted && d.state.hover_hit.as_deref() == Some(hit.as_str());
        d.kit.label_line(
            cx,
            Font::Regular,
            size,
            1.3,
            p.foreground,
            dvec2(rect.pos.x + 4.0, y + (row_height - size * 1.3) * 0.5),
            &label,
            (rect.size.x - button_width - 12.0).max(1.0),
        );
        d.kit.rect(cx, button, if over { p.surface3 } else { p.surface2 }, 6.0);
        d.kit.label_line(
            cx,
            Font::Regular,
            small,
            1.3,
            if submitted { p.muted } else { p.foreground },
            dvec2(button.pos.x + 10.0, y + (row_height - small * 1.3) * 0.5),
            if submitted { &stopping } else { &stop },
            (button_width - 20.0).max(1.0),
        );
        if !submitted {
            d.canvas.add_hit(button, HitTarget::Chrome(hit));
        }
        y += row_height;
    }
    y - rect.pos.y
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_hit_requires_the_current_offer_not_a_task_name() {
        let snapshot = json!({"liveProviderTasks":[{"token":"new","description":"Worker"}]});
        assert!(has_offer(offered(&snapshot), "new"));
        assert!(!has_offer(offered(&snapshot), "old"));
        assert!(!has_offer(offered(&snapshot), "Worker"));
        assert!(!has_offer(offered(&json!({})), "new"));
    }
}
