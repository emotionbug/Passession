use crate::{Result, anyhow, ensure, runtime::Output, valid_id};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};
use tokio::{
    fs,
    io::AsyncWriteExt,
    sync::{Mutex, OnceCell, broadcast},
};

type Validate = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;
type Migration =
    Arc<dyn Fn(Value, u64) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync>;

/// Validator returns normalized JSON, including defaults. Invalid stored values
/// remain intact until an explicit reset, avoiding silent configuration loss.
pub struct SettingsDefinition {
    pub id: String,
    pub version: u64,
    validate: Validate,
    migrate: Option<Migration>,
}
impl SettingsDefinition {
    pub fn new(
        id: impl Into<String>,
        version: u64,
        validate: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            version,
            validate: Arc::new(validate),
            migrate: None,
        }
    }
    pub fn migrate<F, Fut>(mut self, migrate: F) -> Self
    where
        F: Fn(Value, u64) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value>> + Send + 'static,
    {
        self.migrate = Some(Arc::new(move |value, version| {
            Box::pin(migrate(value, version))
        }));
        self
    }
}
struct Inner {
    definition: SettingsDefinition,
    binding: OnceCell<(PathBuf, Output)>,
    serial: Mutex<()>,
    changes: broadcast::Sender<Value>,
}
#[derive(Clone)]
pub struct Settings(Arc<Inner>);
impl Settings {
    pub(crate) fn new(definition: SettingsDefinition) -> Result<Self> {
        valid_id(&definition.id)?;
        ensure!(!definition.id.contains('.'), "Invalid settings ID");
        ensure!(definition.version > 0, "Invalid settings version");
        let (changes, _) = broadcast::channel(32);
        Ok(Self(Arc::new(Inner {
            definition,
            binding: OnceCell::new(),
            serial: Mutex::new(()),
            changes,
        })))
    }
    pub fn id(&self) -> &str {
        &self.0.definition.id
    }
    pub(crate) fn bind(&self, directory: PathBuf, output: Output) -> Result<()> {
        self.0
            .binding
            .set((directory, output))
            .map_err(|_| anyhow!("Settings already initialized"))
    }
    /// Lagged subscribers receive an explicit broadcast error and can call read()
    /// to resynchronize; stored revisions remain the source of truth.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.0.changes.subscribe()
    }
    pub async fn read(&self) -> Result<Value> {
        self.invoke("read", json!({})).await
    }
    pub async fn write(&self, revision: &str, values: Value) -> Result<Value> {
        self.invoke("write", json!({"revision":revision,"values":values}))
            .await
    }
    pub async fn reset(&self, revision: &str) -> Result<Value> {
        self.invoke("reset", json!({"revision":revision})).await
    }
    async fn stored(&self) -> Result<(Option<Vec<u8>>, String)> {
        let (directory, _) = self
            .0
            .binding
            .get()
            .ok_or_else(|| anyhow!("Settings storage unavailable"))?;
        match fs::read(directory.join(format!("{}.json", self.id()))).await {
            Ok(raw) => {
                let revision = digest(&raw);
                Ok((Some(raw), revision))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok((None, "missing".into()))
            }
            Err(error) => Err(error.into()),
        }
    }
    async fn persist(&self, values: &Value) -> Result<String> {
        let (directory, _) = self
            .0
            .binding
            .get()
            .ok_or_else(|| anyhow!("Settings storage unavailable"))?;
        fs::create_dir_all(directory).await?;
        let path = directory.join(format!("{}.json", self.id()));
        let temporary = directory.join(format!("{}.{}.tmp", self.id(), uuid::Uuid::new_v4()));
        let raw =
            serde_json::to_vec(&json!({"version":self.0.definition.version,"values":values}))?;
        let result: Result<()> = async {
            let mut options = fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await?;
            file.write_all(&raw).await?;
            file.sync_all().await?;
            drop(file);
            fs::rename(&temporary, &path).await?;
            Ok(())
        }
        .await;
        let _ = fs::remove_file(&temporary).await;
        result?;
        Ok(digest(&raw))
    }
    async fn notify(&self, state: Value) -> Result<()> {
        let _ = self.0.changes.send(state);
        self.0
            .binding
            .get()
            .ok_or_else(|| anyhow!("Settings storage unavailable"))?
            .1
            .send(json!({"type":"settings.changed","settingsId":self.id()}))
            .await
    }
    pub(crate) async fn invoke(&self, operation: &str, input: Value) -> Result<Value> {
        let _guard = self.0.serial.lock().await;
        let (raw, revision) = self.stored().await?;
        let definition = &self.0.definition;
        if operation == "read" {
            let result: Result<(Value, bool)> = async {
                let (mut values, version) = match &raw {
                    Some(raw) => {
                        let envelope: Value = serde_json::from_slice(raw)?;
                        envelope_parts(envelope)?
                    }
                    None => (json!({}), definition.version),
                };
                if version != definition.version {
                    ensure!(
                        version < definition.version,
                        "Settings were saved by a newer plugin version"
                    );
                    let migrate = definition.migrate.as_ref().ok_or_else(|| {
                        anyhow!("Settings version {version} requires a migration")
                    })?;
                    values = migrate(values, version).await?;
                }
                Ok((
                    (definition.validate)(values)?,
                    version != definition.version,
                ))
            }
            .await;
            return match result {
                Ok((values, migrated)) => {
                    let revision = if migrated {
                        self.persist(&values).await?
                    } else {
                        revision
                    };
                    let state = json!({"status":"ready","revision":revision,"values":values});
                    if migrated {
                        self.notify(state.clone()).await?;
                    }
                    Ok(state)
                }
                Err(error) => {
                    Ok(json!({"status":"invalid","revision":revision,"error":error.to_string()}))
                }
            };
        }
        ensure!(
            operation == "write" || operation == "reset",
            "Unknown settings operation"
        );
        let expected = input["revision"]
            .as_str()
            .ok_or_else(|| anyhow!("Settings write requires revision"))?;
        if expected != revision {
            return Ok(
                json!({"status":"conflict","error":"Settings changed on another client. Reload before saving again."}),
            );
        }
        let parsed: Result<Value> = (|| {
            if operation == "write" {
                if let Some(raw) = &raw {
                    let (_, version) = envelope_parts(serde_json::from_slice(raw)?)?;
                    ensure!(
                        version == definition.version,
                        "Reload or reset settings before saving a different schema version"
                    );
                }
                (definition.validate)(
                    input
                        .get("values")
                        .ok_or_else(|| anyhow!("Settings write requires values"))?
                        .clone(),
                )
            } else {
                (definition.validate)(json!({}))
            }
        })();
        let values = match parsed {
            Ok(value) => value,
            Err(error) => return Ok(json!({"status":"invalid","error":error.to_string()})),
        };
        let revision = self.persist(&values).await?;
        self.notify(json!({"status":"ready","values":values,"revision":revision}))
            .await?;
        Ok(json!({"status":"saved","values":values,"revision":revision}))
    }
}
fn envelope_parts(mut envelope: Value) -> Result<(Value, u64)> {
    let version = envelope["version"]
        .as_u64()
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow!("Invalid settings envelope version"))?;
    ensure!(
        envelope.get("values").is_some(),
        "Settings envelope requires values"
    );
    Ok((envelope["values"].take(), version))
}
fn digest(raw: &[u8]) -> String {
    format!("{:x}", Sha256::digest(raw))
}
