//! MCP form and URL elicitations share the permission card, but not its default approval behavior.
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

#[derive(Debug)]
pub(super) enum Elicitation {
    Form(Form),
    Url(url::Url),
}

#[derive(Debug)]
pub(super) struct Form {
    fields: Vec<Field>,
    validator: jsonschema::Validator,
}

#[derive(Debug)]
struct Field {
    key: String,
    title: String,
    description: Option<String>,
    required: bool,
    input: Input,
    options: Vec<Choice>,
}

#[derive(Debug, PartialEq, Eq)]
enum Input {
    Text,
    Number,
    Single,
    Multi,
}

#[derive(Debug)]
struct Choice {
    label: String,
    value: Value,
}

pub(super) struct Answer {
    action: &'static str,
    content: Option<Value>,
}

impl Answer {
    pub(super) fn native(self, codex: bool) -> Value {
        let mut response = json!({"action":self.action});
        if let Some(content) = self.content {
            response["content"] = content;
        } else if codex {
            response["content"] = Value::Null;
        }
        if codex {
            response["_meta"] = Value::Null;
        }
        response
    }

    /// Do not rebroadcast submitted form values in a permission resolution event.
    pub(super) fn resolution(&self) -> Value {
        match self.action {
            "accept" => json!({"behavior":"allow","selectedActionId":"accept"}),
            "decline" => json!({"behavior":"deny","selectedActionId":"decline"}),
            _ => json!({"behavior":"deny","selectedActionId":"cancel","interrupt":true}),
        }
    }

    pub(super) fn cancel() -> Self {
        Self { action: "cancel", content: None }
    }
}

impl Elicitation {
    pub(super) fn form(raw: &Value, property_order: &Value, openai: bool) -> Result<Self> {
        let schema = object(raw)?;
        keys(schema, &["$schema", "type", "properties", "required", "title", "description", "additionalProperties"])?;
        ensure!(schema.get("type").and_then(Value::as_str) == Some("object"), "MCP form schema must describe an object");
        let draft = match annotation(schema, "$schema")? {
            None | Some("https://json-schema.org/draft/2020-12/schema" | "https://json-schema.org/draft/2020-12/schema#") => jsonschema::Draft::Draft202012,
            Some("http://json-schema.org/draft-07/schema#" | "http://json-schema.org/draft-07/schema") => jsonschema::Draft::Draft7,
            Some(_) => bail!("Unsupported MCP form schema draft"),
        };
        annotation(schema, "title")?;
        annotation(schema, "description")?;
        if let Some(extra) = schema.get("additionalProperties") {
            ensure!(extra.is_boolean(), "MCP form additionalProperties must be boolean");
        }
        let properties = object(schema.get("properties").ok_or_else(|| anyhow!("Missing MCP form properties"))?)?;
        ensure!(properties.len() <= 64, "Too many MCP form fields");
        let required = names(schema.get("required"), properties)?;
        let order = names((!property_order.is_null()).then_some(property_order), properties)?;
        let ordered: HashSet<_> = order.iter().copied().collect();
        let mut fields = Vec::with_capacity(properties.len());
        let mut checked = Map::new();
        for key in order.into_iter().chain(properties.keys().map(String::as_str).filter(|key| !ordered.contains(key))) {
            let (field, mut property) = Field::parse(key, &properties[key], required.contains(&key), openai)?;
            if let Some(default) = property.remove("default") {
                // Defaults are annotations, not answers. Reject the wrong wire type, but never populate the form from them.
                let valid = match property["type"].as_str() {
                    Some("string") => default.is_string(),
                    Some("number" | "integer") => default.is_number(),
                    Some("boolean") => default.is_boolean(),
                    Some("array") => default.as_array().is_some_and(|items| items.iter().all(Value::is_string)),
                    _ => false,
                };
                ensure!(valid, "Invalid MCP form field default");
            }
            fields.push(field);
            checked.insert(key.to_owned(), Value::Object(property));
        }
        let schema = json!({"type":"object","properties":checked,"required":required,"additionalProperties":false});
        let validator = jsonschema::options().with_draft(draft).should_validate_formats(true)
            .offline().build(&schema)
            .map_err(|_| anyhow!("Invalid MCP form schema constraints"))?;
        Ok(Self::Form(Form { fields, validator }))
    }

