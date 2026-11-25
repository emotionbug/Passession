//! OMP's `ask` tool takes one HTML form, and Paseo shows the whole form as one question card.
//! OMP 19.1 and later send the form as one `form` dialog, which the card answers with the
//! form's data. Earlier OMP has no RPC form dialog and asks one dialog per field; a checkbox
//! group becomes a `select` repeated once per toggled option, without its checked state.
//! Paseo answers those dialogs itself, checking each one against the dialog OMP's own rules
//! predict. A dialog that does not match stops the replay and is shown to the user, with
//! every later one.

mod dom;
mod parse;
#[cfg(test)]
mod tests;

use crate::providers::question_answers;
use parse::{Block, Kind};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// OMP's automatic free-text option of a radio or checkbox group.
const OTHER: &str = "Other (type your own)";
/// OMP's multi-select finish option, after a theme-dependent symbol.
const DONE: &str = " Done selecting";
const HEADER_CHARS: usize = 24;

pub(super) struct AskForm {
    pub(super) tool: String,
    /// The form as OMP sent it, for clients that render HTML.
    html: String,
    plans: Vec<Plan>,
    title: String,
    trailing: Vec<String>,
    stage: Stage,
}

enum Stage {
    /// Waiting for OMP's first dialog: a `form` dialog, or the first per-field one.
    Waiting,
    /// The card is shown in place of this first dialog.
    Shown {
        request: String,
        held: Value,
    },
    /// The card answers OMP's `form` dialog, this request, whole.
    Form {
        request: String,
    },
    Replaying(Replay),
}

/// One named field, as OMP will ask it.
struct Plan {
    /// The field's `name`, which OMP reports its answer under.
    name: String,
    title: String,
    header: String,
    question: String,
    kind: PlanKind,
}

enum PlanKind {
    Choice {
        multi: bool,
        required: bool,
        /// Dialog labels of the enabled options.
        labels: Vec<String>,
        /// The enabled options' submitted values, in the order of `labels`.
        values: Vec<String>,
        /// The form data holds an array: a checkbox group or multi-select of several options.
        array: bool,
        defaults: BTreeSet<usize>,
        /// Checked disabled options: OMP counts them but never offers them.
        locked: usize,
        other: Option<String>,
    },
    Text {
        editor: bool,
        value: String,
        placeholder: String,
        required: bool,
    },
}

#[derive(Clone, Debug)]
enum Target {
    Single(Pick),
    Multi {
        picks: BTreeSet<usize>,
        other: Option<String>,
    },
    Text(String),
}

#[derive(Clone, Debug)]
enum Pick {
    Option(usize),
    Other(String),
}

pub(super) enum Step {
    /// Show this card in place of the dialog.
    Show(Value),
    /// Answer the dialog with this body.
    Reply(Value),
    /// A validation re-prompt: show the dialog; the replay resumes after it.
    Ask,
    /// The form is complete; the dialog is not part of it.
    Finished,
    /// The dialog differs from the prediction.
    Abandon(String),
}

pub(super) enum Submitted {
    /// Answer the held first per-field dialog with this body; the replay continues.
    Reply(Value),
    /// Answer the `form` dialog with this body; the form is complete.
    Data(Value),
    Cancelled,
}

