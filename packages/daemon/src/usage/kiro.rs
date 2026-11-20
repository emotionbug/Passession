//! Read-only built-in usage for OMP's managed Kiro HTTP authentication stores.
mod accounts;
mod auth;
mod normalize;
mod transport;

use super::{
    UsageContext,
    common::{UsageAccount, UsageRuntime, UsageScope, hash_account_key, unavailable},
};
use accounts::{DEFAULT_ACCOUNT, Target};
use anyhow::Result;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const LOGIN_UNAVAILABLE: &str =
    "Kiro HTTP login unavailable. Run omp-kiro login, then refresh usage.";
const LOGIN_CHANGED: &str =
    "This Kiro HTTP login or profile changed. Refresh the usage account list.";
const CANCELLED: &str = "The Kiro usage request was cancelled.";
const TIMEOUT: &str = "The Kiro usage request timed out.";

/// Which account profiles a usage scope covers.
#[derive(Debug, PartialEq)]
enum Selection {
    /// Daemon-wide: every signed-in account, one row per profile when it has several.
    All,
    /// A `kiro-http/…` session: the default account's saved profile.
    Saved,
    /// A session on a pinned profile or API-key provider.
    Pinned(String),
}

struct EnvironmentKey {
    credential: Result<auth::Credential, &'static str>,
    extra_ca: Option<PathBuf>,
}

/// Credential bytes stay in the request runtime, never in a report locator.
#[derive(Clone, Default)]
pub(super) struct Runtime(Arc<HashMap<String, EnvironmentKey>>);

impl Runtime {
    pub(super) fn new(context: &UsageContext) -> Self {
        let mut keys = HashMap::new();
        let mut add = |env: &BTreeMap<String, String>| {
            if let Some((id, credential)) = auth::environment_key(env) {
                keys.insert(
                    id,
                    EnvironmentKey {
                        credential,
                        extra_ca: extra_ca(env),
                    },
                );
            }
        };
        add(&context.env);
        for session in &context.sessions {
            if session["_runtimeUnavailable"] != true
                && let Some(env) = session["env"].as_object()
                && env.contains_key("KIRO_API_KEY")
            {
                // Only these values can affect API-key authentication and TLS.
                let env = ["KIRO_API_KEY", "KIRO_API_REGION", "NODE_EXTRA_CA_CERTS"]
                    .into_iter()
                    .filter_map(|key| Some((key.to_owned(), env.get(key)?.as_str()?.to_owned())))
                    .collect();
                add(&env);
            }
        }
        Self(Arc::new(keys))
    }
}

