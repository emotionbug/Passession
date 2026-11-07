//! ToolCallDetailsContent painter (tool_call_details.dart): full-bleed shell /
//! sub-agent / diff blocks, bordered scroll areas for read/write/search/fetch,
//! plain text sections, Paseo MCP Prompt/Details/Result sections, input/output
//! JSON groups, the error section and the loading skeleton. Inline details are
//! capped (400; 200 in overview groups and permission cards) and scroll inside
//! the cap; long lines never wrap and scroll sideways.

use super::canvas::*;
use super::highlight;
use super::i18n::t;
use super::markdown::view::code_rich;
use super::paint::Draw;
use super::tools::{DetailBlock, DiffKind};
use makepad_widgets::*;
use std::rc::Rc;

fn code_height(d: &Draw) -> f64 {
    d.look.typo.system_size(18.0) / d.look.code()
}

/// A clipped, scrollable viewport over content of `content` size drawn by
/// `paint(cx, d, origin)`. Returns the viewport height.
#[allow(clippy::too_many_arguments)]
fn scrolled(cx: &mut Cx2d, d: &mut Draw, key: &str, x: f64, y: f64, w: f64, content: DVec2, pad: Padding, max_h: Option<f64>, paint: impl FnOnce(&mut Cx2d, &mut Draw, DVec2)) -> f64 {
    let inner_w = (w - pad.left - pad.right).max(1.0);
    let full_h = content.y + pad.top + pad.bottom;
    let h = max_h.map(|m| full_h.min(m)).unwrap_or(full_h);
    let max_x = (content.x - inner_w).max(0.0);
    let max_y = (full_h - h).max(0.0);
    let kx = format!("{key}:x");
    let ky = format!("{key}:y");
    d.state.extent_x.insert(kx.clone(), max_x);
    d.state.extent_y.insert(ky.clone(), max_y);
    let sx = d.state.scroll_x.get(&kx).copied().unwrap_or(0.0).clamp(0.0, max_x);
    let sy = d.state.scroll_y.get(&ky).copied().unwrap_or(0.0).clamp(0.0, max_y);
    let view = rect_xywh(x, y, w, h);
    cx.begin_turtle(Walk { abs_pos: Some(view.pos), margin: Margin::default(), width: Size::Fixed(w), height: Size::Fixed(h.max(1.0)) }, Layout::default());
    d.canvas.push_clip(view);
    paint(cx, d, dvec2(x + pad.left - sx, y + pad.top - sy));
    d.canvas.pop_clip();
    cx.end_turtle();
    if max_x > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollX(kx));
    }
    if max_y > 0.0 {
        d.canvas.add_hit(view, HitTarget::ScrollY(ky));
        // Scroll thumb.
        let thumb = (h * h / full_h).max(16.0);
        let ty = y + (h - thumb) * (sy / max_y);
        d.kit.rect(cx, rect_xywh(x + w - 5.0, ty, 3.0, thumb), with_alpha(d.look.palette.muted, 0.5), 1.5);
    }
    h
}

fn mono(cx: &mut Cx2d, d: &mut Draw, text: &str, color: Vec4) -> Rc<makepad_widgets::makepad_draw::text::layouter::LaidoutText> {
    let size = d.look.code();
    let spans = [RichSpan { len: text.len(), font: Font::Mono, size_px: size, color }];
    d.kit.layout_rich(cx, text, &spans, None, code_height(d))
}

fn pad(l: f64, t: f64, r: f64, b: f64) -> Padding {
    Padding { left: l, top: t, right: r, bottom: b }
}

