//! Native Paseo plugins are standalone executables, not dynamically loaded libraries.
//! Standard output belongs exclusively to the bounded newline-JSON protocol. Use
//! [`Context::log`] or standard error for diagnostics. [`Plugin::run`] also implements
//! the host's `--validate <directory> <daemon-version>` command without invoking handlers.
pub mod acp;
pub mod daemon;
pub mod provider;
mod runtime;
mod settings;
pub mod usage;

pub use anyhow::{Result, anyhow, bail, ensure};
pub use async_trait::async_trait;
use serde_json::{Value, json};
pub use settings::{Settings, SettingsDefinition};
use std::{collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc};
pub use tokio_util::sync::CancellationToken;

pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
pub(crate) type Handler = Arc<dyn Fn(Value, Context) -> HandlerFuture + Send + Sync>;
pub(crate) fn handler<F, Fut>(function: F) -> Handler
where
    F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value>> + Send + 'static,
{
    Arc::new(move |value, context| Box::pin(function(value, context)))
}

#[derive(Clone)]
pub struct Context {
    pub plugin_id: Arc<str>,
    pub plugin_directory: Arc<PathBuf>,
    pub app_version: Arc<str>,
    pub cancellation: CancellationToken,
    pub daemon: daemon::Daemon,
    pub(crate) output: runtime::Output,
}
impl Context {
    /// Diagnostics are base64 framed so they cannot corrupt protocol stdout.
    pub async fn log(&self, message: impl AsRef<str>) -> Result<()> {
        use base64::Engine;
        let text = message.as_ref();
        for part in text.as_bytes().chunks(8192) {
            self.output.send(json!({"type":"log","stream":"stderr","data":base64::engine::general_purpose::STANDARD.encode(part)})).await?;
        }
        self.output
            .send(json!({"type":"log","stream":"stderr","data":"Cg=="}))
            .await
    }
}

