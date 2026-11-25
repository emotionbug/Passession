use super::super::{Mapper, tool_item};
use super::parse::{self, Kind};
use super::{AskForm, PlanKind};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const DONE_LABEL: &str = "✓ Done selecting";

/// OMP's RPC fallback for an `ask` form (`tools/ask.ts` `X`, `B` and `Q`): one dialog per
/// named field, a checkbox group toggled one option per `select`. `None` when a dialog is
/// cancelled.
fn omp(html: &str, mut answer: impl FnMut(Value) -> Value) -> Option<Map<String, Value>> {
    let form = parse::parse(html);
    let titles = form.titles();
    let mut data = Map::new();
    let mut next_id = 0;
    let mut ask = |mut dialog: Value| -> Option<Value> {
        next_id += 1;
        dialog["type"] = json!("extension_ui_request");
        dialog["id"] = json!(format!("dialog-{next_id}"));
        let reply = answer(dialog);
        (reply["cancelled"] != true).then(|| reply["value"].clone())
    };
    for (index, field) in form.fields.iter().enumerate() {
        if field.name.is_empty() {
            continue;
        }
        let title = &titles[index];
        match &field.kind {
            Kind::Choice {
                multi,
                other,
                options,
                ..
            } => {
                let enabled: Vec<usize> = (0..options.len())
                    .filter(|&i| !options[i].disabled)
                    .collect();
                let mut labels: Vec<String> = Vec::new();
                for &i in &enabled {
                    let base = parse::collapse(&options[i].label);
                    let base = if base.is_empty() {
                        options[i].value.clone()
                    } else {
                        base
                    };
                    labels.push(dedupe(&base, &labels));
                }
                let other_label = other.then(|| dedupe("Other (type your own)", &labels));
                let mut offered = labels.clone();
                offered.extend(other_label.clone());
                let mut checked: BTreeSet<usize> =
                    (0..options.len()).filter(|&i| options[i].checked).collect();
                let mut custom: Option<String> = None;
                if *multi {
                    loop {
                        let count = checked.len() + usize::from(custom.is_some());
                        let mut shown = offered.clone();
                        if !field.required || count > 0 {
                            shown.push(DONE_LABEL.into());
                        }
                        let shown_title = if count > 0 {
                            format!("({count} selected) {title}")
                        } else {
                            title.clone()
                        };
                        let value =
                            ask(json!({"method":"select","title":shown_title,"options":shown}))?;
                        let value = value.as_str()?.to_owned();
                        if value == DONE_LABEL {
                            break;
                        }
                        if Some(&value) == other_label.as_ref() {
                            if custom.take().is_some() {
                                continue;
                            }
                            let text = ask(json!({"method":"input","title":title}))?;
                            custom = Some(text.as_str().unwrap_or("").trim().to_owned())
                                .filter(|t| !t.is_empty());
                            continue;
                        }
                        let option = enabled[labels.iter().position(|l| *l == value)?];
                        if !checked.remove(&option) {
                            checked.insert(option);
                        }
                    }
                } else {
                    loop {
                        let value =
                            ask(json!({"method":"select","title":title,"options":offered}))?;
                        let value = value.as_str()?.to_owned();
                        if Some(&value) == other_label.as_ref() {
                            let text = ask(json!({"method":"input","title":title}))?;
                            let text = text.as_str().unwrap_or("").trim().to_owned();
                            if text.is_empty() {
                                continue;
                            }
                            checked.clear();
                            custom = Some(text);
                            break;
                        }
                        checked =
                            BTreeSet::from([enabled[labels.iter().position(|l| *l == value)?]]);
                        break;
                    }
                }
                let mut values: Vec<Value> = options
                    .iter()
                    .enumerate()
                    .filter(|(i, o)| checked.contains(i) && !o.disabled)
                    .map(|(_, o)| json!(o.value))
                    .collect();
                if *multi && options.len() > 1 {
                    values.extend(custom.map(Value::from));
                    data.insert(field.name.clone(), Value::Array(values));
                } else if let Some(value) = custom
                    .map(Value::from)
                    .or_else(|| values.into_iter().next())
                {
                    data.insert(field.name.clone(), value);
                }
            }
            Kind::Text {
                multiline, value, ..
            } => {
                let editor = *multiline || !value.is_empty();
                let mut error = None;
                loop {
                    let shown_title = match error {
                        Some(error) => format!("{title} ({error})"),
                        None => title.clone(),
                    };
                    let reply = if editor {
                        ask(json!({"method":"editor","title":shown_title,"prefill":value}))?
                    } else {
                        ask(json!({"method":"input","title":shown_title}))?
                    };
                    let text = reply.as_str().unwrap_or("").to_owned();
                    error = (field.required && text.trim().is_empty()).then_some("Required");
                    if error.is_none() {
                        data.insert(field.name.clone(), json!(text));
                        break;
                    }
                }
            }
        }
    }
    Some(data)
}