fn extra_ca(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    env.get("NODE_EXTRA_CA_CERTS")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn unmeasured(detail: &str) -> Value {
    unavailable(json!({"kind":"no_quota","detail":detail}))
}

fn scope_env(
    scope: &UsageScope,
    global: impl FnOnce() -> BTreeMap<String, String>,
) -> Option<(BTreeMap<String, String>, Selection)> {
    match scope.provider.as_deref() {
        None => return Some((global(), Selection::All)),
        Some("omp" | "kiro-http") => {}
        _ => return None,
    }
    let model_provider = scope
        .model
        .as_deref()
        .and_then(|model| model.split_once('/'))
        .map(|(provider, _)| provider);
    if matches!(model_provider, Some("kiro" | "kiro-cli")) {
        return None;
    }
    let pinned =
        model_provider.filter(|provider| provider.starts_with("kiro-") && *provider != "kiro-http");
    if let Some(provider) = pinned {
        return Some((scope.env.clone(), Selection::Pinned(provider.to_owned())));
    }
    // Preserve OMP HTTP markers, without claiming CLI/non-HTTP sessions.
    if scope.provider.as_deref() == Some("kiro-http")
        || (model_provider.is_none()
            && scope
                .env
                .get("OMP_KIRO_TRANSPORT")
                .is_some_and(|v| v == "http"))
        || model_provider == Some("kiro-http")
    {
        Some((scope.env.clone(), Selection::Saved))
    } else {
        None
    }
}

pub(super) async fn discover(rt: &UsageRuntime, scope: &UsageScope) -> Result<Vec<UsageAccount>> {
    let Some((env, selection)) = scope_env(scope, || {
        let mut env = rt.env.clone();
        if !env
            .get("HOME")
            .is_some_and(|home| std::path::Path::new(home).is_absolute())
        {
            env.insert("HOME".into(), rt.home.to_string_lossy().into_owned());
        }
        env
    }) else {
        return Ok(Vec::new());
    };
    Ok(discover_env(env, selection, &rt.cancel).await)
}
async fn discover_env(
    env: BTreeMap<String, String>,
    selection: Selection,
    cancellation: &CancellationToken,
) -> Vec<UsageAccount> {
    if cancellation.is_cancelled() {
        return Vec::new();
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Vec::new(),
        _ = tokio::time::sleep(Duration::from_secs(3)) => Vec::new(),
        result = tokio::task::spawn_blocking(move || {
            let default_key = auth::environment_key(&env);
            let mut rows = Vec::new();
            let overridden = default_key.is_some();
            let uses_environment = match &selection {
                Selection::All | Selection::Saved => true,
                Selection::Pinned(provider) => provider == "kiro-default-key",
            };
            if let Some((id, _)) = default_key.filter(|_| uses_environment) {
                rows.push(UsageAccount {
                    key: id.clone(),
                    label: Some("Kiro HTTP · environment API key".into()),
                    harness: Some("Kiro HTTP".into()),
                    input: json!(Input::Environment { login_id: id }),
                });
            }
            let Ok(default) = auth::auth_path(&env) else {
                return rows;
            };
            let stores = if selection == Selection::Saved {
                vec![(DEFAULT_ACCOUNT.to_owned(), default)]
            } else {
                accounts::stores(&default)
            };
            rows.extend(stores.iter().flat_map(|(name, path)| {
                store_accounts(name, path, &selection, overridden)
            }));
            rows
        }) => result.unwrap_or_default(),
    }
}

fn store_accounts(
    name: &str,
    path: &Path,
    selection: &Selection,
    overridden: bool,
) -> Vec<UsageAccount> {
    if name == DEFAULT_ACCOUNT && overridden {
        return Vec::new();
    }
    let Ok(path) = path.canonicalize() else {
        return Vec::new();
    };
    let Ok((credential, catalog)) = auth::read_store(&path) else {
        return Vec::new();
    };
    let Some(path) = path.to_str() else {
        return Vec::new();
    };
    let saved_arn = credential.profile_arn.as_deref();
    let saved = || match saved_arn {
        Some(arn) => Profile::Saved {
            profile_arn: arn.to_owned(),
        },
        None => Profile::Unselected,
    };
    let make_account = |profile: Profile, profile_label: Option<&str>| UsageAccount {
        key: profile.arn().map_or_else(
            || hash_account_key(path),
            |arn| hash_account_key(&format!("{path}\n{arn}")),
        ),
        label: Some(match (profile_label, name == DEFAULT_ACCOUNT) {
            (Some(label), _) => format!(
                "Kiro HTTP · {name} · {}",
                label.replace(char::is_control, " ")
            ),
            (None, true) => format!("Kiro HTTP · {}", credential.provider),
            (None, false) => format!("Kiro HTTP · {name} · {}", credential.provider),
        }),
        harness: Some("Kiro HTTP".into()),
        input: json!(Input::Store {
            auth_file: path.to_owned(),
            login_id: credential.login_id.clone(),
            profile,
        }),
    };
    if credential.is_api_key() {
        return match selection {
            Selection::Pinned(id) if id != &format!("kiro-{name}-key") => Vec::new(),
            _ => vec![make_account(Profile::ApiKey, None)],
        };
    }
    if *selection == Selection::Saved {
        return vec![make_account(saved(), None)];
    }
    let targets = accounts::profile_targets(name, &credential, catalog);
    let target_account = |target: &Target| {
        let arn = target.arn.as_str();
        let sole_saved = targets.len() == 1 && saved_arn == Some(arn);
        let profile = if sole_saved || (*selection == Selection::All && saved_arn == Some(arn)) {
            saved()
        } else {
            Profile::Listed {
                profile_arn: arn.to_owned(),
            }
        };
        make_account(
            profile,
            (!sole_saved).then(|| target.name.as_deref().unwrap_or(arn)),
        )
    };
    match selection {
        Selection::Pinned(provider) => targets
            .iter()
            .find(|target| target.provider.as_deref() == Some(provider))
            .map(target_account)
            .into_iter()
            .collect(),
        _ if !targets.is_empty() => targets.iter().map(target_account).collect(),
        _ => vec![make_account(saved(), None)],
    }
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Input {
    Environment {
        login_id: String,
    },
    Store {
        auth_file: String,
        login_id: String,
        profile: Profile,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum Profile {
    ApiKey,
    Unselected,
    Saved { profile_arn: String },
    Listed { profile_arn: String },
}

impl Profile {
    fn arn(&self) -> Option<&str> {
        match self {
            Self::ApiKey | Self::Unselected => None,
            Self::Saved { profile_arn } | Self::Listed { profile_arn } => Some(profile_arn),
        }
    }
}

pub(super) async fn fetch(rt: &UsageRuntime, input: &Value) -> Result<Value> {
    let input: Input = match Input::deserialize(input) {
        Ok(input) => input,
        Err(_) => {
            return Ok(transport::error(
                "Invalid Kiro authentication store locator.",
            ));
        }
    };
    if let Input::Store {
        auth_file, profile, ..
    } = &input
        && (!Path::new(auth_file).is_absolute()
            || !profile.arn().is_none_or(auth::valid_profile_arn))
    {
        return Ok(transport::error(
            "Invalid Kiro authentication store locator.",
        ));
    }
    Ok(tokio::select! {
        biased;
        _ = rt.cancel.cancelled() => transport::error(CANCELLED),
        _ = tokio::time::sleep(Duration::from_secs(15)) => transport::error(TIMEOUT),
        report = fetch_credential(rt, input) => report,
    })
}
async fn fetch_credential(rt: &UsageRuntime, input: Input) -> Value {
    match input {
        Input::Environment { login_id } => {
            let Some(key) = rt.kiro.0.get(&login_id) else {
                return unmeasured(LOGIN_CHANGED);
            };
            let credential = match &key.credential {
                Ok(credential) => credential,
                Err(message) => return unmeasured(message),
            };
            request(rt, credential, key.extra_ca.clone(), true).await
        }
        Input::Store {
            auth_file,
            login_id,
            profile,
        } => {
            let result =
                tokio::task::spawn_blocking(move || auth::read_store(Path::new(&auth_file))).await;
            let Ok(Ok((mut credential, catalog))) = result else {
                return unmeasured(LOGIN_UNAVAILABLE);
            };
            let same_profile = match &profile {
                Profile::ApiKey => credential.is_api_key(),
                Profile::Unselected => !credential.is_api_key() && credential.profile_arn.is_none(),
                Profile::Saved { profile_arn } => {
                    !credential.is_api_key() && credential.profile_arn.as_ref() == Some(profile_arn)
                }
                Profile::Listed { profile_arn } => {
                    !credential.is_api_key()
                        && catalog.is_some_and(|profiles| {
                            profiles.iter().any(|profile| profile.arn == *profile_arn)
                        })
                }
            };
            if !same_profile || credential.login_id != login_id {
                return unmeasured(LOGIN_CHANGED);
            }
            credential.profile_arn = profile.arn().map(str::to_owned);
            request(rt, &credential, extra_ca(&rt.env), false).await
        }
    }
}

async fn request(
    rt: &UsageRuntime,
    credential: &auth::Credential,
    extra_ca: Option<PathBuf>,
    environment: bool,
) -> Value {
    if credential.disabled {
        return unmeasured("This Kiro HTTP login is disabled. Sign in again with omp-kiro login.");
    }
    if let Some(expires_at) = credential
        .expires_at
        .filter(|expires_at| *expires_at <= Utc::now())
    {
        return json!({"status":"unavailable", "problem":{
            "kind":"expired", "expiresAt":expires_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "refreshedBy":"omp-kiro",
        }});
    }
    if !credential.is_api_key() && credential.profile_arn.is_none() {
        return unmeasured("Select a Kiro HTTP profile in omp-kiro, then refresh usage.");
    }
    // SQLite connections are dropped before TLS setup or any HTTP await.
    let client =
        match tokio::task::spawn_blocking(move || transport::client(extra_ca.as_deref())).await {
            Ok(Ok(client)) => client,
            Ok(Err(message)) => return transport::error(message),
            Err(_) => return transport::error("The Kiro usage service could not be reached."),
        };
    if environment && credential.region.is_none() {
        let Some(region) = transport::probe_api_key_region(rt, &client, credential).await else {
            return unmeasured(
                "Kiro API key did not resolve to one region; set KIRO_API_REGION to the key's AWS region.",
            );
        };
        return transport::fetch_region(rt, client, credential, region).await;
    }
    transport::fetch(rt, client, credential).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::Path;

    pub(super) fn runtime(home: &Path) -> UsageRuntime {
        UsageRuntime {
            home: home.into(),
            env: BTreeMap::new(),
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            endpoints: BTreeMap::new(),
            muse_providers: Vec::new(),
            omp: Default::default(),
            kiro: Default::default(),
            plugins: None,
        }
    }
    fn global() -> UsageScope {
        UsageScope {
            provider: None,
            model: None,
            env: BTreeMap::new(),
        }
    }

    pub(super) fn store(path: &Path, provider: &str, expires_at: &str) {
        let db = Connection::open(path).unwrap();
        db.execute(
            "CREATE TABLE credentials (id TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        db.execute("INSERT INTO credentials VALUES ('active', ?1)", [json!({"kind":"social","provider":provider,"access_token":"private-access-token","expires_at":expires_at}).to_string()]).unwrap();
    }
    pub(super) fn scoped(path: &Path) -> UsageScope {
        UsageScope {
            provider: Some("omp".into()),
            model: Some("kiro-http/auto".into()),
            env: BTreeMap::from([("OMP_KIRO_AUTH_FILE".into(), path.to_str().unwrap().into())]),
        }
    }
    async fn discover_found(
        found: Option<(BTreeMap<String, String>, Selection)>,
    ) -> Vec<UsageAccount> {
        let (env, selection) = found.unwrap();
        discover_env(env, selection, &CancellationToken::new()).await
    }

    fn saved_input(path: &Path, login_id: &str) -> Value {
        json!(Input::Store {
            auth_file: path.to_string_lossy().into_owned(),
            login_id: login_id.to_owned(),
            profile: Profile::Unselected,
        })
    }

    #[tokio::test]
    async fn source_session_auth_is_isolated_and_account_has_no_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("daemon.db");
        let session = dir.path().join("session.db");
        store(&daemon, "google", "2099-01-01T00:00:00Z");
        store(&session, "github", "2099-01-01T00:00:00Z");
        let accounts = discover_found(scope_env(&scoped(&session), || {
            panic!("session must not inspect daemon environment")
        }))
        .await;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label.as_deref(), Some("Kiro HTTP · github"));
        let canonical = session.canonicalize().unwrap();
        assert_eq!(
            accounts[0].key,
            hash_account_key(canonical.to_str().unwrap())
        );
        assert_eq!(
            accounts[0].input,
            saved_input(&canonical, &auth::read(&canonical).unwrap().login_id)
        );
        assert!(
            !accounts[0]
                .input
                .to_string()
                .contains("private-access-token")
        );
        let accounts = discover_found(scope_env(&global(), || {
            BTreeMap::from([("OMP_KIRO_AUTH_FILE".into(), daemon.to_str().unwrap().into())])
        }))
        .await;
        assert_eq!(accounts[0].label.as_deref(), Some("Kiro HTTP · google"));
        let missing = dir.path().join("missing.db");
        assert!(
            discover_found(scope_env(&scoped(&missing), || panic!(
                "no daemon fallback"
            )))
            .await
            .is_empty()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn source_only_claims_http_scope_markers() {
        for marker in 0..3 {
            let scope = UsageScope {
                provider: Some(if marker == 0 { "kiro-http" } else { "omp" }.into()),
                model: (marker == 1).then(|| "kiro-http/auto".into()),
                env: if marker == 2 {
                    BTreeMap::from([("OMP_KIRO_TRANSPORT".into(), "http".into())])
                } else {
                    BTreeMap::new()
                },
            };
            assert!(scope_env(&scope, || panic!("no global fallback")).is_some());
        }
        assert!(
            scope_env(
                &UsageScope {
                    provider: Some("omp".into()),
                    model: Some("kiro/auto".into()),
                    env: BTreeMap::new()
                },
                || panic!("no global fallback")
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn source_expired_is_discovered_and_reported_without_refresh_or_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        store(&path, "github", "2000-01-01T00:00:00Z");
        let before = std::fs::read(&path).unwrap();
        let accounts = discover_found(scope_env(&scoped(&path), BTreeMap::new)).await;
        assert_eq!(accounts.len(), 1);
        let result = fetch(&runtime(dir.path()), &accounts[0].input)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({"status":"unavailable","problem":{"kind":"expired","expiresAt":"2000-01-01T00:00:00.000Z","refreshedBy":"omp-kiro"}})
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn source_input_and_store_errors_are_strict_and_sanitized() {
        for input in [
            Value::Null,
            json!({}),
            saved_input(Path::new("relative"), "opaque-id"),
            json!({"kind":"store","authFile":123,"loginId":"opaque-id","profile":{"kind":"api-key"}}),
            json!({"kind":"store","authFile":"/valid","loginId":"opaque-id","profile":{"kind":"api-key"},"accessToken":"private-access-token"}),
            json!({"kind":"store","authFile":"/valid","loginId":"opaque-id","profile":{"kind":"listed","profileArn":"arn:aws:codewhisperer:us-east-1:1:other/X"}}),
        ] {
            let value =
                serde_json::to_value(fetch(&runtime(Path::new("/unused")), &input).await.unwrap())
                    .unwrap();
            assert_eq!(
                value,
                json!({"status":"error","error":"Invalid Kiro authentication store locator."})
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-auth-path.db");
        std::fs::write(&path, b"private-access-token invalid sqlite").unwrap();
        let report = fetch(&runtime(dir.path()), &saved_input(&path, "opaque-id"))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            unmeasured(LOGIN_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn source_cancellation_precedes_storage_and_network_work() {
        let token = CancellationToken::new();
        token.cancel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let mut rt = runtime(dir.path());
        rt.cancel = token.clone();
        let report = fetch(&rt, &saved_input(&path, "opaque-id")).await.unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"status":"error","error":CANCELLED})
        );
        assert!(!path.exists());
        assert!(
            discover_env(BTreeMap::new(), Selection::All, &token)
                .await
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_canonical_aliases_share_an_account_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let alias = dir.path().join("alias.db");
        store(&path, "github", "2099-01-01T00:00:00Z");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let first = discover_found(scope_env(&scoped(&path), BTreeMap::new)).await;
        let second = discover_found(scope_env(&scoped(&alias), BTreeMap::new)).await;
        assert_eq!(first[0].key, second[0].key);
        assert_eq!(first[0].input, second[0].input);
    }

    #[tokio::test]
    async fn source_runtime_env_and_home_apply_only_to_global_scope() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("daemon.db");
        let session_home = dir.path().join("session-home");
        let session = session_home.join(".config/omp-kiro/auth.sqlite3");
        let global_home = dir.path().join("global-home");
        let default_global = global_home.join(".config/omp-kiro/auth.sqlite3");
        std::fs::create_dir_all(session.parent().unwrap()).unwrap();
        std::fs::create_dir_all(default_global.parent().unwrap()).unwrap();
        store(&daemon, "google", "2099-01-01T00:00:00Z");
        store(&session, "github", "2099-01-01T00:00:00Z");
        store(&default_global, "google", "2099-01-01T00:00:00Z");
        let mut rt = runtime(&global_home);
        rt.env
            .insert("OMP_KIRO_AUTH_FILE".into(), daemon.to_str().unwrap().into());
        let global_accounts = discover(&rt, &global()).await.unwrap();
        assert_eq!(
            global_accounts[0].input["authFile"],
            daemon.canonicalize().unwrap().to_str().unwrap()
        );
        let scope = UsageScope {
            provider: Some("omp".into()),
            model: Some("kiro-http/auto".into()),
            env: BTreeMap::from([("HOME".into(), session_home.to_str().unwrap().into())]),
        };
        let accounts = discover(&rt, &scope).await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].input["authFile"],
            session.canonicalize().unwrap().to_str().unwrap()
        );
        rt.env.clear();
        assert_eq!(
            discover(&rt, &global()).await.unwrap()[0].input["authFile"],
            default_global.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(discover(&rt, &scope).await.unwrap()[0].key, accounts[0].key);
        let missing = scoped(&dir.path().join("missing.db"));
        assert!(discover(&rt, &missing).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_cancellation_interrupts_a_stalled_http_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        login(&path, "github", Some(PERSONAL));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut rt = runtime(dir.path());
        rt.endpoints.insert(
            "https://management.us-east-1.kiro.dev/".into(),
            format!("http://{}/", listener.local_addr().unwrap()),
        );
        let cancel = rt.cancel.clone();
        let input = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::Saved, false)
            .remove(0)
            .input;
        let fetch_task = tokio::spawn(async move { fetch(&rt, &input).await.unwrap() });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n")
            .await
            .unwrap();
        cancel.cancel();
        let report = tokio::time::timeout(Duration::from_secs(1), fetch_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report, transport::error(CANCELLED));
    }

    pub(super) const PERSONAL: &str =
        "arn:aws:codewhisperer:us-east-1:123456789012:profile/PERSONAL01";
    const TEAM: &str = "arn:aws:codewhisperer:eu-central-1:123456789012:profile/Team-02";

    pub(super) fn login(path: &Path, provider: &str, profile_arn: Option<&str>) {
        store(path, provider, "2099-01-01T00:00:00Z");
        if let Some(arn) = profile_arn {
            Connection::open(path).unwrap().execute(
                "UPDATE credentials SET value = json_set(value, '$.profile_arn', ?1) WHERE id = 'active'",
                [arn],
            ).unwrap();
        }
    }
    fn catalog(path: &Path, profiles: &[(&str, &str)]) {
        let profiles: Vec<Value> = profiles
            .iter()
            .map(|(arn, name)| json!({"arn":arn,"name":name}))
            .collect();
        Connection::open(path)
            .unwrap()
            .execute(
                "INSERT INTO credentials VALUES ('profiles', ?1)",
                [
                    json!({"version":1,"fetchedAt":"2026-11-20T00:00:00.000Z","profiles":profiles})
                        .to_string(),
                ],
            )
            .unwrap();
    }

    #[tokio::test]
    async fn source_lists_named_accounts_and_scopes_pinned_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("auth.sqlite3");
        let accounts = dir.path().join("accounts");
        std::fs::create_dir(&accounts).unwrap();
        login(
            &default,
            "google",
            Some("arn:aws:codewhisperer:us-east-1:1:profile/DEFAULT01"),
        );
        let work = accounts.join("work.sqlite3");
        login(&work, "github", None);
        catalog(
            &work,
            &[
                (PERSONAL, "Personal"),
                (TEAM, "Team\nEU"),
                (
                    "arn:aws:codewhisperer:ap-northeast-1:1:profile/TOKYO",
                    "Unsupported region",
                ),
            ],
        );
        login(
            &accounts.join("solo.sqlite3"),
            "google",
            Some("arn:aws:codewhisperer:us-east-1:1:profile/SOLO01"),
        );
        for ignored in ["http.sqlite3", "Bad.sqlite3", "default.sqlite3"] {
            login(&accounts.join(ignored), "google", None);
        }
        let env = BTreeMap::from([(
            "OMP_KIRO_AUTH_FILE".into(),
            default.to_str().unwrap().into(),
        )]);
        let all = discover_found(scope_env(&global(), || env.clone())).await;
        let labels: Vec<_> = all
            .iter()
            .map(|account| account.label.as_deref().unwrap())
            .collect();
        assert_eq!(
            labels,
            [
                "Kiro HTTP · google",
                "Kiro HTTP · solo · google",
                "Kiro HTTP · work · Team EU",
                "Kiro HTTP · work · Personal",
            ]
        );
        let work = work.canonicalize().unwrap();
        let work = work.to_str().unwrap();
        assert_eq!(all[3].input["authFile"], work);
        assert_eq!(
            all[3].input["profile"],
            json!({"kind":"listed","profileArn":PERSONAL})
        );
        assert_eq!(
            all[2].input["profile"],
            json!({"kind":"listed","profileArn":TEAM})
        );
        assert_ne!(all[2].key, all[3].key);
        assert_eq!(
            all[1].input["profile"],
            json!({
                "kind":"saved","profileArn":"arn:aws:codewhisperer:us-east-1:1:profile/SOLO01"
            })
        );

        let pinned = |model: &str| UsageScope {
            provider: Some("omp".into()),
            model: Some(model.into()),
            env: env.clone(),
        };
        let found = discover_found(scope_env(&pinned("kiro-work-personal01/auto"), || {
            panic!("pinned sessions use their own environment")
        }))
        .await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, all[3].key);
        assert_eq!(found[0].input, all[3].input);
        // The single-profile account also shares an ARN-scoped key with the pinned view.
        let solo = discover_found(scope_env(&pinned("kiro-solo-solo01/auto"), BTreeMap::new)).await;
        assert_eq!(solo[0].key, all[1].key);
        assert!(
            discover_found(scope_env(&pinned("kiro-work-missing/auto"), BTreeMap::new))
                .await
                .is_empty()
        );
        assert!(scope_env(&pinned("kiro-cli/auto"), || panic!("no global fallback")).is_none());
        // kiro-http keeps reading only the default account's saved selection.
        let saved = discover_found(scope_env(&pinned("kiro-http/auto"), BTreeMap::new)).await;
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].key, all[0].key);
    }

    #[tokio::test]
    async fn source_profile_locator_requests_that_profile_without_writes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        login(
            &path,
            "github",
            Some("arn:aws:codewhisperer:us-east-1:1:profile/SAVED"),
        );
        catalog(&path, &[(PERSONAL, "Personal")]);
        let before = std::fs::read(&path).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut rt = runtime(dir.path());
        rt.endpoints.insert(
            "https://management.us-east-1.kiro.dev/".into(),
            format!("http://{}/", listener.local_addr().unwrap()),
        );
        let input = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false)
            .remove(0)
            .input;
        let fetch_task = tokio::spawn(async move { fetch(&rt, &input).await.unwrap() });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        while !String::from_utf8_lossy(&bytes).contains(PERSONAL) {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "the request ended without the pinned profile");
            bytes.extend_from_slice(&buffer[..count]);
        }
        assert!(!String::from_utf8_lossy(&bytes).contains("profile/SAVED"));
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), fetch_task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn source_pins_named_api_keys_and_keeps_disabled_keys_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("accounts/work.sqlite3");
        std::fs::create_dir(work.parent().unwrap()).unwrap();
        store(&work, "github", "2099-01-01T00:00:00Z");
        let db = Connection::open(&work).unwrap();
        db.execute(
            "UPDATE credentials SET value = ?1 WHERE id = 'active'",
            [json!({
                "kind":"api-key","provider":"api-key","access_token":"ksk_test-key",
                "region":"us-west-2", "disabled":"private-reason"
            })
            .to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO credentials VALUES ('refresh_lease', '{\"owner\":\"fixture\",\"expiresAt\":0}')",
            [],
        )
        .unwrap();
        drop(db);
        let mut rt = runtime(dir.path());
        rt.env = scoped(&dir.path().join("auth.sqlite3")).env;
        let mut scope = scoped(&dir.path().join("auth.sqlite3"));
        scope.model = Some("kiro-work-key/auto".into());
        let pinned = discover(&rt, &scope).await.unwrap();
        assert_eq!(pinned.len(), 1);
        let global = discover(&rt, &global()).await.unwrap();
        assert_eq!(global[0].key, pinned[0].key);
        assert_eq!(
            global[0].label.as_deref(),
            Some("Kiro HTTP · work · api-key")
        );
        assert_eq!(
            pinned[0].input["authFile"],
            work.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(pinned[0].input["profile"], json!({"kind":"api-key"}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        rt.endpoints.insert(
            "https://management.us-west-2.kiro.dev/".into(),
            format!("http://{}/", listener.local_addr().unwrap()),
        );
        let report = fetch(&rt, &pinned[0].input).await.unwrap();
        let report = crate::usage::validate_report(report).unwrap();
        assert_eq!(report["status"], "unavailable");
        assert!(
            report["problem"]["detail"]
                .as_str()
                .unwrap()
                .contains("Sign in again")
        );
        assert!(!report.to_string().contains("private-reason"));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );

        scope.model = Some("kiro/auto".into());
        scope.env.insert("OMP_KIRO_TRANSPORT".into(), "http".into());
        assert!(discover(&rt, &scope).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_default_environment_key_overrides_store_and_uses_scoped_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sqlite3");
        login(&path, "github", Some(PERSONAL));
        let named = dir.path().join("accounts/work.sqlite3");
        std::fs::create_dir(named.parent().unwrap()).unwrap();
        login(&named, "google", Some(TEAM));
        let mut daemon_env = scoped(&path).env;
        daemon_env.insert("KIRO_API_KEY".into(), "ksk_daemon-fixture".into());
        daemon_env.insert("KIRO_API_REGION".into(), "us-west-2".into());
        let mut scope = scoped(&path);
        scope
            .env
            .insert("KIRO_API_KEY".into(), " ksk_session-fixture ".into());
        scope
            .env
            .insert("KIRO_API_REGION".into(), "eu-west-1".into());
        let mut rt = runtime(dir.path());
        rt.env = daemon_env.clone();
        rt.kiro = Runtime::new(&UsageContext {
            env: daemon_env,
            user_home: Some(dir.path().to_owned()),
            provider_specs: Vec::new(),
            enabled_provider_ids: Default::default(),
            sessions: vec![json!({"env":scope.env})],
            muse_providers: Vec::new(),
            plugins: None,
        });
        let global = discover(&rt, &global()).await.unwrap();
        assert_eq!(global.len(), 2);
        assert_eq!(global[0].input["kind"], "environment");
        assert_eq!(
            global[1].input["authFile"],
            named.canonicalize().unwrap().to_str().unwrap()
        );
        let scoped = discover(&rt, &scope).await.unwrap();
        assert_eq!(scoped.len(), 1);
        assert_ne!(global[0].key, scoped[0].key);
        for (row, token, region) in [
            (&global[0], "ksk_daemon-fixture", "us-west-2"),
            (&scoped[0], "ksk_session-fixture", "eu-west-1"),
        ] {
            assert!(!row.input.to_string().contains(token));
            let body = json!({"nextDateReset":4_000_000_000u64,"usageBreakdownList":[
                {"resourceType":"CREDIT","currentUsage":1,"usageLimit":100}
            ]});
            let (endpoint, received, server) = transport::tests::fixture(200, body.to_string());
            rt.endpoints
                .insert(format!("https://management.{region}.kiro.dev/"), endpoint);
            let report = fetch(&rt, &row.input).await.unwrap();
            assert_eq!(report["status"], "available");
            assert!(!report.to_string().contains(token));
            let request = received.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(request.contains(&format!("Bearer {token}")));
            assert!(request.contains("GetUsageLimits"));
            assert!(!request.contains("profileArn"));
            server.join().unwrap();
        }
        scope.model = Some("kiro-default-key/auto".into());
        assert_eq!(discover(&rt, &scope).await.unwrap()[0].key, scoped[0].key);
        scope.model = Some("kiro-default-personal01/auto".into());
        assert!(discover(&rt, &scope).await.unwrap().is_empty());
        scope.model = Some("kiro-work-team02/auto".into());
        assert_eq!(discover(&rt, &scope).await.unwrap()[0].key, global[1].key);
    }

    #[tokio::test]
    async fn source_saved_profile_key_matches_global_across_catalog_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sqlite3");
        login(&path, "github", Some(PERSONAL));
        let saved = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::Saved, false).remove(0);
        let sole = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false).remove(0);
        assert_eq!(sole.key, saved.key);
        catalog(&path, &[(PERSONAL, "Personal"), (TEAM, "Team")]);
        let multiple = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false);
        assert_eq!(multiple.len(), 2);
        let personal = multiple
            .iter()
            .find(|row| row.input["profile"]["profileArn"] == PERSONAL)
            .unwrap();
        assert_eq!(personal.key, saved.key);
        assert_eq!(personal.input, saved.input);
    }

    #[tokio::test]
    async fn source_pinned_locators_reject_profile_kind_and_login_changes_before_http() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sqlite3");
        login(&path, "github", Some(PERSONAL));
        let mut rt = runtime(dir.path());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        for region in ["us-east-1", "eu-central-1", "us-west-2"] {
            rt.endpoints.insert(
                format!("https://management.{region}.kiro.dev/"),
                format!("http://{}/", listener.local_addr().unwrap()),
            );
        }
        let saved = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false).remove(0);
        let db = Connection::open(&path).unwrap();
        db.execute("UPDATE credentials SET value = json_set(value, '$.profile_arn', ?1) WHERE id = 'active'", [TEAM]).unwrap();
        assert_eq!(
            fetch(&rt, &saved.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        catalog(&path, &[(PERSONAL, "Personal"), (TEAM, "Team")]);
        let listed = store_accounts(
            DEFAULT_ACCOUNT,
            &path,
            &Selection::Pinned("kiro-default-personal01".into()),
            false,
        )
        .remove(0);
        db.execute("DELETE FROM credentials WHERE id = 'profiles'", [])
            .unwrap();
        assert_eq!(
            fetch(&rt, &listed.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        let key = json!({"kind":"api-key","provider":"api-key","region":"us-west-2","access_token":"ksk_first-fixture"});
        let replace = |value: &Value| {
            db.execute(
                "UPDATE credentials SET value = ?1 WHERE id = 'active'",
                [value.to_string()],
            )
            .unwrap()
        };
        replace(&key);
        assert_eq!(
            fetch(&rt, &saved.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        let api_key = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false).remove(0);
        replace(
            &json!({"kind":"api-key","provider":"api-key","region":"us-west-2","access_token":"ksk_replacement-fixture"}),
        );
        let replacement = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false).remove(0);
        assert_eq!(api_key.key, replacement.key);
        assert_ne!(api_key.input, replacement.input);
        assert_eq!(
            fetch(&rt, &api_key.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        replace(
            &json!({"kind":"social","provider":"github","profile_arn":PERSONAL,"expires_at":"2099-01-01T00:00:00Z","access_token":"different-oauth-fixture"}),
        );
        assert_eq!(
            fetch(&rt, &api_key.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        assert_eq!(
            fetch(&rt, &saved.input).await.unwrap(),
            unmeasured(LOGIN_CHANGED)
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn source_unselected_oauth_does_not_fetch_unpinned_quota() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sqlite3");
        login(&path, "github", None);
        let row = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::Saved, false).remove(0);
        let rt = runtime(dir.path());
        assert_eq!(
            fetch(&rt, &row.input).await.unwrap(),
            unmeasured("Select a Kiro HTTP profile in omp-kiro, then refresh usage."),
        );
    }

    #[test]
    fn source_pinned_provider_collisions_have_no_winner_but_keep_profile_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.sqlite3");
        login(&path, "github", None);
        let collision = "arn:aws:codewhisperer:us-east-1:1:profile/team_02";
        catalog(
            &path,
            &[
                (TEAM, "Team"),
                (collision, "Other Team"),
                (PERSONAL, "Personal"),
                (PERSONAL, "Personal"),
            ],
        );
        let rows = store_accounts(DEFAULT_ACCOUNT, &path, &Selection::All, false);
        assert_eq!(rows.len(), 3);
        let pinned =
            |id: &str| store_accounts(DEFAULT_ACCOUNT, &path, &Selection::Pinned(id.into()), false);
        assert!(pinned("kiro-default-team02").is_empty());
        assert_eq!(pinned("kiro-default-personal01").len(), 1);
    }
}