    pub(super) fn url(raw: &Value, id: &Value) -> Result<Self> {
        required_text(id, "elicitation ID")?;
        let value = required_text(raw, "URL")?;
        let url = paseo_protocol::helpers::mcp_elicitation_url(value)
            .ok_or_else(|| anyhow!("MCP elicitation needs a valid HTTP(S) URL without credentials"))?;
        Ok(Self::Url(url))
    }

    pub(super) fn request(&self, server: &str, message: &str) -> Value {
        let fields = match self {
            Self::Form(form) => form.fields.as_slice(),
            Self::Url(_) => &[],
        };
        let mut request = json!({
            "name":"McpElicitation",
            "kind":if fields.is_empty() { "tool" } else { "question" },
            "title":format!("MCP request from {server}"),
            "description":message,
            "input":{},
            "metadata":{"mcpElicitation":true},
            "actions":[
                {"id":"cancel","label":"Cancel","behavior":"deny","variant":"secondary","intent":"dismiss"},
                {"id":"decline","label":"Decline","behavior":"deny","variant":"danger"},
                {"id":"accept","label":"Accept","behavior":"allow","variant":"primary"}
            ]
        });
        if !fields.is_empty() {
            request["input"]["questions"] = Value::Array(fields.iter().map(Field::question).collect());
        } else if let Self::Url(url) = self {
            request["input"]["url"] = json!(url.as_str());
        }
        request
    }

    pub(super) fn answer(&self, response: &Value) -> Result<Answer> {
        if let Some(action) = response.get("selectedActionId") {
            ensure!(action.is_string(), "MCP elicitation action must be a string");
        }
        if let Some(interrupt) = response.get("interrupt") {
            ensure!(interrupt.is_boolean(), "MCP elicitation interrupt must be boolean");
        }
        let action = response["selectedActionId"].as_str();
        let interrupted = response["interrupt"] == true;
        let native_action = match (response["behavior"].as_str(), action, interrupted) {
            (Some("allow"), Some("accept"), false) => "accept",
            (Some("deny"), Some("decline"), false) => "decline",
            // Existing interrupt, steer, and shutdown callers can cancel without selecting a card action.
            (Some("deny"), Some("cancel") | None, _) => "cancel",
            _ => bail!("Select Accept, Decline, or Cancel on the MCP request"),
        };
        let content = if native_action == "accept" {
            match self {
                Self::Form(form) => Some(form.content(response)?),
                Self::Url(_) => {
                    ensure!(response.get("updatedInput").is_none(), "MCP URL acceptance does not take form data");
                    None
                }
            }
        } else {
            None
        };
        Ok(Answer { action: native_action, content })
    }
}

impl Form {
    fn content(&self, response: &Value) -> Result<Value> {
        let mut content = Map::new();
        let input = &response["updatedInput"];
        ensure!(response.get("updatedInput").is_none() || input.is_object(), "MCP form input must be an object");
        if let Some(submitted) = input.get("content") {
            ensure!(input.get("selections").is_none() && submitted.is_object(), "MCP form content must be one object");
            self.validate(submitted)?;
            return Ok(submitted.clone());
        }
        if !self.fields.is_empty() || input.get("selections").is_some() {
            let selections = input["selections"].as_object().ok_or_else(|| anyhow!("Submit explicit answers to the MCP form"))?;
            ensure!(selections.keys().all(|key| self.fields.iter().any(|f| &f.key == key)), "Unknown MCP form field");
            for field in &self.fields {
                if let Some(entry) = selections.get(&field.key)
                    && let Some(value) = field.answer(entry)?
                {
                    content.insert(field.key.clone(), value);
                } else {
                    ensure!(!field.required, "Answer the required MCP field ‘{}’", field.title);
                }
            }
        }
        let content = Value::Object(content);
        self.validate(&content)?;
        Ok(content)
    }

