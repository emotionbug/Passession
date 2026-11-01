//! Lossless message objects validated against the complete native JSON Schemas.
//! Validation is separate from normalization: unknown extension fields survive.
use crate::{MAX_WEBSOCKET_MESSAGE_BYTES, ProtocolError};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::{collections::HashMap, sync::LazyLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
}

pub const SESSION_INBOUND_SCHEMA: &str = include_str!("../schema/session-inbound.json");
pub const SESSION_OUTBOUND_SCHEMA: &str = include_str!("../schema/session-outbound.json");
pub const WS_INBOUND_SCHEMA: &str = include_str!("../schema/ws-inbound.json");
pub const WS_OUTBOUND_SCHEMA: &str = include_str!("../schema/ws-outbound.json");
pub const TERMINAL_STATE_SCHEMA: &str = include_str!("../schema/terminal-state.json");
pub const PASEO_CONFIG_SCHEMA: &str = include_str!("paseo.config.v1.json");

fn compile(schema: &Value) -> jsonschema::Validator {
    jsonschema::options()
        .should_validate_formats(true)
        .should_ignore_unknown_formats(true)
        .offline()
        .build(schema)
        .expect("embedded protocol schema must be valid")
}
fn catalog(text: &str) -> HashMap<String, jsonschema::Validator> {
    let root: Value = serde_json::from_str(text).expect("embedded protocol JSON");
    root["oneOf"]
        .as_array()
        .or_else(|| root["anyOf"].as_array())
        .expect("message union")
        .iter()
        .map(|branch| {
            let mut branch = branch.clone();
            if let Some(defs) = root.get("$defs") {
                branch["$defs"] = defs.clone();
            }
            (
                branch["properties"]["type"]["const"]
                    .as_str()
                    .expect("message discriminator")
                    .to_owned(),
                compile(&branch),
            )
        })
        .collect()
}
static INBOUND: LazyLock<HashMap<String, jsonschema::Validator>> =
    LazyLock::new(|| catalog(SESSION_INBOUND_SCHEMA));
static OUTBOUND: LazyLock<HashMap<String, jsonschema::Validator>> =
    LazyLock::new(|| catalog(SESSION_OUTBOUND_SCHEMA));
static WS_IN: LazyLock<HashMap<String, jsonschema::Validator>> =
    LazyLock::new(|| catalog(WS_INBOUND_SCHEMA));
static WS_OUT: LazyLock<HashMap<String, jsonschema::Validator>> =
    LazyLock::new(|| catalog(WS_OUTBOUND_SCHEMA));
static TERMINAL: LazyLock<jsonschema::Validator> =
    LazyLock::new(|| compile(&serde_json::from_str(TERMINAL_STATE_SCHEMA).unwrap()));
static CONFIG: LazyLock<jsonschema::Validator> =
    LazyLock::new(|| compile(&serde_json::from_str(PASEO_CONFIG_SCHEMA).unwrap()));

fn validate(
    catalog: &HashMap<String, jsonschema::Validator>,
    value: &Value,
) -> Result<(), ProtocolError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::Invalid("type"))?;
    let validator = catalog.get(kind).ok_or(ProtocolError::UnknownMessage)?;
    validator
        .validate(value)
        .map_err(|e| ProtocolError::Schema(e.instance_path().to_string()))?;
    validate_semantics(value)
}
pub fn validate_session(value: &Value, direction: Direction) -> Result<(), ProtocolError> {
    validate(
        match direction {
            Direction::Inbound => &INBOUND,
            Direction::Outbound => &OUTBOUND,
        },
        value,
    )
}
pub fn validate_terminal_snapshot(value: &Value) -> Result<(), ProtocolError> {
    TERMINAL
        .validate(value)
        .map_err(|e| ProtocolError::Schema(e.instance_path().to_string()))
}
/// Validates the public configuration document without loading filesystem state.
pub fn validate_config(value: &Value) -> Result<(), ProtocolError> {
    CONFIG
        .validate(value)
        .map_err(|e| ProtocolError::Schema(e.instance_path().to_string()))
}
pub fn message_types(direction: Direction) -> impl Iterator<Item = &'static str> {
    let catalog = match direction {
        Direction::Inbound => &*INBOUND,
        Direction::Outbound => &*OUTBOUND,
    };
    catalog.keys().map(String::as_str)
}

