//! Flutter `HostConfig`: endpoint resolution on top of the SDK target parser, the local socket
//! identity used for credential selection, and hello authentication.
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use paseo_client::{
    crypto::SecurityMode,
    transport::target::{Endpoint, Target},
};
use serde_json::{Value, json};

use crate::CoreError;

pub(crate) struct ResolvedHost {
    pub target: Target,
    /// `expectedServerId`, else the relay server id named by a pairing URL.
    pub expected_server_id: Option<String>,
    /// Daemon public key of a relay endpoint.
    pub relay_key: Option<String>,
    pub require_protected: bool,
    pub ssh: bool,
    /// The config password, else the endpoint password.
    pub password: Option<String>,
}

impl ResolvedHost {
    pub fn is_relay(&self) -> bool {
        self.relay_key.is_some()
    }
}

pub(crate) fn text<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config[key].as_str()
}

fn invalid() -> CoreError {
    // Parser errors can contain the endpoint, which can contain a password.
    CoreError::new(
        "invalid_connection",
        "Invalid host connection. Use host:port, ws(s)://, tcp://, unix://, pipe://, ssh:// or a pairing URL.",
        false,
    )
}

pub(crate) fn resolve(config: &Value) -> Result<ResolvedHost, CoreError> {
    let endpoint = text(config, "endpoint").unwrap_or_default().trim();
    let pairing =
        endpoint.starts_with("relay://") || endpoint.contains("#connect=") || endpoint.contains("#offer=");
    if !pairing && (endpoint.starts_with("ws://") || endpoint.starts_with("wss://") || endpoint.starts_with("tcp://"))
        && let Ok(url) = url::Url::parse(endpoint)
            && url
                .query_pairs()
                .any(|(key, value)| (key == "role" && value == "client") || key == "serverId")
            {
                return Err(CoreError::new(
                    "invalid_connection",
                    "A relay pairing offer with its daemon public key is required",
                    false,
                ));
            }
    let mut target = Target::parse(endpoint).map_err(|_| invalid())?;
    let ssh = matches!(target.endpoint, Endpoint::Ssh { .. });
    let mut relay_key = None;
    let mut require_protected = false;
    let mut relay_server = None;
    if let Some((key, mode)) = target.security.take() {
        // Original relayConnectionFromOffer stores `daemonPublicKeyB64.trim()`; the key must be
        // canonical base64 of 32 bytes.
        let key = key.trim().to_owned();
        let canonical = STANDARD
            .decode(&key)
            .ok()
            .is_some_and(|bytes| bytes.len() == 32 && STANDARD.encode(&bytes) == key);
        if !canonical {
            return Err(CoreError::new(
                "invalid_connection",
                "Noncanonical daemon public key",
                false,
            ));
        }
        require_protected = mode == SecurityMode::Protected;
        if let Endpoint::Socket { url, .. } = &target.endpoint {
            relay_server = url::Url::parse(url).ok().and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "serverId")
                    .map(|(_, value)| value.into_owned())
            });
        }
        target.security = Some((key.clone(), mode));
        relay_key = Some(key);
    }
    let password = match config.get("password") {
        Some(Value::String(password)) => Some(password.clone()),
        _ => target.password.clone(),
    }
    .filter(|password| !password.is_empty());
    let expected_server_id = text(config, "expectedServerId")
        .map(str::to_owned)
        .or(relay_server);
    Ok(ResolvedHost {
        target,
        expected_server_id,
        relay_key,
        require_protected,
        ssh,
        password,
    })
}

fn expand_home(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    path.to_owned()
}

/// Flutter `identity(uri)`: the socket a non-relay, non-SSH endpoint listens on. Loopback
/// hosts compare equal, Unix sockets and pipes by path.
pub(crate) fn socket_identity(endpoint: &str) -> Option<String> {
    let target = Target::parse(endpoint).ok()?;
    if target.security.is_some() {
        return None;
    }
    match &target.endpoint {
        Endpoint::Ssh { .. } => None,
        Endpoint::Socket {
            ipc: Some(path), ..
        } => {
            let scheme = if target.description.starts_with("pipe://") {
                "pipe"
            } else {
                "unix"
            };
            Some(format!("{scheme}:{}", expand_home(path)))
        }
        Endpoint::Socket { url, ipc: None } => {
            let url = url::Url::parse(url).ok()?;
            let host = url.host_str()?;
            let host = if matches!(
                host,
                "localhost" | "127.0.0.1" | "0.0.0.0" | "::1" | "::" | "[::1]" | "[::]"
            ) {
                "localhost"
            } else {
                host
            };
            Some(format!("{host}:{}", url.port_or_known_default()?))
        }
    }
}