fn dedupe(base: &str, taken: &[String]) -> String {
    let mut candidate = base.to_owned();
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{base} ({n})");
        n += 1;
    }
    candidate
}

/// Paseo between OMP and the user: the form card is answered by `form`, any step-by-step card
/// by the next of `steps`.
struct Harness {
    mapper: Mapper,
    cards: Vec<Value>,
    form: Box<dyn FnMut(&Value) -> Value>,
    steps: Vec<Value>,
}

impl Harness {
    fn new(html: &str, form: impl FnMut(&Value) -> Value + 'static) -> Self {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":html}}));
        Self {
            mapper,
            cards: Vec::new(),
            form: Box::new(form),
            steps: Vec::new(),
        }
    }

    fn answer(&mut self, dialog: Value) -> Value {
        let events = self.mapper.map(&dialog);
        let Some(card) = events
            .iter()
            .find(|e| e["type"] == "permission_requested")
            .map(|e| e["request"].clone())
        else {
            let mut replies = self.mapper.drain_responses();
            assert_eq!(
                replies.len(),
                1,
                "dialog neither shown nor answered: {dialog}"
            );
            return replies.remove(0);
        };
        self.cards.push(card.clone());
        let response = if card["metadata"]["ompAskForm"] == true {
            (self.form)(&card)
        } else {
            assert!(!self.steps.is_empty(), "unexpected step card: {card}");
            self.steps.remove(0)
        };
        let reply = self
            .mapper
            .permission_response(card["id"].as_str().unwrap(), &response)
            .unwrap();
        assert!(self.mapper.drain_responses().is_empty());
        reply
    }
}