/// Every session RPC/event is representable without guessing an incomplete enum.
/// Construct with `parse` for schema validation, or `new` for a future extension.
/// Serde deserialization checks only the structural discriminator because the
/// transport direction is not available to a Deserializer. At network boundaries
/// use `parse_envelope` or `SessionMessage::parse`, not `serde_json::from_slice`.
/// Parsing is lossless: schema defaults and UI-only normalization are not applied.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionMessage(Map<String, Value>);
impl SessionMessage {
    pub fn new(
        kind: impl Into<String>,
        mut fields: Map<String, Value>,
    ) -> Result<Self, ProtocolError> {
        let kind = kind.into();
        if kind.is_empty() {
            return Err(ProtocolError::Invalid("type"));
        }
        fields.insert("type".into(), Value::String(kind));
        Ok(Self(fields))
    }
    pub fn parse(value: Value, direction: Direction) -> Result<Self, ProtocolError> {
        validate_session(&value, direction)?;
        match value {
            Value::Object(fields) => Ok(Self(fields)),
            _ => Err(ProtocolError::Invalid("message")),
        }
    }
    pub fn kind(&self) -> &str {
        self.0["type"].as_str().expect("validated type")
    }
    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }
    pub fn payload(&self) -> Option<&Value> {
        self.0.get("payload")
    }
    pub fn request_id(&self) -> Option<&str> {
        self.0
            .get("requestId")
            .and_then(Value::as_str)
            .or_else(|| self.payload()?.get("requestId")?.as_str())
    }
    pub fn into_value(self) -> Value {
        Value::Object(self.0)
    }
    pub fn into_envelope(self) -> Envelope {
        Envelope::Session { message: self }
    }
}
impl<'de> Deserialize<'de> for SessionMessage {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let fields = Map::<String, Value>::deserialize(d)?;
        if !fields
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            return Err(serde::de::Error::custom("message type is required"));
        }
        Ok(Self(fields))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Envelope {
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "pong")]
    Pong,
    #[serde(rename = "session")]
    Session { message: SessionMessage },
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "recording_state")]
    RecordingState {
        #[serde(rename = "isRecording")]
        is_recording: bool,
    },
    #[serde(rename = "hello.rejected")]
    HelloRejected {
        reason: String,
        accepts: Vec<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub client_id: String,
    pub client_type: ClientType,
    pub protocol_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Authentication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_permissions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<ProducerIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Map<String, Value>>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProducerIdentity {
    pub server_id: String,
    pub source_id: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientType {
    Mobile,
    Browser,
    Cli,
    Mcp,
    Hub,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Authentication {
    Password { password: String },
    LocalCredential { token: String },
}
impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authentication([redacted])")
    }
}
pub fn parse_envelope(bytes: &[u8], direction: Direction) -> Result<Envelope, ProtocolError> {
    if bytes.len() > MAX_WEBSOCKET_MESSAGE_BYTES {
        return Err(ProtocolError::WireLimit);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| ProtocolError::Json)?;
    validate(
        match direction {
            Direction::Inbound => &WS_IN,
            Direction::Outbound => &WS_OUT,
        },
        &value,
    )?;
    if let Some(message) = value.get("message") {
        validate_semantics(message)?;
    }
    serde_json::from_value(value).map_err(|_| ProtocolError::Json)
}
pub fn encode_envelope(
    envelope: &Envelope,
    direction: Direction,
) -> Result<Vec<u8>, ProtocolError> {
    let value = serde_json::to_value(envelope).map_err(|_| ProtocolError::Json)?;
    validate(
        match direction {
            Direction::Inbound => &WS_IN,
            Direction::Outbound => &WS_OUT,
        },
        &value,
    )?;
    if let Some(message) = value.get("message") {
        validate_semantics(message)?;
    }
    let bytes = serde_json::to_vec(&value).map_err(|_| ProtocolError::Json)?;
    if bytes.len() > MAX_WEBSOCKET_MESSAGE_BYTES {
        return Err(ProtocolError::WireLimit);
    }
    Ok(bytes)
}

// Cross-field semantics not expressible by the original structural schema.
fn validate_semantics(value: &Value) -> Result<(), ProtocolError> {
    if value["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("hub."))
    {
        let payload = &value["payload"];
        if let (Some(agent_id), Some(nested_id)) =
            (payload["agentId"].as_str(), payload["agent"]["id"].as_str())
        {
            if agent_id != nested_id {
                return Err(ProtocolError::Invalid("hub agent correlation"));
            }
        }
    }
    if value["type"] == "browser.automation.execute.request" {
        let command = value.get("command").unwrap_or(value);
        let args = command.get("args").or_else(|| value.get("args"));
        if let Some(args) = args {
            if command["command"] == "wait" {
                let count = ["text", "url"]
                    .iter()
                    .filter(|key| args[**key].as_str().is_some_and(|s| !s.is_empty()))
                    .count();
                if count != 1 {
                    return Err(ProtocolError::Invalid("wait condition"));
                }
            }
            if matches!(command["command"].as_str(), Some("new_tab" | "navigate")) {
                if let Some(raw) = args["url"].as_str() {
                    let url =
                        url::Url::parse(raw).map_err(|_| ProtocolError::Invalid("browser URL"))?;
                    if !matches!(url.scheme(), "http" | "https") {
                        return Err(ProtocolError::Invalid("browser URL"));
                    }
                }
            }
        }
    }
    Ok(())
}