/// Registration is deterministic and complete before the executable advertises readiness.
#[derive(Default)]
pub struct Plugin {
    pub(crate) methods: BTreeMap<String, Handler>,
    pub(crate) events: BTreeMap<String, Vec<Handler>>,
    pub(crate) before: BTreeMap<String, Vec<Handler>>,
    pub(crate) providers: BTreeMap<String, Arc<dyn provider::Provider>>,
    pub(crate) usage: BTreeMap<String, UsageSource>,
    pub(crate) settings: Vec<Settings>,
    pub(crate) cleanup: Option<Handler>,
}
impl Plugin {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn rpc<F, Fut>(&mut self, name: &str, function: F) -> Result<&mut Self>
    where
        F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        valid_id(name)?;
        ensure!(
            !self.methods.contains_key(name),
            "Duplicate RPC method: {name}"
        );
        self.methods.insert(name.into(), handler(function));
        Ok(self)
    }
    /// Event handlers run in registration order. Failures are logged without preventing later handlers.
    pub fn on<F, Fut>(&mut self, name: &str, function: F) -> Result<&mut Self>
    where
        F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        ensure!(EVENTS.contains(&name), "Unknown lifecycle event: {name}");
        self.events
            .entry(name.into())
            .or_default()
            .push(handler(function));
        Ok(self)
    }
    /// Input is `{request: ...}`. Return null to preserve it, or the complete transformed request.
    pub fn before<F, Fut>(&mut self, name: &str, function: F) -> Result<&mut Self>
    where
        F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        ensure!(BEFORE.contains(&name), "Unknown before hook: {name}");
        self.before
            .entry(name.into())
            .or_default()
            .push(handler(function));
        Ok(self)
    }
    pub fn provider(&mut self, provider: impl provider::Provider + 'static) -> Result<&mut Self> {
        let metadata = provider.metadata();
        metadata.validate()?;
        ensure!(
            !self.providers.contains_key(&metadata.id),
            "Duplicate provider: {}",
            metadata.id
        );
        self.providers
            .insert(metadata.id.clone(), Arc::new(provider));
        Ok(self)
    }
    pub fn usage_source(&mut self, source: UsageSource) -> Result<&mut Self> {
        valid_id(&source.id)?;
        ensure!(
            !source.label.trim().is_empty(),
            "Usage source requires a label"
        );
        ensure!(
            !self.usage.contains_key(&source.id),
            "Duplicate usage source: {}",
            source.id
        );
        self.usage.insert(source.id.clone(), source);
        Ok(self)
    }
    pub fn settings(&mut self, definition: SettingsDefinition) -> Result<Settings> {
        let settings = Settings::new(definition)?;
        let id = settings.id().to_owned();
        for operation in ["read", "write", "reset"] {
            ensure!(
                !self
                    .methods
                    .contains_key(&format!("settings.{id}.{operation}")),
                "Duplicate settings: {id}"
            );
        }
        for operation in ["read", "write", "reset"] {
            let settings = settings.clone();
            self.rpc(
                &format!("settings.{id}.{operation}"),
                move |input, _context| {
                    let settings = settings.clone();
                    async move { settings.invoke(operation, input).await }
                },
            )?;
        }
        self.settings.push(settings.clone());
        Ok(settings)
    }
    pub fn cleanup<F, Fut>(&mut self, function: F) -> Result<&mut Self>
    where
        F: Fn(Value, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        ensure!(self.cleanup.is_none(), "Cleanup is already registered");
        self.cleanup = Some(handler(function));
        Ok(self)
    }
    pub async fn run(self) -> Result<()> {
        runtime::run(self).await
    }
    /// The same runtime on caller-owned IO, useful for protocol tests and embedding in a process supervisor.
    pub async fn serve<R, W>(self, reader: R, writer: W) -> Result<()>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        runtime::serve(self, reader, writer).await
    }
    pub(crate) fn metadata(&self) -> Value {
        json!({"methods":self.methods.keys().collect::<Vec<_>>(),
            "hooks":{"events":self.events.keys().collect::<Vec<_>>(),"before":self.before.keys().collect::<Vec<_>>()},
            "providers":self.providers.values().map(|provider|provider.metadata()).collect::<Vec<_>>(),
            "usageSources":self.usage.values().map(|source| {
                let mut metadata=json!({"id":source.id,"label":source.label});
                if let Some(icon)=&source.icon {metadata["icon"]=json!(icon);}
                metadata
            }).collect::<Vec<_>>()})
    }
}

pub struct UsageSource {
    pub id: String,
    pub label: String,
    /// SVG text, not a path, matching ready metadata used by the host.
    pub icon: Option<String>,
    pub(crate) discover: Handler,
    pub(crate) fetch: Handler,
}
impl UsageSource {
    /// Discovery receives a global/session scope. Fetch receives the persisted source input.
    /// Validate credentials/input in the callback before contacting an upstream service.
    pub fn new<D, DF, F, FF>(
        id: impl Into<String>,
        label: impl Into<String>,
        discover: D,
        fetch: F,
    ) -> Self
    where
        D: Fn(Value, Context) -> DF + Send + Sync + 'static,
        DF: Future<Output = Result<Value>> + Send + 'static,
        F: Fn(Value, Context) -> FF + Send + Sync + 'static,
        FF: Future<Output = Result<Value>> + Send + 'static,
    {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            discover: handler(discover),
            fetch: handler(fetch),
        }
    }
}

pub(crate) const EVENTS: &[&str] = &[
    "agent.created",
    "agent.closed",
    "agent.turn_started",
    "agent.turn_ended",
    "agent.permission_requested",
    "agent.permission_resolved",
    "agent.archived",
    "workspace.created",
    "workspace.archived",
];
pub(crate) const BEFORE: &[&str] = &["agent.create", "agent.session_open", "workspace.create"];
pub(crate) fn valid_id(id: &str) -> Result<()> {
    ensure!(
        id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c)),
        "Invalid plugin identifier: {id}"
    );
    Ok(())
}
