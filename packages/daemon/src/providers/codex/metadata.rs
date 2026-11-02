//! Codex app-server discovery (version, models, config, collaboration modes,
//! skills) shared by session opens and draft reads of one provider instance.

use super::transport::{RpcError, Transport};
use crate::providers::{
    api::ProviderSpec,
    registry::catalog::{
        memo::{self, Loaded, Memo},
        refresh_activity,
    },
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;

pub(super) struct Metadata {
    pub models: Vec<Value>,
    pub collaboration: Vec<Value>,
    pub resolved_config: Value,
}

/// A history-only session never asks the app-server for its catalog.
impl Default for Metadata {
    fn default() -> Self {
        Self {
            models: Vec::new(),
            collaboration: Vec::new(),
            resolved_config: json!({}),
        }
    }
}

pub(super) struct Discovery {
    pub metadata: Arc<Metadata>,
    /// None when the app-server could not list skills; callers then read the
    /// skill directories themselves.
    pub skills: Option<Vec<Value>>,
}

#[derive(Default)]
pub(super) struct MetadataCache {
    versions: Memo<String>,
    discoveries: Memo<Arc<Discovery>>,
}

impl MetadataCache {
    /// A forced catalog refresh is the user's request to see current models,
    /// credentials and skills; later opens must not reuse earlier discovery.
    pub fn invalidate(&self) {
        self.versions.clear();
        self.discoveries.clear();
    }

    pub async fn probe(&self, spec: &ProviderSpec, cancel: CancellationToken) -> Result<String> {
        let key = memo::key(&json!([spec.command, memo::shared_env(&spec.env)]));
        let waiting = cancel.clone();
        self.versions
            .get_or_load(key, &waiting, || async move {
                super::probe(spec, cancel).await.map(Loaded::Complete)
            })
            .await
    }

    /// `args` and `env` are the app-server launch that `load` talks to. Only
    /// that launch identity, the working directory and the files Codex reads
    /// for config, credentials and skills select a shared result.
    pub async fn discover<F, Fut>(
        &self,
        spec: &ProviderSpec,
        args: &[&str],
        env: &BTreeMap<String, String>,
        cwd: &Value,
        cancel: &CancellationToken,
        load: F,
    ) -> Result<Arc<Discovery>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Loaded<Arc<Discovery>>>>,
    {
        let key = discovery_key(spec, args, env, cwd).await;
        self.discoveries.get_or_load(key, cancel, load).await
    }
}

async fn discovery_key(
    spec: &ProviderSpec,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: &Value,
) -> String {
    let codex_home = super::home(spec, env);
    let mut watched = vec![
        codex_home.join("config.toml"),
        codex_home.join("auth.json"),
        codex_home.join("skills"),
    ];
    if let Some(cwd) = cwd.as_str() {
        let project = Path::new(cwd).join(".codex");
        watched.extend([project.join("config.toml"), project.join("skills")]);
    }
    let fingerprint = memo::fingerprint(&watched).await;
    let mut environment = memo::shared_env(&spec.env);
    environment.extend(memo::shared_env(env));
    memo::key(&json!([spec.command, args, environment, cwd, fingerprint]))
}

/// Auxiliary lookups degrade to defaults, as Codex itself does. A degraded
/// answer is shared only when the app-server definitively lacks the method;
/// transport loss or an internal error may succeed for the next open.
fn degraded(result: Result<Value>, complete: &mut bool) -> Option<Value> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            *complete &= error
                .downcast_ref::<RpcError>()
                .is_some_and(|error| error.code == -32601);
            None
        }
    }
}

pub(super) async fn fetch(client: &Transport, cwd: &Value) -> Result<Loaded<Arc<Discovery>>> {
    let model_reply =
        refresh_activity("model/list", client.request("model/list", json!({}))).await?;
    let models = model_reply["data"]
        .as_array()
        .ok_or_else(|| anyhow!("Codex model/list returned no model array"))?
        .clone();
    let mut complete = true;
    let resolved_config = refresh_activity("config/read", async {
        let read = client.request("config/read", json!({"cwd":cwd})).await;
        let mut config = degraded(read, &mut complete)
            .map(|reply| reply["config"].clone())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let saved = client.request("getUserSavedConfig", json!({})).await;
        if let Some(saved) = degraded(saved, &mut complete) {
            for (source, dest) in [
                ("model", "model"),
                ("modelReasoningEffort", "model_reasoning_effort"),
            ] {
                if saved["config"][source]
                    .as_str()
                    .is_some_and(|s| !s.trim().is_empty())
                {
                    config[dest] = saved["config"][source].clone();
                }
            }
        }
        Ok(config)
    })
    .await?;
    let collaboration = client.request("collaborationMode/list", json!({})).await;
    let collaboration = degraded(collaboration, &mut complete)
        .and_then(|v| v["data"].as_array().cloned())
        .unwrap_or_default();
    let skills = client.request("skills/list", json!({"cwds":[cwd]})).await;
    let skills = degraded(skills, &mut complete).map(|v| {
        v["data"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|v| v["skills"].as_array().into_iter().flatten())
            .filter(|s| s["enabled"] != false && s["name"].is_string() && s["path"].is_string())
            .cloned()
            .collect()
    });
    let discovery = Arc::new(Discovery {
        metadata: Arc::new(Metadata {
            models,
            collaboration,
            resolved_config,
        }),
        skills,
    });
    Ok(if complete {
        Loaded::Complete(discovery)
    } else {
        Loaded::Partial(discovery)
    })
}

#[cfg(test)]
mod tests {
    use super::{Discovery, Metadata, MetadataCache, discovery_key};
    use crate::providers::{api::ProviderSpec, registry::catalog::memo::Loaded};
    use anyhow::anyhow;
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tokio_util::sync::CancellationToken;

