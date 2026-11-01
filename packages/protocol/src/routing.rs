//! Transport-independent endpoint identities and shareable connection offers.
use crate::ProtocolError;
use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

pub const DEFAULT_RELAY_ENDPOINT: &str = "relay.paseo.sh:443";
pub const CURRENT_RELAY_PROTOCOL_VERSION: &str = "2";
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostPort {
    pub host: String,
    pub port: u16,
    pub is_ipv6: bool,
}
impl HostPort {
    pub fn parse(input: &str) -> Result<Self, ProtocolError> {
        let input = input.trim();
        let (host, port, is_ipv6) = if let Some(rest) = input.strip_prefix('[') {
            let (host, port) = rest.split_once("]:").ok_or(ProtocolError::Endpoint)?;
            (host, port, true)
        } else {
            let (host, port) = input.rsplit_once(':').ok_or(ProtocolError::Endpoint)?;
            (host, port, false)
        };
        let host = host.trim();
        if host.is_empty()
            || port.is_empty()
            || port.len() > 5
            || !port.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(ProtocolError::Endpoint);
        }
        let port = port.parse::<u16>().map_err(|_| ProtocolError::Endpoint)?;
        if port == 0 {
            return Err(ProtocolError::Endpoint);
        }
        Ok(Self {
            host: host.into(),
            port,
            is_ipv6,
        })
    }
    pub fn authority(&self) -> String {
        if self.is_ipv6 {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}
pub fn normalize_host_port(input: &str) -> Result<String, ProtocolError> {
    Ok(HostPort::parse(input)?.authority())
}
pub fn normalize_loopback_to_localhost(input: &str) -> Result<String, ProtocolError> {
    let hp = HostPort::parse(input)?;
    if matches!(hp.host.as_str(), "127.0.0.1" | "0.0.0.0" | "::1" | "::") {
        Ok(format!("localhost:{}", hp.port))
    } else {
        Ok(input.into())
    }
}
pub fn derive_label_from_endpoint(input: &str) -> String {
    HostPort::parse(input)
        .map(|v| v.host)
        .unwrap_or_else(|_| "Unnamed Host".into())
}
pub fn build_daemon_websocket_url(endpoint: &str, use_tls: bool) -> Result<Url, ProtocolError> {
    let hp = HostPort::parse(endpoint)?;
    Url::parse(&format!(
        "{}://{}/ws",
        if use_tls { "wss" } else { "ws" },
        hp.authority()
    ))
    .map_err(|_| ProtocolError::Endpoint)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayRole {
    Server,
    Client,
}
pub fn build_relay_websocket_url(
    endpoint: &str,
    use_tls: bool,
    server_id: &str,
    role: RelayRole,
    connection_id: Option<&str>,
    version: Option<&str>,
) -> Result<Url, ProtocolError> {
    let version = version
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(CURRENT_RELAY_PROTOCOL_VERSION);
    if !matches!(version, "1" | "2") {
        return Err(ProtocolError::Invalid("relay version"));
    }
    let mut url = build_daemon_websocket_url(endpoint, use_tls)?;
    url.query_pairs_mut()
        .append_pair("serverId", server_id)
        .append_pair(
            "role",
            match role {
                RelayRole::Server => "server",
                RelayRole::Client => "client",
            },
        )
        .append_pair("v", version);
    if let Some(id) = connection_id.filter(|s| !s.is_empty()) {
        url.query_pairs_mut().append_pair("connectionId", id);
    }
    Ok(url)
}
pub fn extract_host_port_from_websocket_url(input: &str) -> Result<String, ProtocolError> {
    let url = Url::parse(input).map_err(|_| ProtocolError::Endpoint)?;
    if !matches!(url.scheme(), "ws" | "wss") || url.path().trim_end_matches('/') != "/ws" {
        return Err(ProtocolError::Endpoint);
    }
    let host = url.host_str().ok_or(ProtocolError::Endpoint)?;
    let port = url.port_or_known_default().ok_or(ProtocolError::Endpoint)?;
    Ok(format!("{host}:{port}"))
}
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectionUri {
    pub endpoint: HostPort,
    pub use_tls: bool,
    pub password: Option<String>,
}
impl std::fmt::Debug for ConnectionUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionUri")
            .field("endpoint", &self.endpoint)
            .field("use_tls", &self.use_tls)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}
fn parse_transport_uri(input: &str, scheme: &str) -> Result<(Url, HostPort), ProtocolError> {
    let url = Url::parse(input.trim()).map_err(|_| ProtocolError::Endpoint)?;
    if url.scheme() != scheme || !url.username().is_empty() || url.password().is_some() {
        return Err(ProtocolError::Endpoint);
    }
    let host = url.host_str().ok_or(ProtocolError::Endpoint)?;
    let port = url
        .port()
        .filter(|p| *p > 0)
        .ok_or(ProtocolError::Endpoint)?;
    let endpoint = HostPort::parse(&format!("{host}:{port}"))?;
    Ok((url, endpoint))
}
fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}
pub fn parse_connection_uri(input: &str) -> Result<ConnectionUri, ProtocolError> {
    let (url, endpoint) = parse_transport_uri(input, "tcp")?;
    Ok(ConnectionUri {
        endpoint,
        use_tls: query(&url, "ssl").as_deref() == Some("true"),
        password: query(&url, "password").filter(|s| !s.is_empty()),
    })
}
/// Passwords are excluded by default. Set `include_password` only for protected storage.
pub fn serialize_connection_uri(
    parts: &ConnectionUri,
    include_password: bool,
) -> Result<String, ProtocolError> {
    let mut url = Url::parse(&format!("tcp://{}", parts.endpoint.authority()))
        .map_err(|_| ProtocolError::Endpoint)?;
    if parts.use_tls {
        url.query_pairs_mut().append_pair("ssl", "true");
    }
    if include_password {
        if let Some(password) = &parts.password {
            url.query_pairs_mut().append_pair("password", password);
        }
    }
    Ok(url.into())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayEndpoint {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_tls: Option<bool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MinimumSecurityMode {
    Protected,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionOffer {
    pub v: u8,
    pub server_id: String,
    pub daemon_public_key_b64: String,
    pub relay: RelayEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_security_mode: Option<MinimumSecurityMode>,
}
impl ConnectionOffer {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.v != 2
            || self.server_id.is_empty()
            || self.daemon_public_key_b64.is_empty()
            || self.relay.endpoint.is_empty()
        {
            return Err(ProtocolError::Invalid("connection offer"));
        }
        Ok(())
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let result: Self = serde_json::from_slice(bytes).map_err(|_| ProtocolError::Json)?;
        result.validate()?;
        Ok(result)
    }
    pub fn encode(&self) -> Result<String, ProtocolError> {
        self.validate()?;
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).map_err(|_| ProtocolError::Json)?))
    }
}
pub fn parse_connection_offer_from_url(
    input: &str,
) -> Result<Option<ConnectionOffer>, ProtocolError> {
    let Some((_, encoded)) = input.trim().split_once("#offer=") else {
        return Ok(None);
    };
    let encoded = encoded.trim();
    if encoded.is_empty() {
        return Ok(None);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .or_else(|_| URL_SAFE.decode(encoded))
        .map_err(|_| ProtocolError::Invalid("connection offer encoding"))?;
    ConnectionOffer::parse(&bytes).map(Some)
}
// decodeURIComponent semantics: '+' stays literal; malformed UTF-8/escapes fail.
fn percent_decode(input: &str) -> Result<String, ProtocolError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(ProtocolError::Endpoint);
            }
            let hi = (bytes[i + 1] as char)
                .to_digit(16)
                .ok_or(ProtocolError::Endpoint)?;
            let lo = (bytes[i + 2] as char)
                .to_digit(16)
                .ok_or(ProtocolError::Endpoint)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ProtocolError::Endpoint)
}
#[derive(Clone, PartialEq, Eq)]
pub struct RelayConnectionUri {
    pub offer: ConnectionOffer,
    pub password: Option<String>,
}
impl std::fmt::Debug for RelayConnectionUri {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayConnectionUri")
            .field("offer", &self.offer)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}
