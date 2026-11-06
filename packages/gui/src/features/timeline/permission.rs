//! Port of `permission_card.dart` (agent-stream/view.tsx PermissionRequestCard,
//! QuestionFormCard, PlanCard): plain permission cards with the tool detail and
//! server-declared (or deny/accept) actions, plan permissions, and question
//! forms with tabs, single/multi select, a custom answer field, dismiss and
//! next/submit. Responses go through `agent_permission_response` with a 15s
//! deadline; a failed response re-enables the card (the question form also
//! shows its alert).

use super::canvas::*;
use super::details::draw_details;
use super::i18n::t;
use super::json::*;
use super::markdown;
use super::paint::Draw;
use super::tools::{detail_blocks, detail_is_full_bleed};
use makepad_widgets::*;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};

/// question-form-card-core.ts QuestionFormQuestion.
#[derive(Clone, Debug)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<Value>,
    pub multi_select: bool,
    pub allow_other: bool,
    pub allow_empty: bool,
    pub placeholder: Option<String>,
    pub dismiss_label: Option<String>,
}

impl Question {
    pub fn shows_text_input(&self) -> bool {
        self.options.is_empty() || self.allow_other
    }
}

/// parseQuestionFormQuestions: None unless every question is well formed.
pub fn questions(request: &Value) -> Option<Vec<Question>> {
    let raw = get(get(request, "input"), "questions").as_array()?;
    if raw.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for q in raw {
        let question = q.get("question")?.as_str()?;
        let header = q.get("header")?.as_str()?;
        let options = q.get("options")?.as_array()?;
        if options.iter().any(|o| !o.is_object() || !get(o, "label").is_string()) {
            return None;
        }
        out.push(Question {
            question: question.to_owned(),
            header: header.to_owned(),
            options: options.clone(),
            multi_select: is_true(get(q, "multiSelect")),
            allow_other: is_true(get(q, "allowOther")) || is_true(get(q, "isOther")),
            allow_empty: is_true(get(q, "allowEmpty")),
            placeholder: get(q, "placeholder").as_str().map(str::to_owned),
            dismiss_label: get(q, "dismissLabel").as_str().map(str::to_owned),
        });
    }
    Some(out)
}

#[derive(Clone, Debug, Default)]
pub struct CardState {
    pub request_id: String,
    pub selections: HashMap<usize, BTreeSet<usize>>,
    pub other_texts: HashMap<usize, String>,
    pub active_question: usize,
    pub pending: bool,
    pub failed: bool,
    pub responding_action: Option<String>,
    pub plan_expanded: Option<bool>,
}

impl CardState {
    pub fn for_request(request: &Value) -> Self {
        Self { request_id: value_text(get(request, "id")), ..Default::default() }
    }

    pub fn is_answered(&self, q: &Question, index: usize) -> bool {
        if self.selections.get(&index).is_some_and(|s| !s.is_empty()) {
            return true;
        }
        if !q.shows_text_input() {
            return false;
        }
        if self.other_texts.get(&index).is_some_and(|t| !t.trim().is_empty()) {
            return true;
        }
        q.allow_empty
    }

    pub fn build_answers(&self, qs: &[Question]) -> Value {
        let mut answers = serde_json::Map::new();
        for (i, q) in qs.iter().enumerate() {
            let labels: Vec<String> = self
                .selections
                .get(&i)
                .map(|s| s.iter().map(|&o| str_of(get(&q.options[o], "label")).to_owned()).collect())
                .unwrap_or_default();
            let text = self.other_texts.get(&i).map(|t| t.trim().to_owned()).unwrap_or_default();
            if q.shows_text_input() {
                if !text.is_empty() {
                    let value = if q.multi_select {
                        labels.iter().cloned().chain(std::iter::once(text)).collect::<Vec<_>>().join(", ")
                    } else {
                        text
                    };
                    answers.insert(q.header.clone(), json!(value));
                    continue;
                }
                if q.allow_empty && q.options.is_empty() {
                    answers.insert(q.header.clone(), json!(""));
                    continue;
                }
            }
            if !labels.is_empty() {
                answers.insert(q.header.clone(), json!(labels.join(", ")));
            }
        }
        Value::Object(answers)
    }

