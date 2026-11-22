//! Read-only OMP extension text around the native compose box, outside history.
use super::canvas::{ChatKit, Font};
use super::paint::Look;
use makepad_widgets::*;
use serde_json::Value;

pub(super) struct ExtensionDock<'a> {
    ui: &'a Value,
    counts: [usize; 3],
    visible: [usize; 3],
    line_height: f64,
}

#[derive(Clone, Copy)]
enum Band {
    Status = 0,
    Above = 1,
    Below = 2,
}

fn lines(ui: &Value, band: Band) -> impl Iterator<Item = &str> {
    let statuses = ui["statuses"]
        .as_object()
        .filter(|_| matches!(band, Band::Status))
        .into_iter()
        .flat_map(|entries| entries.values())
        .filter_map(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let widgets = ui["widgets"]
        .as_object()
        .into_iter()
        .flat_map(|entries| entries.values())
        .filter(move |widget| match band {
            Band::Status => false,
            Band::Above => widget["placement"] == "aboveEditor",
            Band::Below => widget["placement"] == "belowEditor",
        })
        .flat_map(|widget| widget["lines"].as_array().into_iter().flatten())
        .filter_map(Value::as_str);
    statuses.chain(widgets)
}

fn distribute(counts: [usize; 3], mut budget: usize) -> [usize; 3] {
    let mut visible = [0; 3];
    // Share short viewports across occupied bands rather than hiding an entire
    // placement whenever another one fills the available space first.
    while budget > 0 && visible != counts {
        for (shown, total) in visible.iter_mut().zip(counts) {
            if *shown < total && budget > 0 {
                *shown += 1;
                budget -= 1;
            }
        }
    }
    visible
}

impl<'a> ExtensionDock<'a> {
    pub(super) fn new(agent: &'a Value, viewport: f64, look: Look) -> Option<Self> {
        let ui = agent.pointer("/runtimeInfo/extra/ompExtensionUi")?;
        let line_height = look.ui(13.0) * 1.55;
        let budget = ((viewport / 3.0) / line_height) as usize;
        let counts = [Band::Status, Band::Above, Band::Below].map(|band| lines(ui, band).count());
        let visible = distribute(counts, budget);
        (visible != [0; 3]).then_some(Self {
            ui,
            counts,
            visible,
            line_height,
        })
    }

    pub(super) fn height(&self) -> f64 {
        self.visible.iter().sum::<usize>() as f64 * self.line_height
    }

    pub(super) fn above(&self, cx: &mut Cx2d, kit: &mut ChatKit, look: Look) {
        self.draw(cx, kit, look, Band::Above);
    }

    pub(super) fn below(&self, cx: &mut Cx2d, kit: &mut ChatKit, look: Look) {
        self.draw(cx, kit, look, Band::Below);
        self.draw(cx, kit, look, Band::Status);
    }

    fn draw(&self, cx: &mut Cx2d, kit: &mut ChatKit, look: Look, band: Band) {
        let visible = self.visible[band as usize];
        if visible == 0 {
            return;
        }
        let rect = cx.walk_turtle(Walk {
            abs_pos: None,
            margin: Margin::default(),
            width: Size::Fill,
            height: Size::Fixed(visible as f64 * self.line_height),
        });
        let width = (rect.size.x - 32.0).clamp(0.0, look.typo.content_max_width);
        if width <= 0.0 {
            return;
        }
        let x = rect.pos.x + (rect.size.x - width) * 0.5;
        let overflow = self.counts[band as usize] > visible;
        let (font, color) = match band {
            Band::Status => (Font::Regular, look.palette.muted),
            _ => (Font::Mono, look.palette.foreground),
        };
        for (index, line) in lines(self.ui, band).take(visible).enumerate() {
            let pos = dvec2(x, rect.pos.y + index as f64 * self.line_height);
            let room = if overflow && index + 1 == visible && width >= 20.0 {
                kit.label_line(
                    cx,
                    font,
                    look.ui(13.0),
                    1.3,
                    color,
                    dvec2(x + width - 16.0, pos.y),
                    "…",
                    16.0,
                );
                width - 20.0
            } else {
                width
            };
            if room > 0.0 {
                kit.label_line(cx, font, look.ui(13.0), 1.3, color, pos, line, room);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omp_extension_dock_draws_only_its_snapshot_bands() {
        let ui = json!({"statuses":{"same":"ready"},"widgets":{
            "same":{"placement":"aboveEditor","lines":["top one","top two"]},
            "other":{"placement":"belowEditor","lines":["bottom"]}
        }});
        assert_eq!(lines(&ui, Band::Status).collect::<Vec<_>>(), ["ready"]);
        assert_eq!(lines(&ui, Band::Above).collect::<Vec<_>>(), ["top one", "top two"]);
        assert_eq!(lines(&ui, Band::Below).collect::<Vec<_>>(), ["bottom"]);
        assert_eq!(distribute([1, 2, 1], 3), [1, 1, 1]);
        assert_eq!(distribute([0, 10, 0], 4), [0, 4, 0]);
        assert_eq!(distribute([1, 2, 1], 8), [1, 2, 1]);
    }
}
