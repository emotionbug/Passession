use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Resolved executable prefix. Adapters append only their native protocol flags.
/// Secrets intentionally do not implement Debug or Serialize.
#[derive(Clone)]
pub struct ProviderSpec {
    pub id: String,
    pub family: String,
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub options: Value,
    pub defaults: Value,
    pub runtime_settings: Value,
    pub configured_models: Vec<Value>,
}

/// Per-agent authority and configuration, never persisted as provider options.
#[derive(Clone)]
pub struct LaunchContext {
    pub agent_id: String,
    pub home: PathBuf,
    pub executable: PathBuf,
    pub env: BTreeMap<String, String>,
    pub mcp_servers: Value,
    pub tool_catalog: Value,
    pub tool_manifest: Value,
    pub tool_executor: Option<Arc<dyn NativeToolExecutor>>,
    pub cancel: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct NativeToolInputError {
    pub status: u16,
    pub issues: Option<Value>,
    pub message: String,
}

/// Bound to one agent's authority; callers cannot supply a replacement identity.
#[async_trait]
pub trait NativeToolExecutor: Send + Sync {
    async fn execute(&self, name: &str, input: Value, cancel: CancellationToken) -> Result<Value> {
        self.execute_streaming(name, input, cancel, None).await
    }

    async fn execute_streaming(
        &self,
        name: &str,
        input: Value,
        cancel: CancellationToken,
        updates: Option<mpsc::Sender<Value>>,
    ) -> Result<Value>;
}

/// One bounded consumer owns event persistence before outward publication.
/// The manager drains this receiver before creating or resuming a provider.
#[derive(Clone)]
pub struct EventSink {
    sender: mpsc::Sender<Value>,
    cancel: CancellationToken,
}

impl EventSink {
    pub fn channel(capacity: usize, cancel: CancellationToken) -> (Self, mpsc::Receiver<Value>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (Self { sender, cancel }, receiver)
    }

    pub async fn emit(&self, event: Value) -> Result<()> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(anyhow!("provider event owner closed")),
            result = self.sender.send(event) => result.map_err(|_| anyhow!("provider event consumer closed")),
        }
    }

    pub fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

/// Definite prompt delivery evidence raised synchronously by a provider or the
/// manager. `rejected` means the provider explicitly refused the prompt; false
/// means it was proven never to reach the provider. Ambiguous failures must not
/// use this type.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct PromptDeliveryError {
    pub rejected: bool,
    pub message: String,
}
impl PromptDeliveryError {
    pub fn not_delivered(message: impl Into<String>) -> anyhow::Error {
        Self { rejected: false, message: message.into() }.into()
    }
    pub fn rejected(message: impl Into<String>) -> anyhow::Error {
        Self { rejected: true, message: message.into() }.into()
    }
}

/// A native acknowledgement correlated to one provider prompt request.
#[derive(Clone, Debug)]
pub enum PromptAck {
    /// Native evidence, for example the provider request ID.
    Accepted(Value),
    Rejected(String),
    /// The request was proven never written to the provider.
    NotDelivered(String),
    /// The request may have reached the provider without an acknowledgement.
    Unknown(String),
}
pub type PromptAckFuture = futures_util::future::BoxFuture<'static, PromptAck>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RewindTarget {
    Conversation,
    Files,
    Both,
}

pub struct ImportedSession {
    pub session: Arc<dyn AgentSession>,
    pub config: Value,
    pub persistence: Value,
    pub timeline: Vec<Value>,
    pub provider_subagent_events: Vec<Value>,
}

/// A successful replay may still be a drained cache or bounded context window.
/// Only a complete source snapshot can authorize explicit history truncation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HistoryCoverage {
    Complete,
    Unverified,
}