/// The option of `question` whose label contains `needle`.
fn option(card: &Value, question: usize, needle: &str) -> String {
    card["input"]["questions"][question]["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["label"].as_str())
        .find(|label| label.contains(needle))
        .unwrap_or_else(|| panic!("no option containing {needle:?}"))
        .to_owned()
}

fn header(card: &Value, question: usize) -> String {
    card["input"]["questions"][question]["header"]
        .as_str()
        .unwrap()
        .to_owned()
}

const CLEANUP: &str = r#"<form><p>현재 여유 공간은 <strong>3.1GiB</strong>입니다.</p><fieldset><legend>Passession Wine 범위</legend><label><input type="radio" name="wine" value="all" checked> <code>~/.cache/passession-wine</code> 전체 약 15GiB</label><label><input type="radio" name="wine" value="target"> 빌드 결과 <code>target</code>만</label></fieldset><fieldset><legend>추가 정리 항목</legend><label><input type="checkbox" name="items" value="codex_old" checked> Codex 구버전</label><label><input type="checkbox" name="items" value="pkg_caches" checked> 캐시: <code>~/.npm/_npx</code>, <code>~/.cache/zig</code></label><label><input type="checkbox" name="items" value="old_daemon" checked> 과거 데몬 기록</label><label><input type="checkbox" name="items" value="review" checked> 리뷰 산출물</label><label><input type="checkbox" name="items" value="latest_backup"> 최신 롤백 백업</label></fieldset><p>예상 확보량: 약 27GiB.</p></form>"#;

#[test]
fn form_card_unchecks_defaults_that_the_user_cleared() {
    let mut harness = Harness::new(CLEANUP, |card| {
        let mut selections = Map::new();
        selections.insert(
            header(card, 0),
            json!({"options":[option(card, 0, "빌드 결과")]}),
        );
        selections.insert(
            header(card, 1),
            json!({"options":[option(card, 1, "최신 롤백")]}),
        );
        json!({"behavior":"allow","updatedInput":{"answers":{},"selections":selections}})
    });
    let data = omp(CLEANUP, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(data["wine"], "target");
    assert_eq!(data["items"], json!(["latest_backup"]));

    assert_eq!(harness.cards.len(), 1, "the whole form is one card");
    let card = &harness.cards[0];
    let items = &card["input"]["questions"][1];
    assert_eq!(items["multiSelect"], true);
    assert_eq!(items["allowOther"], true);
    let defaults: Vec<bool> = items["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["default"] == true)
        .collect();
    assert_eq!(defaults, [true, true, true, true, false]);
    assert!(
        card["input"]["questions"][0]["question"]
            .as_str()
            .unwrap()
            .starts_with("현재 여유 공간은 3.1GiB입니다.")
    );
    assert!(
        items["question"]
            .as_str()
            .unwrap()
            .ends_with("예상 확보량: 약 27GiB.")
    );
}

#[test]
fn untouched_form_keeps_its_defaults() {
    let mut harness = Harness::new(
        CLEANUP,
        |_| json!({"behavior":"allow","updatedInput":{"answers":{}}}),
    );
    let data = omp(CLEANUP, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(data["wine"], "all");
    assert_eq!(
        data["items"],
        json!(["codex_old", "pkg_caches", "old_daemon", "review"])
    );
}

#[test]
fn ui_updates_preserve_the_form_before_during_and_after_submission() {
    for method in ["setStatus", "setWidget", "setTitle", "setEditorText"] {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":CLEANUP}}));
        let update = json!({"type":"extension_ui_request","id":"ui-update","method":method,
            "statusKey":"form-test","statusText":"Status while asking",
            "widgetKey":"form-test","widgetLines":["Widget while asking"]});
        mapper.map(&update);
        let mut shown = 0;
        let data = omp(CLEANUP, |dialog| {
            let events = mapper.map(&dialog);
            mapper.map(&update);
            let Some(card) = events
                .iter()
                .find(|e| e["type"] == "permission_requested")
                .map(|e| &e["request"])
            else {
                let replies = mapper.drain_responses();
                assert_eq!(replies.len(), 1, "{method}: {dialog}");
                return replies.into_iter().next().unwrap();
            };
            shown += 1;
            assert_eq!(card["metadata"]["ompAskForm"], true, "{method}");
            assert_eq!(mapper.pending_permissions(), vec![card.clone()]);
            let response = json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{
                header(card, 0): {"options":[option(card, 0, "빌드 결과")]},
                header(card, 1): {"options":[option(card, 1, "최신 롤백")]}
            }}});
            mapper
                .permission_response(card["id"].as_str().unwrap(), &response)
                .unwrap()
        })
        .unwrap_or_else(|| panic!("{method} cancelled the submitted form"));
        assert_eq!(data["wine"], "target");
        assert_eq!(data["items"], json!(["latest_backup"]));
        assert_eq!(shown, 1, "{method} interrupted the form replay");
        assert!(mapper.pending_permissions().is_empty());
        assert!(mapper.drain_responses().is_empty());
    }
}

#[test]
fn joined_answers_keep_comma_labels_custom_text_and_editor_values() {
    const HTML: &str = r#"<form>
        <fieldset><legend>Store</legend>
          <label><input type="radio" name="store" value="sqlite" checked> SQLite: one file, no service</label>
          <label><input type="radio" name="store" value="redis"> Redis</label></fieldset>
        <fieldset><legend>Extras</legend>
          <label><input type="checkbox" name="extras" value="ab"> A, B</label>
          <label><input type="checkbox" name="extras" value="c" checked> C</label></fieldset>
        <label>Days <input type="number" name="days" value="30"></label>
    </form>"#;
    let mut harness = Harness::new(HTML, |card| {
        json!({"behavior":"allow","updatedInput":{"answers":{
            header(card, 0): "Postgres, managed",
            header(card, 1): "A, B, extra, text",
            header(card, 2): "45"
        }}})
    });
    let data = omp(HTML, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(data["store"], "Postgres, managed");
    assert_eq!(data["extras"], json!(["ab", "extra, text"]));
    assert_eq!(data["days"], "45");
    assert_eq!(
        harness.cards[0]["input"]["questions"][2]["defaultText"],
        "30"
    );
}

#[test]
fn validation_reprompt_is_asked_then_the_replay_resumes() {
    const HTML: &str = r#"<form><label>Who <input type="text" name="who" required></label>
        <fieldset><legend>Ping</legend>
          <label><input type="checkbox" name="ping" value="a"> A</label>
          <label><input type="checkbox" name="ping" value="b"> B</label></fieldset></form>"#;
    let mut harness = Harness::new(HTML, |card| {
        json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{
            header(card, 0): {"options":[],"other":""},
            header(card, 1): {"options":["B"]}
        }}})
    });
    harness
        .steps
        .push(json!({"behavior":"allow","updatedInput":{"answers":{"Response":"Kim"}}}));
    let data = omp(HTML, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(data["who"], "Kim");
    assert_eq!(data["ping"], json!(["b"]));
    assert_eq!(harness.cards.len(), 2);
    assert_eq!(
        harness.cards[1]["input"]["questions"][0]["question"],
        "Who (Required)"
    );
}