    fn validate(&self, content: &Value) -> Result<()> {
        if let Some(error) = self.validator.iter_errors(content).next() {
            // Keep the value itself out of diagnostics and activity notifications.
            let path = error.instance_path().to_string();
            if path.is_empty() {
                bail!("Answers do not satisfy the MCP form schema");
            }
            bail!("Answer for MCP field {path} does not satisfy its schema");
        }
        Ok(())
    }
}

impl Field {
    fn parse(key: &str, raw: &Value, required: bool, openai: bool) -> Result<(Self, Map<String, Value>)> {
        let raw = object(raw)?;
        let title = annotation(raw, "title")?.filter(|s| !s.trim().is_empty()).unwrap_or(key).to_owned();
        let description = annotation(raw, "description")?.map(str::to_owned);
        let ty = raw.get("type").and_then(Value::as_str).ok_or_else(|| anyhow!("Missing MCP field type"))?;
        let mut schema;
        let (input, mut options) = match ty {
            "openai/imagePicker" if openai => {
                keys(raw, &["type", "title", "description", "items", "default"])?;
                let values = choices(raw.get("items").unwrap_or(&Value::Null), |item| {
                    let item = object(item)?;
                    keys(item, &["id", "title", "description", "image"])?;
                    let id = required_text(item.get("id").unwrap_or(&Value::Null), "image option ID")?;
                    annotation(item, "image")?;
                    annotation(item, "description")?;
                    Ok((annotation(item, "title")?.unwrap_or(id), id))
                })?;
                // The vendor wire asks for an item ID. Never fetch or open its preview source.
                schema = raw.iter().filter(|(key, _)| *key != "items").map(|(key, value)| (key.clone(), value.clone())).collect::<Map<_, _>>();
                schema.insert("type".into(), json!("string"));
                schema.insert("enum".into(), Value::Array(values.iter().map(|c| c.value.clone()).collect()));
                (Input::Single, values)
            }
            "string" if raw.contains_key("enum") || raw.contains_key("oneOf") => {
                keys(raw, &["type", "title", "description", "default", "enum", "enumNames", "oneOf"])?;
                let options = enum_choices(raw, "oneOf", openai)?;
                // With unique const strings, oneOf has exactly the same accepted
                // values as enum. Keep presentation payloads out of the validator.
                schema = raw.iter().filter(|(key, _)| !matches!(key.as_str(), "enumNames" | "oneOf" | "enum"))
                    .map(|(key, value)| (key.clone(), value.clone())).collect();
                schema.insert("enum".into(), Value::Array(options.iter().map(|c| c.value.clone()).collect()));
                (Input::Single, options)
            }
            "string" => {
                keys(raw, &["type", "title", "description", "default", "minLength", "maxLength", "format"])?;
                check_range(raw, "minLength", "maxLength", Value::as_u64)?;
                let format = annotation(raw, "format")?;
                ensure!(matches!(format, None | Some("email" | "uri" | "date" | "date-time")), "Unsupported MCP string format");
                let empty = raw.get("minLength").and_then(Value::as_u64).unwrap_or(0) == 0 && format.is_none();
                schema = raw.clone();
                (Input::Text, if empty { vec![Choice { label: "Empty string".into(), value: json!("") }] } else { Vec::new() })
            }
            "integer" | "number" => {
                keys(raw, &["type", "title", "description", "default", "minimum", "maximum"])?;
                check_range(raw, "minimum", "maximum", Value::as_f64)?;
                schema = raw.clone();
                (Input::Number, Vec::new())
            }
            "boolean" => {
                keys(raw, &["type", "title", "description", "default"])?;
                schema = raw.clone();
                (Input::Single, vec![Choice { label: "True".into(), value: json!(true) }, Choice { label: "False".into(), value: json!(false) }])
            }
            "array" => {
                keys(raw, &["type", "title", "description", "default", "items", "minItems", "maxItems", "uniqueItems"])?;
                check_range(raw, "minItems", "maxItems", Value::as_u64)?;
                let items = object(raw.get("items").unwrap_or(&Value::Null))?;
                keys(items, &["type", "enum", "anyOf"])?;
                ensure!(items.get("type").is_none_or(|t| t == "string"), "MCP array items must be strings");
                let mut values = enum_choices(items, "anyOf", false)?;
                let min = raw.get("minItems").and_then(Value::as_u64).unwrap_or(0);
                ensure!(min <= values.len() as u64, "MCP form requires more choices than it offers");
                schema = raw.clone();
                if let Some(unique) = schema.get("uniqueItems") {
                    ensure!(unique.is_boolean(), "MCP form uniqueItems must be boolean");
                }
                if raw.get("maxItems").and_then(Value::as_u64) == Some(0) {
                    values.clear();
                }
                if min == 0 {
                    values.push(Choice { label: "Empty list".into(), value: json!([]) });
                }
                (Input::Multi, values)
            }
            _ => bail!("Unsupported MCP form field type"),
        };
        schema.retain(|_, value| !value.is_null());
        let mut labels = HashSet::new();
        for option in &mut options {
            let mut next = option.label.clone();
            let mut suffix = 2;
            while !labels.insert(next.clone()) {
                next = format!("{} ({suffix})", option.label);
                suffix += 1;
            }
            option.label = next;
        }
        Ok((Self { key: key.to_owned(), title, description, required, input, options }, schema))
    }