/// Draws the detail blocks; returns the height. `fill` stretches the first
/// shell/edit/write/read/sub_agent block to the sheet height.
#[allow(clippy::too_many_arguments)]
pub fn draw_details(cx: &mut Cx2d, d: &mut Draw, blocks: &[DetailBlock], full_bleed: bool, skeleton: bool, key: &str, x: f64, y: f64, w: f64, max_h: Option<f64>, fill: Option<f64>) -> f64 {
    let p = d.look.palette;
    if blocks.is_empty() {
        if skeleton {
            // _LoadingSkeleton: three 12px surface3 pills (100/72/48%), 8 apart.
            let mut cy = y + 12.0;
            for factor in [1.0, 0.72, 0.48] {
                d.kit.rect(cx, rect_xywh(x + 12.0, cy, (w - 24.0) * factor, 12.0), p.surface3, 6.0);
                cy += 20.0;
            }
            return cy - 8.0 + 12.0 - y;
        }
        let size = d.look.ui(14.0);
        let empty = t("toolCallDetails.empty");
        let laid = d.kit.layout(cx, Font::Italic, size, 1.4, &empty, Some(w), 0.0);
        d.kit.draw_laidout(cx, Font::Italic, p.muted, dvec2(x, y), &laid);
        return laidout_size(&laid).y;
    }
    let gap = if full_bleed { 8.0 } else { 16.0 };
    let mut cy = y;
    for (index, block) in blocks.iter().enumerate() {
        if index > 0 {
            cy += gap;
        }
        let block_key = format!("{key}:{index}");
        let min_h = if index == 0 { fill } else { None };
        cy += draw_block(cx, d, block, &block_key, x, cy, w, max_h, min_h);
    }
    cy - y
}

