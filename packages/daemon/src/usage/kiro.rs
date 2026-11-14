//! Read-only built-in usage for OMP's managed Kiro HTTP authentication store.
mod auth;
mod normalize;
mod transport;

use super::common::{UsageAccount, UsageRuntime, UsageScope, hash_account_key};
use anyhow::Result;
use tokio_util::sync::CancellationToken;
use chrono::{SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

const LOGIN_UNAVAILABLE: &str =
    "Kiro HTTP login unavailable. Run omp-kiro login, then refresh usage.";
const CANCELLED: &str = "The Kiro usage request was cancelled.";
const TIMEOUT: &str = "The Kiro usage request timed out.";

fn scope_env(
    scope: &UsageScope,
    global: impl FnOnce() -> BTreeMap<String, String>,
) -> Option<BTreeMap<String, String>> {
    if scope.provider.is_none() {
        return Some(global());
    }
    // Preserve OMP HTTP markers, without claiming CLI/non-HTTP sessions.
    if scope.provider.as_deref() == Some("kiro-http")
        || scope.env.get("OMP_KIRO_TRANSPORT").is_some_and(|v| v == "http")
        || scope.model.as_deref().is_some_and(|v| v.starts_with("kiro-http/"))
    {
        Some(scope.env.clone())
    } else {
        None
    }
}

pub(super) async fn discover(rt: &UsageRuntime, scope: &UsageScope) -> Result<Vec<UsageAccount>> {
    let Some(env) = scope_env(scope, || {
        let mut env = rt.env.clone();
        if !env.get("HOME").is_some_and(|home| std::path::Path::new(home).is_absolute()) {
            env.insert("HOME".into(), rt.home.to_string_lossy().into_owned());
        }
        env
    }) else {
        return Ok(Vec::new());
    };
    Ok(discover_env(env, &rt.cancel).await)
}
async fn discover_env(
    env: BTreeMap<String, String>,
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
            let path = auth::auth_path(&env).ok()?.canonicalize().ok()?;
            let credential = auth::read(&path).ok()?;
            let path = path.to_str()?;
            Some(UsageAccount {
                key: hash_account_key(path),
                label: Some(format!("Kiro HTTP · {}", credential.provider)),
                harness: Some("Kiro HTTP".into()),
                input: json!({"authFile":path}),
            })
        }) => result.ok().flatten().into_iter().collect(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    auth_file: String,
}