#[test]
fn unpredicted_dialog_is_asked_step_by_step_without_an_automatic_answer() {
    let mut mapper = Mapper::new("omp", "omp");
    mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":CLEANUP}}));
    let events = mapper.map(&json!({"type":"extension_ui_request","id":"d1","method":"select","title":"Passession Wine 범위","options":["Something else","Other (type your own)"]}));
    let card = &events[0]["request"];
    assert_ne!(card["metadata"]["ompAskForm"], true);
    assert!(
        card["input"]["questions"][0]["question"]
            .as_str()
            .unwrap()
            .starts_with("Paseo could not match this OMP form step")
    );
    assert!(mapper.drain_responses().is_empty());
}

#[test]
fn agent_cancel_withdraws_the_form_card() {
    for first in [omp_first_dialog(CLEANUP), form_dialog(CLEANUP)] {
        let mut mapper = Mapper::new("omp", "omp");
        mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":CLEANUP}}));
        let events = mapper.map(&first);
        assert_eq!(events[0]["request"]["metadata"]["ompAskForm"], true);
        let events = mapper.map(
            &json!({"type":"extension_ui_request","id":"c1","method":"cancel","targetId":first["id"]}),
        );
        assert_eq!(events[0]["type"], "permission_resolved");
        assert_eq!(events[0]["requestId"], first["id"]);
        assert!(mapper.pending_permissions().is_empty());
        assert!(
            mapper
                .permission_response(first["id"].as_str().unwrap(), &json!({"behavior":"allow"}))
                .is_err()
        );
    }
}

/// OMP 19.1+ (`rpc-mode.ts` `requestRpcFormDialog`) sends the whole form as one dialog.
fn form_dialog(html: &str) -> Value {
    json!({"type":"extension_ui_request","id":"form-1","method":"form","html":html,"timeout":300000})
}

/// The card Paseo shows for OMP's `form` dialog.
fn form_card(mapper: &mut Mapper, html: &str) -> Value {
    mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":html}}));
    let events = mapper.map(&form_dialog(html));
    let card = events
        .iter()
        .find(|e| e["type"] == "permission_requested")
        .map(|e| e["request"].clone())
        .expect("the form dialog shows a card");
    assert_eq!(card["metadata"]["ompAskForm"], true);
    assert_eq!(mapper.pending_permissions(), vec![card.clone()]);
    card
}

#[test]
fn form_dialog_answers_with_the_data_the_replay_submits() {
    let recordings = recordings();
    for (name, response, steps) in recorded_cases() {
        if !steps.is_empty() {
            continue;
        }
        let html = recordings[name]["html"].as_str().unwrap();
        let replay_response = response.clone();
        let mut harness = Harness::new(html, move |_| replay_response.clone());
        let replayed = omp(html, |dialog| harness.answer(dialog)).unwrap();

        let mut mapper = Mapper::new("omp", "omp");
        let card = form_card(&mut mapper, html);
        let reply = mapper
            .permission_response(card["id"].as_str().unwrap(), &response)
            .unwrap();
        assert_eq!(
            reply,
            json!({"type":"extension_ui_response","id":"form-1","data":replayed}),
            "{name}"
        );
        assert!(mapper.pending_permissions().is_empty(), "{name}");
        assert!(mapper.drain_responses().is_empty(), "{name}");
    }
}