/// What an `ask` form asked, for the tool's record, in document order: the form's title, and
/// the text of every field's question (the prose before it and its title), each followed by
/// its answer once OMP reports one (`{name, question, answer}` per field), then the prose
/// after the last field. `None` when the form has no named control.
pub(super) fn record(html: &str, answers: &[Value]) -> Option<(String, String)> {
    let form = AskForm::new("", html)?;
    let answer = |entry: &Value| {
        entry["answer"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or("No selection")
            .to_owned()
    };
    let mut parts: Vec<String> = form
        .plans
        .iter()
        .map(
            |plan| match answers.iter().find(|a| a["name"] == plan.name.as_str()) {
                Some(entry) => format!("{}\n{}", plan.question, answer(entry)),
                None => plan.question.clone(),
            },
        )
        .collect();
    parts.extend(form.trailing.iter().cloned());
    // An answer OMP reports under a name this parse did not find is still part of the record.
    for entry in answers {
        if form
            .plans
            .iter()
            .any(|plan| entry["name"] == plan.name.as_str())
        {
            continue;
        }
        let question = [&entry["question"], &entry["name"]]
            .into_iter()
            .find_map(|v| v.as_str().filter(|text| !text.trim().is_empty()))
            .unwrap_or("Question");
        parts.push(format!("{question}\n{}", answer(entry)));
    }
    Some((form.title, parts.join("\n\n")))
}

impl AskForm {
    /// `None` when the form has no named control: OMP then returns without a dialog.
    pub(super) fn new(tool: &str, html: &str) -> Option<Self> {
        let form = parse::parse(html);
        let titles = form.titles();
        let mut contexts: Vec<Vec<String>> = vec![Vec::new(); form.fields.len()];
        let mut pending: Vec<String> = Vec::new();
        for section in &form.sections {
            if !section.title.is_empty() {
                pending.push(section.title.clone());
            }
            for block in &section.blocks {
                match block {
                    Block::Text(text) | Block::Other(text) => {
                        if !text.trim().is_empty() {
                            pending.push(text.clone());
                        }
                    }
                    Block::Field(index) if !form.fields[*index].name.is_empty() => {
                        contexts[*index] = std::mem::take(&mut pending);
                    }
                    Block::Field(_) => {}
                }
            }
        }
        let mut headers: Vec<String> = Vec::new();
        let mut plans = Vec::new();
        for (index, field) in form.fields.iter().enumerate() {
            if field.name.is_empty() {
                continue;
            }
            let title = titles[index].clone();
            let mut lines: Vec<&str> = contexts[index]
                .iter()
                .map(String::as_str)
                .filter(|line| parse::collapse(line) != title)
                .collect();
            lines.push(&title);
            let header = unique(
                &short(if title.is_empty() {
                    &field.name
                } else {
                    &title
                }),
                &headers,
            );
            headers.push(header.clone());
            let kind = match &field.kind {
                Kind::Choice {
                    multi,
                    other,
                    options,
                    ..
                } => {
                    let mut labels: Vec<String> = Vec::new();
                    let mut values: Vec<String> = Vec::new();
                    let mut defaults = BTreeSet::new();
                    for option in options.iter().filter(|o| !o.disabled) {
                        let label = parse::collapse(&option.label);
                        let label = if label.is_empty() {
                            option.value.clone()
                        } else {
                            label
                        };
                        if option.checked {
                            defaults.insert(labels.len());
                        }
                        labels.push(unique(&label, &labels));
                        values.push(option.value.clone());
                    }
                    PlanKind::Choice {
                        multi: *multi,
                        required: field.required,
                        locked: options.iter().filter(|o| o.disabled && o.checked).count(),
                        other: other.then(|| unique(OTHER, &labels)),
                        labels,
                        values,
                        array: *multi && options.len() > 1,
                        defaults,
                    }
                }
                Kind::Text {
                    multiline,
                    value,
                    placeholder,
                } => PlanKind::Text {
                    editor: *multiline || !value.is_empty(),
                    value: value.clone(),
                    placeholder: placeholder.clone(),
                    required: field.required,
                },
            };
            plans.push(Plan {
                name: field.name.clone(),
                question: lines.join("\n\n"),
                title,
                header,
                kind,
            });
        }
        let first = plans.first()?;
        let title = form
            .sections
            .iter()
            .map(|s| s.title.as_str())
            .find(|t| !t.is_empty())
            .unwrap_or(&first.title)
            .to_owned();
        Some(Self {
            tool: tool.to_owned(),
            html: html.to_owned(),
            plans,
            title,
            trailing: pending,
            stage: Stage::Waiting,
        })
    }

    pub(super) fn shows(&self, request: &str) -> bool {
        match &self.stage {
            Stage::Shown { request: id, .. } | Stage::Form { request: id } => id == request,
            Stage::Waiting | Stage::Replaying(_) => false,
        }
    }

    /// The card for OMP's `form` dialog `id`, which the card's answer settles whole.
    pub(super) fn show(&mut self, provider: &str, id: &str) -> Value {
        self.stage = Stage::Form {
            request: id.to_owned(),
        };
        self.request(provider, id)
    }

    pub(super) fn dialog(&mut self, provider: &str, event: &Value) -> Step {
        match &mut self.stage {
            Stage::Waiting => {
                let start = Replay::new(&self.plans, Vec::new());
                if let Err(reason) = start.expected(&self.plans[0]).check(event) {
                    return Step::Abandon(reason);
                }
                let id = event["id"].as_str().unwrap_or("").to_owned();
                let request = self.request(provider, &id);
                self.stage = Stage::Shown {
                    request: id,
                    held: event.clone(),
                };
                Step::Show(request)
            }
            Stage::Shown { .. } | Stage::Form { .. } => {
                Step::Abandon("OMP asked again before the form was answered".into())
            }
            Stage::Replaying(replay) => replay.next(&self.plans, event),
        }
    }

    /// Answers the `form` dialog, or the held first per-field dialog, from the card's response:
    /// its question answers, or `formData` from a client that showed the form's HTML.
    pub(super) fn submit(&mut self, response: &Value) -> Result<Submitted, String> {
        let held = match &self.stage {
            Stage::Shown { held, .. } => Some(held.clone()),
            Stage::Form { .. } => None,
            Stage::Waiting | Stage::Replaying(_) => {
                return Err("The form is not waiting for an answer".into());
            }
        };
        if response["behavior"] != "allow" {
            return Ok(Submitted::Cancelled);
        }
        let targets = match response.get("formData") {
            Some(form) => self.form_targets(form)?,
            None => self.targets(response)?,
        };
        let Some(held) = held else {
            return Ok(Submitted::Data(json!({ "data": self.data(&targets)? })));
        };
        let mut replay = Replay::new(&self.plans, targets);
        match replay.next(&self.plans, &held) {
            Step::Reply(body) => {
                self.stage = Stage::Replaying(replay);
                Ok(Submitted::Reply(body))
            }
            Step::Abandon(reason) => Err(reason),
            _ => Err("The form's first step could not be answered".into()),
        }
    }

    fn request(&self, provider: &str, id: &str) -> Value {
        let last = self.plans.len() - 1;
        let questions: Vec<Value> = self
            .plans
            .iter()
            .enumerate()
            .map(|(index, plan)| {
                let mut text = plan.question.clone();
                if index == last && !self.trailing.is_empty() {
                    text = format!("{text}\n\n{}", self.trailing.join("\n\n"));
                }
                match &plan.kind {
                    PlanKind::Choice {
                        multi,
                        required,
                        labels,
                        defaults,
                        other,
                        ..
                    } => {
                        let options: Vec<Value> = labels
                            .iter()
                            .enumerate()
                            .map(|(i, label)| {
                                if defaults.contains(&i) {
                                    json!({"label":label,"default":true})
                                } else {
                                    json!({"label":label})
                                }
                            })
                            .collect();
                        json!({"question":text,"header":plan.header,"name":plan.name,"options":options,"multiSelect":multi,"allowOther":other.is_some(),"allowEmpty":*multi && !required})
                    }
                    PlanKind::Text {
                        value,
                        placeholder,
                        required,
                        ..
                    } => {
                        let mut q = json!({"question":text,"header":plan.header,"name":plan.name,"options":[],"multiSelect":false,"allowEmpty":!required});
                        if !placeholder.is_empty() {
                            q["placeholder"] = json!(placeholder);
                        }
                        if !value.is_empty() {
                            q["defaultText"] = json!(value);
                        }
                        q
                    }
                }
            })
            .collect();
        json!({"id":id,"provider":provider,"name":"OMP ask","kind":"question","title":self.title,"input":{"questions":questions,"html":self.html},"metadata":{"ompAskForm":true}})
    }

    /// Targets from the data of the form shown as HTML: `values` holds every submitted
    /// control value under its name (an unchecked box or unselected radio submits nothing),
    /// `other` the text typed into the Other answer a client adds to a choice that allows one.
    fn form_targets(&self, form: &Value) -> Result<Vec<Target>, String> {
        self.plans
            .iter()
            .map(|plan| {
                let submitted: Vec<&str> = form["values"][&plan.name]
                    .as_array()
                    .map(|values| values.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let custom = form["other"][&plan.name]
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                match &plan.kind {
                    PlanKind::Choice {
                        multi,
                        values,
                        other,
                        ..
                    } => {
                        if let Some(text) = custom.as_ref().filter(|_| other.is_none()) {
                            return Err(format!("“{}” has no option “{text}”", plan.title));
                        }
                        let picks = submitted
                            .iter()
                            .map(|value| {
                                values.iter().position(|v| v == value).ok_or_else(|| {
                                    format!("“{}” has no option “{value}”", plan.title)
                                })
                            })
                            .collect::<Result<BTreeSet<usize>, String>>()?;
                        if *multi {
                            return Ok(Target::Multi {
                                picks,
                                other: custom,
                            });
                        }
                        match (custom, picks.first()) {
                            (Some(text), _) => Ok(Target::Single(Pick::Other(text))),
                            (None, Some(&index)) => Ok(Target::Single(Pick::Option(index))),
                            (None, None) => Err(format!("Answer “{}”", plan.title)),
                        }
                    }
                    PlanKind::Text { .. } => Ok(Target::Text(
                        submitted.first().copied().unwrap_or_default().to_owned(),
                    )),
                }
            })
            .collect()
    }

    fn targets(&self, response: &Value) -> Result<Vec<Target>, String> {
        self.plans
            .iter()
            .map(|plan| match &plan.kind {
                PlanKind::Choice {
                    multi,
                    labels,
                    defaults,
                    other,
                    ..
                } => {
                    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                    let answer = question_answers::read(response, &plan.header, &refs, *multi);
                    if let Some(text) = answer.as_ref().and_then(|a| a.other.as_ref())
                        && other.is_none()
                    {
                        return Err(format!("“{}” has no option “{text}”", plan.title));
                    }
                    let picks = match &answer {
                        Some(a) => a
                            .options
                            .iter()
                            .map(|label| {
                                refs.iter().position(|l| l == label).ok_or_else(|| {
                                    format!("“{}” has no option “{label}”", plan.title)
                                })
                            })
                            .collect::<Result<BTreeSet<usize>, String>>()?,
                        None => defaults.clone(),
                    };
                    let custom = answer.and_then(|a| a.other);
                    if *multi {
                        return Ok(Target::Multi {
                            picks,
                            other: custom,
                        });
                    }
                    let pick = match (picks.first(), custom) {
                        (_, Some(text)) => Pick::Other(text),
                        (Some(&index), None) => Pick::Option(index),
                        (None, None) => defaults
                            .first()
                            .map(|&index| Pick::Option(index))
                            .ok_or_else(|| format!("Answer “{}”", plan.title))?,
                    };
                    Ok(Target::Single(pick))
                }
                PlanKind::Text { value, .. } => Ok(Target::Text(
                    question_answers::read(response, &plan.header, &[], false)
                        .map_or_else(|| value.clone(), |a| a.other.unwrap_or_default()),
                )),
            })
            .collect()
    }

    /// The form data the answers submit, in the shape OMP's `form` dialog validates: each
    /// field's value under its name, an array for a checkbox group or multi-select of several
    /// options, and nothing for a lone checkbox left unchecked. A required field left empty is
    /// refused, so the card stays open instead of OMP failing the ask.
    fn data(&self, targets: &[Target]) -> Result<Map<String, Value>, String> {
        let mut data = Map::new();
        for (plan, target) in self.plans.iter().zip(targets) {
            let value = match (&plan.kind, target) {
                (PlanKind::Choice { values, .. }, Target::Single(Pick::Option(index))) => {
                    Some(json!(values[*index]))
                }
                (PlanKind::Choice { .. }, Target::Single(Pick::Other(text))) => Some(json!(text)),
                (
                    PlanKind::Choice {
                        values,
                        array,
                        required,
                        ..
                    },
                    Target::Multi { picks, other },
                ) => {
                    // OMP refuses an entry twice: an option value repeated, or Other text equal to one.
                    let mut chosen: Vec<&str> = Vec::new();
                    for entry in picks.iter().map(|&i| values[i].as_str()).chain(other.as_deref()) {
                        if !chosen.contains(&entry) {
                            chosen.push(entry);
                        }
                    }
                    if *required && chosen.is_empty() {
                        return Err(format!("Answer “{}”", plan.title));
                    }
                    if *array {
                        Some(json!(chosen))
                    } else {
                        chosen.first().map(|value| json!(value))
                    }
                }
                (PlanKind::Text { required, .. }, Target::Text(text)) => {
                    if *required && text.trim().is_empty() {
                        return Err(format!("Answer “{}”", plan.title));
                    }
                    Some(json!(text))
                }
                _ => unreachable!("targets follow the plans"),
            };
            if let Some(value) = value {
                data.insert(plan.name.clone(), value);
            }
        }
        Ok(data)
    }
}

/// Replays the submitted form through OMP's per-field dialogs.
struct Replay {
    targets: Vec<Target>,
    index: usize,
    /// OMP is asking for the custom text after its Other option was picked.
    other_input: bool,
    checked: BTreeSet<usize>,
    other_set: bool,
    /// The text field answered last, which OMP may re-ask with a validation message.
    last_text: Option<usize>,
}

/// The dialog OMP sends next.
struct Expected<'a> {
    method: &'static str,
    title: String,
    options: Option<Vec<&'a str>>,
    done: bool,
    prefill: Option<&'a str>,
}

impl Expected<'_> {
    /// The finish option's label, when the dialog matches.
    fn check(&self, event: &Value) -> Result<Option<String>, String> {
        let method = event["method"].as_str().unwrap_or("");
        let title = event["title"].as_str().unwrap_or("");
        if method != self.method || prompt_line(title) != self.title {
            return Err(format!(
                "expected {} “{}”, got {method} “{title}”",
                self.method, self.title
            ));
        }
        if let Some(prefill) = self.prefill
            && event["prefill"].as_str().unwrap_or("") != prefill
        {
            return Err(format!("unexpected prefill for “{title}”"));
        }
        let Some(expected) = &self.options else {
            return Ok(None);
        };
        let got: Vec<&str> = event["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let count = expected.len();
        let done = self.done.then(|| got.get(count).copied()).flatten();
        let fits = got.len() == count + usize::from(self.done)
            && got[..count.min(got.len())] == expected[..]
            && done.is_none_or(|label| label.ends_with(DONE));
        if !fits {
            return Err(format!("unexpected options for “{title}”: {got:?}"));
        }
        Ok(done.map(str::to_owned))
    }
}

impl Replay {
    fn new(plans: &[Plan], targets: Vec<Target>) -> Self {
        let mut replay = Self {
            targets,
            index: 0,
            other_input: false,
            checked: BTreeSet::new(),
            other_set: false,
            last_text: None,
        };
        replay.reset(plans);
        replay
    }

    fn reset(&mut self, plans: &[Plan]) {
        self.other_input = false;
        self.other_set = false;
        self.checked = match plans.get(self.index).map(|p| &p.kind) {
            Some(PlanKind::Choice {
                multi: true,
                defaults,
                ..
            }) => defaults.clone(),
            _ => BTreeSet::new(),
        };
    }

    fn advance(&mut self, plans: &[Plan]) {
        self.index += 1;
        self.reset(plans);
    }

    fn expected<'a>(&self, plan: &'a Plan) -> Expected<'a> {
        let input = || Expected {
            method: "input",
            title: plan.title.clone(),
            options: None,
            done: false,
            prefill: None,
        };
        match &plan.kind {
            _ if self.other_input => input(),
            PlanKind::Choice {
                multi,
                required,
                labels,
                locked,
                other,
                ..
            } => {
                let mut options: Vec<&str> = labels.iter().map(String::as_str).collect();
                options.extend(other.as_deref());
                let count = self.checked.len() + locked + usize::from(self.other_set);
                let title = if *multi && count > 0 {
                    format!("({count} selected) {}", plan.title)
                } else {
                    plan.title.clone()
                };
                Expected {
                    method: "select",
                    title,
                    options: Some(options),
                    done: *multi && (!required || count > 0),
                    prefill: None,
                }
            }
            PlanKind::Text { editor: false, .. } => input(),
            PlanKind::Text { value, .. } => Expected {
                method: "editor",
                title: plan.title.clone(),
                options: None,
                done: false,
                prefill: Some(value),
            },
        }
    }

    fn next(&mut self, plans: &[Plan], event: &Value) -> Step {
        let checked = plans
            .get(self.index)
            .map(|plan| self.expected(plan).check(event));
        let done = match checked {
            Some(Ok(done)) => done,
            mismatch => {
                let title = prompt_line(event["title"].as_str().unwrap_or(""));
                if let Some(previous) = self.last_text
                    && matches!(event["method"].as_str(), Some("input" | "editor"))
                    && title
                        .strip_prefix(plans[previous].title.as_str())
                        .and_then(|rest| rest.strip_prefix(" ("))
                        .is_some_and(|rest| rest.ends_with(')'))
                {
                    return Step::Ask;
                }
                return match mismatch {
                    Some(Err(reason)) => Step::Abandon(reason),
                    _ => Step::Finished,
                };
            }
        };
        self.last_text = None;
        let plan = &plans[self.index];
        let reply = |value: &str| Step::Reply(json!({"value":value}));
        match (&plan.kind, self.targets[self.index].clone()) {
            (PlanKind::Choice { labels, other, .. }, Target::Single(pick)) => match pick {
                Pick::Other(text) if self.other_input => {
                    self.advance(plans);
                    reply(&text)
                }
                Pick::Other(_) => {
                    self.other_input = true;
                    reply(other.as_deref().unwrap_or(OTHER))
                }
                Pick::Option(index) => {
                    let label = labels[index].clone();
                    self.advance(plans);
                    reply(&label)
                }
            },
            (PlanKind::Choice { labels, other, .. }, Target::Multi { picks, other: text }) => {
                if self.other_input {
                    self.other_input = false;
                    self.other_set = true;
                    return reply(text.as_deref().unwrap_or(""));
                }
                if let Some(index) =
                    (0..labels.len()).find(|i| self.checked.contains(i) != picks.contains(i))
                {
                    if !self.checked.remove(&index) {
                        self.checked.insert(index);
                    }
                    return reply(&labels[index]);
                }
                if text.is_some() != self.other_set {
                    if self.other_set {
                        self.other_set = false;
                    } else {
                        self.other_input = true;
                    }
                    return reply(other.as_deref().unwrap_or(OTHER));
                }
                match done {
                    Some(done) => {
                        self.advance(plans);
                        reply(&done)
                    }
                    None => Step::Abandon(format!("“{}” needs a selection", plan.title)),
                }
            }
            (PlanKind::Text { .. }, Target::Text(text)) => {
                self.last_text = Some(self.index);
                self.advance(plans);
                reply(&text)
            }
            _ => Step::Abandon(format!("no answer for “{}”", plan.title)),
        }
    }
}

/// The line of an OMP dialog title that names the step. OMP 19 puts the field's form text
/// before it, after a blank line (`tools/ask.ts` `promptTitle`); the line itself is one line.
fn prompt_line(title: &str) -> &str {
    title.rsplit("\n\n").next().unwrap_or(title)
}

fn unique(base: &str, taken: &[String]) -> String {
    let mut candidate = base.to_owned();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base} ({n})");
        n += 1;
    }
    candidate
}

fn short(text: &str) -> String {
    if text.chars().count() <= HEADER_CHARS {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(HEADER_CHARS - 1).collect();
    out.push('…');
    out
}
