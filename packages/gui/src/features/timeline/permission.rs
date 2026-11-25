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
use paseo_gui_platform::webview::{WebTheme, WebView, WebViewOptions};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};

/// question-form-card-core.ts QuestionFormQuestion.
#[derive(Clone, Debug)]
pub struct Question {
    pub question: String,
    pub header: String,
    answer_key: Option<String>,
    pub options: Vec<Value>,
    pub multi_select: bool,
    pub allow_other: bool,
    pub allow_empty: bool,
    pub placeholder: Option<String>,
    pub dismiss_label: Option<String>,
    /// Options selected before the user answers (`options[].default`).
    pub default_options: BTreeSet<usize>,
    /// The text answer before the user edits it (`defaultText`).
    pub default_text: Option<String>,
}

impl Question {
    pub fn shows_text_input(&self) -> bool {
        self.options.is_empty() || self.allow_other
    }
}

pub fn is_elicitation(request: &Value) -> bool {
    get(get(request, "metadata"), "mcpElicitation") == true
}

pub fn elicitation_url(request: &Value) -> Option<String> {
    if !is_elicitation(request) {
        return None;
    }
    let raw = get(get(request, "input"), "url").as_str()?;
    paseo_protocol::helpers::mcp_elicitation_url(raw).map(Into::into)
}

pub(crate) const OPEN_ELICITATION_URL: &str = "mcp_open_url";

/// The HTML of an OMP `ask` form, which the card shows as is where web views exist.
pub fn form_html(request: &Value) -> Option<&str> {
    (get(get(request, "metadata"), "ompAskForm") == true)
        .then(|| get(get(request, "input"), "html").as_str())
        .flatten()
        .filter(|html| !html.trim().is_empty())
}

/// Whether the card shows the form's HTML in a web view rather than its questions.
pub fn shows_form(request: &Value, state: &CardState) -> bool {
    !state.form_unavailable && form_html(request).is_some() && WebView::supported()
}

/// The web view of the form in `request`: its HTML, an Other answer for every choice that
/// allows one (by field name), and the card's colors and text size.
pub fn form_options(request: &Value, theme: WebTheme) -> Option<WebViewOptions> {
    let other = get(get(request, "input"), "questions")
        .as_array()?
        .iter()
        .filter(|q| is_true(get(q, "allowOther")))
        .filter_map(|q| get(q, "name").as_str().map(str::to_owned))
        .collect();
    Some(WebViewOptions {
        html: form_html(request)?.to_owned(),
        other,
        other_label: t("message.question.otherPlaceholder"),
        theme,
    })
}