#[test]
fn form_dialog_data_follows_html_form_submission() {
    const HTML: &str = r#"<form>
        <label><input type="checkbox" name="agree" value="yes"> I agree</label>
        <label><input type="checkbox" name="lone"> Lone</label>
        <label>Size <select name="size"><option value="s">Small</option><option value="l" selected>Large</option></select></label>
        <fieldset><legend>Tags</legend>
          <label><input type="checkbox" name="tags" value="x" checked disabled> Locked</label>
          <label><input type="checkbox" name="tags" value="y"> Y</label></fieldset>
        <label>Who <input type="text" name="who" required></label>
    </form>"#;
    let mut mapper = Mapper::new("omp", "omp");
    let card = form_card(&mut mapper, HTML);
    let id = card["id"].as_str().unwrap();
    let response = |who: &str| {
        let selections = json!({
            header(&card, 0): {"options":["I agree"]},
            header(&card, 1): {"options":[]},
            header(&card, 3): {"options":["Y"]}
        });
        json!({"behavior":"allow","updatedInput":{"answers":{header(&card, 4): who},"selections":selections}})
    };
    assert!(mapper.permission_response(id, &response("")).is_err());
    assert_eq!(
        mapper.pending_permissions().len(),
        1,
        "an empty required field keeps the card"
    );

    let reply = mapper.permission_response(id, &response("Kim")).unwrap();
    // The unchecked lone checkbox submits nothing; the disabled option never submits.
    assert_eq!(
        reply["data"],
        json!({"agree":"yes","size":"l","tags":["y"],"who":"Kim"})
    );

    let mut mapper = Mapper::new("omp", "omp");
    let card = form_card(&mut mapper, HTML);
    let reply = mapper
        .permission_response(
            card["id"].as_str().unwrap(),
            &json!({"behavior":"deny","message":"Dismissed by user"}),
        )
        .unwrap();
    assert_eq!(
        reply,
        json!({"type":"extension_ui_response","id":"form-1","cancelled":true})
    );
}

/// The answer of a client that showed the form's HTML: every submitted value by name, and the
/// text of the Other answer it added to a choice.
fn html_answer(values: Value, other: Value) -> Value {
    json!({"behavior":"allow","formData":{"values":values,"other":other}})
}

#[test]
fn html_form_data_answers_the_form_dialog() {
    const HTML: &str = r#"<form>
        <label><input type="checkbox" name="agree" value="yes"> I agree</label>
        <label>Size <select name="size"><option value="s">Small</option><option value="l" selected>Large</option></select></label>
        <fieldset><legend>Tags</legend>
          <label><input type="checkbox" name="tags" value="x" checked disabled> Locked</label>
          <label><input type="checkbox" name="tags" value="y"> Y</label>
          <label><input type="checkbox" name="tags" value="z"> Z</label></fieldset>
        <label>Who <input type="text" name="who" required></label>
    </form>"#;
    let mut mapper = Mapper::new("omp", "omp");
    let card = form_card(&mut mapper, HTML);
    assert_eq!(
        card["input"]["html"], HTML,
        "the card carries the form as sent"
    );
    let names: Vec<&str> = card["input"]["questions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["agree", "size", "tags", "who"]);
    let id = card["id"].as_str().unwrap();

    let empty_who = html_answer(json!({"size":["s"],"who":[""]}), json!({}));
    assert!(mapper.permission_response(id, &empty_who).is_err());
    let unknown = html_answer(json!({"size":["xl"],"who":["Kim"]}), json!({}));
    assert!(mapper.permission_response(id, &unknown).is_err());
    assert_eq!(
        mapper.pending_permissions().len(),
        1,
        "a refused answer keeps the card"
    );

    let answer = html_answer(
        json!({"agree":["yes"],"size":["s"],"tags":["z","y"],"who":["Kim"]}),
        json!({"tags":"  w  "}),
    );
    let reply = mapper.permission_response(id, &answer).unwrap();
    assert_eq!(
        reply["data"],
        json!({"agree":"yes","size":"s","tags":["y","z","w"],"who":"Kim"})
    );
    assert!(mapper.pending_permissions().is_empty());
}

#[test]
fn html_form_data_drives_the_replay_of_earlier_omp() {
    let mut harness = Harness::new(CLEANUP, |_| {
        html_answer(
            json!({"wine":["target"],"items":["review","codex_old"]}),
            json!({"wine":"   ","items":"logs"}),
        )
    });
    let data = omp(CLEANUP, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(data["wine"], "target");
    assert_eq!(data["items"], json!(["codex_old", "review", "logs"]));
    assert_eq!(harness.cards.len(), 1, "the whole form is one card");

    let mut harness = Harness::new(CLEANUP, |_| {
        html_answer(json!({"items":[]}), json!({"wine":"Only logs"}))
    });
    let data = omp(CLEANUP, |dialog| harness.answer(dialog)).unwrap();
    assert_eq!(
        data["wine"], "Only logs",
        "Other text answers a radio group"
    );
}

#[test]
fn dismissed_form_cancels_the_ask() {
    let mut harness = Harness::new(
        CLEANUP,
        |_| json!({"behavior":"deny","message":"Dismissed by user"}),
    );
    assert!(omp(CLEANUP, |dialog| harness.answer(dialog)).is_none());
}

#[test]
fn invalid_answer_keeps_the_card_pending() {
    let mut mapper = Mapper::new("omp", "omp");
    mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":CLEANUP}}));
    let first = omp_first_dialog(CLEANUP);
    let id = first["id"].as_str().unwrap().to_owned();
    mapper.map(&first);
    let card = mapper.pending_permissions()[0].clone();
    let wrong = json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{header(&card, 1): {"options":["No such item"]}}}});
    assert!(mapper.permission_response(&id, &wrong).is_err());
    assert_eq!(mapper.pending_permissions().len(), 1);
}