    /// Option toggle; single-select advances to the next question.
    pub fn toggle(&mut self, q: &Question, index: usize, option: usize, count: usize) -> bool {
        let selected = self.selections.entry(index).or_default();
        if q.multi_select {
            if !selected.remove(&option) {
                selected.insert(option);
            }
        } else if selected.contains(&option) {
            selected.clear();
        } else {
            selected.clear();
            selected.insert(option);
        }
        let mut cleared_other = false;
        // Single-select: an option and a custom answer replace each other.
        if !q.multi_select && self.other_texts.get(&index).is_some_and(|t| !t.is_empty()) {
            self.other_texts.remove(&index);
            cleared_other = true;
        }
        if !q.multi_select && !self.selections[&index].is_empty() {
            self.active_question = (index + 1).min(count.saturating_sub(1));
        }
        cleared_other
    }

    pub fn set_other(&mut self, q: &Question, index: usize, text: &str) {
        self.other_texts.insert(index, text.to_owned());
        if !q.multi_select && !text.is_empty() {
            self.selections.insert(index, BTreeSet::new());
        }
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// view.tsx planMarkdown: metadata.planText, else a string input.plan.
pub fn plan_markdown(r: &Value) -> Option<String> {
    if let Some(text) = non_empty_str(get(get(r, "metadata"), "planText")) {
        return Some(text.to_owned());
    }
    non_empty_str(get(get(r, "input"), "plan")).map(str::to_owned)
}

pub fn title(r: &Value) -> String {
    if get(r, "kind") == "plan" {
        return t("agentStream.permission.plan");
    }
    non_empty_str(get(r, "title"))
        .or_else(|| non_empty_str(get(r, "name")))
        .map(str::to_owned)
        .unwrap_or_else(|| t("agentStream.permission.required"))
}

/// The card's actions: server-declared, else deny + accept/implement.
pub fn actions(r: &Value) -> Vec<Value> {
    let declared = list(get(r, "actions"));
    if !declared.is_empty() {
        return declared.to_vec();
    }
    vec![
        json!({"id": "reject", "label": t("agentStream.permission.deny"), "behavior": "deny", "variant": "danger"}),
        json!({
            "id": "accept",
            "label": t(if get(r, "kind") == "plan" { "agentStream.permission.implement" } else { "agentStream.permission.accept" }),
            "behavior": "allow",
            "variant": "primary",
        }),
    ]
}

pub fn action_response(action: &Value) -> Value {
    let id = value_text(get(action, "id"));
    if get(action, "behavior") == "allow" {
        json!({"behavior": "allow", "selectedActionId": id})
    } else {
        json!({"behavior": "deny", "selectedActionId": id, "message": "Denied by user"})
    }
}

/// The answer field the row draws (a native TextInput) for the active question.
pub struct AnswerField {
    pub rect: Rect,
    pub placeholder: String,
    pub text: String,
    pub enabled: bool,
    pub highlighted: bool,
}

/// _CardButton: 8/12 padding, radius 6, 14px icon + base label 8 apart.
#[allow(clippy::too_many_arguments)]
fn card_button(cx: &mut Cx2d, d: &mut Draw, x: f64, y: f64, w: Option<f64>, label: &str, icon: &str, fg: Vec4, bg: Option<Vec4>, border: Vec4, responding: bool, dimmed: bool, target: HitTarget) -> DVec2 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let text = d.kit.measure(cx, Font::Regular, base, 1.4, label, None);
    let natural = 12.0 + 14.0 + 8.0 + text.x + 12.0;
    let bw = w.unwrap_or(natural);
    let bh = text.y.max(14.0) + 16.0;
    let r = rect_xywh(x, y, bw, bh);
    let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
    let fill = bg.unwrap_or(if hovered { p.surface2 } else { p.surface1 });
    let alpha = if dimmed { 0.5 } else { 1.0 };
    d.kit.bordered(cx, r, with_alpha(fill, fill.w * alpha), 6.0, with_alpha(border, border.w * alpha), 1.0);
    if responding {
        d.kit.spinner(cx, rect_xywh(r.center().x - 8.0, r.center().y - 8.0, 16.0, 16.0), fg, super::paint::now_seconds() % 1.0);
        d.animating = true;
    } else {
        let start = x + (bw - (14.0 + 8.0 + text.x)) * 0.5;
        d.kit.icon(cx, icon, rect_xywh(start, y, 14.0, bh), 14.0, with_alpha(fg, fg.w * alpha));
        d.kit.label(cx, Font::Regular, base, 1.4, with_alpha(fg, fg.w * alpha), dvec2(start + 22.0, y + (bh - text.y) * 0.5), label, None);
    }
    d.canvas.add_hit(r, target);
    dvec2(bw, bh)
}

pub fn hit_key(target: &HitTarget) -> String {
    format!("{target:?}")
}

/// Draws one pending permission (`key` = pendingPermissionKey). Returns the
/// card height and the answer field to place, if any.
#[allow(clippy::too_many_arguments)]
pub fn draw(cx: &mut Cx2d, d: &mut Draw, key: &str, r: &Value, x: f64, y: f64, w: f64, opts: &mut markdown::MdOpts) -> (f64, Option<AnswerField>) {
    if d.state.permission.as_ref().is_none_or(|s| s.request_id != value_text(get(r, "id"))) {
        d.state.permission = Some(CardState::for_request(r));
    }
    if get(r, "kind") == "question" {
        return match questions(r) {
            Some(qs) => question_form(cx, d, key, &qs, x, y, w),
            None => (0.0, None),
        };
    }
    let plan = get(r, "kind") == "plan";
    let plan_text = plan_markdown(r);
    if let (true, Some(text)) = (plan, plan_text.as_deref()) {
        return (plan_card(cx, d, key, &title(r), str_of(get(r, "description")), text, x, y, w, opts, Some(r)), None);
    }
    (12.0 + permission_card(cx, d, key, r, plan_text.as_deref(), x, y + 12.0, w, opts) + 12.0, None)
}

fn footer(cx: &mut Cx2d, d: &mut Draw, key: &str, r: &Value, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let state = d.state.permission.clone().unwrap_or_default();
    let question = t("agentStream.permission.question");
    let size = d.kit.label(cx, Font::Regular, base, 1.4, p.muted, dvec2(x, y + 4.0), &question, Some(w));
    let mut cy = y + size.y + 8.0 + 8.0;
    let mut cx_pos = x;
    let mut row_h: f64 = 0.0;
    for action in actions(r) {
        let id = value_text(get(&action, "id"));
        let fg = if get(&action, "variant") == "primary" { p.foreground } else { p.muted };
        let icon = if get(&action, "behavior") == "allow" { "check" } else { "x" };
        let label = str_of(get(&action, "label")).to_owned();
        let target = HitTarget::PermissionAction { key: key.to_owned(), action: id.clone() };
        let responding = state.responding_action.as_deref() == Some(id.as_str());
        if d.look.compact {
            let s = card_button(cx, d, x, cy, Some(w), &label, icon, fg, None, p.border_accent, responding, state.pending && !responding, target);
            cy += s.y + 8.0;
        } else {
            let text_w = d.kit.measure(cx, Font::Regular, base, 1.4, &label, None).x + 46.0;
            if cx_pos > x && cx_pos + text_w > x + w {
                cx_pos = x;
                cy += row_h + 8.0;
                row_h = 0.0;
            }
            let s = card_button(cx, d, cx_pos, cy, None, &label, icon, fg, None, p.border_accent, responding, state.pending && !responding, target);
            cx_pos += s.x + 8.0;
            row_h = row_h.max(s.y);
        }
    }
    if !d.look.compact {
        cy += row_h;
    } else {
        cy -= 8.0;
    }
    cy - y
}

#[allow(clippy::too_many_arguments)]
fn permission_card(cx: &mut Cx2d, d: &mut Draw, key: &str, r: &Value, plan_text: Option<&str>, x: f64, y: f64, w: f64, opts: &mut markdown::MdOpts) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let paint = |cx: &mut Cx2d, d: &mut Draw, opts: &mut markdown::MdOpts| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        let title = title(r);
        let laid = d.kit.layout(cx, Font::Regular, base, 22.0 / base, &title, Some(inner), 0.0);
        d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 12.0, cy), &laid);
        d.canvas.add_run(laid.clone(), dvec2(x + 12.0, cy), Some('\n'));
        cy += laidout_size(&laid).y + 8.0;
        let description = str_of(get(r, "description"));
        if !description.is_empty() {
            let laid = d.kit.layout(cx, Font::Regular, base, 20.0 / base, description, Some(inner), 0.0);
            d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 12.0, cy), &laid);
            d.canvas.add_run(laid.clone(), dvec2(x + 12.0, cy), Some('\n'));
            cy += laidout_size(&laid).y + 8.0;
        }
        if let Some(text) = plan_text {
            cy += plan_card(cx, d, &format!("{key}:proposed"), &t("agentStream.permission.proposedPlan"), "", text, x + 12.0, cy, inner, opts, None) + 8.0;
        }
        if get(r, "kind") != "plan" {
            let detail = if get(r, "detail").is_object() {
                get(r, "detail").clone()
            } else {
                json!({"type": "unknown", "input": get(r, "input"), "output": null})
            };
            let blocks = detail_blocks(&detail, None, None, &|k| t(k));
            cy += draw_details(cx, d, &blocks, detail_is_full_bleed(&detail), false, &format!("{key}:detail"), x + 12.0, cy, inner, Some(200.0), None) + 8.0;
        }
        cy += footer(cx, d, key, r, x + 12.0, cy, inner);
        cy + 12.0 - y
    };
    let h = super::markdown::view::measured(cx, d, opts, |cx, d, opts| paint(cx, d, opts));
    d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, opts)
}