pub fn parse_relay_connection_uri(input: &str) -> Result<RelayConnectionUri, ProtocolError> {
    let decoded;
    let uri = if let Some((_, suffix)) = input.trim().split_once("#connect=") {
        decoded = percent_decode(suffix)?;
        decoded.as_str()
    } else {
        input.trim()
    };
    let (url, endpoint) = parse_transport_uri(uri, "relay")?;
    let server_id = percent_decode(url.path().strip_prefix('/').unwrap_or(url.path()))?;
    let key = query(&url, "key")
        .filter(|s| !s.is_empty())
        .ok_or(ProtocolError::Endpoint)?;
    if server_id.is_empty() || server_id.contains('/') {
        return Err(ProtocolError::Endpoint);
    }
    let minimum_security_mode = match query(&url, "security").as_deref() {
        None => None,
        Some("protected") => Some(MinimumSecurityMode::Protected),
        _ => return Err(ProtocolError::Invalid("security mode")),
    };
    Ok(RelayConnectionUri {
        offer: ConnectionOffer {
            v: 2,
            server_id,
            daemon_public_key_b64: key,
            relay: RelayEndpoint {
                endpoint: endpoint.authority(),
                use_tls: Some(query(&url, "ssl").as_deref() == Some("true")),
            },
            minimum_security_mode,
        },
        password: query(&url, "password").filter(|s| !s.is_empty()),
    })
}
pub fn serialize_relay_connection_uri(parts: &RelayConnectionUri) -> Result<String, ProtocolError> {
    parts.offer.validate()?;
    if parts.offer.server_id.contains('/') {
        return Err(ProtocolError::Endpoint);
    }
    let mut url = Url::parse(&format!(
        "relay://{}/",
        normalize_host_port(&parts.offer.relay.endpoint)?
    ))
    .map_err(|_| ProtocolError::Endpoint)?;
    url.path_segments_mut()
        .map_err(|_| ProtocolError::Endpoint)?
        .clear()
        .push(&parts.offer.server_id);
    url.query_pairs_mut()
        .append_pair("key", &parts.offer.daemon_public_key_b64);
    if parts.offer.relay.use_tls == Some(true) {
        url.query_pairs_mut().append_pair("ssl", "true");
    }
    if parts.offer.minimum_security_mode.is_some() {
        url.query_pairs_mut().append_pair("security", "protected");
    }
    if let Some(password) = &parts.password {
        url.query_pairs_mut().append_pair("password", password);
    }
    Ok(url.into())
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostSummary {
    pub registration_id: String,
    pub server_id: Option<String>,
    pub label: Option<String>,
    pub source: HostSource,
    pub metadata: BTreeMap<String, String>,
    pub enabled: bool,
    pub status: HostStatus,
    pub target_description: String,
    pub last_online_at: Option<String>,
    pub next_retry_at: Option<String>,
    pub error: Option<HostError>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSource {
    pub id: String,
    pub key: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostError {
    pub code: String,
    pub message: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostStatus {
    Connecting,
    Online,
    Offline,
    Error,
    Disabled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostIdentity {
    pub server_id: String,
    pub label: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostDirectory {
    #[serde(rename = "self")]
    pub local: HostIdentity,
    pub hosts: Vec<HostSummary>,
}