/// The first dialog OMP sends for `html`.
fn omp_first_dialog(html: &str) -> Value {
    let mut first = None;
    omp(html, |dialog| {
        first.get_or_insert(dialog);
        json!({"cancelled":true})
    });
    first.unwrap()
}

#[test]
fn parser_follows_omp_label_and_option_rules() {
    const HTML: &str = r#"<form>
        <p>Intro text.</p>
        <label for="n">Display name</label><input id="n" name="display_name">
        <input name="email_address" type="email" placeholder="a@b">
        <input name="aria" aria-label="By aria">
        <fieldset><legend>Size</legend>
          <select name="size"><option disabled>Pick</option><option value="s">Small</option><option>Large</option></select>
        </fieldset>
        <p>Choose colours</p>
        <label><input type="checkbox" name="colour" value="r"> Red</label>
        <label><input type="checkbox" name="colour" value="r2"> Red</label>
        <label><input type="checkbox" name="colour" value="o"> Other colour</label>
        <table><tr><th>Item</th><th>Qty</th></tr><tr><td>Apples</td><td><input name="qty" type="number"></td></tr></table>
        <textarea name="notes">
line</textarea>
        <label><input type="checkbox" name="agree"> I agree</label>
        <input type="hidden" name="secret" value="x"><button>Send</button>
    </form>"#;
    let form = AskForm::new("ask-1", HTML).unwrap();
    let titles: Vec<&str> = form.plans.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Display name",
            "email address",
            "By aria",
            "Size",
            "Choose colours",
            "Qty",
            "notes",
            "I agree"
        ]
    );
    assert_eq!(form.plans[0].question, "Intro text.\n\nDisplay name");
    let choice = |index: usize| match &form.plans[index].kind {
        PlanKind::Choice {
            labels,
            defaults,
            other,
            ..
        } => (labels.clone(), defaults.clone(), other.clone()),
        PlanKind::Text { .. } => panic!("plan {index} is text"),
    };
    assert_eq!(
        choice(3),
        (
            vec!["Small".into(), "Large".into()],
            BTreeSet::from([0]),
            None
        )
    );
    assert_eq!(
        choice(4),
        (
            vec!["Red".into(), "Red (2)".into(), "Other colour".into()],
            BTreeSet::new(),
            None
        )
    );
    assert!(matches!(
        &form.plans[6].kind,
        PlanKind::Text { editor: true, value, .. } if value == "line"
    ));
    assert!(
        matches!(&form.plans[1].kind, PlanKind::Text { editor: false, placeholder, .. } if placeholder == "a@b")
    );
    assert!(AskForm::new("ask-2", "<form><p>No controls</p></form>").is_none());
}

#[test]
fn completed_form_lists_each_answer() {
    let item = tool_item(
        "omp",
        "ask-1",
        "ask",
        &json!({"html":"<form></form>"}),
        "completed",
        &json!({"content":[{"type":"text","text":"done"}],"details":{"answers":[
            {"name":"store","question":"Store","answer":"Redis"},
            {"name":"notes","question":"Notes","answer":""}
        ]}}),
        &Value::Null,
    );
    assert_eq!(
        item["detail"]["text"],
        "Store\nRedis\n\nNotes\nNo selection"
    );
}