/// components/plan-card.tsx: collapsible surface1 card with a 16px chevron,
/// base title, muted description, markdown body and optional action footer.
#[allow(clippy::too_many_arguments)]
pub fn plan_card(cx: &mut Cx2d, d: &mut Draw, key: &str, title: &str, description: &str, text: &str, x: f64, y: f64, w: f64, opts: &mut markdown::MdOpts, footer_request: Option<&Value>) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let toggle = format!("{key}:plan");
    let expanded = d.state.is_expanded(&toggle, true);
    let paint = |cx: &mut Cx2d, d: &mut Draw, opts: &mut markdown::MdOpts| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        let laid = d.kit.layout(cx, Font::Regular, base, 22.0 / base, title, Some(inner - 24.0), 0.0);
        let row_h = laidout_size(&laid).y.max(24.0);
        d.kit.icon(cx, if expanded { "chevron-down" } else { "chevron-right" }, rect_xywh(x + 12.0, cy, 16.0, row_h), 16.0, p.muted);
        d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 36.0, cy + (row_h - laidout_size(&laid).y) * 0.5), &laid);
        d.canvas.add_hit(rect_xywh(x + 12.0, cy, inner, row_h), HitTarget::Toggle(toggle.clone()));
        cy += row_h;
        if expanded {
            if !description.is_empty() {
                cy += 8.0;
                let laid = d.kit.layout(cx, Font::Regular, base, 20.0 / base, description, Some(inner), 0.0);
                d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 12.0, cy), &laid);
                cy += laidout_size(&laid).y;
            }
            cy += 8.0;
            cy += markdown::draw(cx, d, opts, text, x + 12.0, cy, inner, false, false);
        }
        if let Some(request) = footer_request {
            cy += 8.0;
            cy += footer(cx, d, key, request, x + 12.0, cy, inner);
        }
        cy + 12.0 - y
    };
    let h = super::markdown::view::measured(cx, d, opts, |cx, d, opts| paint(cx, d, opts));
    d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, opts)
}