/// parseQuestionFormQuestions: None unless every question is well formed.
pub fn questions(request: &Value) -> Option<Vec<Question>> {
    let raw = get(get(request, "input"), "questions").as_array()?;
    if raw.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let elicitation = is_elicitation(request);
    for q in raw {
        let question = q.get("question")?.as_str()?;
        let header = q.get("header")?.as_str()?;
        let answer_key = if elicitation {
            Some(q.get("answerKey")?.as_str()?.to_owned())
        } else {
            None
        };
        let options = q.get("options")?.as_array()?;
        if options
            .iter()
            .any(|o| !o.is_object() || !get(o, "label").is_string())
        {
            return None;
        }
        let multi_select = is_true(get(q, "multiSelect"));
        let mut default_options: BTreeSet<usize> = options
            .iter()
            .enumerate()
            .filter(|(_, o)| !elicitation && is_true(get(o, "default")))
            .map(|(i, _)| i)
            .collect();
        if !multi_select && let Some(&first) = default_options.first() {
            default_options = BTreeSet::from([first]);
        }
        out.push(Question {
            question: question.to_owned(),
            header: header.to_owned(),
            answer_key,
            options: options.clone(),
            multi_select,
            allow_other: is_true(get(q, "allowOther")) || is_true(get(q, "isOther")),
            allow_empty: is_true(get(q, "allowEmpty")),
            placeholder: get(q, "placeholder").as_str().map(str::to_owned),
            dismiss_label: get(q, "dismissLabel").as_str().map(str::to_owned),
            default_options,
            default_text: (!elicitation)
                .then(|| get(q, "defaultText").as_str().map(str::to_owned))
                .flatten(),
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
    /// The form's content height its web view reported, in logical px.
    pub form_height: Option<f64>,
    /// The data the form's web view submitted, sent as the answer.
    pub form_data: Option<Value>,
    /// The web view failed: the card shows the form's questions instead.
    pub form_unavailable: bool,
}

impl CardState {
    pub fn for_request(request: &Value) -> Self {
        let mut state = Self {
            request_id: value_text(get(request, "id")),
            ..Default::default()
        };
        for (index, q) in questions(request).into_iter().flatten().enumerate() {
            if !q.default_options.is_empty() {
                state.selections.insert(index, q.default_options);
            }
            if let Some(text) = q.default_text {
                state.other_texts.insert(index, text);
            }
        }
        state
    }

    pub fn is_answered(&self, q: &Question, index: usize) -> bool {
        if q.answer_key.is_some() && q.allow_empty {
            return true;
        }
        if self.selections.get(&index).is_some_and(|s| !s.is_empty()) {
            return true;
        }
        if !q.shows_text_input() {
            return false;
        }
        if self
            .other_texts
            .get(&index)
            .is_some_and(|t| if q.answer_key.is_some() { !t.is_empty() } else { !t.trim().is_empty() })
        {
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
                .map(|s| {
                    s.iter()
                        .map(|&o| str_of(get(&q.options[o], "label")).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let text = self
                .other_texts
                .get(&i)
                .map(|t| t.trim().to_owned())
                .unwrap_or_default();
            if q.shows_text_input() {
                if !text.is_empty() {
                    let value = if q.multi_select {
                        labels
                            .iter()
                            .cloned()
                            .chain(std::iter::once(text))
                            .collect::<Vec<_>>()
                            .join(", ")
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

    /// `{header: {"options": [label, ...], "other": text}}` for every question, beside the
    /// joined `answers` strings older daemons read: a label containing ", " stays whole and
    /// an emptied multi-select stays distinguishable from an unanswered one.
    pub fn build_selections(&self, qs: &[Question]) -> Value {
        let mut selections = serde_json::Map::new();
        for (i, q) in qs.iter().enumerate() {
            let options: Vec<&str> = self
                .selections
                .get(&i)
                .into_iter()
                .flatten()
                .filter_map(|&o| q.options.get(o))
                .map(|o| str_of(get(o, "label")))
                .collect();
            let mut entry = json!({"options": options});
            let text = self.other_texts.get(&i).map_or("", |t| {
                if q.answer_key.is_some() { t.as_str() } else { t.trim() }
            });
            if q.shows_text_input() && !text.is_empty() {
                entry["other"] = json!(text);
            }
            if q.answer_key.is_none() || !options.is_empty() || entry.get("other").is_some() {
                selections.insert(q.answer_key.as_ref().unwrap_or(&q.header).clone(), entry);
            }
        }
        Value::Object(selections)
    }

    /// Option toggle; single-select advances to the next question.
    pub fn toggle(&mut self, q: &Question, index: usize, option: usize, count: usize) -> bool {
        let selected = self.selections.entry(index).or_default();
        if q.multi_select {
            if !selected.remove(&option) {
                if q.answer_key.is_some() {
                    let exclusive = |index| q.options.get(index).is_some_and(|o| is_true(get(o, "exclusive")));
                    if exclusive(option) {
                        selected.clear();
                    } else {
                        selected.retain(|&index| !exclusive(index));
                    }
                }
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

/// What the row places over the card: the answer TextInput, or the form's web view.
pub enum CardField {
    Answer(AnswerField),
    Form(FormField),
}

/// The answer field the row draws (a native TextInput) for the active question.
pub struct AnswerField {
    pub rect: Rect,
    pub placeholder: String,
    pub text: String,
    pub enabled: bool,
    pub highlighted: bool,
}

/// Where the row places the form's web view (window logical px).
pub struct FormField {
    pub rect: Rect,
    pub theme: WebTheme,
}

/// _CardButton: 8/12 padding, radius 6, 14px icon + base label 8 apart.
/// At most `max_w` wide; a label that does not fit is ellipsized. Busy or
/// disabled buttons register no hit.
#[allow(clippy::too_many_arguments)]
fn card_button(
    cx: &mut Cx2d,
    d: &mut Draw,
    x: f64,
    y: f64,
    w: Option<f64>,
    max_w: f64,
    label: &str,
    icon: &str,
    fg: Vec4,
    bg: Option<Vec4>,
    border: Vec4,
    responding: bool,
    dimmed: bool,
    target: HitTarget,
) -> DVec2 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let measured = d.kit.measure(cx, Font::Regular, base, 1.4, label, None);
    let bw = w.unwrap_or(button_width(measured.x)).min(max_w).max(1.0);
    let room = (bw - button_width(0.0)).max(1.0);
    let clamped = (measured.x > room + 0.5).then(|| {
        d.kit
            .layout_clamped(cx, Font::Regular, base, 1.4, label, room, 1)
    });
    let text = clamped.as_ref().map_or(measured, |laid| laidout_size(laid));
    let bh = text.y.max(14.0) + 16.0;
    let r = rect_xywh(x, y, bw, bh);
    let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
    let fill = bg.unwrap_or(if hovered { p.surface2 } else { p.surface1 });
    let alpha = if dimmed { 0.5 } else { 1.0 };
    d.kit.bordered(
        cx,
        r,
        with_alpha(fill, fill.w * alpha),
        6.0,
        with_alpha(border, border.w * alpha),
        1.0,
    );
    if responding {
        d.spinner(
            cx,
            rect_xywh(r.center().x - 8.0, r.center().y - 8.0, 16.0, 16.0),
            fg,
        );
    } else {
        let start = x + (bw - (14.0 + 8.0 + text.x)) * 0.5;
        let color = with_alpha(fg, fg.w * alpha);
        d.kit
            .icon(cx, icon, rect_xywh(start, y, 14.0, bh), 14.0, color);
        let pos = dvec2(start + 22.0, y + (bh - text.y) * 0.5);
        match &clamped {
            Some(laid) => d.kit.draw_laidout(cx, Font::Regular, color, pos, laid),
            None => {
                d.kit
                    .label(cx, Font::Regular, base, 1.4, color, pos, label, None);
            }
        }
    }
    if !responding && !dimmed {
        d.canvas.add_hit(r, target);
    }
    dvec2(bw, bh)
}

/// A card button's width around a label `text_w` wide (padding and icon).
fn button_width(text_w: f64) -> f64 {
    12.0 + 14.0 + 8.0 + text_w + 12.0
}

pub fn hit_key(target: &HitTarget) -> String {
    format!("{target:?}")
}

/// Draws one pending permission (`key` = pendingPermissionKey). Returns the
/// card height and the answer field to place, if any.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    r: &Value,
    x: f64,
    y: f64,
    w: f64,
    opts: &mut markdown::MdOpts,
) -> (f64, Option<CardField>) {
    if d.state
        .permission
        .as_ref()
        .is_none_or(|s| s.request_id != value_text(get(r, "id")))
    {
        d.state.permission = Some(CardState::for_request(r));
    }
    if get(r, "kind") == "question" {
        if d.state
            .permission
            .as_ref()
            .is_some_and(|s| shows_form(r, s))
        {
            return form_card(cx, d, key, x, y, w);
        }
        return match questions(r) {
            Some(qs) => question_form(cx, d, key, r, &qs, x, y, w),
            None => (0.0, None),
        };
    }
    let plan = get(r, "kind") == "plan";
    let plan_text = plan_markdown(r);
    if let (true, Some(text)) = (plan, plan_text.as_deref()) {
        return (
            plan_card(
                cx,
                d,
                key,
                &title(r),
                str_of(get(r, "description")),
                text,
                x,
                y,
                w,
                opts,
                Some(r),
            ),
            None,
        );
    }
    (
        12.0 + permission_card(cx, d, key, r, plan_text.as_deref(), x, y + 12.0, w, opts) + 12.0,
        None,
    )
}

fn footer(cx: &mut Cx2d, d: &mut Draw, key: &str, r: &Value, x: f64, y: f64, w: f64) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let state = d.state.permission.clone().unwrap_or_default();
    let elicitation = is_elicitation(r);
    let question = t("agentStream.permission.question");
    let size = d.kit.label(
        cx,
        Font::Regular,
        base,
        1.4,
        p.muted,
        dvec2(x, y + 4.0),
        &question,
        Some(w),
    );
    let mut cy = y + size.y + 8.0 + 8.0;
    let mut cx_pos = x;
    let mut row_h: f64 = 0.0;
    let mut choices = actions(r);
    if elicitation_url(r).is_some() {
        choices.insert(0, json!({"id":OPEN_ELICITATION_URL}));
    }
    for action in choices {
        let id = value_text(get(&action, "id"));
        let opening = id == OPEN_ELICITATION_URL;
        let fg = if opening || get(&action, "variant") == "primary" {
            p.foreground
        } else {
            p.muted
        };
        let icon = if opening {
            "external-link"
        } else if get(&action, "behavior") == "allow" {
            "check"
        } else {
            "x"
        };
        let label = if opening {
            t("agentStream.permission.mcpOpen")
        } else if elicitation {
            match id.as_str() {
                "cancel" => t("common.actions.cancel"),
                "decline" => t("agentStream.permission.mcpDecline"),
                "accept" => t("agentStream.permission.accept"),
                _ => str_of(get(&action, "label")).to_owned(),
            }
        } else {
            str_of(get(&action, "label")).to_owned()
        };
        let target = HitTarget::PermissionAction {
            key: key.to_owned(),
            action: id.clone(),
        };
        let responding = state.responding_action.as_deref() == Some(id.as_str());
        if d.look.compact {
            let s = card_button(
                cx,
                d,
                x,
                cy,
                Some(w),
                w,
                &label,
                icon,
                fg,
                None,
                p.border_accent,
                responding,
                state.pending && !responding,
                target,
            );
            cy += s.y + 8.0;
        } else {
            let text_w = button_width(d.kit.measure(cx, Font::Regular, base, 1.4, &label, None).x);
            if cx_pos > x && cx_pos + text_w > x + w {
                cx_pos = x;
                cy += row_h + 8.0;
                row_h = 0.0;
            }
            let s = card_button(
                cx,
                d,
                cx_pos,
                cy,
                None,
                x + w - cx_pos,
                &label,
                icon,
                fg,
                None,
                p.border_accent,
                responding,
                state.pending && !responding,
                target,
            );
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
fn permission_card(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    r: &Value,
    plan_text: Option<&str>,
    x: f64,
    y: f64,
    w: f64,
    opts: &mut markdown::MdOpts,
) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let paint = |cx: &mut Cx2d, d: &mut Draw, opts: &mut markdown::MdOpts| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        let title = title(r);
        let laid = d.kit.layout(
            cx,
            Font::Regular,
            base,
            d.look.typo.system_size(22.0) / base,
            &title,
            Some(inner),
            0.0,
        );
        d.kit
            .draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 12.0, cy), &laid);
        d.canvas
            .add_run(laid.clone(), dvec2(x + 12.0, cy), Some('\n'));
        cy += laidout_size(&laid).y + 8.0;
        let description = str_of(get(r, "description"));
        if !description.is_empty() {
            let laid = d.kit.layout(
                cx,
                Font::Regular,
                base,
                d.look.typo.system_size(20.0) / base,
                description,
                Some(inner),
                0.0,
            );
            d.kit
                .draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 12.0, cy), &laid);
            d.canvas
                .add_run(laid.clone(), dvec2(x + 12.0, cy), Some('\n'));
            cy += laidout_size(&laid).y + 8.0;
        }
        if let Some(text) = plan_text {
            cy += plan_card(
                cx,
                d,
                &format!("{key}:proposed"),
                &t("agentStream.permission.proposedPlan"),
                "",
                text,
                x + 12.0,
                cy,
                inner,
                opts,
                None,
            ) + 8.0;
        }
        if let Some(url) = elicitation_url(r) {
            cy += elicitation_text(cx, d, &url, x + 12.0, cy, inner);
        } else if get(r, "kind") != "plan" && !is_elicitation(r) {
            let detail = if get(r, "detail").is_object() {
                get(r, "detail").clone()
            } else {
                json!({"type": "unknown", "input": get(r, "input"), "output": null})
            };
            let blocks = detail_blocks(&detail, None, None, &|k| t(k));
            cy += draw_details(
                cx,
                d,
                &blocks,
                detail_is_full_bleed(&detail),
                false,
                &format!("{key}:detail"),
                x + 12.0,
                cy,
                inner,
                Some(200.0),
                None,
            ) + 8.0;
        }
        cy += footer(cx, d, key, r, x + 12.0, cy, inner);
        cy + 12.0 - y
    };
    let h = super::markdown::view::measured(cx, d, opts, |cx, d, opts| paint(cx, d, opts));
    d.kit
        .bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, opts)
}

fn elicitation_text(cx: &mut Cx2d, d: &mut Draw, text: &str, x: f64, y: f64, width: f64) -> f64 {
    let base = d.look.ui(14.0);
    let laid = d.kit.layout(cx, Font::Regular, base,
        d.look.typo.system_size(22.0) / base, text, Some(width), 0.0);
    let height = laidout_size(&laid).y + 8.0;
    d.kit.draw_laidout(cx, Font::Regular, d.look.palette.foreground, dvec2(x, y), &laid);
    d.canvas.add_run(laid, dvec2(x, y), Some('\n'));
    height
}

/// components/plan-card.tsx: collapsible surface1 card with a 16px chevron,
/// base title, muted description, markdown body and optional action footer.
#[allow(clippy::too_many_arguments)]
pub fn plan_card(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    title: &str,
    description: &str,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    opts: &mut markdown::MdOpts,
    footer_request: Option<&Value>,
) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let toggle = format!("{key}:plan");
    let expanded = d.state.is_expanded(&toggle, true);
    let paint = |cx: &mut Cx2d, d: &mut Draw, opts: &mut markdown::MdOpts| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        let laid = d.kit.layout(
            cx,
            Font::Regular,
            base,
            d.look.typo.system_size(22.0) / base,
            title,
            Some(inner - 24.0),
            0.0,
        );
        let row_h = laidout_size(&laid).y.max(24.0);
        d.kit.icon(
            cx,
            if expanded {
                "chevron-down"
            } else {
                "chevron-right"
            },
            rect_xywh(x + 12.0, cy, 16.0, row_h),
            16.0,
            p.muted,
        );
        d.kit.draw_laidout(
            cx,
            Font::Regular,
            p.foreground,
            dvec2(x + 36.0, cy + (row_h - laidout_size(&laid).y) * 0.5),
            &laid,
        );
        d.canvas.add_hit(
            rect_xywh(x + 12.0, cy, inner, row_h),
            HitTarget::Toggle(toggle.clone()),
        );
        cy += row_h;
        if expanded {
            if !description.is_empty() {
                cy += 8.0;
                let laid = d.kit.layout(
                    cx,
                    Font::Regular,
                    base,
                    d.look.typo.system_size(20.0) / base,
                    description,
                    Some(inner),
                    0.0,
                );
                d.kit
                    .draw_laidout(cx, Font::Regular, p.muted, dvec2(x + 12.0, cy), &laid);
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
    d.kit
        .bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, opts)
}

#[allow(clippy::too_many_arguments)]
fn question_form(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    request: &Value,
    qs: &[Question],
    x: f64,
    y: f64,
    w: f64,
) -> (f64, Option<CardField>) {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let state = d.state.permission.clone().unwrap_or_default();
    let elicitation = is_elicitation(request);
    let index = state.active_question.min(qs.len() - 1);
    let q = &qs[index];
    let all_answered = qs.iter().enumerate().all(|(i, q)| state.is_answered(q, i));
    let last = index == qs.len() - 1;
    let primary_disabled = state.pending
        || if last {
            !all_answered
        } else {
            !state.is_answered(q, index)
        };
    let dismiss_label = if elicitation {
        t("common.actions.cancel")
    } else {
        qs.iter()
            .filter_map(|q| q.dismiss_label.as_deref())
            .find(|l| !l.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| t("common.actions.dismiss"))
    };
    let decline_label = elicitation.then(|| t("agentStream.permission.mcpDecline"));
    let selected = state.selections.get(&index).cloned().unwrap_or_default();
    let mut field = None;
    let paint = |cx: &mut Cx2d, d: &mut Draw, field: &mut Option<CardField>| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        if elicitation {
            cy += elicitation_text(cx, d, &title(request), x + 12.0, cy, inner);
            if let Some(description) = non_empty_str(get(request, "description")) {
                cy += elicitation_text(cx, d, description, x + 12.0, cy, inner);
            }
        }
        if qs.len() > 1 {
            // Titled tabs per question header (QuestionNavButton).
            let mut tx = x + 24.0;
            let mut row_h: f64 = 0.0;
            for (i, question) in qs.iter().enumerate() {
                let answered = state.is_answered(question, i);
                let label = d
                    .kit
                    .measure(cx, Font::Regular, base, 1.4, &question.header, None);
                let tw = label.x + 16.0 + if answered { 16.0 } else { 0.0 };
                let th = label.y.max(20.0) + 8.0;
                if tx > x + 24.0 && tx + tw > x + w - 12.0 {
                    tx = x + 24.0;
                    cy += row_h + 4.0;
                    row_h = 0.0;
                }
                let active = i == index;
                let target = HitTarget::QuestionTab {
                    key: key.to_owned(),
                    index: i,
                };
                let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
                let fg = if active { p.foreground } else { p.muted };
                d.kit.bordered(
                    cx,
                    rect_xywh(tx, cy, tw, th),
                    if active || hovered {
                        p.surface2
                    } else {
                        p.surface1
                    },
                    6.0,
                    if active { p.muted } else { p.border },
                    1.0,
                );
                let mut lx = tx + 8.0;
                if answered {
                    d.kit
                        .icon(cx, "check", rect_xywh(lx, cy, 12.0, th), 12.0, fg);
                    lx += 16.0;
                }
                d.kit.label(
                    cx,
                    Font::Regular,
                    base,
                    1.4,
                    fg,
                    dvec2(lx, cy + (th - label.y) * 0.5),
                    &question.header,
                    None,
                );
                if !state.pending {
                    d.canvas.add_hit(rect_xywh(tx, cy, tw, th), target);
                }
                tx += tw + 4.0;
                row_h = row_h.max(th);
            }
            cy += row_h + 12.0;
        }
        let laid = d.kit.layout(
            cx,
            Font::Regular,
            base,
            d.look.typo.system_size(22.0) / base,
            &q.question,
            Some(inner - 24.0),
            0.0,
        );
        d.kit
            .draw_laidout(cx, Font::Regular, p.foreground, dvec2(x + 24.0, cy), &laid);
        d.canvas
            .add_run(laid.clone(), dvec2(x + 24.0, cy), Some('\n'));
        cy += laidout_size(&laid).y + 4.0 + 12.0;
        for (j, option) in q.options.iter().enumerate() {
            if j > 0 {
                cy += 4.0;
            }
            cy += question_option(
                cx,
                d,
                key,
                option,
                j,
                selected.contains(&j),
                q.multi_select,
                state.pending,
                x + 12.0,
                cy,
                inner,
            );
        }
        if q.shows_text_input() {
            if !q.options.is_empty() {
                cy += 8.0;
            }
            let other = state.other_texts.get(&index).cloned().unwrap_or_default();
            let fh = base * 1.4 + 26.0;
            let rect = rect_xywh(x + 12.0, cy, inner, fh);
            let highlighted = !other.is_empty();
            d.kit.bordered(
                cx,
                rect,
                p.surface2,
                8.0,
                if highlighted {
                    p.border_accent
                } else {
                    p.border
                },
                1.0,
            );
            *field = Some(CardField::Answer(AnswerField {
                rect,
                placeholder: q.placeholder.clone().unwrap_or_else(|| {
                    t(if q.options.is_empty() {
                        "message.question.answerPlaceholder"
                    } else {
                        "message.question.otherPlaceholder"
                    })
                }),
                text: other,
                enabled: !state.pending,
                highlighted,
            }));
            cy += fh;
        }
        cy += 12.0;
        if state.failed {
            cy += super::chrome::error_alert(
                cx,
                d,
                &t("message.question.responseFailed"),
                x + 12.0,
                cy,
                inner,
            ) + 12.0;
        }
        let primary_label = t(match (last, elicitation) {
            (true, true) => "agentStream.permission.accept",
            (true, false) => "message.question.submit",
            (false, _) => "message.question.next",
        });
        cy += answer_buttons(
            cx,
            d,
            key,
            &state,
            x,
            cy,
            w,
            &dismiss_label,
            decline_label.as_deref(),
            &primary_label,
            primary_disabled,
        );
        cy + 12.0 - y
    };
    let mut scratch = None;
    let h = d.measure(|d| paint(cx, d, &mut scratch));
    d.kit
        .bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, &mut field);
    (h, field)
}

/// The card's dismiss, optional decline and primary buttons from `y`, in one row or
/// stacked when they do not fit; returns their height.
#[allow(clippy::too_many_arguments)]
fn answer_buttons(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    state: &CardState,
    x: f64,
    y: f64,
    w: f64,
    dismiss_label: &str,
    decline_label: Option<&str>,
    primary_label: &str,
    primary_disabled: bool,
) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let inner = w - 24.0;
    let mut cy = y;
    let dismiss_target = HitTarget::QuestionDismiss {
        key: key.to_owned(),
    };
    let primary_target = HitTarget::QuestionPrimary {
        key: key.to_owned(),
    };
    let dismiss_responding = state.pending && state.responding_action.as_deref() == Some("dismiss");
    let primary_responding = state.pending && state.responding_action.as_deref() == Some("submit");
    let decline = decline_label.map(|label| {
        let responding = state.responding_action.as_deref() == Some("decline") && state.pending;
        (label, "x", p.muted, None, p.border_accent, responding, state.pending && !responding,
            HitTarget::PermissionAction { key: key.to_owned(), action: "decline".into() })
    });
    let buttons = [
        Some((dismiss_label, "x", p.muted, None, p.border_accent, dismiss_responding,
            state.pending && !dismiss_responding, dismiss_target)),
        decline,
        Some((primary_label, "check", p.on_tertiary, Some(p.accent), p.accent,
            primary_responding, primary_disabled, primary_target)),
    ];
    let stacked = d.look.compact || buttons.iter().flatten().map(|(label, ..)| {
        button_width(d.kit.measure(cx, Font::Regular, base, 1.4, label, None).x) + 8.0
    }).sum::<f64>() - 8.0 > inner;
    let mut bx = x + 12.0;
    let mut row_h: f64 = 0.0;
    for (label, icon, fg, bg, border, responding, disabled, target) in buttons.into_iter().flatten() {
        let size = card_button(cx, d, bx, cy, stacked.then_some(inner),
            x + w - 12.0 - bx, label, icon, fg, bg, border, responding, disabled, target);
        if stacked {
            cy += size.y + 8.0;
        } else {
            bx += size.x + 8.0;
            row_h = row_h.max(size.y);
        }
    }
    cy += if stacked { -8.0 } else { row_h };
    cy - y
}

/// The form's area before its web view reports the content height.
const FORM_LOADING_HEIGHT: f64 = 96.0;

/// An OMP `ask` form shown as its HTML: the row places a web view over the reserved area;
/// Submit asks the page for its data, which the row then sends.
fn form_card(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    x: f64,
    y: f64,
    w: f64,
) -> (f64, Option<CardField>) {
    let p = d.look.palette;
    let state = d.state.permission.clone().unwrap_or_default();
    let dismiss_label = t("common.actions.dismiss");
    let primary_label = t("message.question.submit");
    let theme = WebTheme {
        dark: p.is_dark,
        background: p.surface1,
        foreground: p.foreground,
        muted: p.muted,
        border: p.border,
        accent: p.accent,
        surface: p.surface2,
        font_size: d.look.ui(14.0),
    };
    let mut field = None;
    let paint = |cx: &mut Cx2d, d: &mut Draw, field: &mut Option<CardField>| -> f64 {
        let inner = w - 24.0;
        let mut cy = y + 12.0;
        let height = state.form_height.unwrap_or(FORM_LOADING_HEIGHT);
        let rect = rect_xywh(x + 12.0, cy, inner, height);
        if state.form_height.is_none() {
            d.spinner(
                cx,
                rect_xywh(x + 12.0 + inner * 0.5 - 9.0, cy + height * 0.5 - 9.0, 18.0, 18.0),
                p.muted,
            );
        }
        *field = Some(CardField::Form(FormField {
            rect,
            theme: theme.clone(),
        }));
        cy += height + 12.0;
        if state.failed {
            cy += super::chrome::error_alert(
                cx,
                d,
                &t("message.question.responseFailed"),
                x + 12.0,
                cy,
                inner,
            ) + 12.0;
        }
        cy += answer_buttons(
            cx,
            d,
            key,
            &state,
            x,
            cy,
            w,
            &dismiss_label,
            None,
            &primary_label,
            state.pending,
        );
        cy + 12.0 - y
    };
    let mut scratch = None;
    let h = d.measure(|d| paint(cx, d, &mut scratch));
    d.kit
        .bordered(cx, rect_xywh(x, y, w, h), p.surface1, 8.0, p.border, 1.0);
    paint(cx, d, &mut field);
    (h, field)
}

/// QuestionOptionRow: 12/8 padding, radius 6, surface2 when hovered or
/// selected, 18px radio/checkbox control, label, description and preview.
#[allow(clippy::too_many_arguments)]
fn question_option(
    cx: &mut Cx2d,
    d: &mut Draw,
    key: &str,
    option: &Value,
    j: usize,
    selected: bool,
    multi: bool,
    pending: bool,
    x: f64,
    y: f64,
    w: f64,
) -> f64 {
    let p = d.look.palette;
    let base = d.look.ui(14.0);
    let target = HitTarget::QuestionOption {
        key: key.to_owned(),
        index: j,
    };
    let hovered = d.state.hover_hit.as_deref() == Some(hit_key(&target).as_str());
    let inner = w - 24.0 - 26.0;
    let label = d.kit.layout(
        cx,
        Font::Regular,
        base,
        d.look.typo.system_size(22.0) / base,
        str_of(get(option, "label")),
        Some(inner),
        0.0,
    );
    let mut h = laidout_size(&label).y;
    let description = non_empty_str(get(option, "description")).map(|text| {
        d.kit.layout(
            cx,
            Font::Regular,
            base,
            d.look.typo.system_size(20.0) / base,
            text,
            Some(inner),
            0.0,
        )
    });
    if let Some(desc) = &description {
        h += 4.0 + laidout_size(desc).y;
    }
    let code = d.look.code();
    let preview = non_empty_str(get(option, "preview")).map(|text| {
        d.kit.layout(
            cx,
            Font::Mono,
            code,
            d.look.typo.system_size(20.0) / code,
            text,
            Some(inner - 32.0),
            0.0,
        )
    });
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
        d.kit.bordered(
            cx,
            control,
            if selected { p.accent } else { TRANSPARENT },
            4.0,
            if selected { p.accent } else { p.muted },
            1.0,
        );
        if selected {
            d.kit.icon(cx, "check", control, 12.0, p.on_tertiary);
        }
    } else {
        d.kit.bordered(
            cx,
            control,
            TRANSPARENT,
            9.0,
            if selected { p.accent } else { p.muted },
            1.0,
        );
        if selected {
            d.kit.rect(
                cx,
                rect_xywh(control.pos.x + 5.0, control.pos.y + 5.0, 8.0, 8.0),
                p.accent,
                4.0,
            );
        }
    }
    let tx = x + 12.0 + 26.0;
    let mut cy = y + 8.0;
    d.kit.draw_laidout(
        cx,
        Font::Regular,
        if selected { p.foreground } else { p.muted },
        dvec2(tx, cy),
        &label,
    );
    cy += laidout_size(&label).y;
    if let Some(desc) = description {
        cy += 4.0;
        d.kit
            .draw_laidout(cx, Font::Regular, p.muted, dvec2(tx, cy), &desc);
        cy += laidout_size(&desc).y;
    }
    if let Some(prev) = preview {
        cy += 4.0;
        let ph = laidout_size(&prev).y + 24.0;
        d.kit.bordered(
            cx,
            rect_xywh(tx, cy, inner, ph),
            p.surface0,
            8.0,
            p.border,
            1.0,
        );
        d.kit.draw_laidout(
            cx,
            Font::Mono,
            p.foreground,
            dvec2(tx + 16.0, cy + 12.0),
            &prev,
        );
        d.canvas
            .add_run(prev.clone(), dvec2(tx + 16.0, cy + 12.0), Some('\n'));
    }
    if !pending {
        d.canvas.add_hit(r, target);
    }
    total
}

/// The response the question form sends (submit or dismiss).
pub fn question_response(r: &Value, qs: &[Question], state: &CardState, dismiss: bool) -> Value {
    if is_elicitation(r) {
        return if dismiss {
            json!({"behavior":"deny","selectedActionId":"cancel","interrupt":true})
        } else {
            json!({"behavior":"allow","selectedActionId":"accept",
                "updatedInput":{"selections":state.build_selections(qs)}})
        };
    }
    if !dismiss && let Some(data) = &state.form_data {
        return json!({"behavior": "allow", "formData": data});
    }
    let mut input = map(get(r, "input")).clone();
    // The answer names its fields; the form's HTML need not travel back.
    input.remove("html");
    if dismiss {
        let submit_empty = qs.iter().all(|q| q.allow_empty && q.options.is_empty());
        if !submit_empty {
            return json!({"behavior": "deny", "message": "Dismissed by user"});
        }
    }
    input.insert("answers".into(), state.build_answers(qs));
    input.insert("selections".into(), state.build_selections(qs));
    json!({"behavior": "allow", "updatedInput": Value::Object(input)})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_defaults_prefill_the_card_and_answers_keep_comma_labels() {
        let request = json!({"id":"q","kind":"question","input":{"questions":[
            {"question":"Items?","header":"Items","multiSelect":true,"allowOther":true,
             "options":[{"label":"A, B","default":true},{"label":"C","default":true},{"label":"D"}]},
            {"question":"Days?","header":"Days","options":[],"defaultText":"30"}
        ]}});
        let qs = questions(&request).unwrap();
        let mut state = CardState::for_request(&request);
        assert_eq!(state.selections[&0], BTreeSet::from([0, 1]));
        assert_eq!(state.other_texts[&1], "30");

        state.toggle(&qs[0], 0, 1, qs.len());
        state.set_other(&qs[0], 0, "x, y");
        let response = question_response(&request, &qs, &state, false);
        assert_eq!(response["updatedInput"]["answers"]["Items"], "A, B, x, y");
        assert_eq!(
            response["updatedInput"]["selections"],
            json!({"Items":{"options":["A, B"],"other":"x, y"},"Days":{"options":[],"other":"30"}})
        );
    }

    #[test]
    fn mcp_form_omits_unanswered_fields_and_keeps_exact_user_values() {
        let request = json!({"id":"mcp","kind":"question","metadata":{"mcpElicitation":true},"input":{"questions":[
            {"question":"Choose labels","header":"Duplicate","answerKey":"tags","multiSelect":true,"allowEmpty":true,
                "options":[{"label":"A, B","default":true},{"label":"Empty list","exclusive":true}]},
            {"question":"Provide text","header":"Duplicate","answerKey":"text","allowEmpty":true,
                "options":[],"defaultText":"Not a user answer"},
            {"question":"Choose a value","header":"Other","answerKey":"omitted","allowEmpty":true,
                "options":[{"label":"Other"}]}
        ]}});
        let qs = questions(&request).unwrap();
        let mut state = CardState::for_request(&request);
        assert!(qs.iter().enumerate().all(|(i, q)| state.is_answered(q, i)));
        assert_eq!(question_response(&request, &qs, &state, false)["updatedInput"]["selections"], json!({}));
        state.toggle(&qs[0], 0, 0, qs.len());
        state.toggle(&qs[0], 0, 1, qs.len());
        assert_eq!(state.selections[&0], BTreeSet::from([1]));
        state.toggle(&qs[0], 0, 0, qs.len());
        assert_eq!(state.selections[&0], BTreeSet::from([0]));
        state.set_other(&qs[1], 1, " exact text ");
        assert_eq!(
            question_response(&request, &qs, &state, false),
            json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"selections":{
                "tags":{"options":["A, B"]},
                "text":{"options":[],"other":" exact text "}
            }}})
        );
        assert_eq!(question_response(&request, &qs, &state, true),
            json!({"behavior":"deny","selectedActionId":"cancel","interrupt":true}));
    }

    #[test]
    fn mcp_url_open_action_is_separate_from_provider_choices() {
        let mut request = json!({"metadata":{"mcpElicitation":true},"input":{"url":"https://example.test/path?complete=yes#step"},
            "actions":[{"id":"decline","behavior":"deny"},{"id":"cancel","behavior":"deny"},{"id":"accept","behavior":"allow"}]});
        assert_eq!(elicitation_url(&request).as_deref(), request["input"]["url"].as_str());
        assert!(actions(&request).iter().all(|a| a["id"] != OPEN_ELICITATION_URL));
        request["input"]["url"] = json!("javascript:alert(1)");
        assert!(elicitation_url(&request).is_none());
    }
}