#[test]
fn record_shows_what_was_asked_with_or_without_an_answer() {
    const STORE: &str = r#"<form><p>Sessions are lost on restart.</p><fieldset><legend>Session store</legend><label><input type="radio" name="store" value="sqlite"> SQLite</label><label><input type="radio" name="store" value="redis"> Redis</label></fieldset><label>Notes <input type="text" name="notes"></label><p>Applies after a restart.</p></form>"#;
    const CANCELLED: &str = "Ask tool was cancelled by the user";
    let text = |args: Value, status: &str, result: Value| {
        let error = if status == "failed" {
            json!(CANCELLED)
        } else {
            Value::Null
        };
        let item = tool_item("omp", "ask-1", "ask", &args, status, &result, &error);
        assert_eq!(item["error"], error);
        (
            item["detail"]["label"].as_str().unwrap().to_owned(),
            item["detail"]["text"].as_str().unwrap().to_owned(),
        )
    };
    let cancelled = json!({"content":[{"type":"text","text":CANCELLED}],"details":{}});
    let asked = "Sessions are lost on restart.\n\nSession store";

    let form = json!({"html":STORE});
    let answered = json!({"content":[{"type":"text","text":"User answers:\n{}"}],
        "details":{"answers":[
            {"name":"store","question":"Session store","answer":"Redis"},
            {"name":"notes","question":"Notes","answer":""}]}});
    assert_eq!(
        text(form.clone(), "completed", answered),
        (
            "Session store".into(),
            format!("{asked}\nRedis\n\nNotes\nNo selection\n\nApplies after a restart.")
        )
    );
    // The pending and the cancelled record show the question; the cancellation is the error.
    for (status, result) in [("running", Value::Null), ("failed", cancelled.clone())] {
        assert_eq!(
            text(form.clone(), status, result),
            (
                "Session store".into(),
                format!("{asked}\n\nNotes\n\nApplies after a restart.")
            ),
            "{status}"
        );
    }

    let questions = json!({"questions":[
        {"id":"a","question":"Which store?","options":[{"label":"SQLite"},{"label":"Redis"}]},
        {"id":"b","question":"Which region?","options":[{"label":"Seoul"}]}]});
    assert_eq!(
        text(questions.clone(), "failed", cancelled),
        (
            "Which store?".into(),
            "Which store?\n\nWhich region?".into()
        )
    );
    let results = json!({"content":[{"type":"text","text":"User answers"}],"details":{"results":[
        {"id":"a","question":"Which store?","selectedOptions":["Redis"]},
        {"id":"b","question":"Which region?","selectedOptions":[],"customInput":"Tokyo"}]}});
    assert_eq!(
        text(questions, "completed", results).1,
        "Which store?\nRedis\n\nWhich region?\nTokyo"
    );
}

/// Card answers for the forms in `omp_rpc_dialogs.json`: the form card's response, then the
/// answers to any step-by-step cards.
fn recorded_cases() -> [(&'static str, Value, Vec<Value>); 4] {
    [
        (
            "cleanup",
            json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{
                "Passession Wine 범위":{"options":["빌드 결과 target만"]},
                "추가 정리 항목":{"options":["캐시: ~/.npm/_npx, ~/.cache/zig","과거 데몬 기록","리뷰 산출물","최신 롤백 백업"]}
            }}}),
            vec![],
        ),
        (
            "joined",
            json!({"behavior":"allow","updatedInput":{"answers":{
                "Store":"Postgres, managed","Extras":"A, B, extra, text","Days":"45"
            }}}),
            vec![],
        ),
        (
            "reprompt",
            json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{
                "Who":{"options":[],"other":""},"Ping":{"options":["B"]}
            }}}),
            vec![json!({"behavior":"allow","updatedInput":{"answers":{"Response":"Kim"}}})],
        ),
        (
            "context",
            json!({"behavior":"allow","updatedInput":{"answers":{},"selections":{
                "Owner":{"options":[],"other":""},"Channel":{"options":[],"other":"Nightly"}
            }}}),
            vec![json!({"behavior":"allow","updatedInput":{"answers":{"Response":"Kim"}}})],
        ),
    ]
}

fn recordings() -> Value {
    serde_json::from_str(include_str!("omp_rpc_dialogs.json")).unwrap()
}