pub(crate) fn valid_credential(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Flutter `PaseoClient._authentication`: a credential file is used only while the daemon that
/// owns it (its sibling `paseo.pid`) listens on the selected socket.
pub(crate) async fn authentication(config: &Value, resolved: &ResolvedHost) -> Option<Value> {
    let mut token = text(config, "localCredential").map(str::to_owned);
    if let Some(path) = text(config, "localCredentialPath")
        && !resolved.is_relay() && !resolved.ssh
            && let Some(found) = credential_for(Path::new(path), config).await {
                token = Some(found);
            }
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        return Some(json!({"kind": "localCredential", "token": token}));
    }
    resolved
        .password
        .as_ref()
        .map(|password| json!({"kind": "password", "password": password}))
}

async fn credential_for(path: &Path, config: &Value) -> Option<String> {
    let lock_path: PathBuf = path.parent()?.join("paseo.pid");
    let lock: Value = serde_json::from_slice(&tokio::fs::read(lock_path).await.ok()?).ok()?;
    let listening = socket_identity(lock["listen"].as_str()?)?;
    let selected = socket_identity(text(config, "endpoint")?)?;
    if listening != selected {
        return None;
    }
    let found = tokio::fs::read_to_string(path).await.ok()?;
    let found = found.trim();
    valid_credential(found).then(|| found.to_owned())
}

/// What the host dialog shows about a connection before saving it (pairing confirmation, the
/// `sameRelay` check). Never contains the password.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostConfigInfo {
    pub is_relay: bool,
    pub expected_server_id: Option<String>,
    pub daemon_public_key: Option<String>,
    pub require_protected: bool,
    /// `ws`/`wss` (direct and relay), `unix`, `pipe` or `ssh`.
    pub scheme: String,
    /// Host name, IPC path, or `user@host` for SSH.
    pub host: String,
    pub port: Option<u16>,
}

/// Resolves a Flutter `HostConfig` JSON (`endpoint`, optional `password`/`expectedServerId`)
/// exactly as a connection would. The error message is user-facing.
pub fn inspect_host_config(config: &Value) -> Result<HostConfigInfo, CoreError> {
    let resolved = resolve(config)?;
    let (scheme, host, port) = match &resolved.target.endpoint {
        Endpoint::Ssh { host, port, .. } => ("ssh".to_owned(), host.clone(), *port),
        Endpoint::Socket { ipc: Some(path), .. } => {
            let scheme = if resolved.target.description.starts_with("pipe://") {
                "pipe"
            } else {
                "unix"
            };
            (scheme.to_owned(), path.clone(), None)
        }
        Endpoint::Socket { url, ipc: None } => {
            let url = url::Url::parse(url).map_err(|_| invalid())?;
            (
                url.scheme().to_owned(),
                url.host_str().unwrap_or_default().to_owned(),
                url.port_or_known_default(),
            )
        }
    };
    Ok(HostConfigInfo {
        is_relay: resolved.is_relay(),
        expected_server_id: resolved.expected_server_id,
        daemon_public_key: resolved.relay_key,
        require_protected: resolved.require_protected,
        scheme,
        host,
        port,
    })
}

/// Flutter `probeConnectTimeout`.
pub(crate) fn probe_connect_timeout(config: &Value) -> std::time::Duration {
    match resolve(config) {
        Ok(resolved) if resolved.is_relay() => std::time::Duration::from_secs(10),
        Ok(resolved) if resolved.ssh => std::time::Duration::from_secs(15),
        _ => std::time::Duration::from_secs(6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_sockets_share_an_identity_and_relays_have_none() {
        assert_eq!(
            socket_identity("127.0.0.1:6767"),
            socket_identity("ws://localhost:6767/ws")
        );
        assert_ne!(
            socket_identity("localhost:6767"),
            socket_identity("localhost:6768")
        );
        assert_eq!(socket_identity("ssh://box?daemonPort=6767"), None);
        assert_eq!(
            socket_identity("unix:///tmp/paseo.sock").as_deref(),
            Some("unix:/tmp/paseo.sock")
        );
    }

    #[test]
    fn config_password_overrides_the_endpoint_password() {
        let resolved = resolve(&json!({"endpoint":"tcp://host:1?password=url","password":"mine"})).unwrap();
        assert_eq!(resolved.password.as_deref(), Some("mine"));
        let resolved = resolve(&json!({"endpoint":"tcp://host:1?password=url"})).unwrap();
        assert_eq!(resolved.password.as_deref(), Some("url"));
    }

    #[test]
    fn relay_ids_without_a_key_are_rejected() {
        assert!(resolve(&json!({"endpoint":"ws://relay:443/ws?serverId=srv&role=client"})).is_err());
        assert!(resolve(&json!({"endpoint":"not a host"})).is_err());
    }
}