    fn question(&self) -> Value {
        let options = self.options.iter().map(|choice| {
            let mut option = json!({"label":choice.label});
            if choice.value == json!([]) {
                option["exclusive"] = Value::Bool(true);
            }
            option
        }).collect::<Vec<_>>();
        let requirement = if self.required { "required" } else { "optional" };
        json!({
            "answerKey":self.key,
            "header":self.title,
            "question":match &self.description {
                Some(description) => format!("{} ({requirement})\n{description}", self.title),
                None => format!("{} ({requirement})", self.title),
            },
            "options":options,
            "multiSelect":self.input == Input::Multi,
            "allowOther":self.input == Input::Text && !self.options.is_empty(),
            "allowEmpty":!self.required
        })
    }

    fn answer(&self, entry: &Value) -> Result<Option<Value>> {
        let entry = object(entry)?;
        keys(entry, &["options", "other"])?;
        let options = entry.get("options").and_then(Value::as_array).ok_or_else(|| anyhow!("MCP form options must be an array"))?;
        let mut seen = HashSet::new();
        let selected: Vec<_> = options.iter().map(|value| {
            let label = value.as_str().ok_or_else(|| anyhow!("MCP form choices must be strings"))?;
            ensure!(seen.insert(label), "Duplicate MCP form choice");
            self.options.iter().find(|choice| choice.label == label).map(|choice| &choice.value)
                .ok_or_else(|| anyhow!("Unknown MCP form choice"))
        }).collect::<Result<_>>()?;
        let text = entry.get("other").map(|text| text.as_str().ok_or_else(|| anyhow!("MCP form text must be a string"))).transpose()?;
        if let Some(text) = text {
            ensure!(selected.is_empty(), "MCP form cannot submit text and choices for the same field");
            return Ok(Some(match self.input {
                Input::Text => json!(text),
                Input::Number => serde_json::from_str::<Value>(text).ok().filter(Value::is_number)
                    .ok_or_else(|| anyhow!("Enter a number for ‘{}’", self.title))?,
                _ => bail!("MCP form choice field does not accept text"),
            }));
        }
        if selected.is_empty() {
            return Ok(None);
        }
        if self.input == Input::Multi {
            if selected.iter().any(|value| value.is_array()) {
                ensure!(selected.len() == 1, "Choose an empty list or values, not both");
                return Ok(Some(Value::Array(Vec::new())));
            }
            return Ok(Some(Value::Array(selected.into_iter().cloned().collect())));
        }
        ensure!(selected.len() == 1, "Choose one option for ‘{}’", self.title);
        Ok(Some(selected[0].clone()))
    }
}

pub(super) fn required_text<'a>(raw: &'a Value, field: &str) -> Result<&'a str> {
    raw.as_str().filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow!("Invalid MCP {field}"))
}

fn object(value: &Value) -> Result<&Map<String, Value>> {
    value.as_object().ok_or_else(|| anyhow!("Expected an MCP object"))
}