#[allow(clippy::too_many_arguments)]
fn draw_block(cx: &mut Cx2d, d: &mut Draw, block: &DetailBlock, key: &str, x: f64, y: f64, w: f64, max_h: Option<f64>, min_h: Option<f64>) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    match block {
        DetailBlock::Shell { command, output } => {
            let text = if output.is_empty() { format!("$ {command}") } else { format!("$ {command}\n\n{output}") };
            let size = d.look.code();
            let spans = [
                RichSpan { len: 2, font: Font::Mono, size_px: size, color: p.muted },
                RichSpan { len: text.len() - 2, font: Font::Mono, size_px: size, color: p.foreground },
            ];
            let laid = d.kit.layout_rich(cx, &text, &spans, None, code_height(d));
            let content = laidout_size(&laid);
            let h_full = content.y + 36.0;
            let h = max_h.map(|m| h_full.min(m)).unwrap_or(h_full).max(min_h.unwrap_or(0.0));
            d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface1, 0.0);
            scrolled(cx, d, key, x, y, w, content, pad(12.0, 12.0, 28.0, 24.0), Some(h), |cx, d, origin| {
                d.kit.draw_rich(cx, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            })
            .max(h)
        }
        DetailBlock::SubAgentLog { session, actions, log, fallback } => {
            let size = d.look.code();
            let lh = d.look.typo.system_size(18.0);
            let mut lines: Vec<Vec<(String, Vec4)>> = Vec::new();
            if let Some(session) = session {
                lines.push(vec![(format!("session {session}"), p.muted)]);
            }
            for (tool, summary) in actions {
                let mut line = vec![(tool.clone(), p.muted)];
                if let Some(summary) = summary {
                    line.push((format!("  {summary}"), p.foreground));
                }
                lines.push(line);
            }
            let body = if !log.is_empty() { log.clone() } else if actions.is_empty() { fallback.clone() } else { String::new() };
            let mut text = String::new();
            let mut spans = Vec::new();
            for line in &lines {
                if !text.is_empty() {
                    text.push('\n');
                    spans.push(RichSpan { len: 1, font: Font::Mono, size_px: size, color: p.foreground });
                }
                for (part, color) in line {
                    text.push_str(part);
                    spans.push(RichSpan { len: part.len(), font: Font::Mono, size_px: size, color: *color });
                }
            }
            if !body.is_empty() {
                if !text.is_empty() {
                    text.push_str("\n\n");
                    spans.push(RichSpan { len: 2, font: Font::Mono, size_px: size, color: p.foreground });
                }
                text.push_str(&body);
                spans.push(RichSpan { len: body.len(), font: Font::Mono, size_px: size, color: p.foreground });
            }
            let laid = d.kit.layout_rich(cx, &text, &spans, None, lh / size);
            let content = laidout_size(&laid);
            let h_full = content.y + 36.0;
            let h = max_h.map(|m| h_full.min(m)).unwrap_or(h_full).max(min_h.unwrap_or(0.0));
            d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface1, 0.0);
            scrolled(cx, d, key, x, y, w, content, pad(12.0, 12.0, 28.0, 24.0), Some(h), |cx, d, origin| {
                d.kit.draw_rich(cx, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            })
            .max(h)
        }
        DetailBlock::Diff(lines) => {
            if lines.is_empty() {
                let empty = t("diffViewer.empty");
                let size = d.kit.measure(cx, Font::Regular, base, 1.4, &empty, None);
                d.kit.label(cx, Font::Regular, base, 1.4, p.muted, dvec2(x + (w - size.x) * 0.5, y + 16.0), &empty, None);
                return size.y + 32.0;
            }
            let laid: Vec<_> = lines
                .iter()
                .map(|(kind, content)| {
                    let color = if matches!(kind, DiffKind::Header | DiffKind::Context) { p.muted } else { p.foreground };
                    (*kind, mono(cx, d, if content.is_empty() { " " } else { content }, color))
                })
                .collect();
            let row_h = d.look.code() * 1.35 + 8.0;
            let width = laid.iter().map(|(_, l)| laidout_size(l).x).fold(0.0, f64::max);
            let content = dvec2(width, row_h * laid.len() as f64);
            let h_full = content.y + 36.0;
            let h = max_h.map(|m| h_full.min(m)).unwrap_or(h_full).max(min_h.unwrap_or(0.0));
            d.kit.rect(cx, rect_xywh(x, y, w, h), p.surface1, 0.0);
            scrolled(cx, d, key, x, y, w, content, pad(12.0, 12.0, 28.0, 24.0), Some(h), |cx, d, origin| {
                let row_w = (w - 40.0).max(width);
                for (i, (kind, line)) in laid.iter().enumerate() {
                    let ry = origin.y + row_h * i as f64;
                    let bg = match kind {
                        DiffKind::Add => Some(rgba(0x262ea043)),
                        DiffKind::Remove => Some(rgba(0x1af85149)),
                        _ => None,
                    };
                    if let Some(bg) = bg {
                        d.kit.rect(cx, rect_xywh(origin.x, ry, row_w, row_h), bg, 0.0);
                    }
                    let pos = dvec2(origin.x, ry + 4.0);
                    d.kit.draw_rich(cx, pos, line);
                    d.canvas.add_run(line.clone(), pos, Some('\n'));
                }
            })
            .max(h)
        }
        DetailBlock::Source { content, path, start_line } => {
            if !d.state.fence_highlights.contains_key(key) {
                d.state.fence_highlights.insert(key.to_owned(), Default::default());
            }
            let colors = highlight::file(d.state.fence_highlights.get_mut(key).unwrap(), content, path, d.look.syntax.as_str(), d.look.dark());
            let (text, spans) = code_rich(content, colors.as_ref(), p.foreground, d.look.code());
            let laid = d.kit.layout_rich(cx, &text, &spans, None, code_height(d));
            let mut gutter: Option<(Rc<_>, f64)> = None;
            if let Some(start) = start_line {
                let count = content.matches('\n').count() as i64 + 1;
                let last = start + count - 1;
                let digits = last.to_string().len().max(2);
                let numbers = (0..count).map(|i| format!("{:>digits$} ", start + i)).collect::<Vec<_>>().join("\n");
                let g = mono(cx, d, &numbers, with_alpha(p.muted, 0.6));
                let gw = laidout_size(&g).x;
                gutter = Some((g, gw));
            }
            let gw = gutter.as_ref().map(|g| g.1).unwrap_or(0.0);
            let size = laidout_size(&laid);
            let content_size = dvec2(size.x + gw, size.y);
            let h_full = content_size.y + 24.0;
            let h = max_h.map(|m| h_full.min(m)).unwrap_or(h_full).max(min_h.unwrap_or(0.0));
            d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface2, 4.0, p.border, 1.0);
            scrolled(cx, d, key, x, y, w, content_size, pad(12.0, 12.0, 12.0, 12.0), Some(h), |cx, d, origin| {
                if let Some((g, _)) = &gutter {
                    d.kit.draw_rich(cx, origin, g);
                }
                let pos = origin + dvec2(gw, 0.0);
                d.kit.draw_rich(cx, pos, &laid);
                d.canvas.add_run(laid.clone(), pos, Some('\n'));
            })
            .max(h)
        }
        DetailBlock::Code(text) | DetailBlock::JsonBox(text) => {
            let laid = mono(cx, d, text, p.foreground);
            let size = laidout_size(&laid);
            let cap = if matches!(block, DetailBlock::JsonBox(_)) { None } else { max_h };
            let h_full = size.y + 24.0;
            let h = cap.map(|m| h_full.min(m)).unwrap_or(h_full);
            d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface2, 4.0, p.border, 1.0);
            scrolled(cx, d, key, x, y, w, size, pad(12.0, 12.0, 12.0, 12.0), Some(h), |cx, d, origin| {
                d.kit.draw_rich(cx, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            })
        }
        DetailBlock::BareCode(text) => {
            let laid = mono(cx, d, text, p.foreground);
            let size = laidout_size(&laid);
            scrolled(cx, d, key, x, y, w, size, pad(0.0, 0.0, 0.0, 0.0), None, |cx, d, origin| {
                d.kit.draw_rich(cx, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            })
        }
        DetailBlock::PlainText(text) => {
            let laid = d.kit.layout(cx, Font::Regular, base, d.look.typo.system_size(22.0) / base, text, Some((w - 24.0).max(1.0)), 0.0);
            let size = laidout_size(&laid);
            let h_full = size.y + 24.0;
            let h = max_h.map(|m| h_full.min(m)).unwrap_or(h_full);
            d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface2, 4.0, p.border, 1.0);
            scrolled(cx, d, key, x, y, w, dvec2(0.0, size.y), pad(12.0, 12.0, 12.0, 12.0), Some(h), |cx, d, origin| {
                d.kit.draw_laidout(cx, Font::Regular, p.foreground, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            })
        }
        DetailBlock::GroupHeader(title) => {
            let size = d.kit.label(cx, Font::Regular, base, 1.4, p.muted, dvec2(x + 12.0, y + 8.0), title, None);
            let h = size.y + 16.0;
            d.kit.rect(cx, rect_xywh(x, y + h - 1.0, w, 1.0), p.border, 0.0);
            h
        }
        DetailBlock::Paseo(section) => {
            // PaseoDetailSection: 16 inset, 12 gap, bottom border; medium title,
            // content-size prose (1.5) or 120px muted labels beside values.
            let small = d.look.ui(12.0);
            let content = d.look.content();
            let line = (base * 1.5).round();
            let mut cy = y + 16.0;
            let title = d.kit.layout(cx, Font::Medium, base, 1.4, &section.title, Some(w - 32.0), 0.0);
            d.kit.draw_laidout(cx, Font::Medium, p.foreground, dvec2(x + 16.0, cy), &title);
            cy += laidout_size(&title).y + 12.0;
            if let Some(text) = &section.text {
                let laid = d.kit.layout(cx, Font::Regular, content, (content * 1.5).round() / content, text, Some(w - 32.0), 0.0);
                d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 16.0, cy), &laid);
                d.canvas.add_run(laid.clone(), dvec2(x + 16.0, cy), Some('\n'));
                cy += laidout_size(&laid).y;
            } else {
                for (i, (label, value)) in section.fields.iter().enumerate() {
                    if i > 0 {
                        cy += 12.0;
                    }
                    let l = d.kit.layout(cx, Font::Regular, small, line / small, label, Some(120.0), 0.0);
                    d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 16.0, cy), &l);
                    let v = d.kit.layout(cx, Font::Regular, base, line / base, value, Some((w - 32.0 - 136.0).max(1.0)), 0.0);
                    d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 152.0, cy), &v);
                    d.canvas.add_run(v.clone(), dvec2(x + 152.0, cy), Some('\n'));
                    cy += laidout_size(&l).y.max(laidout_size(&v).y);
                }
            }
            cy += 16.0;
            d.kit.rect(cx, rect_xywh(x, cy - 1.0, w, 1.0), p.border, 0.0);
            cy - y
        }
        DetailBlock::Error(text) => {
            let label = t("toolCallDetails.error").to_uppercase();
            let size = d.kit.label(cx, Font::Semibold, d.look.ui(12.0), 1.3, p.destructive, dvec2(x, y), &label, None);
            let by = y + size.y + 8.0;
            let laid = mono(cx, d, text, p.destructive);
            let content = laidout_size(&laid);
            let h = content.y + 24.0;
            d.kit.bordered(cx, rect_xywh(x, by, w, h), p.surface2, 4.0, p.destructive, 1.0);
            scrolled(cx, d, key, x, by, w, content, pad(12.0, 12.0, 12.0, 12.0), None, |cx, d, origin| {
                d.kit.draw_rich(cx, origin, &laid);
                d.canvas.add_run(laid.clone(), origin, Some('\n'));
            });
            size.y + 8.0 + h
        }
    }
}