fn question_form(cx: &mut Cx2d, d: &mut Draw, key: &str, qs: &[Question], x: f64, y: f64, w: f64) -> (f64, Option<AnswerField>) {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let state = d.state.permission.clone().unwrap_or_default();
    let index = state.active_question.min(qs.len() - 1);
    let q = &qs[index];
    let all_answered = qs.iter().enumerate().all(|(i, q)| state.is_answered(q, i));
    let last = index == qs.len() - 1;
    let primary_disabled = state.pending || if last { !all_answered } else { !state.is_answered(q, index) };
    let dismiss_label = qs
        .iter()
        .filter_map(|q| q.dismiss_label.clone())
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| t("common.actions.dismiss"));
    let selected = state.selections.get(&index).cloned().unwrap_or_default();
    let mut field = None;
    let paint = |cx: &mut Cx2d, d: &mut Draw, field: &mut Option<AnswerField>| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        if qs.len() > 1 {
            // Titled tabs per question header (QuestionNavButton).
            let mut tx = x + 24.0;
            let mut row_h: f64 = 0.0;
            for (i, question) in qs.iter().enumerate() {
                let answered = state.is_answered(question, i);
                let label = d.kit.measure(cx, Font::Regular, base, 1.4, &question.header, None);
                let tw = label.x + 16.0 + if answered { 16.0 } else { 0.0 };
                let th = label.y.max(20.0) + 8.0;
                if tx > x + 24.0 && tx + tw > x + w - 12.0 {
                    tx = x + 24.0;
                    cy += row_h + 4.0;
                    row_h = 0.0;
                }
                let active = i == index;
                let target = HitTarget::QuestionTab { key: key.to_owned(), index: i };
                let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
                let fg = if active { p.foreground } else { p.muted };
                d.kit.bordered(cx, rect_xywh(tx, cy, tw, th), if active || hovered { p.surface2 } else { p.surface1 }, 6.0, if active { p.muted } else { p.border }, 1.0);
                let mut lx = tx + 8.0;
                if answered {
                    d.kit.icon(cx, "check", rect_xywh(lx, cy, 12.0, th), 12.0, fg);
                    lx += 16.0;
                }
                d.kit.label(cx, Font::Regular, base, 1.4, fg, dvec2(lx, cy + (th - label.y) * 0.5), &question.header, None);
                if !state.pending {
                    d.canvas.add_hit(rect_xywh(tx, cy, tw, th), target);
                }
                tx += tw + 4.0;
                row_h = row_h.max(th);
            }
            cy += row_h + 12.0;
        }
        let laid = d.kit.layout(cx, Font::Regular, base, 22.0 / base, &q.question, Some(inner - 24.0), 0.0);
        d.kit.draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 24.0, cy), &laid);
        d.canvas.add_run(laid.clone(), dvec2(x + 24.0, cy), Some('\n'));
        cy += laidout_size(&laid).y + 4.0 + 12.0;
        for (j, option) in q.options.iter().enumerate() {
            if j > 0 {
                cy += 4.0;
            }
            cy += question_option(cx, d, key, option, j, selected.contains(&j), q.multi_select, state.pending, x + 12.0, cy, inner);
        }
        if q.shows_text_input() {
            if !q.options.is_empty() {
                cy += 8.0;
            }
            let other = state.other_texts.get(&index).cloned().unwrap_or_default();
            let fh = base * 1.4 + 26.0;
            let rect = rect_xywh(x + 12.0, cy, inner, fh);
            let highlighted = !other.is_empty();
            d.kit.bordered(cx, rect, p.surface2, 8.0, if highlighted { p.border_accent } else { p.border }, 1.0);
            *field = Some(AnswerField {
                rect,
                placeholder: q.placeholder.clone().unwrap_or_else(|| t(if q.options.is_empty() { "message.question.answerPlaceholder" } else { "message.question.otherPlaceholder" })),
                text: other,
                enabled: !state.pending,
                highlighted,
            });
            cy += fh;
        }
        cy += 12.0;
        if state.failed {
            cy += super::chrome::error_alert(cx, d, &t("message.question.responseFailed"), x + 12.0, cy, inner) + 12.0;
        }
        let primary_label = t(if last { "message.question.submit" } else { "message.question.next" });
        let dismiss_target = HitTarget::QuestionDismiss { key: key.to_owned() };
        let primary_target = HitTarget::QuestionPrimary { key: key.to_owned() };
        if d.look.compact {
            let s = card_button(cx, d, x + 12.0, cy, Some(inner), &dismiss_label, "x", p.muted, None, p.border_accent, state.pending && state.responding_action.as_deref() == Some("dismiss"), false, dismiss_target);
            cy += s.y + 8.0;
            let s = card_button(cx, d, x + 12.0, cy, Some(inner), &primary_label, "check", p.on_tertiary, Some(p.accent), p.accent, state.pending && state.responding_action.as_deref() == Some("submit"), primary_disabled, primary_target);
            cy += s.y;
        } else {
            let s = card_button(cx, d, x + 12.0, cy, None, &dismiss_label, "x", p.muted, None, p.border_accent, state.pending && state.responding_action.as_deref() == Some("dismiss"), false, dismiss_target);
            let s2 = card_button(cx, d, x + 12.0 + s.x + 8.0, cy, None, &primary_label, "check", p.on_tertiary, Some(p.accent), p.accent, state.pending && state.responding_action.as_deref() == Some("submit"), primary_disabled, primary_target);
            cy += s.y.max(s2.y);
        }
        cy + 12.0 - y
    };
    let mut scratch = None;
    let h = d.measure(|d| paint(cx, d, &mut scratch));
    d.kit.bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, &mut field);
    (h, field)
}