fn keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    ensure!(map.keys().all(|k| allowed.contains(&k.as_str())), "Unsupported MCP form or response field");
    Ok(())
}

fn annotation<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => bail!("MCP form {key} must be a string"),
    }
}

fn names<'a>(value: Option<&'a Value>, properties: &Map<String, Value>) -> Result<Vec<&'a str>> {
    let mut seen = HashSet::new();
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| anyhow!("MCP form field names must be an array"))?.iter().map(|value| {
            let name = value.as_str().ok_or_else(|| anyhow!("MCP form field name must be a string"))?;
            ensure!(properties.contains_key(name) && seen.insert(name), "Unknown or duplicate MCP form field name");
            Ok(name)
        }).collect(),
    }
}

fn check_range<T: PartialOrd>(schema: &Map<String, Value>, min: &str, max: &str, parse: impl Fn(&Value) -> Option<T>) -> Result<()> {
    let bound = |key| schema.get(key).filter(|value| !value.is_null()).map(|value| {
        parse(value).ok_or_else(|| anyhow!("Invalid MCP form {key} bound"))
    }).transpose();
    if let (Some(min), Some(max)) = (bound(min)?, bound(max)?) {
        ensure!(min <= max, "Invalid MCP form range");
    }
    Ok(())
}

fn choices<'a>(raw: &'a Value, mut read: impl FnMut(&'a Value) -> Result<(&'a str, &'a str)>) -> Result<Vec<Choice>> {
    let list = raw.as_array().filter(|a| !a.is_empty() && a.len() <= 128).ok_or_else(|| anyhow!("MCP form requires between 1 and 128 choices"))?;
    let mut seen = HashSet::new();
    list.iter().map(|entry| {
        let (label, value) = read(entry)?;
        ensure!(seen.insert(value), "Duplicate MCP form choice value");
        let label = if !label.trim().is_empty() { label } else if !value.is_empty() { value } else { "Empty string" };
        Ok(Choice { label: label.to_owned(), value: json!(value) })
    }).collect()
}