/// The dialogs a real `omp --mode rpc-ui` sent for these forms, per OMP version, with its
/// accepted answers, in the order OMP sends them: the assistant message with the `ask` call,
/// the first dialog, then the tool's start. Paseo must send every recorded answer itself.
#[test]
fn replay_matches_dialogs_recorded_from_omp() {
    let recordings = recordings();
    for (name, form, steps) in recorded_cases() {
        let recording = &recordings[name];
        let html = &recording["html"];
        for (omp, transcript) in recording["transcripts"].as_object().unwrap() {
            let mut mapper = Mapper::new("omp", "omp");
            mapper.map(
                &json!({"type":"message_end","message":{"role":"assistant","content":[
                {"type":"toolCall","id":"ask-1","name":"ask","arguments":{"html":html}}]}}),
            );
            let mut steps = steps.clone().into_iter();
            let mut form_cards = 0;
            for (index, entry) in transcript.as_array().unwrap().iter().enumerate() {
                let dialog = &entry["dialog"];
                let events = mapper.map(dialog);
                if index == 0 {
                    mapper.map(&json!({"type":"tool_execution_start","toolCallId":"ask-1","toolName":"ask","args":{"html":html}}));
                }
                let reply = match events.iter().find(|e| e["type"] == "permission_requested") {
                    Some(event) => {
                        let card = &event["request"];
                        let response = if card["metadata"]["ompAskForm"] == true {
                            form_cards += 1;
                            form.clone()
                        } else {
                            steps.next().expect("a step answer")
                        };
                        mapper
                            .permission_response(card["id"].as_str().unwrap(), &response)
                            .unwrap()
                    }
                    None => mapper.drain_responses().pop().expect("an automatic answer"),
                };
                assert_eq!(
                    reply["value"], entry["response"]["value"],
                    "{name} on {omp}: {}",
                    dialog["title"]
                );
            }
            assert_eq!(form_cards, 1, "{name} on {omp}");
            assert!(steps.next().is_none(), "{name} on {omp}");
            assert!(mapper.pending_permissions().is_empty(), "{name} on {omp}");
        }
    }
}

/// The same forms against the installed `omp` and its configured model, through Paseo's
/// mapper. Run after an OMP upgrade with `--ignored`; when OMP changes its dialogs, add that
/// run's dialogs to `omp_rpc_dialogs.json` under the new version.
#[test]
#[ignore = "drives the installed omp and its configured model"]
fn installed_omp_accepts_paseo_answers() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let recordings = recordings();
    for (name, form, steps) in recorded_cases() {
        let recording = &recordings[name];
        let html = recording["html"].as_str().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("omp")
            .args(["--mode", "rpc-ui", "--no-session", "--tools", "ask"])
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("omp on PATH");
        let pid = child.id();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(300));
            let _ = Command::new("kill").arg(pid.to_string()).status();
        });
        let mut stdin = child.stdin.take().unwrap();
        let prompt = format!(
            "Call the `ask` tool exactly once. Pass the HTML below as the `html` argument, verbatim, without changing a single character. After the tool returns, reply with only OK.\n\n{html}"
        );
        writeln!(
            stdin,
            "{}",
            json!({"id":"live","type":"prompt","message":prompt})
        )
        .unwrap();
        let mut mapper = Mapper::new("omp", "omp");
        let mut steps = steps.into_iter();
        let mut answers = None;
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let Ok(frame) = serde_json::from_str::<Value>(&line.unwrap()) else {
                continue;
            };
            if frame["type"] == "tool_execution_start" && frame["toolName"] == "ask" {
                assert_eq!(
                    frame["args"]["html"], html,
                    "{name}: the model changed the form"
                );
            }
            if frame["type"] == "tool_execution_end" && frame["toolName"] == "ask" {
                answers = Some(frame["result"]["details"]["answers"].clone());
                break;
            }
            for event in mapper.map(&frame) {
                if event["type"] != "permission_requested" {
                    continue;
                }
                let card = &event["request"];
                let response = if card["metadata"]["ompAskForm"] == true {
                    form.clone()
                } else {
                    steps.next().expect("a step answer")
                };
                let reply = mapper
                    .permission_response(card["id"].as_str().unwrap(), &response)
                    .unwrap();
                writeln!(stdin, "{reply}").unwrap();
            }
            for reply in mapper.drain_responses() {
                writeln!(stdin, "{reply}").unwrap();
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            answers.expect("omp finished the ask"),
            recording["answers"],
            "{name}"
        );
    }
}
