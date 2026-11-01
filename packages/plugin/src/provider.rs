//! Provider wire payloads retain the host's JSON shape, including session persistence,
//! timeline items, capability extensions, permissions and child sessions. Providers
//! reject unsupported inputs rather than acknowledging operations they did not perform.
use crate::{Context, Result, bail, ensure, runtime::Output, valid_id};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
    pub requires_configuration: bool,
    pub has_catalog_cache_key: bool,
    pub has_status: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}
impl Metadata {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: None,
            icon_path: None,
            requires_configuration: false,
            has_catalog_cache_key: false,
            has_status: false,
            command: None,
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        valid_id(&self.id)?;
        ensure!(!self.label.trim().is_empty(), "Provider requires a label");
        if let Some(command) = &self.command {
            ensure!(
                !command.is_empty()
                    && !command[0].trim().is_empty()
                    && command.iter().all(|v| !v.contains('\0')),
                "Invalid provider command"
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Events {
    pub(crate) connection_id: Arc<str>,
    pub(crate) output: Output,
    pub(crate) cancellation: CancellationToken,
}
impl Events {
    pub fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }
    /// Delivery is backpressured and never silently drops events. A connection's
    /// cancellation interrupts a blocked publisher so shutdown cannot deadlock.
    pub async fn emit(&self, event: Value) -> Result<()> {
        ensure!(
            event.is_object() && event["type"].as_str().is_some_and(|s| !s.is_empty()),
            "Provider event requires a type"
        );
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => bail!("Provider connection closed"),
            result = self.output.send(json!({"type":"provider.event","connectionId":self.connection_id.as_ref(),"event":event})) => result,
        }
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn metadata(&self) -> Metadata;
    /// Versions and capabilities are the host's supported set. Return only a
    /// negotiated subset; the runtime rejects versions/capabilities not offered.
    async fn connect(
        &self,
        request: Value,
        events: Events,
        context: Context,
    ) -> Result<Arc<dyn Connection>>;
    async fn status(&self, _request: Value, _context: Context) -> Result<Value> {
        bail!("Provider has no status capability")
    }
    async fn catalog_cache_key(
        &self,
        _options: Value,
        _context: Context,
    ) -> Result<Option<String>> {
        Ok(None)
    }
}

#[async_trait]
pub trait Connection: Send + Sync {
    fn version(&self) -> u64;
    fn capabilities(&self) -> Vec<String>;
    /// Acceptance is emitted only after this resolves successfully. Session
    /// results and permission requests are emitted separately using Events.
    async fn send(&self, input: Value) -> Result<()>;
    /// Must stop producer tasks, close child processes and resolve pending permission requests.
    async fn close(&self) -> Result<()>;
}