fn enum_choices(schema: &Map<String, Value>, alternatives: &str, openai: bool) -> Result<Vec<Choice>> {
    if let Some(values) = schema.get("enum") {
        ensure!(!schema.contains_key(alternatives), "Multiple MCP form choice declarations");
        let labels = schema.get("enumNames").filter(|labels| !labels.is_null()).map(|labels| {
            labels.as_array().filter(|labels| Some(labels.len()) == values.as_array().map(Vec::len) && labels.iter().all(Value::is_string))
                .ok_or_else(|| anyhow!("MCP form enumNames must label each choice"))
        }).transpose()?;
        let mut index = 0;
        choices(values, |entry| {
            let value = entry.as_str().ok_or_else(|| anyhow!("MCP form enum values must be strings"))?;
            let label = labels.and_then(|labels| labels[index].as_str()).unwrap_or(value);
            index += 1;
            Ok((label, value))
        })
    } else {
        ensure!(!schema.contains_key("enumNames"), "MCP form enumNames require enum");
        choices(schema.get(alternatives).unwrap_or(&Value::Null), |entry| {
            let item = object(entry)?;
            if openai {
                keys(item, &["title", "const", "x-openai-preview"])?;
                if let Some(preview) = item.get("x-openai-preview") {
                    let preview = object(preview)?;
                    ensure!(preview.get("src").is_some_and(Value::is_string), "Invalid MCP choice preview source");
                }
            } else {
                keys(item, &["title", "const"])?;
            }
            let value = item.get("const").and_then(Value::as_str).ok_or_else(|| anyhow!("MCP form const values must be strings"))?;
            let title = annotation(item, "title")?.ok_or_else(|| anyhow!("Missing MCP form choice title"))?;
            Ok((title, value))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_form_validation_preserves_typed_values_and_explicit_empties() {
        let schema = json!({"type":"object","required":["name","emails","features"],"properties":{
            "name":{"type":"string","maxLength":6,"default":"Jane"},
            "emails":{"type":"array","items":{"enum":["a","b"]},"minItems":0,"maxItems":2},
            "features":{"type":"array","items":{"anyOf":[{"const":"option-a","title":"A, B"},{"const":"option-b","title":"A, B"}]},"minItems":1},
            "ratio":{"type":"number","minimum":0,"maximum":1},
            "defaulted":{"type":"boolean","default":true}
        }});
        let form = Elicitation::form(&schema, &Value::Null, false).unwrap();
        let select = |values| json!({"behavior":"allow","selectedActionId":"accept","updatedInput":{"selections":values}});
        let selections = json!({
            "name":{"options":["Empty string"]},
            "emails":{"options":["Empty list"]},
            "features":{"options":["A, B","A, B (2)"]},
            "ratio":{"options":[],"other":"0.5"}
        });
        assert_eq!(form.answer(&select(selections.clone())).unwrap().native(false),
            json!({"action":"accept","content":{"name":"","emails":[],"features":["option-a","option-b"],"ratio":0.5}}));
        for (field, invalid) in [
            ("name", json!({"options":[],"other":"too long"})),
            ("name", json!({"options":["Empty string"],"other":"conflict"})),
            ("emails", json!({"options":["Empty list","a"]})),
            ("features", json!({"options":["A, B","A, B"]})),
            ("features", json!({"options":[],"other":"A, B"})),
            ("features", json!({"options":[42]})),
            ("ratio", json!({"options":[],"other":"-2"})),
            ("ratio", json!({"options":[],"other":"true"})),
            ("extra", json!({"options":["Anything"]})),
        ] {
            let mut candidate = selections.clone();
            candidate[field] = invalid;
            assert!(form.answer(&select(candidate)).is_err(), "accepted invalid {field}");
        }
        assert!(form.answer(&select(json!({}))).is_err());
        assert!(form.answer(&json!({"behavior":"allow","selectedActionId":"accept"})).is_err());
    }

    #[test]
    fn mcp_malformed_or_unrenderable_schemas_fail_closed() {
        for (field, openai) in [
            (json!({"type":"string","minLength":9,"maxLength":4}), false),
            (json!({"type":"string","maxLength":true}), false),
            (json!({"type":"string","default":true}), false),
            (json!({"type":"number","minimum":"0"}), false),
            (json!({"type":"string","enum":["same","same"]}), false),
            (json!({"type":"string","oneOf":[{"title":"missing const"}]}), false),
            (json!({"type":"array","items":{"enum":[1]}}), false),
            (json!({"type":"object","properties":{}}), false),
            (json!({"type":"string","$ref":"https://example.test/schema"}), false),
            (json!({"type":"openai/imagePicker","items":[{"title":"missing ID"}]}), true),
            (json!({"type":"openai/imagePicker","items":[{"id":"x","image":false}]}), true),
            (json!({"type":"openai/imagePicker"}), true),
        ] {
            let schema = json!({"type":"object","properties":{"field":field}});
            assert!(Elicitation::form(&schema, &Value::Null, openai).is_err(), "{schema}");
        }
        assert!(Elicitation::form(&json!({"type":"object","properties":{},"required":["unknown"]}), &Value::Null, false).is_err());
    }

    #[test]
    fn mcp_url_never_allows_non_web_or_confused_authorities() {
        for raw in [
            "javascript:alert(1)", "data:text/html,hello", "file:///tmp/secret", "//example.test",
            "https:///example.test", "https://name:pass@example.test/", "https://@example.test/",
            "https://example.test\\@wrong.test", "https://example.test/a\nb",
        ] {
            assert!(Elicitation::url(&json!(raw), &json!("id")).is_err(), "{raw}");
        }
        let url = Elicitation::url(&json!("https://example.test/authorize?a=1#step"), &json!("id")).unwrap();
        assert_eq!(url.request("demo", "Authenticate")["input"]["url"], "https://example.test/authorize?a=1#step");
        assert!(url.answer(&json!({"behavior":"allow"})).is_err());
        assert_eq!(url.answer(&json!({"behavior":"allow","selectedActionId":"accept"})).unwrap().native(false), json!({"action":"accept"}));
    }
}
