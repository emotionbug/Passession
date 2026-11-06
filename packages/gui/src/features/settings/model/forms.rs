//! Flutter `SettingField` / `editSettingsForm`: a sheet of labelled fields,
//! validated and parsed on Save, whose save callback finishes the sheet.

use std::collections::HashMap;
use std::rc::Rc;

use serde_json::{Map, Value};

use crate::features::settings::i18n::t;
use crate::features::settings::json::{parse_pairs, text_lines};
use crate::features::settings::rpc::Env;
use crate::features::settings::state::Settings;

#[derive(Clone, Debug)]
pub enum Kind {
    Text { multiline: bool, secret: bool },
    Bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Parse {
    Raw,
    /// Trimmed non-empty lines.
    Lines,
    /// `*` means `true`, otherwise lines (hostnames / trusted proxies).
    LinesOrWildcard,
    /// `NAME=value` lines.
    EnvPairs,
    /// `key=value` lines.
    KeyPairs,
    PositiveInt,
}

#[derive(Clone, Debug)]
pub struct SettingField {
    pub key: String,
    pub label: String,
    pub kind: Kind,
    /// Initial text for text fields, bool for switches.
    pub value: Value,
    pub required: bool,
    pub parse: Parse,
}

impl SettingField {
    pub fn text(key: &str, label: String, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            label,
            kind: Kind::Text { multiline: false, secret: false },
            value: Value::String(value.into()),
            required: false,
            parse: Parse::Raw,
        }
    }
    pub fn multiline(mut self) -> Self {
        if let Kind::Text { secret, .. } = self.kind {
            self.kind = Kind::Text { multiline: true, secret };
        }
        self
    }
    pub fn secret(mut self) -> Self {
        if let Kind::Text { multiline, .. } = self.kind {
            self.kind = Kind::Text { multiline, secret: true };
        }
        self
    }
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }
    pub fn parse(mut self, parse: Parse) -> Self {
        self.parse = parse;
        self
    }
    pub fn toggle(key: &str, label: String, value: bool) -> Self {
        Self { key: key.into(), label, kind: Kind::Bool, value: Value::Bool(value), required: false, parse: Parse::Raw }
    }
}

pub fn positive_integer(text: &str) -> Result<i64, String> {
    match text.trim().parse::<i64>() {
        Ok(number) if number > 0 => Ok(number),
        _ => Err(t("settingsNative.positiveInteger")),
    }
}

pub fn parse_value(parse: Parse, text: &str) -> Result<Value, String> {
    Ok(match parse {
        Parse::Raw => Value::String(text.to_string()),
        Parse::Lines => Value::Array(text_lines(text).into_iter().map(Value::String).collect()),
        Parse::LinesOrWildcard => {
            if text.trim() == "*" {
                Value::Bool(true)
            } else {
                Value::Array(text_lines(text).into_iter().map(Value::String).collect())
            }
        }
        Parse::EnvPairs => Value::Object(parse_pairs(text, &t("settingsNative.useNameValue"))?),
        Parse::KeyPairs => Value::Object(parse_pairs(text, &t("settingsNative.useKeyValue"))?),
        Parse::PositiveInt => Value::from(positive_integer(text)?),
    })
}

/// Save callback: receives the sheet id and the parsed values; it must end
/// with [`Settings::form_done`].
pub type FormSave = Rc<dyn Fn(&mut Settings, &mut Env, u64, Map<String, Value>)>;

pub struct FormSheet {
    pub id: u64,
    pub title: String,
    pub description: Option<String>,
    pub fields: Vec<SettingField>,
    pub bools: HashMap<String, bool>,
    pub errors: HashMap<String, String>,
    pub busy: bool,
    pub error: Option<String>,
    pub on_save: FormSave,
}

impl FormSheet {
    pub fn new(id: u64, title: String, fields: Vec<SettingField>, on_save: FormSave) -> Self {
        let mut bools = HashMap::new();
        for field in &fields {
            if let Kind::Bool = field.kind {
                bools.insert(field.key.clone(), field.value.as_bool() == Some(true));
            }
        }
        Self { id, title, description: None, fields, bools, errors: HashMap::new(), busy: false, error: None, on_save }
    }

    pub fn describe(mut self, description: String) -> Self {
        self.description = Some(description);
        self
    }

    pub fn field_id(&self, key: &str) -> String {
        format!("sheet.{}.{key}", self.id)
    }

    /// Flutter `save()` validation: required text and parsers.
    pub fn collect(&mut self, text_of: impl Fn(&str) -> String) -> Option<Map<String, Value>> {
        self.errors.clear();
        let mut values = Map::new();
        for field in &self.fields {
            match &field.kind {
                Kind::Text { .. } => {
                    let text = text_of(&self.field_id(&field.key));
                    if field.required && text.trim().is_empty() {
                        self.errors.insert(field.key.clone(), t("settingsNative.required"));
                        continue;
                    }
                    match parse_value(field.parse, &text) {
                        Ok(value) => {
                            values.insert(field.key.clone(), value);
                        }
                        Err(message) => {
                            self.errors.insert(field.key.clone(), message);
                        }
                    }
                }
                Kind::Bool => {
                    values.insert(field.key.clone(), Value::Bool(self.bools.get(&field.key).copied().unwrap_or(false)));
                }
            }
        }
        if self.errors.is_empty() { Some(values) } else { None }
    }
}
