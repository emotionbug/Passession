//! Epoch-scoped sequence identities; a cursor from another epoch is never ordered.
use crate::{MAX_SAFE_INTEGER, ProtocolError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineCursor {
    pub epoch: String,
    #[serde(deserialize_with = "deserialize_sequence")]
    pub seq: u64,
}
impl TimelineCursor {
    pub fn new(epoch: impl Into<String>, seq: u64) -> Result<Self, ProtocolError> {
        if seq > MAX_SAFE_INTEGER {
            return Err(ProtocolError::Invalid("cursor.seq"));
        }
        Ok(Self {
            epoch: epoch.into(),
            seq,
        })
    }
    pub fn parse(value: Value) -> Result<Self, ProtocolError> {
        let cursor: Self =
            serde_json::from_value(value).map_err(|_| ProtocolError::Invalid("cursor"))?;
        Self::new(cursor.epoch, cursor.seq)
    }
    pub fn compare(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (self.epoch == other.epoch).then(|| self.seq.cmp(&other.seq))
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimelineDirection {
    Tail,
    Before,
    After,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimelineProjection {
    #[default]
    Projected,
    Canonical,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineWindow {
    #[serde(deserialize_with = "deserialize_sequence")]
    pub min_seq: u64,
    #[serde(deserialize_with = "deserialize_sequence")]
    pub max_seq: u64,
    #[serde(deserialize_with = "deserialize_sequence")]
    pub next_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCursor {
    pub generation: String,
    #[serde(deserialize_with = "deserialize_sequence")]
    pub after_seq: u64,
}
fn deserialize_sequence<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value > MAX_SAFE_INTEGER {
        return Err(serde::de::Error::custom(
            "sequence exceeds the wire safe-integer limit",
        ));
    }
    Ok(value)
}
pub fn timeline_item_identity(item: &Value) -> Option<String> {
    match item.get("type")?.as_str()? {
        "tool_call" => Some(item.get("callId")?.as_str()?.to_owned()),
        "plugin" => Some(format!(
            "{}/{}",
            item.get("pluginId")?.as_str()?,
            item.get("id")?.as_str()?
        )),
        _ => None,
    }
}
pub fn terminal_subscription_key(cwd: &str, workspace_id: Option<&str>) -> String {
    workspace_id.map_or_else(|| cwd.to_owned(), |id| format!("{id}::{cwd}"))
}