    fn spec(home: &Path) -> ProviderSpec {
        ProviderSpec {
            id: "codex".into(),
            family: "codex".into(),
            command: vec!["codex".into()],
            env: BTreeMap::from([("CODEX_HOME".into(), home.to_string_lossy().into_owned())]),
            options: json!({}),
            defaults: json!({}),
            runtime_settings: json!({}),
            configured_models: Vec::new(),
        }
    }

    fn agent(id: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("PASEO_AGENT_ID".to_owned(), id.to_owned())])
    }

    fn discovered(model: &str) -> Arc<Discovery> {
        Arc::new(Discovery {
            metadata: Arc::new(Metadata {
                models: vec![json!({"id":model})],
                ..Metadata::default()
            }),
            skills: Some(Vec::new()),
        })
    }

    async fn open(
        cache: &MetadataCache,
        spec: &ProviderSpec,
        env: &BTreeMap<String, String>,
        cwd: &Value,
        loads: &AtomicUsize,
        model: &str,
    ) -> String {
        let discovery = cache
            .discover(
                spec,
                &["app-server"],
                env,
                cwd,
                &CancellationToken::new(),
                || async {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(Loaded::Complete(discovered(model)))
                },
            )
            .await
            .unwrap();
        discovery.metadata.models[0]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn agents_in_one_workspace_reuse_one_discovery() {
        let home = tempfile::tempdir().unwrap();
        let spec = spec(home.path());
        let cache = MetadataCache::default();
        let loads = AtomicUsize::new(0);
        let cwd = json!("/work/a");
        let first = open(&cache, &spec, &agent("a"), &cwd, &loads, "gpt-a").await;
        let second = open(&cache, &spec, &agent("b"), &cwd, &loads, "gpt-b").await;
        assert_eq!((first.as_str(), second.as_str()), ("gpt-a", "gpt-a"));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_opens_wait_for_one_app_server_discovery() {
        let home = tempfile::tempdir().unwrap();
        let spec = spec(home.path());
        let cache = MetadataCache::default();
        let loads = AtomicUsize::new(0);
        let cwd = json!("/work/a");
        let (first, second) = (agent("a"), agent("b"));
        let opens = futures_util::future::join_all([
            open(&cache, &spec, &first, &cwd, &loads, "gpt-a"),
            open(&cache, &spec, &second, &cwd, &loads, "gpt-b"),
        ])
        .await;
        assert_eq!(opens[0], opens[1]);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn workspace_environment_command_and_launch_flags_are_separate_scopes() {
        let home = tempfile::tempdir().unwrap();
        let base = spec(home.path());
        let env = agent("a");
        let cwd = json!("/work/a");
        let reference = discovery_key(&base, &["app-server"], &env, &cwd).await;
        assert_ne!(
            reference,
            discovery_key(&base, &["app-server"], &env, &json!("/work/b")).await
        );
        assert_ne!(
            reference,
            discovery_key(&base, &["app-server", "--enable", "goals"], &env, &cwd).await
        );
        let mut keyed = env.clone();
        keyed.insert("OPENAI_API_KEY".into(), "other-account".into());
        assert_ne!(
            reference,
            discovery_key(&base, &["app-server"], &keyed, &cwd).await
        );
        let mut command = base.clone();
        command.command = vec!["/opt/codex-next/bin/codex".into()];
        assert_ne!(
            reference,
            discovery_key(&command, &["app-server"], &env, &cwd).await
        );
        let other_home = tempfile::tempdir().unwrap();
        assert_ne!(
            reference,
            discovery_key(&spec(other_home.path()), &["app-server"], &env, &cwd).await
        );
        assert_eq!(
            reference,
            discovery_key(&base, &["app-server"], &agent("b"), &cwd).await
        );
    }

    #[tokio::test]
    async fn login_or_config_edits_rediscover_instead_of_reusing() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let spec = spec(home.path());
        let cache = MetadataCache::default();
        let loads = AtomicUsize::new(0);
        let env = agent("a");
        let cwd = json!(project.path());
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "before").await,
            "before"
        );
        std::fs::write(home.path().join("auth.json"), r#"{"account":"other"}"#).unwrap();
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "login").await,
            "login"
        );
        std::fs::write(home.path().join("config.toml"), "model = \"gpt-5\"").unwrap();
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "config").await,
            "config"
        );
        std::fs::create_dir_all(project.path().join(".codex/skills")).unwrap();
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "skills").await,
            "skills"
        );
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "unused").await,
            "skills"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn forced_refresh_discards_reused_discovery() {
        let home = tempfile::tempdir().unwrap();
        let spec = spec(home.path());
        let cache = MetadataCache::default();
        let loads = AtomicUsize::new(0);
        let (env, cwd) = (agent("a"), json!("/work/a"));
        open(&cache, &spec, &env, &cwd, &loads, "old").await;
        cache.invalidate();
        assert_eq!(open(&cache, &spec, &env, &cwd, &loads, "new").await, "new");
        assert_eq!(loads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_discovery_is_not_reused_by_the_next_open() {
        let home = tempfile::tempdir().unwrap();
        let spec = spec(home.path());
        let cache = MetadataCache::default();
        let loads = AtomicUsize::new(0);
        let (env, cwd) = (agent("a"), json!("/work/a"));
        let failed = cache
            .discover(
                &spec,
                &["app-server"],
                &env,
                &cwd,
                &CancellationToken::new(),
                || async {
                    Err::<Loaded<Arc<Discovery>>, _>(anyhow!("Codex model/list disconnected"))
                },
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(
            open(&cache, &spec, &env, &cwd, &loads, "retry").await,
            "retry"
        );
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }
}