/// QuestionOptionRow: 12/8 padding, radius 6, surface2 when hovered or
/// selected, 18px radio/checkbox control, label, description and preview.
#[allow(clippy::too_many_arguments)]
fn question_option(cx: &mut Cx2d, d: &mut Draw, key: &str, option: &Value, j: usize, selected: bool, multi: bool, pending: bool, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let target = HitTarget::QuestionOption { key: key.to_owned(), index: j };
    let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
    let inner = w - 24.0 - 26.0;
    let label = d.kit.layout(cx, Font::Regular, base, 22.0 / base, str_of(get(option, "label")), Some(inner), 0.0);
    let mut h = laidout_size(&label).y;
    let description = non_empty_str(get(option, "description")).map(|text| d.kit.layout(cx, Font::Regular, base, 20.0 / base, text, Some(inner), 0.0));
    if let Some(desc) = &description {
        h += 4.0 + laidout_size(desc).y;
    }
    let code = d.look.code();
    let preview = non_empty_str(get(option, "preview")).map(|text| d.kit.layout(cx, Font::Mono, code, 20.0 / code, text, Some(inner - 32.0), 0.0));
    if let Some(prev) = &preview {
        h += 4.0 + laidout_size(prev).y + 24.0;
    }
    let total = h + 16.0;
    let r = rect_xywh(x, y, w, total);
    if hovered || selected {
        d.kit.rect(cx, r, p.surface2, 6.0);
    }
    let control = rect_xywh(x + 12.0, y + 10.0, 18.0, 18.0);
    if multi {
        d.kit.bordered(cx, control, if selected { p.accent } else { TRANSPARENT }, 4.0, if selected { p.accent } else { p.extra_muted }, 1.0);
        if selected {
            d.kit.icon(cx, "check", control, 12.0, p.on_tertiary);
        }
    } else {
        d.kit.bordered(cx, control, TRANSPARENT, 9.0, if selected { p.accent } else { p.extra_muted }, 1.0);
        if selected {
            d.kit.rect(cx, rect_xywh(control.pos.x + 5.0, control.pos.y + 5.0, 8.0, 8.0), p.accent, 4.0);
        }
    }
    let tx = x + 12.0 + 26.0;
    let mut cy = y + 8.0;
    d.kit.draw_laidout(cx, Font::Regular, if selected { p.foreground } else { p.muted }, dvec2(tx, cy), &label);
    cy += laidout_size(&label).y;
    if let Some(desc) = description {
        cy += 4.0;
        d.kit.draw_laidout(cx, Font::Regular, p.muted, dvec2(tx, cy), &desc);
        cy += laidout_size(&desc).y;
    }
    if let Some(prev) = preview {
        cy += 4.0;
        let ph = laidout_size(&prev).y + 24.0;
        d.kit.bordered(cx, rect_xywh(tx, cy, inner, ph), p.surface0, 8.0, p.border, 1.0);
        d.kit.draw_laidout(cx, Font::Mono, p.foreground, dvec2(tx + 16.0, cy + 12.0), &prev);
        d.canvas.add_run(prev.clone(), dvec2(tx + 16.0, cy + 12.0), Some('\n'));
    }
    if !pending {
        d.canvas.add_hit(r, target);
    }
    total
}

/// The response the question form sends (submit or dismiss).
pub fn question_response(r: &Value, qs: &[Question], state: &CardState, dismiss: bool) -> Value {
    let mut input = map(get(r, "input")).clone();
    if dismiss {
        let submit_empty = qs.iter().all(|q| q.allow_empty && q.options.is_empty());
        if !submit_empty {
            return json!({"behavior": "deny", "message": "Dismissed by user"});
        }
    }
    input.insert("answers".into(), state.build_answers(qs));
    json!({"behavior": "allow", "updatedInput": Value::Object(input)})
}