/// Values retain the existing agent-sdk-types.ts shapes, including opaque native
/// persistence handles and provider-specific metadata. Unsupported operations
/// must agree with the adapter's actual advertised capabilities.
#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &str;
    fn native_id(&self) -> Option<String>;
    fn capabilities(&self) -> &Value;
    fn features(&self) -> Arc<[Value]>;
    fn initial_timeline(&self) -> &[Value];
    fn describe_persistence(&self) -> Option<Value>;
    fn pending_permissions(&self) -> Vec<Value>;
    fn usage_session(&self) -> Option<Value>;

    async fn start_turn(&self, prompt: Value, options: Value) -> Result<Value>;
    async fn steer_active_turn(&self, prompt: Value, options: Value) -> Result<Value>;
    async fn stream_history(&self, sink: EventSink) -> Result<HistoryCoverage>;
    async fn runtime_info(&self) -> Result<Value>;
    async fn available_modes(&self) -> Result<Vec<Value>>;
    async fn current_mode(&self) -> Result<Option<String>>;
    async fn set_mode(&self, mode: &str) -> Result<Option<Value>>;
    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<Option<Value>>;
    async fn interrupt(&self) -> Result<()>;
    async fn close(&self) -> Result<()>;
    async fn list_commands(&self) -> Result<Vec<Value>>;
    async fn set_model(&self, model: Option<&str>) -> Result<()>;
    async fn set_thinking_option(&self, option: Option<&str>) -> Result<Option<Value>>;
    async fn set_feature(&self, feature: &str, value: Value) -> Result<()>;
    async fn rewind(&self, target: RewindTarget, message_id: &str) -> Result<()>;
    /// Recognize without requests or events. The manager publishes the accepted
    /// prompt before polling this owned operation outside its admission barrier.
    /// Execution failures become command-error stream events, not rejected sends.
    fn prepare_out_of_band(
        self: Arc<Self>,
        _prompt: &Value,
    ) -> Option<futures_util::future::BoxFuture<'static, Result<()>>> {
        None
    }
    /// Some providers return a turn ID before their native prompt request is
    /// acknowledged. They return that turn's correlated acknowledgement here,
    /// once; None means no separate native acknowledgement is observable.
    fn native_acknowledgement(&self, _turn_id: &str) -> Option<PromptAckFuture> {
        None
    }
}

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> &Value;
    /// Whether the provider has paired native archive/restore operations.
    /// Paseo's own archive metadata remains supported for every provider.
    fn supports_native_archive(&self) -> bool { false }
    /// None delegates to the registry's generic creation policy, matching the
    /// original optional provider hook rather than overriding it with defaults.
    fn resolve_create_config(&self, _input: &Value) -> Option<Result<Value>> {
        None
    }
    fn is_create_config_unattended(&self, _input: &Value) -> Option<bool> {
        None
    }
    async fn create_session(
        &self,
        config: Value,
        context: LaunchContext,
        options: Value,
        sink: EventSink,
    ) -> Result<Arc<dyn AgentSession>>;
    async fn resume_session(
        &self,
        persistence: Value,
        config: Value,
        context: LaunchContext,
        options: Value,
        sink: EventSink,
    ) -> Result<Arc<dyn AgentSession>>;
    async fn fetch_catalog(&self, options: Value, cancel: CancellationToken) -> Result<Value>;
    /// None preserves target-scoped discovery; Some shares only this provider's
    /// catalogs under its source-owned identity key.
    async fn catalog_cache_key(
        &self,
        _options: Value,
        _cancel: CancellationToken,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    fn resolve_configured_model(&self, model: Value) -> Result<Value> {
        Ok(model)
    }
    async fn resolve_default_mode(
        &self,
        _input: Value,
        _cancel: CancellationToken,
    ) -> Result<Option<String>> {
        Ok(None)
    }
    async fn list_features(&self, config: &Value) -> Result<Vec<Value>>;
    async fn list_commands(&self, config: &Value) -> Result<Vec<Value>>;
    async fn list_importable_sessions(&self, options: Value) -> Result<Vec<Value>>;
    async fn import_session(
        &self,
        input: Value,
        config: Value,
        stored_config: Value,
        context: LaunchContext,
        sink: EventSink,
    ) -> Result<ImportedSession>;
    async fn is_available(&self, options: Value, cancel: CancellationToken) -> Result<bool>;
    async fn diagnostic(&self) -> Result<String>;
    async fn archive_native_session(&self, persistence: &Value) -> Result<()>;
    async fn unarchive_native_session(&self, persistence: &Value) -> Result<()>;
    async fn shutdown(&self) -> Result<()>;
}