pub(super) async fn fetch(rt: &UsageRuntime, input: &Value) -> Result<Value> {
    let input: Input = match Input::deserialize(input) {
        Ok(input) => input,
        Err(_) => return Ok(transport::error("Invalid Kiro authentication store locator.")),
    };
    let path = PathBuf::from(input.auth_file);
    if !path.is_absolute() {
        return Ok(transport::error("Invalid Kiro authentication store locator."));
    }
    Ok(tokio::select! {
        biased;
        _ = rt.cancel.cancelled() => transport::error(CANCELLED),
        _ = tokio::time::sleep(Duration::from_secs(15)) => transport::error(TIMEOUT),
        report = fetch_path(rt, path) => report,
    })
}
async fn fetch_path(rt: &UsageRuntime, path: PathBuf) -> Value {
    let credential = match tokio::task::spawn_blocking(move || auth::read(&path)).await {
        Ok(Ok(credential)) => credential,
        _ => return transport::error(LOGIN_UNAVAILABLE),
    };
    if credential.expires_at <= Utc::now() {
        return json!({"status":"unavailable", "problem":{
            "kind":"expired", "expiresAt":credential.expires_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "refreshedBy":"omp-kiro",
        }});
    }
    let extra_ca = rt.env.get("NODE_EXTRA_CA_CERTS")
        .filter(|v| !v.is_empty()).map(PathBuf::from);
    let client = match tokio::task::spawn_blocking(move || transport::client(extra_ca.as_deref())).await {
        Ok(Ok(client)) => client,
        Ok(Err(message)) => return transport::error(message),
        Err(_) => return transport::error("The Kiro usage service could not be reached."),
    };
    // auth::read has dropped its connection before TLS setup or any HTTP await.
    transport::fetch(rt, client, &credential).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::Path;

    pub(super) fn runtime(home: &Path) -> UsageRuntime {
        UsageRuntime {
            home: home.into(), env: BTreeMap::new(), http: reqwest::Client::new(),
            cancel: CancellationToken::new(), endpoints: BTreeMap::new(),
            muse_providers: Vec::new(), plugins: None,
        }
    }
    fn global() -> UsageScope {
        UsageScope { provider: None, model: None, env: BTreeMap::new() }
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

    #[tokio::test]
    async fn source_session_auth_is_isolated_and_account_has_no_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let daemon = dir.path().join("daemon.db");
        let session = dir.path().join("session.db");
        store(&daemon, "google", "2099-01-01T00:00:00Z");
        store(&session, "github", "2099-01-01T00:00:00Z");
        let env = scope_env(&scoped(&session), || {
            panic!("session must not inspect daemon environment")
        })
        .unwrap();
        let accounts = discover_env(env, &CancellationToken::new()).await;
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].label.as_deref(), Some("Kiro HTTP · github"));
        let canonical = session.canonicalize().unwrap();
        assert_eq!(
            accounts[0].key,
            hash_account_key(canonical.to_str().unwrap())
        );
        assert_eq!(
            accounts[0].input,
            json!({"authFile":canonical.to_str().unwrap()})
        );
        assert!(
            !accounts[0].input.to_string().contains("private-access-token")
        );
        let global = scope_env(&global(), || {
            BTreeMap::from([("OMP_KIRO_AUTH_FILE".into(), daemon.to_str().unwrap().into())])
        })
        .unwrap();
        let accounts = discover_env(global, &CancellationToken::new()).await;
        assert_eq!(accounts[0].label.as_deref(), Some("Kiro HTTP · google"));
        let missing = dir.path().join("missing.db");
        let env = scope_env(&scoped(&missing), || panic!("no daemon fallback")).unwrap();
        assert!(
            discover_env(env, &CancellationToken::new())
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
        let env = scope_env(&scoped(&path), BTreeMap::new).unwrap();
        let accounts = discover_env(env, &CancellationToken::new()).await;
        assert_eq!(accounts.len(), 1);
        let result = fetch(&runtime(dir.path()), &accounts[0].input).await.unwrap();
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
            json!({"authFile":"relative"}),
            json!({"authFile":123}),
            json!({"authFile":"/valid","accessToken":"private-access-token"}),
        ] {
            let value =
                serde_json::to_value(fetch(&runtime(Path::new("/unused")), &input).await.unwrap()).unwrap();
            assert_eq!(
                value,
                json!({"status":"error","error":"Invalid Kiro authentication store locator."})
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private-auth-path.db");
        std::fs::write(&path, b"private-access-token invalid sqlite").unwrap();
        let report = fetch(
            &runtime(dir.path()),
            &json!({"authFile":path.to_str().unwrap()}),
        )
        .await.unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"status":"error","error":LOGIN_UNAVAILABLE})
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
        let report = fetch(&rt, &json!({"authFile":path.to_str().unwrap()})).await.unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"status":"error","error":CANCELLED})
        );
        assert!(!path.exists());
        assert!(discover_env(BTreeMap::new(), &token).await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn source_canonical_aliases_share_an_account_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.db");
        let alias = dir.path().join("alias.db");
        store(&path, "github", "2099-01-01T00:00:00Z");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        let first = discover_env(
            scope_env(&scoped(&path), BTreeMap::new).unwrap(),
            &CancellationToken::new(),
        )
        .await;
        let second = discover_env(
            scope_env(&scoped(&alias), BTreeMap::new).unwrap(),
            &CancellationToken::new(),
        )
        .await;
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
        rt.env.insert("OMP_KIRO_AUTH_FILE".into(), daemon.to_str().unwrap().into());
        let global_accounts = discover(&rt, &global()).await.unwrap();
        assert_eq!(global_accounts[0].input["authFile"], daemon.canonicalize().unwrap().to_str().unwrap());
        let scope = UsageScope {
            provider: Some("omp".into()), model: Some("kiro-http/auto".into()),
            env: BTreeMap::from([("HOME".into(), session_home.to_str().unwrap().into())]),
        };
        let accounts = discover(&rt, &scope).await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].input["authFile"], session.canonicalize().unwrap().to_str().unwrap());
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
        store(&path, "github", "2099-01-01T00:00:00Z");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut rt = runtime(dir.path());
        rt.endpoints.insert(
            "https://management.us-east-1.kiro.dev/".into(),
            format!("http://{}/", listener.local_addr().unwrap()),
        );
        let cancel = rt.cancel.clone();
        let input = json!({"authFile":path.to_str().unwrap()});
        let fetch_task = tokio::spawn(async move { fetch(&rt, &input).await.unwrap() });
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await.unwrap().unwrap();
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
        socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n").await.unwrap();
        cancel.cancel();
        let report = tokio::time::timeout(Duration::from_secs(1), fetch_task).await.unwrap().unwrap();
        assert_eq!(report, transport::error(CANCELLED));
    }
}
